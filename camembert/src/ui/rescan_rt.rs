//! The `r` rescan's runtime (`docs/design/subtree-rescan-decisions.md`
//! D2–D4): at most one background scan of one directory, and when each
//! grafted directory was last refreshed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use camembert_core::scan::{LiveScan, ScanError, ScanOptions, ScanOutcome, Scanner};
use camembert_core::tree::{DirId, Tree};
use tracing::info;

/// A finished rescan, ready to graft.
pub struct Landed {
    pub dir: DirId,
    pub path: PathBuf,
    pub result: Result<ScanOutcome, ScanError>,
}

struct Job {
    dir: DirId,
    path: PathBuf,
    label: String,
    live: LiveScan,
}

pub struct RescanRuntime {
    options: ScanOptions,
    job: Option<Job>,
    /// Grafted directory → unix seconds of its refresh. Only live
    /// directories are kept (see [`RescanRuntime::record`]).
    refreshed: HashMap<DirId, i64>,
}

impl RescanRuntime {
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            job: None,
            refreshed: HashMap::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.job.is_some()
    }

    /// The running rescan's directory name, for the header.
    pub fn running_label(&self) -> Option<&str> {
        self.job.as_ref().map(|job| job.label.as_str())
    }

    /// Start rescanning `dir` off the UI thread. The seed and the path are
    /// read under the read lock; the scan itself never touches the arena.
    pub fn start(&mut self, lock: &RwLock<ScanOutcome>, dir: DirId) {
        let (path, seed) = {
            let outcome = lock
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (outcome.path_of(dir), outcome.rescan_link_seed(dir))
        };
        let label = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        info!(path = %path.display(), seeded_links = seed.len(), "rescan starting");
        let live = Scanner::new(self.options.clone())
            .with_link_seed(seed)
            .scan_live(path.clone());
        self.job = Some(Job {
            dir,
            path,
            label,
            live,
        });
    }

    /// The rescan, once it has finished.
    pub fn poll(&mut self) -> Option<Landed> {
        if !self.job.as_ref()?.live.is_finished() {
            return None;
        }
        let job = self.job.take()?;
        Some(Landed {
            dir: job.dir,
            path: job.path,
            result: job.live.join(),
        })
    }

    /// Stop the running rescan without waiting for it (quitting).
    pub fn cancel(&mut self) {
        if let Some(job) = self.job.take() {
            job.live.cancel();
        }
    }

    /// Record that `dir` was just refreshed, and forget directories the
    /// graft tombstoned.
    pub fn record(&mut self, tree: &Tree, dir: DirId, now: i64) {
        self.refreshed
            .retain(|&d, _| !tree.is_removed(tree.dir(d).node));
        self.refreshed.insert(dir, now);
    }

    /// Forget every refresh: the whole outcome was replaced, and every
    /// `DirId` with it.
    pub fn reset(&mut self) {
        self.refreshed.clear();
    }

    /// Number of live refreshed directories.
    pub fn refreshed_count(&self) -> usize {
        self.refreshed.len()
    }

    /// The most recent refresh at or above `dir`.
    pub fn refreshed_at(&self, tree: &Tree, dir: DirId) -> Option<i64> {
        if self.refreshed.is_empty() {
            return None;
        }
        let mut newest = None;
        let mut cur = Some(dir);
        while let Some(d) = cur {
            if let Some(&at) = self.refreshed.get(&d) {
                newest = newest.max(Some(at));
            }
            cur = tree.dir(d).parent;
        }
        newest
    }
}
