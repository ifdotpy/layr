//! Start the disk at login and mount it with the macOS WebDAV client.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const LABEL: &str = "ws.layr.mount";

pub(super) fn plist(exe: &Path, url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> String {
    let args = [
        exe.display().to_string(),
        "mount".into(),
        url.into(),
        "--keychain-only".into(),
        "--listen".into(),
        listen.into(),
        "--cache".into(),
        cache.display().to_string(),
        "--cache-size".into(),
        limit.to_string(),
        "--at".into(),
        home.join("Layr").display().to_string(),
    ];
    let args = args.iter().map(|arg| format!("<string>{}</string>", super::webdav::xml(arg))).collect::<String>();
    let log = super::webdav::xml(&home.join("Library/Logs/layr-mount.log").display().to_string());
    let mut environment = format!("<key>HOME</key><string>{}</string>", super::webdav::xml(&home.display().to_string()));
    for name in ["LAYR_ROOT", "XDG_CONFIG_HOME", "LAYR_PERSON_DIR", "LAYR_PERSON_KEY_FILE", "LAYR_PERSON_LOGIN"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push_str(&format!("<key>{name}</key><string>{}</string>", super::webdav::xml(&value.to_string_lossy())));
        }
    }
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array>{args}</array><key>EnvironmentVariables</key><dict>{environment}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>\n")
}

pub(super) fn install(url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    install_service(url, listen, cache, limit, home, false)
}
pub(super) fn install_all(listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    install_service("--all", listen, cache, limit, home, true)
}
fn install_service(url: &str, listen: &str, cache: &Path, limit: u64, home: &Path, all: bool) -> Result<()> {
    let label = if all { "ws.layr.mount.all" } else { LABEL };
    if !cfg!(target_os = "macos") {
        bail!("mount --install requires macOS");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let agent = home.join(format!("Library/LaunchAgents/{label}.plist"));
    std::fs::create_dir_all(agent.parent().unwrap())?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    let text = plist(&exe, url, listen, cache, limit, home)
        .replace(&format!("<string>{LABEL}</string>"), &format!("<string>{label}</string>"));
    crate::fsutil::atomic_write(&agent, text.as_bytes(), 0o600)?;
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
    let output = Command::new("/bin/launchctl").args(["bootstrap", &domain]).arg(&agent).output()?;
    if !output.status.success() {
        bail!("launchctl could not start the disk agent");
    }
    println!("The disk starts at login and mounts at {}", home.join("Layr").display());
    Ok(())
}

pub(super) fn remove(home: &Path) -> Result<()> {
    remove_service(home, false)
}
pub(super) fn remove_all(home: &Path) -> Result<()> {
    remove_service(home, true)
}
fn remove_service(home: &Path, all: bool) -> Result<()> {
    let label = if all { "ws.layr.mount.all" } else { LABEL };
    if !cfg!(target_os = "macos") {
        bail!("mount --remove requires macOS");
    }
    let agent = home.join(format!("Library/LaunchAgents/{label}.plist"));
    if !agent.exists() {
        return Ok(());
    }
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
    let _ = Command::new("/sbin/umount").arg(home.join("Layr")).output();
    std::fs::remove_file(agent)?;
    println!("The disk no longer starts at login");
    Ok(())
}

// Read the service arguments that install wrote, as Desk did.
pub(super) fn installed(home: &Path) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for label in [LABEL, "ws.layr.mount.all"] {
        let text = match std::fs::read_to_string(home.join(format!("Library/LaunchAgents/{label}.plist"))) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let Some((_, args)) = text.split_once("<key>ProgramArguments</key>") else { continue };
        let args: Vec<_> = args
            .split("</array>")
            .next()
            .unwrap_or("")
            .split("<string>")
            .skip(1)
            .filter_map(|s| {
                s.split_once("</string>").map(|(s, _)| {
                    s.replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&quot;", "\"")
                        .replace("&apos;", "'")
                        .replace("&amp;", "&")
                })
            })
            .collect();
        let url = args.get(2).context("invalid mount agent arguments")?;
        let listen = args.windows(2).find(|pair| pair[0] == "--listen").map(|pair| pair[1].clone()).unwrap_or_default();
        result.push((url.clone(), listen));
    }
    Ok(result)
}

pub(super) fn mount(url: &str, at: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("--at requires the macOS WebDAV client");
    }
    let mounts = Command::new("/sbin/mount").output()?;
    if String::from_utf8_lossy(&mounts.stdout).contains(&format!("{url} on {} (", at.display())) {
        return Ok(());
    }
    std::fs::create_dir_all(at)?;
    let output = Command::new("/sbin/mount_webdav")
        .args(["-S", "-v", "Layr", url])
        .arg(at)
        .output()
        .context("cannot start the macOS WebDAV client")?;
    if !output.status.success() {
        bail!("the macOS WebDAV client could not mount the disk");
    }
    eprintln!("layr mount: mounted at {}", at.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_arguments_are_xml_and_contain_no_paper_key_input() {
        let text =
            plist(Path::new("/A & B/layr"), "file:///a<b", "127.0.0.1:8095", Path::new("/cache"), 17, Path::new("/home/u"));
        assert!(text.contains("/A &amp; B/layr"));
        assert!(text.contains("file:///a&lt;b"));
        assert!(text.contains("/home/u/Layr"));
        assert!(!text.contains("paper-key"));
    }
    #[test]
    fn all_store_arguments_round_trip_without_a_secret() {
        let home = std::env::temp_dir().join(crate::ids::new_id());
        let agent = home.join("Library/LaunchAgents/ws.layr.mount.all.plist");
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        std::fs::write(&agent, plist(Path::new("/bin/layr"), "--all", "127.0.0.1:8095", Path::new("/cache"), 17, &home)).unwrap();
        assert_eq!(installed(&home).unwrap(), vec![("--all".into(), "127.0.0.1:8095".into())]);
        std::fs::remove_dir_all(home).unwrap();
    }
}
