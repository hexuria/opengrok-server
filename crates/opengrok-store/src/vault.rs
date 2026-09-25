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
//! NO DEFAULT KEY. `OG_CREDENTIAL_KEK` is required with no default: a default would mean every
//! deployment that forgot to set one shares a key, and a token encrypted on anybody's laptop would
//! open here.
//!
//! EVERY BLOB NAMES ITS KEY. The key has been lost once already (a reboot regenerated it, and the
//! org's box key read as "no computer"); with nothing on the row saying which key sealed it, a lost
//! key and a tampered row were the same unexplained failure. The id is derived from the key, so an
//! operator never types one and cannot mislabel one, and it is safe to store and log: it is a
//! truncated hash, never the key. Rotation is `OG_CREDENTIAL_KEK_OLD` plus `opengrok vault reseal`
//! (`vault_rows.rs`); a row with no key id predates this and is tried under every key held.

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::{StoreError, StoreResult};

/// Encrypts and decrypts credentials. One per process, built from the keys at boot: the current
/// key seals, and it or any retired key opens.
#[derive(Clone)]
pub struct Vault {
    current: HeldKey,
    retired: Vec<HeldKey>,
}

#[derive(Clone)]
struct HeldKey {
    id: String,
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
    /// Which key sealed it. `None` only on rows written before key ids were recorded.
    pub key_id: Option<String>,
}

/// Under a key this server holds, a moved row and an altered blob are one answer; telling them
/// apart takes decrypting, which is how an oracle starts.
const ALTERED: &str =
    "a stored credential could not be opened: it was altered or moved to another row";
/// Read from the row's key id, not learned by trying to decrypt, so naming it leaks nothing.
const LOST: &str = "this credential was sealed with a key this server no longer has: put that key \
     in OG_CREDENTIAL_KEK_OLD and run `opengrok vault reseal`, or save the credential again";
/// A row with no key id that no key opens cannot be told apart from an altered one, so both
/// causes are named rather than one guessed.
const UNLABELLED: &str = "this credential opens with none of this server's keys: it was sealed \
     with a key this server no longer has, or it was altered. Put the old key in \
     OG_CREDENTIAL_KEK_OLD and run `opengrok vault reseal`, or save the credential again";

impl HeldKey {
    /// Both failures are named separately because they need different fixes: one is a typo in the
    /// value, the other is a value of the wrong size. `var` names which value, because with a
    /// retired key in play the operator is looking at more than one.
    fn decode(var: &str, kek: &str) -> StoreResult<Self> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(kek.trim())
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "{var} is not valid base64: {error}; \
                     generate one with `openssl rand -base64 32`"
                ))
            })?;
        if raw.len() != 32 {
            return Err(StoreError::Corrupt(format!(
                "{var} must decode to 32 bytes, got {}; \
                 generate one with `openssl rand -base64 32`",
                raw.len()
            )));
        }
        let key = Key::try_from(raw.as_slice())
            .map_err(|_| StoreError::Corrupt(format!("{var} is the wrong size")))?;
        use sha2::Digest;
        let digest = sha2::Sha256::new()
            .chain_update(b"opengrok credential key id v1\0")
            .chain_update(&raw)
            .finalize();
        Ok(Self {
            id: digest.iter().take(6).map(|b| format!("{b:02x}")).collect(),
            cipher: ChaCha20Poly1305::new(&key),
        })
    }
}

impl Vault {
    /// Build from a base64 KEK of exactly 32 bytes, with no retired keys.
    pub fn from_base64_key(kek: &str) -> StoreResult<Self> {
        Self::from_base64_keys(kek, &[])
    }

    /// The current key (`OG_CREDENTIAL_KEK`) and the retired ones (`OG_CREDENTIAL_KEK_OLD`).
    /// A key listed twice, or the current key pasted into the retired list, is one key.
    pub fn from_base64_keys(current: &str, retired: &[&str]) -> StoreResult<Self> {
        let current = HeldKey::decode("OG_CREDENTIAL_KEK", current)?;
        let mut held: Vec<HeldKey> = Vec::new();
        for (n, kek) in retired.iter().enumerate() {
            let key = HeldKey::decode(&format!("OG_CREDENTIAL_KEK_OLD (entry {})", n + 1), kek)?;
            if key.id != current.id && held.iter().all(|k| k.id != key.id) {
                held.push(key);
            }
        }
        Ok(Self {
            current,
            retired: held,
        })
    }

    /// The id every new seal records.
    pub fn key_id(&self) -> &str {
        &self.current.id
    }

    pub fn retired_key_ids(&self) -> Vec<String> {
        self.retired.iter().map(|k| k.id.clone()).collect()
    }

    /// Whether a blob naming this key id can be opened here.
    pub fn holds(&self, key_id: &str) -> bool {
        self.keys().any(|k| k.id == key_id)
    }

    fn keys(&self) -> impl Iterator<Item = &HeldKey> {
        std::iter::once(&self.current).chain(&self.retired)
    }

    /// Seal a credential against the row it will live in, always under the current key.
    pub fn seal(&self, id: &str, plaintext: &str) -> StoreResult<Sealed> {
        let nonce_bytes: [u8; 12] = {
            use rand::RngExt;
            rand::rng().random()
        };
        let nonce = Nonce::from(nonce_bytes);

        let ciphertext = self
            .current
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
            key_id: Some(self.current.id.clone()),
        })
    }

    /// Open a credential for the row it belongs to. Every failure is `Unopenable`, and which
    /// sentence it carries depends only on the row's key id — see the three constants above.
    pub fn open(&self, id: &str, sealed: &Sealed) -> StoreResult<String> {
        let unopenable = |why: &str| StoreError::Unopenable(why.to_string());
        let bytes: [u8; 12] = sealed
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| unopenable(ALTERED))?;
        let nonce = Nonce::from(bytes);
        let payload = || Payload {
            msg: &sealed.ciphertext,
            aad: id.as_bytes(),
        };
        let plaintext = match sealed.key_id.as_deref() {
            Some(key_id) => self
                .keys()
                .find(|k| k.id == key_id)
                .ok_or_else(|| unopenable(LOST))?
                .cipher
                .decrypt(&nonce, payload())
                .map_err(|_| unopenable(ALTERED))?,
            None => self
                .keys()
                .find_map(|k| k.cipher.decrypt(&nonce, payload()).ok())
                .ok_or_else(|| unopenable(UNLABELLED))?,
        };
        String::from_utf8(plaintext).map_err(|_| unopenable(ALTERED))
    }
}
