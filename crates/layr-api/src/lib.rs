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

impl StoreState {
    /// Whether a command can safely open this store with the current binary.
    pub const fn compatible(self) -> bool {
        match self {
            Self::Absent | Self::Empty | Self::Unmarked | Self::Compatible => true,
            Self::Incompatible => false,
        }
    }
}

/// A metadata upgrade available on the next writable open.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Upgrade {
    pub metadata_only: bool,
    pub from_format: u32,
    pub to_format: u32,
}

/// The JSON response to [`StoreVersion`], validated on construction and decoding.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "WireReport", into = "WireReport")]
pub struct StoreVersionReport {
    binary: Version,
    store: Option<Version>,
    state: StoreState,
    upgrade: Option<Upgrade>,
    api: u32,
}

/// A report whose state, stored format, or upgrade contradicts its other fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidReport;

impl std::fmt::Display for InvalidReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("contradictory store-version report")
    }
}

impl std::error::Error for InvalidReport {}

impl StoreVersionReport {
    /// Construct a report with consistent store identity and upgrade metadata.
    pub fn new(
        binary: Version,
        store: Option<Version>,
        state: StoreState,
        upgrade: Option<Upgrade>,
        api: u32,
    ) -> Result<Self, InvalidReport> {
        let valid_store = match state {
            StoreState::Absent | StoreState::Empty | StoreState::Unmarked => store.is_none(),
            StoreState::Compatible => store.as_ref().is_some_and(|stored| stored.format == binary.format),
            StoreState::Incompatible => store.as_ref().is_none_or(|stored| stored.format != binary.format),
        };
        let valid_upgrade = match (state, &upgrade) {
            (StoreState::Unmarked, Some(upgrade)) => {
                upgrade.metadata_only && upgrade.from_format == 0 && upgrade.to_format == binary.format
            }
            (StoreState::Unmarked, None) | (_, Some(_)) => false,
            (_, None) => true,
        };
        if !valid_store || !valid_upgrade {
            return Err(InvalidReport);
        }
        Ok(Self { binary, store, state, upgrade, api })
    }

    pub fn binary(&self) -> &Version {
        &self.binary
    }

    pub fn store(&self) -> Option<&Version> {
        self.store.as_ref()
    }

    pub const fn state(&self) -> StoreState {
        self.state
    }

    pub const fn compatible(&self) -> bool {
        self.state.compatible()
    }

    pub fn upgrade(&self) -> Option<&Upgrade> {
        self.upgrade.as_ref()
    }

    pub const fn api(&self) -> u32 {
        self.api
    }
}

// The wire alone carries the redundant compatibility flag; the public model
// derives it from the state and exposes no mutable invariant fields.
#[derive(Deserialize, Serialize)]
struct WireReport {
    binary: Version,
    store: Option<Version>,
    compatible: bool,
    state: StoreState,
    upgrade: Option<Upgrade>,
    api: u32,
}

impl TryFrom<WireReport> for StoreVersionReport {
    type Error = InvalidReport;

    fn try_from(wire: WireReport) -> Result<Self, Self::Error> {
        if wire.compatible != wire.state.compatible() {
            return Err(InvalidReport);
        }
        Self::new(wire.binary, wire.store, wire.state, wire.upgrade, wire.api)
    }
}

impl From<StoreVersionReport> for WireReport {
    fn from(report: StoreVersionReport) -> Self {
        Self {
            compatible: report.compatible(),
            binary: report.binary,
            store: report.store,
            state: report.state,
            upgrade: report.upgrade,
            api: report.api,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contradictory_reports_are_rejected() {
        let valid = serde_json::json!({
            "binary": {"format": 1, "version": RELEASE, "build": "test"},
            "store": null,
            "compatible": true,
            "state": "absent",
            "upgrade": null,
            "api": API_MAJOR
        });
        let mut cases = Vec::new();
        let mut contradictory = valid.clone();
        contradictory["state"] = "incompatible".into();
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["upgrade"] = serde_json::json!({"metadataOnly": true, "fromFormat": 0, "toFormat": 1});
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["state"] = "compatible".into();
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["state"] = "unmarked".into();
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["store"] = valid["binary"].clone();
        cases.push(contradictory);
        let upgrade = serde_json::json!({"metadataOnly": true, "fromFormat": 0, "toFormat": 1});
        for (field, value) in [("metadataOnly", false.into()), ("fromFormat", 1.into()), ("toFormat", 2.into())] {
            let mut contradictory = valid.clone();
            contradictory["state"] = "unmarked".into();
            contradictory["upgrade"] = upgrade.clone();
            contradictory["upgrade"][field] = value;
            cases.push(contradictory);
        }
        let mut contradictory = valid.clone();
        contradictory["state"] = "incompatible".into();
        contradictory["compatible"] = false.into();
        contradictory["upgrade"] = upgrade;
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["state"] = "incompatible".into();
        contradictory["compatible"] = false.into();
        contradictory["store"] = valid["binary"].clone();
        cases.push(contradictory);
        let mut contradictory = valid.clone();
        contradictory["compatible"] = false.into();
        cases.push(contradictory);
        for json in cases {
            assert!(serde_json::from_value::<StoreVersionReport>(json.clone()).is_err(), "accepted {json}");
        }
    }

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
