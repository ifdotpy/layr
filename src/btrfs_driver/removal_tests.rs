use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn restore_original(f: &Fixture, public: &Path, moved: &Path, ownership: OwnedTree) -> anyhow::Result<()> {
    crate::fs::remove(public)?;
    std::fs::rename(moved, public)?;
    f.driver.remove(ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    f.no_stages()?;
    Ok(())
}

#[test]
fn replacement_between_precheck_and_capture_is_restored_without_deletion() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let public = f.path.join("source");
    let moved = f.path.join("moved");
    let (a, b) = (public.clone(), moved.clone());
    let mut replaced = false;
    f.driver.set_fault(move |point| {
        if point == "quarantine-rename" && !replaced {
            replaced = true;
            std::fs::rename(&a, &b)?;
            backend(crate::fs::create(&a))?;
            std::fs::write(a.join("sentinel"), b"foreign")?;
        }
        Ok(())
    })?;
    drop(source.tree);
    let failure = f.driver.remove(source.ownership).unwrap_err();
    assert!(matches!(failure.error, FsError::LocationChanged));
    assert_eq!(std::fs::read(public.join("sentinel"))?, b"foreign");
    assert_eq!(std::fs::read(moved.join("kept"))?, b"source");
    f.no_stages()?;
    restore_original(&f, &public, &moved, failure.ownership)
}

#[test]
fn failed_restoration_retains_foreign_custody_and_original_authority() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let public = f.path.join("source");
    let moved = f.path.join("moved");
    let (a, b) = (public.clone(), moved.clone());
    let (mut replaced, mut blocked) = (false, false);
    f.driver.set_fault(move |point| {
        if point == "quarantine-rename" && !replaced {
            replaced = true;
            std::fs::rename(&a, &b)?;
            backend(crate::fs::create(&a))?;
            std::fs::write(a.join("sentinel"), b"first")?;
        }
        if point == "restore-custody" && !blocked {
            blocked = true;
            backend(crate::fs::create(&a))?;
            std::fs::write(a.join("sentinel"), b"second")?;
        }
        Ok(())
    })?;
    drop(source.tree);
    let failure = f.driver.remove(source.ownership).unwrap_err();
    let held = {
        let trees = f.driver.trees()?;
        let Resource::Pinned(pinned) = trees.get(&f.driver.issuer.owned_key(&failure.ownership)?).unwrap() else {
            panic!("custody record absent");
        };
        let Placement::Custody { stage, .. } = &pinned.placement else {
            panic!("foreign candidate was not retained");
        };
        f.path.join(OsStr::from_bytes(stage.operation.name.as_bytes())).join("tree/sentinel")
    };
    assert_eq!(std::fs::read(&held)?, b"first");
    assert_eq!(std::fs::read(public.join("sentinel"))?, b"second");
    assert_eq!(std::fs::read(moved.join("kept"))?, b"source");
    let failure = f.driver.remove(failure.ownership).unwrap_err();
    assert_eq!(std::fs::read(&held)?, b"first");
    crate::fs::remove(&public)?;
    let failure = f.driver.remove(failure.ownership).unwrap_err();
    assert!(matches!(failure.error, FsError::LocationChanged));
    assert_eq!(std::fs::read(public.join("sentinel"))?, b"first");
    restore_original(&f, &public, &moved, failure.ownership)
}

#[test]
fn capture_and_post_destruction_failures_retry_without_redeleting() -> anyhow::Result<()> {
    for phase in ["quarantined", "quarantine-inspect", "destroyed", "directory-cleanup"] {
        let Some(f) = Fixture::new()? else {
            return Ok(());
        };
        let source = f.source()?;
        let destroys = Arc::new(AtomicUsize::new(0));
        let count = destroys.clone();
        let mut failed = false;
        f.driver.set_fault(move |point| {
            if point == "destroy-root" {
                count.fetch_add(1, Ordering::SeqCst);
            }
            if point == phase && !failed {
                failed = true;
                return Err(std::io::Error::from_raw_os_error(rustix::io::Errno::IO.raw_os_error()).into());
            }
            Ok(())
        })?;
        drop(source.tree);
        let failure = f.driver.remove(source.ownership).unwrap_err();
        f.driver.remove(failure.ownership).map_err(|e| anyhow::anyhow!("{phase}: {}", e.error))?;
        assert_eq!(destroys.load(Ordering::SeqCst), 1, "phase {phase}");
        assert!(!f.path.join("source").exists());
        f.no_stages()?;
    }
    Ok(())
}

#[test]
fn nested_root_moved_out_after_open_is_not_followed() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let original = f.path.join("source");
    crate::fs::create(&original.join("nested"))?;
    crate::fs::create(&original.join("nested/deeper"))?;
    std::fs::write(original.join("nested/deeper/secret"), b"outside survives")?;
    let outside = f.path.join("outside");
    std::fs::create_dir(&outside)?;
    let parent = File::open(&outside)?;
    let root = f
        .driver
        .trees()?
        .get(&f.driver.issuer.working_key(&source.tree)?)
        .and_then(Resource::pinned)
        .unwrap()
        .reader
        .directory
        .root_file()
        .try_clone()?;
    let mut moved = false;
    f.driver.set_fault(move |point| {
        if point == "nested-opened" && !moved {
            moved = true;
            io(rustix::fs::renameat_with(&root, "nested", &parent, "moved", rustix::fs::RenameFlags::NOREPLACE))?;
        }
        Ok(())
    })?;
    drop(source.tree);
    f.driver.remove(source.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    assert!(!original.exists());
    assert_eq!(std::fs::read(outside.join("moved/deeper/secret"))?, b"outside survives");
    f.no_stages()?;
    Ok(())
}
