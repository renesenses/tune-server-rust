use std::collections::HashSet;

use futures_util::{StreamExt, TryStreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Album, Artist, Metadata, Playlist, Track, image::Images};

use super::{bounded, unsupported};
use crate::{TuneError, streaming::traits::*};

const MAX_COLLECTION: usize = 300;

/// Only Spotify entities, never an arbitrary URL fetched with session tokens.
pub(super) fn uri(id: &str, kind: &str) -> Result<SpotifyUri, TuneError> {
    let value = if id.starts_with("spotify:") {
        id.to_owned()
    } else if let Some(path) = id.strip_prefix("https://open.spotify.com/") {
        let path = path.split(['?', '#']).next().unwrap_or(path);
        let (actual_kind, id) = path.split_once('/').ok_or("Invalid Spotify link")?;
        format!("spotify:{actual_kind}:{id}")
    } else {
        format!("spotify:{kind}:{id}")
    };
    let raw_id = value.rsplit(':').next().unwrap_or_default();
    if raw_id.len() != 22 || !raw_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Invalid Spotify identifier length or alphabet".into());
    }
    let uri = SpotifyUri::from_uri(&value)
        .map_err(|_| TuneError::from("Invalid Spotify identifier".to_owned()))?;
    if uri.item_type() != kind || uri.to_id().map_or(true, |id| id.len() != 22) {
        return Err("Unexpected Spotify entity type or identifier".into());
    }
    Ok(uri)
}

fn id(uri: &SpotifyUri) -> String {
    uri.to_id().unwrap_or_default()
}

fn cover(images: &Images) -> Option<String> {
    images
        .iter()
        .max_by_key(|i| i.width)
        .map(|i| format!("https://i.scdn.co/image/{}", i.id))
}

pub(super) fn map_track(t: Track) -> StreamTrack {
    let artist = t.artists.first();
    StreamTrack {
        id: id(&t.id),
        title: t.name,
        artist: artist.map(|a| a.name.clone()).unwrap_or_default(),
        artist_id: artist.map(|a| id(&a.id)),
        album: Some(t.album.name),
        album_id: Some(id(&t.album.id)),
        duration_ms: t.duration.max(0) as u64,
        cover_path: cover(&t.album.covers),
        track_number: u32::try_from(t.number).ok(),
        disc_number: u32::try_from(t.disc_number).ok(),
        explicit: t.is_explicit,
        // Metadata lists possible formats, not the one the player will get.
        quality: None,
        isrc: None,
        composer: None,
    }
}

fn map_album(a: Album) -> StreamAlbum {
    StreamAlbum {
        id: id(&a.id),
        title: a.name.clone(),
        artist: a
            .artists
            .first()
            .map(|a| a.name.clone())
            .unwrap_or_default(),
        artist_id: a.artists.first().map(|a| id(&a.id)),
        cover_path: cover(&a.covers),
        year: u32::try_from(a.date.year()).ok(),
        track_count: a.tracks().count() as u32,
        quality: None,
    }
}

pub(super) async fn track(session: &Session, track_id: &str) -> Result<StreamTrack, TuneError> {
    Ok(map_track(
        bounded(Track::get(session, &uri(track_id, "track")?)).await?,
    ))
}

pub(super) async fn album(session: &Session, album_id: &str) -> Result<StreamAlbum, TuneError> {
    Ok(map_album(
        bounded(Album::get(session, &uri(album_id, "album")?)).await?,
    ))
}

async fn tracks(session: &Session, uris: Vec<SpotifyUri>) -> Result<Vec<StreamTrack>, TuneError> {
    super::metadata::tracks(session, uris).await
}

pub(super) async fn album_tracks(
    session: &Session,
    album_id: &str,
) -> Result<Vec<StreamTrack>, TuneError> {
    let album = bounded(Album::get(session, &uri(album_id, "album")?)).await?;
    tracks(session, album.tracks().cloned().collect()).await
}

pub(super) async fn artist(session: &Session, artist_id: &str) -> Result<StreamArtist, TuneError> {
    let a = bounded(Artist::get(session, &uri(artist_id, "artist")?)).await?;
    Ok(StreamArtist {
        id: id(&a.id),
        name: a.name,
        image_path: cover(&a.portraits),
        bio: None,
    })
}

pub(super) async fn artist_albums(
    session: &Session,
    artist_id: &str,
) -> Result<Vec<StreamAlbum>, TuneError> {
    let artist = bounded(Artist::get(session, &uri(artist_id, "artist")?)).await?;
    let uris: Vec<_> = artist.albums_current().cloned().collect();
    if uris.len() > MAX_COLLECTION {
        return Err(unsupported("discographies above 300 albums"));
    }
    stream::iter(
        uris.into_iter()
            .map(|uri| async move { Ok(map_album(bounded(Album::get(session, &uri)).await?)) }),
    )
    .buffered(4)
    .try_collect()
    .await
}

pub(super) async fn playlist(
    session: &Session,
    playlist_id: &str,
) -> Result<StreamPlaylist, TuneError> {
    let p = bounded(Playlist::get(session, &uri(playlist_id, "playlist")?)).await?;
    let owner = match &p.id {
        SpotifyUri::Playlist { user, .. } => user.clone(),
        _ => None,
    };
    Ok(StreamPlaylist {
        id: id(&p.id),
        name: p.attributes.name,
        description: Some(p.attributes.description),
        cover_path: p.attributes.picture_sizes.first().map(|i| i.url.clone()),
        track_count: p.length.max(0) as u32,
        owner,
    })
}

pub(super) async fn playlist_tracks(
    session: &Session,
    playlist_id: &str,
) -> Result<Vec<StreamTrack>, TuneError> {
    let uris = super::collections::playlist_uris(session, playlist_id).await?;
    tracks(session, uris).await
}

pub(super) fn search_uri(query: &str) -> Result<String, TuneError> {
    let query = query.trim();
    if query.is_empty() || query.len() > 512 {
        return Err("Spotify search must contain 1–512 bytes".into());
    }
    // get_context embeds this inside an HTTP path. Escape slashes, ?, # and
    // percent as data; only spaces use Spotify's documented '+' convention.
    Ok(format!(
        "spotify:search:{}",
        urlencoding::encode(query).replace("%20", "+")
    ))
}

pub(super) async fn search(
    session: &Session,
    query: &str,
    limit: usize,
) -> Result<SearchResults, TuneError> {
    let mut result = SearchResults {
        tracks: vec![],
        albums: vec![],
        artists: vec![],
        playlists: vec![],
    };
    // Pasting a Spotify link also gives a deterministic route to an album or
    // playlist while free-text search remains an experimental track context.
    if query.starts_with("spotify:") || query.starts_with("https://open.spotify.com/") {
        if uri(query, "track").is_ok() {
            result.tracks.push(track(session, query).await?);
        } else if uri(query, "album").is_ok() {
            result.albums.push(album(session, query).await?);
        } else if uri(query, "artist").is_ok() {
            result.artists.push(artist(session, query).await?);
        } else if uri(query, "playlist").is_ok() {
            result.playlists.push(playlist(session, query).await?);
        } else {
            return Err("Unsupported Spotify link".into());
        }
        return Ok(result);
    }
    let context = bounded(session.spclient().get_context(&search_uri(query)?)).await?;
    if context.pages.is_empty() {
        return Err(unsupported("search context returned no pages"));
    }
    let mut seen = HashSet::new();
    let mut uris = Vec::new();
    let limit = if limit == 0 { 30 } else { limit.min(30) };
    for page in &context.pages {
        for item in &page.tracks {
            let value = if !item.uri().is_empty() {
                uri(item.uri(), "track")?
            } else {
                SpotifyUri::Track {
                    id: librespot_core::SpotifyId::from_raw(item.gid()).map_err(|_| {
                        TuneError::from("Spotify search track has no valid identifier".to_owned())
                    })?,
                }
            };
            if seen.insert(value.clone()) {
                uris.push(value);
            }
            if uris.len() == limit {
                break;
            }
        }
        if uris.len() == limit {
            break;
        }
    }
    if uris.is_empty() && context.pages.iter().any(|p| !p.page_url().is_empty()) {
        return Err(unsupported("search context requires page resolution"));
    }
    result.tracks = tracks(session, uris).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_identifiers_reject_wrong_kind_and_external_hosts() {
        let valid = "4uLU6hMCjMI75M1A2tKUQC";
        assert!(uri(valid, "track").is_ok());
        assert!(
            uri(
                &format!("https://open.spotify.com/track/{valid}?si=test"),
                "track"
            )
            .is_ok()
        );
        assert!(uri(&format!("spotify:album:{valid}"), "track").is_err());
        assert!(uri("https://attacker.invalid/track/secret", "track").is_err());
        assert!(uri("x", "track").is_err());
    }
    #[test]
    fn native_search_query_cannot_change_request_path() {
        assert_eq!(
            search_uri(" hello world ").unwrap(),
            "spotify:search:hello+world"
        );
        assert_eq!(
            search_uri("a/b?c#d+e").unwrap(),
            "spotify:search:a%2Fb%3Fc%23d%2Be"
        );
        assert!(search_uri("  ").is_err());
        assert!(search_uri(&"x".repeat(513)).is_err());
    }
}
