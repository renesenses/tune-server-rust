//! Read the native saved collection, not albums inferred from liked tracks.
use std::collections::HashSet;

use librespot_core::{Session, SpotifyUri};
use serde::{Deserialize, Serialize};

use super::{bounded, catalog, unsupported};
use tune_core::TuneError;

const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;
const MAX_ALBUMS: usize = 2000;

#[derive(Serialize)]
struct PageRequest<'a> {
    username: &'a str,
    set: &'static str,
    pagination_token: &'a str,
    limit: usize,
}

// Field names/types from librespot 0.8.0 proto/collection2v2.proto.
// That schema is shipped upstream but not exported by librespot-protocol.
// Spotify's JSON representation avoids a second protobuf code generator.
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    next_page_token: String,
    #[serde(default)]
    sync_token: String,
}

#[derive(Deserialize)]
struct Item {
    uri: String,
    #[serde(default, deserialize_with = "added_at")]
    added_at: i64,
    #[serde(default)]
    is_removed: bool,
}

// ProtoJSON accepts integer fields as decimal strings as well as numbers.
// Every timestamp in the live Spotify collection was quoted (2026-09-15).
fn added_at<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Integer {
        Number(i64),
        Quoted(String),
    }
    match Option::<Integer>::deserialize(deserializer)? {
        None => Ok(0),
        Some(Integer::Number(n)) => Ok(n),
        Some(Integer::Quoted(s)) => s
            .parse()
            .map_err(|_| serde::de::Error::custom("invalid saved collection timestamp")),
    }
}

async fn read_albums<F, Fut>(username: &str, fetch: F) -> Result<Vec<SpotifyUri>, TuneError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<bytes::Bytes, TuneError>>,
{
    let mut token = String::new();
    let mut tokens = HashSet::new();
    let mut seen = HashSet::new();
    let mut albums = Vec::new();
    for _ in 0..MAX_PAGES {
        let request = serde_json::to_string(&PageRequest {
            username,
            set: "collection",
            pagination_token: &token,
            limit: PAGE_SIZE,
        })?;
        let bytes = fetch(request).await?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Spotify saved collection response exceeds limit".into());
        }
        let page: Page = serde_json::from_slice(&bytes)
            .map_err(|_| TuneError::from("Spotify saved collection response is invalid"))?;
        if page.items.len() > PAGE_SIZE {
            return Err("Spotify saved collection page exceeds requested size".into());
        }
        if page.next_page_token.len() > 4096 || page.sync_token.len() > 4096 {
            return Err("Spotify saved collection token exceeds limit".into());
        }
        if !page.next_page_token.is_empty() && page.items.is_empty() {
            return Err("Spotify saved collection pagination made no progress".into());
        }
        for item in page.items {
            let mut parts = item.uri.splitn(3, ':');
            if parts.next() != Some("spotify")
                || parts.next().is_none_or(str::is_empty)
                || parts.next().is_none_or(str::is_empty)
                || item.added_at < 0
            {
                return Err("Spotify saved collection contains an invalid item".into());
            }
            if !seen.insert(item.uri.clone()) {
                return Err(
                    "Spotify saved collection changed or contains duplicate items; retry".into(),
                );
            }
            if item.uri.starts_with("spotify:album:") && !item.is_removed {
                albums.push((item.added_at, catalog::uri(&item.uri, "album")?));
                if albums.len() > MAX_ALBUMS {
                    return Err(unsupported("saved collections above 2000 albums"));
                }
            }
        }
        token = page.next_page_token;
        if token.is_empty() {
            // An empty/malformed reply must not masquerade as an empty account.
            if page.sync_token.is_empty() {
                return Err("Spotify saved collection has no completion token".into());
            }
            albums.sort_by(|a, b| b.0.cmp(&a.0));
            return Ok(albums.into_iter().map(|(_, uri)| uri).collect());
        }
        if !tokens.insert(token.clone()) {
            return Err("Spotify saved collection pagination loop".into());
        }
    }
    Err(unsupported("saved collection scans above 10000 entries"))
}

pub(super) async fn album_uris(session: &Session) -> Result<Vec<SpotifyUri>, TuneError> {
    read_albums(&session.username(), |body| async move {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        bounded(session.spclient().request_as_json(
            &reqwest::Method::POST,
            "/collection/v2/paging",
            Some(headers),
            Some(&body),
        ))
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn uri(n: usize, kind: &str) -> String {
        format!("spotify:{kind}:{n:022}")
    }
    fn bytes(value: Value) -> bytes::Bytes {
        serde_json::to_vec(&value).unwrap().into()
    }

    #[tokio::test]
    async fn native_saved_albums_accept_real_quoted_timestamps_without_relaxing_validation() {
        let result = read_albums("fixture-user", |_| async {
            Ok(bytes(json!({"items":[
                {"uri":uri(1,"album"),"added_at":"1700000000"},
                {"uri":uri(2,"album"),"added_at":1700000001},
                {"uri":uri(3,"album")},
                {"uri":uri(4,"album"),"added_at":null}
            ],"sync_token":"done"})))
        }).await.expect("Real Spotify timestamps are quoted integers; they must not make saved albums unreadable");
        assert_eq!(
            result
                .iter()
                .map(|u| u.to_uri().unwrap())
                .collect::<Vec<_>>(),
            vec![
                uri(2, "album"),
                uri(1, "album"),
                uri(3, "album"),
                uri(4, "album")
            ],
            "Numeric and quoted dates must sort identically, absent dates last"
        );
        for invalid in [
            json!("broken"),
            json!("-1"),
            json!("9223372036854775808"),
            json!(true),
            json!(1.5),
        ] {
            assert!(read_albums("fixture-user", |_|async {Ok(bytes(json!({"items":[{"uri":uri(1,"album"),"added_at":invalid}],"sync_token":"done"})))}).await.is_err(),"Invalid timestamps must stay explicit errors");
        }
    }

    #[tokio::test]
    async fn native_saved_albums_read_all_pages_without_inventing_albums_from_tracks() {
        let calls = std::sync::Mutex::new(Vec::new());
        let result = read_albums("fixture-user", |body| {
            let r: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(r["set"], "collection", "Saved albums must query the native collection set");
            assert_eq!(r["username"], "fixture-user");
            assert_eq!(r["limit"], 100);
            calls.lock().unwrap().push(r["pagination_token"].clone());
            async move { Ok(if r["pagination_token"] == "" {
                bytes(json!({"items":[{"uri":uri(1,"album"),"added_at":1},{"uri":uri(9,"track"),"added_at":5}],"next_page_token":"second"}))
            } else {
                assert_eq!(r["pagination_token"], "second");
                bytes(json!({"items":[{"uri":uri(2,"album"),"added_at":3},{"uri":uri(3,"album"),"added_at":4,"is_removed":true},{"uri":uri(4,"artist"),"added_at":7}],"sync_token":"done"}))
            }) }
        }).await.unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "Saved albums must read the continuation page"
        );
        assert_eq!(
            result
                .iter()
                .map(|u| u.to_uri().unwrap())
                .collect::<Vec<_>>(),
            vec![uri(2, "album"), uri(1, "album")],
            "Only explicitly saved, non-removed albums belong here, newest first"
        );
    }

    #[tokio::test]
    async fn native_saved_albums_refuse_incomplete_repeated_and_failed_pages() {
        for defect in [
            "missing_completion",
            "empty_continuation",
            "loop",
            "duplicate",
            "invalid_album",
            "invalid_item",
            "network",
        ] {
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let result = read_albums("fixture-user", |_| {
                let call = calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                async move {
                    if defect == "network" { return Err("fixture HTTP 429".into()); }
                    Ok(bytes(match defect {
                        "missing_completion" => json!({}),
                        "empty_continuation" => json!({"items":[],"next_page_token":"next"}),
                        "loop" => json!({"items":[{"uri":uri(call,"album"),"added_at":1}],"next_page_token":"same"}),
                        "duplicate" => json!({"items":[{"uri":uri(1,"album"),"added_at":1},{"uri":uri(1,"album"),"added_at":1}],"sync_token":"done"}),
                        "invalid_album" => json!({"items":[{"uri":"spotify:album:bad","added_at":1}],"sync_token":"done"}),
                        "invalid_item" => json!({"items":[{"uri":"broken","added_at":1}],"sync_token":"done"}),
                        _ => unreachable!(),
                    }))
                }
            }).await;
            assert!(
                result.is_err(),
                "Saved albums must reject {defect}, not present a partial or empty account"
            );
        }
        assert!(
            read_albums("fixture-user", |_| async {
                Ok(bytes(json!({"sync_token":"done"})))
            })
            .await
            .unwrap()
            .is_empty(),
            "A completed empty saved collection is legitimate"
        );
    }

    #[tokio::test]
    async fn native_saved_albums_keep_scan_and_output_limits_explicit() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = read_albums("fixture-user", |_| {
            let call=calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            async move { Ok(bytes(json!({"items":(0..100).map(|n|json!({"uri":uri(call*100+n,"album"),"added_at":1})).collect::<Vec<_>>(),"next_page_token":format!("page-{call}")}))) }
        }).await;
        assert!(
            matches!(result, Err(TuneError::Unsupported(_))),
            "Oversized saved libraries require an explicit refusal, never truncation"
        );
    }
}
