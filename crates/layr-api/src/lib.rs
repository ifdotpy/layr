//! Typed commands and JSON contracts shared by Layr and its consumers.
//!
//! Execute [`Command::args`] with the Layr binary, then deserialize its JSON
//! stdout as `C::Output`. Keep that association in generic runners:
//!
//! ```
//! use layr_api::{Command, StoreVersion};
//! fn decode<C: Command>(_: &C, json: &str) -> C::Output {
//!     serde_json::from_str(json).unwrap()
//! }
//! ```

#![forbid(unsafe_code)]

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::ffi::OsString;

/// The Layr release this contract ships with, maintained by release-please.
pub const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// Major version of the command/JSON contract. Bump for incompatible changes
/// to command arguments or response types; additive response fields do not bump it.
pub const API_MAJOR: u32 = 1;

mod sealed {
    pub trait Sealed {}
}

/// A command bound to its JSON response type.
///
/// Only this crate can implement commands, so consumers share the same contracts.
///
/// A runner cannot substitute one command's response for another command's
/// associated output, even when both commands use JSON:
///
/// ```compile_fail,E0308
/// use layr_api::{Command, StoreVersion};
/// fn wrong_output<C: Command>(_: &C, report: <StoreVersion as Command>::Output) -> C::Output {
///     report
/// }
/// ```
pub trait Command: sealed::Sealed {
    type Output: Serialize + DeserializeOwned;

    /// Arguments after the executable, including JSON output selection.
    fn args(&self) -> Vec<OsString>;
}

/// Inspect local store compatibility without opening or changing the store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreVersion;

impl sealed::Sealed for StoreVersion {}

impl Command for StoreVersion {
    type Output = StoreVersionReport;

    fn args(&self) -> Vec<OsString> {
        vec!["store-version".into(), "--json".into()]
    }
}

/// Build identity and storage format of a binary or store marker.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Version {
    pub format: u32,
    pub version: String,
    pub build: String,
}

/// The closed set of store states emitted by `store-version`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoreState {
    Absent,
    Empty,
    Unmarked,
    Compatible,
    Incompatible,
}

/// A metadata upgrade available on the next writable open.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Upgrade {
    pub metadata_only: bool,
    pub from_format: u32,
    pub to_format: u32,
}

/// The JSON response to [`StoreVersion`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoreVersionReport {
    pub binary: Version,
    pub store: Option<Version>,
    pub compatible: bool,
    pub state: StoreState,
    pub upgrade: Option<Upgrade>,
    pub api: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_states_are_closed_and_keep_their_wire_names() {
        for (state, name) in [
            (StoreState::Absent, "absent"),
            (StoreState::Empty, "empty"),
            (StoreState::Unmarked, "unmarked"),
            (StoreState::Compatible, "compatible"),
            (StoreState::Incompatible, "incompatible"),
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<StoreState>(&json).unwrap(), state);
        }
        assert!(serde_json::from_str::<StoreState>("\"future-state\"").is_err());
    }
}
