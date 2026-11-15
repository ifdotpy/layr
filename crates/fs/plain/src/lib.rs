//! Reference filesystem drivers: real directory copies and deterministic memory trees.
#![forbid(unsafe_code)]
mod directory;
mod memory;
mod plain;
pub use directory::DirectoryTree;
pub use memory::{MemoryFs, MemoryTree};
pub use plain::PlainFs;
