//! End-to-end dump test: scan a real temp tree, write a v1 dump, decode
//! it with the zstd crate (and the `zstd` CLI when present), and check the
//! spec invariants: header, ordering, hardlink attribution, number
//! encoding, seek-table/`x` consistency.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use camembert_core::dump::{DumpMeta, decode_name, write_dump, write_dump_to_path};
use camembert_core::scan::{ScanOptions, Scanner};

/// Build the fixture tree:
///
/// ```text
/// root/
///   caf<0xE9>          (non-UTF-8 name, 5 B)
///   empty/
///   link1              (hardlink pair with sub/link0; canonical: root/link1)
///   locked/            (chmod 000: unreadable — skipped when running as root)
///   sl -> sub          (symlink)
///   sub/
///     data.bin         (3 KiB)
///     link0
/// ```
struct Fixture {
    dir: tempfile::TempDir,
    /// The unreadable dir exists (non-root only).
    locked: bool,
}

fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    fs::create_dir(root.join("sub")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::write(root.join("sub/data.bin"), vec![0xA5u8; 3072]).unwrap();
    fs::write(root.join("link1"), b"hardlinked twice").unwrap();
    fs::hard_link(root.join("link1"), root.join("sub/link0")).unwrap();
    std::os::unix::fs::symlink("sub", root.join("sl")).unwrap();
    let non_utf8 = std::ffi::OsStr::from_bytes(b"caf\xe9");
    fs::write(root.join(non_utf8), b"bytes").unwrap();

    // Unreadable directory — meaningless under root, which reads anything,
    // so probe instead of assuming.
    fs::create_dir(root.join("locked")).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let locked = fs::read_dir(root.join("locked")).is_err();
    if !locked {
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir(root.join("locked")).unwrap();
    }
    Fixture { dir, locked }
}

fn restore_locked(fixture: &Fixture) {
    if fixture.locked {
        let _ = fs::set_permissions(
            fixture.dir.path().join("locked"),
            fs::Permissions::from_mode(0o755),
        );
    }
}

/// Raw-byte, component-wise path comparison (spec §4), on decoded names.
fn cmp_paths(a: &str, b: &str) -> std::cmp::Ordering {
    let comps = |p: &str| -> Vec<Vec<u8>> { p.split('/').map(decode_name).collect() };
    comps(a).cmp(&comps(b))
}

/// Parse the trailing seek table: (compressed, decompressed) per frame.
fn parse_seek_table(bytes: &[u8]) -> Vec<(usize, usize)> {
    let len = bytes.len();
    assert_eq!(
        &bytes[len - 4..],
        &0x8F92_EAB1u32.to_le_bytes(),
        "seekable footer magic"
    );
    assert_eq!(bytes[len - 5], 0x00, "descriptor: no per-frame checksums");
    let count = u32::from_le_bytes(bytes[len - 9..len - 5].try_into().unwrap()) as usize;
    let start = len - (8 + count * 8 + 9);
    assert_eq!(
        &bytes[start..start + 4],
        &0x184D_2A5Eu32.to_le_bytes(),
        "skippable magic"
    );
    let payload = u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
    assert_eq!(payload as usize, count * 8 + 9);
    (0..count)
        .map(|i| {
            let at = start + 8 + i * 8;
            (
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize,
                u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize,
            )
        })
        .collect()
}

#[test]
fn dump_of_a_real_scan_holds_the_spec_invariants() {
    let fixture = build_fixture();
    let root_path = fixture.dir.path().to_path_buf();

    let scanner = Scanner::new(ScanOptions::default());
    let mut outcome = scanner.scan(&root_path).expect("scan");
    outcome.finalize_hardlinks();

    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(1_753_142_400);
    let mut bytes = Vec::new();
    write_dump(&outcome, &mut bytes, &DumpMeta { timestamp: ts }).expect("write_dump");
    restore_locked(&fixture);

    // Stock streaming decode reads everything and skips the seek table.
    let text = zstd::stream::decode_all(&bytes[..]).expect("zstd stream decode");
    let text = String::from_utf8(text).expect("JSON lines are valid UTF-8");
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("every line parses as JSON"))
        .collect();

    // Header.
    let h = &lines[0];
    assert_eq!(h["t"], "h");
    assert_eq!(h["format"], "camembert-dump");
    assert_eq!(h["v"], 1);
    assert_eq!(h["ts"], 1_753_142_400u64);
    assert_eq!(
        decode_name(h["root"].as_str().unwrap()),
        root_path.as_os_str().as_bytes()
    );
    assert!(h["dev"].is_string(), "dev is a JSON string");
    assert_eq!(h["ordered"], true);
    assert_eq!(h["sem"], "blocks");

    // d lines: one per scanned directory (the fixture has no excluded or
    // stat-failed dirs), in DFS preorder == component-wise path order.
    let d_lines: Vec<&Value> = lines.iter().filter(|l| l["t"] == "d").collect();
    assert_eq!(d_lines.len() as u64, outcome.dirs, "d-count == dirs");
    let d_paths: Vec<&str> = d_lines
        .iter()
        .map(|l| l["path"].as_str().unwrap())
        .collect();
    assert!(
        d_paths
            .windows(2)
            .all(|w| cmp_paths(w[0], w[1]) == std::cmp::Ordering::Less),
        "d lines strictly ascend component-wise: {d_paths:?}"
    );

    // The non-UTF-8 name round-trips through the encoding.
    let entry_names: Vec<Vec<u8>> = lines
        .iter()
        .filter(|l| l.get("t").is_none())
        .map(|l| decode_name(l["n"].as_str().unwrap()))
        .collect();
    assert!(
        entry_names.contains(&b"caf\xe9".to_vec()),
        "non-UTF-8 name survives: {entry_names:?}"
    );

    // Every block's entries sorted by raw name bytes.
    let mut current_block: Vec<Vec<u8>> = Vec::new();
    for line in &lines[1..] {
        if line.get("t").is_none() {
            current_block.push(decode_name(line["n"].as_str().unwrap()));
        } else {
            assert!(
                current_block.windows(2).all(|w| w[0] < w[1]),
                "entries sorted raw-byte within a block: {current_block:?}"
            );
            current_block.clear();
        }
    }

    // Hardlinks: ino is a string, nlink present; the pair is attributed to
    // the canonical owner root/link1 ("link1" < "sub" component-wise), so
    // sub's totals exclude the inode and the root counts it once.
    let link_entries: Vec<&Value> = lines
        .iter()
        .filter(|l| l.get("t").is_none() && l.get("i").is_some())
        .collect();
    assert_eq!(link_entries.len(), 2, "both links keep full metadata");
    for e in &link_entries {
        assert!(e["i"].is_string(), "ino is a JSON string: {e}");
        assert_eq!(e["l"], 2);
    }
    let link_size = fs::metadata(root_path.join("link1")).unwrap().len();
    let root_d = d_lines[0];
    let sub_d = d_lines
        .iter()
        .find(|l| l["path"].as_str().unwrap().ends_with("/sub"))
        .expect("sub d line");
    let data_size = 3072u64;
    let sub_own = fs::metadata(root_path.join("sub")).unwrap().len();
    assert_eq!(
        sub_d["ta"].as_u64().unwrap(),
        sub_own + data_size,
        "sub excludes the hardlinked inode (canonical owner is root/link1)"
    );
    assert_eq!(
        root_d["ta"].as_u64().unwrap(),
        outcome.totals.apparent,
        "root d totals == outcome totals (post-reattribution)"
    );
    assert!(
        root_d["ta"].as_u64().unwrap() >= link_size,
        "inode counted exactly once at the root"
    );

    // Unreadable dir: err:true, zero children (non-root runs only).
    if fixture.locked {
        let locked_d = d_lines
            .iter()
            .find(|l| l["path"].as_str().unwrap().ends_with("/locked"))
            .expect("locked d line");
        assert_eq!(locked_d["err"], true);
        assert_eq!(locked_d["nf"], 0);
        assert_eq!(locked_d["nd"], 0);
        assert!(outcome.errors >= 1);
    }

    // Symlink entry has k:"l"; the empty dir has a d line with no entries.
    let sl = lines
        .iter()
        .find(|l| l.get("t").is_none() && l["n"] == "sl")
        .expect("symlink entry");
    assert_eq!(sl["k"], "l");
    let empty_d = d_lines
        .iter()
        .find(|l| l["path"].as_str().unwrap().ends_with("/empty"))
        .expect("empty d line");
    assert_eq!(empty_d["tn"], 1);

    // e line: last, mirrors the outcome.
    let e = lines.last().unwrap();
    assert_eq!(e["t"], "e");
    assert_eq!(e["entries"].as_u64().unwrap(), outcome.entries);
    assert_eq!(e["dirs"].as_u64().unwrap(), outcome.dirs);
    assert_eq!(e["errors"].as_u64().unwrap(), outcome.errors);
    assert_eq!(e["ta"].as_u64().unwrap(), outcome.totals.apparent);
    assert_eq!(e["td"].as_u64().unwrap(), outcome.totals.real);

    // Seek table vs x lines: frames tile the file; every x ordinal is a
    // valid frame whose decoded content starts d-lines with the x path.
    let entries = parse_seek_table(&bytes);
    let frames_len: usize = entries.iter().map(|&(c, _)| c).sum();
    assert_eq!(frames_len + 8 + entries.len() * 8 + 9, bytes.len());
    let mut offsets = Vec::with_capacity(entries.len());
    let mut at = 0;
    for &(c, _) in &entries {
        offsets.push(at);
        at += c;
    }
    for line in lines.iter().filter(|l| l["t"] == "x") {
        let f = line["f"].as_u64().unwrap() as usize;
        let p = line["p"].as_str().unwrap();
        assert!(f < entries.len(), "x.f within the seek table");
        let (c, d) = entries[f];
        let frame = &bytes[offsets[f]..offsets[f] + c];
        let data = zstd::bulk::decompress(frame, d).expect("frame decodes independently");
        let text = String::from_utf8(data).unwrap();
        let first_d = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["t"] == "d")
            .expect("x-indexed frame contains a d line");
        assert_eq!(first_d["path"], p, "x maps the frame to its first d path");
    }
}

#[test]
fn write_to_path_is_atomic_and_zstd_cli_compatible() {
    let fixture = build_fixture();
    let root_path = fixture.dir.path().to_path_buf();
    let scanner = Scanner::new(ScanOptions::default());
    let mut outcome = scanner.scan(&root_path).expect("scan");
    outcome.finalize_hardlinks();
    restore_locked(&fixture);

    let out_dir = tempfile::tempdir().unwrap();
    let dump_path = out_dir.path().join("scan.cmbt");
    write_dump_to_path(
        &outcome,
        &dump_path,
        &DumpMeta {
            timestamp: UNIX_EPOCH + Duration::from_secs(1_753_142_400),
        },
    )
    .expect("write_dump_to_path");
    assert!(dump_path.exists());
    assert!(
        !Path::new(&format!("{}.part", dump_path.display())).exists(),
        ".part renamed away"
    );

    let bytes = fs::read(&dump_path).unwrap();
    let expected_lines = zstd::stream::decode_all(&bytes[..]).unwrap();
    let expected_count = expected_lines.iter().filter(|&&b| b == b'\n').count();

    // Interop promise: stock `zstd -dc` must decode the whole stream. Skip
    // gracefully when the binary is not installed.
    match std::process::Command::new("zstd")
        .arg("-dc")
        .arg(&dump_path)
        .output()
    {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("zstd CLI not found: skipping the interop check");
        }
        Err(err) => panic!("spawning zstd failed: {err}"),
        Ok(output) => {
            assert!(
                output.status.success(),
                "zstd -dc failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, expected_lines, "CLI and crate decode agree");
            let cli_count = output.stdout.iter().filter(|&&b| b == b'\n').count();
            assert_eq!(cli_count, expected_count);
        }
    }
}
