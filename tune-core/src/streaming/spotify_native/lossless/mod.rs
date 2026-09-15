//! Experimental PlayPlay/FLAC path, deliberately separate from Ogg/librespot.
//! The calculation backend is replaceable; no proprietary binary is shipped.
mod decoder;
mod key_provider;
mod protocol;
mod source;

use librespot_core::Session;
use librespot_protocol::{extension_kind::ExtensionKind, storage_resolve::StorageResolveResponse};
use protobuf::Message;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn requested_format() -> Result<Option<i32>, String> {
    parse_preference(std::env::var("TUNE_SPOTIFY_LOSSLESS").ok().as_deref())
}

fn parse_preference(value: Option<&str>) -> Result<Option<i32>, String> {
    match value {
        None | Some("") | Some("0") | Some("off") => Ok(None),
        Some("16") => Ok(Some(16)),
        Some("24") => Ok(Some(22)),
        _ => Err("TUNE_SPOTIFY_LOSSLESS must be off, 16 or 24".into()),
    }
}

// Never Debug: contains a content key and signed CDN URLs, all worker-local.
pub(super) struct Prepared {
    urls: Vec<String>,
    key: [u8; 16],
    bit_depth: u16,
}

impl Prepared {
    pub(super) fn decode(
        self,
        seek_ms: u32,
        track: crate::streaming::StreamTrack,
        published: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), String> {
        // Only a server-listed alternative for a missing/removed CDN object.
        // Never retry or bypass 401/403/429, and never silently downgrade codec.
        let mut last_error = "Spotify returned no CDN URL".to_owned();
        for url in self.urls.iter().take(3) {
            let source = match source::RangeSource::open(url) {
                Ok(source) => source,
                Err(error) if error.ends_with("(HTTP 404)") || error.ends_with("(HTTP 410)") => {
                    last_error = error;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let source = source::DecryptSource::new(source, &self.key);
            return decoder::run(Box::new(source), self.bit_depth, seek_ms, track, published);
        }
        Err(last_error)
    }
}

pub(super) async fn prepare(session: &Session, id: &str, format: i32) -> Result<Prepared, String> {
    let bit_depth = match format {
        16 => 16,
        22 => 24,
        _ => return Err("Spotify lossless format is unsupported".into()),
    };
    // Validate the local backend before any license request.
    let provider = key_provider::KeyProvider::open().await?;
    let uri = super::catalog::uri(id, "track").map_err(|_| "Spotify track URI is invalid")?;
    let extension = session
        .spclient()
        .get_metadata(ExtensionKind::AUDIO_FILES, &uri)
        .await
        .map_err(|_| "Spotify audio-files metadata request failed")?;
    let files = protocol::audio_files(&extension)?;
    let file = files
        .iter()
        .find(|file| file.format.as_ref().map(|v| v.value()) == Some(format))
        .ok_or_else(|| {
            format!("Spotify FLAC {bit_depth}-bit is unavailable for this track/account")
        })?;
    let file_id: [u8; 20] = file
        .file_id()
        .try_into()
        .map_err(|_| "Spotify file ID is invalid")?;

    let base = session
        .spclient()
        .base_url()
        .await
        .map_err(|_| "Spotify license endpoint lookup failed")?;
    let mut endpoint =
        reqwest::Url::parse(&base).map_err(|_| "Spotify license endpoint is invalid")?;
    if endpoint.scheme() != "https"
        || !endpoint
            .host_str()
            .is_some_and(|h| h.ends_with(".spotify.com"))
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.port_or_known_default() != Some(443)
    {
        return Err("Spotify license endpoint is outside the allowed transport".into());
    }
    endpoint.set_path(&format!("/playplay/v1/key/{}", hex::encode(file_id)));
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let bearer = session
        .login5()
        .auth_token()
        .await
        .map_err(|_| "Spotify session token request failed")?;
    let client_token = session
        .spclient()
        .client_token()
        .await
        .map_err(|_| "Spotify client token request failed")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Spotify license clock is invalid")?
        .as_secs();
    let body = protocol::license_request(&provider.token, now);
    let client = crate::http::client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Spotify license client failed")?;
    let mut response = client
        .post(endpoint)
        .bearer_auth(&bearer.access_token)
        .header("client-token", client_token)
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/x-protobuf")
        .body(body)
        .send()
        .await
        .map_err(|_| "Spotify license transport failed")?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "Spotify PlayPlay license refused (HTTP {})",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > protocol::MAX_MESSAGE as u64)
    {
        return Err("Spotify license response exceeds limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Spotify license body failed")?
    {
        if bytes.len() + chunk.len() > protocol::MAX_MESSAGE {
            return Err("Spotify license response exceeds limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let key = provider
        .derive(&file_id, &protocol::obfuscated_key(&bytes)?)
        .await?;
    // Format-qualified v2 is required: the old resolver returns dead FLAC URLs.
    let path = format!(
        "/storage-resolve/v2/files/audio/interactive/{format}/{}",
        hex::encode(file_id)
    );
    let storage = session
        .spclient()
        .request(&reqwest::Method::GET, &path, None, None)
        .await
        .map_err(|_| "Spotify FLAC storage resolution failed")?;
    if storage.len() > protocol::MAX_MESSAGE {
        return Err("Spotify storage response exceeds limit".into());
    }
    let storage = StorageResolveResponse::parse_from_bytes(&storage)
        .map_err(|_| "Spotify storage response is invalid")?;
    if !storage.fileid.is_empty() && storage.fileid != file_id {
        return Err("Spotify storage resolved a different file".into());
    }
    if storage.cdnurl.is_empty() {
        return Err("Spotify FLAC storage has no CDN URL".into());
    }
    for url in storage.cdnurl.iter().take(3) {
        protocol::cdn_url(url)?;
    }
    Ok(Prepared {
        urls: storage.cdnurl.into_iter().take(3).collect(),
        key,
        bit_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_lossless_is_opt_in_and_never_silently_downgrades_configuration() {
        assert_eq!(parse_preference(None).unwrap(), None);
        assert_eq!(parse_preference(Some("off")).unwrap(), None);
        assert_eq!(parse_preference(Some("16")).unwrap(), Some(16));
        assert_eq!(parse_preference(Some("24")).unwrap(), Some(22));
        assert!(parse_preference(Some("lossless-auto")).is_err());
    }
}
