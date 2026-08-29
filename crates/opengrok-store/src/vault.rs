//! Where a credential actually lives.
//!
//! A CREDENTIAL IS NOT AN EVENT. The log is durable, replayable and exportable — everything you
//! want for a transcript and everything you do not want for a bearer token, which would outlive the
//! session it belongs to and survive every attempt to delete it. So `connection.rs` records *that*
//! a connection exists and this records *what opens it*, encrypted, in a row that can be shredded.
//!
//! AEAD, NOT ENCRYPTION. ChaCha20-Poly1305 authenticates as well as hides: a ciphertext somebody
//! edited fails to open rather than opening to something else. That matters here because the thing
//! being decrypted is fed straight into an outbound request, so "decrypts to plausible garbage" is
//! not a failure mode worth having.
//!
//! THE ID IS THE ASSOCIATED DATA. Binding each ciphertext to its own row id means a blob moved from
//! one row to another stops opening. Without it, swapping two rows would silently swap two people's
//! credentials — the database would look untouched and the wrong token would go out.
//!
//! ROTATION IS A REAL EVENT, NOT A HOPE. `OG_CREDENTIAL_KEK` is required with no default: a default
//! would mean every deployment that forgot to set one shares a key, and a token encrypted on
//! anybody's laptop would open here.

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::{StoreError, StoreResult};

/// Encrypts and decrypts credentials. One per process, built from the KEK at boot.
#[derive(Clone)]
pub struct Vault {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for Vault {
    /// Hand-written so the key cannot reach a log through a derived `Debug`, matching `TokenMinter`
    /// and the two provider doors.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

/// A sealed credential, as it sits in a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Fresh per seal. Reusing one with the same key is the failure that breaks this cipher
    /// outright, so it is generated here and never chosen by a caller.
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl Vault {
    /// Build from a base64 KEK of exactly 32 bytes.
    ///
    /// Both failures are named separately because they need different fixes: one is a typo in the
    /// value, the other is a value of the wrong size.
    pub fn from_base64_key(kek: &str) -> StoreResult<Self> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(kek.trim())
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "OG_CREDENTIAL_KEK is not valid base64: {error}; \
                     generate one with `openssl rand -base64 32`"
                ))
            })?;

        if raw.len() != 32 {
            return Err(StoreError::Corrupt(format!(
                "OG_CREDENTIAL_KEK must decode to 32 bytes, got {}; \
                 generate one with `openssl rand -base64 32`",
                raw.len()
            )));
        }

        let key = Key::try_from(raw.as_slice())
            .map_err(|_| StoreError::Corrupt("OG_CREDENTIAL_KEK is the wrong size".to_string()))?;
        Ok(Self {
            cipher: ChaCha20Poly1305::new(&key),
        })
    }

    /// Seal a credential against the row it will live in.
    pub fn seal(&self, id: &str, plaintext: &str) -> StoreResult<Sealed> {
        let nonce_bytes: [u8; 12] = {
            use rand::RngExt;
            rand::rng().random()
        };
        let nonce = Nonce::from(nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    // The row id, so a blob moved to another row stops opening.
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| StoreError::Corrupt("a credential could not be sealed".to_string()))?;

        Ok(Sealed {
            nonce: nonce_bytes.to_vec(),
            ciphertext,
        })
    }

    /// Open a credential for the row it belongs to.
    ///
    /// The error deliberately says nothing about *why*. A wrong key, a tampered blob and a moved
    /// row are the same answer to anybody asking from outside, and distinguishing them is how a
    /// decryption oracle starts.
    pub fn open(&self, id: &str, sealed: &Sealed) -> StoreResult<String> {
        if sealed.nonce.len() != 12 {
            return Err(StoreError::Corrupt(
                "a stored credential could not be opened".to_string(),
            ));
        }
        let bytes: [u8; 12] = sealed.nonce.as_slice().try_into().map_err(|_| {
            StoreError::Corrupt("a stored credential could not be opened".to_string())
        })?;
        let nonce = Nonce::from(bytes);

        let plaintext = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &sealed.ciphertext,
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| {
                StoreError::Corrupt("a stored credential could not be opened".to_string())
            })?;

        String::from_utf8(plaintext)
            .map_err(|_| StoreError::Corrupt("a stored credential could not be opened".to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const KEK: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
    const OTHER_KEK: &str = "ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=";

    fn vault() -> Vault {
        Vault::from_base64_key(KEK).unwrap()
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
        // Both still open, so the randomness costs nothing.
        assert_eq!(vault().open("conn_1", &first).unwrap(), "same");
        assert_eq!(vault().open("conn_1", &second).unwrap(), "same");
    }

    /// THE ROW BINDING. A blob moved to another row must stop opening — otherwise swapping two
    /// rows swaps two people's credentials and the database looks untouched.
    #[test]
    fn a_credential_moved_to_another_row_will_not_open() {
        let sealed = vault().seal("conn_mine", "mine").unwrap();
        assert!(vault().open("conn_yours", &sealed).is_err());
    }

    /// AEAD, not encryption: a tampered ciphertext fails rather than opening to something else.
    #[test]
    fn a_tampered_ciphertext_is_refused_not_decrypted() {
        let mut sealed = vault().seal("conn_1", "mine").unwrap();
        if let Some(byte) = sealed.ciphertext.first_mut() {
            *byte ^= 0xFF;
        }
        assert!(vault().open("conn_1", &sealed).is_err());
    }

    #[test]
    fn another_key_cannot_open_it() {
        let sealed = vault().seal("conn_1", "mine").unwrap();
        let other = Vault::from_base64_key(OTHER_KEK).unwrap();
        assert!(other.open("conn_1", &sealed).is_err());
    }

    /// Every failure says the same thing. Distinguishing them is how a decryption oracle starts.
    #[test]
    fn every_failure_reads_identically() {
        let sealed = vault().seal("conn_1", "mine").unwrap();
        let wrong_row = vault().open("conn_2", &sealed).unwrap_err().to_string();
        let wrong_key = Vault::from_base64_key(OTHER_KEK)
            .unwrap()
            .open("conn_1", &sealed)
            .unwrap_err()
            .to_string();
        assert_eq!(wrong_row, wrong_key);
    }

    /// The two setup mistakes need different fixes, so they are named differently.
    #[test]
    fn a_bad_key_says_which_mistake_was_made() {
        let not_base64 = Vault::from_base64_key("not base64!!")
            .unwrap_err()
            .to_string();
        assert!(not_base64.contains("not valid base64"), "{not_base64}");
        assert!(not_base64.contains("openssl rand"), "and how to fix it");

        let too_short = Vault::from_base64_key("c2hvcnQ=").unwrap_err().to_string();
        assert!(too_short.contains("32 bytes"), "{too_short}");
    }

    /// However it is logged, the key must not be printable.
    #[test]
    fn the_vault_does_not_print_its_key() {
        assert_eq!(format!("{:?}", vault()), "Vault(<redacted>)");
    }

    /// A truncated nonce must be refused rather than panicking on a slice.
    #[test]
    fn a_malformed_row_is_refused_rather_than_crashing() {
        let sealed = Sealed {
            nonce: vec![0; 3],
            ciphertext: vec![1, 2, 3],
        };
        assert!(vault().open("conn_1", &sealed).is_err());
    }
}
