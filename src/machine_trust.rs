//! Project trust records for machine identities and limits.

use crate::ctx::Ctx;
use crate::ids;
use crate::store::Project;
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Record that this project trusts a machine's records (its id and key), with `limits` (`expires_at`, `role`,
/// `overlays`, docs/ci-trust.md) when given.
pub fn trust(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    p: &Project,
    machine: &str,
    key: &str,
    limits: Option<&Value>,
) -> Result<bool> {
    locked.check(&p)?;
    if machine == p.store.machine.id {
        return Ok(false);
    }
    if let Some(k) = p.db.trusted(machine)? {
        if k != key {
            bail!("machine {} is trusted with another key", &ids::display(machine)[..8]);
        }
        // A trust with limits narrows a trusted machine (the narrowest limits win, docs/ci-trust.md), for example
        // a full merge machine that becomes a merge machine. A second trust without limits changes nothing.
        if limits.is_none() {
            return Ok(false);
        }
    }
    let mut rec = crate::model::Record::new("machine.trust");
    rec.data = json!({"machine": machine, "public_key": key});
    if let Some(Value::Object(limits)) = limits {
        for (k, v) in limits {
            rec.data[k] = v.clone();
        }
    }
    locked.append(rec, &ctx.actor())?;
    Ok(true)
}
