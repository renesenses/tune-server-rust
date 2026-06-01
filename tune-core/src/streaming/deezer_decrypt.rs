use md5::{Digest, Md5};

use blowfish::Blowfish;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};

type BlowfishCbcDec = cbc::Decryptor<Blowfish>;

const SECRET: &[u8] = b"g4el58wc0zvf9na1";
const IV: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
const CHUNK_SIZE: usize = 2048;

pub fn compute_blowfish_key(sng_id: &str) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(sng_id.as_bytes());
    let md5_hex = format!("{:x}", hasher.finalize());
    let md5_bytes = md5_hex.as_bytes();
    (0..16)
        .map(|i| md5_bytes[i] ^ md5_bytes[i + 16] ^ SECRET[i])
        .collect()
}

pub fn decrypt_chunk(chunk: &[u8], key: &[u8]) -> Vec<u8> {
    if chunk.len() < 8 {
        return chunk.to_vec();
    }
    let aligned_len = (chunk.len() / 8) * 8;
    let (head, tail) = chunk.split_at(aligned_len);
    let mut buf = head.to_vec();
    let decryptor = BlowfishCbcDec::new_from_slices(key, &IV).expect("invalid key/iv");
    decryptor
        .decrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buf)
        .expect("decrypt failed");
    buf.extend_from_slice(tail);
    buf
}

pub struct DeezerDecryptStream {
    key: Vec<u8>,
    buffer: Vec<u8>,
    chunk_index: usize,
}

impl DeezerDecryptStream {
    pub fn new(sng_id: &str) -> Self {
        Self {
            key: compute_blowfish_key(sng_id),
            buffer: Vec::new(),
            chunk_index: 0,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut output = Vec::new();
        while self.buffer.len() >= CHUNK_SIZE {
            let chunk: Vec<u8> = self.buffer.drain(..CHUNK_SIZE).collect();
            if self.chunk_index.is_multiple_of(3) {
                output.push(decrypt_chunk(&chunk, &self.key));
            } else {
                output.push(chunk);
            }
            self.chunk_index += 1;
        }
        output
    }

    pub fn finish(self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(self.buffer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blowfish_key_derivation() {
        let key = compute_blowfish_key("12345678");
        assert_eq!(key.len(), 16);
        // Verify deterministic
        assert_eq!(key, compute_blowfish_key("12345678"));
        // Different IDs produce different keys
        assert_ne!(key, compute_blowfish_key("87654321"));
    }

    #[test]
    fn decrypt_chunk_roundtrip() {
        use blowfish::Blowfish;
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};

        type BlowfishCbcEnc = cbc::Encryptor<Blowfish>;

        let key = compute_blowfish_key("999");
        let plain = vec![0x42u8; 2048];
        let mut encrypted = plain.clone();
        let encryptor = BlowfishCbcEnc::new_from_slices(&key, &IV).unwrap();
        encryptor
            .encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut encrypted, 2048)
            .unwrap();

        let decrypted = decrypt_chunk(&encrypted, &key);
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn small_chunk_passthrough() {
        let key = compute_blowfish_key("1");
        let small = vec![1, 2, 3];
        assert_eq!(decrypt_chunk(&small, &key), small);
    }

    #[test]
    fn stripe_logic() {
        let mut stream = DeezerDecryptStream::new("42");
        // Feed 3 chunks worth of data (6144 bytes)
        let data = vec![0u8; 6144];
        let chunks = stream.feed(&data);
        assert_eq!(chunks.len(), 3);
        // Only chunk 0 is decrypted (index 0 % 3 == 0), chunks 1 and 2 pass through
        assert_eq!(chunks[1], vec![0u8; 2048]);
        assert_eq!(chunks[2], vec![0u8; 2048]);
        assert!(stream.finish().is_none());
    }

    #[test]
    fn partial_tail() {
        let mut stream = DeezerDecryptStream::new("42");
        let data = vec![0u8; 2100]; // 2048 + 52
        let chunks = stream.feed(&data);
        assert_eq!(chunks.len(), 1);
        let tail = stream.finish().unwrap();
        assert_eq!(tail.len(), 52);
    }
}
