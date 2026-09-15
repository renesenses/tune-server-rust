//! Bounded, complete collection reads. Never silently shorten a playback queue.
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, Playlist};
use protobuf::Message;

use super::{bounded, catalog, unsupported};
use crate::TuneError;

type PlaylistPage = <Playlist as Metadata>::Message;
const PAGE_SIZE: usize = 100;
pub(super) const MAX_TRACKS: usize = 2_000;

#[derive(Default)]
struct PlaylistPages {
    uris: Vec<SpotifyUri>,
    total: Option<usize>,
    revision: Option<Vec<u8>>,
}

impl PlaylistPages {
    fn append(&mut self, page: PlaylistPage) -> Result<bool, TuneError> {
        let total = page
            .length
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("Spotify playlist has no valid length")?;
        if total > MAX_TRACKS {
            return Err(unsupported("collections above 2000 tracks"));
        }
        if self.total.is_some_and(|n| n != total)
            || self
                .revision
                .as_ref()
                .is_some_and(|r| r.as_slice() != page.revision())
            || page.multiple_heads()
        {
            return Err("Spotify playlist changed during pagination; retry".into());
        }
        let contents = page
            .contents
            .as_ref()
            .ok_or("Spotify playlist is missing its contents")?;
        if contents.pos.and_then(|n| usize::try_from(n).ok()) != Some(self.uris.len()) {
            return Err("Spotify playlist page offset does not match the request".into());
        }
        let next = self.uris.len() + contents.items.len();
        if next > total
            || (contents.items.is_empty() && next < total)
            || (contents.truncated() && next == total)
        {
            return Err("Spotify playlist is partial or pagination made no progress".into());
        }
        if next < total && page.revision().is_empty() {
            return Err("Spotify playlist pagination requires a revision".into());
        }
        for item in &contents.items {
            // Keep duplicates: their positions are meaningful in a playlist.
            // Local files and podcast episodes are not playable by this source.
            self.uris.push(catalog::uri(item.uri(), "track")?);
        }
        self.total = Some(total);
        self.revision = Some(page.revision().to_vec());
        Ok(next == total)
    }
}

async fn read_playlist<F, Fut>(fetch: F) -> Result<Vec<SpotifyUri>, TuneError>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<bytes::Bytes, TuneError>>,
{
    let mut pages = PlaylistPages::default();
    loop {
        let bytes = fetch(pages.uris.len()).await?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Spotify playlist response exceeds limit".into());
        }
        let page = PlaylistPage::parse_from_bytes(&bytes)
            .map_err(|_| TuneError::from("Spotify playlist response is invalid"))?;
        if pages.append(page)? {
            return Ok(pages.uris);
        }
    }
}

pub(super) async fn playlist_uris(
    session: &Session,
    id: &str,
) -> Result<Vec<SpotifyUri>, TuneError> {
    let id = catalog::uri(id, "playlist")?
        .to_id()
        .map_err(|_| TuneError::from("Invalid Spotify playlist identifier"))?;
    read_playlist(|offset| {
        let endpoint = format!("/playlist/v2/playlist/{id}?from={offset}&length={PAGE_SIZE}");
        async move {
            bounded(
                session
                    .spclient()
                    .request(&reqwest::Method::GET, &endpoint, None, None),
            )
            .await
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    const TRACK: &str = "spotify:track:2cGxRwrMyEAp8dEbuZaVv6";

    fn page(total: i32, offset: i32, count: usize) -> PlaylistPage {
        let mut p = PlaylistPage::new();
        p.length = Some(total);
        p.revision = Some(vec![1; 16]);
        let contents = p.contents.mut_or_insert_default();
        contents.pos = Some(offset);
        contents.truncated = Some(offset + (count as i32) < total);
        for _ in 0..count {
            contents.items.push(Default::default());
            contents.items.last_mut().unwrap().uri = Some(TRACK.into());
        }
        p
    }

    #[tokio::test]
    async fn native_playlist_reads_every_page_above_300_and_preserves_duplicates() {
        let offsets = std::sync::Mutex::new(Vec::new());
        let uris = read_playlist(|offset| {
            offsets.lock().unwrap().push(offset);
            async move {
                Ok(page(520, offset as i32, (520 - offset).min(PAGE_SIZE))
                    .write_to_bytes()
                    .unwrap()
                    .into())
            }
        })
        .await
        .expect("A 520-track playlist must load completely, beyond the old 300-track cap");
        assert_eq!(
            *offsets.lock().unwrap(),
            [0, 100, 200, 300, 400, 500],
            "Every Spotify page must be requested at its exact track offset"
        );
        assert_eq!(
            uris.len(),
            520,
            "Playlist duplicates and the last page must survive pagination"
        );
        assert!(uris.iter().all(|uri| uri.to_uri().unwrap() == TRACK));
    }

    #[test]
    fn native_playlist_refuses_changed_incomplete_or_unsupported_pages() {
        for defect in [
            "revision",
            "length",
            "offset",
            "missing_offset",
            "empty",
            "overflow",
            "truncated",
            "kind",
            "missing_contents",
            "multiple_heads",
        ] {
            let mut pages = PlaylistPages::default();
            pages.append(page(2, 0, 1)).unwrap();
            let mut p = page(2, 1, 1);
            match defect {
                "revision" => p.revision = Some(vec![2; 16]),
                "length" => p.length = Some(3),
                "offset" => p.contents.as_mut().unwrap().pos = Some(0),
                "missing_offset" => p.contents.as_mut().unwrap().pos = None,
                "empty" => p.contents.as_mut().unwrap().items.clear(),
                "overflow" => p.contents.as_mut().unwrap().items.push(Default::default()),
                "truncated" => p.contents.as_mut().unwrap().truncated = Some(true),
                "kind" => {
                    p.contents.as_mut().unwrap().items[0].uri = Some("spotify:local:file".into())
                }
                "missing_contents" => p.contents = Default::default(),
                "multiple_heads" => p.multiple_heads = Some(true),
                _ => unreachable!(),
            }
            assert!(
                pages.append(p).is_err(),
                "Spotify playlist must reject {defect}, not return a truncated or mixed queue"
            );
        }
    }

    #[tokio::test]
    async fn native_playlist_empty_is_valid_but_errors_and_resource_limits_are_not() {
        assert!(PlaylistPages::default().append(page(0, 0, 0)).unwrap());
        assert!(PlaylistPages::default().append(page(2001, 0, 0)).is_err());
        let mut missing = page(2, 0, 1);
        missing.revision = None;
        assert!(PlaylistPages::default().append(missing).is_err());
        assert!(
            read_playlist(|_| async { Err("fixture HTTP 403".into()) })
                .await
                .is_err(),
            "A refused playlist must not become an empty list"
        );
        assert!(
            read_playlist(|_| async { Ok(bytes::Bytes::from_static(b"invalid")) })
                .await
                .is_err()
        );
        assert!(
            read_playlist(|_| async { Ok(vec![0; 8 * 1024 * 1024 + 1].into()) })
                .await
                .is_err()
        );
    }
}
