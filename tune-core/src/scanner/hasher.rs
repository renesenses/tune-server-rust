use md5::{Digest, Md5};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SAMPLE_SIZE: usize = 65536; // 64 KB

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
    hasher.update(&buf[..n]);
    Some(format!("{:x}", hasher.finalize()))
}

pub fn compute_audio_hash_str(path: &str) -> Option<String> {
    compute_audio_hash(Path::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_file() {
        let dir = std::env::temp_dir().join("tune_hash_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.bin");
        {
            let mut f = File::create(&path).unwrap();
            let data = vec![42u8; 256 * 1024]; // 256 KB
            f.write_all(&data).unwrap();
        }
        let hash = compute_audio_hash(&path).unwrap();
        assert_eq!(hash.len(), 32);

        let hash2 = compute_audio_hash(&path).unwrap();
        assert_eq!(hash, hash2);

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn hash_empty_file() {
        let path = std::env::temp_dir().join("tune_hash_empty.bin");
        File::create(&path).unwrap();
        assert!(compute_audio_hash(&path).is_none());
        std::fs::remove_file(&path).ok();
    }
}
