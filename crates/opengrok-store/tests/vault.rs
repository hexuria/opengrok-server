//! The vault on its own: sealing, opening, and which key a blob was sealed under.
//!
//! No Postgres. Kept out of `src/` because only the public API is exercised, and the crate's
//! size ceiling counts inline tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_store::{Sealed, StoreError, Vault, VaultCheck};

const KEK: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
const OTHER_KEK: &str = "ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=";
const THIRD_KEK: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWY=";

fn vault() -> Vault {
    Vault::from_base64_key(KEK).unwrap()
}

fn other() -> Vault {
    Vault::from_base64_key(OTHER_KEK).unwrap()
}

/// A blob as a row written before key ids were recorded holds it.
fn legacy(mut sealed: Sealed) -> Sealed {
    sealed.key_id = None;
    sealed
}

#[test]
fn a_sealed_credential_opens_again() {
    let sealed = vault().seal("conn_1", "ghp_secret_token").unwrap();
    assert_eq!(vault().open("conn_1", &sealed).unwrap(), "ghp_secret_token");
}

/// The point of the whole file: the stored bytes must not contain the token.
#[test]
fn the_stored_bytes_do_not_contain_the_token() {
    let secret = "ghp_averydistinctivesecret";
    let sealed = vault().seal("conn_1", secret).unwrap();
    let as_text = String::from_utf8_lossy(&sealed.ciphertext);
    assert!(!as_text.contains(secret), "the ciphertext leaked the token");
    assert!(
        !as_text.contains("ghp_"),
        "even the prefix must not survive"
    );
}

/// Nonce reuse is what breaks this cipher, so two seals of the same value must differ.
#[test]
fn sealing_the_same_value_twice_produces_different_bytes() {
    let first = vault().seal("conn_1", "same").unwrap();
    let second = vault().seal("conn_1", "same").unwrap();
    assert_ne!(first.nonce, second.nonce, "a nonce must never repeat");
    assert_ne!(first.ciphertext, second.ciphertext);
    assert_eq!(vault().open("conn_1", &first).unwrap(), "same");
    assert_eq!(vault().open("conn_1", &second).unwrap(), "same");
}

/// THE ROW BINDING. A blob moved to another row must stop opening — otherwise swapping two
/// rows swaps two people's credentials and the database looks untouched.
#[test]
fn a_credential_moved_to_another_row_will_not_open() {
    let sealed = vault().seal("conn_mine", "mine").unwrap();
    assert!(vault().open("conn_yours", &sealed).is_err());
    assert!(vault().open("conn_yours", &legacy(sealed)).is_err());
}

/// AEAD, not encryption: a tampered ciphertext fails rather than opening to something else.
#[test]
fn a_tampered_ciphertext_is_refused_not_decrypted() {
    let mut sealed = vault().seal("conn_1", "mine").unwrap();
    if let Some(byte) = sealed.ciphertext.first_mut() {
        *byte ^= 0xFF;
    }
    assert!(vault().open("conn_1", &sealed).is_err());
    assert!(vault().open("conn_1", &legacy(sealed)).is_err());
}

#[test]
fn another_key_cannot_open_it() {
    let sealed = vault().seal("conn_1", "mine").unwrap();
    assert!(other().open("conn_1", &sealed).is_err());
    assert!(other().open("conn_1", &legacy(sealed)).is_err());
}

/// Failures that are told apart only by DECRYPTING must read identically — that is how a
/// decryption oracle starts. Under one named key a moved row and a tampered blob are the same
/// answer; for a blob with no key id, a wrong key joins them. What may differ is a key id this
/// server does not hold, because that is read from the row, not learned by trying.
#[test]
fn every_failure_reads_identically() {
    let sealed = vault().seal("conn_1", "mine").unwrap();
    let mut tampered = sealed.clone();
    if let Some(byte) = tampered.ciphertext.last_mut() {
        *byte ^= 0x01;
    }

    let wrong_row = vault().open("conn_2", &sealed).unwrap_err().to_string();
    let altered = vault().open("conn_1", &tampered).unwrap_err().to_string();
    assert_eq!(wrong_row, altered);

    let legacy_wrong_row = vault()
        .open("conn_2", &legacy(sealed.clone()))
        .unwrap_err()
        .to_string();
    let legacy_wrong_key = other()
        .open("conn_1", &legacy(sealed))
        .unwrap_err()
        .to_string();
    let legacy_altered = vault()
        .open("conn_1", &legacy(tampered))
        .unwrap_err()
        .to_string();
    assert_eq!(legacy_wrong_row, legacy_wrong_key);
    assert_eq!(legacy_wrong_row, legacy_altered);
}

/// The two setup mistakes need different fixes, so they are named differently.
#[test]
fn a_bad_key_says_which_mistake_was_made() {
    let not_base64 = Vault::from_base64_key("not base64!!")
        .unwrap_err()
        .to_string();
    assert!(not_base64.contains("not valid base64"), "{not_base64}");
    assert!(not_base64.contains("openssl rand"), "and how to fix it");
    assert!(not_base64.contains("OG_CREDENTIAL_KEK"), "{not_base64}");

    let too_short = Vault::from_base64_key("c2hvcnQ=").unwrap_err().to_string();
    assert!(too_short.contains("32 bytes"), "{too_short}");
}

/// A broken retired key is a broken boot, and the message names the variable to fix — the
/// operator is looking at two keys and must not have to guess which one is wrong.
#[test]
fn a_bad_retired_key_names_its_variable() {
    let error = Vault::from_base64_keys(KEK, &[OTHER_KEK, "not base64!!"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("OG_CREDENTIAL_KEK_OLD"), "{error}");
    assert!(error.contains("not valid base64"), "{error}");

    let short = Vault::from_base64_keys(KEK, &["c2hvcnQ="])
        .unwrap_err()
        .to_string();
    assert!(short.contains("OG_CREDENTIAL_KEK_OLD"), "{short}");
    assert!(short.contains("32 bytes"), "{short}");
}

/// However it is logged, the key must not be printable.
#[test]
fn the_vault_does_not_print_its_key() {
    assert_eq!(format!("{:?}", vault()), "Vault(<redacted>)");
    let ring = Vault::from_base64_keys(KEK, &[OTHER_KEK]).unwrap();
    assert_eq!(format!("{ring:?}"), "Vault(<redacted>)");
}

/// A truncated nonce must be refused rather than panicking on a slice.
#[test]
fn a_malformed_row_is_refused_rather_than_crashing() {
    let sealed = Sealed {
        nonce: vec![0; 3],
        ciphertext: vec![1, 2, 3],
        key_id: Some(vault().key_id().to_string()),
    };
    assert!(vault().open("conn_1", &sealed).is_err());
    assert!(vault().open("conn_1", &legacy(sealed)).is_err());
}

// ---- key ids and retired keys -------------------------------------------------------------

/// Every seal records which key made it, derived from the key so a typo cannot mislabel one —
/// and never the key itself.
#[test]
fn a_sealed_blob_names_the_key_that_sealed_it() {
    let sealed = vault().seal("conn_1", "x").unwrap();
    assert_eq!(sealed.key_id.as_deref(), Some(vault().key_id()));
    assert_eq!(vault().key_id(), vault().key_id(), "stable across boots");
    assert_ne!(vault().key_id(), other().key_id());
    let id = vault().key_id().to_string();
    assert!(!id.is_empty());
    assert!(!KEK.contains(&id) && !id.contains(KEK.trim_end_matches('=')));
}

/// The rotation: the new key seals, the old one still opens what it sealed, and a reseal can
/// tell the two apart without decrypting anything.
#[test]
fn a_retired_key_still_opens_what_it_sealed() {
    let sealed_by_old = other().seal("conn_1", "mine").unwrap();
    let ring = Vault::from_base64_keys(KEK, &[OTHER_KEK]).unwrap();
    assert_eq!(ring.open("conn_1", &sealed_by_old).unwrap(), "mine");

    assert_eq!(
        ring.key_id(),
        vault().key_id(),
        "the current key is the first"
    );
    let fresh = ring.seal("conn_1", "new").unwrap();
    assert_eq!(fresh.key_id.as_deref(), Some(vault().key_id()));
    assert!(
        other().open("conn_1", &fresh).is_err(),
        "a new seal must be under the current key, not a retired one"
    );

    assert!(ring.holds(other().key_id()));
    assert!(ring.holds(vault().key_id()));
    assert!(!ring.holds(Vault::from_base64_key(THIRD_KEK).unwrap().key_id()));
}

/// Rows written before key ids existed carry none. They must keep opening — under the current
/// key or any retired one — or the upgrade itself would lose every saved credential.
#[test]
fn a_row_sealed_before_key_ids_still_opens() {
    let mine = legacy(vault().seal("conn_1", "legacy").unwrap());
    assert_eq!(vault().open("conn_1", &mine).unwrap(), "legacy");

    let old = legacy(other().seal("conn_1", "older").unwrap());
    let ring = Vault::from_base64_keys(KEK, &[THIRD_KEK, OTHER_KEK]).unwrap();
    assert_eq!(ring.open("conn_1", &old).unwrap(), "older");
}

/// The case that has already happened once: the key was regenerated. The answer names it,
/// instead of reading as a database outage.
#[test]
fn a_blob_from_a_key_this_server_does_not_have_says_so() {
    let sealed = other().seal("conn_1", "x").unwrap();
    let error = vault().open("conn_1", &sealed).unwrap_err();
    assert!(matches!(error, StoreError::Unopenable(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("sealed with a key this server no longer has"),
        "{text}"
    );
    assert!(
        text.contains("OG_CREDENTIAL_KEK_OLD"),
        "and how to fix it: {text}"
    );
    assert!(
        !text.contains("could not be read back"),
        "not the event-log sentence: {text}"
    );

    // A blob with no key id that no key opens cannot be told apart from an altered one, so it
    // names both causes rather than guessing.
    let old = legacy(sealed);
    let text = vault().open("conn_1", &old).unwrap_err().to_string();
    assert!(
        text.contains("sealed with a key this server no longer has"),
        "{text}"
    );
    assert!(text.contains("altered"), "{text}");

    // A blob under a key this server DOES hold that will not open was altered or moved; saying
    // "no longer has" there would send the operator hunting for a key that is not lost.
    let mut tampered = vault().seal("conn_1", "x").unwrap();
    if let Some(byte) = tampered.ciphertext.first_mut() {
        *byte ^= 0xFF;
    }
    let text = vault().open("conn_1", &tampered).unwrap_err().to_string();
    assert!(!text.contains("no longer has"), "{text}");
}

/// The current key pasted into the retired list, or one old key listed twice, is one key.
#[test]
fn the_same_key_twice_is_one_key() {
    let ring = Vault::from_base64_keys(KEK, &[KEK, OTHER_KEK, OTHER_KEK]).unwrap();
    let old = other().seal("conn_1", "old").unwrap();
    assert_eq!(ring.open("conn_1", &old).unwrap(), "old");
    assert_eq!(ring.retired_key_ids(), vec![other().key_id().to_string()]);
}

/// The verdict `/health` and the boot log print. Quiet only when every row this server holds
/// opens; rows that merely want a reseal are not a problem, they are a to-do.
#[test]
fn the_check_is_quiet_only_when_everything_opens() {
    let clean = VaultCheck {
        configured: true,
        current: 3,
        ..VaultCheck::default()
    };
    assert_eq!(clean.problem(), None);
    assert_eq!(
        VaultCheck::default().problem(),
        None,
        "no key and nothing sealed is a deployment without connectors, not a fault"
    );

    let rotating = VaultCheck {
        configured: true,
        current: 1,
        retired: 2,
        legacy: 1,
        ..VaultCheck::default()
    };
    assert_eq!(rotating.problem(), None);
    assert_eq!(rotating.to_reseal(), 3);

    let lost = VaultCheck {
        configured: true,
        lost: [("0123456789ab".to_string(), 4)].into(),
        ..VaultCheck::default()
    };
    let text = lost.problem().expect("a lost key is a problem");
    assert!(
        text.contains("no longer has") && text.contains("OG_CREDENTIAL_KEK_OLD"),
        "{text}"
    );
    assert!(
        !text.contains("0123456789ab"),
        "no key id in an unauthenticated sentence: {text}"
    );

    let unkeyed = VaultCheck {
        configured: false,
        legacy: 1,
        ..VaultCheck::default()
    };
    let text = unkeyed.problem().expect("sealed rows and no key");
    assert!(text.contains("OG_CREDENTIAL_KEK is not set"), "{text}");

    let old_rows_dead = VaultCheck {
        configured: true,
        legacy: 5,
        legacy_unopenable: true,
        ..VaultCheck::default()
    };
    let text = old_rows_dead.problem().expect("the canary failed");
    assert!(
        text.contains("no longer has") && text.contains("altered"),
        "{text}"
    );

    let altered = VaultCheck {
        configured: true,
        current: 1,
        failed: vec![vault().key_id().to_string()],
        ..VaultCheck::default()
    };
    let text = altered
        .problem()
        .expect("a row under a held key that will not open");
    assert!(!text.contains("no longer has"), "{text}");
}
