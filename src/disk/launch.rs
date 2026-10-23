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
    let home = super::webdav::xml(&home.display().to_string());
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array>{args}</array><key>EnvironmentVariables</key><dict><key>HOME</key><string>{home}</string></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>\n")
}

pub(super) fn install(url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("mount --install requires macOS");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let agent = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    std::fs::create_dir_all(agent.parent().unwrap())?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    crate::fsutil::atomic_write(&agent, plist(&exe, url, listen, cache, limit, home).as_bytes(), 0o600)?;
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    let output = Command::new("/bin/launchctl").args(["bootstrap", &domain]).arg(&agent).output()?;
    if !output.status.success() {
        bail!("launchctl could not start the disk agent");
    }
    println!("The disk starts at login and mounts at {}", home.join("Layr").display());
    Ok(())
}

pub(super) fn remove(home: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("mount --remove requires macOS");
    }
    let agent = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    if !agent.exists() {
        return Ok(());
    }
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    let _ = Command::new("/sbin/umount").arg(home.join("Layr")).output();
    std::fs::remove_file(agent)?;
    println!("The disk no longer starts at login");
    Ok(())
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
}
