use md5::{Digest, Md5};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const SAMPLE_SIZE: usize = 65536; // 64 KB
const AUDIO_HASH_VERSION: &str = "sample64k-v2";

pub fn compute_audio_hash(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();
    if file_size == 0 {
        return None;
    }

    let offset = file_size / 4; // 25% into the file
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut buf = vec![0u8; SAMPLE_SIZE.min(file_size as usize)];
    let n = file.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }

    let mut hasher = Md5::new();
    // The size makes the candidate substantially more selective at no extra
    // I/O cost. The version prefix prevents the two legacy recipes that used
    // to share this column from ever comparing equal with the new contract.
    hasher.update(file_size.to_le_bytes());
    hasher.update(&buf[..n]);
    Some(format!("{AUDIO_HASH_VERSION}:{:x}", hasher.finalize()))
}

pub fn compute_audio_hash_str(path: &str) -> Option<String> {
    compute_audio_hash(Path::new(path))
}

pub fn is_current_audio_hash(hash: &str) -> bool {
    hash.starts_with(AUDIO_HASH_VERSION)
        && hash.as_bytes().get(AUDIO_HASH_VERSION.len()) == Some(&b':')
}

/// Confirm a candidate duplicate without trusting the sampled hash.
///
/// `audio_hash` deliberately remains cheap enough for a large NAS scan. Any
/// decision that can hide or delete a track must therefore pass this complete
/// byte-for-byte comparison first.
pub fn files_are_byte_identical(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    if left_file.metadata()?.len() != right_file.metadata()?.len() {
        return Ok(false);
    }

    let mut left_buf = [0u8; SAMPLE_SIZE];
    let mut right_buf = [0u8; SAMPLE_SIZE];
    loop {
        let left_n = left_file.read(&mut left_buf)?;
        let right_n = right_file.read(&mut right_buf)?;
        if left_n != right_n || left_buf[..left_n] != right_buf[..right_n] {
            return Ok(false);
        }
        if left_n == 0 {
            return Ok(true);
        }
    }
}

pub fn find_byte_identical_path(path: &Path, candidates: &[String]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        files_are_byte_identical(path, Path::new(candidate))
            .ok()
            .filter(|same| *same)
            .map(|_| candidate.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        {
            let mut f = File::create(&path).unwrap();
            let data = vec![42u8; 256 * 1024]; // 256 KB
            f.write_all(&data).unwrap();
        }
        let hash = compute_audio_hash(&path).unwrap();
        assert!(is_current_audio_hash(&hash));
        assert_eq!(hash.len(), AUDIO_HASH_VERSION.len() + 1 + 32);

        let hash2 = compute_audio_hash(&path).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn hash_empty_file() {
        let empty = tempfile::Builder::new().suffix(".bin").tempfile().unwrap();
        assert!(compute_audio_hash(empty.path()).is_none());
    }

    #[test]
    fn une_collision_de_fenetre_ne_devient_pas_un_doublon_octet_pour_octet() {
        let dir = tempfile::TempDir::new().unwrap();
        let left = dir.path().join("left.flac");
        let right = dir.path().join("right.flac");
        let size = SAMPLE_SIZE * 4;
        let mut left_bytes = vec![0u8; size];
        let mut right_bytes = vec![0u8; size];

        // The sampled window starts at size / 4 and is intentionally equal.
        // Everything after it differs while the two files keep the same size.
        left_bytes[SAMPLE_SIZE * 2..].fill(0x11);
        right_bytes[SAMPLE_SIZE * 2..].fill(0x22);
        std::fs::write(&left, left_bytes).unwrap();
        std::fs::write(&right, right_bytes).unwrap();

        assert_eq!(compute_audio_hash(&left), compute_audio_hash(&right));
        assert!(!files_are_byte_identical(&left, &right).unwrap());
        assert_eq!(
            find_byte_identical_path(&left, &[right.to_string_lossy().into_owned()]),
            None
        );
    }

    #[test]
    fn une_copie_exacte_est_confirmee_sur_tous_les_octets() {
        let dir = tempfile::TempDir::new().unwrap();
        let left = dir.path().join("left.flac");
        let right = dir.path().join("right.flac");
        let bytes = vec![0x5au8; SAMPLE_SIZE * 3];
        std::fs::write(&left, &bytes).unwrap();
        std::fs::write(&right, &bytes).unwrap();

        assert!(files_are_byte_identical(&left, &right).unwrap());
        assert_eq!(
            find_byte_identical_path(&left, &[right.to_string_lossy().into_owned()]),
            Some(right.to_string_lossy().into_owned())
        );
    }
}
