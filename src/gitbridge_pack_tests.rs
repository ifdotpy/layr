use super::*;
use std::os::unix::ffi::OsStrExt;

fn init(root: &Path) -> Result<()> {
    let out = Command::new("git").args(["init", "--bare", "-q"]).arg(root).output()?;
    assert!(out.status.success());
    git_out(root, &["config", "core.filemode", "true"])?;
    git_out(root, &["config", "core.ignorecase", "false"])?;
    git_out(root, &["config", "core.autocrlf", "false"])?;
    // Isolate the fixture from user attributes.
    git_out(root, &["config", "core.attributesfile", "/dev/null"])?;
    Ok(())
}

#[test]
fn initial_pack_matches_index_objects_and_commits() -> Result<()> {
    let root = std::env::temp_dir().join(format!("layr-pack-test-{}", ids::new_id()));
    std::fs::create_dir(&root)?;
    let result = (|| -> Result<()> {
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        let mut paths = Vec::new();
        for n in 0..256 {
            let path = format!("file-{n}");
            std::fs::write(source.join(&path), format!("contents {n}\n"))?;
            paths.push(path);
        }
        for (path, data) in [
            ("empty", Vec::new()),
            ("space tab\tnewline\nquote\"slash\\é", b"\0\xffbinary\n".to_vec()),
            ("large", vec![b'x'; 2 * 1024 * 1024]),
            ("run", b"#!/bin/sh\n".to_vec()),
        ] {
            std::fs::write(source.join(path), data)?;
            paths.push(path.into());
        }
        std::fs::create_dir(source.join("nested"))?;
        std::fs::write(source.join("nested/text"), "nested\n")?;
        paths.push("nested/text".into());
        std::fs::set_permissions(source.join("run"), std::fs::Permissions::from_mode(0o755))?;
        for mode in [0o654, 0o645, 0o744] {
            let path = format!("mode-{mode:o}");
            std::fs::write(source.join(&path), "execute-bit semantics\n")?;
            std::fs::set_permissions(source.join(&path), std::fs::Permissions::from_mode(mode))?;
            paths.push(path);
        }
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(b"missing\nnon-utf8-\xff"), source.join("link"))?;
        paths.push("link".into());
        let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
        let packed = root.join("packed");
        init(&packed)?;
        assert!(can_pack_tree(&packed, &source, &input)?);
        let tree = pack_tree(&packed, &source, &paths)?;
        assert!(git_out(&packed, &["for-each-ref"])?.is_empty(), "no staging ref is published");
        let count = git_out(&packed, &["count-objects", "-v"])?;
        assert!(count.lines().any(|l| l == "count: 0"), "no per-file loose writes: {count}");
        assert!(count.lines().any(|l| l == "packs: 1"), "one streamed pack: {count}");

        let loose = root.join("loose");
        init(&loose)?;
        let mut child = base_cmd(&loose)
            .env("GIT_WORK_TREE", &source)
            .args(["update-index", "--add", "--remove", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().context("missing git stdin")?.write_all(&input)?;
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(tree, git_out(&loose, &["write-tree"])?.trim());
        let state: StateRec = serde_json::from_value(serde_json::json!({
            "id": "pack-test", "kind": "commit", "subvol": "", "time": 1700000000000i64,
            "author": {"name": "Author", "email": "author@example.test"}, "message": "message\n\nTrailer: exact\n"
        }))?;
        let export = ExportContext::load(packed.clone());
        let reference = ExportContext::load(loose.clone());
        let mut parents = Vec::new();
        for message in [None, Some("second"), Some("merge\n\nTrailer: exact\n")] {
            let commit = export.commit(&state, &tree, &parents, message)?;
            assert_eq!(commit, reference.commit(&state, &tree, &parents, message)?);
            parents.push(commit);
        }
        // Read and hash every exported object, including the packed tree and all blobs.
        let mut cat = CatFile::new(&packed)?;
        for oid in git_out(&packed, &["rev-list", "--objects", "--no-object-names", parents.last().context("missing commit")?])?.lines() {
            cat.object(oid, "")?;
        }
        git_out(&packed, &["fsck", "--full", "--no-dangling"])?;
        assert_eq!(pack_tree(&packed, &source, &paths)?, tree, "repeat imports preserve identity");
        let empty = pack_tree(&packed, &source, &[])?;
        assert_eq!(empty, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        // A source failure must not publish a partial branch or prevent a retry.
        assert!(pack_tree(&packed, &source, &["missing".into()]).is_err());
        assert!(git_out(&packed, &["for-each-ref"])?.is_empty());
        assert_eq!(pack_tree(&packed, &source, &paths)?, tree);
        Ok(())
    })();
    std::fs::remove_dir_all(root)?;
    result
}

#[test]
fn initial_pack_defers_to_git_conversions_and_index_semantics() -> Result<()> {
    let root = std::env::temp_dir().join(format!("layr-pack-test-{}", ids::new_id()));
    std::fs::create_dir(&root)?;
    let result = (|| -> Result<()> {
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("text"), "line\r\n")?;
        let git = root.join("git");
        init(&git)?;
        assert!(can_pack_tree(&git, &source, b"text\0")?);
        for (key, value, reset) in
            [("core.autocrlf", "input", "false"), ("core.filemode", "false", "true"), ("core.ignorecase", "true", "false")]
        {
            git_out(&git, &["config", key, value])?;
            assert!(!can_pack_tree(&git, &source, b"text\0")?, "{key}={value}");
            git_out(&git, &["config", key, reset])?;
        }
        for attrs in [source.join(".gitattributes"), git.join("info/attributes"), root.join("global-attributes")] {
            if attrs.file_name().is_some_and(|name| name == "global-attributes") {
                git_out(&git, &["config", "core.attributesfile", attrs.to_str().context("non-UTF-8 attributes path")?])?;
            }
            std::fs::write(&attrs, "text text eol=lf\n")?;
            assert!(!can_pack_tree(&git, &source, b"text\0")?, "{}", attrs.display());
            std::fs::remove_file(attrs)?;
        }
        // Large attribute responses must not deadlock against the input pipe.
        std::fs::write(source.join(".gitattributes"), "* filter=clean\n")?;
        let paths = b"text\0".repeat(20000);
        assert!(!can_pack_tree(&git, &source, &paths)?);
        Ok(())
    })();
    std::fs::remove_dir_all(root)?;
    result
}
