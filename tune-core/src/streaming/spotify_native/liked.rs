//! Read the paired account's liked-tracks context, without Web API credentials.
use super::{bounded, catalog, collections::MAX_TRACKS, unsupported};
use crate::TuneError;
use librespot_core::{Session, SpotifyUri};
use librespot_protocol::{context::Context, context_page::ContextPage};
use std::collections::{HashSet, VecDeque};

fn page_url(url: &str) -> Result<&str, TuneError> {
    // A continuation is not permission to fetch an arbitrary URL with tokens.
    if !url.starts_with("hm://collection-resolve/")
        || url.len() > 4096
        || url.contains(['\\', '#', '@'])
        || url.split('/').any(|s| s == "." || s == "..")
    {
        return Err(unsupported("unrecognized liked-tracks continuation"));
    }
    Ok(url)
}

async fn read<F, Fut>(context: Context, fetch: F) -> Result<Vec<SpotifyUri>, TuneError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<ContextPage, TuneError>>,
{
    if context.loading() || context.pages.is_empty() {
        return Err("Spotify liked tracks context is incomplete".into());
    }
    let mut pending: VecDeque<_> = context.pages.into();
    let mut fetched = HashSet::new();
    let mut seen = HashSet::new();
    let mut uris = Vec::new();
    let mut page_count = 0;
    while let Some(mut page) = pending.pop_front() {
        page_count += 1;
        if page_count > 100 {
            return Err(unsupported("liked tracks pagination exceeds limit"));
        }
        if page.tracks.is_empty() && !page.page_url().is_empty() {
            let url = page_url(page.page_url())?.to_owned();
            if !fetched.insert(url.clone()) {
                return Err("Spotify liked tracks pagination loop".into());
            }
            page = fetch(url).await?;
        }
        if page.loading()
            || (page.tracks.is_empty()
                && (!page.next_page_url().is_empty() || !page.page_url().is_empty()))
        {
            return Err("Spotify liked tracks page is incomplete or made no progress".into());
        }
        if uris.len() + page.tracks.len() > MAX_TRACKS {
            return Err(unsupported("collections above 2000 tracks"));
        }
        for item in &page.tracks {
            let uri = if !item.uri().is_empty() {
                catalog::uri(item.uri(), "track")?
            } else {
                SpotifyUri::Track {
                    id: librespot_core::SpotifyId::from_raw(item.gid()).map_err(|_| {
                        TuneError::from("Spotify liked track has no valid identifier")
                    })?,
                }
            };
            if !seen.insert(uri.clone()) {
                return Err(
                    "Spotify liked tracks changed or repeated during pagination; retry".into(),
                );
            }
            uris.push(uri);
        }
        if !page.next_page_url().is_empty() {
            let next = page_url(page.next_page_url())?.to_owned();
            pending.push_front(ContextPage {
                page_url: Some(next),
                ..Default::default()
            });
        }
    }
    Ok(uris)
}

pub(super) async fn uris(session: &Session) -> Result<Vec<SpotifyUri>, TuneError> {
    let uri = format!(
        "spotify:user:{}:collection",
        urlencoding::encode(&session.username())
    );
    let context = bounded(session.spclient().get_context(&uri)).await?;
    read(context, |url| async move {
        let bytes = bounded(session.spclient().get_next_page(page_url(&url)?)).await?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Spotify liked tracks response exceeds limit".into());
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| TuneError::from("Spotify liked tracks page is invalid"))?;
        protobuf_json_mapping::parse_from_str::<ContextPage>(text)
            .map_err(|_| TuneError::from("Spotify liked tracks page is invalid"))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(start: u128, count: u128) -> ContextPage {
        ContextPage {
            tracks: (start..start + count)
                .map(|id| librespot_protocol::context_track::ContextTrack {
                    gid: Some(id.to_be_bytes().to_vec()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    fn context(page: ContextPage) -> Context {
        Context {
            pages: vec![page],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn native_liked_tracks_reads_the_complete_context_beyond_300() {
        let uris = read(context(page(1, 1512)), |_| async {
            panic!("An inline liked collection needs no continuation request")
        })
        .await
        .expect("All 1512 liked tracks must survive, not the search/300-track limit");
        assert_eq!(
            uris.len(),
            1512,
            "Liked tracks must not be silently truncated"
        );
        assert_eq!(
            uris[0],
            SpotifyUri::Track {
                id: librespot_core::SpotifyId::from_raw(&1u128.to_be_bytes()).unwrap()
            }
        );
        assert!(
            read(context(page(0, 0)), |_| async { unreachable!() })
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn native_liked_tracks_resolves_safe_continuations_and_preserves_order() {
        let mut first = page(1, 2);
        first.next_page_url = Some("hm://collection-resolve/v2/fixture?page=2".into());
        let calls = std::sync::Mutex::new(Vec::new());
        let uris = read(context(first), |url| {
            calls.lock().unwrap().push(url);
            async { Ok(page(3, 2)) }
        })
        .await
        .unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "Liked tracks continuation must actually be fetched"
        );
        assert_eq!(uris.len(), 4, "Both liked-tracks pages must be returned");
        assert_eq!(
            uris[3],
            SpotifyUri::Track {
                id: librespot_core::SpotifyId::from_raw(&4u128.to_be_bytes()).unwrap()
            }
        );
    }

    #[tokio::test]
    async fn native_liked_tracks_refuses_incomplete_unsafe_or_repeated_pages() {
        for defect in [
            "loading",
            "missing",
            "limit",
            "external",
            "loop",
            "duplicate",
            "network",
            "unresolved",
            "empty_progress",
        ] {
            let mut first = page(1, 2);
            first.next_page_url = Some("hm://collection-resolve/v2/fixture?page=2".into());
            let mut c = context(first);
            match defect {
                "loading" => c.loading = Some(true),
                "missing" => c.pages.clear(),
                "limit" => c = context(page(1, 2001)),
                "external" => {
                    c.pages[0].next_page_url = Some("https://attacker.invalid/secret".into())
                }
                _ => (),
            }
            let result = read(c, |_| async {
                match defect {
                    "network" => Err("fixture HTTP 403".into()),
                    "loop" => {
                        let mut p = page(3, 1);
                        p.next_page_url = Some("hm://collection-resolve/v2/fixture?page=2".into());
                        Ok(p)
                    }
                    "duplicate" => Ok(page(1, 2)),
                    "unresolved" => Ok(ContextPage {
                        page_url: Some("hm://collection-resolve/v2/fixture?page=3".into()),
                        ..Default::default()
                    }),
                    "empty_progress" => Ok(ContextPage {
                        next_page_url: Some("hm://collection-resolve/v2/fixture?page=3".into()),
                        ..Default::default()
                    }),
                    _ => panic!(
                        "Unsafe or incomplete contexts must fail before a continuation request"
                    ),
                }
            })
            .await;
            assert!(
                result.is_err(),
                "Liked tracks must refuse {defect}, never invent a complete collection"
            );
        }
        for url in [
            "hm://collection-resolve.evil/secret",
            "hm://collection-resolve/../account",
            "hm://collection-resolve/v2/x#secret",
        ] {
            assert!(
                page_url(url).is_err(),
                "A continuation must stay on the collection resolver"
            );
        }
    }
}
