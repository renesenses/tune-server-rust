//! Consultation MusicBrainz d'un disque par son identifiant.
//!
//! `/ws/2/discid/<id>` avec l'identité (`MB_UA`) et le débit (une requête par
//! `MB_RATE_LIMIT_MS`) que Tune emploie déjà pour MusicBrainz. Rien n'est
//! écrit en bibliothèque : le résultat ne vit que dans la mémoire du greffon.
//! Sans réseau ou sans correspondance, les pistes s'appellent « Piste N ».

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tune_core::metadata::musicbrainz_release::{MB_RATE_LIMIT_MS, MB_UA};

/// Ce que l'écran affiche d'un disque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfosDisque {
    pub titre: String,
    pub artiste: String,
    pub release_id: Option<String>,
    pub pochette: Option<String>,
    /// Titres par numéro de piste (position sur le support).
    pub pistes: HashMap<u8, InfosPiste>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfosPiste {
    pub titre: String,
    pub artiste: Option<String>,
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
            pistes.insert(
                pos as u8,
                InfosPiste {
                    titre,
                    artiste: artiste_piste,
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
        });
    }
    None
}

/// La consultation réelle, avec mémoire par identifiant et débit borné.
pub struct MusicBrainz {
    client: &'static reqwest::Client,
    derniere: Mutex<Option<Instant>>,
    memoire: Mutex<HashMap<String, InfosDisque>>,
}

impl MusicBrainz {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            client: tune_core::http::client::shared(),
            derniere: Mutex::new(None),
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
        {
            // Une requête par `MB_RATE_LIMIT_MS`, tenue sous le verrou pour
            // que deux écrans ouverts ne la franchissent pas ensemble.
            let mut derniere = self.derniere.lock().await;
            if let Some(t) = *derniere {
                let pas = Duration::from_millis(MB_RATE_LIMIT_MS);
                if t.elapsed() < pas {
                    tokio::time::sleep(pas - t.elapsed()).await;
                }
            }
            *derniere = Some(Instant::now());
        }
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
            .await;
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

    #[test]
    fn un_autre_disque_ne_prend_pas_ces_titres() {
        let v: Value = serde_json::from_str(REPONSE).unwrap();
        assert_eq!(lire_reponse(&v, "autre-identifiant"), None);
        assert_eq!(lire_reponse(&serde_json::json!({}), "x"), None);
    }
}
