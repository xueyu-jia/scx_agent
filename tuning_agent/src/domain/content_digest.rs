use serde::Serialize;
use sha2::{Digest as ShaDigest, Sha256};

use crate::domain::Digest;

pub fn content_digest(value: &impl Serialize) -> Result<Digest, String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| format!("failed to serialize digest input: {error}"))?;
    let hash = Sha256::digest(encoded);
    Digest::new(format!("sha256:{hash:x}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn content_digest_is_stable_and_sensitive_to_input() {
        let first = content_digest(&json!({"a": 1, "b": 2})).unwrap();
        let same = content_digest(&json!({"b": 2, "a": 1})).unwrap();
        let different = content_digest(&json!({"a": 1, "b": 3})).unwrap();

        assert_eq!(first, same);
        assert_ne!(first, different);
    }
}
