//! Tune Circle, étape T5 (#5328) : rejouer une référence CHEZ L'APPELANT.
//!
//! Une playlist de cercle ne porte que des références. Chaque membre la
//! rejoue avec ce qu'il possède : ses services connectés et sa bibliothèque.
//! Qobuz chez l'un, Tidal chez l'autre, cela marche.
//!
//! # L'ordre (contrat de #5328)
//!
//! 1. **L'identifiant d'un service que l'utilisateur possède** (`qobuz_id`
//!    s'il a Qobuz…) : le service le confirme par `get_track`.
//! 2. **L'ISRC**, en bibliothèque puis chez chaque service connecté.
//! 3. **Titre, artiste et durée**, la bibliothèque d'abord (le fichier de
//!    l'utilisateur), puis ses services.
//!
//! # Aucun appariement réécrit (#4741)
//!
//! Les étapes 2 et 3 sont UN seul geste par source, celui du moteur de
//! transfert de playlists : [`apparier_en_bibliotheque`] pour la
//! bibliothèque, [`apparier_chez_le_service`] pour un service — la même
//! recherche et le même verdict que `POST /playlist-manager/transfer` et que
//! la capacité `host_streaming_match_track` du greffon « Playlists
//! converter ». Le chemin rapide ISRC de `track_matcher::find_best_match`
//! y tranche avant tout score approché ; on ne fait que regarder si le
//! vainqueur porte l'ISRC de la référence, pour préférer ce verdict-là à un
//! appariement par le texte. Le seuil est celui du transfert
//! ([`MATCH_ACCEPT_SCORE`]) : un appariement approximatif est « introuvable ».
//!
//! # Lecture seule
//!
//! Aucune écriture chez un service : ni playlist, ni favori. Et rien n'est
//! gardé — le résultat part au client et s'oublie.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tracing::debug;
use tune_core::cloud::playlist_hub::SERVICES_DE_REFERENCE;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::appariement_bibliotheque::apparier_en_bibliotheque;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::matching::{MATCH_ACCEPT_SCORE, apparier_chez_le_service};
use tune_core::streaming::traits::{StreamTrack, StreamingService};

use crate::references::{Reference, isrc_normalise};

type Service = Arc<RwLock<Box<dyn StreamingService>>>;

/// Comment une référence a été retrouvée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Methode {
    IdentifiantDeService,
    Isrc,
    Texte,
}

impl Methode {
    pub fn nom(self) -> &'static str {
        match self {
            Self::IdentifiantDeService => "service_id",
            Self::Isrc => "isrc",
            Self::Texte => "text",
        }
    }
}

/// Ce qu'une référence est devenue chez l'appelant.
#[derive(Debug, Clone)]
pub enum Trouvee {
    /// Une piste de SA bibliothèque.
    Bibliotheque { piste: Box<Track>, methode: Methode },
    /// Un titre d'un de SES services.
    Service {
        service: String,
        piste: Box<StreamTrack>,
        methode: Methode,
    },
}

/// Les sources de l'appelant : sa base, et ses services.
pub struct Resolveur {
    backend: Arc<dyn DbBackend>,
    services: Arc<Mutex<ServiceRegistry>>,
}

impl Resolveur {
    pub fn new(backend: Arc<dyn DbBackend>, services: Arc<Mutex<ServiceRegistry>>) -> Self {
        Self { backend, services }
    }

    pub fn backend(&self) -> &Arc<dyn DbBackend> {
        &self.backend
    }

    /// Le service nommé s'il est UTILISABLE (activé et connecté) — la règle
    /// unique de `StreamingService::utilisable`.
    pub async fn service_utilisable(&self, nom: &str) -> Option<Service> {
        let service = self.services.lock().await.get(nom)?;
        let utilisable = service.read().await.utilisable().await;
        utilisable.then_some(service)
    }

    /// Les services de [`SERVICES_DE_REFERENCE`] que l'appelant possède, dans
    /// cet ordre. Relu à chaque résolution : une déconnexion vaut aussitôt.
    pub async fn services_utilisables(&self) -> Vec<(String, Service)> {
        let mut rendus = Vec::new();
        for nom in SERVICES_DE_REFERENCE {
            if let Some(s) = self.service_utilisable(nom).await {
                rendus.push((nom.to_string(), s));
            }
        }
        rendus
    }

    /// Un titre d'un service de l'appelant, par son identifiant.
    pub async fn piste_du_service(&self, nom: &str, id: &str) -> Option<StreamTrack> {
        let service = self.service_utilisable(nom).await?;
        let lu = service.read().await.get_track(id).await;
        match lu {
            Ok(p) => Some(p),
            Err(e) => {
                debug!(service = nom, error = %e, "circle_piste_de_service_introuvable");
                None
            }
        }
    }

    /// Résout UNE référence, `None` si rien ne correspond chez l'appelant.
    ///
    /// `services` : ceux de [`Self::services_utilisables`], lus une fois pour
    /// toute la playlist.
    pub async fn resoudre(
        &self,
        reference: &Reference,
        services: &[(String, Service)],
    ) -> Option<Trouvee> {
        // 1. L'identifiant d'un service que l'utilisateur possède.
        for (nom, id) in &reference.identifiants {
            let Some((_, service)) = services.iter().find(|(s, _)| s == nom) else {
                continue;
            };
            let lu = service.read().await.get_track(id).await;
            if let Ok(piste) = lu {
                return Some(Trouvee::Service {
                    service: nom.clone(),
                    piste: Box::new(piste),
                    methode: Methode::IdentifiantDeService,
                });
            }
        }
        if reference.title.is_empty() {
            return None;
        }
        let isrc = isrc_normalise(&reference.isrc);
        let par_isrc = |candidat: Option<&str>| {
            !isrc.is_empty() && candidat.is_some_and(|c| isrc_normalise(c) == isrc)
        };

        // 2 et 3. Le moteur du transfert de playlists, bibliothèque d'abord.
        let repo = TrackRepo::with_backend(self.backend.clone());
        let locale = apparier_en_bibliotheque(
            &repo,
            &reference.title,
            &reference.artist_name,
            &reference.isrc,
            reference.duration_ms as i64,
            1,
        )
        .ok()
        .and_then(|v| v.into_iter().next())
        .filter(|(_, score)| *score >= MATCH_ACCEPT_SCORE)
        .map(|(t, _)| t);
        if let Some(piste) = &locale
            && par_isrc(piste.isrc.as_deref())
        {
            return Some(Trouvee::Bibliotheque {
                piste: Box::new(piste.clone()),
                methode: Methode::Isrc,
            });
        }

        let mut par_le_texte: Option<(String, StreamTrack)> = None;
        for (nom, service) in services {
            let apparie = {
                let svc = service.read().await;
                apparier_chez_le_service(
                    &**svc,
                    &reference.title,
                    &reference.artist_name,
                    &reference.isrc,
                    reference.duration_ms,
                )
                .await
            };
            let Ok(Some((piste, score))) = apparie else {
                continue;
            };
            if score < MATCH_ACCEPT_SCORE {
                continue;
            }
            if par_isrc(piste.isrc.as_deref()) {
                return Some(Trouvee::Service {
                    service: nom.clone(),
                    piste: Box::new(piste),
                    methode: Methode::Isrc,
                });
            }
            if par_le_texte.is_none() {
                par_le_texte = Some((nom.clone(), piste));
            }
        }

        if let Some(piste) = locale {
            return Some(Trouvee::Bibliotheque {
                piste: Box::new(piste),
                methode: Methode::Texte,
            });
        }
        par_le_texte.map(|(service, piste)| Trouvee::Service {
            service,
            piste: Box::new(piste),
            methode: Methode::Texte,
        })
    }
}

/// La ligne rendue au client pour un morceau de la playlist.
///
/// `{ item_id, status, source, source_id, method }`, et `track_id` pour une
/// piste de la bibliothèque. Pour une piste locale, `source_id` est son
/// identifiant de ligne, jamais la colonne `source_id` de la base (qui peut
/// être un chemin).
pub fn ligne(item_id: &Value, trouvee: Option<&Trouvee>) -> Value {
    match trouvee {
        None => json!({
            "item_id": item_id,
            "status": "not_found",
            "source": Value::Null,
            "source_id": Value::Null,
        }),
        Some(Trouvee::Bibliotheque { piste, methode }) => json!({
            "item_id": item_id,
            "status": "matched",
            "source": "local",
            "source_id": piste.id.map(|i| i.to_string()),
            "track_id": piste.id,
            "method": methode.nom(),
        }),
        Some(Trouvee::Service {
            service,
            piste,
            methode,
        }) => json!({
            "item_id": item_id,
            "status": "matched",
            "source": service,
            "source_id": piste.id,
            "method": methode.nom(),
        }),
    }
}
