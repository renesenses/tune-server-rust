use crate::streaming::deezer_decrypt;

const CHUNK_SIZE: usize = 2048;

pub fn proxy_url_for(server_ip: &str, port: u16, sng_id: &str, ext: &str) -> String {
    format!("http://{server_ip}:{port}/deezer/{sng_id}.{ext}")
}

pub fn content_type_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

pub fn parse_sng_id(filename: &str) -> Option<&str> {
    let sng_id = filename
        .rsplit_once('.')
        .map(|(id, _)| id)
        .unwrap_or(filename);
    if sng_id.chars().all(|c| c.is_ascii_digit()) && !sng_id.is_empty() {
        Some(sng_id)
    } else {
        None
    }
}

pub fn parse_ext(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("flac")
}

pub fn decrypt_stream_buffer(
    buffer: &mut Vec<u8>,
    chunk_index: &mut usize,
    key: &[u8],
    output: &mut Vec<u8>,
) {
    while buffer.len() >= CHUNK_SIZE {
        let chunk: Vec<u8> = buffer.drain(..CHUNK_SIZE).collect();
        if *chunk_index % 3 == 0 {
            output.extend(deezer_decrypt::decrypt_chunk(&chunk, key));
        } else {
            output.extend(&chunk);
        }
        *chunk_index += 1;
    }
}

pub fn compute_blowfish_key(sng_id: &str) -> Vec<u8> {
    deezer_decrypt::compute_blowfish_key(sng_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_format() {
        let url = proxy_url_for("192.168.1.10", 8080, "12345", "flac");
        assert_eq!(url, "http://192.168.1.10:8080/deezer/12345.flac");
    }

    #[test]
    fn content_type_mapping() {
        assert_eq!(content_type_for_ext("flac"), "audio/flac");
        assert_eq!(content_type_for_ext("mp3"), "audio/mpeg");
        assert_eq!(content_type_for_ext("wav"), "application/octet-stream");
    }

    #[test]
    fn parse_sng_id_valid() {
        assert_eq!(parse_sng_id("12345.flac"), Some("12345"));
        assert_eq!(parse_sng_id("999.mp3"), Some("999"));
    }

    #[test]
    fn parse_sng_id_invalid() {
        assert_eq!(parse_sng_id("abc.flac"), None);
        assert_eq!(parse_sng_id(".flac"), None);
    }

    #[test]
    fn parse_ext_works() {
        assert_eq!(parse_ext("12345.flac"), "flac");
        assert_eq!(parse_ext("12345.mp3"), "mp3");
        assert_eq!(parse_ext("12345"), "flac");
    }

    #[test]
    fn blowfish_key_deterministic() {
        let k1 = compute_blowfish_key("12345");
        let k2 = compute_blowfish_key("12345");
        assert_eq!(k1, k2);
    }

    #[test]
    fn blowfish_key_differs_per_song() {
        let k1 = compute_blowfish_key("12345");
        let k2 = compute_blowfish_key("67890");
        assert_ne!(k1, k2);
    }

    #[test]
    fn decrypt_stream_stripe_pattern() {
        let key = compute_blowfish_key("12345");
        let mut buffer = vec![0xAA_u8; CHUNK_SIZE * 4];
        let mut output = Vec::new();
        let mut chunk_index = 0;

        decrypt_stream_buffer(&mut buffer, &mut chunk_index, &key, &mut output);
        assert_eq!(chunk_index, 4);
        assert_eq!(output.len(), CHUNK_SIZE * 4);
    }
}
