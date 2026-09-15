//! Read-only bulk metadata: bounded batches, identity checks, stable queue order.
use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, TryStreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, Track};
use librespot_protocol::{
    extended_metadata::{
        BatchedEntityRequest, BatchedExtensionResponse, EntityRequest, ExtensionQuery,
    },
    extension_kind::ExtensionKind,
};
use protobuf::{EnumOrUnknown, Message};

use super::{bounded, catalog, collections::MAX_TRACKS, unsupported};
use crate::{TuneError, streaming::StreamTrack};

const BATCH_SIZE: usize = 50;

fn request(uris: &[SpotifyUri]) -> Result<BatchedEntityRequest, TuneError> {
    let mut seen = HashSet::new();
    let mut entity_request = Vec::new();
    for uri in uris {
        if seen.insert(uri.clone()) {
            entity_request.push(EntityRequest {
                entity_uri: uri
                    .to_uri()
                    .map_err(|_| TuneError::from("Invalid Spotify track identifier"))?,
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
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
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Spotify metadata response exceeds limit".into());
    }
    let reply = BatchedExtensionResponse::parse_from_bytes(bytes)
        .map_err(|_| TuneError::from("Spotify metadata response is invalid"))?;
    let expected: HashSet<_> = uris.iter().cloned().collect();
    let mut tracks = HashMap::new();
    for group in reply.extended_metadata {
        if group.extension_kind.enum_value_or_default() != ExtensionKind::TRACK_V4
            || !matches!(group.header.get_or_default().provider_error_status, 0 | 200)
        {
            return Err("Spotify track metadata provider failed".into());
        }
        for entry in group.extension_data {
            if !matches!(entry.header.get_or_default().status_code, 0 | 200) {
                return Err(
                    "Spotify track metadata unavailable; no partial collection returned".into(),
                );
            }
            let uri = catalog::uri(&entry.entity_uri, "track")?;
            if !expected.contains(&uri) || tracks.contains_key(&uri) {
                return Err("Spotify metadata contains an unexpected or duplicate track".into());
            }
            let data = entry
                .extension_data
                .as_ref()
                .ok_or("Spotify track metadata is missing")?;
            let message = <Track as Metadata>::Message::parse_from_bytes(&data.value)
                .map_err(|_| TuneError::from("Spotify track metadata is invalid"))?;
            let track = Track::parse(&message, &uri)
                .map_err(|_| TuneError::from("Spotify track metadata cannot be decoded"))?;
            if track.id != uri {
                return Err("Spotify track metadata identity mismatch".into());
            }
            tracks.insert(uri, catalog::map_track(track));
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
    if uris.len() > MAX_TRACKS {
        return Err(unsupported("collections above 2000 tracks"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{
        entity_extension_data::EntityExtensionData, extended_metadata::EntityExtensionDataArray,
    };

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
