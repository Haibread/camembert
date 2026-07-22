//! camembert-dump v1 writer (`docs/format/dump-v1.md`).
//!
//! [`write_dump`] serializes a completed [`ScanOutcome`] as an **ordered**
//! v1 dump: JSON Lines in a zstd seekable container (independent
//! checksummed frames, seek table in a trailing skippable frame — see
//! [`container`]). `zstdcat dump.cmbt | jq` works with stock tools.
//!
//! Representation choices this writer makes within the spec:
//!
//! - **Ordered only** (`"ordered":true`): the writer runs on the frozen
//!   post-scan arena, so tier-2 ordering (DFS preorder, siblings by raw
//!   name bytes) and the `d`-line subtree totals are always available.
//!   The streaming/unordered writer (D5 degrade) is a later increment.
//! - **Excluded mount points** (never scanned, spec §6.4 `ex`) get a `d`
//!   line of their own with `ex:"otherfs"` and zero children, rather than
//!   an entry line in the parent block: §6.2 states subdirectories are
//!   never repeated as entry lines, and the entry `k` enum cannot express
//!   "directory", so a parent-block entry would be indistinguishable from
//!   a file. Readers that ignore unknown `d` keys (§10) still parse it as
//!   an empty directory. Totals on such a line cover the directory's own
//!   inode only.
//! - **Directory children whose stat failed** (kind known from `d_type`,
//!   nothing else) are likewise emitted as a `d` line with `err:true` and
//!   zero totals — the same shape §6.2 prescribes for unreadable
//!   directories.
//! - Consequently the number of `d` lines is `dirs` (scanned, including
//!   unreadable-but-statted ones — what the `e` line reports) **plus**
//!   those synthetic excluded/stat-failed directory lines.
//! - **Entry `dev`** is emitted only for `nlink > 1` entries whose device
//!   differs from the containing directory's (the packed node stores no
//!   per-entry device; a regular entry's device equals its directory's on
//!   every path the scanner takes, since mount points are dir-kind).
//! - `sem:"blocks"`, `ext:false`, `allino:false` — extended metadata and
//!   all-inode emission are later increments.
//!
//! Hardlink attribution (§8): the caller must run
//! [`ScanOutcome::finalize_hardlinks`] first so subtree totals use the
//! canonical (smallest-path) owner; both CLI modes do this right after
//! scan completion.

mod container;
mod encode;

pub use encode::{decode_name, encode_name};

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;
use tracing::debug;

use crate::scan::{HardlinkLink, ScanOutcome};
use crate::tree::{DirId, DirState, ExcludedReason, Kind, NodeFlags, NodeId, Tree};

use container::FrameWriter;
use encode::JsonLine;

/// Caller-provided dump metadata. Core never reads the clock — the
/// frontend passes the timestamp (e.g. `SystemTime::now()`).
#[derive(Debug, Clone, Copy)]
pub struct DumpMeta {
    /// Wall-clock time of the dump, written as the header's `ts`.
    pub timestamp: SystemTime,
}

/// Write an ordered v1 dump of `outcome` to `writer` (flushed on
/// success). Call [`ScanOutcome::finalize_hardlinks`] first — subtree
/// totals must carry canonical hardlink attribution (§8).
pub fn write_dump<W: Write>(outcome: &ScanOutcome, writer: W, meta: &DumpMeta) -> io::Result<()> {
    debug_assert!(
        outcome.hardlinks_finalized() || outcome.hardlink_inodes == 0,
        "write_dump requires finalize_hardlinks() (canonical attribution, spec §8)"
    );
    let ts = meta
        .timestamp
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let stats = EndStats {
        entries: outcome.entries,
        dirs: outcome.dirs,
        errors: outcome.errors,
        elapsed_secs: outcome.elapsed.as_secs_f64(),
    };
    let mut out = write_records(
        outcome.tree(),
        outcome.root(),
        outcome.hardlink_links(),
        &stats,
        ts,
        writer,
        container::TARGET_FRAME_UNCOMPRESSED,
    )?;
    out.flush()
}

/// [`write_dump`] to a file, crash-safely (spec §9): the dump is written
/// to `<path>.part`, synced, and atomically renamed into place. A partial
/// `.part` is removed on failure.
pub fn write_dump_to_path(outcome: &ScanOutcome, path: &Path, meta: &DumpMeta) -> io::Result<()> {
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let write = (|| {
        let mut writer = BufWriter::new(File::create(&part)?);
        write_dump(outcome, &mut writer, meta)?;
        writer.flush()?;
        writer.get_ref().sync_all()
    })();
    match write {
        Ok(()) => {
            std::fs::rename(&part, path)?;
            debug!(path = %path.display(), "dump renamed into place");
            Ok(())
        }
        Err(err) => {
            // Best effort: never leave a torn .part behind on error.
            let _ = std::fs::remove_file(&part);
            Err(err)
        }
    }
}

/// `e`-line counters (mirrors the scan outcome's).
struct EndStats {
    entries: u64,
    dirs: u64,
    errors: u64,
    elapsed_secs: f64,
}

/// A pending DFS item: a scanned directory, or a synthetic one (excluded
/// mount point / stat-failed dir child) that has a `d` line but no
/// children.
enum Work {
    Dir(DirId, String),
    Stub(NodeId, String),
}

fn write_records<W: Write>(
    tree: &Tree,
    root: DirId,
    links: &[HardlinkLink],
    stats: &EndStats,
    ts: u64,
    out: W,
    frame_target: usize,
) -> io::Result<W> {
    let hardlinks: FxHashMap<NodeId, HardlinkLink> =
        links.iter().map(|link| (link.node, *link)).collect();
    let mut fw = FrameWriter::with_target(out, frame_target)?;

    let root_meta = tree.dir(root);
    let root_path = encode_name(tree.name(root_meta.node));
    let mut header = JsonLine::new();
    header.str("t", "h").str("format", "camembert-dump");
    header.u64("v", 1).u64("minor", 0);
    header
        .str("prog", "camembert")
        .str("progver", env!("CARGO_PKG_VERSION"));
    header.u64("ts", ts);
    header.str("root", &root_path);
    header.u64_string("dev", root_meta.dev);
    header
        .str("sem", "blocks")
        .bool("ext", false)
        .bool("ordered", true)
        .bool("allino", false);
    fw.write_line(header.finish().as_bytes())?;

    // Frame ordinal → first d-path in that frame (the `x` index, §6.5).
    let mut x_records: Vec<(u64, String)> = Vec::new();
    let record_x = |x_records: &mut Vec<(u64, String)>, ordinal: u64, path: &str| {
        if x_records.last().is_none_or(|&(f, _)| f != ordinal) {
            x_records.push((ordinal, path.to_owned()));
        }
    };

    // DFS preorder, siblings sorted by raw name bytes (§7 tier 2). The
    // stack receives each directory's dir-kind children in reverse sorted
    // order so pops come out sorted.
    let mut stack = vec![Work::Dir(root, root_path)];
    while let Some(work) = stack.pop() {
        match work {
            Work::Dir(dir, path) => {
                let meta = tree.dir(dir);
                let node = tree.node(meta.node);
                let mut children: Vec<NodeId> = tree.children(dir).collect();
                children.sort_by(|&a, &b| tree.name(a).cmp(tree.name(b)));
                let nd = children
                    .iter()
                    .filter(|&&c| tree.node(c).kind().is_dir())
                    .count() as u64;
                let nf = children.len() as u64 - nd;

                record_x(&mut x_records, fw.frame_ordinal(), &path);
                let mut d = JsonLine::new();
                d.str("t", "d").str("path", &path);
                d.u64("a", node.size().apparent)
                    .u64("d", node.size().real)
                    .i64("m", node.mtime());
                d.u64("nf", nf).u64("nd", nd);
                d.u64("ta", meta.ta)
                    .u64("td", meta.td)
                    .u64("tn", meta.tn)
                    .u64("te", u64::from(meta.te));
                if meta.state == DirState::Error {
                    d.bool("err", true);
                }
                fw.write_line(d.finish().as_bytes())?;

                for &child in &children {
                    if !tree.node(child).kind().is_dir() {
                        let line = entry_line(tree, child, meta.dev, &hardlinks);
                        fw.write_line(line.as_bytes())?;
                    }
                }
                for &child in children.iter().rev() {
                    if tree.node(child).kind().is_dir() {
                        let child_path = format!("{path}/{}", encode_name(tree.name(child)));
                        match tree.dir_of(child) {
                            Some(sub) => stack.push(Work::Dir(sub, child_path)),
                            None => stack.push(Work::Stub(child, child_path)),
                        }
                    }
                }
            }
            Work::Stub(node_id, path) => {
                let node = tree.node(node_id);
                let flags = node.flags();
                let err = flags.contains(NodeFlags::ERROR);
                record_x(&mut x_records, fw.frame_ordinal(), &path);
                let mut d = JsonLine::new();
                d.str("t", "d").str("path", &path);
                d.u64("a", node.size().apparent)
                    .u64("d", node.size().real)
                    .i64("m", node.mtime());
                d.u64("nf", 0).u64("nd", 0);
                d.u64("ta", node.size().apparent)
                    .u64("td", node.size().real)
                    .u64("tn", 1)
                    .u64("te", u64::from(err));
                if err {
                    d.bool("err", true);
                }
                if flags.contains(NodeFlags::EXCLUDED) {
                    let reason = match tree.excluded_reason(node_id) {
                        Some(ExcludedReason::KernFs) => "kernfs",
                        _ => "otherfs",
                    };
                    d.str("ex", reason);
                }
                fw.write_line(d.finish().as_bytes())?;
            }
        }
    }

    for (f, p) in x_records {
        let mut x = JsonLine::new();
        x.str("t", "x").u64("f", f).str("p", &p);
        fw.write_line(x.finish().as_bytes())?;
    }

    let mut e = JsonLine::new();
    e.str("t", "e");
    e.u64("entries", stats.entries)
        .u64("dirs", stats.dirs)
        .u64("errors", stats.errors);
    e.u64("ta", root_meta.ta).u64("td", root_meta.td);
    e.seconds("elapsed", stats.elapsed_secs);
    fw.write_line(e.finish().as_bytes())?;

    fw.finish()
}

/// One §6.4 entry line for a non-directory child.
fn entry_line(
    tree: &Tree,
    id: NodeId,
    dir_dev: u64,
    hardlinks: &FxHashMap<NodeId, HardlinkLink>,
) -> String {
    let node = tree.node(id);
    let mut line = JsonLine::new();
    line.str("n", &encode_name(tree.name(id)));
    line.u64("a", node.size().apparent)
        .u64("d", node.size().real)
        .i64("m", node.mtime());
    if let Some(k) = kind_letter(node.kind()) {
        line.str("k", k);
    }
    if let Some(link) = hardlinks.get(&id) {
        line.u64_string("i", link.ino)
            .u64("l", u64::from(link.nlink));
        if link.dev != dir_dev {
            line.u64_string("dev", link.dev);
        }
    }
    if node.flags().contains(NodeFlags::ERROR) {
        line.bool("err", true);
    }
    line.finish()
}

/// Spec §6.4 `k` values; regular files elide it, and `Kind::Other`
/// (DT_UNKNOWN with a failed stat) has no spec letter — the entry carries
/// `err:true` instead.
fn kind_letter(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Symlink => Some("l"),
        Kind::Block => Some("b"),
        Kind::Char => Some("c"),
        Kind::Fifo => Some("f"),
        Kind::Socket => Some("s"),
        Kind::File | Kind::Other => None,
        Kind::Dir => unreachable!("directories are d lines, not entries"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Size;
    use crate::tree::ChildRun;
    use serde_json::Value;

    /// root "/r" (dev 5) with, deliberately pushed out of order:
    /// files `~`, `\xff`, `b.txt`; dir `a` (containing hardlinked `leaf`
    /// and errored `bad`); excluded mount `mnt`; stat-failed dir `broken`.
    fn sample() -> (Tree, DirId, Vec<HardlinkLink>) {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/r", Size::new(4096, 8), 100);
        let root = tree.add_dir(root_node, None, 5);

        let first = tree.push_node(
            b"~",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(10, 1),
            1,
        );
        tree.push_node(
            b"\xff",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(20, 1),
            2,
        );
        tree.push_node(
            b"b.txt",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(30, 1),
            3,
        );
        let a_node = tree.push_node(
            b"a",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::new(4096, 8),
            4,
        );
        let mnt_node = tree.push_node(
            b"mnt",
            Kind::Dir,
            NodeFlags::EXCLUDED,
            root_node,
            Size::new(4096, 8),
            5,
        );
        tree.set_excluded(mnt_node, ExcludedReason::OtherFs);
        tree.push_node(
            b"broken",
            Kind::Dir,
            NodeFlags::ERROR,
            root_node,
            Size::default(),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: first.index() as u32,
                len: 6,
            },
        );
        // Section delta for root's own block (6 children).
        tree.apply_delta(root, 10 + 20 + 30 + 4096 + 4096, 512 * 3 + 4096 * 2, 6, 1);

        let a = tree.add_dir(a_node, Some(root), 5);
        let leaf = tree.push_node(
            b"leaf",
            Kind::File,
            NodeFlags::default(),
            a_node,
            Size::new(100, 1),
            6,
        );
        tree.push_node(
            b"bad",
            Kind::File,
            NodeFlags::ERROR,
            a_node,
            Size::default(),
            0,
        );
        tree.push_run(
            a,
            ChildRun {
                start: leaf.index() as u32,
                len: 2,
            },
        );
        tree.apply_delta(a, 100, 512, 2, 1);
        tree.release_token(a);
        tree.release_token(root);

        let links = vec![HardlinkLink {
            node: leaf,
            dev: 6, // differs from the dir's dev 5: `dev` must be emitted
            ino: 1 << 60,
            nlink: 2,
        }];
        (tree, root, links)
    }

    fn stats() -> EndStats {
        EndStats {
            entries: 8,
            dirs: 2,
            errors: 2,
            elapsed_secs: 1.25,
        }
    }

    fn dump_lines(tree: &Tree, root: DirId, links: &[HardlinkLink], target: usize) -> Vec<Value> {
        let bytes = write_records(
            tree,
            root,
            links,
            &stats(),
            1_753_142_400,
            Vec::new(),
            target,
        )
        .expect("write_records");
        let text = zstd::stream::decode_all(&bytes[..]).expect("stream decode");
        let text = String::from_utf8(text).expect("valid UTF-8 JSON lines");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    #[test]
    fn header_matches_the_spec() {
        let (tree, root, links) = sample();
        let lines = dump_lines(&tree, root, &links, 1 << 20);
        let h = &lines[0];
        assert_eq!(h["t"], "h");
        assert_eq!(h["format"], "camembert-dump");
        assert_eq!(h["v"], 1);
        assert_eq!(h["minor"], 0);
        assert_eq!(h["prog"], "camembert");
        assert_eq!(h["progver"], env!("CARGO_PKG_VERSION"));
        assert_eq!(h["ts"], 1_753_142_400_u64);
        assert_eq!(h["root"], "/r");
        assert_eq!(h["dev"], "5", "dev is a string");
        assert_eq!(h["sem"], "blocks");
        assert_eq!(h["ext"], false);
        assert_eq!(h["ordered"], true);
        assert_eq!(h["allino"], false);
    }

    #[test]
    fn blocks_are_dfs_ordered_with_raw_byte_siblings() {
        let (tree, root, links) = sample();
        let lines = dump_lines(&tree, root, &links, 1 << 20);

        let d_paths: Vec<&str> = lines
            .iter()
            .filter(|l| l["t"] == "d")
            .map(|l| l["path"].as_str().unwrap())
            .collect();
        // DFS preorder, siblings raw-byte sorted: a < broken < mnt.
        assert_eq!(d_paths, ["/r", "/r/a", "/r/broken", "/r/mnt"]);

        // Root block entries: raw-byte order b.txt (0x62) < ~ (0x7E) <
        // \xff (encoded %FF) — the encoded form would sort %FF first.
        let entries: Vec<&str> = lines
            .iter()
            .filter(|l| l.get("t").is_none())
            .map(|l| l["n"].as_str().unwrap())
            .collect();
        assert_eq!(entries, ["b.txt", "~", "%FF", "bad", "leaf"]);
    }

    #[test]
    fn directory_lines_carry_counts_and_totals() {
        let (tree, root, links) = sample();
        let lines = dump_lines(&tree, root, &links, 1 << 20);
        let d = |path: &str| {
            lines
                .iter()
                .find(|l| l["t"] == "d" && l["path"] == path)
                .unwrap_or_else(|| panic!("d line for {path}"))
        };

        let r = d("/r");
        assert_eq!(r["nf"], 3);
        assert_eq!(r["nd"], 3);
        assert_eq!(r["ta"], 4096 + 10 + 20 + 30 + 4096 + 4096 + 100);
        assert_eq!(r["tn"], 1 + 6 + 2);
        assert_eq!(r["te"], 2);
        assert!(r.get("err").is_none());

        let a = d("/r/a");
        assert_eq!(a["nf"], 2);
        assert_eq!(a["nd"], 0);
        assert_eq!(a["ta"], 4096 + 100);
        assert_eq!(a["te"], 1);

        let broken = d("/r/broken");
        assert_eq!(broken["err"], true);
        assert_eq!(
            (broken["nf"].as_u64(), broken["nd"].as_u64()),
            (Some(0), Some(0))
        );
        assert_eq!(broken["tn"], 1);
        assert_eq!(broken["te"], 1);

        let mnt = d("/r/mnt");
        assert_eq!(mnt["ex"], "otherfs");
        assert_eq!(mnt["tn"], 1);
        assert_eq!(mnt["te"], 0);
        assert!(mnt.get("err").is_none());
    }

    #[test]
    fn hardlink_entries_carry_string_ino_nlink_and_foreign_dev() {
        let (tree, root, links) = sample();
        let lines = dump_lines(&tree, root, &links, 1 << 20);
        let leaf = lines.iter().find(|l| l["n"] == "leaf").unwrap();
        assert_eq!(
            leaf["i"],
            (1u64 << 60).to_string(),
            "ino is a string even above 2^53"
        );
        assert_eq!(leaf["l"], 2);
        assert_eq!(
            leaf["dev"], "6",
            "dev differs from the dir's: emitted, string"
        );

        let bad = lines.iter().find(|l| l["n"] == "bad").unwrap();
        assert_eq!(bad["err"], true);
        assert!(bad.get("i").is_none());
    }

    #[test]
    fn x_and_e_lines_close_the_dump() {
        let (tree, root, links) = sample();
        // Tiny frames: several x lines.
        let lines = dump_lines(&tree, root, &links, 96);

        let x: Vec<(u64, &str)> = lines
            .iter()
            .filter(|l| l["t"] == "x")
            .map(|l| (l["f"].as_u64().unwrap(), l["p"].as_str().unwrap()))
            .collect();
        assert!(!x.is_empty());
        // The tiny target flushes the header alone as frame 0, so the
        // root d line is the first d of a *later* frame.
        assert_eq!(x[0].1, "/r", "first indexed d path is the root");
        assert!(
            x.windows(2).all(|w| w[0].0 < w[1].0),
            "f strictly increases"
        );
        let d_paths: Vec<&str> = lines
            .iter()
            .filter(|l| l["t"] == "d")
            .map(|l| l["path"].as_str().unwrap())
            .collect();
        assert!(x.iter().all(|&(_, p)| d_paths.contains(&p)));

        let e = lines.last().unwrap();
        assert_eq!(e["t"], "e", "e is the last line");
        assert_eq!(e["entries"], 8);
        assert_eq!(e["dirs"], 2);
        assert_eq!(e["errors"], 2);
        assert_eq!(e["ta"], lines[1]["ta"], "e.ta mirrors the root d line");
        assert_eq!(e["elapsed"], 1.25);
    }
}
