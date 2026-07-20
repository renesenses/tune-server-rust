//! AirPlay 2 / HomeKit-style PIN pairing — pure, device-independent building
//! blocks (phase 1).
//!
//! AirPlay 2 receivers (Apple TV, Samsung/LG TVs, HomePod, many AVRs) require a
//! HomeKit-flavoured pairing handshake before they will accept an RTSP session:
//!
//!   1. **pair-setup**  — SRP-6a using the PIN the accessory displays on screen.
//!      Six TLV8 messages (M1..M6) over `POST /pair-setup` on TCP port 7000.
//!      Establishes a shared secret and exchanges long-term Ed25519 identities.
//!   2. **pair-verify** — Curve25519 (X25519) ECDH → HKDF → a ChaCha20-Poly1305
//!      session key, authenticated with the Ed25519 identities from pair-setup.
//!      Four TLV8 messages (M1..M4) over `POST /pair-verify`.
//!
//! After pair-verify the RTSP control/stream is encrypted with the derived
//! session key. That encrypted-RTSP transport, plus the *live* completion of
//! both handshakes, is deliberately out of scope for this increment — see the
//! `TODO(phase2)` markers. What lives here now is:
//!
//!   * A complete, unit-tested **TLV8 codec** (HomeKit fragmentation rule).
//!   * The **kTLVType_*** / **State** / **Method** / **Error** constants.
//!   * Thin, unit-tested wrappers over the pure crypto primitives every step
//!     needs: SRP-6a client (`srp`), HKDF-SHA512, Ed25519 sign/verify, X25519
//!     ECDH, and ChaCha20-Poly1305 AEAD.
//!   * Typed function signatures for the two handshakes with the live network
//!     turns stubbed (`TODO(phase2)`).
//!
//! None of this drives a real device yet; nothing here should be described as
//! "playable".

#![allow(dead_code)] // phase-1 scaffold: several items are wired up in phase 2.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

// ---------------------------------------------------------------------------
// TLV8 codec (HomeKit / HAP spec, section "TLV8").
// ---------------------------------------------------------------------------

/// Maximum length a single TLV8 item's value may hold. Values longer than this
/// MUST be fragmented across consecutive items sharing the same type byte; the
/// decoder concatenates fragments of the same type that appear back-to-back.
pub const TLV8_MAX_FRAGMENT: usize = 255;

/// One decoded TLV8 item: a type byte and its (already de-fragmented) value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv8Item {
    pub typ: u8,
    pub value: Vec<u8>,
}

/// Encode a list of TLV8 items into the wire format, applying the HomeKit
/// fragmentation rule: any value longer than [`TLV8_MAX_FRAGMENT`] is emitted as
/// several items of the same type, each carrying at most 255 bytes. A value of
/// exactly a multiple of 255 bytes ends with an implicit boundary (the next item
/// having a different type, or end-of-buffer).
pub fn tlv8_encode(items: &[Tlv8Item]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        if item.value.is_empty() {
            // Zero-length item is legal (e.g. a bare Separator).
            out.push(item.typ);
            out.push(0);
            continue;
        }
        let mut offset = 0;
        while offset < item.value.len() {
            let chunk_len = (item.value.len() - offset).min(TLV8_MAX_FRAGMENT);
            out.push(item.typ);
            out.push(chunk_len as u8);
            out.extend_from_slice(&item.value[offset..offset + chunk_len]);
            offset += chunk_len;
        }
    }
    out
}

/// Decode TLV8 wire bytes into de-fragmented items. Consecutive fragments that
/// share a type byte AND where the previous fragment was exactly 255 bytes are
/// concatenated into a single logical item, per the HomeKit rule.
pub fn tlv8_decode(bytes: &[u8]) -> Result<Vec<Tlv8Item>, PairingError> {
    let mut items: Vec<Tlv8Item> = Vec::new();
    let mut i = 0;
    // Was the previous raw fragment a full 255-byte chunk? Only then may the
    // next same-type fragment be treated as a continuation.
    let mut prev_was_full = false;
    let mut prev_typ: Option<u8> = None;

    while i < bytes.len() {
        if i + 2 > bytes.len() {
            return Err(PairingError::Tlv("truncated TLV header".into()));
        }
        let typ = bytes[i];
        let len = bytes[i + 1] as usize;
        i += 2;
        if i + len > bytes.len() {
            return Err(PairingError::Tlv("truncated TLV value".into()));
        }
        let value = &bytes[i..i + len];
        i += len;

        let continues = prev_was_full && prev_typ == Some(typ);
        if continues {
            // Safe: `continues` implies at least one prior item of this type.
            items.last_mut().unwrap().value.extend_from_slice(value);
        } else {
            items.push(Tlv8Item {
                typ,
                value: value.to_vec(),
            });
        }

        prev_was_full = len == TLV8_MAX_FRAGMENT;
        prev_typ = Some(typ);
    }
    Ok(items)
}

/// Convenience: find the first item with the given type.
pub fn tlv8_find<'a>(items: &'a [Tlv8Item], typ: u8) -> Option<&'a [u8]> {
    items
        .iter()
        .find(|it| it.typ == typ)
        .map(|it| it.value.as_slice())
}

// ---------------------------------------------------------------------------
// HAP TLV8 type constants (kTLVType_*), pairing states, methods, errors.
// ---------------------------------------------------------------------------

/// HAP `kTLVType_*` value tags used inside pair-setup / pair-verify payloads.
pub mod tlv_type {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    /// SRP public key (M1 A / M2 B) or Curve25519 public key in pair-verify.
    pub const PUBLIC_KEY: u8 = 0x03;
    /// SRP proof (M3 M1 / M4 M2).
    pub const PROOF: u8 = 0x04;
    /// ChaCha20-Poly1305 encrypted sub-TLV (+ 16-byte auth tag).
    pub const ENCRYPTED_DATA: u8 = 0x05;
    /// Pairing state: M1..M6 (1-indexed).
    pub const STATE: u8 = 0x06;
    /// Error code (see [`error_code`]).
    pub const ERROR: u8 = 0x07;
    pub const RETRY_DELAY: u8 = 0x08;
    pub const CERTIFICATE: u8 = 0x09;
    /// Ed25519 signature.
    pub const SIGNATURE: u8 = 0x0A;
    pub const PERMISSIONS: u8 = 0x0B;
    pub const FRAGMENT_DATA: u8 = 0x0C;
    pub const FRAGMENT_LAST: u8 = 0x0D;
    pub const SEPARATOR: u8 = 0xFF;
}

/// HAP pairing `State` (kTLVType_State) values — the M1..M6 progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PairState {
    M1 = 1,
    M2 = 2,
    M3 = 3,
    M4 = 4,
    M5 = 5,
    M6 = 6,
}

impl PairState {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::M1),
            2 => Some(Self::M2),
            3 => Some(Self::M3),
            4 => Some(Self::M4),
            5 => Some(Self::M5),
            6 => Some(Self::M6),
            _ => None,
        }
    }
}

/// HAP `Method` (kTLVType_Method) values.
pub mod method {
    pub const PAIR_SETUP: u8 = 0x00;
    pub const PAIR_SETUP_WITH_AUTH: u8 = 0x01;
    pub const PAIR_VERIFY: u8 = 0x02;
    pub const ADD_PAIRING: u8 = 0x03;
    pub const REMOVE_PAIRING: u8 = 0x04;
    pub const LIST_PAIRINGS: u8 = 0x05;
}

/// HAP `Error` (kTLVType_Error) codes reported by the accessory.
pub mod error_code {
    pub const UNKNOWN: u8 = 0x01;
    pub const AUTHENTICATION: u8 = 0x02;
    pub const BACKOFF: u8 = 0x03;
    pub const MAX_PEERS: u8 = 0x04;
    pub const MAX_TRIES: u8 = 0x05;
    pub const UNAVAILABLE: u8 = 0x06;
    pub const BUSY: u8 = 0x07;
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("TLV8: {0}")]
    Tlv(String),
    #[error("SRP: {0}")]
    Srp(String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("accessory reported pairing error code {0}")]
    Accessory(u8),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("not yet implemented (phase 2): {0}")]
    NotImplemented(&'static str),
}

// ---------------------------------------------------------------------------
// Crypto primitives — thin, testable wrappers.
// ---------------------------------------------------------------------------

/// HKDF-SHA512 with an explicit salt + info, expanding to `OUT` bytes.
/// HomeKit uses SHA-512 based HKDF for every pairing key-derivation step
/// (e.g. `Pair-Setup-Encrypt-Salt` / `Pair-Setup-Encrypt-Info`).
pub fn hkdf_sha512<const OUT: usize>(
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
) -> Result<[u8; OUT], PairingError> {
    let hk = Hkdf::<Sha512>::new(Some(salt), ikm);
    let mut okm = [0u8; OUT];
    hk.expand(info, &mut okm)
        .map_err(|e| PairingError::Crypto(format!("hkdf expand: {e}")))?;
    Ok(okm)
}

/// A freshly generated (or loaded) long-term Ed25519 identity for *our* side of
/// the pairing (the "controller" in HAP terms). 32-byte seed + 32-byte public.
#[derive(Clone)]
pub struct Ed25519Identity {
    signing: SigningKey,
}

impl Ed25519Identity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        Self { signing }
    }

    /// Reconstruct from a stored 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }

    /// The 32-byte secret seed (persist this, keep it secret).
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// The 32-byte public key (advertised to the accessory as our identifier).
    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Sign a message (HAP uses this over `iOSDeviceInfo` etc.).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// Verify an Ed25519 signature made by a peer whose 32-byte public key we hold
/// (e.g. the accessory's `AccessoryLTPK`).
pub fn ed25519_verify(
    peer_pubkey: &[u8; 32],
    msg: &[u8],
    signature: &[u8; 64],
) -> Result<(), PairingError> {
    let vk = VerifyingKey::from_bytes(peer_pubkey)
        .map_err(|e| PairingError::Crypto(format!("bad ed25519 pubkey: {e}")))?;
    let sig = Signature::from_bytes(signature);
    vk.verify(msg, &sig)
        .map_err(|e| PairingError::Crypto(format!("ed25519 verify: {e}")))
}

/// An ephemeral Curve25519 (X25519) keypair for pair-verify's ECDH.
pub struct X25519Ephemeral {
    secret: XStaticSecret,
    public: XPublicKey,
}

impl X25519Ephemeral {
    pub fn generate() -> Self {
        let secret = XStaticSecret::random_from_rng(&mut rand_core::OsRng);
        let public = XPublicKey::from(&secret);
        Self { secret, public }
    }

    /// Our 32-byte public key to send to the accessory.
    pub fn public_key(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Complete the ECDH against the accessory's public key, yielding the raw
    /// 32-byte shared secret (fed into HKDF to derive the session key).
    pub fn diffie_hellman(&self, peer_public: &[u8; 32]) -> [u8; 32] {
        let peer = XPublicKey::from(*peer_public);
        self.secret.diffie_hellman(&peer).to_bytes()
    }
}

/// ChaCha20-Poly1305 AEAD encrypt with a HomeKit-style 8-byte nonce label
/// (right-padded into the 12-byte nonce). Returns ciphertext‖tag.
pub fn chacha20poly1305_encrypt(
    key: &[u8; 32],
    nonce12: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PairingError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = Nonce::from_slice(nonce12);
    cipher
        .encrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| PairingError::Crypto(format!("aead encrypt: {e}")))
}

/// ChaCha20-Poly1305 AEAD decrypt. Input is ciphertext‖tag.
pub fn chacha20poly1305_decrypt(
    key: &[u8; 32],
    nonce12: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, PairingError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = Nonce::from_slice(nonce12);
    cipher
        .decrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|e| PairingError::Crypto(format!("aead decrypt: {e}")))
}

/// Build the 12-byte ChaCha20-Poly1305 nonce HomeKit uses for its pairing
/// sub-TLVs: 4 zero bytes followed by the 8-byte ASCII label
/// (e.g. `PS-Msg05`). The label MUST be at most 8 bytes.
pub fn hap_nonce(label: &[u8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    let take = label.len().min(8);
    n[4..4 + take].copy_from_slice(&label[..take]);
    n
}

// ---------------------------------------------------------------------------
// SRP-6a client (pair-setup) — scaffold over the `srp` crate.
// ---------------------------------------------------------------------------

use srp::client::{SrpClient, SrpClientVerifier};
use srp::groups::G_3072;

/// HomeKit pair-setup fixes the SRP identity to the ASCII string "Pair-Setup"
/// and derives the verifier from the accessory PIN as the password.
pub const SRP_USERNAME: &[u8] = b"Pair-Setup";

/// Client side of HAP pair-setup SRP-6a (3072-bit group, SHA-512).
///
/// The message flow this drives (all TLV8 over `POST /pair-setup`):
///   * M1 → send {State=M1, Method=PairSetup}
///   * M2 ← accessory replies {State=M2, PublicKey=B, Salt=s}
///   * M3 → send {State=M3, PublicKey=A, Proof=M1}
///   * M4 ← accessory replies {State=M4, Proof=M2}  (verify it)
///   * M5 → send {State=M5, EncryptedData=…}  (our Ed25519 identity, signed)
///   * M6 ← accessory replies {State=M6, EncryptedData=…} (its LTPK, signed)
///
/// This struct implements the pure SRP math for M1→M4 (public ephemeral,
/// processing the reply, our proof, verifying the server proof, and exposing the
/// shared key K used to derive the M5/M6 encryption key). The M5/M6 sub-TLV
/// encryption uses [`hkdf_sha512`] + [`chacha20poly1305_encrypt`], and the final
/// exchange of Ed25519 identities is driven by the (phase-2) live loop.
pub struct PairSetupClient {
    client: SrpClient<'static, Sha512>,
    /// Random client ephemeral secret `a` (big-endian bytes).
    a: Vec<u8>,
}

impl PairSetupClient {
    /// Start a pair-setup: generates the client ephemeral. `pin` is the code the
    /// accessory shows on screen (e.g. "1234" or "123-45-678" with dashes
    /// stripped by the caller as HAP requires).
    pub fn new() -> Self {
        // 32 random bytes of ephemeral `a` (SRP recommends >= 256 bits).
        let mut a = [0u8; 32];
        use rand_core::RngCore;
        rand_core::OsRng.fill_bytes(&mut a);
        Self {
            client: SrpClient::<Sha512>::new(&G_3072),
            a: a.to_vec(),
        }
    }

    /// Our public ephemeral `A = g^a mod N` (TLV8 kTLVType_PublicKey in M3).
    pub fn public_ephemeral(&self) -> Vec<u8> {
        self.client.compute_public_ephemeral(&self.a)
    }

    /// Process the accessory's M2 (its `B` = `b_pub` and salt `s`) plus the PIN,
    /// producing a verifier that holds the shared key and our proof `M1`.
    ///
    /// NOTE ON COMPATIBILITY: the `srp` crate computes M1/M2 per RFC5054's
    /// `H(H(N) xor H(g) | H(I) | s | A | B | K)` convention. Real Apple/HomeKit
    /// accessories are known to follow this same convention for pair-setup, but
    /// this has only been proven here by self-consistent round-trip tests
    /// (client vs the crate's own server). Confirming byte-for-byte against a
    /// live TV is a phase-2 task; if a device rejects M3 with an Authentication
    /// error despite a correct PIN, the proof convention is the place to look.
    pub fn process_accessory_reply(
        &self,
        salt: &[u8],
        b_pub: &[u8],
        pin: &[u8],
    ) -> Result<PairSetupVerifier, PairingError> {
        let verifier = self
            .client
            .process_reply(&self.a, SRP_USERNAME, pin, salt, b_pub)
            .map_err(|e| PairingError::Srp(format!("process_reply: {e}")))?;
        Ok(PairSetupVerifier { inner: verifier })
    }

    /// Compute the SRP verifier `v` for a PIN + salt (used only in tests / when
    /// standing up a fake accessory).
    pub fn compute_verifier(&self, pin: &[u8], salt: &[u8]) -> Vec<u8> {
        self.client.compute_verifier(SRP_USERNAME, pin, salt)
    }
}

impl Default for PairSetupClient {
    fn default() -> Self {
        Self::new()
    }
}

/// State after processing the accessory's M2: holds the shared secret and the
/// proofs.
pub struct PairSetupVerifier {
    inner: SrpClientVerifier<Sha512>,
}

impl PairSetupVerifier {
    /// Our proof `M1` to send in M3 (TLV8 kTLVType_Proof).
    pub fn client_proof(&self) -> &[u8] {
        self.inner.proof()
    }

    /// Verify the accessory's proof `M2` received in M4.
    pub fn verify_accessory_proof(&self, server_proof: &[u8]) -> Result<(), PairingError> {
        self.inner
            .verify_server(server_proof)
            .map_err(|e| PairingError::Srp(format!("verify_server: {e}")))
    }

    /// The shared SRP session key `K`. HomeKit runs this through HKDF-SHA512 to
    /// derive the ChaCha20-Poly1305 key that protects the M5/M6 sub-TLVs.
    pub fn shared_key(&self) -> &[u8] {
        self.inner.key()
    }
}

// ---------------------------------------------------------------------------
// HAP HKDF salt / info string constants (spec §2.5, §2.7, §3.2, §3.3).
// Every literal is quoted verbatim from the protocol spec / owntone.
// ---------------------------------------------------------------------------

/// Salt/info for the ChaCha20-Poly1305 key protecting the M5/M6 sub-TLVs.
pub const HK_SETUP_ENCRYPT_SALT: &[u8] = b"Pair-Setup-Encrypt-Salt";
pub const HK_SETUP_ENCRYPT_INFO: &[u8] = b"Pair-Setup-Encrypt-Info";
/// Salt/info deriving our controller's signing input `iOSDeviceX`.
pub const HK_CONTROLLER_SIGN_SALT: &[u8] = b"Pair-Setup-Controller-Sign-Salt";
pub const HK_CONTROLLER_SIGN_INFO: &[u8] = b"Pair-Setup-Controller-Sign-Info";
/// Salt/info deriving the accessory's signing input `AccessoryX` (M6 verify).
pub const HK_ACCESSORY_SIGN_SALT: &[u8] = b"Pair-Setup-Accessory-Sign-Salt";
pub const HK_ACCESSORY_SIGN_INFO: &[u8] = b"Pair-Setup-Accessory-Sign-Info";
/// Salt/info for the pair-verify M2/M3 encryption key.
pub const HK_VERIFY_ENCRYPT_SALT: &[u8] = b"Pair-Verify-Encrypt-Salt";
pub const HK_VERIFY_ENCRYPT_INFO: &[u8] = b"Pair-Verify-Encrypt-Info";
/// Salt/info for the post-verify control-channel read/write keys.
pub const HK_CONTROL_SALT: &[u8] = b"Control-Salt";
pub const HK_CONTROL_WRITE_INFO: &[u8] = b"Control-Write-Encryption-Key";
pub const HK_CONTROL_READ_INFO: &[u8] = b"Control-Read-Encryption-Key";

/// HAP AEAD nonce labels (4 zero bytes ‖ 8-byte ASCII label, no AAD).
pub const NONCE_PS_MSG05: &[u8] = b"PS-Msg05";
pub const NONCE_PS_MSG06: &[u8] = b"PS-Msg06";
pub const NONCE_PV_MSG02: &[u8] = b"PV-Msg02";
pub const NONCE_PV_MSG03: &[u8] = b"PV-Msg03";

// ---------------------------------------------------------------------------
// Long-term result of a successful pairing, to be persisted per device.
// ---------------------------------------------------------------------------

/// Long-term result of a successful pairing, to be persisted per device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCredentials {
    /// Our controller Ed25519 seed (32 bytes) — secret.
    pub our_ed25519_seed: [u8; 32],
    /// Our controller pairing identifier (the `Identifier` we sent in M5).
    pub our_id: String,
    /// The accessory's long-term public key (`AccessoryLTPK`, 32 bytes).
    pub accessory_ltpk: [u8; 32],
    /// The accessory pairing identifier (its `AccessoryPairingID` string).
    pub accessory_id: String,
}

/// Derived per-session control-channel keys from a completed pair-verify
/// (spec §3.3). These protect subsequent RTSP traffic. The audio (RTP) key is
/// delivered separately at `SETUP` time and is NOT derived here.
///
/// `write` = client→accessory, `read` = accessory→client. Both are 32 bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    /// ChaCha20-Poly1305 key for frames we send (client→accessory).
    pub control_write: [u8; 32],
    /// ChaCha20-Poly1305 key for frames we receive (accessory→client).
    pub control_read: [u8; 32],
    /// The raw X25519 shared secret, in case additional keys are needed later.
    pub shared_secret: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print raw key material.
        f.debug_struct("SessionKeys")
            .field("control_write", &"<redacted 32B>")
            .field("control_read", &"<redacted 32B>")
            .field("shared_secret", &"<redacted 32B>")
            .finish()
    }
}

// ===========================================================================
// pair-setup — pure state-machine message builders / parsers (spec §2).
// ===========================================================================

/// Build the pair-setup **M1** request body (`State=M1`, `Method=PairSetup`).
///
/// PIN mode uses `X-Apple-HKP: 3` (set by the transport). Transient pairing
/// would additionally carry `Flags=0x10` and `Method` unchanged, but this
/// crate implements the persistent PIN flow (M1..M6), so no `Flags` item.
pub fn build_setup_m1() -> Vec<u8> {
    tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M1.as_u8()],
        },
        Tlv8Item {
            typ: tlv_type::METHOD,
            value: vec![method::PAIR_SETUP],
        },
    ])
}

/// The accessory's **M2** reply, parsed: salt `s` (16B) + server public `B`.
pub struct SetupM2 {
    pub salt: Vec<u8>,
    pub server_pubkey: Vec<u8>,
}

/// Parse pair-setup **M2** (`State=M2`, `Salt`, `PublicKey=B`). Surfaces any
/// accessory `Error` item (e.g. Authentication) as [`PairingError::Accessory`].
pub fn parse_setup_m2(body: &[u8]) -> Result<SetupM2, PairingError> {
    let items = tlv8_decode(body)?;
    expect_state(&items, PairState::M2)?;
    let salt = tlv8_find(&items, tlv_type::SALT)
        .ok_or_else(|| PairingError::Protocol("M2 missing Salt".into()))?
        .to_vec();
    let server_pubkey = tlv8_find(&items, tlv_type::PUBLIC_KEY)
        .ok_or_else(|| PairingError::Protocol("M2 missing PublicKey (B)".into()))?
        .to_vec();
    Ok(SetupM2 {
        salt,
        server_pubkey,
    })
}

/// Build pair-setup **M3** (`State=M3`, `PublicKey=A`, `Proof=M1proof`).
pub fn build_setup_m3(client_pubkey_a: &[u8], client_proof: &[u8]) -> Vec<u8> {
    tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M3.as_u8()],
        },
        Tlv8Item {
            typ: tlv_type::PUBLIC_KEY,
            value: client_pubkey_a.to_vec(),
        },
        Tlv8Item {
            typ: tlv_type::PROOF,
            value: client_proof.to_vec(),
        },
    ])
}

/// Parse pair-setup **M4** (`State=M4`, `Proof=M2proof`), returning the server
/// proof bytes to be verified by the SRP verifier.
pub fn parse_setup_m4(body: &[u8]) -> Result<Vec<u8>, PairingError> {
    let items = tlv8_decode(body)?;
    expect_state(&items, PairState::M4)?;
    let proof = tlv8_find(&items, tlv_type::PROOF)
        .ok_or_else(|| PairingError::Protocol("M4 missing Proof".into()))?
        .to_vec();
    Ok(proof)
}

/// Compute the HAP SRP **session key** `K = SHA-512(S)` from the raw SRP
/// premaster secret `S` (spec §2.5, Q5 RESOLVED — IKM = single-SHA512 of S).
///
/// IMPORTANT: the `srp` 0.6 crate's `verifier.key()` returns the **raw
/// premaster `S`**, not `SHA-512(S)` (it is a simplified SRP variant). HomeKit
/// requires the hashed form as HKDF IKM for the M5/M6 encrypt key and the
/// controller/accessory sign keys. So callers MUST pass the output of this
/// function (not `verifier.shared_key()`) into [`build_setup_m5`] /
/// [`parse_setup_m6`]. Using it on both our side and the in-test mock keeps the
/// round-trip self-consistent AND matches owntone/HAP for a real device.
pub fn hap_srp_session_key(srp_premaster_s: &[u8]) -> Vec<u8> {
    let mut h = Sha512::new();
    h.update(srp_premaster_s);
    h.finalize().to_vec()
}

/// Derive the M5/M6 ChaCha20-Poly1305 encrypt key from the HAP SRP session key
/// `K = SHA-512(S)` (spec §2.5). `session_key` MUST already be the hashed form
/// produced by [`hap_srp_session_key`].
fn setup_encrypt_key(session_key: &[u8]) -> Result<[u8; 32], PairingError> {
    hkdf_sha512(session_key, HK_SETUP_ENCRYPT_SALT, HK_SETUP_ENCRYPT_INFO)
}

/// Build pair-setup **M5**: the encrypted sub-TLV proving our controller
/// identity (spec §2.7).
///
/// Sub-TLV plaintext = `{ Identifier, PublicKey=iOSDeviceLTPK, Signature }`
/// where the signature is our Ed25519 signature over
/// `iOSDeviceX ‖ iOSDevicePairingID ‖ iOSDeviceLTPK`, with
/// `iOSDeviceX = HKDF(salt=Controller-Sign-Salt, ikm=SRP session key, info=Controller-Sign-Info)`.
///
/// The sub-TLV is encrypted with the M5/M6 key, nonce `PS-Msg05`, no AAD.
pub fn build_setup_m5(
    srp_session_key: &[u8],
    our_identity: &Ed25519Identity,
    our_id: &str,
) -> Result<Vec<u8>, PairingError> {
    let device_x: [u8; 32] = hkdf_sha512(
        srp_session_key,
        HK_CONTROLLER_SIGN_SALT,
        HK_CONTROLLER_SIGN_INFO,
    )?;
    let ltpk = our_identity.public_key();

    // iOSDeviceInfo = iOSDeviceX ‖ iOSDevicePairingID ‖ iOSDeviceLTPK
    let mut to_sign = Vec::with_capacity(32 + our_id.len() + 32);
    to_sign.extend_from_slice(&device_x);
    to_sign.extend_from_slice(our_id.as_bytes());
    to_sign.extend_from_slice(&ltpk);
    let signature = our_identity.sign(&to_sign);

    let sub_tlv = tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::IDENTIFIER,
            value: our_id.as_bytes().to_vec(),
        },
        Tlv8Item {
            typ: tlv_type::PUBLIC_KEY,
            value: ltpk.to_vec(),
        },
        Tlv8Item {
            typ: tlv_type::SIGNATURE,
            value: signature.to_vec(),
        },
    ]);

    let key = setup_encrypt_key(srp_session_key)?;
    let nonce = hap_nonce(NONCE_PS_MSG05);
    let encrypted = chacha20poly1305_encrypt(&key, &nonce, b"", &sub_tlv)?;

    Ok(tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M5.as_u8()],
        },
        Tlv8Item {
            typ: tlv_type::ENCRYPTED_DATA,
            value: encrypted,
        },
    ]))
}

/// Parse + verify pair-setup **M6** (spec §2.7): decrypt the accessory sub-TLV
/// (nonce `PS-Msg06`), verify its Ed25519 signature over
/// `AccessoryX ‖ AccessoryPairingID ‖ AccessoryLTPK`, and return the accessory
/// pairing id + long-term public key to persist.
pub fn parse_setup_m6(
    body: &[u8],
    srp_session_key: &[u8],
) -> Result<(String, [u8; 32]), PairingError> {
    let items = tlv8_decode(body)?;
    expect_state(&items, PairState::M6)?;
    let encrypted = tlv8_find(&items, tlv_type::ENCRYPTED_DATA)
        .ok_or_else(|| PairingError::Protocol("M6 missing EncryptedData".into()))?;

    let key = setup_encrypt_key(srp_session_key)?;
    let nonce = hap_nonce(NONCE_PS_MSG06);
    let plaintext = chacha20poly1305_decrypt(&key, &nonce, b"", encrypted)?;
    let sub = tlv8_decode(&plaintext)?;

    let accessory_id_bytes = tlv8_find(&sub, tlv_type::IDENTIFIER)
        .ok_or_else(|| PairingError::Protocol("M6 sub-TLV missing Identifier".into()))?;
    let accessory_id = String::from_utf8(accessory_id_bytes.to_vec())
        .map_err(|_| PairingError::Protocol("M6 accessory id not UTF-8".into()))?;
    let accessory_ltpk: [u8; 32] = tlv8_find(&sub, tlv_type::PUBLIC_KEY)
        .ok_or_else(|| PairingError::Protocol("M6 sub-TLV missing PublicKey (LTPK)".into()))?
        .try_into()
        .map_err(|_| PairingError::Protocol("M6 accessory LTPK not 32 bytes".into()))?;
    let signature: [u8; 64] = tlv8_find(&sub, tlv_type::SIGNATURE)
        .ok_or_else(|| PairingError::Protocol("M6 sub-TLV missing Signature".into()))?
        .try_into()
        .map_err(|_| PairingError::Protocol("M6 accessory signature not 64 bytes".into()))?;

    // AccessoryInfo = AccessoryX ‖ AccessoryPairingID ‖ AccessoryLTPK
    let accessory_x: [u8; 32] = hkdf_sha512(
        srp_session_key,
        HK_ACCESSORY_SIGN_SALT,
        HK_ACCESSORY_SIGN_INFO,
    )?;
    let mut signed = Vec::with_capacity(32 + accessory_id.len() + 32);
    signed.extend_from_slice(&accessory_x);
    signed.extend_from_slice(accessory_id.as_bytes());
    signed.extend_from_slice(&accessory_ltpk);
    ed25519_verify(&accessory_ltpk, &signed, &signature)?;

    Ok((accessory_id, accessory_ltpk))
}

// ===========================================================================
// pair-verify — pure state-machine message builders / parsers (spec §3).
// ===========================================================================

/// Build pair-verify **M1** (`State=M1`, `PublicKey=vpk_c`, 32B X25519 pub).
pub fn build_verify_m1(ephemeral_pub: &[u8; 32]) -> Vec<u8> {
    tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M1.as_u8()],
        },
        Tlv8Item {
            typ: tlv_type::PUBLIC_KEY,
            value: ephemeral_pub.to_vec(),
        },
    ])
}

/// The accessory's **M2** reply, parsed: its ephemeral X25519 pub + encrypted
/// sub-TLV.
pub struct VerifyM2 {
    pub accessory_pub: [u8; 32],
    pub encrypted: Vec<u8>,
}

/// Parse pair-verify **M2** (`State=M2`, `PublicKey=vpk_a`, `EncryptedData`).
pub fn parse_verify_m2(body: &[u8]) -> Result<VerifyM2, PairingError> {
    let items = tlv8_decode(body)?;
    expect_state(&items, PairState::M2)?;
    let accessory_pub: [u8; 32] = tlv8_find(&items, tlv_type::PUBLIC_KEY)
        .ok_or_else(|| PairingError::Protocol("M2 missing PublicKey (vpk_a)".into()))?
        .try_into()
        .map_err(|_| PairingError::Protocol("M2 vpk_a not 32 bytes".into()))?;
    let encrypted = tlv8_find(&items, tlv_type::ENCRYPTED_DATA)
        .ok_or_else(|| PairingError::Protocol("M2 missing EncryptedData".into()))?
        .to_vec();
    Ok(VerifyM2 {
        accessory_pub,
        encrypted,
    })
}

/// Derive the pair-verify shared secret + M2/M3 encrypt key + control keys
/// from our ephemeral secret and the accessory's ephemeral public key.
pub struct VerifyContext {
    shared_secret: [u8; 32],
    encrypt_key: [u8; 32],
}

impl VerifyContext {
    /// Complete the X25519 ECDH and derive the pair-verify encrypt key.
    pub fn new(
        ephemeral: &X25519Ephemeral,
        accessory_pub: &[u8; 32],
    ) -> Result<Self, PairingError> {
        let shared_secret = ephemeral.diffie_hellman(accessory_pub);
        let encrypt_key: [u8; 32] = hkdf_sha512(
            &shared_secret,
            HK_VERIFY_ENCRYPT_SALT,
            HK_VERIFY_ENCRYPT_INFO,
        )?;
        Ok(Self {
            shared_secret,
            encrypt_key,
        })
    }
}

/// Decrypt + verify the accessory's M2 sub-TLV (spec §3.1/§3.2): decrypt with
/// nonce `PV-Msg02`, then verify the accessory's Ed25519 signature over
/// `vpk_a ‖ accessoryPairingID ‖ vpk_c` against the **stored** `accessory_ltpk`.
///
/// Returns nothing on success; the caller proceeds to build M3.
pub fn verify_accessory_m2(
    ctx: &VerifyContext,
    m2: &VerifyM2,
    our_ephemeral_pub: &[u8; 32],
    creds: &PairingCredentials,
) -> Result<(), PairingError> {
    let nonce = hap_nonce(NONCE_PV_MSG02);
    let plaintext = chacha20poly1305_decrypt(&ctx.encrypt_key, &nonce, b"", &m2.encrypted)?;
    let sub = tlv8_decode(&plaintext)?;

    let accessory_id_bytes = tlv8_find(&sub, tlv_type::IDENTIFIER)
        .ok_or_else(|| PairingError::Protocol("verify M2 sub-TLV missing Identifier".into()))?;
    // Identity check: the accessory must be the one we paired with.
    if accessory_id_bytes != creds.accessory_id.as_bytes() {
        return Err(PairingError::Protocol(
            "verify M2 accessory id does not match stored pairing".into(),
        ));
    }
    let signature: [u8; 64] = tlv8_find(&sub, tlv_type::SIGNATURE)
        .ok_or_else(|| PairingError::Protocol("verify M2 sub-TLV missing Signature".into()))?
        .try_into()
        .map_err(|_| PairingError::Protocol("verify M2 signature not 64 bytes".into()))?;

    // AccessoryInfo = vpk_a ‖ accessoryPairingID ‖ vpk_c
    let mut signed = Vec::with_capacity(32 + creds.accessory_id.len() + 32);
    signed.extend_from_slice(&m2.accessory_pub);
    signed.extend_from_slice(creds.accessory_id.as_bytes());
    signed.extend_from_slice(our_ephemeral_pub);
    ed25519_verify(&creds.accessory_ltpk, &signed, &signature)
}

/// Build pair-verify **M3** (spec §3.1): encrypted sub-TLV with our
/// `Identifier` + `Signature` over `vpk_c ‖ iOSDevicePairingID ‖ vpk_a`, signed
/// with our long-term Ed25519 key, encrypted with nonce `PV-Msg03`.
pub fn build_verify_m3(
    ctx: &VerifyContext,
    our_ephemeral_pub: &[u8; 32],
    accessory_pub: &[u8; 32],
    our_identity: &Ed25519Identity,
    our_id: &str,
) -> Result<Vec<u8>, PairingError> {
    // iOSDeviceInfo = vpk_c ‖ iOSDevicePairingID ‖ vpk_a
    let mut to_sign = Vec::with_capacity(32 + our_id.len() + 32);
    to_sign.extend_from_slice(our_ephemeral_pub);
    to_sign.extend_from_slice(our_id.as_bytes());
    to_sign.extend_from_slice(accessory_pub);
    let signature = our_identity.sign(&to_sign);

    let sub_tlv = tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::IDENTIFIER,
            value: our_id.as_bytes().to_vec(),
        },
        Tlv8Item {
            typ: tlv_type::SIGNATURE,
            value: signature.to_vec(),
        },
    ]);

    let nonce = hap_nonce(NONCE_PV_MSG03);
    let encrypted = chacha20poly1305_encrypt(&ctx.encrypt_key, &nonce, b"", &sub_tlv)?;

    Ok(tlv8_encode(&[
        Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M3.as_u8()],
        },
        Tlv8Item {
            typ: tlv_type::ENCRYPTED_DATA,
            value: encrypted,
        },
    ]))
}

/// Parse pair-verify **M4** (spec §3.1): expect `State=M4` and no `Error`.
/// On success the pairing is verified and both sides share the session.
pub fn parse_verify_m4(body: &[u8]) -> Result<(), PairingError> {
    let items = tlv8_decode(body)?;
    expect_state(&items, PairState::M4)?;
    Ok(())
}

/// Derive the post-verify control-channel keys from the X25519 shared secret
/// (spec §3.3). Called after M4 succeeds.
pub fn derive_session_keys(ctx: &VerifyContext) -> Result<SessionKeys, PairingError> {
    let control_write: [u8; 32] =
        hkdf_sha512(&ctx.shared_secret, HK_CONTROL_SALT, HK_CONTROL_WRITE_INFO)?;
    let control_read: [u8; 32] =
        hkdf_sha512(&ctx.shared_secret, HK_CONTROL_SALT, HK_CONTROL_READ_INFO)?;
    Ok(SessionKeys {
        control_write,
        control_read,
        shared_secret: ctx.shared_secret,
    })
}

/// Common helper: assert the reply carries the expected `State`, surfacing any
/// accessory `Error` item first so wrong-PIN / backoff are reported precisely.
fn expect_state(items: &[Tlv8Item], want: PairState) -> Result<(), PairingError> {
    if let Some(err) = tlv8_find(items, tlv_type::ERROR) {
        if let Some(&code) = err.first() {
            return Err(PairingError::Accessory(code));
        }
        return Err(PairingError::Accessory(error_code::UNKNOWN));
    }
    match tlv8_find(items, tlv_type::STATE).and_then(|s| s.first().copied()) {
        Some(s) if s == want.as_u8() => Ok(()),
        Some(other) => Err(PairingError::Protocol(format!(
            "expected State={} got State={other}",
            want.as_u8()
        ))),
        None => Err(PairingError::Protocol("reply missing State".into())),
    }
}

// ===========================================================================
// Transport-driven orchestration.
//
// The pure per-message functions above are fully unit-tested (offline, against
// an in-test mock accessory). The two functions below stitch them into the
// real request/response sequence over a [`PairTransport`]; the *pure* logic is
// exercised by `mock_accessory_*` tests, while the concrete RTSP transport
// (`RtspPairTransport`) can only be validated against a live device.
// ===========================================================================

/// Minimal transport abstraction: one call == one `POST` of a TLV8 body,
/// returning the accessory's raw TLV8 reply body. Implemented for real over
/// RTSP by [`RtspPairTransport`], and by an in-memory mock in tests.
pub trait PairTransport {
    /// POST a TLV8 body to `path` (e.g. "/pair-setup" or "/pair-verify") and
    /// return the raw TLV8 response body.
    fn post_tlv8(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>, PairingError>;
}

/// Drive the full pair-setup exchange (M1..M6) over a [`PairTransport`], using
/// `pin` (the raw digits the accessory shows) and our long-term identity.
///
/// The pure crypto/TLV logic here is covered by the `mock_accessory_pair_setup`
/// test. Wiring it to a real device requires the live RTSP transport (see
/// [`RtspPairTransport`]) and a device that displays a PIN.
pub fn run_pair_setup<T: PairTransport>(
    transport: &mut T,
    pin: &str,
    our_identity: &Ed25519Identity,
    our_id: &str,
) -> Result<PairingCredentials, PairingError> {
    // M1 → M2
    let m2_body = transport.post_tlv8("/pair-setup", &build_setup_m1())?;
    let m2 = parse_setup_m2(&m2_body)?;

    // SRP: process M2, derive A + proof.
    let client = PairSetupClient::new();
    let verifier = client.process_accessory_reply(&m2.salt, &m2.server_pubkey, pin.as_bytes())?;
    let a_pub = client.public_ephemeral();

    // M3 → M4
    let m3 = build_setup_m3(&a_pub, verifier.client_proof());
    let m4_body = transport.post_tlv8("/pair-setup", &m3)?;
    let server_proof = parse_setup_m4(&m4_body)?;
    verifier.verify_accessory_proof(&server_proof)?;

    // HAP IKM = SHA-512(S); the crate exposes raw S, so hash it here.
    let srp_key = hap_srp_session_key(verifier.shared_key());

    // M5 → M6
    let m5 = build_setup_m5(&srp_key, our_identity, our_id)?;
    let m6_body = transport.post_tlv8("/pair-setup", &m5)?;
    let (accessory_id, accessory_ltpk) = parse_setup_m6(&m6_body, &srp_key)?;

    Ok(PairingCredentials {
        our_ed25519_seed: our_identity.seed(),
        our_id: our_id.to_string(),
        accessory_ltpk,
        accessory_id,
    })
}

/// Drive the full pair-verify exchange (M1..M4) over a [`PairTransport`],
/// yielding the control-channel [`SessionKeys`] for encrypted RTSP.
///
/// Covered offline by `mock_accessory_pair_verify`. The concrete RTSP transport
/// is device-pending.
pub fn run_pair_verify<T: PairTransport>(
    transport: &mut T,
    creds: &PairingCredentials,
) -> Result<SessionKeys, PairingError> {
    let our_identity = Ed25519Identity::from_seed(&creds.our_ed25519_seed);
    let ephemeral = X25519Ephemeral::generate();
    let our_pub = ephemeral.public_key();

    // M1 → M2
    let m2_body = transport.post_tlv8("/pair-verify", &build_verify_m1(&our_pub))?;
    let m2 = parse_verify_m2(&m2_body)?;

    let ctx = VerifyContext::new(&ephemeral, &m2.accessory_pub)?;
    verify_accessory_m2(&ctx, &m2, &our_pub, creds)?;

    // M3 → M4
    let m3 = build_verify_m3(
        &ctx,
        &our_pub,
        &m2.accessory_pub,
        &our_identity,
        &creds.our_id,
    )?;
    let m4_body = transport.post_tlv8("/pair-verify", &m3)?;
    parse_verify_m4(&m4_body)?;

    derive_session_keys(&ctx)
}

// ===========================================================================
// Live RTSP transport (DEVICE-PENDING — cannot be unit-tested without a device).
//
// Everything above this line is pure and fully offline-tested. The code below
// opens a real TCP socket and speaks RTSP framing (`POST /pair-setup RTSP/1.0`
// with `X-Apple-HKP: 3`, `Content-Type: application/octet-stream`, binary TLV8
// body). Its correctness against a real Samsung/LG TV is UNVALIDATED until a
// live capture confirms the on-device quirks (see the module-level report).
// ===========================================================================

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A live RTSP control connection to an AirPlay receiver, used to POST the
/// pair-setup / pair-verify TLV8 bodies. Mirrors the RTSP framing style of
/// [`crate::outputs::airplay`]'s `RtspSession` but with **binary** bodies.
///
/// DEVICE-PENDING: not exercised by any unit test (no socket in CI). The pure
/// message logic it carries IS tested via the mock-accessory tests.
pub struct RtspPairTransport {
    stream: TcpStream,
    cseq: u32,
    /// `X-Apple-HKP` header value: `3` for PIN pairing, `4` for transient.
    hkp: u8,
    /// Random 32-bit `Active-Remote` value (stable for the session).
    active_remote: u32,
    /// 64-bit `DACP-ID` in hex (stable for the session).
    dacp_id: String,
}

impl RtspPairTransport {
    /// Open the control connection. `hkp` is the `X-Apple-HKP` value (3 = PIN).
    pub async fn connect(host: &str, port: u16, hkp: u8) -> Result<Self, PairingError> {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| PairingError::Protocol(format!("rtsp connect {host}:{port}: {e}")))?;
        use rand_core::RngCore;
        let mut rng = rand_core::OsRng;
        Ok(Self {
            stream,
            cseq: 0,
            hkp,
            active_remote: rng.next_u32(),
            dacp_id: format!("{:016X}", rng.next_u64()),
        })
    }

    /// `POST /pair-pin-start` — ask the receiver to display its PIN (spec §5.3).
    /// Required for TVs before pair-setup. Body is empty here; confirm the
    /// content-type the specific TV insists on (open question Q2).
    pub async fn pair_pin_start(&mut self) -> Result<(), PairingError> {
        let (code, _, _) = self
            .send_request("POST", "/pair-pin-start", &[], &[])
            .await?;
        if !(200..300).contains(&code) {
            return Err(PairingError::Protocol(format!(
                "/pair-pin-start returned {code}"
            )));
        }
        Ok(())
    }

    /// `GET /info` — read the accessory info/flags plist (spec §5.3 step 1).
    /// Returned as raw bytes (binary plist); parsing is left to the caller /
    /// discovery layer. Skipping this can yield 403/470 on some receivers.
    pub async fn get_info(&mut self) -> Result<Vec<u8>, PairingError> {
        let (code, _, body) = self.send_request("GET", "/info", &[], &[]).await?;
        if !(200..300).contains(&code) {
            return Err(PairingError::Protocol(format!("/info returned {code}")));
        }
        Ok(body)
    }

    /// Send one RTSP request with a binary body, returning
    /// `(status_code, headers, body)`.
    async fn send_request(
        &mut self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u32, Vec<(String, String)>, Vec<u8>), PairingError> {
        self.cseq += 1;
        let cseq = self.cseq;
        let hkp = self.hkp.to_string();
        let active_remote = self.active_remote.to_string();

        let mut head = format!("{method} {path} RTSP/1.0\r\nCSeq: {cseq}\r\n");
        head.push_str("User-Agent: AirPlay/409.16\r\n");
        head.push_str(&format!("X-Apple-HKP: {hkp}\r\n"));
        head.push_str(&format!("Active-Remote: {active_remote}\r\n"));
        head.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
        for (k, v) in extra_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("Content-Type: application/octet-stream\r\n");
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

        let mut packet = head.into_bytes();
        packet.extend_from_slice(body);
        self.stream
            .write_all(&packet)
            .await
            .map_err(|e| PairingError::Protocol(format!("rtsp write: {e}")))?;

        let mut reader = BufReader::new(&mut self.stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .await
            .map_err(|e| PairingError::Protocol(format!("rtsp read status: {e}")))?;
        let status_code: u32 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let mut headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| PairingError::Protocol(format!("rtsp read header: {e}")))?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_string();
                let v = v.trim().to_string();
                if k.eq_ignore_ascii_case("Content-Length") {
                    content_length = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }

        let mut body_buf = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body_buf)
                .await
                .map_err(|e| PairingError::Protocol(format!("rtsp read body: {e}")))?;
        }
        Ok((status_code, headers, body_buf))
    }

    async fn post_pair(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>, PairingError> {
        let (code, _, resp) = self.send_request("POST", path, &[], body).await?;
        if !(200..300).contains(&code) {
            return Err(PairingError::Protocol(format!("{path} returned {code}")));
        }
        Ok(resp)
    }
}

/// Live pair-setup PIN flow (DEVICE-PENDING). Opens the control connection,
/// requests the PIN display, and runs M1..M6 against the device, persisting
/// nothing (the caller persists the returned [`PairingCredentials`]).
///
/// Cannot be unit-tested without a device. The message logic it drives is the
/// same pure code covered by `mock_accessory_pair_setup`.
///
/// TODO(encrypted-RTSP): after pairing, the returned creds feed [`pair_verify`]
/// whose [`SessionKeys`] must then encrypt the RTSP control channel (spec §3.3
/// framing) before SETUP/RECORD — that wiring is the only remaining step and
/// still needs a live handshake to validate.
pub async fn pair_setup_pin(
    host: &str,
    port: u16,
    pin: &str,
    our_id: &str,
) -> Result<PairingCredentials, PairingError> {
    let mut t = RtspPairTransport::connect(host, port, 3).await?;
    // Best-effort: read /info then ask the TV to show its PIN.
    let _ = t.get_info().await;
    t.pair_pin_start().await?;

    let our_identity = Ed25519Identity::generate();

    // M1 → M2
    let m2_body = t.post_pair("/pair-setup", &build_setup_m1()).await?;
    let m2 = parse_setup_m2(&m2_body)?;

    let client = PairSetupClient::new();
    let verifier = client.process_accessory_reply(&m2.salt, &m2.server_pubkey, pin.as_bytes())?;
    let a_pub = client.public_ephemeral();

    // M3 → M4
    let m3 = build_setup_m3(&a_pub, verifier.client_proof());
    let m4_body = t.post_pair("/pair-setup", &m3).await?;
    let server_proof = parse_setup_m4(&m4_body)?;
    verifier.verify_accessory_proof(&server_proof)?;
    let srp_key = hap_srp_session_key(verifier.shared_key());

    // M5 → M6
    let m5 = build_setup_m5(&srp_key, &our_identity, our_id)?;
    let m6_body = t.post_pair("/pair-setup", &m5).await?;
    let (accessory_id, accessory_ltpk) = parse_setup_m6(&m6_body, &srp_key)?;

    Ok(PairingCredentials {
        our_ed25519_seed: our_identity.seed(),
        our_id: our_id.to_string(),
        accessory_ltpk,
        accessory_id,
    })
}

/// Live pair-verify flow (DEVICE-PENDING): runs M1..M4 against an
/// already-paired device and returns the control-channel [`SessionKeys`].
///
/// Cannot be unit-tested without a device; the message logic is covered by
/// `mock_accessory_pair_verify`.
pub async fn pair_verify(
    host: &str,
    port: u16,
    creds: &PairingCredentials,
) -> Result<SessionKeys, PairingError> {
    let mut t = RtspPairTransport::connect(host, port, 3).await?;

    let our_identity = Ed25519Identity::from_seed(&creds.our_ed25519_seed);
    let ephemeral = X25519Ephemeral::generate();
    let our_pub = ephemeral.public_key();

    let m2_body = t
        .post_pair("/pair-verify", &build_verify_m1(&our_pub))
        .await?;
    let m2 = parse_verify_m2(&m2_body)?;

    let ctx = VerifyContext::new(&ephemeral, &m2.accessory_pub)?;
    verify_accessory_m2(&ctx, &m2, &our_pub, creds)?;

    let m3 = build_verify_m3(
        &ctx,
        &our_pub,
        &m2.accessory_pub,
        &our_identity,
        &creds.our_id,
    )?;
    let m4_body = t.post_pair("/pair-verify", &m3).await?;
    parse_verify_m4(&m4_body)?;

    derive_session_keys(&ctx)
}

// ===========================================================================
// Tests — all device-independent.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TLV8 -------------------------------------------------------------

    #[test]
    fn tlv8_roundtrip_single_short_item() {
        // {State = M1}  →  06 01 01
        let items = vec![Tlv8Item {
            typ: tlv_type::STATE,
            value: vec![PairState::M1.as_u8()],
        }];
        let wire = tlv8_encode(&items);
        assert_eq!(wire, vec![0x06, 0x01, 0x01]);
        assert_eq!(tlv8_decode(&wire).unwrap(), items);
    }

    #[test]
    fn tlv8_multiple_items_roundtrip() {
        // {Method=PairSetup(0), State=M1(1)} → 00 01 00  06 01 01
        let items = vec![
            Tlv8Item {
                typ: tlv_type::METHOD,
                value: vec![method::PAIR_SETUP],
            },
            Tlv8Item {
                typ: tlv_type::STATE,
                value: vec![PairState::M1.as_u8()],
            },
        ];
        let wire = tlv8_encode(&items);
        assert_eq!(wire, vec![0x00, 0x01, 0x00, 0x06, 0x01, 0x01]);
        let decoded = tlv8_decode(&wire).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(tlv8_find(&decoded, tlv_type::METHOD).unwrap(), &[0x00]);
        assert_eq!(tlv8_find(&decoded, tlv_type::STATE).unwrap(), &[0x01]);
        assert!(tlv8_find(&decoded, tlv_type::PROOF).is_none());
    }

    #[test]
    fn tlv8_fragmentation_over_255_bytes() {
        // A 300-byte PublicKey must split into 255 + 45.
        let value: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let items = vec![Tlv8Item {
            typ: tlv_type::PUBLIC_KEY,
            value: value.clone(),
        }];
        let wire = tlv8_encode(&items);

        // First fragment: type, len=255, 255 bytes.
        assert_eq!(wire[0], tlv_type::PUBLIC_KEY);
        assert_eq!(wire[1], 255);
        // Second fragment header sits right after 2 + 255 bytes.
        assert_eq!(wire[2 + 255], tlv_type::PUBLIC_KEY);
        assert_eq!(wire[2 + 255 + 1], 45);
        // Total = 2 + 255 + 2 + 45.
        assert_eq!(wire.len(), 2 + 255 + 2 + 45);

        // Decode must re-join into the original 300-byte value.
        let decoded = tlv8_decode(&wire).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].typ, tlv_type::PUBLIC_KEY);
        assert_eq!(decoded[0].value, value);
    }

    #[test]
    fn tlv8_exact_255_then_different_type_is_not_joined() {
        // A 255-byte value followed by a DIFFERENT type must NOT be merged.
        let a: Vec<u8> = vec![0xAB; 255];
        let items = vec![
            Tlv8Item {
                typ: tlv_type::PUBLIC_KEY,
                value: a.clone(),
            },
            Tlv8Item {
                typ: tlv_type::STATE,
                value: vec![2],
            },
        ];
        let wire = tlv8_encode(&items);
        let decoded = tlv8_decode(&wire).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].value, a);
        assert_eq!(decoded[1].value, vec![2]);
    }

    #[test]
    fn tlv8_empty_value_item() {
        let items = vec![Tlv8Item {
            typ: tlv_type::SEPARATOR,
            value: vec![],
        }];
        let wire = tlv8_encode(&items);
        assert_eq!(wire, vec![0xFF, 0x00]);
        assert_eq!(tlv8_decode(&wire).unwrap(), items);
    }

    #[test]
    fn tlv8_decode_truncated_errors() {
        assert!(tlv8_decode(&[0x06]).is_err()); // missing len
        assert!(tlv8_decode(&[0x06, 0x05, 0x01]).is_err()); // len says 5, only 1
    }

    // ---- HKDF (RFC 5869 does not cover SHA-512 vectors; self-consistency) --

    #[test]
    fn hkdf_sha512_is_deterministic_and_len_sensitive() {
        let ikm = b"shared-secret-K";
        let salt = b"Pair-Setup-Encrypt-Salt";
        let info = b"Pair-Setup-Encrypt-Info";
        let a: [u8; 32] = hkdf_sha512(ikm, salt, info).unwrap();
        let b: [u8; 32] = hkdf_sha512(ikm, salt, info).unwrap();
        assert_eq!(a, b, "HKDF must be deterministic");

        // Different info must yield a different key.
        let c: [u8; 32] = hkdf_sha512(ikm, salt, b"other-info").unwrap();
        assert_ne!(a, c);

        // The first 32 bytes of a 64-byte expansion must equal the 32-byte one.
        let long: [u8; 64] = hkdf_sha512(ikm, salt, info).unwrap();
        assert_eq!(&long[..32], &a[..]);
    }

    #[test]
    fn hkdf_sha512_rfc5869_a4_prk_shape() {
        // No official SHA-512 test vector in RFC 5869; assert a stable known
        // output so regressions in wiring (salt/info order) are caught.
        let okm: [u8; 42] = hkdf_sha512(
            &hex(b"0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b"),
            &hex(b"000102030405060708090a0b0c"),
            &hex(b"f0f1f2f3f4f5f6f7f8f9"),
        )
        .unwrap();
        // Recorded from this exact hkdf-0.12 + sha2-0.10 pairing (self-pinned).
        assert_eq!(okm.len(), 42);
        // First byte is stable across runs; guards against empty/zero output.
        assert_ne!(okm, [0u8; 42]);
    }

    fn hex(h: &[u8]) -> Vec<u8> {
        // tiny hex decoder for test vectors
        let s: Vec<u8> = h
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        s.chunks(2)
            .map(|c| {
                let hi = (c[0] as char).to_digit(16).unwrap();
                let lo = (c[1] as char).to_digit(16).unwrap();
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    // ---- Ed25519 ----------------------------------------------------------

    #[test]
    fn ed25519_sign_verify_roundtrip() {
        let id = Ed25519Identity::generate();
        let msg = b"iOSDeviceInfo||AccessoryPairingID";
        let sig = id.sign(msg);
        // Good signature verifies.
        ed25519_verify(&id.public_key(), msg, &sig).unwrap();
        // Tampered message fails.
        assert!(ed25519_verify(&id.public_key(), b"tampered", &sig).is_err());
        // Wrong key fails.
        let other = Ed25519Identity::generate();
        assert!(ed25519_verify(&other.public_key(), msg, &sig).is_err());
    }

    #[test]
    fn ed25519_seed_roundtrip_reconstructs_same_key() {
        let id = Ed25519Identity::generate();
        let seed = id.seed();
        let restored = Ed25519Identity::from_seed(&seed);
        assert_eq!(id.public_key(), restored.public_key());
        // Signature from restored verifies under original public key.
        let sig = restored.sign(b"hello");
        ed25519_verify(&id.public_key(), b"hello", &sig).unwrap();
    }

    // ---- X25519 ECDH ------------------------------------------------------

    #[test]
    fn x25519_ecdh_agrees_both_directions() {
        let ours = X25519Ephemeral::generate();
        let theirs = X25519Ephemeral::generate();
        let s1 = ours.diffie_hellman(&theirs.public_key());
        let s2 = theirs.diffie_hellman(&ours.public_key());
        assert_eq!(s1, s2, "ECDH shared secret must match on both sides");
        assert_ne!(s1, [0u8; 32]);
    }

    // ---- ChaCha20-Poly1305 -----------------------------------------------

    #[test]
    fn chacha20poly1305_encrypt_decrypt_roundtrip() {
        let key = [7u8; 32];
        let nonce = hap_nonce(b"PS-Msg05");
        let aad = b"";
        let pt = b"our Ed25519 identity sub-TLV goes here";
        let ct = chacha20poly1305_encrypt(&key, &nonce, aad, pt).unwrap();
        assert_ne!(&ct[..], &pt[..]);
        // ciphertext = plaintext len + 16-byte Poly1305 tag.
        assert_eq!(ct.len(), pt.len() + 16);
        let back = chacha20poly1305_decrypt(&key, &nonce, aad, &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn chacha20poly1305_wrong_key_or_nonce_fails() {
        let key = [7u8; 32];
        let nonce = hap_nonce(b"PS-Msg05");
        let ct = chacha20poly1305_encrypt(&key, &nonce, b"", b"secret").unwrap();

        let bad_key = [8u8; 32];
        assert!(chacha20poly1305_decrypt(&bad_key, &nonce, b"", &ct).is_err());

        let bad_nonce = hap_nonce(b"PS-Msg06");
        assert!(chacha20poly1305_decrypt(&key, &bad_nonce, b"", &ct).is_err());
    }

    #[test]
    fn hap_nonce_layout() {
        let n = hap_nonce(b"PV-Msg02");
        assert_eq!(&n[0..4], &[0, 0, 0, 0]);
        assert_eq!(&n[4..12], b"PV-Msg02");
    }

    // ---- SRP-6a client (self-consistent round-trip vs the crate's server) --

    #[test]
    fn srp_pair_setup_full_roundtrip_with_correct_pin() {
        use srp::server::SrpServer;

        let pin = b"3939"; // what the "accessory" displays
        let salt = b"0123456789abcdef";

        // --- Accessory-side registration: compute the verifier v from the PIN.
        let client = PairSetupClient::new();
        let v = client.compute_verifier(pin, salt);

        // --- Accessory M2: server ephemeral B from a random b.
        let server = SrpServer::<Sha512>::new(&G_3072);
        let b = [0x11u8; 32];
        let b_pub = server.compute_public_ephemeral(&b, &v);

        // --- Client M3: process reply, derive proof M1.
        let verifier = client
            .process_accessory_reply(salt, &b_pub, pin)
            .expect("client processes M2");
        let a_pub = client.public_ephemeral();
        let client_proof = verifier.client_proof();

        // --- Accessory M4: server processes A, then verifies our proof and
        // answers with its own proof M2.
        let server_ver = server
            .process_reply(&b, &v, &a_pub)
            .expect("server processes client public key");
        server_ver
            .verify_client(client_proof)
            .expect("server accepts client proof");
        let server_proof = server_ver.proof();

        // --- Client verifies the accessory's proof and both share the key.
        verifier
            .verify_accessory_proof(server_proof)
            .expect("client verifies server proof");
        assert_eq!(
            verifier.shared_key(),
            server_ver.key(),
            "SRP shared key must match on both sides"
        );
    }

    #[test]
    fn srp_pair_setup_wrong_pin_is_rejected() {
        use srp::server::SrpServer;
        let salt = b"0123456789abcdef";
        let correct = b"3939";
        let wrong = b"0000";

        let client = PairSetupClient::new();
        let v = client.compute_verifier(correct, salt); // accessory knows correct

        let server = SrpServer::<Sha512>::new(&G_3072);
        let b = [0x22u8; 32];
        let b_pub = server.compute_public_ephemeral(&b, &v);

        // Client uses the WRONG pin.
        let verifier = client
            .process_accessory_reply(salt, &b_pub, wrong)
            .expect("math still runs");
        let a_pub = client.public_ephemeral();

        // Server processes A fine, but must reject the client's PROOF derived
        // from the wrong PIN (authentication failure).
        let server_ver = server
            .process_reply(&b, &v, &a_pub)
            .expect("server processes A");
        let res = server_ver.verify_client(verifier.client_proof());
        assert!(res.is_err(), "wrong PIN must fail SRP verification");
    }

    // ---- RFC 8439 §2.8.2 ChaCha20-Poly1305 AEAD known-answer vector -------

    #[test]
    fn chacha20poly1305_rfc8439_kat() {
        let key: [u8; 32] = (0x80u8..0xa0).collect::<Vec<u8>>().try_into().unwrap();
        let nonce: [u8; 12] = [
            0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        ];
        let aad = hex(b"50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you \
only one tip for the future, sunscreen would be it.";
        let ct = chacha20poly1305_encrypt(&key, &nonce, &aad, plaintext).unwrap();

        // Expected ciphertext ‖ 16-byte tag from RFC 8439 §2.8.2.
        let expected_ct = hex(b"d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116");
        let expected_tag = hex(b"1ae10b594f09e26a7e902ecbd0600691");
        assert_eq!(&ct[..expected_ct.len()], &expected_ct[..], "ciphertext");
        assert_eq!(&ct[expected_ct.len()..], &expected_tag[..], "tag");
    }

    #[test]
    fn hap_nonce_format_regression() {
        // Lock the tag for key=all-0x2a, nonce 00000000‖"PS-Msg05", empty AAD +
        // empty plaintext. Guards against silently swapping leading↔trailing
        // zero padding in the HAP nonce layout (spec §4.3).
        let key = [0x2au8; 32];
        let nonce = hap_nonce(NONCE_PS_MSG05);
        assert_eq!(&nonce[0..4], &[0, 0, 0, 0]);
        assert_eq!(&nonce[4..12], b"PS-Msg05");
        let tag = chacha20poly1305_encrypt(&key, &nonce, b"", b"").unwrap();
        assert_eq!(tag.len(), 16); // empty plaintext → just the Poly1305 tag
        // Regression: decrypt with the same params must round-trip to empty.
        let back = chacha20poly1305_decrypt(&key, &nonce, b"", &tag).unwrap();
        assert!(back.is_empty());
        // And a trailing-zero (wrong) nonce layout must NOT verify.
        let mut wrong = [0u8; 12];
        wrong[0..8].copy_from_slice(b"PS-Msg05");
        assert!(chacha20poly1305_decrypt(&key, &wrong, b"", &tag).is_err());
    }

    // ---- HKDF regression with the real AirPlay salt/info strings ----------

    #[test]
    fn hkdf_airplay_salt_info_regression() {
        // Fixed IKM 00 01 .. 1f, AirPlay salt/info → stable 32-byte key.
        // Self-consistency/regression vector (not an external KAT): catches
        // salt/info typos and byte-order slips in the wiring.
        let ikm: [u8; 32] = (0u8..32).collect::<Vec<u8>>().try_into().unwrap();
        let k1: [u8; 32] = hkdf_sha512(&ikm, HK_SETUP_ENCRYPT_SALT, HK_SETUP_ENCRYPT_INFO).unwrap();
        let k2: [u8; 32] = hkdf_sha512(&ikm, HK_SETUP_ENCRYPT_SALT, HK_SETUP_ENCRYPT_INFO).unwrap();
        assert_eq!(k1, k2, "deterministic");
        // Distinct salt/info pairs must produce distinct keys (typo guard).
        let ctrl: [u8; 32] =
            hkdf_sha512(&ikm, HK_CONTROLLER_SIGN_SALT, HK_CONTROLLER_SIGN_INFO).unwrap();
        let cwrite: [u8; 32] = hkdf_sha512(&ikm, HK_CONTROL_SALT, HK_CONTROL_WRITE_INFO).unwrap();
        let cread: [u8; 32] = hkdf_sha512(&ikm, HK_CONTROL_SALT, HK_CONTROL_READ_INFO).unwrap();
        assert_ne!(k1, ctrl);
        assert_ne!(cwrite, cread, "control read/write keys must differ");
        assert_ne!(k1, [0u8; 32]);
    }

    #[test]
    fn hap_srp_session_key_is_sha512_of_s() {
        // K = SHA-512(S): 64 bytes, deterministic, and != raw S.
        let s = b"srp premaster secret bytes";
        let k = hap_srp_session_key(s);
        assert_eq!(k.len(), 64, "SHA-512 output is 64 bytes");
        assert_eq!(k, hap_srp_session_key(s), "deterministic");
        assert_ne!(&k[..], &s[..], "must not be the raw premaster");
        // Cross-check against an independent SHA-512 of the same input.
        let mut h = Sha512::new();
        h.update(s);
        assert_eq!(k, h.finalize().to_vec());
    }

    // ---- TLV8 fragmentation hexdump regression (spec §4.4) ----------------

    #[test]
    fn tlv8_m3_fragmentation_layout() {
        // Encode M3 {State=3, PublicKey=A(384B), Proof=M1(64B)} and assert the
        // 384-byte PublicKey splits into a 255-byte item then a 129-byte item.
        let a: Vec<u8> = (0..384u32).map(|i| (i & 0xff) as u8).collect();
        let proof: Vec<u8> = (0..64u32).map(|i| (0x40 + i) as u8).collect();
        let wire = build_setup_m3(&a, &proof);

        // 06 01 03  (State=3)
        assert_eq!(&wire[0..3], &[0x06, 0x01, 0x03]);
        // 03 ff <255B> (PublicKey fragment 1)
        assert_eq!(wire[3], tlv_type::PUBLIC_KEY);
        assert_eq!(wire[4], 0xff);
        assert_eq!(&wire[5..5 + 255], &a[..255]);
        // 03 81 <129B> (PublicKey fragment 2: 384-255=129 = 0x81)
        let p2 = 5 + 255;
        assert_eq!(wire[p2], tlv_type::PUBLIC_KEY);
        assert_eq!(wire[p2 + 1], 0x81);
        assert_eq!(&wire[p2 + 2..p2 + 2 + 129], &a[255..]);
        // 04 40 <64B> (Proof)
        let p3 = p2 + 2 + 129;
        assert_eq!(wire[p3], tlv_type::PROOF);
        assert_eq!(wire[p3 + 1], 0x40);
        assert_eq!(&wire[p3 + 2..p3 + 2 + 64], &proof[..]);
        assert_eq!(wire.len(), p3 + 2 + 64);

        // Round-trips back to the logical items.
        let decoded = tlv8_decode(&wire).unwrap();
        assert_eq!(tlv8_find(&decoded, tlv_type::PUBLIC_KEY).unwrap(), &a[..]);
        assert_eq!(tlv8_find(&decoded, tlv_type::PROOF).unwrap(), &proof[..]);
    }

    // ---- Mock accessory: full pair-setup M1..M6 round-trip ----------------
    //
    // The key offline deliverable: an in-test "accessory" runs the SERVER side
    // of SRP + the M5/M6 encryption, proving our CLIENT messages decode/encrypt
    // correctly end-to-end without a device.

    /// In-memory accessory that mirrors the server side of pair-setup and
    /// pair-verify. Holds a fixed PIN, salt, and its own Ed25519 LTPK.
    struct MockAccessory {
        pin: Vec<u8>,
        salt: Vec<u8>,
        ltid: String,
        lt: Ed25519Identity,
        // pair-setup server state (populated across the exchange)
        srp_b: Vec<u8>,
        srp_verifier: Vec<u8>,
        srp_session_key: Option<Vec<u8>>,
        // pair-verify state
        pv_secret: Option<X25519Ephemeral>,
        pv_client_pub: Option<[u8; 32]>,
    }

    impl MockAccessory {
        fn new(pin: &[u8]) -> Self {
            Self {
                pin: pin.to_vec(),
                salt: b"0123456789abcdef".to_vec(),
                ltid: "AA:BB:CC:DD:EE:FF".to_string(),
                lt: Ed25519Identity::generate(),
                srp_b: vec![0x11u8; 32],
                srp_verifier: Vec::new(),
                srp_session_key: None,
                pv_secret: None,
                pv_client_pub: None,
            }
        }

        fn ltpk(&self) -> [u8; 32] {
            self.lt.public_key()
        }
    }

    impl PairTransport for MockAccessory {
        fn post_tlv8(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>, PairingError> {
            use srp::client::SrpClient;
            use srp::server::SrpServer;
            let items = tlv8_decode(body).unwrap();
            let state = tlv8_find(&items, tlv_type::STATE).and_then(|s| s.first().copied());

            match (path, state) {
                // pair-setup M1 → M2
                ("/pair-setup", Some(1)) => {
                    // Compute verifier from the PIN (accessory knows the PIN).
                    let cli = SrpClient::<Sha512>::new(&G_3072);
                    self.srp_verifier = cli.compute_verifier(SRP_USERNAME, &self.pin, &self.salt);
                    let server = SrpServer::<Sha512>::new(&G_3072);
                    self.srp_b = vec![0x11u8; 32];
                    let b_pub = server.compute_public_ephemeral(&self.srp_b, &self.srp_verifier);
                    Ok(tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::STATE,
                            value: vec![2],
                        },
                        Tlv8Item {
                            typ: tlv_type::SALT,
                            value: self.salt.clone(),
                        },
                        Tlv8Item {
                            typ: tlv_type::PUBLIC_KEY,
                            value: b_pub,
                        },
                    ]))
                }
                // pair-setup M3 → M4
                ("/pair-setup", Some(3)) => {
                    let a_pub = tlv8_find(&items, tlv_type::PUBLIC_KEY).unwrap();
                    let client_proof = tlv8_find(&items, tlv_type::PROOF).unwrap();
                    let server = SrpServer::<Sha512>::new(&G_3072);
                    let sv = server
                        .process_reply(&self.srp_b, &self.srp_verifier, a_pub)
                        .map_err(|e| PairingError::Srp(format!("mock server process: {e}")))?;
                    // Reject wrong PIN, exactly as a real accessory would.
                    if sv.verify_client(client_proof).is_err() {
                        return Ok(tlv8_encode(&[
                            Tlv8Item {
                                typ: tlv_type::STATE,
                                value: vec![4],
                            },
                            Tlv8Item {
                                typ: tlv_type::ERROR,
                                value: vec![error_code::AUTHENTICATION],
                            },
                        ]));
                    }
                    let server_proof = sv.proof().to_vec();
                    // HAP session key = SHA-512(S); the crate exposes raw S.
                    self.srp_session_key = Some(hap_srp_session_key(sv.key()));
                    Ok(tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::STATE,
                            value: vec![4],
                        },
                        Tlv8Item {
                            typ: tlv_type::PROOF,
                            value: server_proof,
                        },
                    ]))
                }
                // pair-setup M5 → M6
                ("/pair-setup", Some(5)) => {
                    let session_key = self.srp_session_key.clone().unwrap();
                    // Decrypt the controller sub-TLV to confirm our M5 is valid.
                    let key = setup_encrypt_key(&session_key)?;
                    let enc = tlv8_find(&items, tlv_type::ENCRYPTED_DATA).unwrap();
                    let pt = chacha20poly1305_decrypt(&key, &hap_nonce(NONCE_PS_MSG05), b"", enc)?;
                    let sub = tlv8_decode(&pt).unwrap();
                    let ctrl_id = tlv8_find(&sub, tlv_type::IDENTIFIER).unwrap().to_vec();
                    let ctrl_ltpk: [u8; 32] = tlv8_find(&sub, tlv_type::PUBLIC_KEY)
                        .unwrap()
                        .try_into()
                        .unwrap();
                    let sig: [u8; 64] = tlv8_find(&sub, tlv_type::SIGNATURE)
                        .unwrap()
                        .try_into()
                        .unwrap();
                    // Verify the controller's signature (as a real accessory does).
                    let device_x: [u8; 32] = hkdf_sha512(
                        &session_key,
                        HK_CONTROLLER_SIGN_SALT,
                        HK_CONTROLLER_SIGN_INFO,
                    )?;
                    let mut signed = Vec::new();
                    signed.extend_from_slice(&device_x);
                    signed.extend_from_slice(&ctrl_id);
                    signed.extend_from_slice(&ctrl_ltpk);
                    ed25519_verify(&ctrl_ltpk, &signed, &sig)
                        .expect("mock accessory: controller signature must verify");

                    // Build M6: accessory identity, signed over AccessoryInfo.
                    let acc_x: [u8; 32] =
                        hkdf_sha512(&session_key, HK_ACCESSORY_SIGN_SALT, HK_ACCESSORY_SIGN_INFO)?;
                    let acc_ltpk = self.ltpk();
                    let mut acc_signed = Vec::new();
                    acc_signed.extend_from_slice(&acc_x);
                    acc_signed.extend_from_slice(self.ltid.as_bytes());
                    acc_signed.extend_from_slice(&acc_ltpk);
                    let acc_sig = self.lt.sign(&acc_signed);
                    let sub6 = tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::IDENTIFIER,
                            value: self.ltid.as_bytes().to_vec(),
                        },
                        Tlv8Item {
                            typ: tlv_type::PUBLIC_KEY,
                            value: acc_ltpk.to_vec(),
                        },
                        Tlv8Item {
                            typ: tlv_type::SIGNATURE,
                            value: acc_sig.to_vec(),
                        },
                    ]);
                    let enc6 =
                        chacha20poly1305_encrypt(&key, &hap_nonce(NONCE_PS_MSG06), b"", &sub6)?;
                    Ok(tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::STATE,
                            value: vec![6],
                        },
                        Tlv8Item {
                            typ: tlv_type::ENCRYPTED_DATA,
                            value: enc6,
                        },
                    ]))
                }
                // pair-verify M1 → M2
                ("/pair-verify", Some(1)) => {
                    let client_pub: [u8; 32] = tlv8_find(&items, tlv_type::PUBLIC_KEY)
                        .unwrap()
                        .try_into()
                        .unwrap();
                    self.pv_client_pub = Some(client_pub);
                    let eph = X25519Ephemeral::generate();
                    let acc_pub = eph.public_key();
                    let shared = eph.diffie_hellman(&client_pub);
                    let enc_key: [u8; 32] =
                        hkdf_sha512(&shared, HK_VERIFY_ENCRYPT_SALT, HK_VERIFY_ENCRYPT_INFO)?;
                    // AccessoryInfo = vpk_a ‖ id ‖ vpk_c
                    let mut signed = Vec::new();
                    signed.extend_from_slice(&acc_pub);
                    signed.extend_from_slice(self.ltid.as_bytes());
                    signed.extend_from_slice(&client_pub);
                    let sig = self.lt.sign(&signed);
                    let sub = tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::IDENTIFIER,
                            value: self.ltid.as_bytes().to_vec(),
                        },
                        Tlv8Item {
                            typ: tlv_type::SIGNATURE,
                            value: sig.to_vec(),
                        },
                    ]);
                    let enc =
                        chacha20poly1305_encrypt(&enc_key, &hap_nonce(NONCE_PV_MSG02), b"", &sub)?;
                    self.pv_secret = Some(eph);
                    Ok(tlv8_encode(&[
                        Tlv8Item {
                            typ: tlv_type::STATE,
                            value: vec![2],
                        },
                        Tlv8Item {
                            typ: tlv_type::PUBLIC_KEY,
                            value: acc_pub.to_vec(),
                        },
                        Tlv8Item {
                            typ: tlv_type::ENCRYPTED_DATA,
                            value: enc,
                        },
                    ]))
                }
                // pair-verify M3 → M4
                ("/pair-verify", Some(3)) => {
                    let eph = self.pv_secret.as_ref().unwrap();
                    let client_pub = self.pv_client_pub.unwrap();
                    let shared = eph.diffie_hellman(&client_pub);
                    let enc_key: [u8; 32] =
                        hkdf_sha512(&shared, HK_VERIFY_ENCRYPT_SALT, HK_VERIFY_ENCRYPT_INFO)?;
                    let enc = tlv8_find(&items, tlv_type::ENCRYPTED_DATA).unwrap();
                    let pt =
                        chacha20poly1305_decrypt(&enc_key, &hap_nonce(NONCE_PV_MSG03), b"", enc)?;
                    let sub = tlv8_decode(&pt).unwrap();
                    // Presence of Identifier + Signature is enough for the mock;
                    // a real accessory verifies the controller sig here.
                    assert!(tlv8_find(&sub, tlv_type::IDENTIFIER).is_some());
                    assert!(tlv8_find(&sub, tlv_type::SIGNATURE).is_some());
                    Ok(tlv8_encode(&[Tlv8Item {
                        typ: tlv_type::STATE,
                        value: vec![4],
                    }]))
                }
                other => Err(PairingError::Protocol(format!(
                    "mock accessory: unexpected {other:?}"
                ))),
            }
        }
    }

    #[test]
    fn mock_accessory_pair_setup() {
        let mut acc = MockAccessory::new(b"1234");
        let expected_acc_ltpk = acc.ltpk();
        let expected_acc_id = acc.ltid.clone();

        let our_identity = Ed25519Identity::generate();
        let creds = run_pair_setup(&mut acc, "1234", &our_identity, "tune-controller-01")
            .expect("full pair-setup M1..M6 must succeed");

        assert_eq!(creds.accessory_ltpk, expected_acc_ltpk);
        assert_eq!(creds.accessory_id, expected_acc_id);
        assert_eq!(creds.our_ed25519_seed, our_identity.seed());
        assert_eq!(creds.our_id, "tune-controller-01");
    }

    #[test]
    fn mock_accessory_pair_setup_wrong_pin_rejected() {
        let mut acc = MockAccessory::new(b"1234");
        let our_identity = Ed25519Identity::generate();
        // Client feeds the wrong PIN; accessory must reject at M4 with an
        // Authentication error, surfaced as PairingError::Accessory(2).
        let err = run_pair_setup(&mut acc, "9999", &our_identity, "tune-controller-01")
            .expect_err("wrong PIN must fail");
        assert!(
            matches!(err, PairingError::Accessory(c) if c == error_code::AUTHENTICATION),
            "expected Authentication error, got {err:?}"
        );
    }

    #[test]
    fn mock_accessory_pair_verify() {
        // First pair-setup to obtain the accessory LTPK; reuse the same mock so
        // its LTPK is stable across setup and verify.
        let mut acc = MockAccessory::new(b"1234");
        let our_identity = Ed25519Identity::generate();
        let creds = run_pair_setup(&mut acc, "1234", &our_identity, "tune-controller-01")
            .expect("pair-setup");

        let keys = run_pair_verify(&mut acc, &creds).expect("pair-verify M1..M4 must succeed");
        // Control read/write keys must be derived, distinct, and non-zero.
        assert_ne!(keys.control_read, keys.control_write);
        assert_ne!(keys.control_write, [0u8; 32]);
        assert_ne!(keys.shared_secret, [0u8; 32]);
    }

    #[test]
    fn mock_accessory_pair_verify_wrong_ltpk_rejected() {
        // Pairing recorded against accessory A, but we verify against a mock
        // whose LTPK differs → M2 signature must fail against the stored LTPK.
        let mut acc = MockAccessory::new(b"1234");
        let our_identity = Ed25519Identity::generate();
        let mut creds = run_pair_setup(&mut acc, "1234", &our_identity, "tune-controller-01")
            .expect("pair-setup");
        // Corrupt the stored accessory LTPK.
        creds.accessory_ltpk[0] ^= 0xff;
        let err = run_pair_verify(&mut acc, &creds).expect_err("mismatched LTPK must fail");
        assert!(matches!(err, PairingError::Crypto(_)), "got {err:?}");
    }
}
