use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

const CACHE_TTL_SECS: u64 = 3600;
const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr/api/v1/artists";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistData {
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

struct CacheEntry {
    data: ArtistData,
    fetched_at: Instant,
}

pub struct ArtistEnrichmentClient {
    base_url: String,
    cache: HashMap<String, CacheEntry>,
    /// Nom (minuscules) → MBID résolu, `None` mémorisé aussi. Une radio
    /// interroge le même artiste plusieurs fois de suite ; sans ça chaque
    /// graine coûterait deux appels réseau au lieu d'un.
    mbid_cache: HashMap<String, Option<String>>,
    timeout_secs: u64,
}

/// La liste utile d'une réponse, quelle que soit son enveloppe : un tableau nu,
/// `{"data":[…]}`, `{"data":{"<clé>":[…]}}` ou `{"<clé>":[…]}`.
fn extract_list(v: &serde_json::Value, keys: &[&str]) -> Vec<serde_json::Value> {
    if let Some(arr) = v.as_array() {
        return arr.clone();
    }
    let inner = v.get("data").unwrap_or(v);
    if let Some(arr) = inner.as_array() {
        return arr.clone();
    }
    for k in keys {
        if let Some(arr) = inner.get(k).and_then(|x| x.as_array()) {
            return arr.clone();
        }
    }
    vec![]
}

/// Forme d'un identifiant MusicBrainz : un UUID canonique.
fn is_mbid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

impl ArtistEnrichmentClient {
    pub fn new(base_url: Option<&str>, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            cache: HashMap::new(),
            mbid_cache: HashMap::new(),
            timeout_secs,
        }
    }

    pub async fn get_artist(&mut self, mbid: &str) -> Option<ArtistData> {
        if let Some(cached) = self.cache_get(mbid) {
            return Some(cached);
        }
        let data = self.request(&format!("/{mbid}")).await?;
        let mut artist = ArtistData { fields: data };

        if let Some(inner) = artist.fields.get_mut("data")
            && let Some(img) = inner.get("image_url").and_then(|v| v.as_str())
            && img.starts_with("/storage/")
        {
            let base = self
                .base_url
                .split("/api/")
                .next()
                .unwrap_or(&self.base_url);
            let full = format!("{base}{img}");
            inner["image_url"] = serde_json::json!(full);
        }

        self.cache_set(mbid, artist.clone());
        Some(artist)
    }

    pub async fn get_bio(&mut self, mbid: &str, lang: &str) -> Option<String> {
        let data = self
            .request_with_params(&format!("/{mbid}/bio"), &[("lang", lang)])
            .await?;
        data.get("bio")
            .or_else(|| data.get("text"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub async fn get_similar(&mut self, mbid: &str) -> Vec<serde_json::Value> {
        // L'API rend `{"data":{"musicbrainz_id":…,"similar_artists":[…]}}`.
        // On ne cherchait que la clé `artists`, absente : la liste ressortait
        // vide même contre le bon hôte, et l'appelant ne pouvait pas
        // distinguer « personne ne ressemble » de « mauvaise question » (#1730).
        match self.request(&format!("/{mbid}/similar")).await {
            Some(v) => extract_list(&v, &["similar_artists", "artists"]),
            None => vec![],
        }
    }

    pub async fn search(&mut self, query: &str) -> Vec<serde_json::Value> {
        // `{"data":[…],"count":n}` — même correction d'enveloppe.
        match self.request_with_params("/search", &[("q", query)]).await {
            Some(v) => extract_list(&v, &["artists"]),
            None => vec![],
        }
    }

    /// Résout un NOM d'artiste en identifiant MusicBrainz.
    ///
    /// `get_similar` est indexée par MBID. Une piste en streaming n'en porte
    /// aucun, et environ neuf artistes locaux sur dix non plus : la source
    /// répondait donc « personne » à chaque fois. L'API sait pourtant résoudre
    /// un nom — `/search?q=` — mais le client ne l'enchaînait jamais (#1730).
    ///
    /// Seule une correspondance EXACTE est acceptée. `search?q=Caravan` rend
    /// aussi « La Caravane Electro » ; bâtir la radio dessus donnerait un
    /// voisinage sans rapport. Même règle que `auto_dj::pick_seed_artist_id`.
    pub async fn resolve_mbid(&mut self, name: &str) -> Option<String> {
        let wanted = name.trim();
        if wanted.is_empty() {
            return None;
        }
        if let Some(cached) = self.mbid_cache.get(&wanted.to_lowercase()) {
            return cached.clone();
        }
        let found = self
            .search(wanted)
            .await
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.trim().eq_ignore_ascii_case(wanted))
            })
            .and_then(|a| a.get("musicbrainz_id"))
            .and_then(|v| v.as_str())
            // L'API renvoie l'entrée telle quelle quand elle ne trouve rien :
            // `/artists/Pink Floyd/similar` répond 200 avec
            // `musicbrainz_id: "Pink Floyd"`. Sans ce filtre on repartirait
            // interroger l'API avec le nom, c'est-à-dire la panne d'origine.
            .filter(|s| is_mbid(s))
            .map(str::to_owned);
        self.mbid_cache.insert(wanted.to_lowercase(), found.clone());
        found
    }

    pub async fn refresh(&mut self, mbid: &str) -> Option<ArtistData> {
        self.cache.remove(mbid);
        let url = format!("{}/{mbid}/refresh", self.base_url);
        let client = crate::http::client::shared();
        let resp = client
            .post(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        let artist = ArtistData { fields: data };
        self.cache_set(mbid, artist.clone());
        Some(artist)
    }

    fn cache_get(&self, mbid: &str) -> Option<ArtistData> {
        let entry = self.cache.get(mbid)?;
        if entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS {
            return None;
        }
        Some(entry.data.clone())
    }

    fn cache_set(&mut self, mbid: &str, data: ArtistData) {
        self.cache.insert(
            mbid.to_string(),
            CacheEntry {
                data,
                fetched_at: Instant::now(),
            },
        );
    }

    async fn request(&self, path: &str) -> Option<serde_json::Value> {
        let url = format!("{}{path}", self.base_url);
        let client = crate::http::client::shared();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        resp.json().await.ok()
    }

    async fn request_with_params(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Option<serde_json::Value> {
        let url = format!("{}{path}", self.base_url);
        let client = crate::http::client::shared();
        let resp = client
            .get(&url)
            .query(params)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        resp.json().await.ok()
    }
}

impl Default for ArtistEnrichmentClient {
    fn default() -> Self {
        Self::new(None, 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url() {
        // Assertion EXACTE, volontairement. L'ancienne se contentait de
        // `contains("mozaiklabs.fr")` — vrai aussi pour `api.mozaiklabs.fr`,
        // un domaine qui n'existe pas et vers lequel deux appelants
        // repliaient. Le test restait vert pendant que la source de
        // suggestions était morte (#1730).
        let client = ArtistEnrichmentClient::default();
        assert_eq!(client.base_url, "https://mozaiklabs.fr/api/v1/artists");
    }

    #[test]
    fn absent_setting_falls_back_to_the_default_host() {
        // Ce que font désormais auto_dj::similar_artist_names et la route
        // /artists/{id}/similar quand le réglage `artist_enrichment_api` est
        // absent : passer None, jamais une adresse codée en dur.
        let from_setting: Option<String> = None;
        let client = ArtistEnrichmentClient::new(from_setting.as_deref(), 5);
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn extracts_the_real_similar_payload() {
        // Enveloppe RÉELLE, capturée le 15/08 sur
        // GET /api/v1/artists/a74b1b7f-…/similar. On ne cherchait que la clé
        // `artists`, absente : la liste ressortait vide même contre le bon
        // hôte (#1730).
        let v = serde_json::json!({
            "data": {
                "musicbrainz_id": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
                "name": "Radiohead",
                "similar_artists": [
                    {"name": "Thom Yorke", "reason": "…"},
                    {"name": "Atoms for Peace", "reason": "…"}
                ]
            }
        });
        let list = extract_list(&v, &["similar_artists", "artists"]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "Thom Yorke");
    }

    #[test]
    fn extracts_the_real_search_payload() {
        // GET /api/v1/artists/search?q=Radiohead
        let v = serde_json::json!({
            "data": [{"id": 375,
                      "musicbrainz_id": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
                      "name": "Radiohead"}],
            "count": 1
        });
        let list = extract_list(&v, &["artists"]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "Radiohead");
    }

    #[test]
    fn extracts_tolerates_other_envelopes() {
        // Tableau nu et clé nommée : on ne sait pas ce que rendent les
        // déploiements plus anciens, on reste tolérant.
        let bare = serde_json::json!([{"name": "A"}]);
        assert_eq!(extract_list(&bare, &["artists"]).len(), 1);
        let named = serde_json::json!({"artists": [{"name": "A"}, {"name": "B"}]});
        assert_eq!(extract_list(&named, &["artists"]).len(), 2);
        let empty = serde_json::json!({"data": {"name": "X"}});
        assert!(extract_list(&empty, &["similar_artists"]).is_empty());
    }

    #[test]
    fn mbid_shape_rejects_an_echoed_name() {
        // `/artists/Pink Floyd/similar` répond 200 avec
        // `musicbrainz_id: "Pink Floyd"` : l'API réémet l'entrée quand elle ne
        // trouve rien. Sans ce filtre on repartirait interroger l'API avec le
        // nom — la panne d'origine.
        assert!(is_mbid("a74b1b7f-71a5-4011-9441-d0b5e4122711"));
        assert!(!is_mbid("Pink Floyd"));
        assert!(!is_mbid(""));
        assert!(!is_mbid("a74b1b7f71a540119441d0b5e4122711"));
        assert!(!is_mbid("a74b1b7f-71a5-4011-9441-d0b5e412271"));
        assert!(!is_mbid("g74b1b7f-71a5-4011-9441-d0b5e4122711"));
    }

    #[test]
    fn present_setting_still_wins() {
        // L'adresse reste surchargeable — c'est ce qui permet de pointer une
        // pré-production sans recompiler.
        let from_setting = Some("http://192.168.1.10:3000/api/v1/artists".to_string());
        let client = ArtistEnrichmentClient::new(from_setting.as_deref(), 5);
        assert_eq!(client.base_url, "http://192.168.1.10:3000/api/v1/artists");
    }

    #[test]
    fn cache_miss() {
        let client = ArtistEnrichmentClient::default();
        assert!(client.cache_get("nonexistent-mbid").is_none());
    }

    #[test]
    fn cache_set_and_get() {
        let mut client = ArtistEnrichmentClient::default();
        let data = ArtistData {
            fields: serde_json::json!({"name": "Test"}),
        };
        client.cache_set("abc-123", data.clone());
        let cached = client.cache_get("abc-123");
        assert!(cached.is_some());
    }

    #[test]
    fn custom_url() {
        let client = ArtistEnrichmentClient::new(Some("http://localhost:3000/api"), 10);
        assert_eq!(client.base_url, "http://localhost:3000/api");
    }
}
