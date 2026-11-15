//! Descriptor-relative Unix tree IO shared by filesystem drivers.
#![forbid(unsafe_code)]
mod directory;
pub use directory::DirectoryTree;

mod copy;
pub use copy::copy_contents;

#[cfg(target_os = "linux")]
pub use copy::clone_contents;

mod exchange;
pub use exchange::{Exchange, ExchangeLocation};
