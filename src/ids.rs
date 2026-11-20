//! Ids. Records and states use UUIDv7. A state is shown as 32 hex digits that start with the
//! random part of the UUID, so short prefixes are unique like git hashes.

use uuid::Uuid;

pub fn new_uuid() -> Uuid {
    Uuid::now_v7()
}

/// A random u64 straight from the OS. `rand::random` draws from the thread RNG, and after the
/// service forks a request process the child still serves the parent's buffered block before
/// it reseeds (rand 0.8, `ReseedingRng`): two requests got the same "random" try folder
/// `.try-3a75b87d22a1b9df` on 2026-11-16, and the second `layr try` failed. Names of temporary
/// folders, files and sockets take this instead.
pub fn random_u64() -> u64 {
    use rand::RngCore;
    rand::rngs::OsRng.next_u64()
}

pub fn new_id() -> String {
    new_uuid().hyphenated().to_string()
}

/// Display form: bytes 10..16, 6..10, 0..6 of the UUID in hex. A bijection with the UUID.
pub fn display(id: &str) -> String {
    match Uuid::parse_str(id) {
        Ok(u) => {
            let b = u.as_bytes();
            let mut v = Vec::with_capacity(16);
            v.extend_from_slice(&b[10..16]);
            v.extend_from_slice(&b[6..10]);
            v.extend_from_slice(&b[0..6]);
            hex::encode(v)
        }
        Err(_) => id.to_string(),
    }
}

pub fn short(id: &str) -> String {
    match layr_types::StateId::parse(id) {
        Ok(id) => id.short().to_string(),
        Err(_) => id.chars().take(7).collect(),
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_handles_invalid_text_without_panicking() {
        assert_eq!(super::short(""), "");
        assert_eq!(super::short("éééééééé"), "ééééééé");
        assert_eq!(super::short("00112233-4455-6677-8899-aabbccddeeff"), "aabbccd");
    }
}
