//! Check results that Layr Cloud signs (`check.server` records, docs/checks.md).
//!
//! On Public and Private projects the server runs CI jobs. A job reports its result with its short-lived job
//! token (`POST /checks`); the server writes a `check.server` record whose data the server key signs. Line
//! protection and the merge queue take such a result when the key is the one pinned for a remote of the
//! project (`cloud.remote.<name>.server-key`), so a CI job needs no long-lived machine identity that this
//! project trusts.

use crate::repo::Repo;
use anyhow::{bail, Result};
#[cfg(test)]
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

/// The record operation of a server check result.
pub const OP: &str = "check.server";
const DOMAIN: &[u8] = b"layr/check.server/v1\0";

/// The data of a `check.server` record. `sig` signs the other fields (canonical JSON after `DOMAIN`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerCheck {
    pub state: String,
    pub check: String,
    pub platform: String,
    pub exit: i64,
    pub job: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sig: String,
}

impl ServerCheck {
    fn signed_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.sig.clear();
        let mut bytes = DOMAIN.to_vec();
        bytes.extend(layr_store::canonical::canonical(&serde_json::to_value(unsigned)?)?);
        Ok(bytes)
    }

    /// Signs the result with the server key. The server does this; layr signs only in tests.
    #[cfg(test)]
    pub fn sign(&mut self, key: &SigningKey) -> Result<()> {
        self.sig = layr_store::keys::sign_bytes(&self.signed_bytes()?, key);
        Ok(())
    }

    pub fn verify(&self, key: &VerifyingKey) -> bool {
        !self.sig.is_empty()
            && self.signed_bytes().is_ok_and(|bytes| layr_store::keys::verify_bytes(&bytes, &self.sig, key).is_ok())
    }
}

/// Whether `key` is a config key of a pinned server key: `cloud.remote.<name>.server-key`.
pub fn is_key_setting(key: &str) -> bool {
    key.strip_prefix("cloud.remote.").and_then(|rest| rest.strip_suffix(".server-key")).is_some_and(|name| !name.is_empty())
}

/// A server key as the config keeps it: 64 lowercase hex digits of an Ed25519 public key.
pub fn parse_key(text: &str) -> Result<VerifyingKey> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("a server key is 64 lowercase hex digits (an Ed25519 public key)");
    }
    let bytes: [u8; 32] = hex::decode(text)?.try_into().map_err(|_| anyhow::anyhow!("invalid server key"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

/// The server keys that the remotes of this project pin.
pub fn pinned_keys(repo: &Repo) -> Result<Vec<VerifyingKey>> {
    let mut keys = Vec::new();
    for (key, _) in repo.p.db.config_all()? {
        if !is_key_setting(&key) {
            continue;
        }
        if let Some(text) = repo.p.config_value(&key)?.and_then(|v| v.as_str().map(str::to_owned)) {
            keys.push(parse_key(&text)?);
        }
    }
    Ok(keys)
}

/// Whether a server check result signed by a pinned server key says that check `check` passed (exit 0) on
/// exactly `state`, on a platform that meets `platform` when one is asked.
pub fn passed(repo: &Repo, state: &str, check: &str, platform: Option<&str>) -> Result<bool> {
    let keys = pinned_keys(repo)?;
    if keys.is_empty() {
        return Ok(false);
    }
    for data in repo.p.db.server_checks(state)? {
        let Ok(result) = serde_json::from_value::<ServerCheck>(data) else { continue };
        if result.state != state || result.check != check || result.exit != 0 {
            continue;
        }
        if platform.is_some_and(|want| !crate::cmd::checks::meets(want, &result.platform)) {
            continue;
        }
        if keys.iter().any(|key| result.verify(key)) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};
    use crate::model::{Record, StateRec};
    use std::collections::HashMap;

    const PROJECT: &str = "01a12290-0000-7000-8000-0000000000b2";

    fn context(root: &std::path::Path) -> Ctx {
        Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.to_path_buf(),
            env: HashMap::new(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        }
    }

    fn project(ctx: &Ctx) -> (crate::repo::Repo<'_>, String, String) {
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        std::fs::create_dir_all(dir.join("states")).unwrap();
        let project = crate::store::Project {
            store: &ctx.store,
            name: "p".into(),
            dir: dir.clone(),
            id: PROJECT.into(),
            db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"id":PROJECT,"name":"p","config":{}});
        project.append(record, &ctx.actor()).unwrap();
        let mut states = Vec::new();
        for _ in 0..2 {
            let id = crate::ids::new_id();
            std::fs::create_dir(project.state_path(&id)).unwrap();
            let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
                "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
                "parents":[],"time":crate::ids::now_ms()}))
            .unwrap();
            let mut record = Record::new("state");
            record.states.push(state);
            project.append(record, &ctx.actor()).unwrap();
            states.push(id);
        }
        (crate::repo::Repo { p: project, ctx: None }, states[0].clone(), states[1].clone())
    }

    fn append(repo: &crate::repo::Repo, ctx: &Ctx, op: &str, data: serde_json::Value) {
        let mut record = Record::new(op);
        record.data = data;
        repo.p.append(record, &ctx.actor()).unwrap();
    }

    fn result(state: &str, exit: i64) -> ServerCheck {
        ServerCheck {
            state: state.into(),
            check: "test".into(),
            platform: "linux-aarch64".into(),
            exit,
            job: "i-0123456789abcdef0".into(),
            sig: String::new(),
        }
    }

    #[test]
    fn protection_takes_a_result_that_the_pinned_server_key_signed_for_exactly_its_state() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let (repo, state, other) = project(&ctx);
        let server = SigningKey::from_bytes(&[7; 32]);
        let command = "cargo test";
        append(&repo, &ctx, "config", serde_json::json!({"config":{"check.test":command}}));

        // A valid result before any key is pinned counts for nothing.
        let mut valid = result(&state, 0);
        valid.sign(&server).unwrap();
        append(&repo, &ctx, OP, serde_json::to_value(&valid).unwrap());
        assert!(!crate::cmd::attest::check_passed(&repo, &state, "test", command).unwrap(), "no pinned key");

        let pin = hex::encode(server.verifying_key().to_bytes());
        append(&repo, &ctx, "config", serde_json::json!({"config":{"cloud.remote.cloud.server-key":pin}}));
        assert!(crate::cmd::attest::check_passed(&repo, &state, "test", command).unwrap(), "a valid server result passes");
        // The platform of a check with platforms must meet the result's platform.
        assert!(passed(&repo, &state, "test", Some("linux")).unwrap());
        assert!(!passed(&repo, &state, "test", Some("macos")).unwrap());
        // Another check name does not pass.
        assert!(!crate::cmd::attest::check_passed(&repo, &state, "lint", command).unwrap());
        // A result for another state does not pass that state.
        assert!(!crate::cmd::attest::check_passed(&repo, &other, "test", command).unwrap(), "only the exact state");
        // The merge rule of a protected line (and so the queue) takes it for the new head only.
        let rule: crate::access::Rule = serde_json::from_value(serde_json::json!({"update":"writer","expect":false,
            "check":["test"],"direct":false,"rewrite":"admin","paths":{},"approvals":0,"approvers":[],"owners":[],
            "keep_approvals":[]}))
        .unwrap();
        assert!(crate::cmd::mergecmd::merge_met_rule(&repo, &rule, &other, &state, &state).unwrap());
        assert!(!crate::cmd::mergecmd::merge_met_rule(&repo, &rule, &state, &other, &other).unwrap());
        drop(repo);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn forged_unsigned_failed_and_changed_results_do_not_pass() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let (repo, state, other) = project(&ctx);
        let server = SigningKey::from_bytes(&[7; 32]);
        let forger = SigningKey::from_bytes(&[9; 32]);
        let command = "cargo test";
        let pin = hex::encode(server.verifying_key().to_bytes());
        append(&repo, &ctx, "config", serde_json::json!({"config":{"check.test":command,"cloud.remote.cloud.server-key":pin}}));

        let mut forged = result(&state, 0);
        forged.sign(&forger).unwrap();
        append(&repo, &ctx, OP, serde_json::to_value(&forged).unwrap());
        append(&repo, &ctx, OP, serde_json::to_value(result(&state, 0)).unwrap());
        let mut failed = result(&state, 1);
        failed.sign(&server).unwrap();
        append(&repo, &ctx, OP, serde_json::to_value(&failed).unwrap());
        // A signed result for another state, moved to this state.
        let mut moved = result(&other, 0);
        moved.sign(&server).unwrap();
        moved.state = state.clone();
        append(&repo, &ctx, OP, serde_json::to_value(&moved).unwrap());
        // A signed failure changed to exit 0.
        failed.exit = 0;
        append(&repo, &ctx, OP, serde_json::to_value(&failed).unwrap());
        assert!(!crate::cmd::attest::check_passed(&repo, &state, "test", command).unwrap());
        assert!(!forged.verify(&server.verifying_key()) && !result(&state, 0).verify(&server.verifying_key()));
        // A server key in the config is checked: a bad value is refused.
        assert!(parse_key("00").is_err() && parse_key(&"Z".repeat(64)).is_err());
        assert!(is_key_setting("cloud.remote.cloud.server-key") && !is_key_setting("cloud.remote..server-key"));
        drop(repo);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
