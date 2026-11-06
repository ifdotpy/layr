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

use super::{keys, Options, Report, Store};
use crate::gitbridge;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use layr_hashers::{ContentAddress, Digest};
use std::collections::BTreeSet;

const PACK: &str = ".pack.age";
const COMMITS: &str = ".commits.age";

#[derive(serde::Serialize, serde::Deserialize)]
struct CommitList {
    format: u32,
    commits: Vec<String>,
}

fn decode(identity: &str, body: &[u8]) -> Result<CommitList> {
    let list: CommitList = serde_json::from_slice(&keys::decrypt(identity, body)?).context("invalid Git pack commit list")?;
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

/// Send the Git objects of the imported commits that the remote does not list yet.
pub(super) fn push(store: &Store, repo: &Repo, opts: &Options, report: &mut Report) -> Result<()> {
    // A writer that cannot read the store cannot tell which commits it lists.
    if !store.backend.can_read() || !gitbridge::has_store(repo) {
        return Ok(());
    }
    let imported = repo.p.db.import_commits()?;
    if imported.is_empty() {
        return Ok(());
    }
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
    let mut listed = BTreeSet::new();
    for object in store.backend.list("git/")? {
        if object.key.ends_with(COMMITS) {
            let body = store.backend.get(&object.key)?.with_context(|| format!("{} disappeared", object.key))?;
            listed.extend(decode(&identity, &body)?.commits);
        }
    }
    let wanted: Vec<String> = imported.iter().filter(|c| !listed.contains(*c)).cloned().collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let have = gitbridge::has_commits(repo, &wanted)?;
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
        .zip(gitbridge::has_commits(repo, &listed_here)?)
        .filter_map(|(c, present)| present.then_some(c))
        .collect();
    // The list names only the commits that the pack holds: a commit that a covered commit reaches is in that
    // commit's pack already (an older history that a later import added).
    let new = gitbridge::commits_outside(repo, &new, &covered)?;
    if new.is_empty() {
        return Ok(());
    }
    let pack = gitbridge::pack_commits(repo, &new, &covered)?;
    let name = hex::encode(ContentAddress::digest(&pack));
    store.backend.put_new(&format!("git/{name}{PACK}"), keys::encrypt(&descriptor.recipient, &pack)?)?;
    let list = serde_json::to_vec(&serde_json::json!({"format": 1, "commits": new}))?;
    store.backend.put_new(&format!("git/{name}{COMMITS}"), keys::encrypt(&descriptor.recipient, &list)?)?;
    report.git_packs += 1;
    Ok(())
}

/// Install the Git packs whose commits the local Git store lacks. Returns the number installed.
pub(super) fn fetch(store: &Store, repo: &Repo, identity: &str) -> Result<usize> {
    let mut installed = 0;
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    for object in store.backend.list("git/")? {
        let Some(stem) = object.key.strip_suffix(COMMITS) else { continue };
        let body = store.backend.get(&object.key)?.with_context(|| format!("{} disappeared", object.key))?;
        let list = decode(identity, &body)?;
        gitbridge::init_store(repo)?;
        if gitbridge::has_commits(repo, &list.commits)?.iter().all(|present| *present) {
            continue;
        }
        let pack_key = format!("{stem}{PACK}");
        let body = store.backend.get(&pack_key)?.with_context(|| format!("store remote lacks {pack_key}"))?;
        let pack = keys::decrypt(identity, &body)?;
        let name = stem.strip_prefix("git/").unwrap_or(stem);
        if hex::encode(ContentAddress::digest(&pack)) != name {
            bail!("Git pack {pack_key} does not match its name");
        }
        gitbridge::install_pack(repo, &pack)?;
        named.push((pack_key, list.commits));
        installed += 1;
    }
    // A pack may name a commit that another pack holds (a pack of an older layr listed the commits that its
    // history exclusion left out): the commits are checked after all packs are in. A commit that no pack holds
    // stays missing: an imported state of it cannot be read here, and the rest of the fetch goes on.
    for (pack_key, commits) in named {
        let missing = commits.iter().zip(gitbridge::has_commits(repo, &commits)?).filter(|(_, present)| !present).count();
        if missing > 0 {
            eprintln!("layr: Git pack {pack_key} names {missing} commit(s) that no Git pack of the remote holds");
        }
    }
    Ok(installed)
}
