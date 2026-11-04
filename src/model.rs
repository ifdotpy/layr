//! Shared record types and validation, plus the native build platform.
pub use layr_model::operations::{Transfer, VersionOperation};
pub use layr_model::types::*;
pub use layr_model::validation::{is_cache_name, is_ref, is_uuid, validate};

/// The platform of this build of layr, `<os>-<arch>` as Rust names them (for example
/// `linux-x86_64`, `linux-aarch64`, `macos-aarch64`). A cache snapshot records the platform it was
/// saved on (`StateRec::cache_platform`); build output in it, such as a Rust `target` folder, is
/// made for that platform.
pub fn cache_platform() -> String {
    // Test only (tests/t138-cache-platform-lines.sh): `LAYR_TEST_PLATFORM` acts as another platform.
    if let Ok(p) = std::env::var("LAYR_TEST_PLATFORM") {
        return p;
    }
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
