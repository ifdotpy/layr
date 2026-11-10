//! Store the decrypted store identity in the user's login Keychain.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

#[cfg(all(test, target_os = "macos"))]
fn test_state() -> &'static std::sync::RwLock<Option<security_framework::os::macos::keychain::SecKeychain>> {
    static STATE: std::sync::OnceLock<std::sync::RwLock<Option<security_framework::os::macos::keychain::SecKeychain>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| std::sync::RwLock::new(None))
}

#[cfg(all(test, target_os = "macos"))]
pub(super) struct TestKeychain {
    dir: std::path::PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, target_os = "macos"))]
impl Drop for TestKeychain {
    fn drop(&mut self) {
        *test_state().write().unwrap_or_else(|error| error.into_inner()) = None;
        // This private directory contains only the Keychain and securityd's sidecar files.
        let cleanup = || -> std::io::Result<()> {
            for entry in std::fs::read_dir(&self.dir)? {
                std::fs::remove_file(entry?.path())?;
            }
            std::fs::remove_dir(&self.dir)
        };
        if let Err(error) = cleanup() {
            eprintln!("cannot remove test Keychain {}: {error}", self.dir.display());
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn test_service() -> TestKeychain {
    use security_framework::os::macos::keychain::{CreateOptions, SecKeychain};
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::{Mutex, OnceLock};
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = SERIAL.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|error| error.into_inner());
    let dir = std::env::temp_dir().join(format!("layr-test-keychain-{}", crate::ids::new_id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).expect("create test Keychain directory");
    let fixture = TestKeychain { dir, _guard: guard };
    let keychain: SecKeychain =
        CreateOptions::new().password("layr-test-only").create(fixture.dir.join("test.keychain")).expect("create test Keychain");
    *test_state().write().unwrap_or_else(|error| error.into_inner()) = Some(keychain);
    fixture
}

#[cfg(all(test, target_os = "macos"))]
fn test_keychain() -> security_framework::os::macos::keychain::SecKeychain {
    test_state().read().unwrap_or_else(|error| error.into_inner()).clone().expect("Keychain tests must create a test fixture")
}

#[cfg(all(test, target_os = "macos"))]
#[allow(unsafe_code)] // Security.framework exports the query keys as static pointers.
fn test_options(account: &str, add: bool) -> security_framework::passwords::PasswordOptions {
    use core_foundation::{array::CFArray, base::TCFType, string::CFString};
    use security_framework::passwords::PasswordOptions;
    use security_framework_sys::item::{kSecMatchSearchList, kSecUseKeychain};
    let keychain = test_keychain();
    let mut options = PasswordOptions::new_generic_password(&service(), account);
    // Every test query names this Keychain. It cannot fall back to the user's search list.
    #[allow(deprecated)]
    unsafe {
        let key = if add { kSecUseKeychain } else { kSecMatchSearchList };
        let value = if add { keychain.into_CFType() } else { CFArray::from_CFTypes(&[keychain]).into_CFType() };
        options.query.push((CFString::wrap_under_get_rule(key), value));
    }
    options
}

#[cfg(all(test, target_os = "macos"))]
#[allow(unsafe_code)] // SecItemUpdate and kSecValueData are Security.framework symbols.
fn test_set(account: &str, identity: &[u8]) -> security_framework::base::Result<()> {
    use core_foundation::{base::TCFType, data::CFData, dictionary::CFDictionary, string::CFString};
    use security_framework_sys::{item::kSecValueData, keychain_item::SecItemUpdate};
    match security_framework::passwords::generic_password(test_options(account, false)) {
        Err(error) if error.code() == -25300 => {
            return security_framework::passwords::set_generic_password_options(identity, test_options(account, true));
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let search = test_options(account, false);
    #[allow(deprecated)]
    let query = CFDictionary::from_CFType_pairs(&search.query);
    let value = unsafe { CFString::wrap_under_get_rule(kSecValueData) };
    let update = CFDictionary::from_CFType_pairs(&[(value, CFData::from_buffer(identity).into_CFType())]);
    let status = unsafe { SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef()) };
    if status == 0 {
        Ok(())
    } else {
        Err(security_framework::base::Error::from_code(status))
    }
}

pub fn account(url: &str) -> String {
    use layr_hashers::{ContentAddress, Digest};
    hex::encode(ContentAddress::digest(url.as_bytes()))
}

#[cfg(target_os = "macos")]
fn service() -> String {
    "ws.layr.mount".into()
}

pub fn get(account: &str) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords::generic_password;
        #[cfg(test)]
        let options = test_options(account, false);
        #[cfg(not(test))]
        let options = security_framework::passwords::PasswordOptions::new_generic_password(&service(), account);
        return match generic_password(options) {
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
        #[cfg(test)]
        let result = test_set(account, identity.as_bytes());
        #[cfg(not(test))]
        let result = security_framework::passwords::set_generic_password(&service(), account, identity.as_bytes());
        result.map_err(|_| anyhow::anyhow!("cannot save the disk key in the login Keychain"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (account, identity);
    }
    Ok(())
}

pub fn remove(account: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    match {
        #[cfg(test)]
        {
            security_framework::passwords::delete_generic_password_options(test_options(account, false))
        }
        #[cfg(not(test))]
        {
            security_framework::passwords::delete_generic_password(&service(), account)
        }
    } {
        Ok(()) => {}
        Err(e) if e.code() == -25300 => {}
        Err(_) => bail!("cannot remove the disk key from the login Keychain"),
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = account;
        bail!("the Keychain is macOS only");
    }
    #[cfg(target_os = "macos")]
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
    fn test_keychain_is_removed_after_a_panic() {
        let mut path = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let fixture = test_service();
            path = Some(fixture.dir.clone());
            panic!("test fixture cleanup");
        }));
        assert!(result.is_err());
        assert!(!path.unwrap().exists());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn login_keychain_roundtrip() {
        let _keychain = test_service();
        let account = format!("layr-mount-test-{}", crate::ids::new_id());
        let value = format!("test-marker-{}", crate::ids::new_id());
        set(&account, &value).unwrap();
        assert!(test_keychain().find_generic_password(&service(), &account).is_ok());
        let updated = format!("test-marker-{}", crate::ids::new_id());
        set(&account, &updated).unwrap();
        let found = get(&account).unwrap();
        let removed = remove(&account);
        assert!(removed.is_ok(), "cannot remove the test Keychain item");
        assert!(get(&account).unwrap().is_none(), "the key remains after lock");
        remove(&account).unwrap();
        assert!(found.as_deref() == Some(updated.as_str()), "Keychain roundtrip differs");
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
