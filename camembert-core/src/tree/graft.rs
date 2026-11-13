//! Post-scan subtree graft (`docs/design/subtree-rescan-decisions.md` D1):
//! replace one directory's descendants with those of a fresh scan rooted
//! at it, keeping the directory's own node and `DirId` so every handle the
//! UI holds on it or its ancestors stays valid.

use super::{ChildRun, DirId, DirMeta, NodeFlags, NodeId, Tree};

/// Fresh-tree node → session-tree node, for the nodes a graft copied.
#[derive(Debug)]
pub(crate) struct GraftMap {
    map: Vec<u32>,
}

impl GraftMap {
    const UNMAPPED: u32 = u32::MAX;

    pub(crate) fn get(&self, fresh: NodeId) -> Option<NodeId> {
        match self.map.get(fresh.index()) {
            Some(&id) if id != Self::UNMAPPED => Some(NodeId(id)),
            _ => None,
        }
    }
}

/// What [`Tree::graft`] did.
#[derive(Debug)]
pub(crate) struct Graft {
    pub(crate) map: GraftMap,
    /// Every node the graft tombstoned: the target's previous live
    /// descendants.
    pub(crate) tombstoned: Vec<NodeId>,
}

impl Tree {
    /// Replace `target`'s descendants with those of `fresh_root` in
    /// `fresh`, a scan of the same directory.
    ///
    /// The target keeps its node, name, parent and `DirId`; its own size,
    /// mtime, state and device become the fresh root's, and its error mark
    /// is cleared (a fresh scan only exists if its root could be read).
    /// Old descendants are tombstoned, never reused, and the target's run
    /// list is replaced by the fresh children's runs, appended at the end of
    /// the arena. Ancestors move by the difference between the fresh and
    /// the old subtree aggregates.
    ///
    /// Hardlink attribution across the graft boundary is the caller's job
    /// (`ScanOutcome::graft_rescan`): this copies the fresh tree's flags and
    /// counted-link markers as they are.
    ///
    /// Post-scan only, like [`Tree::apply_removal`]: the caller is the
    /// frozen arena's single writer.
    pub(crate) fn graft(&mut self, target: DirId, fresh: &Tree, fresh_root: DirId) -> Graft {
        let target_node = self.dirs[target.index()].node;
        debug_assert!(
            !self.is_removed(target_node),
            "graft onto a removed directory"
        );
        let old = Aggregates::of(&self.dirs[target.index()]);

        let tombstoned = self.tombstone_descendants(target);

        let fresh_meta = fresh.dir(fresh_root);
        let fresh_root_node = *fresh.node(fresh_meta.node);
        {
            let node = &mut self.nodes[target_node.index()];
            node.apparent = fresh_root_node.apparent;
            node.disk = fresh_root_node.disk;
            node.mtime = fresh_root_node.mtime;
        }
        self.set_flag(target_node, NodeFlags::ERROR, false);
        self.error_reasons.remove(&target_node);
        {
            let meta = &mut self.dirs[target.index()];
            meta.runs.clear();
            meta.ta = fresh_meta.ta;
            meta.td = fresh_meta.td;
            meta.tn = fresh_meta.tn;
            meta.te = fresh_meta.te;
            meta.state = fresh_meta.state;
            meta.dev = fresh_meta.dev;
        }
        if let Some(parent) = self.dirs[target.index()].parent {
            self.shift_ancestors(parent, old, Aggregates::of(fresh_meta));
        }

        let mut map = vec![GraftMap::UNMAPPED; fresh.node_count()];
        map[fresh_meta.node.index()] = target_node.0;
        let mut queue = vec![(fresh_root, target)];
        while let Some((fresh_dir, dir)) = queue.pop() {
            let dir_node = self.dirs[dir.index()].node;
            let kids: Vec<NodeId> = fresh.children(fresh_dir).collect();
            let start = u32::try_from(self.nodes.len()).expect("node arena exceeds u32 entries");
            for kid in kids.iter().copied() {
                let n = fresh.node(kid);
                let copy = self.push_node(
                    fresh.name(kid),
                    n.kind(),
                    n.flags(),
                    dir_node,
                    n.size(),
                    n.mtime(),
                );
                map[kid.index()] = copy.0;
                if let Some(reason) = fresh.excluded_reason(kid) {
                    self.excluded.insert(copy, reason);
                }
                if let Some(errno) = fresh.error_reason(kid) {
                    self.error_reasons.insert(copy, errno);
                }
                if fresh.hardlink_firsts.contains(&kid) {
                    self.hardlink_firsts.insert(copy);
                }
                if let Some(kid_dir) = fresh.dir_of(kid) {
                    let meta = fresh.dir(kid_dir);
                    let id = u32::try_from(self.dirs.len()).expect("dir arena exceeds u32 entries");
                    self.dirs.push(DirMeta {
                        node: copy,
                        parent: Some(dir),
                        runs: Vec::new(),
                        ta: meta.ta,
                        td: meta.td,
                        tn: meta.tn,
                        te: meta.te,
                        pending: 0,
                        state: meta.state,
                        dev: meta.dev,
                    });
                    let id = DirId(id);
                    self.dir_of.insert(copy, id);
                    queue.push((kid_dir, id));
                }
            }
            let len = u32::try_from(kids.len()).expect("directory exceeds u32 entries");
            self.push_run(dir, ChildRun { start, len });
        }

        Graft {
            map: GraftMap { map },
            tombstoned,
        }
    }

    /// Tombstone every live descendant of `dir` (not `dir` itself), keeping
    /// the error side table and the live directory count consistent the
    /// way [`Tree::apply_removal`] does. Aggregates are left to the caller.
    fn tombstone_descendants(&mut self, dir: DirId) -> Vec<NodeId> {
        let mut removed = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let children: Vec<NodeId> = self.children(d).collect();
            for child in children {
                self.tombstones.insert(child);
                self.error_reasons.remove(&child);
                if let Some(child_dir) = self.dir_of(child) {
                    self.removed_dirs += 1;
                    stack.push(child_dir);
                }
                removed.push(child);
            }
        }
        removed
    }

    /// Move `dir` and every ancestor from `old` to `new`, component by
    /// component. `old` is part of every ancestor's aggregate, so the
    /// subtraction cannot underflow.
    fn shift_ancestors(&mut self, dir: DirId, old: Aggregates, new: Aggregates) {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let meta = &mut self.dirs[d.index()];
            debug_assert!(
                meta.ta >= old.ta && meta.td >= old.td && meta.tn >= old.tn && meta.te >= old.te,
                "graft underflow: an ancestor holds less than the subtree it contains"
            );
            meta.ta = meta.ta - old.ta + new.ta;
            meta.td = meta.td - old.td + new.td;
            meta.tn = meta.tn - old.tn + new.tn;
            meta.te = meta.te - old.te + new.te;
            cur = meta.parent;
        }
    }

    fn set_flag(&mut self, node: NodeId, flag: NodeFlags, on: bool) {
        let n = &mut self.nodes[node.index()];
        let mut flags = n.flags();
        if on {
            flags.insert(flag);
        } else {
            flags.remove(flag);
        }
        n.name_kind = (n.name_kind & !(0b111 << super::FLAGS_SHIFT))
            | (u32::from(flags.bits()) << super::FLAGS_SHIFT);
    }

    /// Drop `node` from the counted-link side set (the inverse of
    /// `mark_hardlink_first`), when a merged hardlink group demotes it.
    pub(crate) fn unmark_hardlink_first(&mut self, node: NodeId) {
        self.hardlink_firsts.remove(&node);
    }

    /// Live excluded mount points: `(all, kernel pseudo-filesystems)`.
    pub fn live_excluded_counts(&self) -> (u64, u64) {
        let mut all = 0;
        let mut kernfs = 0;
        for (&node, &reason) in &self.excluded {
            if self.is_removed(node) {
                continue;
            }
            all += 1;
            if reason == super::ExcludedReason::KernFs {
                kernfs += 1;
            }
        }
        (all, kernfs)
    }
}

#[derive(Debug, Clone, Copy)]
struct Aggregates {
    ta: u64,
    td: u64,
    tn: u64,
    te: u32,
}

impl Aggregates {
    fn of(meta: &DirMeta) -> Self {
        Self {
            ta: meta.ta,
            td: meta.td,
            tn: meta.tn,
            te: meta.te,
        }
    }
}
