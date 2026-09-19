use base64::{Engine, engine::general_purpose::STANDARD};
use blake2::{Blake2b512, Digest};
use ed25519_dalek::{Signer, SigningKey};
// Publicly known fixture only. Never installed into an operator trust store.
pub fn sign(bytes: &[u8]) -> (Vec<String>, String) {
    let key = SigningKey::from_bytes(&[7; 32]);
    let id = [9; 8];
    let signature = key.sign(&Blake2b512::digest(bytes)).to_bytes();
    let mut pk = b"Ed".to_vec();
    pk.extend(id);
    pk.extend(key.verifying_key().as_bytes());
    let mut sig = b"ED".to_vec();
    sig.extend(id);
    sig.extend(signature);
    let mut global = signature.to_vec();
    global.extend(b"test fixture");
    (
        vec![STANDARD.encode(pk)],
        format!(
            "untrusted comment: fixture\n{}\ntrusted comment: test fixture\n{}\n",
            STANDARD.encode(sig),
            STANDARD.encode(key.sign(&global).to_bytes())
        ),
    )
}
