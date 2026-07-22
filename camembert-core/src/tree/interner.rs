//! Append-only name interner.
//!
//! Stores **raw OS bytes** (filenames are not UTF-8 on Linux) in a single
//! byte arena, deduplicated through a hash table. Interning is append-only:
//! a [`NameRef`] handed out once stays valid for the lifetime of the
//! interner, which is what lets [`super::Node`] carry a bare `u32` name
//! reference.

use std::hash::BuildHasher;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

/// Handle to an interned name: an index into the interner's span table.
///
/// Only the low 26 bits are usable by [`super::Node`]'s packed layout, so
/// the tree supports up to 2^26 (~67 M) *unique* names. See the packing
/// notes in [`super`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameRef(pub(crate) u32);

/// Append-only byte arena + dedup table for filenames.
#[derive(Default)]
pub struct NameInterner {
    /// Concatenated raw name bytes.
    bytes: Vec<u8>,
    /// `(offset, len)` into `bytes`, indexed by `NameRef`.
    spans: Vec<(u32, u32)>,
    /// Dedup table: values are span indices, hashed/compared by name bytes.
    table: HashTable<u32>,
    hasher: FxBuildHasher,
}

impl std::fmt::Debug for NameInterner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NameInterner")
            .field("names", &self.spans.len())
            .field("arena_bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl NameInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of unique names interned so far.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Total bytes held by the name arena (budget accounting).
    pub fn arena_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Intern `name`, returning the existing handle if already present.
    pub fn intern(&mut self, name: &[u8]) -> NameRef {
        let Self {
            bytes,
            spans,
            table,
            hasher,
        } = self;
        let hash = hasher.hash_one(name);
        if let Some(&idx) = table.find(hash, |&i| {
            let (start, len) = spans[i as usize];
            &bytes[start as usize..(start + len) as usize] == name
        }) {
            return NameRef(idx);
        }
        let start = u32::try_from(bytes.len()).expect("name arena exceeds 4 GiB");
        let len = u32::try_from(name.len()).expect("name longer than u32::MAX");
        bytes.extend_from_slice(name);
        let idx = u32::try_from(spans.len()).expect("more than u32::MAX unique names");
        spans.push((start, len));
        table.insert_unique(hash, idx, |&i| {
            let (start, len) = spans[i as usize];
            hasher.hash_one(&bytes[start as usize..(start + len) as usize])
        });
        NameRef(idx)
    }

    /// Raw bytes of an interned name.
    pub fn get(&self, name: NameRef) -> &[u8] {
        let (start, len) = self.spans[name.0 as usize];
        &self.bytes[start as usize..(start + len) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_utf8_and_raw_bytes() {
        let mut interner = NameInterner::new();
        // Valid UTF-8, invalid UTF-8 (latin-1 é), and an embedded 0xFF.
        let names: [&[u8]; 4] = [b"hello", b"caf\xe9", b"\xff\xfe", b""];
        let refs: Vec<NameRef> = names.iter().map(|n| interner.intern(n)).collect();
        for (name, r) in names.iter().zip(&refs) {
            assert_eq!(interner.get(*r), *name);
        }
        assert_eq!(interner.len(), 4);
    }

    #[test]
    fn interning_deduplicates() {
        let mut interner = NameInterner::new();
        let a = interner.intern(b"src");
        let b = interner.intern(b"lib.rs");
        let c = interner.intern(b"src");
        assert_eq!(a, c);
        assert_ne!(a, b);
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.arena_bytes(), b"src".len() + b"lib.rs".len());
    }

    #[test]
    fn many_names_survive_growth() {
        let mut interner = NameInterner::new();
        let refs: Vec<NameRef> = (0..10_000)
            .map(|i| interner.intern(format!("name-{i}").as_bytes()))
            .collect();
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(interner.get(*r), format!("name-{i}").as_bytes());
        }
        // Re-interning returns the same refs.
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(interner.intern(format!("name-{i}").as_bytes()), *r);
        }
    }
}
