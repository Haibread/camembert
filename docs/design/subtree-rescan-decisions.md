# Subtree rescan — decisions (co-design session, 2026-11-13)

Outcome of the co-design session over
[subtree-rescan-dossier.md](subtree-rescan-dossier.md), which holds the
options, the attack and the displacement answer. Settled; reopening one
requires a new element. Covers
[issue #35](https://github.com/Haibread/camembert/issues/35).

## D1 — Graft, with hardlink groups merged across the boundary

Option B of the dossier. A rescan scans one directory with the
session's `ScanOptions` and grafts the result into the session tree. The
directory's node and `DirId` survive, its old descendants are
tombstoned, the fresh descendants are appended as new child runs, and
the difference between the fresh and old subtree aggregates moves up
the ancestor chain.

Hardlink groups touched by the graft are those with a link tombstoned by
it, or a link in the fresh subtree. They are re-canonicalised on their
live links: exactly one counted link per group, the canonical one (dump
D2's smallest raw path). That restores the invariant a full scan
establishes, for every link the original scan registered. The group's
link count is refreshed too: a fresh link's `nlink` is current, and an
inode the rescan no longer reaches loses exactly the links the graft
tombstoned. An inode left with a single link, where link counts are
known, leaves the registry, because a full scan would not register it. A link created
after the scan between the subtree and the outside is invisible, because
the outside is not re-observed. That limit is stated in the README and
`--help`, not patched.

Rescanning the root replaces the whole outcome rather than grafting, so
a full rescan never doubles the arena.

## D2 — `r` acts on the directory row under the cursor

Same targeting as `o`/`y`. On a file row it flashes, like any key with
no meaning on that row. Excluded rows (mount points, kernel
filesystems) are refused, as deletion refuses them. In flat view, `r`
rescans the row's containing directory. With the cursor on nothing (an
empty directory), `r` rescans the directory being viewed, which is how
the root is reached. Post-scan only, on both platforms.

## D3 — Background rescan, swap at the end

The rescan runs off the UI thread while the old subtree stays on
screen. The header status reads `rescanning <name>` with the spinner,
where it otherwise reads `done`. On completion the graft runs under
the write lock, inside the same envelope a deletion uses (floor
cancelled first, then epoch bump, oracle invalidation, floor respawn,
view rebuild). One rescan at a time. `q` cancels it with the session.

A rescan whose root can no longer be opened grafts nothing. `ENOENT`
removes the directory from the tree, because that is what the
filesystem now says; any other failure keeps the old subtree and says
why in a toast. A cancelled rescan grafts nothing.

## D4 — Freshness is shown, never implied

The session records when each grafted directory was refreshed. The
selection card shows `refreshed 2 min ago` for any entry at or under a
refreshed directory (the most recent refresh on its ancestor chain).
The header's `done` status gains `· N refreshed` once at least one
graft has landed. A dump written with `-o` is the scan's instant and is not
rewritten.

## D5 — Windows seeds the file-id registry; one landing

Under `LinkPolicy::FileIdRepeat` a link is registered only on a repeat
sighting, so a subtree scan cannot see that its first sighting of an
inode is a repeat of a link outside. The rescan seeds the owner with the
file ids of live registered groups that have a link outside the
directory. A seeded id's first sighting is promoted on the spot, exactly
like a repeat. Unix needs no seed, because `nlink > 1` registers every
multi-link inode on its own. Both platforms ship in one change, with the
Windows bench before/after that the scan-owner touch requires.
