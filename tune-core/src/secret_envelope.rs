//! Envelope encryption with a recovery key.
//!
//! The classic "passphrase + emergency kit" scheme (1Password Secret Key,
//! Bitwarden recovery code, the iCloud keychain): a random **data encryption
//! key** (DEK) does the actual encrypting, and that DEK is wrapped twice —
//! once under the user's passphrase, once under a randomly generated
//! **recovery key** shown exactly once. Either secret opens the envelope, so
//! forgetting the passphrase does not destroy the data.
//!
//! # What this buys, and what it does not
//!
//! It defends against *loss*, not *theft*. The recovery key is a second
//! credential that also decrypts everything: leaked, it is as good as the
//! passphrase. That is the accepted trade in every product listed above, but
//! it means the recovery key must be shown once, stored by the user offline,
//! and never persisted server-side.
//!
//! Nothing here is secret at rest: [`Envelope`] contains only wrapped keys and
//! AEAD ciphertext, so it is safe to embed in a config snapshot that leaves the
//! machine (see [`crate::config_backup`]).
//!
//! # Construction
//!
//! - DEK: 32 random bytes from the OS RNG.
//! - Key wrapping: Argon2id(secret, per-slot salt) → 32-byte KEK, then
//!   XChaCha20-Poly1305 over the DEK. Two independent slots, each with its own
//!   salt and nonce, so the two secrets never share key material.
//! - Payload: XChaCha20-Poly1305 under the DEK.
//!
//! Same primitives as [`crate::db_backup`], which has been shipping the
//! password-encrypted database backups.

use serde::{Deserialize, Serialize};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20 extended nonce
const DEK_LEN: usize = 32;

/// Envelope format version. Bump when the wire shape or the primitives change;
/// [`Envelope::open`] refuses anything it does not recognise rather than
/// guessing.
pub const ENVELOPE_VERSION: u8 = 1;

/// Bytes of entropy behind a recovery key. 20 bytes = 160 bits, encoded as 32
/// Crockford base32 characters.
const RECOVERY_ENTROPY: usize = 20;

// ── Wire format ─────────────────────────────────────────────────────

/// One wrapped copy of the DEK. Hex-encoded so the envelope survives a round
/// trip through JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeySlot {
    /// Argon2id salt for deriving the key-encryption key from the secret.
    pub salt: String,
    /// XChaCha20-Poly1305 nonce used to wrap the DEK.
    pub nonce: String,
    /// The wrapped DEK (ciphertext + Poly1305 tag).
    pub wrapped_dek: String,
}

/// A payload sealed under a DEK, with the two ways to recover that DEK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u8,
    /// DEK wrapped under the user's passphrase.
    pub passphrase_slot: KeySlot,
    /// DEK wrapped under the generated recovery key.
    pub recovery_slot: KeySlot,
    /// Nonce for the payload itself.
    pub nonce: String,
    /// The sealed payload (ciphertext + Poly1305 tag).
    pub ciphertext: String,
}

/// A freshly generated recovery key, in the grouped form shown to the user.
///
/// Deliberately not [`Clone`] and never serialised: it exists for exactly as
/// long as it takes to hand it to the caller that will display it once.
#[derive(Debug)]
pub struct RecoveryKey(String);

impl RecoveryKey {
    /// The display form: `XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX`.
    pub fn display(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

// ── Crockford base32 ────────────────────────────────────────────────
// Excludes I, L, O and U: no character can be confused with another when a
// user copies the key off a screen or a printout.

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u16 = 0;
    let mut bits: u8 = 0;
    for &byte in data {
        buffer = (buffer << 8) | byte as u16;
        bits += 8;
        while bits >= 5 {
            let idx = ((buffer >> (bits - 5)) & 0x1f) as usize;
            out.push(CROCKFORD[idx] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(CROCKFORD[idx] as char);
    }
    out
}

/// Normalise a user-typed recovery key: strip grouping, uppercase, and fold the
/// characters Crockford treats as aliases (`I`/`L` → `1`, `O` → `0`).
///
/// Without this, a key read off a printout comes back as "wrong recovery key"
/// for a reason the user cannot see.
pub fn normalize_recovery_key(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect()
}

fn group(s: &str) -> String {
    s.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

// ── Hex ─────────────────────────────────────────────────────────────

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex decode: {e}")))
        .collect()
}

// ── Crypto helpers ──────────────────────────────────────────────────

fn derive_kek(secret: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(secret.as_bytes(), salt, &mut key)
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key)
}

fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| format!("OS RNG unavailable: {e}"))?;
    Ok(buf)
}

fn seal(key: &[u8; 32], nonce: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|e| e.to_string())?;
    cipher
        .encrypt(XNonce::from_slice(nonce), plaintext)
        .map_err(|e| format!("encryption failed: {e}"))
}

fn unseal(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|e| e.to_string())?;
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| "wrong secret or corrupted envelope".to_string())
}

/// Wrap a DEK under one secret, producing a fresh salt and nonce.
fn wrap_dek(secret: &str, dek: &[u8]) -> Result<KeySlot, String> {
    let salt = random_bytes(SALT_LEN)?;
    let nonce = random_bytes(NONCE_LEN)?;
    let kek = derive_kek(secret, &salt)?;
    let wrapped = seal(&kek, &nonce, dek)?;
    Ok(KeySlot {
        salt: hex_encode(&salt),
        nonce: hex_encode(&nonce),
        wrapped_dek: hex_encode(&wrapped),
    })
}

fn unwrap_dek(secret: &str, slot: &KeySlot) -> Result<[u8; DEK_LEN], String> {
    let salt = hex_decode(&slot.salt)?;
    let nonce = hex_decode(&slot.nonce)?;
    let wrapped = hex_decode(&slot.wrapped_dek)?;
    let kek = derive_kek(secret, &salt)?;
    let dek = unseal(&kek, &nonce, &wrapped)?;
    dek.try_into()
        .map_err(|_| "unwrapped key has the wrong length".to_string())
}

// ── Public API ──────────────────────────────────────────────────────

impl Envelope {
    /// Seal `plaintext` under a fresh DEK, wrapped under `passphrase` and under
    /// a newly generated recovery key.
    ///
    /// The recovery key is returned, never stored: display it once and let the
    /// user write it down. There is no way to recover it afterwards — that is
    /// the point.
    pub fn seal_new(plaintext: &[u8], passphrase: &str) -> Result<(Self, RecoveryKey), String> {
        if passphrase.is_empty() {
            return Err("passphrase must not be empty".into());
        }
        let dek = random_bytes(DEK_LEN)?;
        let recovery_raw = base32_encode(&random_bytes(RECOVERY_ENTROPY)?);

        let passphrase_slot = wrap_dek(passphrase, &dek)?;
        let recovery_slot = wrap_dek(&recovery_raw, &dek)?;

        let nonce = random_bytes(NONCE_LEN)?;
        let key: [u8; DEK_LEN] = dek
            .clone()
            .try_into()
            .map_err(|_| "DEK has the wrong length".to_string())?;
        let ciphertext = seal(&key, &nonce, plaintext)?;

        Ok((
            Envelope {
                version: ENVELOPE_VERSION,
                passphrase_slot,
                recovery_slot,
                nonce: hex_encode(&nonce),
                ciphertext: hex_encode(&ciphertext),
            },
            RecoveryKey(group(&recovery_raw)),
        ))
    }

    /// Seal `plaintext` reusing the DEK of an existing envelope, so a snapshot
    /// taken later still opens with the passphrase and recovery key the user
    /// already has.
    ///
    /// `secret` must open `self` — that is what proves the caller is entitled
    /// to the DEK. Key slots are carried over untouched.
    pub fn reseal(&self, secret: &str, plaintext: &[u8]) -> Result<Self, String> {
        let dek = self.unwrap_with(secret)?;
        let nonce = random_bytes(NONCE_LEN)?;
        let ciphertext = seal(&dek, &nonce, plaintext)?;
        Ok(Envelope {
            version: ENVELOPE_VERSION,
            passphrase_slot: self.passphrase_slot.clone(),
            recovery_slot: self.recovery_slot.clone(),
            nonce: hex_encode(&nonce),
            ciphertext: hex_encode(&ciphertext),
        })
    }

    /// Open the envelope with either the passphrase or the recovery key.
    ///
    /// The caller does not have to say which one it holds: both slots are
    /// tried. A recovery key is normalised first, so the grouped form the user
    /// copied off a printout works as typed.
    pub fn open(&self, secret: &str) -> Result<Vec<u8>, String> {
        let dek = self.unwrap_with(secret)?;
        let nonce = hex_decode(&self.nonce)?;
        let ciphertext = hex_decode(&self.ciphertext)?;
        unseal(&dek, &nonce, &ciphertext)
    }

    /// Recover the DEK from whichever slot `secret` opens.
    fn unwrap_with(&self, secret: &str) -> Result<[u8; DEK_LEN], String> {
        if self.version != ENVELOPE_VERSION {
            return Err(format!(
                "unsupported envelope version {} (this build understands {ENVELOPE_VERSION})",
                self.version
            ));
        }
        if let Ok(dek) = unwrap_dek(secret, &self.passphrase_slot) {
            return Ok(dek);
        }
        // Not the passphrase — try it as a recovery key, normalising the
        // grouping and Crockford aliases the user may have typed.
        let normalized = normalize_recovery_key(secret);
        if !normalized.is_empty() {
            if let Ok(dek) = unwrap_dek(&normalized, &self.recovery_slot) {
                return Ok(dek);
            }
        }
        Err("wrong passphrase or recovery key".into())
    }

    /// Replace the passphrase slot, keeping the DEK and the recovery key.
    ///
    /// `current_secret` must open the envelope. The recovery key the user
    /// already wrote down keeps working — rotating a passphrase must not
    /// silently invalidate the emergency kit.
    pub fn change_passphrase(
        &self,
        current_secret: &str,
        new_passphrase: &str,
    ) -> Result<Self, String> {
        if new_passphrase.is_empty() {
            return Err("passphrase must not be empty".into());
        }
        let dek = self.unwrap_with(current_secret)?;
        Ok(Envelope {
            version: ENVELOPE_VERSION,
            passphrase_slot: wrap_dek(new_passphrase, &dek)?,
            recovery_slot: self.recovery_slot.clone(),
            nonce: self.nonce.clone(),
            ciphertext: self.ciphertext.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = br#"{"tidal":"refresh-token-abc","qobuz":"token-xyz"}"#;

    #[test]
    fn passphrase_opens_the_envelope() {
        let (env, _rk) = Envelope::seal_new(SECRET, "correct horse battery").unwrap();
        assert_eq!(env.open("correct horse battery").unwrap(), SECRET);
    }

    #[test]
    fn recovery_key_opens_the_envelope() {
        let (env, rk) = Envelope::seal_new(SECRET, "passphrase").unwrap();
        // The whole point: the passphrase is gone, the emergency kit still works.
        assert_eq!(env.open(rk.display()).unwrap(), SECRET);
    }

    #[test]
    fn recovery_key_survives_being_retyped() {
        let (env, rk) = Envelope::seal_new(SECRET, "passphrase").unwrap();
        // Lowercased, spaces instead of dashes — what a user actually types.
        let retyped = rk.display().to_lowercase().replace('-', " ");
        assert_eq!(env.open(&retyped).unwrap(), SECRET);
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let (env, _rk) = Envelope::seal_new(SECRET, "passphrase").unwrap();
        assert!(env.open("not the passphrase").is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let (mut env, _rk) = Envelope::seal_new(SECRET, "passphrase").unwrap();
        // Flip the last ciphertext byte → Poly1305 must reject it.
        let mut bytes = hex_decode(&env.ciphertext).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        env.ciphertext = hex_encode(&bytes);
        assert!(env.open("passphrase").is_err());
    }

    #[test]
    fn the_two_slots_do_not_share_key_material() {
        let (env, _rk) = Envelope::seal_new(SECRET, "passphrase").unwrap();
        assert_ne!(env.passphrase_slot.salt, env.recovery_slot.salt);
        assert_ne!(env.passphrase_slot.nonce, env.recovery_slot.nonce);
    }

    #[test]
    fn recovery_keys_are_unique() {
        let (_e1, rk1) = Envelope::seal_new(SECRET, "pw").unwrap();
        let (_e2, rk2) = Envelope::seal_new(SECRET, "pw").unwrap();
        assert_ne!(rk1.display(), rk2.display());
    }

    #[test]
    fn recovery_key_is_grouped_and_unambiguous() {
        let (_env, rk) = Envelope::seal_new(SECRET, "pw").unwrap();
        let shown = rk.display();
        assert_eq!(shown.len(), 32 + 7, "32 chars in 8 groups of 4");
        for c in shown.chars().filter(|c| *c != '-') {
            assert!(
                CROCKFORD.contains(&(c as u8)),
                "{c} is not in the Crockford alphabet"
            );
        }
    }

    #[test]
    fn resealing_keeps_both_secrets_working() {
        let (env, rk) = Envelope::seal_new(SECRET, "pw").unwrap();
        let later = env.reseal("pw", b"a newer set of tokens").unwrap();
        assert_eq!(later.open("pw").unwrap(), b"a newer set of tokens");
        assert_eq!(later.open(rk.display()).unwrap(), b"a newer set of tokens");
    }

    #[test]
    fn resealing_requires_a_valid_secret() {
        let (env, _rk) = Envelope::seal_new(SECRET, "pw").unwrap();
        assert!(env.reseal("wrong", b"payload").is_err());
    }

    /// Rotating the passphrase must not invalidate the emergency kit the user
    /// already printed.
    #[test]
    fn changing_the_passphrase_preserves_the_recovery_key() {
        let (env, rk) = Envelope::seal_new(SECRET, "old pw").unwrap();
        let rotated = env.change_passphrase("old pw", "new pw").unwrap();
        assert_eq!(rotated.open("new pw").unwrap(), SECRET);
        assert_eq!(rotated.open(rk.display()).unwrap(), SECRET);
        assert!(rotated.open("old pw").is_err());
    }

    /// The recovery key alone is enough to set a new passphrase — the whole
    /// point of holding one.
    #[test]
    fn the_recovery_key_can_reset_the_passphrase() {
        let (env, rk) = Envelope::seal_new(SECRET, "forgotten").unwrap();
        let rotated = env.change_passphrase(rk.display(), "brand new").unwrap();
        assert_eq!(rotated.open("brand new").unwrap(), SECRET);
    }

    #[test]
    fn empty_passphrase_is_refused() {
        assert!(Envelope::seal_new(SECRET, "").is_err());
    }

    #[test]
    fn a_future_version_is_refused_rather_than_guessed() {
        let (mut env, _rk) = Envelope::seal_new(SECRET, "pw").unwrap();
        env.version = ENVELOPE_VERSION + 1;
        let err = env.open("pw").unwrap_err();
        assert!(err.contains("unsupported envelope version"), "{err}");
    }

    #[test]
    fn envelope_survives_a_json_round_trip() {
        let (env, rk) = Envelope::seal_new(SECRET, "pw").unwrap();
        let json = serde_json::to_string(&env).unwrap();
        // Nothing recoverable must appear in the serialised form.
        assert!(!json.contains("refresh-token-abc"));
        assert!(!json.contains(rk.display()));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.open("pw").unwrap(), SECRET);
    }
}
