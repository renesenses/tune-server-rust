//! `POST /system/dynamic-range/analyze` et `GET /system/dynamic-range/progress`
//! — le geste qui lance la mesure de la plage dynamique, et sa jauge (#4185).
//!
//! « Pas trouvé où lancer l'analyse » (Tades, 0.9.150, fil 1800) : il n'y
//! avait rien à trouver. `routes/system/mod.rs` n'exposait qu'une route
//! ReplayGain, en lecture (`GET /replaygain/progress`, #4144), et la mesure du
//! DR n'avait qu'un appelant, le troisième rang de la cascade de fond
//! (`tune-core/src/audio/replaygain.rs`, `spawn`) — après le ReplayGain et
//! après les empreintes. Le scan a son `POST`, les paroles ont le leur, les
//! empreintes ont leur rattrapage forcé (`POST /library/duplicates/empreintes`).
//! Voici celui de la plage dynamique.
//!
//! La mécanique vit dans `tune_core::audio::replaygain::plage_dynamique` ; ces
//! deux routes n'en sont que la porte HTTP. Elles suivent `POST /system/scan`
//! (202 quand ça part, 409 quand ça court déjà) et
//! `POST /library/lyrics/write` (409 « refusé » qui nomme le réglage manquant).
//!
//! **Pas de porte payante** : la plage dynamique n'en a nulle part ailleurs
//! (ni la cascade de fond, ni `/library/stats/completeness`, ni l'affichage),
//! et la lancer à la main n'en introduit pas une.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::audio::replaygain::plage_dynamique::{Attente, Fin, Refus, Releve};

use crate::state::AppState;

/// Identifiant au registre `background_tasks` — l'indicateur « tâches de fond ».
const TASK_ID: &str = "dynamic_range";

/// Le relevé, dans la forme que le client lit — la même que
/// `GET /system/replaygain/progress`, pour que l'écran Santé traite les deux
/// cartes avec le même code.
///
/// Champs :
/// - `active` : un passage est ouvert. ⚠️ pas « en train de décoder à cette
///   seconde » : voir `waiting_reason` ;
/// - `processed` / `total` / `remaining` : la jauge ;
/// - `lots` : lots de 25 rendus depuis l'ouverture ;
/// - `updated_at` : horodatage unix de la dernière mise à jour ;
/// - `reported` : quelque chose a été lancé depuis le démarrage du serveur.
///   Un `0 / 0` jamais renseigné ne doit pas se lire « rien à mesurer » ;
/// - `waiting_reason` : `"playback"` (une zone joue, #1310), `"thermal"`
///   (machine trop chaude, #1576), `"analysis_slot"` (une autre passe décode,
///   on attend son lot) ou `null` ;
/// - `last_outcome` : comment le dernier passage s'est fini — `"completed"`,
///   `"analysis_disabled"` (réglage coupé en cours de route, #2496),
///   `"stalled"` (les lots ne rendent plus rien) — ou `null`.
fn releve_json(r: &Releve) -> Value {
    json!({
        "active": r.actif,
        "processed": r.traitees,
        "total": r.total,
        "remaining": (r.total - r.traitees).max(0),
        "lots": r.lots,
        "updated_at": r.maj_epoch,
        "reported": r.a_parle(),
        "waiting_reason": r.attente.map(Attente::code),
        "last_outcome": r.derniere_fin.map(Fin::code),
    })
}

/// GET /api/v1/system/dynamic-range/progress
///
/// Le relevé ([`releve_json`]) plus :
/// - `enabled` : l'analyse est armée (`replaygain_mode` ≠ off ET coche non
///   décochée). Un passage ne s'ouvrira pas sans elle, et la carte doit le
///   dire plutôt que d'afficher un bouton qui rend 409 ;
/// - `candidates` : combien de pistes un passage prendrait MAINTENANT.
///   Compté seulement au repos et l'analyse armée — c'est un `COUNT(*)` à
///   cinq `NOT EXISTS`, et l'écran sonde en boucle ; pendant un passage, la
///   jauge le dit déjà (`remaining`). `null` quand il n'est pas compté.
pub(crate) async fn dynamic_range_progress(State(state): State<AppState>) -> Json<Value> {
    let releve = state.passe_dr.releve();
    let enabled = tune_core::audio::replaygain::analysis_enabled(&state.backend);
    let candidates = (!releve.actif && enabled)
        .then(|| tune_core::audio::replaygain::compter_les_candidats_dr(&state.backend));
    let mut body = releve_json(&releve);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("enabled".into(), json!(enabled));
        obj.insert("candidates".into(), json!(candidates));
    }
    Json(body)
}

/// POST /api/v1/system/dynamic-range/analyze
///
/// Lance un passage de mesure de la plage dynamique, tout de suite, sans
/// attendre que la cascade de fond ait fini le ReplayGain et les empreintes.
/// Le passage reprend exactement les pistes que la cascade aurait prises
/// (`rg_analyzed` ou gain lu dans les tags, sans `dr_track`, ni
/// `dr_indisponible`, ni report frais — `CANDIDATS_DR_WHERE`), par lots de
/// 25, sous le même verrou d'analyse : jamais deux décodages en même temps.
///
/// Réponses :
/// - **202** `{"status":"started", …relevé}` — le passage est ouvert ;
///   `total` dit combien de pistes il va prendre ;
/// - **200** `{"status":"nothing_to_do","candidates":0, …relevé}` — l'analyse
///   est armée mais aucune piste n'est candidate : rien n'est ouvert. C'est
///   la réponse d'une bibliothèque déjà mesurée, ou dont les pistes ont
///   toutes été écartées (`dr_indisponible`) ou reportées (#1865) ;
/// - **409** `{"status":"already_running", …relevé}` — un passage court
///   déjà ; le relevé dit où il en est. Pas de second passage ;
/// - **409** `{"status":"refused","reason":"analysis_disabled","detail":…,
///   "setting":"replaygain_source"}` — l'analyse est coupée (#2496). Le DR se
///   mesure sur le même décodage que le ReplayGain : « Désactivé » désactive,
///   y compris à la demande. `detail` nomme le réglage en cause ; `setting`
///   est le champ de `PATCH /system/config` qui l'arme.
///
/// L'avancement se lit sur `GET /system/dynamic-range/progress` et sur le
/// registre `GET /system/background-tasks` (tâche `dynamic_range`).
pub(crate) async fn dynamic_range_analyze(State(state): State<AppState>) -> impl IntoResponse {
    // Compté AVANT d'ouvrir, pour rendre « rien à faire » sans campagne. Le
    // compte de l'ouverture (dans `demarrer`) est le même texte SQL : pas de
    // dérive possible entre les deux.
    if state.passe_dr.releve().actif {
        // Gratuit : un bouton cliqué deux fois ne doit pas payer un COUNT.
        return (
            StatusCode::CONFLICT,
            Json(avec_statut("already_running", &state.passe_dr.releve())),
        );
    }
    if let Some(detail) = tune_core::audio::replaygain::motif_d_inaction(&state.backend) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "refused",
                "reason": "analysis_disabled",
                "detail": detail,
                "setting": "replaygain_source",
            })),
        );
    }
    if tune_core::audio::replaygain::compter_les_candidats_dr(&state.backend) <= 0 {
        let mut body = avec_statut("nothing_to_do", &state.passe_dr.releve());
        if let Some(obj) = body.as_object_mut() {
            obj.insert("candidates".into(), json!(0));
        }
        return (StatusCode::OK, Json(body));
    }

    // Le garde RAII vit dans la fermeture d'avancement : la passe la lâche à
    // sa fin, et le registre retire la tâche à ce moment-là — jamais avant,
    // jamais après (voir `background_tasks.rs`, « phantom perpetual task »).
    let guard = state
        .background_tasks
        .begin(TASK_ID, "Mesure de la plage dynamique…", "analysis");
    let registre = state.background_tasks.clone();
    let sur_avancement = Box::new(move |r: &Releve| {
        let _garde = &guard;
        let detail = match (r.actif, r.attente) {
            (false, _) => "terminé",
            (true, Some(Attente::Lecture)) => "en attente : lecture en cours",
            (true, Some(Attente::Chaleur)) => "en attente : machine trop chaude",
            (true, Some(Attente::Creneau)) => "en attente : une autre analyse décode",
            // #4573 — suspendue à la main depuis l'écran « État du serveur ».
            // Le passage reste OUVERT : sa jauge ne bouge pas et il repartira
            // au même point, d'où « en pause » et non « terminé ».
            (true, Some(Attente::Pause)) => "en pause",
            (true, None) => "Plage dynamique",
        };
        registre.update_progress(
            TASK_ID,
            r.traitees.max(0) as u64,
            r.total.max(0) as u64,
            detail,
        );
    });

    match state
        .passe_dr
        .demarrer(state.backend.clone(), sur_avancement)
    {
        Ok(ouverture) => (
            StatusCode::ACCEPTED,
            Json(avec_statut("started", &ouverture)),
        ),
        Err(Refus::DejaEnCours(releve)) => (
            StatusCode::CONFLICT,
            Json(avec_statut("already_running", &releve)),
        ),
        Err(Refus::AnalyseDesactivee(detail)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "refused",
                "reason": "analysis_disabled",
                "detail": detail,
                "setting": "replaygain_source",
            })),
        ),
    }
}

fn avec_statut(statut: &'static str, r: &Releve) -> Value {
    let mut body = releve_json(r);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("status".into(), json!(statut));
    }
    body
}

#[cfg(test)]
mod tests_4185 {
    use super::*;
    use axum::response::Response;
    use std::time::Duration;
    use tune_core::audio::replaygain::plage_dynamique::Cadence;
    use tune_core::db::settings_repo::SettingsRepo;

    async fn corps(resp: Response) -> (StatusCode, Value) {
        let statut = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (statut, serde_json::from_slice(&bytes).unwrap())
    }

    /// Une base neuve, l'analyse armée, la cadence raccourcie.
    fn etat_arme() -> AppState {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        state.passe_dr.cadence_pour_les_essais(Cadence {
            report_lecture: Duration::from_millis(20),
            report_chaleur: Duration::from_millis(20),
            report_pause: Duration::from_millis(20),
            entre_lots: Duration::from_millis(5),
            garde_thermique: false,
        });
        state
    }

    /// Une piste candidate au DR : analysée avant que le DR n'existe, fichier
    /// ABSENT — elle se reporte (#1865) sans décoder, ce qui suffit à la
    /// route : c'est le lot qui la retire des candidats, pas la mesure.
    fn piste_candidate(state: &AppState, id: i64, chemin: &str) {
        let params: &[&dyn tune_core::db::backend::ToSqlValue] = &[&id, &chemin];
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels) \
                 VALUES (?, 'Joga', ?, 300000, 44100, 2)",
                params,
            )
            .unwrap();
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone())
            .set(id, "rg_analyzed", "1700000000")
            .unwrap();
    }

    fn zone(state: &AppState, etat: &str) {
        state
            .backend
            .execute(
                "INSERT INTO zones (id, name, last_play_state) VALUES (1, 'Salon', ?) \
                 ON CONFLICT(id) DO UPDATE SET last_play_state = excluded.last_play_state",
                &[&etat],
            )
            .unwrap();
    }

    async fn attendre_la_fin(state: &AppState) -> Value {
        for _ in 0..600 {
            if !state.passe_dr.releve().actif {
                let Json(v) = dynamic_range_progress(State(state.clone())).await;
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("le passage n'a pas fini : {:?}", state.passe_dr.releve());
    }

    /// Le refus quand l'analyse n'est pas armée : 409, et le réglage nommé.
    /// Sur une base NEUVE, `replaygain_mode` est absent — c'est le cas de
    /// l'installation qui n'a jamais rien réglé (#2496).
    #[tokio::test]
    async fn analyse_coupee_409_qui_nomme_le_reglage() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        piste_candidate(&state, 42, "/nulle/part.flac");
        let (statut, body) = corps(
            dynamic_range_analyze(State(state.clone()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["status"], "refused");
        assert_eq!(body["reason"], "analysis_disabled");
        assert_eq!(body["setting"], "replaygain_source");
        assert!(
            body["detail"].as_str().unwrap_or("").contains("ABSENT"),
            "le détail doit dire que le mode n'a jamais été réglé : {body}"
        );
        assert!(!state.passe_dr.releve().actif, "un refus n'ouvre rien");

        let Json(p) = dynamic_range_progress(State(state)).await;
        assert_eq!(p["enabled"], false);
        assert_eq!(p["active"], false);
        assert_eq!(p["reported"], false);
        assert_eq!(
            p["candidates"],
            Value::Null,
            "pas compté quand l'analyse est coupée"
        );
    }

    /// Rien à mesurer : 200 `nothing_to_do`, aucun passage ouvert.
    #[tokio::test]
    async fn sans_candidat_200_rien_a_faire() {
        let state = etat_arme();
        let (statut, body) = corps(
            dynamic_range_analyze(State(state.clone()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "nothing_to_do");
        assert_eq!(body["candidates"], 0);
        assert!(!state.passe_dr.releve().actif);
        let Json(p) = dynamic_range_progress(State(state)).await;
        assert_eq!(p["enabled"], true);
        assert_eq!(p["candidates"], 0);
    }

    /// Le cœur de #4185 côté route : 202 au premier appel, 409
    /// `already_running` au second pendant que le passage court, la jauge
    /// qui dit ce qu'il attend, la tâche au registre — puis la fin, et le
    /// passage qui a bien retiré la piste des candidats.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lance_puis_refuse_le_doublon_puis_finit() {
        let state = etat_arme();
        piste_candidate(&state, 42, "/nulle/part.flac");
        piste_candidate(&state, 43, "/nulle/part/2.flac");
        // Une zone joue : le passage s'ouvre mais attend (#1310) — la fenêtre
        // dans laquelle le second appel doit être refusé.
        zone(&state, "playing");

        let (statut, body) = corps(
            dynamic_range_analyze(State(state.clone()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["status"], "started");
        assert_eq!(body["active"], true);
        assert_eq!(
            body["total"], 2,
            "le dénominateur est le compte des candidats"
        );
        assert_eq!(body["processed"], 0);
        assert_eq!(body["remaining"], 2);

        let (statut, body) = corps(
            dynamic_range_analyze(State(state.clone()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::CONFLICT,
            "un second appel pendant le passage doit être refusé, pas doublé (#4185) : {body}"
        );
        assert_eq!(body["status"], "already_running");
        assert_eq!(body["total"], 2);

        // La jauge dit POURQUOI rien n'avance.
        for _ in 0..200 {
            if state.passe_dr.releve().attente.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let Json(p) = dynamic_range_progress(State(state.clone())).await;
        assert_eq!(p["active"], true);
        assert_eq!(p["waiting_reason"], "playback", "{p}");
        assert_eq!(
            p["candidates"],
            Value::Null,
            "pas recompté pendant un passage"
        );
        let taches = state.background_tasks.snapshot();
        assert!(
            taches.iter().any(|t| t.id == TASK_ID),
            "la tâche doit figurer au registre des tâches de fond"
        );

        zone(&state, "stopped");
        let p = attendre_la_fin(&state).await;
        assert_eq!(p["active"], false);
        assert_eq!(p["last_outcome"], "completed", "{p}");
        assert_eq!(p["processed"], 2);
        assert_eq!(p["remaining"], 0);
        assert_eq!(p["reported"], true);
        assert_eq!(p["waiting_reason"], Value::Null);
        // Les deux pistes sont REPORTÉES (#1865), donc plus candidates.
        assert_eq!(p["candidates"], 0);
        for _ in 0..100 {
            if !state
                .background_tasks
                .snapshot()
                .iter()
                .any(|t| t.id == TASK_ID)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !state
                .background_tasks
                .snapshot()
                .iter()
                .any(|t| t.id == TASK_ID),
            "la tâche doit quitter le registre à la fin du passage"
        );

        // Fini : un nouvel appel n'est plus refusé — et n'a plus rien à faire.
        let (statut, body) = corps(
            dynamic_range_analyze(State(state.clone()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "nothing_to_do");
    }
}
