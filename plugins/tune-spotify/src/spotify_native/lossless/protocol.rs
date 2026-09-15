//! The small protobuf surface absent from librespot-protocol 0.8.0's exports.
//! No account token, APK token, content key or signed URL belongs in a fixture.
use librespot_protocol::metadata::AudioFile;
use protobuf::Message;

pub(super) const MAX_MESSAGE: usize = 65_536;

fn varint(bytes: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut result = 0;
    for shift in (0..70).step_by(7) {
        let value = *bytes.get(*position).ok_or("Spotify protobuf truncated")?;
        *position += 1;
        if shift == 63 && value > 1 {
            return Err("Spotify protobuf integer overflow".into());
        }
        result |= u64::from(value & 127) << shift;
        if value & 128 == 0 {
            return Ok(result);
        }
    }
    Err("Spotify protobuf integer overflow".into())
}

pub(super) fn fields(bytes: &[u8], wanted: u64) -> Result<Vec<&[u8]>, String> {
    if bytes.len() > MAX_MESSAGE {
        return Err("Spotify protobuf exceeds limit".into());
    }
    let mut position = 0;
    let mut found = Vec::new();
    while position < bytes.len() {
        let tag = varint(bytes, &mut position)?;
        if tag >> 3 == 0 || tag >> 3 > 0x1fff_ffff {
            return Err("Spotify protobuf invalid field number".into());
        }
        if tag >> 3 == wanted && tag & 7 != 2 {
            return Err("Spotify protobuf wrong field type".into());
        }
        let count = match tag & 7 {
            0 => {
                varint(bytes, &mut position)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(varint(bytes, &mut position)?)
                .map_err(|_| "Spotify protobuf length overflow")?,
            5 => 4,
            _ => return Err("Spotify protobuf unsupported wire type".into()),
        };
        let end = position
            .checked_add(count)
            .ok_or("Spotify protobuf length overflow")?;
        let field = bytes
            .get(position..end)
            .ok_or("Spotify protobuf truncated")?;
        if tag >> 3 == wanted {
            found.push(field);
        }
        position = end;
    }
    Ok(found)
}

pub(super) fn audio_files(bytes: &[u8]) -> Result<Vec<AudioFile>, String> {
    let mut files = Vec::new();
    // AudioFilesExtensionResponse.files(1) -> ExtendedAudioFile.file(1).
    for item in fields(bytes, 1)? {
        let nested = fields(item, 1)?;
        if nested.len() != 1 {
            return Err("Spotify extended audio file is ambiguous".into());
        }
        let file = AudioFile::parse_from_bytes(nested[0])
            .map_err(|_| "Spotify extended audio file is invalid")?;
        if file.file_id().len() != 20 || file.format.is_none() {
            return Err("Spotify extended audio file is incomplete".into());
        }
        files.push(file);
    }
    Ok(files)
}

pub(super) fn license_request(token: &[u8; 16], timestamp: u64) -> Vec<u8> {
    // PlayPlay version 5, interactive audio track. No offline cache registration.
    let mut out = vec![8, 5, 18, 16];
    out.extend_from_slice(token);
    out.extend_from_slice(&[32, 1, 40, 1, 48]);
    let mut value = timestamp;
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

pub(super) fn obfuscated_key(bytes: &[u8]) -> Result<[u8; 16], String> {
    let keys = fields(bytes, 1)?;
    if keys.len() != 1 {
        return Err("Spotify license must contain exactly one key".into());
    }
    keys[0]
        .try_into()
        .map_err(|_| "Spotify license key has invalid length".into())
    // Field 2 is NOT the CRC32 of the clear key. Its semantics are unknown.
}

pub(super) fn cdn_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "Spotify CDN URL is invalid")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || !url.host_str().is_some_and(|host| {
            [".scdn.co", ".spotifycdn.com", ".akamaized.net"]
                .iter()
                .any(|suffix| host.ends_with(suffix))
        })
    {
        return Err("Spotify CDN URL is outside the allowed transport".into());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_lossless_license_request_is_interactive_v5_without_offline_cache() {
        let request = license_request(&[0x42; 16], 300);
        assert_eq!(&request[..4], &[8, 5, 18, 16]);
        assert_eq!(&request[4..20], &[0x42; 16]);
        assert_eq!(&request[20..], &[32, 1, 40, 1, 48, 0xac, 2]);
    }
    #[test]
    fn native_lossless_license_rejects_missing_duplicate_and_truncated_keys() {
        let good = [vec![10, 16], vec![42; 16], vec![18, 4, 1, 2, 3, 4]].concat();
        assert_eq!(obfuscated_key(&good).unwrap(), [42; 16]);
        for bad in [
            vec![],
            vec![10, 16, 1],
            [good.clone(), good].concat(),
            vec![0],
            vec![15],
            vec![10, 255],
            vec![10, 0],
            vec![0x80; 10],
            [vec![10], vec![255; 10]].concat(),
            vec![0; MAX_MESSAGE + 1],
        ] {
            assert!(
                obfuscated_key(&bad).is_err(),
                "Malformed licenses must never yield a key"
            );
        }
    }
    #[test]
    fn native_lossless_extension_reads_real_audio_file_nesting() {
        let file = [vec![10, 20], vec![0x77; 20], vec![16, 16]].concat();
        let inner = [vec![10, file.len() as u8], file].concat();
        let outer = [vec![10, inner.len() as u8], inner].concat();
        let files = audio_files(&outer).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_id(), &[0x77; 20]);
        assert_eq!(files[0].format.as_ref().unwrap().value(), 16);
        assert!(audio_files(&outer[..outer.len() - 1]).is_err());
    }
    #[test]
    fn native_lossless_cdn_requires_https_and_a_label_bounded_host() {
        assert!(cdn_url("https://audio-fa-l.spotifycdn.com/audio/test?sig=synthetic").is_ok());
        for url in [
            "http://audio.spotifycdn.com/a",
            "https://spotifycdn.com.evil.test/a",
            "https://evilspotifycdn.com/a",
            "https://127.0.0.1/a",
            "https://user:pass@audio.spotifycdn.com/a",
            "https://audio.spotifycdn.com:444/a",
            "https://audio.spotifycdn.com/a#fragment",
        ] {
            assert!(
                cdn_url(url).is_err(),
                "Untrusted CDN transport must be refused"
            );
        }
    }
}
