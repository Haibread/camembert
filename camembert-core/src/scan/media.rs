//! Storage-media detection for the auto thread-count policy (`THREADS=0`).
//!
//! The scan engine spreads directory traversal across worker threads
//! (`scan.rs`); on rotational storage, more parallel readers just adds
//! seek thrashing, while non-rotational storage (SSD/NVMe) benefits from
//! more in-flight syscalls than the historical flat cap of 8 allowed
//! (measured ~15% wall-time win at 16 threads on a 200k-file NVMe tree).
//! `THREADS=0` therefore inspects the scan root's backing device once per
//! scan and picks a worker count from [`thread_count`] accordingly.
//!
//! Three concerns are deliberately split:
//! - [`resolve_media`] does the sysfs I/O (real device probing, root
//!   injectable so tests point it at a fake tree instead of `/sys`);
//! - [`parse_mountinfo`] is a pure parser (fixture-tested, no I/O) for the
//!   `major == 0` fallback described below;
//! - [`thread_count`] is a pure function of `(cores, Media)`, unit-tested
//!   without touching a filesystem at all.
//!
//! ## The `major == 0` case (anonymous block devices)
//!
//! btrfs and similar filesystems allocate an anonymous (`major == 0`)
//! `st_dev` per subvolume/instance — there is no `/sys/dev/block` node to
//! read directly. Rather than give up, [`resolve_media`] falls back to
//! `/proc/self/mountinfo`: find the mount covering the scan root, read
//! its **source** device (the field after the ` - ` separator, e.g.
//! `/dev/nvme0n1p2`; sources that aren't `/dev/`-rooted — `tmpfs`,
//! `overlay`, … — carry no usable device and stay [`Media::Unknown`]),
//! stat that device node for its real `major:minor`, and run the normal
//! sysfs resolution on *that*.
//!
//! This is still an approximation for **multi-device btrfs** (RAID0/1/10
//! across several block devices): mountinfo's source is only ever *one*
//! member device, so a btrfs volume spanning one SSD and one HDD is
//! classified from whichever member happens to be reported — a mixed
//! volume can therefore be misclassified either way. A precise answer
//! would enumerate `/sys/fs/btrfs/<uuid>/devices/` and combine every
//! member the same conservative way [`resolve_device`] combines
//! device-mapper slaves; that is a possible refinement, not implemented
//! here (single-device btrfs, the common case, is unaffected).

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Real sysfs root used in production; tests inject a fake tree instead.
pub(crate) const DEFAULT_SYSFS_ROOT: &str = "/sys";

/// Real mountinfo path used in production; tests inject a fixture file
/// instead.
pub(crate) const DEFAULT_MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

/// Bound on device-mapper/RAID slave recursion (stacked dm targets, e.g.
/// LUKS-on-LVM-on-RAID, or an accidental cycle). Four levels covers every
/// realistic stack with room to spare.
const MAX_SLAVE_DEPTH: u32 = 4;

/// What the auto thread policy knows about the scan root's storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Media {
    /// `queue/rotational` reads `0` on the resolved device (or, for a
    /// device-mapper/RAID stack, on every slave). `via` is set when this
    /// came from the `major == 0` mountinfo fallback rather than a direct
    /// `st_dev` lookup, e.g. `Some("btrfs via /dev/nvme0n1p2")`.
    Ssd { via: Option<String> },
    /// `queue/rotational` reads `1` on the resolved device, or on ANY
    /// slave of a device-mapper/RAID stack — conservative: one spinning
    /// disk in the stack is enough to thrash on over-parallel reads.
    /// `via` as in [`Media::Ssd`].
    Hdd { device: String, via: Option<String> },
    /// Undetectable: anonymous `major:minor` (0) with no usable
    /// mountinfo fallback, a missing/unreadable sysfs node, or a
    /// device-mapper stack that bottoms out without a definite answer
    /// (containers, network filesystems, permission-denied sysfs, a
    /// multi-device btrfs member that itself can't be read, …).
    Unknown { reason: &'static str },
}

impl Media {
    /// Human-readable form for the `media` log field, e.g. `"ssd"`,
    /// `"hdd (sda rotational)"`, `"ssd (btrfs via /dev/nvme0n1p2)"`,
    /// `"unknown (anon bdev (major 0), no mountinfo device match)"`.
    pub(crate) fn describe(&self) -> String {
        match self {
            Media::Ssd { via: None } => "ssd".to_string(),
            Media::Ssd { via: Some(via) } => format!("ssd ({via})"),
            Media::Hdd { device, via: None } => format!("hdd ({device} rotational)"),
            Media::Hdd {
                device,
                via: Some(via),
            } => format!("hdd ({device} rotational, {via})"),
            Media::Unknown { reason } => format!("unknown ({reason})"),
        }
    }
}

/// Decide the auto (`THREADS=0`) worker count for a scan root.
///
/// - non-rotational: `min(cores, 16)` — more parallelism helps;
/// - rotational: `2` — enough to keep one syscall in flight while another
///   completes, without thrashing the seek head;
/// - undetectable: `min(cores * 2, 8)`, the historical safe default.
///
/// Pure and filesystem-free so the full decision table is testable
/// without a real block device; sysfs I/O lives in [`resolve_media`].
pub(crate) fn thread_count(cores: usize, media: &Media) -> usize {
    match media {
        Media::Ssd { .. } => cores.clamp(1, 16),
        Media::Hdd { .. } => 2,
        Media::Unknown { .. } => (cores * 2).clamp(1, 8),
    }
}

/// Resolve the [`Media`] backing `root_path`'s `st_dev` (`dev`), reading
/// sysfs under `sysfs_root` (`/sys` in production, a fake tree in tests).
/// Falls back to `/proc/self/mountinfo` when `dev`'s major is `0` (see the
/// module docs); `root_path` is only used for that fallback.
pub(crate) fn resolve_media(dev: u64, root_path: &Path, sysfs_root: &Path) -> Media {
    resolve_media_from(
        dev,
        root_path,
        sysfs_root,
        Path::new(DEFAULT_MOUNTINFO_PATH),
    )
}

/// [`resolve_media`] with an injectable mountinfo path (tests only; the
/// public entry point above always uses [`DEFAULT_MOUNTINFO_PATH`]).
fn resolve_media_from(
    dev: u64,
    root_path: &Path,
    sysfs_root: &Path,
    mountinfo_path: &Path,
) -> Media {
    let major = rustix::fs::major(dev);
    let minor = rustix::fs::minor(dev);
    if major == 0 {
        return resolve_anon_bdev(root_path, sysfs_root, mountinfo_path);
    }
    resolve_via_dev_block(major, minor, sysfs_root)
}

/// The `major == 0` fallback: locate the scan root's mount in
/// `mountinfo_path`, resolve its source device's real `major:minor` via
/// `stat`, and hand that to the normal sysfs resolution.
fn resolve_anon_bdev(root_path: &Path, sysfs_root: &Path, mountinfo_path: &Path) -> Media {
    let Ok(canon_root) = fs::canonicalize(root_path) else {
        return Media::Unknown {
            reason: "anon bdev (major 0), root path not canonicalizable",
        };
    };
    let Ok(contents) = fs::read_to_string(mountinfo_path) else {
        return Media::Unknown {
            reason: "anon bdev (major 0), mountinfo unreadable",
        };
    };
    let Some(device) = parse_mountinfo(&contents, &canon_root) else {
        return Media::Unknown {
            reason: "anon bdev (major 0), no mountinfo device match",
        };
    };
    // Re-derive the fstype for the `via` log annotation; `parse_mountinfo`
    // is deliberately just `(contents, path) -> Option<PathBuf>` (that's
    // the unit-tested surface), so the one extra field it doesn't expose
    // is fetched with a second, equally cheap pass over the same (tiny,
    // already-in-memory) mountinfo text.
    let fstype = mount_fstype(&contents, &canon_root).unwrap_or_else(|| "unknown-fs".to_string());
    let Some(rdev) = device_node_rdev(&device) else {
        return Media::Unknown {
            reason: "anon bdev (major 0), mountinfo device node unstattable",
        };
    };
    let major = rustix::fs::major(rdev);
    let minor = rustix::fs::minor(rdev);
    let media = resolve_via_dev_block(major, minor, sysfs_root);
    with_via(media, format!("{fstype} via {}", device.display()))
}

/// Attach the mountinfo fallback's provenance (`"<fstype> via
/// <device>"`) to a resolved [`Media::Ssd`]/[`Media::Hdd`]; an
/// [`Media::Unknown`] result is rewritten to a generic reason (the
/// original one, e.g. `"no /sys/dev/block node"`, doesn't read well
/// without also repeating the mountinfo context, and `reason` is a
/// `&'static str` so it can't carry the dynamic device path).
fn with_via(media: Media, via: String) -> Media {
    match media {
        Media::Ssd { .. } => Media::Ssd { via: Some(via) },
        Media::Hdd { device, .. } => Media::Hdd {
            device,
            via: Some(via),
        },
        Media::Unknown { .. } => Media::Unknown {
            reason: "anon bdev (major 0), mountinfo-resolved device undetectable",
        },
    }
}

/// Stat `device` (expected to be a `/dev/...` block special file) for its
/// represented device number (`st_rdev`), the `major:minor` sysfs keys
/// off of — as opposed to `device`'s own inode `st_dev`, which is
/// whatever filesystem `/dev` itself lives on (`devtmpfs`, almost
/// always). Returns `None` for anything that isn't a block device
/// (including "doesn't exist" / "not stat-able").
fn device_node_rdev(device: &Path) -> Option<u64> {
    let meta = fs::metadata(device).ok()?;
    if !meta.file_type().is_block_device() {
        return None;
    }
    Some(meta.rdev())
}

/// Resolve `major:minor` directly against sysfs: canonicalize
/// `/sys/dev/block/<major>:<minor>`, hop from a partition to its parent
/// whole-device directory, then read (or recurse through) `rotational`.
fn resolve_via_dev_block(major: u32, minor: u32, sysfs_root: &Path) -> Media {
    let link = sysfs_root
        .join("dev")
        .join("block")
        .join(format!("{major}:{minor}"));
    let Ok(device_dir) = fs::canonicalize(&link) else {
        return Media::Unknown {
            reason: "no /sys/dev/block node",
        };
    };

    let device_dir = match resolve_partition_parent(&device_dir) {
        Some(dir) => dir,
        None => {
            return Media::Unknown {
                reason: "partition has no parent device",
            };
        }
    };
    resolve_device(&device_dir, 0)
}

/// If `device_dir` is a partition (has a `partition` file — the standard
/// sysfs marker; partitions have no `queue/` of their own), resolve to
/// the parent whole-device directory. Otherwise return it unchanged.
fn resolve_partition_parent(device_dir: &Path) -> Option<PathBuf> {
    if device_dir.join("partition").is_file() {
        device_dir.parent().map(Path::to_path_buf)
    } else {
        Some(device_dir.to_path_buf())
    }
}

/// Resolve a whole-device sysfs directory: recurse into `slaves/` for
/// device-mapper/RAID stacks (bounded by [`MAX_SLAVE_DEPTH`]), otherwise
/// read `queue/rotational` directly.
///
/// Every block device in sysfs carries a `slaves/` directory by kernel
/// convention — it is **not** itself a signal of stacking; a whole-disk
/// device (an NVMe namespace, a plain SATA disk, …) has one too, just
/// empty. Only a *non-empty* `slaves/` means "this device is built on top
/// of others" (device-mapper, `md` RAID, …); an empty one is read the
/// same as a device with no `slaves/` entry at all.
fn resolve_device(device_dir: &Path, depth: u32) -> Media {
    let slaves: Vec<_> = fs::read_dir(device_dir.join("slaves"))
        .map(|entries| entries.flatten().collect())
        .unwrap_or_default();
    if slaves.is_empty() {
        return read_rotational(device_dir);
    }
    if depth >= MAX_SLAVE_DEPTH {
        return Media::Unknown {
            reason: "slave recursion depth exceeded",
        };
    }
    let mut saw_unknown = false;
    for entry in slaves {
        match fs::canonicalize(entry.path()) {
            Ok(slave_dir) => match resolve_device(&slave_dir, depth + 1) {
                Media::Hdd { device, .. } => {
                    return Media::Hdd { device, via: None };
                }
                Media::Unknown { .. } => saw_unknown = true,
                Media::Ssd { .. } => {}
            },
            Err(_) => saw_unknown = true,
        }
    }
    if saw_unknown {
        // Conservative: an undetectable slave means the stack as a whole
        // is undetectable, not "assume SSD".
        return Media::Unknown {
            reason: "device-mapper slave undetectable",
        };
    }
    Media::Ssd { via: None }
}

/// Leaf case: read `queue/rotational` off a whole-device directory.
fn read_rotational(device_dir: &Path) -> Media {
    let name = device_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("device")
        .to_string();
    match fs::read_to_string(device_dir.join("queue").join("rotational")) {
        Ok(contents) => match contents.trim() {
            "1" => Media::Hdd {
                device: name,
                via: None,
            },
            "0" => Media::Ssd { via: None },
            _ => Media::Unknown {
                reason: "queue/rotational has an unexpected value",
            },
        },
        Err(_) => Media::Unknown {
            reason: "queue/rotational missing",
        },
    }
}

// --- /proc/self/mountinfo parsing ------------------------------------
//
// Format (`proc(5)`), space-separated:
//   <id> <parent-id> <major:minor> <root> <mount-point> <options>
//     [<optional tag>...] - <fstype> <source> <super options>
// The mount point (and root) fields octal-escape whitespace/backslash
// (` ` -> `\040`, tab -> `\011`, newline -> `\012`, `\` -> `\134`) since
// the line itself is space-delimited; nothing else in the format needs
// escaping.

/// One parsed mountinfo line's fields relevant to media detection.
struct MountEntry {
    mount_point: PathBuf,
    fstype: String,
    source: String,
}

/// Undo mountinfo's octal escaping of whitespace/backslash in path
/// fields. Escapes are always `\` + exactly three octal digits, all
/// ASCII, so scanning by `char` (not raw bytes) is safe even when the
/// path itself contains multi-byte UTF-8.
fn unescape_octal(field: &str) -> String {
    let chars: Vec<char> = field.chars().collect();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < chars.len() {
        let is_escape = chars[i] == '\\'
            && i + 3 < chars.len()
            && chars[i + 1..=i + 3].iter().all(|c| ('0'..='7').contains(c));
        if is_escape {
            let value = chars[i + 1..=i + 3]
                .iter()
                .fold(0u32, |acc, c| acc * 8 + c.to_digit(8).unwrap_or(0));
            if let Some(decoded) = char::from_u32(value) {
                out.push(decoded);
            }
            i += 4;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Parse one mountinfo line, if it is well-formed. Ignores the numeric
/// ids, the `root` field, mount options, and any optional fields before
/// the `-` separator — none of those affect media detection.
fn parse_line(line: &str) -> Option<MountEntry> {
    let mut fields = line.split(' ').filter(|s| !s.is_empty());
    let _id = fields.next()?;
    let _parent_id = fields.next()?;
    let _majmin = fields.next()?;
    let _root = fields.next()?;
    let mount_point_raw = fields.next()?;
    let _options = fields.next()?;
    // Zero or more optional fields, terminated by a literal "-".
    for field in fields.by_ref() {
        if field == "-" {
            break;
        }
    }
    let fstype = fields.next()?;
    let source = fields.next()?;
    Some(MountEntry {
        mount_point: PathBuf::from(unescape_octal(mount_point_raw)),
        fstype: fstype.to_string(),
        source: source.to_string(),
    })
}

/// Among every mount whose mount point is a prefix of `root_path`
/// (component-wise, via [`Path::starts_with`] — so `/mnt1` does not
/// spuriously match `/mnt12/foo`), return the most specific one (most
/// path components): the mount that actually covers `root_path`.
fn best_mount_entry(contents: &str, root_path: &Path) -> Option<MountEntry> {
    contents
        .lines()
        .filter_map(parse_line)
        .filter(|entry| root_path.starts_with(&entry.mount_point))
        .max_by_key(|entry| entry.mount_point.components().count())
}

/// Find the mountinfo entry covering `root_path` and return its source
/// device path (e.g. `/dev/nvme0n1p2`), or `None` if there is no match or
/// the covering mount's source isn't a `/dev/` path (`tmpfs`, `overlay`,
/// network filesystems, …). Pure parsing, no I/O — the caller reads
/// `contents` from `/proc/self/mountinfo` (see [`resolve_media`]).
///
/// Only a real block device's node can be `stat`-ed for a `major:minor`;
/// non-`/dev/` sources are left for the caller to treat as undetectable
/// rather than guessed at.
pub(crate) fn parse_mountinfo(contents: &str, root_path: &Path) -> Option<PathBuf> {
    let entry = best_mount_entry(contents, root_path)?;
    entry
        .source
        .starts_with("/dev/")
        .then(|| PathBuf::from(entry.source))
}

/// The fstype of the mount covering `root_path`, for the `media` log
/// line's `via` annotation (`parse_mountinfo` deliberately doesn't
/// surface this — it's the one function this module unit-tests against
/// mountinfo fixtures, and its signature stays exactly
/// `(contents, root_path) -> Option<PathBuf>`).
fn mount_fstype(contents: &str, root_path: &Path) -> Option<String> {
    best_mount_entry(contents, root_path).map(|entry| entry.fstype)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // --- thread_count: pure policy table --------------------------------

    #[test]
    fn ssd_uses_cores_capped_at_16() {
        for (cores, expected) in [(1, 1), (4, 4), (8, 8), (16, 16), (32, 16)] {
            assert_eq!(
                thread_count(cores, &Media::Ssd { via: None }),
                expected,
                "cores={cores}"
            );
        }
    }

    #[test]
    fn hdd_is_always_two() {
        for cores in [1, 2, 4, 8, 16, 32] {
            let media = Media::Hdd {
                device: "sda".into(),
                via: None,
            };
            assert_eq!(thread_count(cores, &media), 2, "cores={cores}");
        }
    }

    #[test]
    fn unknown_keeps_historical_2x_capped_at_8() {
        let media = Media::Unknown { reason: "test" };
        for (cores, expected) in [(1, 2), (2, 4), (4, 8), (8, 8), (16, 8), (32, 8)] {
            assert_eq!(thread_count(cores, &media), expected, "cores={cores}");
        }
    }

    #[test]
    fn describe_matches_log_format() {
        assert_eq!(Media::Ssd { via: None }.describe(), "ssd");
        assert_eq!(
            Media::Hdd {
                device: "sda".into(),
                via: None,
            }
            .describe(),
            "hdd (sda rotational)"
        );
        assert_eq!(
            Media::Unknown {
                reason: "anon bdev (major 0)"
            }
            .describe(),
            "unknown (anon bdev (major 0))"
        );
        assert_eq!(
            Media::Ssd {
                via: Some("btrfs via /dev/nvme0n1p2".into())
            }
            .describe(),
            "ssd (btrfs via /dev/nvme0n1p2)"
        );
        assert_eq!(
            Media::Hdd {
                device: "sda".into(),
                via: Some("btrfs via /dev/sda1".into()),
            }
            .describe(),
            "hdd (sda rotational, btrfs via /dev/sda1)"
        );
    }

    // --- resolve_media: fake sysfs tree ---------------------------------

    /// Build `<root>/block/<name>/queue/rotational` with `rotational`
    /// as its content, returning the device directory.
    fn make_device(root: &Path, name: &str, rotational: &str) -> PathBuf {
        let dir = root.join("block").join(name);
        fs::create_dir_all(dir.join("queue")).unwrap();
        fs::write(dir.join("queue/rotational"), format!("{rotational}\n")).unwrap();
        // Real sysfs gives every block device an empty `slaves/` by
        // kernel convention, whole disks included — reproduce that here
        // so the plain-device tests exercise the same shape `resolve_device`
        // sees in production (an earlier version of this module treated
        // an empty `slaves/` as "undetectable dm stack", which broke
        // every real whole-disk device; see `empty_slaves_dir_falls_back_to_own_rotational`).
        fs::create_dir_all(dir.join("slaves")).unwrap();
        dir
    }

    /// Symlink `<root>/dev/block/<major>:<minor>` at `target`, as real
    /// sysfs does.
    fn link_dev_block(root: &Path, major: u32, minor: u32, target: &Path) {
        let dev_block = root.join("dev/block");
        fs::create_dir_all(&dev_block).unwrap();
        symlink(target, dev_block.join(format!("{major}:{minor}"))).unwrap();
    }

    fn dev(major: u32, minor: u32) -> u64 {
        rustix::fs::makedev(major, minor)
    }

    /// A root path that always exists (so `fs::canonicalize` in the
    /// `major == 0` fallback succeeds) and isn't otherwise meaningful.
    fn any_root(tmp: &Path) -> PathBuf {
        tmp.to_path_buf()
    }

    #[test]
    fn non_anon_device_ignores_root_path_and_mountinfo() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = make_device(tmp.path(), "nvme0n1", "0");
        link_dev_block(tmp.path(), 259, 0, &device_dir);

        // Bogus root path and mountinfo path: a non-zero major must never
        // touch either.
        let media = resolve_media_from(
            dev(259, 0),
            Path::new("/does/not/exist"),
            tmp.path(),
            Path::new("/does/not/exist/mountinfo"),
        );
        assert_eq!(media, Media::Ssd { via: None });
    }

    #[test]
    fn missing_dev_block_node_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let media = resolve_media_from(
            dev(8, 0),
            &any_root(tmp.path()),
            tmp.path(),
            Path::new("/nonexistent"),
        );
        assert_eq!(
            media,
            Media::Unknown {
                reason: "no /sys/dev/block node"
            }
        );
    }

    #[test]
    fn whole_device_non_rotational_is_ssd() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = make_device(tmp.path(), "nvme0n1", "0");
        link_dev_block(tmp.path(), 259, 0, &device_dir);

        assert_eq!(
            resolve_via_dev_block(259, 0, tmp.path()),
            Media::Ssd { via: None }
        );
    }

    #[test]
    fn whole_device_rotational_is_hdd_with_device_name() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = make_device(tmp.path(), "sda", "1");
        link_dev_block(tmp.path(), 8, 0, &device_dir);

        assert_eq!(
            resolve_via_dev_block(8, 0, tmp.path()),
            Media::Hdd {
                device: "sda".into(),
                via: None,
            }
        );
    }

    #[test]
    fn queue_rotational_missing_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = tmp.path().join("block/weird");
        fs::create_dir_all(&device_dir).unwrap(); // no queue/ at all
        link_dev_block(tmp.path(), 8, 0, &device_dir);

        assert_eq!(
            resolve_via_dev_block(8, 0, tmp.path()),
            Media::Unknown {
                reason: "queue/rotational missing"
            }
        );
    }

    #[test]
    fn partition_resolves_to_parent_device() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = make_device(tmp.path(), "sda", "0");
        // sda1: a partition subdirectory with a `partition` marker file
        // and no queue/ of its own.
        let partition_dir = device_dir.join("sda1");
        fs::create_dir_all(&partition_dir).unwrap();
        fs::write(partition_dir.join("partition"), "1\n").unwrap();
        link_dev_block(tmp.path(), 8, 1, &partition_dir);

        // Root filesystem is on sda1 (major:minor 8:1); rotational lives
        // on the parent whole-device directory (sda).
        assert_eq!(
            resolve_via_dev_block(8, 1, tmp.path()),
            Media::Ssd { via: None }
        );
    }

    #[test]
    fn partition_of_rotational_parent_is_hdd() {
        let tmp = tempfile::tempdir().unwrap();
        let device_dir = make_device(tmp.path(), "sdb", "1");
        let partition_dir = device_dir.join("sdb1");
        fs::create_dir_all(&partition_dir).unwrap();
        fs::write(partition_dir.join("partition"), "1\n").unwrap();
        link_dev_block(tmp.path(), 8, 17, &partition_dir);

        assert_eq!(
            resolve_via_dev_block(8, 17, tmp.path()),
            Media::Hdd {
                device: "sdb".into(),
                via: None,
            }
        );
    }

    #[test]
    fn dm_stack_all_ssd_slaves_is_ssd() {
        let tmp = tempfile::tempdir().unwrap();
        let ssd_a = make_device(tmp.path(), "nvme0n1", "0");
        let ssd_b = make_device(tmp.path(), "nvme1n1", "0");
        let dm = tmp.path().join("block/dm-0");
        fs::create_dir_all(dm.join("slaves")).unwrap();
        symlink(&ssd_a, dm.join("slaves/nvme0n1")).unwrap();
        symlink(&ssd_b, dm.join("slaves/nvme1n1")).unwrap();
        link_dev_block(tmp.path(), 253, 0, &dm);

        assert_eq!(
            resolve_via_dev_block(253, 0, tmp.path()),
            Media::Ssd { via: None }
        );
    }

    #[test]
    fn dm_stack_with_one_rotational_slave_is_hdd_conservative() {
        let tmp = tempfile::tempdir().unwrap();
        let ssd = make_device(tmp.path(), "nvme0n1", "0");
        let hdd = make_device(tmp.path(), "sdc", "1");
        let dm = tmp.path().join("block/dm-1");
        fs::create_dir_all(dm.join("slaves")).unwrap();
        symlink(&ssd, dm.join("slaves/nvme0n1")).unwrap();
        symlink(&hdd, dm.join("slaves/sdc")).unwrap();
        link_dev_block(tmp.path(), 253, 1, &dm);

        assert_eq!(
            resolve_via_dev_block(253, 1, tmp.path()),
            Media::Hdd {
                device: "sdc".into(),
                via: None,
            }
        );
    }

    #[test]
    fn dm_stack_with_undetectable_slave_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let ssd = make_device(tmp.path(), "nvme0n1", "0");
        let broken = tmp.path().join("block/mystery");
        fs::create_dir_all(&broken).unwrap(); // no queue/rotational
        let dm = tmp.path().join("block/dm-2");
        fs::create_dir_all(dm.join("slaves")).unwrap();
        symlink(&ssd, dm.join("slaves/nvme0n1")).unwrap();
        symlink(&broken, dm.join("slaves/mystery")).unwrap();
        link_dev_block(tmp.path(), 253, 2, &dm);

        assert_eq!(
            resolve_via_dev_block(253, 2, tmp.path()),
            Media::Unknown {
                reason: "device-mapper slave undetectable"
            }
        );
    }

    #[test]
    fn empty_slaves_dir_falls_back_to_own_rotational() {
        // Every real block device has an (often empty) `slaves/`
        // directory by kernel convention — a plain whole disk is not
        // exempt (confirmed against a live NVMe namespace on this
        // machine). An empty `slaves/` must be read the same as no
        // `slaves/` at all: this device's own `queue/rotational`, not
        // "undetectable".
        let tmp = tempfile::tempdir().unwrap();
        let dm = tmp.path().join("block/dm-3");
        fs::create_dir_all(dm.join("slaves")).unwrap(); // present but empty
        fs::create_dir_all(dm.join("queue")).unwrap();
        fs::write(dm.join("queue/rotational"), "0\n").unwrap();
        link_dev_block(tmp.path(), 253, 3, &dm);

        assert_eq!(
            resolve_via_dev_block(253, 3, tmp.path()),
            Media::Ssd { via: None }
        );
    }

    /// Build a chain of `depth` stacked dm devices, each the sole slave
    /// of the next, bottoming out at a real rotational device. Returns
    /// the top-level device directory.
    fn make_dm_chain(root: &Path, depth: u32) -> PathBuf {
        let mut current = make_device(root, "sdz", "1");
        for level in 0..depth {
            let dm = root.join("block").join(format!("dm-chain-{level}"));
            fs::create_dir_all(dm.join("slaves")).unwrap();
            symlink(&current, dm.join("slaves/prev")).unwrap();
            current = dm;
        }
        current
    }

    #[test]
    fn slave_recursion_within_bound_still_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        // MAX_SLAVE_DEPTH levels of dm stacking on top of one rotational
        // leaf: still within bound, so the HDD is found.
        let top = make_dm_chain(tmp.path(), MAX_SLAVE_DEPTH);
        link_dev_block(tmp.path(), 253, 9, &top);

        assert_eq!(
            resolve_via_dev_block(253, 9, tmp.path()),
            Media::Hdd {
                device: "sdz".into(),
                via: None,
            }
        );
    }

    #[test]
    fn slave_recursion_beyond_bound_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // One level past the bound: recursion must stop rather than
        // walking (or looping) indefinitely. The depth-exceeded verdict
        // is produced MAX_SLAVE_DEPTH levels down and propagates up as a
        // generic "slave undetectable" Unknown (the intermediate levels
        // don't know *why* their slave came back unknown) — what matters
        // here is that it terminates as Unknown rather than panicking,
        // looping, or misreporting SSD/HDD.
        let top = make_dm_chain(tmp.path(), MAX_SLAVE_DEPTH + 1);
        link_dev_block(tmp.path(), 253, 10, &top);

        assert!(matches!(
            resolve_via_dev_block(253, 10, tmp.path()),
            Media::Unknown { .. }
        ));
    }

    // --- major-0 (anon bdev) fallback: failure branches -----------------

    #[test]
    fn anon_bdev_root_path_not_canonicalizable_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let media = resolve_media_from(
            dev(0, 5),
            Path::new("/definitely/does/not/exist"),
            tmp.path(),
            Path::new("/proc/self/mountinfo"),
        );
        assert_eq!(
            media,
            Media::Unknown {
                reason: "anon bdev (major 0), root path not canonicalizable"
            }
        );
    }

    #[test]
    fn anon_bdev_unreadable_mountinfo_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let media = resolve_media_from(
            dev(0, 5),
            &any_root(tmp.path()),
            tmp.path(),
            &tmp.path().join("no-such-mountinfo"),
        );
        assert_eq!(
            media,
            Media::Unknown {
                reason: "anon bdev (major 0), mountinfo unreadable"
            }
        );
    }

    #[test]
    fn anon_bdev_no_matching_mount_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let mountinfo = tmp.path().join("mountinfo");
        // A mountinfo with entries, none of which contain root_path.
        fs::write(
            &mountinfo,
            "1 0 0:1 / /somewhere-else rw - ext4 /dev/sda1 rw\n",
        )
        .unwrap();
        let root = tmp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();

        let media = resolve_media_from(dev(0, 5), &root, tmp.path(), &mountinfo);
        assert_eq!(
            media,
            Media::Unknown {
                reason: "anon bdev (major 0), no mountinfo device match"
            }
        );
    }

    #[test]
    fn anon_bdev_non_dev_source_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tmp-mount");
        fs::create_dir_all(&root).unwrap();
        let mountinfo = tmp.path().join("mountinfo");
        fs::write(
            &mountinfo,
            format!("1 0 0:5 / {} rw - tmpfs tmpfs rw\n", root.to_str().unwrap()),
        )
        .unwrap();

        let media = resolve_media_from(dev(0, 5), &root, tmp.path(), &mountinfo);
        assert_eq!(
            media,
            Media::Unknown {
                reason: "anon bdev (major 0), no mountinfo device match"
            }
        );
    }

    #[test]
    fn anon_bdev_device_node_missing_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("btrfs-root");
        fs::create_dir_all(&root).unwrap();
        let mountinfo = tmp.path().join("mountinfo");
        // Source path doesn't exist (and isn't a block device even if it
        // did): device_node_rdev must reject it, not panic.
        fs::write(
            &mountinfo,
            format!(
                "1 0 0:24 / {} rw - btrfs /dev/does-not-exist rw\n",
                root.to_str().unwrap()
            ),
        )
        .unwrap();

        let media = resolve_media_from(dev(0, 24), &root, tmp.path(), &mountinfo);
        assert_eq!(
            media,
            Media::Unknown {
                reason: "anon bdev (major 0), mountinfo device node unstattable"
            }
        );
    }

    #[test]
    fn device_node_rdev_rejects_non_block_device() {
        let tmp = tempfile::tempdir().unwrap();
        let regular_file = tmp.path().join("not-a-device");
        fs::write(&regular_file, b"hello").unwrap();
        assert_eq!(device_node_rdev(&regular_file), None);

        assert_eq!(device_node_rdev(&tmp.path().join("missing")), None);
    }

    // --- parse_mountinfo: pure fixture parsing --------------------------
    //
    // One combined fixture covering: btrfs with subvol paths (root "/"
    // and "/home", both /dev/nvme0n1p2), a bind mount (same source,
    // different mount point, non-"/" root field), an ext4 mount, an
    // overlay mount (non-/dev source), a tmpfs mount (non-/dev source),
    // a mount point with an escaped space, and enough nesting ("/",
    // "/var", "/var/lib") to exercise longest-prefix selection.
    const MOUNTINFO_FIXTURE: &str = "\
17 25 0:17 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd,subvolid=256,subvol=/@
25 17 0:17 /@home /home rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd,subvolid=257,subvol=/@home
30 17 8:17 / /mnt/data rw,relatime shared:2 - ext4 /dev/sdb1 rw
90 17 8:17 /sub /mnt/data-bind rw,relatime - ext4 /dev/sdb1 rw
40 17 0:35 / /var/lib/docker/overlay2/abc123/merged rw,relatime - overlay overlay rw,lowerdir=l,upperdir=u,workdir=w
50 17 0:5 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw
60 17 8:33 / /mnt/My\\040Drive rw,relatime - ext4 /dev/sdc1 rw
70 17 0:17 /@var /var rw,relatime shared:3 - btrfs /dev/nvme0n1p2 rw,subvol=/@var
80 17 0:60 / /var/lib rw,relatime - tmpfs tmpfs rw
";

    #[test]
    fn root_mount_resolves_to_its_device() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/")),
            Some(PathBuf::from("/dev/nvme0n1p2"))
        );
    }

    #[test]
    fn nested_path_under_home_resolves_via_the_home_mount() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/home/user/docs")),
            Some(PathBuf::from("/dev/nvme0n1p2"))
        );
    }

    #[test]
    fn bind_mount_source_is_still_extracted() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/mnt/data-bind/x")),
            Some(PathBuf::from("/dev/sdb1"))
        );
    }

    #[test]
    fn longest_prefix_beats_a_shorter_covering_mount() {
        // "/mnt/data/foo" is covered by both "/" and "/mnt/data"; the
        // deeper, more specific mount must win.
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/mnt/data/foo")),
            Some(PathBuf::from("/dev/sdb1"))
        );
    }

    #[test]
    fn nested_longest_prefix_selection_among_three_covering_mounts() {
        // "/var/lib/foo/bar" is covered by three stacked mounts: "/"
        // (btrfs), "/var" (btrfs), and "/var/lib" (tmpfs). The deepest,
        // most specific one must win. Getting this wrong (e.g. picking
        // "/" or "/var") would wrongly report a real device instead of
        // the correct answer: tmpfs has no usable source, so this is
        // undetectable.
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/var/lib/foo/bar")),
            None
        );
    }

    #[test]
    fn var_itself_resolves_to_its_own_mount_not_the_root() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/var/cache")),
            Some(PathBuf::from("/dev/nvme0n1p2"))
        );
    }

    #[test]
    fn overlay_source_is_not_a_dev_path() {
        assert_eq!(
            parse_mountinfo(
                MOUNTINFO_FIXTURE,
                Path::new("/var/lib/docker/overlay2/abc123/merged/app")
            ),
            None
        );
    }

    #[test]
    fn tmpfs_source_is_not_a_dev_path() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/tmp/scratch")),
            None
        );
    }

    #[test]
    fn escaped_space_in_mount_point_is_decoded() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO_FIXTURE, Path::new("/mnt/My Drive/sub/file")),
            Some(PathBuf::from("/dev/sdc1"))
        );
    }

    #[test]
    fn component_boundary_is_respected_not_a_naive_string_prefix() {
        // "/mnt" is not itself a mount point in the fixture, but this
        // guards the general mechanism: starts_with is component-wise,
        // so a mount at "/mnt/data" must not match "/mnt/data2/foo".
        let fixture = "1 0 8:1 / /mnt/data rw - ext4 /dev/sdb1 rw\n";
        assert_eq!(parse_mountinfo(fixture, Path::new("/mnt/data2/foo")), None);
        assert_eq!(
            parse_mountinfo(fixture, Path::new("/mnt/data/foo")),
            Some(PathBuf::from("/dev/sdb1"))
        );
    }

    #[test]
    fn no_covering_mount_is_none() {
        let fixture = "1 0 8:1 / /mnt/data rw - ext4 /dev/sdb1 rw\n";
        assert_eq!(parse_mountinfo(fixture, Path::new("/unrelated/path")), None);
    }

    #[test]
    fn unescape_octal_handles_space_and_backslash() {
        assert_eq!(unescape_octal("plain"), "plain");
        assert_eq!(unescape_octal("My\\040Drive"), "My Drive");
        assert_eq!(unescape_octal("back\\134slash"), "back\\slash");
        // A lone trailing backslash (malformed/truncated escape) is kept
        // literally rather than panicking on out-of-bounds indexing.
        assert_eq!(unescape_octal("trailing\\"), "trailing\\");
    }
}
