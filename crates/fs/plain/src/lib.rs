//! Reference filesystem drivers: real directory copies and deterministic memory trees.
#![forbid(unsafe_code)]
mod memory;
mod plain;
pub use layr_fs_unix::DirectoryTree;
pub use memory::{MemoryFs, MemoryTree};
pub use plain::PlainFs;
