//! Core library for camembert: filesystem scanning, aggregation, and size
//! semantics. Frontends (TUI, GUI) depend on this crate and never the other
//! way around.

pub mod dump;
pub mod scan;
pub mod size;
pub mod tree;
pub mod view;
