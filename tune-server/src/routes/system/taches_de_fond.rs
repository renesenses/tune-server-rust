//! Suspendre et reprendre les traitements de fond — la porte HTTP du registre
//! `tune_core::taches_de_fond`.
//!
//! Le scan avait `POST /system/scan/cancel` ; le ReplayGain, la plage
//! dynamique, l'analyse acoustique, l'enrichissement des métadonnées et les
//! images d'artistes n'avaient **rien**. Sur le .18, ces passes tournent
//! pendant des heures — 57 % de 47 118 pistes en plage dynamique — et mangent
//! le disque et le processeur pendant qu'on écoute.
//!
//! Quatre routes, toutes dans la famille `/system/background-tasks` — celle
//! qui sert déjà l'état des cartes :
//!
//! | Route | Effet |
//! |---|---|
//! | `POST /system/background-tasks/{id}/pause` | Suspend UN traitement |
//! | `POST /system/background-tasks/{id}/resume` | Le reprend |
//! | `POST /system/background-tasks/pause-all` | L'interrupteur général |
//! | `POST /system/background-tasks/resume-all` | Tout reprendre |
//!
//! Les `{id}` sont ceux de [`tune_core::taches_de_fond::Tache::id`] :
//! `replaygain`, `fingerprints`, `dynamic_range`, `acoustic`, `enrichment`,
//! `artist_images`. Un identifiant inconnu rend **404** en nommant ceux qui
//! existent, plutôt qu'un 200 qui n'aurait rien suspendu — c'est exactement le
//! piège du corps JSON à champ inconnu, que `serde` jette en rendant 200.
//!
//! ⚠️ **Le scan n'en fait pas partie**, et
//! [`tune_core::taches_de_fond::pourquoi_le_scan_n_est_pas_suspendable`] dit
//! pourquoi : il a déjà son `cancel`, sa porte est un état de processus, et
//! une pause qui survit au redémarrage n'aurait plus de scan à reprendre.
//!
//! Toutes rendent le même corps que `GET /system/background-tasks` : un seul
//! aller-retour suffit à l'écran pour redessiner ses cartes, sans second
//! sondage derrière le clic.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::taches_de_fond::{Etat, Tache, est_en_pause};

use crate::state::AppState;

/// L'état d'un traitement, sans la moindre requête.
///
/// Tout l'écran sonde cette route en boucle : chaque signal lu ici doit être
/// gratuit. Aucun `COUNT(*)` — c'est la règle que `/system/replaygain/progress`
/// s'impose déjà pour ses candidats, et la raison pour laquelle l'activité
/// acoustique a dû se doter de son propre témoin
/// ([`tune_core::audio::embedding::balayage_acoustique_en_cours`]).
///
/// Les signaux, un par traitement :
///
/// * **`replaygain` et `fingerprints`** — `progression::releve().actif`. Les
///   deux partagent une seule boucle, une seule campagne au registre
///   (`TACHE_REPLAYGAIN`) et un seul avancement : ce sont deux rangs de la même
///   cascade, pas deux passes.
/// * **`dynamic_range`** — le passage à la demande (#4185) s'il court, sinon la
///   cascade : le rang 3 travaille sous l'avancement du ReplayGain.
/// * **`acoustic`** — le dernier lot a-t-il rendu quelque chose.
/// * **`enrichment` et `artist_images`** — le registre `background_tasks`, où
///   ces passes s'inscrivent déjà par `begin()` le temps qu'elles vivent.
/// * **`identification`** — même signal, même registre : le pilote de lot de
///   `POST /library/identify-all` s'y inscrit sous `identification_lot` (#4805).
fn etat_de(state: &AppState, tache: Tache) -> Etat {
    if est_en_pause(tache) {
        return Etat::EnPause;
    }
    let cascade = tune_core::audio::replaygain::progression::releve().actif;
    let en_cours = match tache {
        Tache::ReplayGain | Tache::Empreintes => cascade,
        Tache::PlageDynamique => state.passe_dr.releve().actif || cascade,
        // Le module `embedding` est derrière `audio-embedding` : sans la
        // feature, la passe acoustique n'existe pas, donc elle ne tourne pas.
        // La suspendre reste possible et sans effet — plutôt qu'un identifiant
        // qui disparaîtrait du relevé selon la recette de compilation, ce que
        // le client n'a aucun moyen de deviner.
        #[cfg(feature = "audio-embedding")]
        Tache::Acoustique => tune_core::audio::embedding::balayage_acoustique_en_cours(),
        #[cfg(not(feature = "audio-embedding"))]
        Tache::Acoustique => false,
        Tache::Enrichissement => inscrite(state, &["enrich_all", "bios", "credits_enrich_auto"]),
        Tache::ImagesArtistes => inscrite(state, &["artist_artwork", "artwork"]),
        Tache::Identification => inscrite(state, &["identification_lot"]),
    };
    if en_cours {
        Etat::EnCours
    } else {
        Etat::AuRepos
    }
}

/// L'une de ces tâches est-elle inscrite au registre des tâches en cours ?
fn inscrite(state: &AppState, ids: &[&str]) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| ids.contains(&t.id.as_str()))
}

/// Le bloc que toutes les routes de cette famille rendent, `GET` comprise.
///
/// Un seul corps, écrit UNE fois : deux formes divergeraient dès la première
/// tâche ajoutée, et l'écran lirait un état sur le `GET` et un autre après son
/// propre clic.
pub(crate) fn instantane(state: &AppState) -> Value {
    let traitements: Vec<Value> = Tache::TOUTES
        .into_iter()
        .map(|tache| {
            json!({
                "id": tache.id(),
                "state": etat_de(state, tache).code(),
                "paused": est_en_pause(tache),
            })
        })
        .collect();
    json!({
        "tasks": state.background_tasks.snapshot(),
        "pausable": traitements,
        "all_paused": tune_core::taches_de_fond::tout_est_suspendu(),
        // Le scan n'est pas suspendable, et l'écran ne doit pas avoir à le
        // deviner : il porte « Arrêter », pas « Pause ».
        "scan_pausable": false,
    })
}

/// Le 404 d'un identifiant inconnu — qui NOMME les identifiants servis.
///
/// Un refus qui ne dit pas ce qui était attendu renvoie l'appelant chercher au
/// hasard ; c'est le reproche déjà adressé au 409 « analyse désactivée » de la
/// plage dynamique, réglé de la même façon (#4185).
fn inconnue(id: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "status": "unknown_task",
            "requested": id,
            "known": Tache::TOUTES.map(|t| t.id()),
            "scan": tune_core::taches_de_fond::pourquoi_le_scan_n_est_pas_suspendable(),
        })),
    )
}

/// Le 500 d'une écriture de réglage refusée. La pause n'a PAS été posée — le
/// registre écrit la base avant son miroir — et le dire vaut mieux qu'un 200
/// sur une pause qui se lèverait au prochain redémarrage.
fn echec(e: String) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "status": "error", "error": e })),
    )
}

/// `POST /system/background-tasks/{id}/pause`
///
/// Suspend un traitement. Idempotent : suspendre ce qui l'est déjà rend 200.
///
/// La pause est **coopérative** : elle est honorée à la frontière de l'élément
/// suivant — entre deux pistes, entre deux artistes — jamais au milieu d'un
/// décodage ni d'une écriture. Le travail déjà fait est écrit, rien n'est
/// perdu, et la reprise repart au même point.
///
/// Elle est **persistante** : posée en base, elle survit au redémarrage du
/// serveur. Un traitement suspendu ne repart pas de lui-même.
pub(crate) async fn pause_tache(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(tache) = Tache::depuis_id(&id) else {
        return inconnue(&id).into_response();
    };
    match tune_core::taches_de_fond::mettre_en_pause(&state.backend, tache) {
        Ok(()) => reponse(&state),
        Err(e) => echec(e).into_response(),
    }
}

/// `POST /system/background-tasks/{id}/resume` — reprend un traitement.
/// Idempotent.
pub(crate) async fn reprendre_tache(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(tache) = Tache::depuis_id(&id) else {
        return inconnue(&id).into_response();
    };
    match tune_core::taches_de_fond::reprendre(&state.backend, tache) {
        Ok(()) => reponse(&state),
        Err(e) => echec(e).into_response(),
    }
}

/// `POST /system/background-tasks/pause-all` — l'interrupteur général.
///
/// « Suspendre tous les traitements » : le geste d'un soir d'écoute. Il pose la
/// même pause sur les six traitements, une par une — pas un septième drapeau
/// « global », qui se désynchroniserait des six dès qu'on en reprend un seul.
pub(crate) async fn tout_suspendre(State(state): State<AppState>) -> impl IntoResponse {
    match tune_core::taches_de_fond::tout_suspendre(&state.backend) {
        Ok(()) => reponse(&state),
        Err(e) => echec(e).into_response(),
    }
}

/// `POST /system/background-tasks/resume-all` — tout reprendre.
pub(crate) async fn tout_reprendre(State(state): State<AppState>) -> impl IntoResponse {
    match tune_core::taches_de_fond::tout_reprendre(&state.backend) {
        Ok(()) => reponse(&state),
        Err(e) => echec(e).into_response(),
    }
}

/// Le 200 commun : l'instantané complet, et le même évènement que le registre
/// émet déjà, pour que les autres onglets ouverts se redessinent sans sondage.
fn reponse(state: &AppState) -> axum::response::Response {
    let corps = instantane(state);
    state
        .event_bus
        .emit("system.background_tasks", corps.clone());
    (StatusCode::OK, Json(corps)).into_response()
}
