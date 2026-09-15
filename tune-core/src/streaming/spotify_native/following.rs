//! Followed artists from the native profile, checked against its total count.
use std::collections::HashSet;

use librespot_core::{Session, SpotifyUri};
use serde::Deserialize;

use super::{bounded, catalog, unsupported};
use crate::TuneError;

const MAX_FOLLOWING: usize = 1000;

#[derive(Clone, Copy)]
enum Read {
    Profile,
    Following,
}

#[derive(Deserialize)]
struct Profile {
    following_count: usize,
}

#[derive(Deserialize)]
struct Following {
    #[serde(default)]
    profiles: Vec<Followed>,
}

#[derive(Deserialize)]
struct Followed {
    uri: String,
    is_following: bool,
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, TuneError> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Spotify following response exceeds limit".into());
    }
    serde_json::from_slice(bytes)
        .map_err(|_| TuneError::from("Spotify following response is invalid"))
}

async fn read_artists<F, Fut>(fetch: F) -> Result<Vec<SpotifyUri>, TuneError>
where
    F: Fn(Read) -> Fut,
    Fut: std::future::Future<Output = Result<bytes::Bytes, TuneError>>,
{
    let before: Profile = decode(&fetch(Read::Profile).await?)?;
    if before.following_count > MAX_FOLLOWING {
        return Err(unsupported(
            "profiles following more than 1000 artists or users",
        ));
    }
    let following: Following = decode(&fetch(Read::Following).await?)?;
    if following.profiles.len() != before.following_count {
        return Err("Spotify following list is incomplete or changed; retry".into());
    }
    let mut seen = HashSet::new();
    let mut artists = Vec::new();
    for item in following.profiles {
        if !item.is_following || !seen.insert(item.uri.clone()) {
            return Err("Spotify following list contains duplicate or unfollowed profiles".into());
        }
        if item.uri.starts_with("spotify:artist:") {
            artists.push(catalog::uri(&item.uri, "artist")?);
        } else if item
            .uri
            .strip_prefix("spotify:user:")
            .is_none_or(str::is_empty)
        {
            return Err("Spotify following list contains an unexpected profile type".into());
        }
    }
    let after: Profile = decode(&fetch(Read::Profile).await?)?;
    if after.following_count != before.following_count {
        return Err("Spotify following count changed during reading; retry".into());
    }
    Ok(artists)
}

pub(super) async fn artist_uris(session: &Session) -> Result<Vec<SpotifyUri>, TuneError> {
    let username = urlencoding::encode(&session.username()).into_owned();
    read_artists(|read| {
        let username = &username;
        async move {
            match read {
                Read::Profile => {
                    bounded(
                        session
                            .spclient()
                            .get_user_profile(username, Some(0), Some(0)),
                    )
                    .await
                }
                Read::Following => bounded(session.spclient().get_user_following(username)).await,
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn bytes(v: Value) -> bytes::Bytes {
        serde_json::to_vec(&v).unwrap().into()
    }
    fn artist(n: usize) -> Value {
        json!({"uri":format!("spotify:artist:{n:022}"),"is_following":true})
    }

    #[tokio::test]
    async fn native_following_keeps_artists_but_not_users_and_verifies_the_total() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result=read_artists(|read| {
            calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            async move { Ok(bytes(match read {
                Read::Profile=>json!({"following_count":3}),
                Read::Following=>json!({"profiles":[artist(2),json!({"uri":"spotify:user:fixture","is_following":true}),artist(1)]}),
            })) }
        }).await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|u| u.to_uri().unwrap())
                .collect::<Vec<_>>(),
            vec![
                artist(2)["uri"].as_str().unwrap(),
                artist(1)["uri"].as_str().unwrap()
            ],
            "Followed users must not become artists; native artist order must be preserved"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "Verify following count both before and after reading the list"
        );
    }

    #[tokio::test]
    async fn native_following_rejects_truncation_mutation_duplicates_and_false_follows() {
        for defect in [
            "truncated",
            "changed",
            "duplicate",
            "unfollowed",
            "type",
            "count",
            "network",
        ] {
            let profiles = std::sync::atomic::AtomicUsize::new(0);
            let result=read_artists(|read| {
                let call=if matches!(read,Read::Profile) { profiles.fetch_add(1,std::sync::atomic::Ordering::SeqCst) } else {0};
                async move {
                    if defect=="network" { return Err("fixture HTTP 429".into()); }
                    Ok(bytes(match read {
                        Read::Profile if defect=="count"=>json!({}),
                        Read::Profile=>json!({"following_count":if (defect=="changed" && call>0) || defect=="duplicate" {2}else{1}}),
                        Read::Following=>json!({"profiles":match defect {
                            "truncated"=>vec![], "duplicate"=>vec![artist(1),artist(1)],
                            "unfollowed"=>vec![json!({"uri":artist(1)["uri"],"is_following":false})],
                            "type"=>vec![json!({"uri":"spotify:album:0000000000000000000001","is_following":true})],
                            _=>vec![artist(1)],
                        }}),
                    }))
                }
            }).await;
            assert!(
                result.is_err(),
                "Following must reject {defect}, never silently return a partial or invented artist list"
            );
        }
    }

    #[tokio::test]
    async fn native_following_empty_and_oversized_accounts_are_distinct() {
        assert!(
            read_artists(|r| async move {
                Ok(bytes(match r {
                    Read::Profile => json!({"following_count":0}),
                    Read::Following => json!({"profiles":[]}),
                }))
            })
            .await
            .unwrap()
            .is_empty()
        );
        let result = read_artists(|r| async move {
            assert!(
                matches!(r, Read::Profile),
                "Oversized following must fail before loading a partial page"
            );
            Ok(bytes(json!({"following_count":1001})))
        })
        .await;
        assert!(
            matches!(result, Err(TuneError::Unsupported(_))),
            "Following above the explicit limit must not be truncated"
        );
    }
}
