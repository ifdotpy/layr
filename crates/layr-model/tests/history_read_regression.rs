//! Generated output fixtures captured from the pre-optimization read algorithms.
use layr_model::{
    history::{self, FileVersion},
    StateRec,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn state(n: usize) -> StateRec {
    serde_json::from_value(json!({
        "id": format!("{n:04}"), "kind": if n == 6 { "cache" } else { "commit" },
        "parents": (n.saturating_sub(2)..n).rev().map(|p| format!("{p:04}")).collect::<Vec<_>>(),
        "subvol": "fixture", "time": if n % 4 == 0 { 32 - n } else { n / 3 },
        "author": {"name":"Tester", "email":"test@example.test"}
    }))
    .unwrap()
}

#[test]
fn generated_history_order_limits_exclusions_and_decode_counts() {
    let records: BTreeMap<_, _> = (0..20)
        .map(|n| {
            let s = state(n);
            (s.id.clone(), (s.kind, serde_json::to_string(&s).unwrap()))
        })
        .collect();
    let expected: Vec<(Vec<String>, usize, usize)> = serde_json::from_str(include_str!("history-order-regression.json")).unwrap();
    let mut expected = expected.into_iter();
    for first_parent in [false, true] {
        for limit in [1, 3, usize::MAX] {
            for exclude in [None, Some("0005")] {
                let starts = vec!["0019".into(), "0018".into(), "0019".into(), "missing".into()];
                let excluded: BTreeSet<_> = exclude.map(String::from).into_iter().collect();
                let run = || {
                    let mut output = Vec::new();
                    let mut decodes = 0;
                    let mut loads = BTreeMap::<String, usize>::new();
                    let fetch = |id: &str| {
                        *loads.entry(id.into()).or_default() += 1;
                        records
                            .get(id)
                            .map(|(_, s)| {
                                decodes += 1;
                                serde_json::from_str(s).map_err(Into::into)
                            })
                            .transpose()
                    };
                    let changed = |s: &StateRec| Ok(vec![if s.id.ends_with('2') { "skip" } else { "file" }.into()]);
                    let matches = |p: &str| p == "file";
                    let accept = |s: StateRec| {
                        output.push(s.id);
                        Ok(output.len() < limit)
                    };
                    history::file_history(&starts, &excluded, first_parent, fetch, changed, matches, accept).unwrap();
                    (output, decodes, loads)
                };
                let (expected_ids, old_decodes, expected_decodes) = expected.next().unwrap();
                let (actual, new_decodes, loads) = run();
                assert_eq!(actual, expected_ids);
                assert_eq!(new_decodes, expected_decodes);
                assert!(new_decodes < old_decodes);
                assert!(loads
                    .iter()
                    .filter(|(id, _)| records.get(*id).is_some_and(|(kind, _)| !kind.auxiliary()))
                    .all(|(_, n)| *n == 1));
                eprintln!(
                    "history first_parent={first_parent} limit={limit} exclude={exclude:?} decodes={old_decodes}->{new_decodes}"
                );
            }
        }
    }
    assert!(expected.next().is_none());
}

fn versions(lines: usize) -> Vec<FileVersion> {
    (0..5)
        .map(|version| FileVersion {
            state: (version != 0).then(|| state(4 - version)),
            bytes: (0..lines)
                .filter(|i| (i + version) % 7 != 0)
                .flat_map(|i| {
                    let ending = if i + 1 == lines { "" } else { "\n" };
                    format!("line {}{ending}", if i % 11 == 0 { i + version } else { i }).into_bytes()
                })
                .collect(),
        })
        .collect()
}

#[test]
fn generated_blame_matches_previous_bytes() {
    let expected: Vec<Vec<history::BlameLine>> = serde_json::from_str(include_str!("blame-regression.json")).unwrap();
    let actual: Vec<_> = [0, 1, 2, 17, 64].into_iter().map(|lines| history::blame(&versions(lines)).unwrap()).collect();
    assert_eq!(actual, expected);
}

#[test]
#[ignore = "generated allocation measurement"]
fn measure_blame_allocations() {
    std::hint::black_box(history::blame(&versions(2_000)).unwrap());
}
