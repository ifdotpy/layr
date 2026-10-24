//! Shared encrypted store format, keys, object transport, and bounded HTTP/1.
#![deny(unsafe_code)]

pub mod aws;
pub mod canonical;
pub mod format;
pub mod http;
pub mod keys;
pub mod paths;
pub mod s3;
pub mod zst;

pub mod person;
