//! `GET /system/replaygain/progress` — où en est la passe ReplayGain (#4144).
//!
//! La carte « ReplayGain » de l'écran Santé affichait `IDLE` pendant qu'une
//! passe de plusieurs heures tournait. Ce n'était pas un défaut du client :
//! `TuneHealthV2.svelte` le disait en toutes lettres — « aucune route
//! d'avancement n'est exposée ». Voici la route.
//!
//! Elle est au scan ce que `/system/scan/status` est au scan : le même couple
//! traitées / total, lisible par sondage. Le fil d'évènements
//! (`library.replaygain.progress`) porte les mêmes chiffres pour qui écoute le
//! WebSocket ; la route existe pour ceux qui n'écoutent pas, et pour le premier
//! affichage, qui arrive toujours avant le premier évènement.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::state::AppState;

/// L'avancement de la passe ReplayGain.
///
/// Champs :
/// - `active` : une campagne est ouverte. ⚠️ pas « en train de décoder à cette
///   seconde » : la passe cède à la lecture et à la garde thermique sans
///   refermer sa campagne ;
/// - `processed` / `total` : le couple de la jauge ;
/// - `remaining` : ce qui reste, dérivé du couple quand la passe a parlé ;
/// - `reported` : la passe a-t-elle annoncé quelque chose depuis le démarrage.
///   C'est ce champ qui distingue « rien à faire » d'un serveur qui vient de
///   démarrer — les deux valent `0 / 0`, et les confondre afficherait une
///   bibliothèque entièrement analysée sur une machine qui n'a encore rien fait ;
/// - `enabled` : l'analyse est armée. Une passe désarmée n'avancera pas, et la
///   carte doit le dire plutôt que d'afficher une jauge immobile ;
/// - `library_analyzed` / `library_eligible` (#5597) : l'avancement de la
///   BIBLIOTHÈQUE — pistes avec un fichier qui portent `rg_analyzed` ou
///   `rg_track_gain`, sur pistes avec un fichier. Lus en base, ils survivent à
///   un redémarrage, contrairement à `processed` / `total` qui repartent de
///   zéro à chaque campagne. Comptés au plus une fois par minute (cache de
///   l'état), `null` quand l'analyse est coupée ou que la requête échoue.
pub(crate) async fn replaygain_progress(State(state): State<AppState>) -> Json<Value> {
    let avancement = tune_core::audio::replaygain::progression::releve();
    let enabled = tune_core::audio::replaygain::analysis_enabled(&state.backend);

    // Le dénominateur AVANT que la passe ait parlé — le seul moment où il n'est
    // pas déjà dans l'instantané. Deux cas : le serveur vient de démarrer (la
    // passe dort 120 s avant son premier lot), ou elle est désarmée.
    //
    // On ne le compte QUE dans ce cas, et jamais quand l'analyse est coupée :
    // c'est un `COUNT(*)` avec trois `NOT EXISTS` sur `track_metadata`, et
    // l'écran Santé sonde en boucle. Le payer à chaque sondage pendant qu'une
    // fonction est simplement éteinte serait une charge permanente pour un
    // chiffre que personne n'attend.
    let (processed, total) = if avancement.a_parle() {
        (avancement.traitees, avancement.total)
    } else if enabled {
        (
            0,
            tune_core::audio::replaygain::compter_les_candidats_replaygain(&state.backend),
        )
    } else {
        (0, 0)
    };

    // Pistes tenues à l'écart parce que leur fichier ne répond pas (#1865).
    // Elles ne sont ni dans `total` ni dans `processed` : sans ce champ, une
    // bibliothèque entière sur un partage démonté se lisait « terminée »
    // (#4254). `waiting_reason` ne s'allume que si c'est la SEULE chose qui
    // reste — même contrat que `/library/search/acoustic/status` (#4187).
    let deferred = if enabled {
        tune_core::audio::replaygain::compter_les_reportees_par_chemin(&state.backend)
    } else {
        0
    };
    let remaining = (total - processed).max(0);
    // #5597 — la jauge de campagne repart de 0 à chaque démarrage et se lisait
    // comme une perte de travail. Le couple de la bibliothèque, lui, est lu en
    // base. Mis en cache : l'écran sonde en boucle, le comptage parcourt
    // toute la table `tracks` (528 000 pistes chez un testeur).
    let bibliotheque = if enabled {
        state.bibliotheque_rg.lire(&state.backend)
    } else {
        None
    };
    let waiting_reason = (remaining == 0 && deferred > 0).then_some("unresolved_paths");
    Json(json!({
        "active": avancement.actif,
        "processed": processed,
        "total": total,
        "remaining": remaining,
        "deferred": deferred,
        "waiting_reason": waiting_reason,
        "updated_at": avancement.maj_epoch,
        "reported": avancement.a_parle(),
        "enabled": enabled,
        "library_analyzed": bibliotheque.map(|b| b.analysees),
        "library_eligible": bibliotheque.map(|b| b.eligibles),
        // #5519 / tune-web-client#1828 — la passe DÉCODE-t-elle en ce moment ?
        // `active` dit seulement qu'une campagne est ouverte : elle le reste
        // quand la plage dynamique « En premier » passe devant, et la carte
        // affichait « en cours » sur une jauge figée.
        "working": avancement.actif
            && tune_core::taches_de_fond::ordre::rang_au_travail()
                == Some(tune_core::taches_de_fond::ordre::Rang::ReplayGain),
    }))
}

/// Le relevé de la remesure (#5882) : combien de mesures de Tune sont
/// d'avant la version courante, et si une campagne les rend à la passe.
/// Compté sur le pool bloquant : le `COUNT` parcourt `tracks`.
async fn releve_de_remesure(state: &AppState) -> Value {
    let backend = state.backend.clone();
    let perimees = tokio::task::spawn_blocking(move || {
        tune_core::audio::replaygain::remesure::compter_les_mesures_perimees(&backend)
    })
    .await
    .ok()
    .flatten();
    json!({
        "stale": perimees,
        "running": tune_core::audio::replaygain::remesure::en_cours(),
        "algo": tune_core::audio::replaygain::RG_ALGO,
        "enabled": tune_core::audio::replaygain::analysis_enabled(&state.backend),
    })
}

/// `GET /system/replaygain/reanalyze` — le relevé de la remesure.
///
/// - `stale` : mesures ReplayGain de Tune d'avant `algo` (sans version, ou
///   d'une version plus ancienne), périmètre des analyses compris. `null` si
///   le comptage échoue ;
/// - `running` : une campagne les rend à la passe, par lots ;
/// - `algo` : la version courante de la mesure ;
/// - `enabled` : l'analyse ReplayGain est armée. Sans elle, rien ne
///   remesurerait.
pub(crate) async fn replaygain_reanalyze_status(State(state): State<AppState>) -> Json<Value> {
    Json(releve_de_remesure(&state).await)
}

/// `POST /system/replaygain/reanalyze` — refaire les mesures ReplayGain et
/// true peak prises avant le correctif des jonctions de segments (#5882).
///
/// Rien n'est mesuré ici et aucun fichier audio n'est écrit : la campagne
/// efface en base, par lots, les mesures périmées, et la passe ReplayGain les
/// refait à son rythme. Les gains lus dans les tags ne sont jamais touchés.
///
/// Réponses :
/// - **202** `{"status":"started", …relevé}` ;
/// - **200** `{"status":"nothing_to_do", …relevé}` : aucune mesure périmée ;
/// - **409** `{"status":"already_running", …relevé}` ;
/// - **409** `{"status":"analysis_disabled", …relevé}` : l'analyse ReplayGain
///   est coupée, rien ne remesurerait ;
/// - **500** `{"status":"error", "error": …}`.
pub(crate) async fn replaygain_reanalyze(
    State(state): State<AppState>,
) -> (axum::http::StatusCode, Json<Value>) {
    use axum::http::StatusCode;
    use tune_core::audio::replaygain::remesure;

    let avec_statut = |statut: &str, mut releve: Value| {
        if let Some(obj) = releve.as_object_mut() {
            obj.insert("status".into(), json!(statut));
        }
        Json(releve)
    };
    if remesure::en_cours() {
        let releve = releve_de_remesure(&state).await;
        return (StatusCode::CONFLICT, avec_statut("already_running", releve));
    }
    let releve = releve_de_remesure(&state).await;
    if !tune_core::audio::replaygain::analysis_enabled(&state.backend) {
        return (
            StatusCode::CONFLICT,
            avec_statut("analysis_disabled", releve),
        );
    }
    if releve.get("stale").and_then(Value::as_i64) == Some(0) {
        return (StatusCode::OK, avec_statut("nothing_to_do", releve));
    }
    let backend = state.backend.clone();
    match tokio::task::spawn_blocking(move || remesure::demander(&backend)).await {
        Ok(Ok(remesure::Demande::Lancee)) => {
            let mut releve = releve;
            if let Some(obj) = releve.as_object_mut() {
                obj.insert("running".into(), json!(true));
            }
            (StatusCode::ACCEPTED, avec_statut("started", releve))
        }
        Ok(Ok(remesure::Demande::DejaEnCours)) => {
            (StatusCode::CONFLICT, avec_statut("already_running", releve))
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "error": e})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "error": e.to_string()})),
        ),
    }
}
