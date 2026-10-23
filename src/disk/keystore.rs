//! Store the decrypted store identity in the user's login Keychain.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

pub fn account(url: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(url.as_bytes()))
}

#[cfg(target_os = "macos")]
const SERVICE: &str = "ws.layr.mount";

pub fn get(account: &str) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords::{generic_password, PasswordOptions};
        return match generic_password(PasswordOptions::new_generic_password(SERVICE, account)) {
            Ok(bytes) => Ok(Some(String::from_utf8(bytes).context("invalid disk key in Keychain")?)),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => bail!("cannot read the disk key from the login Keychain"),
        };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = account;
        Ok(None)
    }
}

pub fn set(account: &str, identity: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        security_framework::passwords::set_generic_password(SERVICE, account, identity.as_bytes())
            .map_err(|_| anyhow::anyhow!("cannot save the disk key in the login Keychain"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (account, identity);
    }
    Ok(())
}

pub fn paper_file(path: &Path) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("cannot open the paper key file")?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.mode() & 0o777 != 0o600 || meta.uid() != rustix::process::getuid().as_raw() {
        bail!("the paper key file must belong to this user and have mode 0600");
    }
    bounded(file)
}

pub fn bounded(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 || bytes.is_empty() {
        bail!("invalid paper key input");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid paper key input"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "uses the macOS login Keychain"]
    fn login_keychain_roundtrip() {
        let account = format!("layr-mount-test-{}", crate::ids::new_id());
        let value = format!("test-marker-{}", crate::ids::new_id());
        set(&account, &value).unwrap();
        let found = get(&account).unwrap();
        let removed = security_framework::passwords::delete_generic_password(SERVICE, &account);
        assert!(removed.is_ok(), "cannot remove the test Keychain item");
        assert!(found.as_deref() == Some(value.as_str()), "Keychain roundtrip differs");
    }
    #[test]
    fn private_key_input_refuses_links_and_public_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("layr-disk-key-{}", crate::ids::new_id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, "test words").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(paper_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(paper_file(&path).is_ok());
        std::os::unix::fs::symlink(&path, dir.join("link")).unwrap();
        assert!(paper_file(&dir.join("link")).is_err());
        assert!(bounded(&vec![b'a'; 4097][..]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
