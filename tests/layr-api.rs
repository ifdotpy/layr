/*
# ci: macos overlayfs
*/
//! Decode the actual CLI response using the consumer's associated output type.
use layr_api::{Command, StoreState, StoreVersion, Version, API_MAJOR, RELEASE};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn inspect<C: Command>(root: &Path, command: C) -> C::Output {
    let output = process::Command::new(env!("CARGO_BIN_EXE_layr"))
        .arg("--root")
        .arg(root)
        .args(command.args())
        .env("LAYR_DIRECT", "1")
        .env_remove("LAYR_SOCKET")
        .env_remove("LAYR_PROJECT")
        .env_remove("LAYR_LINE")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: C::Output = serde_json::from_slice(&output.stdout).unwrap();
    // Every typed response is also serializable, preserving the emitted JSON.
    assert_eq!(serde_json::to_value(&report).unwrap(), serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap());
    report
}

#[test]
fn cli_store_version_uses_the_shared_contract() {
    let fixture = Fixture(std::env::temp_dir().join(format!("layr-api-contract-{}", process::id())));
    fs::create_dir(&fixture.0).unwrap();
    let root = fixture.0.join("store");
    let report = inspect(&root, StoreVersion);
    assert_eq!(report.api(), API_MAJOR);
    assert_eq!(report.binary().version, RELEASE);
    assert_eq!(RELEASE, env!("CARGO_PKG_VERSION"));
    assert_eq!(report.state(), StoreState::Absent);
    assert!(report.compatible());
    assert!(report.store().is_none());
    assert!(report.upgrade().is_none());
    assert!(!root.exists());

    fs::create_dir(&root).unwrap();
    assert_eq!(inspect(&root, StoreVersion).state(), StoreState::Empty);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::create_dir(root.join("machine")).unwrap();
    fs::write(root.join("machine/id"), b"fixture").unwrap();
    let unmarked = inspect(&root, StoreVersion);
    assert_eq!(unmarked.state(), StoreState::Unmarked);
    let upgrade = unmarked.upgrade().unwrap();
    assert!(upgrade.metadata_only);
    assert_eq!(upgrade.from_format, 0);
    assert_eq!(upgrade.to_format, report.binary().format);

    let marker = root.join("store-version.json");
    fs::write(&marker, serde_json::to_vec(report.binary()).unwrap()).unwrap();
    let compatible = inspect(&root, StoreVersion);
    assert_eq!(compatible.state(), StoreState::Compatible);
    assert!(compatible.compatible());
    assert_eq!(compatible.store(), Some(report.binary()));
    let incompatible = Version { format: report.binary().format + 1, ..report.binary().clone() };
    fs::write(&marker, serde_json::to_vec(&incompatible).unwrap()).unwrap();
    let report = inspect(&root, StoreVersion);
    assert_eq!(report.state(), StoreState::Incompatible);
    assert!(!report.compatible());
    assert_eq!(report.store(), Some(&incompatible));
    assert!(report.upgrade().is_none());
}
