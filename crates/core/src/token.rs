//! Opaque secrets for links and sessions: the client holds the token, the database its hash.
use sha2::{Digest, Sha256};

/// 32 random bytes as 64 hex characters.
pub fn new() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    hex::encode(bytes)
}

/// SHA-256 of `token`, hex encoded. The only form a token is stored in.
pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_are_random_and_hashes_stable() {
        let (a, b) = (super::new(), super::new());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(super::hash(&a), super::hash(&a));
        assert_ne!(super::hash(&a), a);
    }
}
