//! Named pointers without working folders.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, RefChange, RefState};
use anyhow::{bail, Result};

pub fn refs(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").value("--expect").value("--ttl"), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let usage = "usage: layr ref (set <name> <rev>|list [<prefix>]|log <name>|delete <name>)";
    let command = a.pos.first().map(String::as_str).ok_or_else(|| exit(128, usage))?;
    match command {
        "list" if a.pos.len() <= 2 => {
            let refs = repo.p.db.refs(a.pos.get(1).map(String::as_str).unwrap_or(""), ids::now_ms())?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&refs)?);
            } else {
                for r in refs {
                    outln!(ctx, "{}\t{}{}", r.name, r.target, r.expires_at.map(|t| format!("\texpires {t}")).unwrap_or_default());
                }
            }
        }
        "log" if a.pos.len() == 2 => {
            let entries = repo.p.db.ref_log(&a.pos[1])?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&entries)?);
            } else {
                let mut n = 0;
                for e in entries {
                    if let Some(value) = e.after {
                        outln!(ctx, "{}@{{{n}}}\t{}", e.name, value.target);
                        n += 1;
                    } else {
                        outln!(ctx, "{}\tdeleted", e.name);
                    }
                }
            }
        }
        "set" | "delete" => {
            let set = command == "set";
            if a.pos.len() != if set { 3 } else { 2 } {
                bail!("{usage}");
            }
            if !set && a.has("--ttl") {
                bail!("--ttl is only valid for ref set");
            }
            repo.require(ctx, "line.manage")?;
            let name = &a.pos[1];
            const PREFIXES: &[&str] = &["env/", "release/", "pins/", "holds/", "leases/", "caches/", "derived/"];
            if !PREFIXES.iter().any(|p| name.starts_with(p)) {
                bail!("mutable ref names must start with one of: {}", PREFIXES.join(", "));
            }
            if !crate::model::is_ref(name) {
                bail!("invalid ref name '{name}'");
            }
            let _lock = repo.p.lock()?;
            if repo.p.db.line_by_name(name)?.is_some() || repo.p.db.line_by_id(name)?.is_some() {
                bail!("'{name}' is a line; use commit, merge or reset");
            }
            let before = repo.p.db.ref_value(name, ids::now_ms())?;
            if let Some(expect) = a.get("--expect") {
                let expected = crate::revs::resolve(repo, h.line.as_ref(), expect)?;
                if before.as_ref().map(|r| &r.target) != Some(&expected) {
                    bail!("ref '{name}' changed: --expect does not match its current position");
                }
            }
            if !set && before.is_none() {
                bail!("ref '{name}' does not exist");
            }
            let after = if set {
                let target = crate::revs::resolve(repo, h.line.as_ref(), &a.pos[2])?;
                repo.state_root(&target)?;
                let expires_at = a
                    .get("--ttl")
                    .map(ttl_ms)
                    .transpose()?
                    .map(|d| ids::now_ms().checked_add(d).ok_or_else(|| anyhow::anyhow!("TTL exceeds the supported time range")))
                    .transpose()?;
                Some(RefState { name: name.clone(), target, expires_at })
            } else {
                None
            };
            let change = RefChange { name: name.clone(), before, after };
            let mut rec = Record::new(if set { "ref.set" } else { "ref.delete" });
            rec.data = serde_json::json!({"ref_change": change});
            repo.commit_record(ctx, rec)?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&change)?);
            } else if let Some(r) = change.after {
                outln!(ctx, "{}\t{}", r.name, r.target);
            } else {
                outln!(ctx, "deleted {name}");
            }
        }
        _ => bail!("{usage}"),
    }
    Ok(0)
}

fn ttl_ms(value: &str) -> Result<i64> {
    let split = value.char_indices().find(|(_, c)| !c.is_ascii_digit()).map(|(i, _)| i).unwrap_or(value.len());
    let n: i64 = value[..split].parse().map_err(|_| anyhow::anyhow!("bad TTL '{value}': use 30s, 5m, 2h or 1d"))?;
    let unit = match &value[split..] {
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("bad TTL '{value}': use 30s, 5m, 2h or 1d"),
    };
    if n <= 0 {
        bail!("TTL must be positive");
    }
    n.checked_mul(unit).ok_or_else(|| anyhow::anyhow!("TTL exceeds the supported time range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ttl_is_positive_and_checked() {
        assert_eq!(ttl_ms("2h").unwrap(), 7_200_000);
        for value in ["", "0s", "-1s", "1", "5ms", "9223372036854775807d", "☃"] {
            assert!(ttl_ms(value).is_err(), "{value}");
        }
    }
}
