//! Persisted object codecs and hashing, independent of storage and transport.
#![forbid(unsafe_code)]

macro_rules! ensure {
    ($condition:expr, $error:expr $(,)?) => {
        if !$condition {
            return Err($error);
        }
    };
}

mod error;
pub use error::Error;
pub type Result<T> = std::result::Result<T, Error>;
pub mod chunks;
pub mod cloud_times;
pub mod id;
pub mod loose;
pub mod pack;
pub mod times;
pub mod tree;
pub use id::{Id, Keying, Kind};
pub use tree::{Entry, Node, Tree};
