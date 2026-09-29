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
