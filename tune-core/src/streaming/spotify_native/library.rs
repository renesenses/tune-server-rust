//! Read-only personal playlists through the paired session's rootlist.
//! No Web API token, browser cookie, or user-supplied endpoint is involved.
use std::collections::HashSet;

use futures_util::{StreamExt, TryStreamExt, stream};
use librespot_core::Session;
use librespot_metadata::{Metadata, Playlist};
use protobuf::Message;

use super::{bounded, catalog, unsupported};
use crate::{TuneError, streaming::traits::StreamPlaylist};

type Rootlist = <Playlist as Metadata>::Message;
const PAGE_SIZE: usize = 100;
const MAX_ENTRIES: usize = 300;

struct PlaylistEntry {
    id: String,
    metadata: Option<StreamPlaylist>,
}

#[derive(Default)]
struct LibraryPages {
    next: usize,
    total: Option<usize>,
    revision: Option<Vec<u8>>,
    seen: HashSet<String>,
    playlists: Vec<PlaylistEntry>,
}

impl LibraryPages {
    /// Fail closed on a partial or changing collection. Folder markers count
    /// toward Spotify offsets even though Tune displays a flat playlist list.
    fn append(&mut self, page: Rootlist) -> Result<bool, TuneError> {
        let total = page
            .length
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("Spotify rootlist has no valid length")?;
        if total > MAX_ENTRIES {
            return Err(unsupported("personal library above 300 entries"));
        }
        if self.total.is_some_and(|previous| previous != total)
            || self
                .revision
                .as_ref()
                .is_some_and(|previous| previous.as_slice() != page.revision())
        {
            return Err("Spotify library changed during pagination; retry".into());
        }
        self.total = Some(total);
        let contents = page
            .contents
            .as_ref()
            .ok_or("Spotify rootlist is missing its contents")?;
        if usize::try_from(contents.pos()).ok() != Some(self.next) {
            return Err("Spotify rootlist page offset does not match the request".into());
        }
        let next = self.next + contents.items.len();
        if next > total
            || (contents.items.is_empty() && self.next < total)
            || (contents.truncated() && next == total)
        {
            return Err("Spotify rootlist is partial or pagination made no progress".into());
        }
        if next < total && page.revision().is_empty() {
            return Err("Spotify rootlist pagination requires a revision".into());
        }
        self.revision = Some(page.revision().to_vec());
        if !contents.meta_items.is_empty() && contents.meta_items.len() != contents.items.len() {
            return Err("Spotify rootlist playlist metadata is incomplete".into());
        }
        for (index, item) in contents.items.iter().enumerate() {
            let value = item.uri();
            if value.starts_with("spotify:start-group:") || value.starts_with("spotify:end-group:")
            {
                continue;
            }
            let uri = catalog::uri(value, "playlist")?;
            let id = uri
                .to_id()
                .map_err(|_| TuneError::from("Invalid Spotify playlist identifier"))?;
            let meta = contents.meta_items.get(index);
            // A denied decoration was also denied by an ordinary metadata
            // lookup on the real account. Do not repeatedly retry that denial
            // or silently omit the entry from the supposedly complete list.
            if meta.is_some_and(|meta| !matches!(meta.status_code(), 0 | 200)) {
                return Err(format!(
                    "Spotify rootlist contains an unavailable playlist (status {}, entry {}); no partial list returned",
                    meta.unwrap().status_code(), self.next + index
                ).into());
            }
            // Decorations are optional in the protocol and genuinely absent
            // for some entries on the paired account. Resolve those through
            // the ordinary playlist metadata route, never invent a name/count.
            let metadata = meta
                .filter(|meta| matches!(meta.status_code(), 0 | 200))
                .and_then(|meta| {
                    let attributes = meta.attributes.as_ref().filter(|a| !a.name().is_empty())?;
                    let track_count = meta.length.and_then(|n| u32::try_from(n).ok())?;
                    Some(StreamPlaylist {
                        id: id.clone(),
                        name: attributes.name().to_owned(),
                        description: attributes.description.clone(),
                        cover_path: attributes.picture_size.iter().find_map(|picture| {
                            picture
                                .url
                                .clone()
                                .filter(|url| url.starts_with("https://"))
                        }),
                        track_count,
                        owner: meta.owner_username.clone(),
                    })
                });
            if self.seen.insert(id.clone()) {
                self.playlists.push(PlaylistEntry { id, metadata });
            }
        }
        self.next = next;
        Ok(next == total)
    }
}

async fn resolve_entries<F, Fut>(
    entries: Vec<PlaylistEntry>,
    lookup: F,
) -> Result<Vec<StreamPlaylist>, TuneError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<StreamPlaylist, TuneError>>,
{
    stream::iter(entries.into_iter().map(|entry| {
        let lookup = &lookup;
        async move {
            match entry.metadata {
                Some(metadata) => Ok(metadata),
                None => lookup(entry.id).await,
            }
        }
    }))
    .buffered(4)
    .try_collect()
    .await
}

pub(super) async fn user_playlists(session: &Session) -> Result<Vec<StreamPlaylist>, TuneError> {
    let mut pages = LibraryPages::default();
    loop {
        let bytes = bounded(session.spclient().get_rootlist(pages.next, Some(PAGE_SIZE))).await?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Spotify rootlist response exceeds limit".into());
        }
        let page = Rootlist::parse_from_bytes(&bytes)
            .map_err(|_| TuneError::from("Spotify rootlist response is invalid"))?;
        if pages.append(page)? {
            return resolve_entries(pages.playlists, |id| async move {
                catalog::playlist(session, &id).await
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIRST: &str = "37i9dQZF1DXcBWIGoYBM5M";
    const SECOND: &str = "37i9dQZF1DX4sWSpwq3LiO";

    fn page(total: i32, offset: i32, uris: &[String]) -> Rootlist {
        let mut page = Rootlist::new();
        page.length = Some(total);
        page.revision = Some(vec![1; 16]);
        let contents = page.contents.mut_or_insert_default();
        contents.pos = Some(offset);
        contents.truncated = Some(offset + (uris.len() as i32) < total);
        for (index, uri) in uris.iter().enumerate() {
            contents.items.push(Default::default());
            contents.items.last_mut().unwrap().uri = Some(uri.clone());
            contents.meta_items.push(Default::default());
            let meta = contents.meta_items.last_mut().unwrap();
            meta.length = Some(42);
            meta.status_code = Some(200);
            meta.owner_username = Some("fixture-owner".into());
            meta.attributes.mut_or_insert_default().name =
                Some(format!("Playlist {offset}-{index}"));
        }
        page
    }

    #[test]
    fn native_library_paginates_folders_and_keeps_playlist_order() {
        let mut pages = LibraryPages::default();
        let first = page(
            5,
            0,
            &[
                "spotify:start-group:folder:Folder".into(),
                format!("spotify:playlist:{FIRST}"),
            ],
        );
        // Real protobuf encode/decode, not a parallel JSON parser.
        let first = Rootlist::parse_from_bytes(&first.write_to_bytes().unwrap()).unwrap();
        assert!(!pages.append(first).unwrap());
        assert_eq!(
            pages.next, 2,
            "Folder entries must count in Spotify rootlist offsets"
        );
        assert!(
            pages
                .append(page(
                    5,
                    2,
                    &[
                        format!("spotify:playlist:{SECOND}"),
                        "spotify:end-group:folder".into(),
                        format!("spotify:playlist:{FIRST}"),
                    ]
                ))
                .unwrap()
        );
        assert_eq!(
            pages
                .playlists
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            [FIRST, SECOND],
            "Personal playlists must be returned in rootlist order without folder duplicates"
        );
        let metadata = pages.playlists[0].metadata.as_ref().unwrap();
        assert_eq!(metadata.track_count, 42);
        assert_eq!(metadata.owner.as_deref(), Some("fixture-owner"));
    }

    #[test]
    fn native_library_refuses_changed_or_partial_pages() {
        for defect in [
            "revision",
            "length",
            "offset",
            "empty",
            "metadata",
            "forbidden",
        ] {
            let mut pages = LibraryPages::default();
            pages
                .append(page(2, 0, &[format!("spotify:playlist:{FIRST}")]))
                .unwrap();
            let mut second = page(2, 1, &[format!("spotify:playlist:{SECOND}")]);
            match defect {
                "revision" => second.revision = Some(vec![2; 16]),
                "length" => second.length = Some(3),
                "offset" => second.contents.as_mut().unwrap().pos = Some(0),
                "empty" => second.contents.as_mut().unwrap().items.clear(),
                "metadata" => second
                    .contents
                    .as_mut()
                    .unwrap()
                    .meta_items
                    .push(Default::default()),
                "forbidden" => {
                    second.contents.as_mut().unwrap().meta_items[0].status_code = Some(403)
                }
                _ => unreachable!(),
            }
            assert!(
                pages.append(second).is_err(),
                "Spotify personal library must refuse {defect}, not return a partial collection"
            );
        }
    }

    #[test]
    fn native_library_accepts_empty_but_rejects_unbounded_or_unknown_items() {
        assert!(LibraryPages::default().append(page(0, 0, &[])).unwrap());
        assert!(LibraryPages::default().append(page(301, 0, &[])).is_err());
        assert!(
            LibraryPages::default()
                .append(page(1, 0, &["https://attacker.invalid/playlist".into()]))
                .is_err()
        );
        let mut malformed = page(0, 0, &[]);
        malformed.length = None;
        assert!(LibraryPages::default().append(malformed).is_err());
    }

    #[tokio::test]
    async fn native_library_resolves_optional_decorations_without_inventing_metadata() {
        for missing in ["name", "all"] {
            let mut pages = LibraryPages::default();
            let mut first = page(2, 0, &[format!("spotify:playlist:{FIRST}")]);
            if missing == "name" {
                first.contents.as_mut().unwrap().meta_items[0]
                    .attributes
                    .mut_or_insert_default()
                    .name = None;
            } else {
                first.contents.as_mut().unwrap().meta_items.clear();
            }
            pages
                .append(first)
                .expect("Spotify rootlist decorations are optional, not a missing playlist");
            pages
                .append(page(2, 1, &[format!("spotify:playlist:{SECOND}")]))
                .unwrap();
            let calls = std::sync::Mutex::new(Vec::new());
            let resolved = resolve_entries(pages.playlists, |id| {
                calls.lock().unwrap().push(id.clone());
                async move {
                    Ok(StreamPlaylist {
                        id,
                        name: "Resolved fixture".into(),
                        description: None,
                        cover_path: None,
                        track_count: 17,
                        owner: None,
                    })
                }
            })
            .await
            .unwrap();
            assert_eq!(
                *calls.lock().unwrap(),
                [FIRST],
                "Only missing decorations need a playlist lookup"
            );
            assert_eq!(resolved[0].name, "Resolved fixture");
            assert_eq!(
                resolved[0].track_count, 17,
                "A missing playlist length must not become zero"
            );
            assert_eq!(resolved[1].id, SECOND);
        }
        let mut pages = LibraryPages::default();
        let mut first = page(1, 0, &[format!("spotify:playlist:{FIRST}")]);
        first.contents.as_mut().unwrap().meta_items.clear();
        pages.append(first).unwrap();
        assert!(
            resolve_entries(pages.playlists, |_| async {
                Err("fixture metadata unavailable".into())
            })
            .await
            .is_err(),
            "A failed metadata lookup must not return a silently shortened library"
        );
    }
}
