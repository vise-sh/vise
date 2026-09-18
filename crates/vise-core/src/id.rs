//! TypeID-style identifiers: `{prefix}_{uuidv7 as lowercase crockford base32}`.
//!
//! UUIDv7 payloads are time-ordered, so IDs sort by creation time and insert
//! at the tail of btree indexes. See https://github.com/jetify-com/typeid

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", encode(uuid::Uuid::now_v7()))
}

/// Random (v4) payload: secrets need entropy, not sortability — a v7 token
/// would leak its mint time and lose ~48 bits of randomness to the timestamp.
pub fn new_token(prefix: &str) -> String {
    format!("{prefix}_{}", encode(uuid::Uuid::new_v4()))
}

/// A short random suffix for generated names (ephemeral host names): the
/// last 6 characters of a random (v4) UUID's encoding, ~30 bits of entropy.
/// Collisions are handled by the caller retrying, not by more bits.
pub fn short_suffix() -> String {
    let encoded = encode(uuid::Uuid::new_v4());
    encoded[encoded.len() - 6..].to_string()
}

fn encode(uuid: uuid::Uuid) -> String {
    let n = u128::from_be_bytes(*uuid.as_bytes());

    // 26 chars * 5 bits = 130 bits; the top 2 bits are always zero
    let mut out = [0u8; 26];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (25 - i);
        *slot = ALPHABET[((n >> shift) & 0x1f) as usize];
    }

    String::from_utf8(out.to_vec()).expect("alphabet is ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_typeid_spec_vector() {
        // https://github.com/jetify-com/typeid/blob/main/spec/README.md
        let uuid = uuid::Uuid::parse_str("01890a5d-ac96-774b-bcce-b302099a8057").unwrap();
        assert_eq!(encode(uuid), "01h455vb4pex5vsknk084sn02q");
    }

    #[test]
    fn short_suffixes_stay_in_the_alphabet() {
        let suffix = short_suffix();
        assert_eq!(suffix.len(), 6);
        assert!(suffix.bytes().all(|byte| ALPHABET.contains(&byte)));
        assert_ne!(short_suffix(), short_suffix(), "suffixes must be random");
    }

    #[test]
    fn new_ids_are_prefixed_and_sortable() {
        let a = new_id("ses");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id("ses");

        assert!(a.starts_with("ses_"));
        assert_eq!(a.len(), "ses_".len() + 26);
        assert!(a < b, "later IDs must sort after earlier ones: {a} vs {b}");
    }
}
