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
