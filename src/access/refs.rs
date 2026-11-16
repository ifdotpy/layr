//! Authorization and signed evidence required to update protected refs.

use crate::ctx::Ctx;
use crate::ids;
use crate::model::RefState;
use anyhow::{bail, Result};

/// Ref checks consume signed evidence. They never execute a configured command.
pub fn authorize_update(
    ctx: &Ctx,
    repo: &crate::repo::Repo,
    name: &str,
    before: Option<&RefState>,
    after: Option<&RefState>,
    expect: bool,
) -> Result<()> {
    let policy = repo.policy();
    let rule = match policy.rule_for(name) {
        Some(rule) => rule,
        None => return repo.require(ctx, "line.manage"),
    };
    let who = crate::repo::Repo::who(ctx);
    if let Some(principal) = policy.denied_by(&who, "line.manage") {
        bail!("permission denied: a deny rule for {} takes ref updates away", crate::access::show_principal(&principal));
    }
    let role = policy.role(&who);
    if role < rule.update {
        bail!("ref '{name}' needs the update role {}; you are {}", rule.update.name(), role.name());
    }
    if rule.expect && !expect {
        bail!("ref '{name}' is protected: add --expect <state>");
    }
    authorize_evidence(repo, name, before, after, &policy, &rule)
}

pub fn authorize_person_update(
    repo: &crate::repo::Repo,
    login: &str,
    name: &str,
    before: Option<&RefState>,
    after: Option<&RefState>,
) -> Result<()> {
    let policy = repo.policy();
    let Some(rule) = policy.rule_for(name) else {
        return policy.check_person(login, "line.manage").map_err(anyhow::Error::msg);
    };
    if policy.person_denied(login, "line.manage") {
        bail!("a deny rule takes ref updates away");
    }
    if policy.person_role(login) < rule.update {
        bail!("ref update needs the role {}", rule.update.name());
    }
    authorize_evidence(repo, name, before, after, &policy, &rule)
}

fn authorize_evidence(
    repo: &crate::repo::Repo,
    name: &str,
    before: Option<&RefState>,
    after: Option<&RefState>,
    policy: &crate::access::Policy,
    rule: &crate::access::Rule,
) -> Result<()> {
    let target = after.or(before).ok_or_else(|| anyhow::anyhow!("ref '{name}' has no target"))?;
    if rule.approvals > 0 {
        let count = crate::access::approval_count(repo, &policy, &rule, before.map(|r| r.target.as_str()), &target.target)?;
        if count < (rule.approvals as usize) {
            bail!(
                "ref '{name}' needs {} approval(s) of exactly state {} from allowed users other than its authors; it has {count}",
                rule.approvals,
                ids::short(&target.target)
            );
        }
    }
    for check in &rule.check {
        let command =
            repo.p.config_value(&format!("check.{check}"))?.and_then(|v| v.as_str().map(String::from)).ok_or_else(|| {
                anyhow::anyhow!("check '{check}' of ref '{name}' is not defined (layr config check.{check} '<command>')")
            })?;
        if !crate::attestation::check_passed(repo, &target.target, &command)? {
            bail!(
                "ref '{name}' needs a passing check '{check}' for exactly state {} and command '{command}'; run 'layr try {} --check {check}' first",
                ids::short(&target.target),
                ids::short(&target.target)
            );
        }
    }
    Ok(())
}
