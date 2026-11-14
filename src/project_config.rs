//! Recorded project configuration updates, including hidden overlay routing.

use crate::ctx::Ctx;
use crate::errors::{error, Kind as ErrorKind};
use crate::model::Record;
use crate::repo::Repo;
use crate::store::Project;
use anyhow::Result;

pub fn set_config(ctx: &Ctx, repo: &Repo, key: &str, value: serde_json::Value) -> Result<()> {
    if key == Project::GC_EVICT_KEY && !value.is_null() && !matches!(value.as_str(), Some("on" | "off")) {
        return Err(error(ErrorKind::Failed, format!("{key} is on or off")));
    }
    if !repo.p.is_overlay() {
        if let Some(rest) = key.strip_prefix("overlay.") {
            let name = rest.split('.').next().unwrap_or("");
            if let Some(info) = repo.overlay_info(name)? {
                if rest == name || rest == format!("{name}.hidden") {
                    return Err(error(ErrorKind::Failed, "overlay visibility changes only through overlay show-to-project"));
                }
                if info.hidden {
                    let orepo = repo.overlay_repo(name)?;
                    orepo.require(ctx, "access")?;
                    let _lock = orepo.p.lock()?;
                    return set_config(ctx, &orepo, key, value);
                }
            }
        }
    }
    let mut rec = Record::new("config");
    let mut m = serde_json::Map::new();
    m.insert(key.to_string(), value);
    rec.data = serde_json::json!({"config": m});
    repo.commit_record(ctx, rec)?;
    Ok(())
}
