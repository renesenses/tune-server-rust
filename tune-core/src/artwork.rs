use std::path::{Path, PathBuf};


const FOLDER_COVER_NAMES: &[&str] = &[
    "cover.jpg", "cover.png", "folder.jpg", "folder.png",
    "front.jpg", "front.png", "album.jpg", "album.png",
    "Cover.jpg", "Cover.png", "Folder.jpg", "Folder.png",
    "Front.jpg", "Front.png",
];

pub fn extract_cover_art(audio_path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;

    let tagged = lofty::read_from_path(audio_path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;

    let mime = match pic.mime_type() {
        Some(lofty::picture::MimeType::Jpeg) => "image/jpeg",
        Some(lofty::picture::MimeType::Png) => "image/png",
        Some(lofty::picture::MimeType::Bmp) => "image/bmp",
        _ => "image/jpeg",
    };

    Some((pic.data().to_vec(), mime.to_string()))
}

pub fn find_folder_cover(audio_path: &Path) -> Option<PathBuf> {
    let dir = audio_path.parent()?;
    for name in FOLDER_COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub fn save_to_cache(data: &[u8], cache_dir: &Path, hash: &str, ext: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(cache_dir).ok()?;
    let filename = format!("{hash}.{ext}");
    let path = cache_dir.join(&filename);
    std::fs::write(&path, data).ok()?;
    Some(path)
}

pub fn artwork_hash(file_path: &str) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    hasher.update(file_path.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn get_or_extract(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    let hash = artwork_hash(&audio_path.to_string_lossy());

    let cached_jpg = cache_dir.join(format!("{hash}.jpg"));
    let cached_png = cache_dir.join(format!("{hash}.png"));
    if cached_jpg.exists() {
        return Some(hash);
    }
    if cached_png.exists() {
        return Some(hash);
    }

    if let Some((data, mime)) = extract_cover_art(audio_path) {
        let ext = if mime.contains("png") { "png" } else { "jpg" };
        save_to_cache(&data, cache_dir, &hash, ext);
        return Some(hash);
    }

    if let Some(folder_cover) = find_folder_cover(audio_path)
        && let Ok(data) = std::fs::read(&folder_cover) {
            let ext = folder_cover.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            save_to_cache(&data, cache_dir, &hash, ext);
            return Some(hash);
        }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_hash_deterministic() {
        let h1 = artwork_hash("/music/test.flac");
        let h2 = artwork_hash("/music/test.flac");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn nonexistent_file_returns_none() {
        assert!(extract_cover_art(Path::new("/tmp/nonexistent.flac")).is_none());
    }
}
