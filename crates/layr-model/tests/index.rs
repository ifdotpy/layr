use anyhow::Result;
use ed25519_dalek::SigningKey;
use layr_model::{history, index as model_index, Model, Record, StateRec};
use layr_store::{format, index::*, keys};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const MACHINE: &str = "01a12290-0000-7000-8000-000000000001";
const PROJECT: &str = "01a12290-0000-7000-8000-000000000002";
const LINE: &str = "01a12290-0000-7000-8000-000000000003";
fn id(n: usize) -> String {
    format!("01a12290-0000-7000-8001-{n:012}")
}
fn state(n: usize, parent: Option<usize>) -> StateRec {
    serde_json::from_value(json!({"id":id(n),"kind":"commit","change_id":id(100+n),"line":LINE,
        "parents":parent.map(id).into_iter().collect::<Vec<_>>(),"subvol":id(200+n),"time":n,
        "author":{"name":"Tester","email":"test@example.test"},"message":format!("state {n}")}))
    .unwrap()
}
fn fixture() -> (Vec<String>, layr_model::Project, SigningKey) {
    let key = SigningKey::from_bytes(&[7; 32]);
    let mut records = Vec::new();
    for n in 1..=4 {
        let mut r = Record::new(if n == 1 { "machine" } else { "commit" });
        r.id = id(300 + n);
        r.project = PROJECT.into();
        r.machine = MACHINE.into();
        r.seq = n as u64;
        r.time = n as i64;
        if n == 1 {
            r.data = json!({"public_key":hex::encode(key.verifying_key().to_bytes())});
        } else {
            r.states.push(state(n, if n == 2 { None } else { Some(n - 1) }));
            r.lines.insert(
                LINE.into(),
                serde_json::from_value(json!({"before":null,"after":{
                "name":"main","head":id(n),"owner":0,"machine":MACHINE}}))
                .unwrap(),
            );
        }
        records.push(r);
    }
    let lines = records
        .iter()
        .map(|r| {
            let raw = serde_json::to_string(r).unwrap();
            format!(r#"{{"r":{raw},"s":"{}"}}"#, keys::sign_bytes(raw.as_bytes(), &key))
        })
        .collect::<Vec<_>>();
    let project = Model::from_verified_lines(lines.iter().map(String::as_str), "").unwrap().projects.remove(PROJECT).unwrap();
    (lines, project, key)
}

#[test]
fn bad_head_watermark_unknown_signer_and_missing_record_use_full_verified_load() -> Result<()> {
    let (lines, project, key) = fixture();
    let identity = keys::generate_identity();
    let recipient = keys::recipient_of(&identity)?;
    let trusted = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
    let good = Index {
        v: 1,
        project: PROJECT.into(),
        machine: MACHINE.into(),
        data: Data::Heads(model_index::heads(&project, model_index::watermark(&lines)?, 1)?),
    };
    let load = |snapshot: &Index, signer: &SigningKey| {
        let bytes = snapshot.seal(&recipient, signer).unwrap();
        model_index::load_or_full(open(&identity, &bytes, &snapshot.name(), PROJECT, &trusted), PROJECT, &lines).unwrap()
    };
    assert!(load(&good, &key).used_index);
    let mut wrong_head = good.clone();
    if let Data::Heads(h) = &mut wrong_head.data {
        h.lines[0].head = id(2);
    }
    let mut wrong_watermark = good.clone();
    if let Data::Heads(h) = &mut wrong_watermark.data {
        h.watermark.get_mut(MACHINE).unwrap().hash = "0".repeat(64);
    }
    let mut missing_record = good.clone();
    if let Data::Heads(h) = &mut missing_record.data {
        h.watermark.get_mut(MACHINE).unwrap().seq = 5;
    }
    let mut unknown_signer = good.clone();
    unknown_signer.machine = id(999);
    for bad in [&wrong_head, &wrong_watermark, &missing_record, &unknown_signer] {
        let loaded = load(bad, &key);
        assert!(!loaded.used_index);
        assert!(loaded.note.unwrap().contains("Index discarded"));
        assert_eq!(loaded.project.line_list()[0].head, id(4));
    }
    let wrong_sig = load(&good, &SigningKey::from_bytes(&[9; 32]));
    assert!(!wrong_sig.used_index);
    let mut prefix_missing = lines.clone();
    prefix_missing.remove(1);
    assert!(model_index::reconcile(&good, &prefix_missing).is_err());
    Ok(())
}

fn tree(bytes: &[u8], mode: &str) -> format::Tree {
    use sha2::{Digest, Sha256};
    format::Tree {
        files: BTreeMap::from([(
            "file".into(),
            format::Entry(mode.into(), bytes.len() as u64, hex::encode(Sha256::digest(bytes)), vec![]),
        )]),
    }
}
#[test]
fn paths_cover_mode_changes_deletion_and_recreation_without_unrelated_blobs() -> Result<()> {
    let (_, project, _) = fixture();
    let empty = format::Tree { files: BTreeMap::new() };
    let t2 = tree(b"first\nshared\n", "100644");
    let t3 = tree(b"first\nshared\n", "100755");
    let t4 = tree(b"last\nshared\n", "100755");
    let paths = BTreeMap::from([
        (id(2), model_index::paths(&id(2), &empty, &t2)),
        (id(3), model_index::paths(&id(3), &t2, &t3)),
        (id(4), model_index::paths(&id(4), &t3, &t4)),
    ]);
    assert_eq!(paths[&id(3)].paths[0].old, paths[&id(3)].paths[0].new);
    let graphs = BTreeMap::from([
        (id(2), Graph { state: id(2), ancestors: vec![] }),
        (id(3), Graph { state: id(3), ancestors: vec![id(2)] }),
        (id(4), Graph { state: id(4), ancestors: vec![id(3), id(2)] }),
    ]);
    let indexed = model_index::History { project: &project, paths: &paths, graphs: &graphs };
    assert_eq!(indexed.ancestor(&id(4), 2)?, id(2));
    assert_eq!(indexed.file_history(&id(4), "file")?.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec![id(4), id(3), id(2)]);
    indexed.check_paths(&id(3), &t2, &t3)?;
    assert!(indexed.check_paths(&id(3), &t2, &t4).is_err());
    let mut reads = Vec::new();
    let blame = indexed.blame(&id(4), "file", |state, path| {
        assert_eq!(path, "file");
        reads.push(state.to_string());
        Ok(Some(if state == id(4) { b"last\nshared\n".to_vec() } else { b"first\nshared\n".to_vec() }))
    })?;
    assert_eq!(reads.len(), 3);
    assert_eq!(blame.iter().map(|l| l.state.clone().unwrap()).collect::<Vec<_>>(), vec![id(4), id(2)]);
    assert_eq!(model_index::paths(&id(4), &t4, &empty).paths[0].new, None);
    assert_eq!(model_index::paths(&id(4), &empty, &t4).paths[0].old, None);
    Ok(())
}

#[test]
fn first_parent_and_all_parent_history_share_order_filtering_and_boundaries() -> Result<()> {
    let (_, project, _) = fixture();
    let mut merge = project.state(&id(4)).unwrap().clone();
    merge.parents = vec![id(2), id(3)];
    for first_parent in [false, true] {
        let mut out = Vec::new();
        history::file_history(
            &[id(4)],
            &BTreeSet::new(),
            first_parent,
            |id| Ok(if id == merge.id { Some(merge.clone()) } else { project.state(id).cloned() }),
            |_| Ok(vec!["file".into()]),
            |p| p == "file",
            |s| {
                out.push(s.id);
                Ok(true)
            },
        )?;
        assert_eq!(out, if first_parent { vec![id(4), id(2)] } else { vec![id(4), id(3), id(2)] });
    }
    let versions = vec![
        history::FileVersion { state: None, bytes: b"working\nshared\n".to_vec() },
        history::FileVersion { state: Some(state(2, None)), bytes: b"saved\nshared\n".to_vec() },
    ];
    let lines = history::blame(&versions)?;
    assert_eq!(lines[0].state, None);
    assert!(lines[1].boundary);
    assert_eq!(history::blame(&[history::FileVersion { state: None, bytes: vec![] }])?, vec![]);
    Ok(())
}
