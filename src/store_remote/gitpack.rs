//! The Git objects of imported commits in a store remote (`docs/remote-format.md`, "Git packs").
//!
//! An imported state keeps only its record on a machine that did not build its tree. A clone of the
//! store must still build any imported state offline, so a push also sends the Git objects of the
//! imported commits as one pack. A push writes only the commits that the remote does not list yet.
//! Two objects per push, named by the SHA-256 of the pack:
//!
//! - `git/<sha256>.pack.age`: the encrypted pack (`git pack-objects`).
//! - `git/<sha256>.commits.age`: the encrypted list of the commits the pack adds. It is written last,
//!   so a pack without a list is an unfinished upload and is ignored.
//!
//! A fetch or clone installs each pack whose commits the local Git store lacks. A store written
//! before packs had no `git/` objects: its imported states read only as built trees, and the next
//! push from a machine with the Git objects sends the pack.

use super::backend::{Backend, ObjectRead};
use super::Replica;
use super::{keys, Options, Report, Store};
use anyhow::{bail, Context, Result};
use layr_hashers::{ContentAddress, Digest};
use std::collections::BTreeSet;

const PACK: &str = ".pack.age";
const COMMITS: &str = ".commits.age";

pub(super) fn is_commit_list(key: &str) -> bool {
    key.starts_with("git/") && key.ends_with(COMMITS)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CommitList {
    format: u32,
    commits: Vec<String>,
}

/// `git/` objects stay in the root namespace through key epochs, so a pack written before the last rotation is
/// encrypted to an older epoch: try the current identity, then the older ones. A rotation on 2026-11-24 made
/// every fetch of project layr fail with "No matching keys found".
fn decrypt(backend: &Backend, identity: &str, body: &[u8]) -> Result<Vec<u8>> {
    let first = match keys::decrypt(identity, body) {
        Ok(plain) => return Ok(plain),
        Err(e) => e,
    };
    if let Backend::Epoch(epoch) = backend {
        for (_, old) in epoch.history.iter().rev() {
            if let Ok(plain) = keys::decrypt(old, body) {
                return Ok(plain);
            }
        }
    }
    Err(first)
}

fn decode(backend: &Backend, identity: &str, body: &[u8]) -> Result<CommitList> {
    let list: CommitList = serde_json::from_slice(&decrypt(backend, identity, body)?).context("invalid Git pack commit list")?;
    if list.format != 1 {
        bail!("unsupported Git pack format {}", list.format);
    }
    for c in &list.commits {
        if c.len() != 40 || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid commit id in a Git pack commit list");
        }
    }
    Ok(list)
}

/// Consume manifests in listing order. Only the gateway batches them, within the transfer budget.
fn visit_lists(backend: &Backend, identity: &str, mut visit: impl FnMut(&str, CommitList) -> Result<()>) -> Result<()> {
    let listing = {
        let _list = crate::trace::span("store Git manifest listing");
        backend.list("git/")?
    };
    let mut consume = |name: &str, body: ObjectRead| {
        let stem = name.strip_suffix(COMMITS).context("invalid Git pack commit list name")?;
        let body = body?.with_context(|| format!("{name} disappeared"))?;
        visit(stem, decode(backend, identity, &body)?)
    };
    let manifests = listing.into_iter().filter(|object| is_commit_list(&object.key));
    let Some(ceiling) = backend.read_limit().map(|limit| limit as u64) else {
        // File reads switch to the caller's credentials, which are process-wide on macOS.
        for object in manifests {
            let body = {
                let _read = crate::trace::span("store Git manifest reads");
                backend.get_within(&object.key, object.size)
            };
            consume(&object.key, body)?;
        }
        return Ok(());
    };

    let manifests: Vec<_> = manifests.collect();
    let budget = super::download::BYTE_BUDGET;
    let mut remaining = manifests.as_slice();
    while !remaining.is_empty() {
        let mut reserved = 0;
        let count = remaining
            .iter()
            .take(super::gateway::BATCH_KEYS)
            .take_while(|object| {
                let size = object.size.unwrap_or(ceiling);
                if size > ceiling {
                    return false;
                }
                reserved += size.max(1);
                reserved <= budget
            })
            .count()
            // At the front, let the backend deliver the oversized item's error without reading it.
            .max(1);
        let (window, rest) = remaining.split_at(count);
        remaining = rest;
        let names: Vec<_> = window.iter().map(|object| object.key.clone()).collect();
        let sizes: Vec<_> = window.iter().map(|object| object.size).collect();
        let mut bodies: Vec<_> = (0..count).map(|_| None).collect();
        let reads = {
            let _reads = crate::trace::span("store Git manifest reads");
            backend.visit_many_results(&names, &sizes, |i, body| {
                bodies[i] = Some(body);
                Ok(())
            })
        };
        // A trailer error follows the decoded items. A failed transfer can also leave items undelivered.
        for (name, body) in names.into_iter().zip(bodies) {
            if body.is_none() && reads.is_err() {
                break;
            }
            consume(&name, body.unwrap_or(Ok(None)))?;
        }
        reads?;
    }
    Ok(())
}

/// Send the Git objects of the imported commits that the remote does not list yet.
pub(super) fn push(store: &Store, local: &Replica<'_>, opts: &Options, report: &mut Report) -> Result<()> {
    // A writer that cannot read the store cannot tell which commits it lists. Layr Cloud mirrors Public and
    // Private projects to GitHub itself and has no Git pack route yet.
    if !store.backend.can_read() || !local.git_has_store() || store.is_api() {
        return Ok(());
    }
    let imported = local.git_imported()?;
    if imported.is_empty() {
        return Ok(());
    }
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
    let mut listed = BTreeSet::new();
    visit_lists(&store.backend, &identity, |_, list| {
        listed.extend(list.commits);
        Ok(())
    })?;
    let wanted: Vec<String> = imported.iter().filter(|c| !listed.contains(*c)).cloned().collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let have = local.git_has_commits(&wanted)?;
    let mut new = Vec::new();
    for (commit, present) in wanted.into_iter().zip(have) {
        if present {
            new.push(commit);
        } else {
            report.omitted.push(format!("Git commit {commit}: not in the local Git store, so not in the pack"));
        }
    }
    if new.is_empty() {
        return Ok(());
    }
    let listed_here: Vec<String> = listed.into_iter().collect();
    let covered: Vec<String> = listed_here
        .iter()
        .cloned()
        .zip(local.git_has_commits(&listed_here)?)
        .filter_map(|(c, present)| present.then_some(c))
        .collect();
    // The list names only the commits that the pack holds: a commit that a covered commit reaches is in that
    // commit's pack already (an older history that a later import added).
    let new = local.git_outside(&new, &covered)?;
    if new.is_empty() {
        return Ok(());
    }
    let pack = local.git_pack(&new, &covered)?;
    let name = hex::encode(ContentAddress::digest(&pack));
    store.backend.put_new(&format!("git/{name}{PACK}"), keys::encrypt(&descriptor.recipient, &pack)?)?;
    let list = serde_json::to_vec(&serde_json::json!({"format": 1, "commits": new}))?;
    store.backend.put_new(&format!("git/{name}{COMMITS}"), keys::encrypt(&descriptor.recipient, &list)?)?;
    report.git_packs += 1;
    Ok(())
}

/// Install the Git packs whose commits the local Git store lacks. Returns the number installed.
pub(super) fn fetch(store: &Store, local: &Replica<'_>, identity: &str) -> Result<usize> {
    let mut installed = 0;
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    visit_lists(&store.backend, identity, |stem, list| {
        local.git_init()?;
        if local.git_has_commits(&list.commits)?.iter().all(|present| *present) {
            return Ok(());
        }
        let pack_key = format!("{stem}{PACK}");
        let body = store.backend.get(&pack_key)?.with_context(|| format!("store remote lacks {pack_key}"))?;
        let pack = decrypt(&store.backend, identity, &body)?;
        let name = stem.strip_prefix("git/").unwrap_or(stem);
        if hex::encode(ContentAddress::digest(&pack)) != name {
            bail!("Git pack {pack_key} does not match its name");
        }
        local.git_install(&pack)?;
        named.push((pack_key, list.commits));
        installed += 1;
        Ok(())
    })?;
    // A pack may name a commit that another pack holds (a pack of an older layr listed the commits that its
    // history exclusion left out): the commits are checked after all packs are in. A commit that no pack holds
    // stays missing: an imported state of it cannot be read here, and the rest of the fetch goes on.
    for (pack_key, commits) in named {
        let missing = commits.iter().zip(local.git_has_commits(&commits)?).filter(|(_, present)| !present).count();
        if missing > 0 {
            eprintln!("layr: Git pack {pack_key} names {missing} commit(s) that no Git pack of the remote holds");
        }
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::{format, gateway, sync_tests::FakeLocal, writer};

    fn fixture() -> Result<(Store, crate::repo::Temp)> {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir)?;
        let cleanup = crate::repo::Temp::plain(dir);
        let ctx = writer::tests::context(&cleanup.path);
        Ok((writer::tests::remote(&cleanup.path, &ctx), cleanup))
    }

    #[test]
    fn git_fetch_and_push_batch_manifest_reads() -> Result<()> {
        let (mut store, _cleanup) = fixture()?;
        writer::initialize_plain(&store, format::Mode::Private)?;
        const COUNT: usize = 257;
        let commits: Vec<_> = (0..COUNT).map(|index| format!("{index:040x}")).collect();
        for (index, commit) in commits.iter().enumerate() {
            let name = format!("git/{index:064x}{COMMITS}");
            let body = serde_json::to_vec(&CommitList { format: 1, commits: vec![commit.clone()] })?;
            store.backend.put_new(&name, body)?;
        }
        store.backend.put_new(&format!("git/{:064x}{PACK}", COUNT), b"unfinished upload".to_vec())?;
        let (backend, transport) = gateway::tests::file_transport(store.backend);
        store.backend = backend;
        let local = FakeLocal::with_git_imports(commits);

        assert_eq!(fetch(&store, &local, keys::PLAIN)?, 0);
        assert_eq!(store.backend.transfer_stats().requests, 3);
        let mut report = Report::default();
        push(&store, &local, &Options::default(), &mut report)?;
        assert_eq!(report.git_packs, 0);
        assert_eq!(store.backend.transfer_stats().requests, 7);
        let log = transport.finish();
        assert_eq!(log.len(), 7);
        assert_eq!(
            log.iter()
                .filter_map(|(request, body)| request.ends_with("/batch").then_some(body["keys"].as_array()))
                .flatten()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [256, 1, 256, 1]
        );
        assert!(log.iter().all(|(request, _)| !request.ends_with(COMMITS) && !request.ends_with(PACK)));
        Ok(())
    }

    #[test]
    fn git_fetch_installs_the_first_pack_before_the_second_manifest_fails() -> Result<()> {
        for (batch_status, cdn_item, cdn_route, expected) in [
            (403, None, None, "gateway get: HTTP 403"),
            (507, Some(403), None, "gateway get: HTTP 403"),
            (507, None, Some(403), "gateway get: HTTP 403"),
            (507, None, Some(200), "CDN reply has no objects"),
        ] {
            let (mut store, _cleanup) = fixture()?;
            let (pack, denied) = seed_ordered_manifests(&store)?;
            let (backend, transport) =
                gateway::tests::file_transport_with_status(store.backend, move |route, key| match (route, key) {
                    ("batch", Some(key)) if key == denied => Some(batch_status),
                    ("cdn", Some(key)) if key == denied => cdn_item,
                    ("cdn", None) => cdn_route,
                    _ => None,
                });
            store.backend = backend;
            let local = FakeLocal::with_git_imports(Vec::new());
            let result = fetch(&store, &local, keys::PLAIN);
            let log = transport.finish();
            assert_eq!(local.git_packs.into_inner(), Some(vec![pack]));
            assert_eq!(result.err().map(|error| error.to_string()).as_deref(), Some(expected));
            assert_eq!(log.len(), 3 + usize::from(cdn_item.or(cdn_route).is_some()));
            assert_eq!(log[1].1["keys"].as_array().map(Vec::len), Some(2));
            assert!(log.last().is_some_and(|(request, _)| request.ends_with(PACK)));
        }
        Ok(())
    }

    #[test]
    fn git_fetch_reports_batch_trailers_after_consuming_manifest_results() -> Result<()> {
        for (denied, overflow, expected) in [
            (true, false, "gateway get: HTTP 403"),
            (false, false, "batch reply has bytes after its bodies"),
            (true, true, "gateway get: HTTP 403"),
        ] {
            let (mut store, cleanup) = fixture()?;
            let (pack, second) = seed_ordered_manifests(&store)?;
            std::fs::write(cleanup.path.join("remote").join(&second), br#"{"format":1,"commits":[]}"#)?;
            let (backend, transport) = gateway::tests::file_transport_with_batch_trailer(
                store.backend,
                move |route, key| match (route, key) {
                    ("batch", Some(key)) if key == second => denied.then_some(403),
                    ("batch", Some(_)) if overflow => Some(507),
                    _ => None,
                },
                b"x",
            );
            store.backend = backend;
            let local = FakeLocal::with_git_imports(Vec::new());
            let result = fetch(&store, &local, keys::PLAIN);
            let log = transport.finish();
            assert_eq!(local.git_packs.into_inner(), Some(vec![pack]));
            assert_eq!(result.err().map(|error| error.to_string()).as_deref(), Some(expected));
            assert_eq!(log.len(), if overflow { 5 } else { 3 });
            assert!(log.last().is_some_and(|(request, _)| request.ends_with(PACK)));
        }
        Ok(())
    }

    #[test]
    fn git_fetch_installs_the_first_pack_before_an_oversized_manifest() -> Result<()> {
        let (mut store, cleanup) = fixture()?;
        let (pack, oversized) = seed_ordered_manifests(&store)?;
        store.backend.put_new(&format!("git/zz{COMMITS}"), br#"{"format":1,"commits":[]}"#.to_vec())?;
        std::fs::File::options().write(true).open(cleanup.path.join("remote").join(&oversized))?.set_len(33 << 20)?;
        let (backend, transport) = gateway::tests::file_transport(store.backend);
        store.backend = backend;
        let local = FakeLocal::with_git_imports(Vec::new());
        let result = fetch(&store, &local, keys::PLAIN);
        let log = transport.finish();
        assert_eq!(local.git_packs.into_inner(), Some(vec![pack]));
        assert!(result.is_err_and(|error| error.to_string().contains(&oversized)));
        assert_eq!(log.len(), 3, "only the first manifest and pack may be read");
        assert_eq!(log[1].1["keys"].as_array().map(Vec::len), Some(1));
        assert!(log[2].0.ends_with(PACK));
        Ok(())
    }

    #[test]
    fn git_fetch_installs_the_first_pack_before_a_later_file_read_error() -> Result<()> {
        let (mut store, cleanup) = fixture()?;
        let ctx = writer::tests::context(&cleanup.path);
        store.backend.as_caller(&ctx)?;
        let (pack, denied) = seed_ordered_manifests(&store)?;
        let root = cleanup.path.join("remote");
        let threads = &super::super::backend::FILE_THREADS;
        threads.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(root.clone(), Default::default());
        let faults = &super::super::backend::FILE_FAULTS;
        faults.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(root.clone(), vec![("get", denied)]);
        let local = FakeLocal::with_git_imports(Vec::new());
        let result = fetch(&store, &local, keys::PLAIN);
        faults.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&root);
        let threads = threads.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&root);
        assert_eq!(threads, Some(std::collections::HashSet::from([std::thread::current().id()])));
        assert_eq!(local.git_packs.into_inner(), Some(vec![pack]));
        assert!(result.is_err_and(|error| error.to_string().contains("injected fault")));
        Ok(())
    }

    #[test]
    fn git_fetch_installs_the_first_pack_before_a_later_invalid_file_key() -> Result<()> {
        let (store, cleanup) = fixture()?;
        let (pack, second) = seed_ordered_manifests(&store)?;
        let root = cleanup.path.join("remote");
        std::fs::rename(root.join(second), root.join(r"git/z\bad.commits.age"))?;
        let local = FakeLocal::with_git_imports(Vec::new());
        let result = fetch(&store, &local, keys::PLAIN);
        assert_eq!(local.git_packs.into_inner(), Some(vec![pack]));
        assert!(result.is_err_and(|error| error.to_string().contains("invalid remote object key")));
        Ok(())
    }

    fn seed_ordered_manifests(store: &Store) -> Result<(Vec<u8>, String)> {
        let pack = b"fixture pack".to_vec();
        let stem = format!("git/{}", hex::encode(ContentAddress::digest(&pack)));
        let second = format!("git/z{COMMITS}");
        store.backend.put_new(&format!("{stem}{PACK}"), pack.clone())?;
        let manifest = serde_json::to_vec(&CommitList { format: 1, commits: vec!["1".repeat(40)] })?;
        store.backend.put_new(&format!("{stem}{COMMITS}"), manifest.clone())?;
        store.backend.put_new(&second, manifest)?;
        Ok((pack, second))
    }

    #[test]
    fn commit_list_selection_is_nested_but_stays_within_git() {
        assert!(is_commit_list("git/sub/bad.commits.age"));
        assert!(!is_commit_list("release/archive.commits.age"));
    }

    #[test]
    fn git_manifests_deliver_the_valid_prefix_before_a_bad_manifest() -> Result<()> {
        let (store, _cleanup) = fixture()?;
        store.backend.put_new("git/a.commits.age", br#"{"format":1,"commits":[]}"#.to_vec())?;
        store.backend.put_new("git/sub/bad.commits.age", br#"{"format":2,"commits":[]}"#.to_vec())?;
        let (backend, transport) = gateway::tests::file_transport(store.backend);
        let mut used = Vec::new();
        let result = visit_lists(&backend, keys::PLAIN, |stem, _| {
            used.push(stem.to_owned());
            Ok(())
        });
        let log = transport.finish();
        assert_eq!(used, ["git/a"]);
        assert_eq!(log.len(), 2, "a listing and one batch POST");
        assert!(result.is_err_and(|error| error.to_string() == "unsupported Git pack format 2"));
        Ok(())
    }
}
