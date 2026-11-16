use crate::errors::{error, Kind as ErrorKind};
use crate::repo::Repo;
use anyhow::{bail, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Peer { host: String, project: String },
    Store,
    Registry,
}

pub fn kind(url: &str) -> Result<Kind> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| error(ErrorKind::Invalid, "use layr git remote for Git URLs"))?;
    if rest.is_empty() || url.chars().any(char::is_control) {
        bail!("invalid remote URL");
    }
    match scheme {
        "ssh" => {
            let (host, project) = rest.split_once('/').ok_or_else(|| error(ErrorKind::Invalid, "use ssh://<host>/<project>"))?;
            if host.is_empty()
                || host.starts_with('-')
                || host.chars().any(|c| !c.is_ascii_alphanumeric() && !".@_-:[]".contains(c))
            {
                bail!("invalid peer host");
            }
            crate::store::check_name("project", project)?;
            Ok(Kind::Peer { host: host.into(), project: project.into() })
        }
        "file" if rest.starts_with('/') => Ok(Kind::Store),
        "s3" if !rest.starts_with('/') && !rest.contains(['?', '#', '@', ':']) => Ok(Kind::Store),
        "layr+https" => {
            crate::store_remote::validate_gateway_url(url)?;
            Ok(Kind::Store)
        }
        "oci" if !rest.starts_with('/') && rest.contains('/') => Ok(Kind::Registry),
        _ => bail!("unsupported remote URL; use ssh://, file:///, s3://, layr+https:// or oci://"),
    }
}

pub fn remote_url(repo: &Repo, name: &str) -> Result<String> {
    if let Some(url) = repo.p.config_value(&format!("cloud.remote.{name}.url"))?.and_then(|v| v.as_str().map(str::to_owned)) {
        return Ok(url);
    }
    if repo.p.config_value(&format!("remote.{name}.url"))?.is_some() {
        return Err(error(ErrorKind::Invalid, format!("{name} is a Git remote; use layr git <fetch|pull|push> {name}")));
    }
    Err(error(ErrorKind::Invalid, format!("remote '{name}' is not configured; use layr remote add {name} <url>")))
}
