//! Descriptor-relative Unix tree IO shared by filesystem drivers.
#![forbid(unsafe_code)]
mod directory;
pub use directory::DirectoryTree;
