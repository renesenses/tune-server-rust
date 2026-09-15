//! Read-only bulk metadata: bounded batches, identity checks, stable queue order.
use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, TryStreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Album, Artist, Metadata, Track};
use librespot_protocol::{
    extended_metadata::{
        BatchedEntityRequest, BatchedExtensionResponse, EntityRequest, ExtensionQuery,
    },
    extension_kind::ExtensionKind,
};
use protobuf::{EnumOrUnknown, Message};

use super::{bounded, catalog, collections::MAX_TRACKS, unsupported};
use crate::{
    TuneError,
    streaming::{StreamAlbum, StreamArtist, StreamTrack},
};

const BATCH_SIZE: usize = 50;

fn request(uris: &[SpotifyUri]) -> Result<BatchedEntityRequest, TuneError> {
    entity_request(uris, ExtensionKind::TRACK_V4)
}

fn entity_request(
    uris: &[SpotifyUri],
    kind: ExtensionKind,
) -> Result<BatchedEntityRequest, TuneError> {
    let mut seen = HashSet::new();
    let mut entity_request = Vec::new();
    for uri in uris {
        if seen.insert(uri.clone()) {
            entity_request.push(EntityRequest {
                entity_uri: uri
                    .to_uri()
                    .map_err(|_| TuneError::from("Invalid Spotify metadata identifier"))?,
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(kind),
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
    }
    Ok(BatchedEntityRequest {
        entity_request,
        ..Default::default()
    })
}

fn decode(uris: &[SpotifyUri], bytes: &[u8]) -> Result<Vec<StreamTrack>, TuneError> {
    decode_entities(
        uris,
        bytes,
        ExtensionKind::TRACK_V4,
        "track",
        |data, uri| {
            let message = <Track as Metadata>::Message::parse_from_bytes(data)
                .map_err(|_| TuneError::from("Spotify track metadata is invalid"))?;
            let track = Track::parse(&message, uri)
                .map_err(|_| TuneError::from("Spotify track metadata cannot be decoded"))?;
            if track.id != *uri {
                return Err("Spotify track metadata identity mismatch".into());
            }
            Ok(catalog::map_track(track))
        },
    )
}

fn decode_entities<T: Clone>(
    uris: &[SpotifyUri],
    bytes: &[u8],
    kind: ExtensionKind,
    name: &str,
    parse: impl Fn(&[u8], &SpotifyUri) -> Result<T, TuneError>,
) -> Result<Vec<T>, TuneError> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Spotify metadata response exceeds limit".into());
    }
    let reply = BatchedExtensionResponse::parse_from_bytes(bytes)
        .map_err(|_| TuneError::from("Spotify metadata response is invalid"))?;
    let expected: HashSet<_> = uris.iter().cloned().collect();
    let mut tracks = HashMap::new();
    for group in reply.extended_metadata {
        if group.extension_kind.enum_value().ok() != Some(kind)
            || !matches!(group.header.get_or_default().provider_error_status, 0 | 200)
        {
            return Err("Spotify metadata provider failed".into());
        }
        for entry in group.extension_data {
            if !matches!(entry.header.get_or_default().status_code, 0 | 200) {
                return Err("Spotify metadata unavailable; no partial collection returned".into());
            }
            let uri = catalog::uri(&entry.entity_uri, name)?;
            if !expected.contains(&uri) || tracks.contains_key(&uri) {
                return Err("Spotify metadata contains an unexpected or duplicate entity".into());
            }
            let data = entry
                .extension_data
                .as_ref()
                .ok_or("Spotify metadata is missing")?;
            let entity = parse(&data.value, &uri)?;
            tracks.insert(uri, entity);
        }
    }
    uris.iter()
        .map(|uri| {
            tracks.get(uri).cloned().ok_or_else(|| {
                TuneError::from("Spotify metadata is incomplete; no partial collection returned")
            })
        })
        .collect()
}

async fn resolve<F, Fut>(uris: Vec<SpotifyUri>, fetch: F) -> Result<Vec<StreamTrack>, TuneError>
where
    F: Fn(BatchedEntityRequest) -> Fut + Sync,
    Fut: std::future::Future<Output = Result<bytes::Bytes, TuneError>> + Send,
{
    resolve_entities(uris, request, decode, fetch).await
}

async fn resolve_entities<T, F, Fut>(
    uris: Vec<SpotifyUri>,
    request: fn(&[SpotifyUri]) -> Result<BatchedEntityRequest, TuneError>,
    decode: fn(&[SpotifyUri], &[u8]) -> Result<Vec<T>, TuneError>,
    fetch: F,
) -> Result<Vec<T>, TuneError>
where
    T: Send,
    F: Fn(BatchedEntityRequest) -> Fut + Sync,
    Fut: std::future::Future<Output = Result<bytes::Bytes, TuneError>> + Send,
{
    if uris.len() > MAX_TRACKS {
        return Err(unsupported("collections above 2000 items"));
    }
    let chunks: Vec<_> = uris
        .chunks(BATCH_SIZE)
        .map(<[SpotifyUri]>::to_vec)
        .collect();
    let batches = stream::iter(chunks.into_iter().map(|chunk| {
        let response = request(&chunk).map(&fetch);
        async move { decode(&chunk, &response?.await?) }
    }))
    .buffered(4)
    .try_collect::<Vec<_>>()
    .await?;
    Ok(batches.into_iter().flatten().collect())
}

pub(super) async fn tracks(
    session: &Session,
    uris: Vec<SpotifyUri>,
) -> Result<Vec<StreamTrack>, TuneError> {
    resolve(uris, |request| async move {
        bounded(session.spclient().request_with_protobuf(
            &reqwest::Method::POST,
            "/extended-metadata/v0/extended-metadata",
            None,
            &request,
        ))
        .await
    })
    .await
}

fn decode_albums(uris: &[SpotifyUri], bytes: &[u8]) -> Result<Vec<StreamAlbum>, TuneError> {
    decode_entities(
        uris,
        bytes,
        ExtensionKind::ALBUM_V4,
        "album",
        |data, uri| {
            let message = <Album as Metadata>::Message::parse_from_bytes(data)
                .map_err(|_| TuneError::from("Spotify album metadata is invalid"))?;
            let album = Album::parse(&message, uri)
                .map_err(|_| TuneError::from("Spotify album metadata cannot be decoded"))?;
            if album.id != *uri {
                return Err("Spotify album metadata identity mismatch".into());
            }
            Ok(catalog::map_album(album))
        },
    )
}

fn decode_artists(uris: &[SpotifyUri], bytes: &[u8]) -> Result<Vec<StreamArtist>, TuneError> {
    decode_entities(
        uris,
        bytes,
        ExtensionKind::ARTIST_V4,
        "artist",
        |data, uri| {
            let message = <Artist as Metadata>::Message::parse_from_bytes(data)
                .map_err(|_| TuneError::from("Spotify artist metadata is invalid"))?;
            let artist = Artist::parse(&message, uri)
                .map_err(|_| TuneError::from("Spotify artist metadata cannot be decoded"))?;
            if artist.id != *uri {
                return Err("Spotify artist metadata identity mismatch".into());
            }
            Ok(catalog::map_artist(artist))
        },
    )
}

pub(super) async fn albums(
    session: &Session,
    uris: Vec<SpotifyUri>,
) -> Result<Vec<StreamAlbum>, TuneError> {
    resolve_entities(
        uris,
        |uris| entity_request(uris, ExtensionKind::ALBUM_V4),
        decode_albums,
        |request| async move {
            bounded(session.spclient().request_with_protobuf(
                &reqwest::Method::POST,
                "/extended-metadata/v0/extended-metadata",
                None,
                &request,
            ))
            .await
        },
    )
    .await
}

pub(super) async fn artists(
    session: &Session,
    uris: Vec<SpotifyUri>,
) -> Result<Vec<StreamArtist>, TuneError> {
    resolve_entities(
        uris,
        |uris| entity_request(uris, ExtensionKind::ARTIST_V4),
        decode_artists,
        |request| async move {
            bounded(session.spclient().request_with_protobuf(
                &reqwest::Method::POST,
                "/extended-metadata/v0/extended-metadata",
                None,
                &request,
            ))
            .await
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{
        entity_extension_data::EntityExtensionData, extended_metadata::EntityExtensionDataArray,
    };

    fn entity_reply(request: BatchedEntityRequest) -> BatchedExtensionResponse {
        let mut reply = BatchedExtensionResponse::new();
        let mut group = EntityExtensionDataArray::new();
        for entry in request.entity_request.into_iter().rev() {
            group.extension_kind = entry.query[0].extension_kind;
            let name = if group.extension_kind.enum_value().unwrap() == ExtensionKind::ALBUM_V4 {
                "album"
            } else {
                "artist"
            };
            let uri = catalog::uri(&entry.entity_uri, name).unwrap();
            let id = match uri {
                SpotifyUri::Album { id } | SpotifyUri::Artist { id } => id,
                _ => unreachable!(),
            };
            let value = if name == "album" {
                librespot_protocol::metadata::Album {
                    gid: Some(id.to_raw().to_vec()),
                    name: Some("Saved fixture album".into()),
                    date: Some(librespot_protocol::metadata::Date {
                        year: Some(2020),
                        month: Some(1),
                        day: Some(1),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                }
                .write_to_bytes()
                .unwrap()
            } else {
                librespot_protocol::metadata::Artist {
                    gid: Some(id.to_raw().to_vec()),
                    name: Some("Followed fixture artist".into()),
                    ..Default::default()
                }
                .write_to_bytes()
                .unwrap()
            };
            group.extension_data.push(EntityExtensionData {
                entity_uri: entry.entity_uri,
                extension_data: Some(protobuf::well_known_types::any::Any {
                    value,
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            });
        }
        reply.extended_metadata.push(group);
        reply
    }

    #[tokio::test]
    async fn native_saved_metadata_batches_albums_and_artists_with_their_real_entity_kind() {
        let albums: Vec<_> = (1u128..124)
            .map(|n| SpotifyUri::Album {
                id: librespot_core::SpotifyId::from_raw(&n.to_be_bytes()).unwrap(),
            })
            .collect();
        let artists: Vec<_> = (1u128..124)
            .map(|n| SpotifyUri::Artist {
                id: librespot_core::SpotifyId::from_raw(&n.to_be_bytes()).unwrap(),
            })
            .collect();
        let calls = std::sync::Mutex::new(Vec::new());
        let fetch = |r: BatchedEntityRequest| {
            calls.lock().unwrap().push(r.entity_request.len());
            async move { Ok(entity_reply(r).write_to_bytes().unwrap().into()) }
        };
        let a = resolve_entities(
            albums.clone(),
            |u| entity_request(u, ExtensionKind::ALBUM_V4),
            decode_albums,
            &fetch,
        )
        .await
        .expect("Saved albums require album metadata, not track metadata");
        let b = resolve_entities(
            artists.clone(),
            |u| entity_request(u, ExtensionKind::ARTIST_V4),
            decode_artists,
            &fetch,
        )
        .await
        .expect("Followed artists require artist metadata, not track metadata");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![50, 50, 23, 50, 50, 23],
            "Albums and artists must retain bounded metadata batches"
        );
        assert_eq!(
            a.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            albums
                .iter()
                .map(|u| u.to_id().unwrap())
                .collect::<Vec<_>>(),
            "Saved albums must retain every requested identity in order"
        );
        assert_eq!(
            b.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            artists
                .iter()
                .map(|u| u.to_id().unwrap())
                .collect::<Vec<_>>(),
            "Followed artists must retain every requested identity in order"
        );
        assert!(
            a.iter()
                .all(|e| e.title == "Saved fixture album" && e.year == Some(2020))
        );
        assert!(b.iter().all(|e| e.name == "Followed fixture artist"));
    }

    #[test]
    fn native_saved_metadata_refuses_missing_foreign_and_denied_entities() {
        for (name, kind) in [
            ("album", ExtensionKind::ALBUM_V4),
            ("artist", ExtensionKind::ARTIST_V4),
        ] {
            let uris = [catalog::uri("0000000000000000000001", name).unwrap()];
            for defect in ["missing", "identity", "denied", "kind"] {
                let mut reply = entity_reply(entity_request(&uris, kind).unwrap());
                let group = &mut reply.extended_metadata[0];
                match defect {
                    "missing" => group.extension_data.clear(),
                    "identity" => {
                        group.extension_data[0].entity_uri =
                            format!("spotify:{name}:0000000000000000000002")
                    }
                    "denied" => {
                        group.extension_data[0]
                            .header
                            .mut_or_insert_default()
                            .status_code = 403
                    }
                    "kind" => group.extension_kind = EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                    _ => unreachable!(),
                }
                let bytes = reply.write_to_bytes().unwrap();
                let rejected = if name == "album" {
                    decode_albums(&uris, &bytes).is_err()
                } else {
                    decode_artists(&uris, &bytes).is_err()
                };
                assert!(
                    rejected,
                    "Saved {name} metadata must reject {defect}, never return a shortened collection"
                );
            }
        }
    }

    fn uri(n: u128) -> SpotifyUri {
        SpotifyUri::Track {
            id: librespot_core::SpotifyId::from_raw(&n.to_be_bytes()).unwrap(),
        }
    }
    fn reply(request: BatchedEntityRequest) -> BatchedExtensionResponse {
        let mut group = EntityExtensionDataArray {
            extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
            ..Default::default()
        };
        // Deliberately reverse network order; the result must follow the request.
        for entry in request.entity_request.into_iter().rev() {
            let SpotifyUri::Track { id } = catalog::uri(&entry.entity_uri, "track").unwrap() else {
                unreachable!()
            };
            let metadata = librespot_protocol::metadata::Track {
                gid: Some(id.to_raw().to_vec()),
                name: Some("Fixture track".into()),
                duration: Some(123000),
                album: Some(librespot_protocol::metadata::Album {
                    gid: Some(vec![0; 16]),
                    name: Some("Fixture album".into()),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            };
            group.extension_data.push(EntityExtensionData {
                entity_uri: entry.entity_uri,
                extension_data: Some(protobuf::well_known_types::any::Any {
                    value: metadata.write_to_bytes().unwrap(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            });
        }
        BatchedExtensionResponse {
            extended_metadata: vec![group],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn native_metadata_batches_large_collections_without_reordering_or_dropping_duplicates() {
        let mut uris: Vec<_> = (1..1513).map(uri).collect();
        uris[1] = uri(1);
        let calls = std::sync::Mutex::new(Vec::new());
        let tracks = resolve(uris.clone(), |request| {
            calls.lock().unwrap().push(request.entity_request.len());
            async move { Ok(reply(request).write_to_bytes().unwrap().into()) }
        })
        .await
        .expect("Large collections must use batched metadata, not the old 300-track refusal");
        assert_eq!(
            calls.lock().unwrap().len(),
            31,
            "1512 tracks must use bounded metadata batches"
        );
        assert!(calls.lock().unwrap().iter().all(|&n| n <= BATCH_SIZE));
        assert_eq!(
            tracks.len(),
            1512,
            "Batched metadata must return every collection position"
        );
        assert_eq!(
            tracks.iter().map(|t| &t.id).collect::<Vec<_>>(),
            uris.iter()
                .map(|u| u.to_id().unwrap())
                .collect::<Vec<_>>()
                .iter()
                .collect::<Vec<_>>(),
            "Network order and deduplication must not change the original collection positions"
        );
    }

    #[test]
    fn native_metadata_refuses_missing_denied_duplicate_or_mismatched_tracks() {
        let uris = [uri(1), uri(2)];
        for defect in [
            "missing",
            "denied",
            "duplicate",
            "foreign",
            "identity",
            "payload",
            "provider",
            "kind",
        ] {
            let mut reply = reply(request(&uris).unwrap());
            let group = &mut reply.extended_metadata[0];
            match defect {
                "missing" => {
                    group.extension_data.pop();
                }
                "denied" => {
                    group.extension_data[0]
                        .header
                        .mut_or_insert_default()
                        .status_code = 403
                }
                "duplicate" => group.extension_data.push(group.extension_data[0].clone()),
                "foreign" => group.extension_data[0].entity_uri = uri(3).to_uri().unwrap(),
                "identity" => {
                    let data = group.extension_data[0].extension_data.as_mut().unwrap();
                    let mut track =
                        librespot_protocol::metadata::Track::parse_from_bytes(&data.value).unwrap();
                    track.gid = Some(3u128.to_be_bytes().to_vec());
                    data.value = track.write_to_bytes().unwrap();
                }
                "payload" => group.extension_data[0].extension_data = Default::default(),
                "provider" => group.header.mut_or_insert_default().provider_error_status = 503,
                "kind" => group.extension_kind = EnumOrUnknown::new(ExtensionKind::ALBUM_V4),
                _ => unreachable!(),
            }
            assert!(
                decode(&uris, &reply.write_to_bytes().unwrap()).is_err(),
                "Spotify metadata must refuse {defect}, never serve a shortened or incorrect collection"
            );
        }
    }

    #[tokio::test]
    async fn native_metadata_limits_and_network_errors_remain_explicit() {
        assert!(
            resolve(vec![], |_| async {
                panic!("An empty collection needs no metadata request")
            })
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            resolve(vec![uri(1); 2001], |_| async {
                panic!("Oversized collections must fail before network access")
            })
            .await
            .is_err()
        );
        assert!(
            resolve(vec![uri(1)], |_| async { Err("fixture HTTP 429".into()) })
                .await
                .is_err(),
            "A global metadata failure must not become an empty collection"
        );
        assert!(decode(&[uri(1)], &vec![0; 8 * 1024 * 1024 + 1]).is_err());
        assert!(decode(&[uri(1)], b"invalid").is_err());
    }
}
