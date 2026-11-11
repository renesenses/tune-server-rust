pub const LASTFM_API: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Debug, Clone, Default)]
pub struct LastfmTags {
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub bio: String,
}

pub async fn get_lastfm_tags(title: &str, artist: &str, api_key: &str) -> LastfmTags {
    if api_key.is_empty() || title.is_empty() {
        return LastfmTags::default();
    }

    let client = crate::http::client::shared();

    let track_data = client
        .get(LASTFM_API)
        .query(&[
            ("method", "track.getInfo"),
            ("track", title),
            ("artist", artist),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success());

    let track_json: serde_json::Value = match track_data {
        Some(r) => r.json().await.unwrap_or_default(),
        None => return LastfmTags::default(),
    };

    let tags: Vec<String> = track_json
        .pointer("/track/toptags/tag")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let genres = tags.iter().take(3).cloned().collect();

    let bio = get_artist_bio(client, artist, api_key).await;

    LastfmTags { genres, tags, bio }
}

async fn get_artist_bio(client: &reqwest::Client, artist: &str, api_key: &str) -> String {
    let resp = client
        .get(LASTFM_API)
        .query(&[
            ("method", "artist.getInfo"),
            ("artist", artist),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok();

    let data: serde_json::Value = match resp {
        Some(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => return String::new(),
    };

    let bio = data
        .pointer("/artist/bio/summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    strip_html(bio)
}

fn strip_html(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(s, "").trim().to_string()
}

pub struct LastfmEnricher {
    api_key: String,
}

impl LastfmEnricher {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
        }
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub async fn enrich_track(&self, title: &str, artist: &str) -> LastfmTags {
        get_lastfm_tags(title, artist, &self.api_key).await
    }

    pub async fn get_top_tags_for_artist(&self, artist: &str) -> Vec<String> {
        if self.api_key.is_empty() || artist.is_empty() {
            return vec![];
        }

        let client = crate::http::client::shared();
        let resp = client
            .get(LASTFM_API)
            .query(&[
                ("method", "artist.getTopTags"),
                ("artist", artist),
                ("api_key", &self.api_key),
                ("format", "json"),
            ])
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok();

        let data: serde_json::Value = match resp {
            Some(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return vec![],
        };

        data.pointer("/toptags/tag")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .take(10)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// #7107 — `artist.getTopTracks` : les titres les plus écoutés d'un artiste
/// sur Last.fm, du plus au moins populaire (l'ordre de la réponse).
///
/// #8350 — DidLyon (fil 2394) : les titres phares d'« Ange » et de
/// « Barbara » étaient ceux d'un homonyme. Last.fm était interrogé par NOM
/// avec `autocorrect=1`, jamais par MBID. Désormais :
/// - avec `mbid`, Last.fm est interrogé par MBID seul ; une réponse dont les
///   pistes portent le MBID d'un AUTRE artiste est refusée ;
/// - sans `mbid`, par nom SANS autocorrect ; une réponse qui désigne un autre
///   nom que celui demandé (redirection) est refusée.
///
/// Rend les NOMS seuls : Last.fm ne donne ni durée ni ISRC dans cette
/// réponse. `base` est l'URL de l'API (celle de Last.fm en production, un
/// faux site dans les tests). `Err` pour tout échec réseau, HTTP ou de
/// lecture, y compris l'erreur JSON `{"error": …}` que Last.fm rend avec un
/// statut 200 ; l'appelant se replie, il ne remonte rien à l'utilisateur.
pub async fn top_tracks_artiste(
    client: &reqwest::Client,
    base: &str,
    artist: &str,
    mbid: Option<&str>,
    api_key: &str,
    limit: usize,
) -> Result<Vec<String>, String> {
    let mbid = mbid.map(str::trim).filter(|m| !m.is_empty());
    if api_key.trim().is_empty() || (artist.trim().is_empty() && mbid.is_none()) {
        return Err("clé ou artiste absent".into());
    }
    let limit = limit.to_string();
    let qui = match mbid {
        Some(m) => ("mbid", m),
        None => ("artist", artist),
    };
    let resp = client
        .get(base)
        .query(&[
            ("method", "artist.getTopTracks"),
            qui,
            ("api_key", api_key),
            ("limit", limit.as_str()),
            ("autocorrect", "0"),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("statut {}", resp.status()));
    }
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if let Some(e) = data.get("error") {
        return Err(format!("erreur Last.fm {e}"));
    }
    if let Some(attendu) = mbid {
        let autre = data
            .pointer("/toptracks/track")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|t| t.pointer("/artist/mbid").and_then(|m| m.as_str()))
            .map(str::trim)
            .find(|m| !m.is_empty() && !m.eq_ignore_ascii_case(attendu));
        if let Some(autre) = autre {
            return Err(format!("homonyme : MBID {autre} au lieu de {attendu}"));
        }
    } else if let Some(rendu) = data
        .pointer("/toptracks/@attr/artist")
        .and_then(|v| v.as_str())
    {
        let cle = |t: &str| -> String {
            t.to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect()
        };
        if cle(rendu) != cle(artist) {
            return Err(format!("homonyme : « {rendu} » au lieu de « {artist} »"));
        }
    }
    Ok(data
        .pointer("/toptracks/track")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_basic() {
        assert_eq!(strip_html("Hello <b>world</b>!"), "Hello world!");
        assert_eq!(strip_html("<a href='x'>link</a>"), "link");
        assert_eq!(strip_html("no tags"), "no tags");
    }

    #[test]
    fn empty_key_returns_default() {
        let enricher = LastfmEnricher::new("");
        assert!(!enricher.has_key());
    }

    #[test]
    fn has_key() {
        let enricher = LastfmEnricher::new("abc123");
        assert!(enricher.has_key());
    }

    #[tokio::test]
    async fn no_key_no_tags() {
        let tags = get_lastfm_tags("Song", "Artist", "").await;
        assert!(tags.tags.is_empty());
        assert!(tags.genres.is_empty());
    }

    #[tokio::test]
    async fn no_title_no_tags() {
        let tags = get_lastfm_tags("", "Artist", "fake-key").await;
        assert!(tags.tags.is_empty());
    }
}
