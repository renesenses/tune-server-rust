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
/// * **`hires_check`** — un lot de la passe des faux hi-res décode (#6285).
pub(crate) fn etat_de(state: &AppState, tache: Tache) -> Etat {
    if est_en_pause(tache) {
        return Etat::EnPause;
    }
    let en_cours = match tache {
        Tache::ReplayGain | Tache::Empreintes | Tache::PlageDynamique => {
            cascade_en_cours(tache, tune_core::taches_de_fond::ordre::rang_au_travail())
                || (tache == Tache::PlageDynamique
                    && (state.passe_dr.releve().actif
                        || tune_core::taches_de_fond::rapports_dr::en_cours()))
        }
        // Le module `embedding` est derrière `audio-embedding` : sans la
        // feature, la passe acoustique n'existe pas, donc elle ne tourne pas.
        // La suspendre reste possible et sans effet — plutôt qu'un identifiant
        // qui disparaîtrait du relevé selon la recette de compilation, ce que
        // le client n'a aucun moyen de deviner.
        #[cfg(feature = "audio-embedding")]
        Tache::Acoustique => tune_core::audio::embedding::balayage_acoustique_en_cours(),
        #[cfg(not(feature = "audio-embedding"))]
        Tache::Acoustique => false,
        // #6282 — la table des inscriptions est celle du drapeau `paused` des
        // tâches (`background_tasks::tache_suspendable`) : une seule source.
        Tache::Enrichissement
        | Tache::ImagesArtistes
        | Tache::Identification
        | Tache::Integrite => inscrite(state, tache),
        // #6285 — la passe des faux hi-res : un lot décode-t-il.
        Tache::FauxHiRes => tune_core::audio::faux_hires_passe::lot_en_cours(),
    };
    if en_cours {
        Etat::EnCours
    } else {
        Etat::AuRepos
    }
}

/// Un traitement de la cascade travaille-t-il, selon le rang qui décode ?
///
/// #5519 / tune-web-client#1828 — le signal était l'avancement ReplayGain
/// (`progression::releve().actif`), c'est-à-dire « une campagne ReplayGain est
/// OUVERTE ». Elle le reste tant qu'il reste des pistes, y compris quand la
/// plage dynamique « En premier » tient le créneau : les trois cartes disaient
/// alors « en cours » ensemble. Le rang au travail dit QUI décode.
///
/// La passe ReplayGain pose aussi l'empreinte et la plage dynamique des pistes
/// qu'elle décode : elle fait donc avancer les trois.
pub(crate) fn cascade_en_cours(
    tache: Tache,
    rang: Option<tune_core::taches_de_fond::ordre::Rang>,
) -> bool {
    use tune_core::taches_de_fond::ordre::Rang;
    match (tache, rang) {
        (_, None) => false,
        (_, Some(Rang::ReplayGain)) => matches!(
            tache,
            Tache::ReplayGain | Tache::Empreintes | Tache::PlageDynamique
        ),
        (Tache::Empreintes, Some(Rang::Empreintes)) => true,
        (Tache::PlageDynamique, Some(Rang::PlageDynamique)) => true,
        _ => false,
    }
}

/// Une tâche relevant de ce traitement est-elle inscrite au registre ?
fn inscrite(state: &AppState, tache: Tache) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| crate::background_tasks::tache_suspendable(&t.id) == Some(tache))
}

/// Le bloc que toutes les routes de cette famille rendent, `GET` comprise.
///
/// Un seul corps, écrit UNE fois : deux formes divergeraient dès la première
/// tâche ajoutée, et l'écran lirait un état sur le `GET` et un autre après son
/// propre clic.
pub(crate) fn instantane(state: &AppState) -> Value {
    // #4681 — la priorité à la lecture : lue UNE fois, pour que chaque carte
    // et le bloc d'ensemble disent la même chose.
    let priorite = tune_core::taches_de_fond::priorite::releve();
    let traitements: Vec<Value> = Tache::TOUTES
        .into_iter()
        .map(|tache| {
            json!({
                "id": tache.id(),
                "state": etat_de(state, tache).code(),
                "paused": est_en_pause(tache),
                // A cédé à la lecture dans la fenêtre récente : ralentie, ou
                // arrêtée jusqu'à l'arrêt de la lecture pour les passes qui
                // décodent.
                "throttled_for_playback": priorite.throttled.contains(&tache.id()),
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
        // Le scan cède pourtant à la lecture comme les autres (#4681).
        "scan": {
            "running": tune_core::scanner::activite::scan_bibliotheque_en_cours(),
            "throttled_for_playback": priorite
                .throttled
                .contains(&tune_core::taches_de_fond::priorite::ID_SCAN),
        },
        "playback_priority": priorite,
        // #5169 — la place de la plage dynamique parmi les passes qui
        // décodent. Absent d'un serveur antérieur : le client cache alors le
        // choix plutôt que d'en proposer un qui n'aurait aucun effet.
        "dynamic_range_priority": tune_core::taches_de_fond::ordre::priorite_dr().id(),
        "dynamic_range_priority_choices": tune_core::taches_de_fond::ordre::PrioriteDr::TOUTES
            .map(|p| p.id()),
        // #5168 — le rattrapage des rapports `foo_dr.txt` : tourne-t-il, et
        // qu'a fait son dernier passage.
        "dynamic_range_sidecar": tune_core::taches_de_fond::rapports_dr::releve(),
        // #4805 (idée 4) — l'identification par empreinte AcoustID peut-elle
        // tourner ? Sans `fpcalc` (Tune OS aujourd'hui) ou sans clé, la passe
        // est désactivée et ce bloc dit pourquoi, pour l'écran Santé.
        "acoustid": crate::routes::library::identification_lot::acoustid::disponibilite(state),
    })
}

/// `POST /system/background-tasks/dynamic-range-priority` — corps
/// `{"priority": "last" | "before_fingerprints" | "first"}` (#5169).
///
/// Un mot inconnu rend **400** en nommant les mots admis : retomber en silence
/// sur le défaut laisserait l'écran afficher un choix qui n'a rien changé.
/// Rend l'instantané complet, comme les routes de pause.
pub(crate) async fn fixer_priorite_dr(
    State(state): State<AppState>,
    Json(corps): Json<Value>,
) -> impl IntoResponse {
    use tune_core::taches_de_fond::ordre::{PrioriteDr, fixer_priorite_dr};
    let demande = corps.get("priority").and_then(Value::as_str).unwrap_or("");
    let Some(priorite) = PrioriteDr::depuis_id(demande) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "unknown_priority",
                "requested": demande,
                "known": PrioriteDr::TOUTES.map(|p| p.id()),
            })),
        )
            .into_response();
    };
    match fixer_priorite_dr(&state.backend, priorite) {
        Ok(()) => reponse(&state),
        Err(e) => echec(e).into_response(),
    }
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
        Ok(()) => {
            // #5469 — lever le drapeau ne suffit pas quand la passe n'existe
            // plus : un arrêt du serveur l'a tuée, et « Reprendre » n'avait
            // plus rien à reprendre. Le bouton en relance une.
            let relancees = crate::reprise_des_passes::relancer_pour(
                &state,
                tache,
                crate::reprise_des_passes::Origine::Reprendre,
            )
            .await;
            reponse_avec_relances(&state, relancees)
        }
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
    match reprendre_et_relancer_tout(&state).await {
        Ok(relancees) => reponse_avec_relances(&state, relancees),
        Err(e) => echec(e).into_response(),
    }
}

/// Lever toutes les pauses, puis relancer les passes que l'arrêt a tuées.
async fn reprendre_et_relancer_tout(state: &AppState) -> Result<Vec<&'static str>, String> {
    tune_core::taches_de_fond::tout_reprendre(&state.backend)?;
    // #5469 — même règle que la reprise d'UN traitement.
    let mut relancees = Vec::new();
    for tache in Tache::TOUTES {
        relancees.extend(
            crate::reprise_des_passes::relancer_pour(
                state,
                tache,
                crate::reprise_des_passes::Origine::Reprendre,
            )
            .await,
        );
    }
    Ok(relancees)
}

/// `POST /system/background-tasks/relaunch-all` — « Tout relancer » (#6330).
///
/// Benjithom, fil 2270 : « Tout relancer » ne faisait rien sur un ReplayGain
/// suspendu à 99 % et une plage dynamique suspendue à 95 %. Le bouton ne
/// connaissait que les trois routes de LANCEMENT (plage dynamique,
/// métadonnées, images d'artistes), écartait tout ce qui était en pause, et
/// rien ne rendait à la passe une piste en échec de mesure.
///
/// Cette route fait les trois choses que le mot promet, dans cet ordre :
///
/// 1. **reprendre** tous les traitements suspendus — ReplayGain compris — et
///    relancer les passes qu'un arrêt a tuées (même chemin que
///    `resume-all`, #5469) ;
/// 2. **rendre à la cascade les pistes en échec** : témoins ReplayGain sans
///    gain, marques « plage dynamique indisponible », reports de chemin
///    ([`tune_core::audio::replaygain::reessai`]). Aucune mesure réussie
///    n'est effacée ;
/// 3. **réveiller** la cascade si elle dort au repos.
///
/// Les passes à lancer (plage dynamique à la demande, métadonnées, images
/// d'artistes) gardent leurs propres routes : le client les appelle ensuite,
/// comme avant, pour celles qui étaient au repos.
///
/// Rend l'instantané, plus `resumed` (les traitements qui étaient suspendus),
/// `relaunched` (#5469) et `retried` (les pistes rendues, par passe).
pub(crate) async fn tout_relancer(State(state): State<AppState>) -> impl IntoResponse {
    let suspendus: Vec<&'static str> = tune_core::taches_de_fond::suspendus()
        .into_iter()
        .map(Tache::id)
        .collect();
    let relancees = match reprendre_et_relancer_tout(&state).await {
        Ok(r) => r,
        Err(e) => return echec(e).into_response(),
    };
    let backend = state.backend.clone();
    let rendues = match tokio::task::spawn_blocking(move || {
        tune_core::audio::replaygain::reessai::rendre_les_pistes_en_echec(&backend)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return echec(e).into_response(),
        Err(e) => return echec(e.to_string()).into_response(),
    };
    // La jauge ReplayGain ne doit pas resservir les échecs d'avant le clic.
    state.bibliotheque_rg.oublier();
    tracing::info!(
        suspendus = ?suspendus,
        relancees = ?relancees,
        pistes_replaygain = rendues.replaygain,
        pistes_plage_dynamique = rendues.dynamic_range,
        reports = rendues.deferred,
        "tout_relancer"
    );
    let mut corps = instantane(&state);
    corps["relaunched"] = json!(relancees);
    corps["resumed"] = json!(suspendus);
    corps["retried"] = json!(rendues);
    state
        .event_bus
        .emit("system.background_tasks", corps.clone());
    (StatusCode::OK, Json(corps)).into_response()
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

/// La réponse des deux routes de reprise : l'instantané, plus `relaunched`,
/// les passes que le clic a relancées faute de les trouver vivantes (#5469).
/// Une liste vide veut dire qu'aucune passe n'était morte, ou qu'aucune n'avait
/// de travail interrompu à reprendre.
fn reponse_avec_relances(
    state: &AppState,
    relancees: Vec<&'static str>,
) -> axum::response::Response {
    let mut corps = instantane(state);
    corps["relaunched"] = json!(relancees);
    state
        .event_bus
        .emit("system.background_tasks", corps.clone());
    (StatusCode::OK, Json(corps)).into_response()
}

#[cfg(test)]
mod tests_5519 {
    use super::*;
    use tune_core::taches_de_fond::ordre::Rang;

    /// tune-web-client#1828 — avec la plage dynamique « En premier », le
    /// rattrapage DR décode pendant que la campagne ReplayGain reste ouverte :
    /// la carte ReplayGain ne doit PAS dire « en cours », la carte DR si.
    #[test]
    fn le_rang_au_travail_dit_qui_est_en_cours() {
        let dr = Some(Rang::PlageDynamique);
        assert!(!cascade_en_cours(Tache::ReplayGain, dr));
        assert!(!cascade_en_cours(Tache::Empreintes, dr));
        assert!(cascade_en_cours(Tache::PlageDynamique, dr));

        // La passe ReplayGain pose aussi empreinte et plage dynamique.
        let rg = Some(Rang::ReplayGain);
        for t in [Tache::ReplayGain, Tache::Empreintes, Tache::PlageDynamique] {
            assert!(cascade_en_cours(t, rg), "{t:?}");
        }

        let emp = Some(Rang::Empreintes);
        assert!(cascade_en_cours(Tache::Empreintes, emp));
        assert!(!cascade_en_cours(Tache::ReplayGain, emp));
        assert!(!cascade_en_cours(Tache::PlageDynamique, emp));

        for t in [Tache::ReplayGain, Tache::Empreintes, Tache::PlageDynamique] {
            assert!(!cascade_en_cours(t, None), "{t:?}");
        }
    }
}

#[cfg(test)]
mod tests_5169 {
    use super::*;
    use tune_core::taches_de_fond::ordre::{PrioriteDr, priorite_dr};

    async fn corps(resp: axum::response::Response) -> (StatusCode, Value) {
        let statut = resp.status();
        let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (statut, serde_json::from_slice(&octets).unwrap())
    }

    /// Le réglage d'ordre se lit dans l'instantané, se change par la route, et
    /// un mot inconnu est REFUSÉ en nommant les mots admis — jamais un 200 qui
    /// retomberait en silence sur le défaut.
    #[tokio::test]
    async fn la_priorite_de_la_plage_dynamique_se_lit_et_se_change() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        tune_core::taches_de_fond::ordre::oublier_pour_les_essais();
        assert_eq!(instantane(&state)["dynamic_range_priority"], "last");
        assert_eq!(
            instantane(&state)["dynamic_range_priority_choices"],
            json!(["last", "before_fingerprints", "first"])
        );

        let (statut, v) = corps(
            fixer_priorite_dr(State(state.clone()), Json(json!({"priority": "avant"})))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["known"], json!(["last", "before_fingerprints", "first"]));
        assert_eq!(
            priorite_dr(),
            PrioriteDr::Derniere,
            "un refus ne change rien"
        );

        let (statut, v) = corps(
            fixer_priorite_dr(State(state.clone()), Json(json!({"priority": "first"})))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["dynamic_range_priority"], "first", "{v}");
        assert_eq!(
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
                .get(tune_core::taches_de_fond::ordre::CLE_REGLAGE)
                .unwrap()
                .as_deref(),
            Some("first"),
            "le réglage est PERSISTÉ, pas seulement tenu en mémoire"
        );
        assert!(v["dynamic_range_sidecar"].is_object(), "{v}");
        tune_core::taches_de_fond::ordre::oublier_pour_les_essais();
    }
}

#[cfg(test)]
mod tests_6286 {
    use super::*;

    /// #6286 — l'audit d'intégrité s'inscrit au registre sous `integrity` : sa
    /// carte le dit en cours, et sa tâche se dit GARÉE quand l'audit est en
    /// pause, comme celles des autres traitements suspendables (#6282).
    #[tokio::test]
    async fn l_audit_inscrit_au_registre_se_dit_en_cours_puis_en_pause() {
        let _pauses = crate::pauses_de_fond_tests::isoler().await;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        assert!(!matches!(etat_de(&state, Tache::Integrite), Etat::EnCours));

        let _suivi = state
            .background_tasks
            .begin("integrity", "Audit d’intégrité", "integrity");
        assert!(matches!(etat_de(&state, Tache::Integrite), Etat::EnCours));
        let tache = |s: &AppState| {
            s.background_tasks
                .snapshot()
                .into_iter()
                .find(|t| t.id == "integrity")
                .expect("tâche inscrite")
        };
        assert!(!tache(&state).paused);

        tune_core::taches_de_fond::mettre_en_pause(&state.backend, Tache::Integrite).unwrap();
        assert!(
            tache(&state).paused,
            "l'audit en pause : sa tâche du registre doit le dire"
        );
    }
}
