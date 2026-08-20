use std::hash::{Hash, Hasher};

use crate::framework::parse::grammar::TerminalId;

pub type TokenFingerprint = u64;

pub fn token_fingerprint<T: Hash>(
    terminal: Option<TerminalId>,
    value: &T,
    length: usize,
) -> TokenFingerprint {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    terminal.hash(&mut hasher);
    value.hash(&mut hasher);
    length.hash(&mut hasher);
    hasher.finish()
}

pub fn eof_fingerprint() -> TokenFingerprint {
    0
}

pub fn error_fingerprint<T: Hash>(error: &T, length: usize) -> TokenFingerprint {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    1u8.hash(&mut hasher);
    error.hash(&mut hasher);
    length.hash(&mut hasher);
    hasher.finish()
}
