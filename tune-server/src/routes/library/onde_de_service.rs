//! L'enveloppe de la barre de progression (200 points, Discussion #8269)
//! pour une piste QOBUZ ou TIDAL en lecture — go de Bertrand du 17/11/2026,
//! rc13.
//!
//! `GET /library/service-tracks/{service}/{id_service}/waveform`
//!
//! La piste en lecture d'un service n'a ni fichier ni identifiant de
//! bibliothèque (`current_track.track_id` est nul) : la route existante
//! `/library/tracks/{id}/waveform` ne peut rien pour elle. Celle-ci prend le
//! service et l'identifiant chez lui (`current_track.source` et `source_id`)
//! et réutilise le téléchargement pour analyse de la fiche technique
//! (`audio_de_service`, server#8739) : adresses résolues sous le garde du
//! service, téléchargement hors de lui, plafonds de taille et de délai, deux
//! téléchargements au plus en même temps, fichier audio EFFACÉ après le
//! calcul. Aucun compte rendu de lecture n'est envoyé au service.
//!
//! Le flux de la lecture n'est PAS réutilisé : `audio_de_service` s'interdit
//! le fichier d'un service en cours de lecture (un fMP4 peut être encore en
//! croissance). Une piste se télécharge donc UNE fois pour son analyse, et
//! plus jamais ensuite : l'enveloppe est gardée sur le disque
//! (`<cache des pochettes>/ondes-service/<clé>.json`, clé du service, de
//! l'identifiant et de la qualité, comme la fiche technique).
//!
//! Réponses — la barre n'attend jamais :
//! - `200 {"waveform": [...]}` : en cache ;
//! - `202 {"waveform": null, "pending": true}` : calcul lancé (ou déjà en
//!   route) en tâche de fond ; l'écran garde la barre classique et redemande ;
//! - `200 {"waveform": null, "raison": …}` : refus DÉFINITIF (piste non
//!   diffusable, hors région, trop volumineuse, fichier téléchargé mais
//!   indécodable, service non pris en charge),
//!   retenu six heures : l'écran ne redemande pas ;
//! - `503 {"waveform": null, "raison": …}` : échec PASSAGER (délai, réseau,
//!   service déconnecté ou désactivé), retenu deux minutes pour ne pas
//!   relancer un téléchargement à chaque relance de l'écran.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::audio_de_service::{self as service, Limites, PisteDeService, Refus};
use crate::state::AppState;

/// Le nombre de points de l'enveloppe : celui de `/library/tracks/{id}/waveform`.
pub(crate) const POINTS: usize = 200;

/// Calculs en route au plus, toutes pistes confondues. Au-delà, la demande
/// répond « pas encore » sans rien lancer : l'écran redemandera.
pub(crate) const CALCULS_EN_ROUTE_MAX: usize = 4;

/// Combien de temps un refus définitif est retenu.
const DUREE_REFUS_DEFINITIF: Duration = Duration::from_secs(6 * 3600);
/// Combien de temps un échec passager est retenu.
const DUREE_ECHEC_PASSAGER: Duration = Duration::from_secs(120);

/// Ce que la route peut dire d'une piste.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Etat {
    Prete(Vec<f32>),
    EnCours,
    Refusee(Refus),
}

/// Un refus que redemander ne changerait pas.
pub(crate) fn refus_definitif(refus: Refus) -> bool {
    matches!(
        refus,
        Refus::NonPrisEnCharge
            | Refus::SourceReseau
            | Refus::NonDiffusable
            | Refus::Region
            | Refus::TropVolumineux
            | Refus::ZoomNonDisponible
            | Refus::FichierIllisible
    )
}

/// Libère la place d'un calcul qui part sans avoir rendu son issue — une
/// panique dans la tâche de fond : sans lui, la piste resterait « en cours »
/// et occuperait une des [`CALCULS_EN_ROUTE_MAX`] places jusqu'au
/// redémarrage. L'échec est classé passager.
struct GardeDuCalcul {
    ondes: Arc<Ondes>,
    cle: String,
    rendu: bool,
}

impl Drop for GardeDuCalcul {
    fn drop(&mut self) {
        if self.rendu {
            return;
        }
        tracing::warn!(cle = %self.cle, "onde_service_calcul_interrompu");
        self.ondes.suivi().insert(
            std::mem::take(&mut self.cle),
            Suivi::Echec {
                refus: Refus::TelechargementImpossible,
                jusqua: Instant::now() + DUREE_ECHEC_PASSAGER,
            },
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum Suivi {
    EnCours,
    Echec { refus: Refus, jusqua: Instant },
}

/// Le cache disque et le suivi des calculs en route.
pub(crate) struct Ondes {
    dossier: PathBuf,
    suivi: Mutex<HashMap<String, Suivi>>,
}

impl Ondes {
    pub(crate) fn new(dossier: PathBuf) -> Self {
        Self {
            dossier,
            suivi: Mutex::new(HashMap::new()),
        }
    }

    fn chemin(&self, cle: &str) -> PathBuf {
        self.dossier.join(format!("{cle}.json"))
    }

    fn lire(&self, cle: &str) -> Option<Vec<f32>> {
        let octets = std::fs::read(self.chemin(cle)).ok()?;
        let points: Vec<f32> = serde_json::from_slice(&octets).ok()?;
        (points.len() >= 2).then_some(points)
    }

    fn ecrire(&self, cle: &str, points: &[f32]) {
        let cible = self.chemin(cle);
        let provisoire = self.dossier.join(format!("{cle}.json.part"));
        let issue = std::fs::create_dir_all(&self.dossier)
            .and_then(|()| {
                std::fs::write(&provisoire, serde_json::to_vec(points).unwrap_or_default())
            })
            .and_then(|()| std::fs::rename(&provisoire, &cible));
        if let Err(e) = issue {
            tracing::warn!(cle, error = %e, "onde_service_cache_non_ecrit");
        }
    }

    fn suivi(&self) -> std::sync::MutexGuard<'_, HashMap<String, Suivi>> {
        self.suivi.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// L'état de la piste `piste`, et le calcul lancé s'il le faut.
    ///
    /// `calculer` n'est appelée que si rien n'est en cache, en route ou
    /// retenu : UNE analyse par piste, quel que soit le nombre de demandes.
    /// Le futur qu'elle rend tourne en tâche de fond ; la réponse n'attend
    /// pas.
    pub(crate) fn consulter<F, Fut>(self: &Arc<Self>, piste: &PisteDeService, calculer: F) -> Etat
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<f32>, Refus>> + Send + 'static,
    {
        let cle = piste.cle();
        if let Some(points) = self.lire(&cle) {
            return Etat::Prete(points);
        }
        {
            let mut suivi = self.suivi();
            match suivi.get(&cle).copied() {
                Some(Suivi::EnCours) => return Etat::EnCours,
                Some(Suivi::Echec { refus, jusqua }) if Instant::now() < jusqua => {
                    return Etat::Refusee(refus);
                }
                _ => {}
            }
            let en_route = suivi
                .values()
                .filter(|s| matches!(s, Suivi::EnCours))
                .count();
            if en_route >= CALCULS_EN_ROUTE_MAX {
                return Etat::EnCours;
            }
            suivi.insert(cle.clone(), Suivi::EnCours);
        }
        let travail = calculer();
        let ondes = Arc::clone(self);
        tokio::spawn(async move {
            let mut garde = GardeDuCalcul {
                ondes: Arc::clone(&ondes),
                cle: cle.clone(),
                rendu: false,
            };
            let issue = travail.await;
            garde.rendu = true;
            let mut suivi = ondes.suivi();
            match issue {
                Ok(points) if points.len() >= 2 => {
                    ondes.ecrire(&cle, &points);
                    suivi.remove(&cle);
                }
                Ok(_) => {
                    // Fichier téléchargé en entier mais indécodable : rien à
                    // dessiner, et le retélécharger (jusqu'à 400 Mo) toutes
                    // les deux minutes n'y changerait rien. Refus définitif.
                    suivi.insert(
                        cle,
                        Suivi::Echec {
                            refus: Refus::FichierIllisible,
                            jusqua: Instant::now() + DUREE_REFUS_DEFINITIF,
                        },
                    );
                }
                Err(refus) => {
                    let duree = if refus_definitif(refus) {
                        DUREE_REFUS_DEFINITIF
                    } else {
                        DUREE_ECHEC_PASSAGER
                    };
                    suivi.insert(
                        cle,
                        Suivi::Echec {
                            refus,
                            jusqua: Instant::now() + duree,
                        },
                    );
                }
            }
        });
        Etat::EnCours
    }
}

/// La réponse HTTP d'un état.
pub(crate) fn reponse(etat: Etat) -> Response {
    match etat {
        Etat::Prete(points) => Json(json!({ "waveform": points })).into_response(),
        Etat::EnCours => (
            StatusCode::ACCEPTED,
            Json(json!({ "waveform": null, "pending": true })),
        )
            .into_response(),
        Etat::Refusee(refus) => {
            let statut = if refus_definitif(refus) {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            (
                statut,
                Json(json!({ "waveform": null, "error": refus.message(), "raison": refus.code() })),
            )
                .into_response()
        }
    }
}

static ONDES: LazyLock<Arc<Ondes>> =
    LazyLock::new(|| Arc::new(Ondes::new(super::artwork_cache_dir().join("ondes-service"))));

/// `GET /library/service-tracks/{service}/{id_service}/waveform`
pub(crate) async fn waveform(
    State(state): State<AppState>,
    Path((service_nom, id_service)): Path<(String, String)>,
) -> Response {
    let Some(piste) = PisteDeService::du_service(&service_nom, &id_service) else {
        return reponse(Etat::Refusee(Refus::NonPrisEnCharge));
    };
    let ondes = Arc::clone(&ONDES);
    let a_calculer = piste.clone();
    let dossier_tmp = service::dossier_temporaire(&ondes.dossier);
    reponse(ondes.consulter(&piste, move || async move {
        let audio = service::telecharger(
            &state,
            &a_calculer,
            &dossier_tmp,
            Limites::de_l_environnement(),
        )
        .await?;
        let chemin = audio.chemin().to_string_lossy().into_owned();
        let points = tune_core::audio::analyzer::generate_waveform(&chemin, POINTS).await;
        // `audio` part ici : le fichier téléchargé est effacé.
        drop(audio);
        Ok(points)
    }))
}

#[cfg(test)]
#[path = "onde_de_service_tests.rs"]
mod tests;
