//! Consultation MusicBrainz d'un disque par son identifiant.
//!
//! `/ws/2/discid/<id>` avec l'identité (`MB_UA`) et le limiteur de débit PARTAGÉ
//! (`rate_limit_delay`) que Tune emploie déjà pour MusicBrainz. Rien n'est
//! écrit en bibliothèque : le résultat ne vit que dans la mémoire du greffon.
//! Sans réseau ou sans correspondance, les pistes s'appellent « Piste N ».

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tune_core::metadata::musicbrainz_release::MB_UA;

/// Ce que l'écran affiche d'un disque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfosDisque {
    pub titre: String,
    pub artiste: String,
    pub release_id: Option<String>,
    pub pochette: Option<String>,
    /// Titres par numéro de piste (position sur le support).
    pub pistes: HashMap<u8, InfosPiste>,
    /// #2466 — pour les balises de l'extraction : identifiants MusicBrainz
    /// des artistes du crédit de la sortie, date de sortie, position du
    /// support dans la sortie (1 par défaut) et nombre de supports.
    pub artiste_ids: Vec<String>,
    pub date: Option<String>,
    pub disque: u32,
    pub disques: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfosPiste {
    pub titre: String,
    pub artiste: Option<String>,
    /// #2466 — l'enregistrement (`MUSICBRAINZ_TRACKID` en Vorbis), la piste
    /// de CETTE sortie (`MUSICBRAINZ_RELEASETRACKID`) et les artistes du
    /// crédit de la piste.
    pub recording_id: Option<String>,
    pub piste_id: Option<String>,
    pub artiste_ids: Vec<String>,
}

/// La source des métadonnées d'un disque. Un trait pour que les routes se
/// prouvent sans réseau.
#[async_trait]
pub trait Consultation: Send + Sync {
    async fn consulter(&self, disc_id: &str) -> Option<InfosDisque>;
}

/// Le crédit d'artiste MusicBrainz, noms et liaisons (« feat. », « & »).
fn credit(v: Option<&Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let s: String = arr
        .iter()
        .map(|c| {
            format!(
                "{}{}",
                c.get("name").and_then(Value::as_str).unwrap_or(""),
                c.get("joinphrase").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Les identifiants MusicBrainz des artistes d'un crédit, dans l'ordre.
fn ids_du_credit(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("artist")?.get("id")?.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

fn texte(v: &Value, cle: &str) -> Option<String> {
    v.get(cle)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Lit une réponse `/ws/2/discid/<id>?inc=recordings+artist-credits`.
///
/// Retient la première sortie dont un SUPPORT porte cet identifiant, et les
/// pistes de CE support (un coffret de trois CD ne doit pas donner au disque
/// 2 les titres du disque 1).
pub fn lire_reponse(reponse: &Value, disc_id: &str) -> Option<InfosDisque> {
    for release in reponse.get("releases")?.as_array()? {
        let Some(media) = release.get("media").and_then(Value::as_array) else {
            continue;
        };
        let support = media.iter().find(|m| {
            m.get("discs").and_then(Value::as_array).is_some_and(|d| {
                d.iter()
                    .any(|x| x.get("id").and_then(Value::as_str) == Some(disc_id))
            })
        });
        let Some(support) = support else { continue };
        let artiste = credit(release.get("artist-credit")).unwrap_or_default();
        let mut pistes = HashMap::new();
        for t in support
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(pos) = t.get("position").and_then(Value::as_u64) else {
                continue;
            };
            let titre = t
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if titre.is_empty() || pos == 0 || pos > 99 {
                continue;
            }
            let artiste_piste = credit(t.get("artist-credit")).filter(|a| *a != artiste);
            let ids_piste = ids_du_credit(t.get("artist-credit"));
            pistes.insert(
                pos as u8,
                InfosPiste {
                    titre,
                    artiste: artiste_piste,
                    recording_id: t.get("recording").and_then(|r| texte(r, "id")),
                    piste_id: texte(t, "id"),
                    artiste_ids: ids_piste,
                },
            );
        }
        let release_id = release
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let pochette = release
            .get("cover-art-archive")
            .and_then(|c| c.get("front"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| {
                release_id
                    .as_ref()
                    .map(|id| format!("https://coverartarchive.org/release/{id}/front-500"))
            })
            .flatten();
        return Some(InfosDisque {
            titre: release
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            artiste,
            release_id,
            pochette,
            pistes,
            artiste_ids: ids_du_credit(release.get("artist-credit")),
            date: texte(release, "date"),
            disque: support
                .get("position")
                .and_then(Value::as_u64)
                .filter(|p| *p > 0)
                .unwrap_or(1) as u32,
            disques: (media.len() as u32).max(1),
        });
    }
    None
}

/// La consultation réelle, avec mémoire par identifiant ; le débit est celui du
/// limiteur MusicBrainz partagé.
pub struct MusicBrainz {
    client: &'static reqwest::Client,
    memoire: Mutex<HashMap<String, InfosDisque>>,
}

impl MusicBrainz {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            client: tune_core::http::client::shared(),
            memoire: Mutex::new(HashMap::new()),
        })
    }
}

#[async_trait]
impl Consultation for MusicBrainz {
    async fn consulter(&self, disc_id: &str) -> Option<InfosDisque> {
        if let Some(i) = self.memoire.lock().await.get(disc_id) {
            return Some(i.clone());
        }
        // Le créneau MusicBrainz du limiteur PARTAGÉ (#4767) : une requête par
        // seconde et par IP pour TOUTES les passes de Tune — pochettes, crédits,
        // types de sortie, identification et ce greffon. Une pause propre au
        // greffon laisserait deux flux se croiser dans la même seconde (503).
        tune_core::metadata::musicbrainz_release::rate_limit_delay().await;
        let reponse = self
            .client
            .get(format!("https://musicbrainz.org/ws/2/discid/{disc_id}"))
            .query(&[
                ("inc", "recordings artist-credits"),
                ("fmt", "json"),
                ("cdstubs", "no"),
            ])
            .header("User-Agent", MB_UA)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .inspect(tune_core::metadata::musicbrainz_release::constater_reponse_musicbrainz);
        let infos = match reponse {
            Ok(r) if r.status().is_success() => r
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| lire_reponse(&v, disc_id)),
            Ok(r) => {
                tracing::info!(disc_id, status = %r.status(), "cd_musicbrainz_sans_correspondance");
                None
            }
            Err(e) => {
                tracing::info!(disc_id, error = %e, "cd_musicbrainz_injoignable");
                None
            }
        }?;
        self.memoire
            .lock()
            .await
            .insert(disc_id.to_string(), infos.clone());
        Some(infos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPONSE: &str =
        include_str!("../tests/fixtures/discid_Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-.json");

    #[test]
    fn la_reponse_publiee_donne_titre_artiste_pistes_et_pochette() {
        let v: Value = serde_json::from_str(REPONSE).unwrap();
        let i = lire_reponse(&v, "Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-").unwrap();
        assert_eq!(i.titre, "Fiction");
        assert_eq!(i.artiste, "Dark Tranquillity");
        assert_eq!(i.pistes.len(), 10);
        assert_eq!(i.pistes[&1].titre, "Nothing to No One");
        assert_eq!(i.pistes[&1].artiste, None);
        assert!(
            i.pochette
                .as_deref()
                .unwrap()
                .starts_with("https://coverartarchive.org/release/")
        );
    }

    /// #2466 — les identifiants que l'extraction écrit en balises : ceux de
    /// la sortie, de l'enregistrement, de la piste et des artistes, plus la
    /// position du support dans un coffret.
    #[test]
    fn les_identifiants_et_la_position_du_support_sont_lus() {
        let v = serde_json::json!({ "releases": [{
            "id": "rel-1", "title": "Coffret", "date": "1999-05-01",
            "artist-credit": [{ "name": "A", "joinphrase": "", "artist": { "id": "art-a" } }],
            "media": [
                { "position": 1, "discs": [{ "id": "autre" }], "tracks": [] },
                { "position": 2, "discs": [{ "id": "ce-disque" }], "tracks": [
                    { "id": "trk-1", "position": 1, "title": "Un",
                      "recording": { "id": "rec-1" },
                      "artist-credit": [{ "name": "B", "artist": { "id": "art-b" } }] }
                ] }
            ]
        }]});
        let i = lire_reponse(&v, "ce-disque").unwrap();
        assert_eq!((i.disque, i.disques), (2, 2));
        assert_eq!(i.date.as_deref(), Some("1999-05-01"));
        assert_eq!(i.artiste_ids, vec!["art-a".to_string()]);
        let p = &i.pistes[&1];
        assert_eq!(p.recording_id.as_deref(), Some("rec-1"));
        assert_eq!(p.piste_id.as_deref(), Some("trk-1"));
        assert_eq!(p.artiste_ids, vec!["art-b".to_string()]);
    }

    #[test]
    fn un_autre_disque_ne_prend_pas_ces_titres() {
        let v: Value = serde_json::from_str(REPONSE).unwrap();
        assert_eq!(lire_reponse(&v, "autre-identifiant"), None);
        assert_eq!(lire_reponse(&serde_json::json!({}), "x"), None);
    }
}
