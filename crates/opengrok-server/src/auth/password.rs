//! Password hashing for credential accounts — argon2id, PHC string format.
//!
//! The stored value is a full PHC string (`$argon2id$v=19$...`), which carries its own salt and
//! parameters, so verification needs nothing but the string and the candidate password. The
//! plaintext password never leaves this module and is never stored.

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

/// Hash a password to a PHC string. Fails only if the RNG or the hasher does, which is a real
/// server fault, not a user error.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| error.to_string())
}

/// Verify a candidate password against a stored PHC hash. `false` for both "wrong password" and a
/// malformed stored hash — a login never leaks which.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let hash = hash_password("correct horse battery staple").expect("hash");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong", &hash));
        // A malformed stored hash verifies nothing rather than panicking.
        assert!(!verify_password("anything", "not-a-phc-string"));
    }
}
