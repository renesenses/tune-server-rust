//! `GET /system/background-tasks/running` — « ce qui tourne » (#4681).
//!
//! Arbitrage de Bertrand du 10/10 : un écran, dans État du serveur, qui dit
//! ce qui tourne en fond pendant une écoute, pour rapprocher une micro-coupure
//! de sa cause. La cession à la lecture existe depuis la rc4 (#5942) ;
//! `GET /system/background-tasks` en publiait les pièces (état des cartes,
//! `throttled_for_playback`, relevé des freins), mais ni l'AVANCEMENT des
//! passes, ni la garde THERMIQUE, qui arrête ou bride les mêmes passes.
//!
//! Cette route rassemble en un corps ce que l'écran doit montrer :
//!
//! * **`passes`** — une ligne par traitement de fond (les identifiants de
//!   [`Tache::id`], plus `scan`), avec :
//!   * `activity` / `reason` : les codes de l'étape 4 du chantier « priorité
//!     de la lecture » (`docs/chantier-priorite-lecture.md` sur
//!     `chantier/priorite-lecture`, écran tune-web-client#2185) —
//!     `working`, `throttled` (avance au ralenti pendant la lecture),
//!     `yielded` (une passe qui décode a tout cédé à la lecture), `paused`
//!     (`user_pause`), `idle` ; plus `hot` (`reason: "thermal"`) quand la
//!     garde thermique la retient ;
//!   * `progress` : `{processed, total, detail}` quand la passe publie un
//!     avancement, `null` sinon (l'analyse acoustique et le scan n'en publient
//!     pas sans requête) ;
//! * **`running`** — les identifiants qui ne sont ni au repos ni en pause ;
//!   **`throttled_for_playback`** — ceux que la lecture ralentit ou arrête ;
//! * **`playback`** — les zones qui jouent, depuis quand, les freins ;
//! * **`thermal`** — la température telle que la garde la voit, la largeur
//!   des lots réglée et effective, et le bridage. #6450 (décision du 20/10) :
//!   quand la température est illisible, le bridage à UN fichier à la fois
//!   est MAINTENU ; la route le dit (`throttled: true`, `reason`).
//!
//! Rien n'est lu en base hormis le réglage de vitesse (une clé primaire) :
//! l'écran sonde cette route en boucle, comme `GET /system/background-tasks`.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::audio::thermal::{self, LectureThermique};
use tune_core::taches_de_fond::priorite::{ID_SCAN, ReleveDePriorite};
use tune_core::taches_de_fond::vitesse::Vitesse;
use tune_core::taches_de_fond::{Etat, Tache};

use crate::state::AppState;

/// Ce qu'une passe fait MAINTENANT. Mêmes codes que l'étape 4 du chantier
/// « priorité de la lecture », plus [`Activite::Retenue`] pour la chaleur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activite {
    EnPause,
    Retenue,
    Cedee,
    Ralentie,
    AuTravail,
    AuRepos,
}

impl Activite {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Activite::EnPause => "paused",
            Activite::Retenue => "hot",
            Activite::Cedee => "yielded",
            Activite::Ralentie => "throttled",
            Activite::AuTravail => "working",
            Activite::AuRepos => "idle",
        }
    }

    pub(crate) fn raison(self) -> Option<&'static str> {
        match self {
            Activite::EnPause => Some("user_pause"),
            Activite::Retenue => Some("thermal"),
            Activite::Cedee | Activite::Ralentie => Some("playback"),
            Activite::AuTravail | Activite::AuRepos => None,
        }
    }

    /// La passe a du travail en cours (même arrêté par la lecture ou la
    /// chaleur) : elle « tourne », au sens de l'écran.
    fn tourne(self) -> bool {
        !matches!(self, Activite::EnPause | Activite::AuRepos)
    }

    /// La lecture la ralentit ou l'arrête.
    fn freinee_par_la_lecture(self) -> bool {
        matches!(self, Activite::Cedee | Activite::Ralentie)
    }
}

/// Les passes qui DÉCODENT cèdent tout à la lecture ; les autres ralentissent.
fn cede_tout(id: &str) -> bool {
    [
        Tache::ReplayGain.id(),
        Tache::Empreintes.id(),
        Tache::PlageDynamique.id(),
        Tache::Acoustique.id(),
        // #6285 — la passe des faux hi-res décode un extrait par piste.
        Tache::FauxHiRes.id(),
        // #6286 — l'audit d'intégrité décode chaque fichier en entier.
        Tache::Integrite.id(),
    ]
    .contains(&id)
}

/// Les passes qu'une garde thermique retient, d'après le nom de la garde
/// (`ThermalGate::should_hold`). La garde `replaygain` tient toute la
/// cascade : ReplayGain, empreintes et rattrapage de la plage dynamique.
fn retenue_par_la_chaleur(id: &str, gardes_chaudes: &[String]) -> bool {
    gardes_chaudes.iter().any(|g| match g.as_str() {
        "replaygain" => [
            Tache::ReplayGain.id(),
            Tache::Empreintes.id(),
            Tache::PlageDynamique.id(),
        ]
        .contains(&id),
        "dynamic_range" => id == Tache::PlageDynamique.id(),
        "acoustique" => id == Tache::Acoustique.id(),
        // #6285 — `faux_hires_passe` interroge la garde sous ce nom.
        "faux_hires" => id == Tache::FauxHiRes.id(),
        _ => false,
    })
}

/// L'activité d'une passe. L'ordre dit ce qui l'emporte : la pause de
/// l'utilisateur, puis la chaleur (elle arrête même hors lecture), puis la
/// lecture — une cession notée ne vaut que tant qu'une zone joue.
pub(crate) fn activite_de(
    id: &str,
    en_pause: bool,
    en_cours: bool,
    priorite: &ReleveDePriorite,
    gardes_chaudes: &[String],
) -> Activite {
    if en_pause {
        return Activite::EnPause;
    }
    if retenue_par_la_chaleur(id, gardes_chaudes) {
        return Activite::Retenue;
    }
    if priorite.playback_active && priorite.throttled.contains(&id) {
        return if cede_tout(id) {
            Activite::Cedee
        } else {
            Activite::Ralentie
        };
    }
    if en_cours {
        Activite::AuTravail
    } else {
        Activite::AuRepos
    }
}

/// L'avancement publié d'une passe, sans requête.
fn avancement(state: &AppState, tache: Tache) -> Value {
    let jauge = |processed: i64, total: i64, detail: &str| {
        json!({
            "processed": processed.max(0),
            "total": total.max(0),
            "detail": detail,
        })
    };
    match tache {
        // La cascade : une seule campagne, un seul avancement pour les trois
        // rangs (`routes/system/taches_de_fond.rs`, `cascade_en_cours`).
        Tache::ReplayGain | Tache::Empreintes => {
            let a = tune_core::audio::replaygain::progression::releve();
            if a.a_parle() {
                jauge(a.traitees, a.total, "cascade")
            } else {
                Value::Null
            }
        }
        Tache::PlageDynamique => {
            let dr = state.passe_dr.releve();
            if dr.actif {
                return jauge(dr.traitees, dr.total, "on_demand");
            }
            let a = tune_core::audio::replaygain::progression::releve();
            if a.actif {
                jauge(a.traitees, a.total, "cascade")
            } else {
                Value::Null
            }
        }
        Tache::Acoustique => Value::Null,
        // #6285 — la passe ne publie pas d'avancement sans requête (le
        // nombre de candidats se compte en base).
        Tache::FauxHiRes => Value::Null,
        // #6286 — l'avancement de l'audit se lit sur `GET /library/integrity`
        // (fichiers traités et total), pas au registre des tâches.
        Tache::Integrite => Value::Null,
        Tache::Enrichissement | Tache::ImagesArtistes | Tache::Identification => state
            .background_tasks
            .snapshot()
            .into_iter()
            .filter(|t| crate::background_tasks::tache_suspendable(&t.id) == Some(tache))
            .find_map(|t| t.progress.map(|p| (t.id, p)))
            .map(|(id, p)| {
                json!({
                    "processed": p.processed,
                    "total": p.total,
                    "detail": if p.detail.is_empty() { id } else { p.detail },
                })
            })
            .unwrap_or(Value::Null),
    }
}

/// Le bloc `thermal`, en logique pure : la route lui passe la lecture de la
/// garde, et les témoins une lecture choisie.
///
/// `lecture` vaut `None` quand le capteur n'a pas répondu à temps : la route
/// ne bloque pas l'écran sur une requête PDH lente, et ne prétend rien.
///
/// `bridage_leve` : le réglage de #6713 (`vitesse::bridage_leve`), qui rend
/// la vitesse réglée quand la température est illisible.
pub(crate) fn bloc_thermique(
    lecture: Option<LectureThermique>,
    vitesse: Vitesse,
    coeurs: usize,
    gardes_chaudes: &[String],
    bridage_leve: bool,
) -> Value {
    let reglee = vitesse.largeur(coeurs);
    let effective = lecture.map(|l| thermal::largeur_selon_la_chaleur(reglee, &l, bridage_leve));
    let motif = lecture.and_then(|l| l.motif_du_bridage()).map(|m| m.id());
    json!({
        "readable": lecture.map(|l| l.celsius().is_some()),
        "temp_c": lecture
            .and_then(|l| l.celsius())
            .map(|c| (c * 10.0).round() / 10.0),
        // #6450 — sans température lisible, les analyses sont bridées à
        // `width_effective` fichier(s) à la fois, quelle que soit la vitesse.
        // #6713 — sauf si l'utilisateur a levé ce bridage : `throttled` est
        // alors faux, `reason` dit toujours pourquoi la température manque,
        // et `throttle_lifted_by_user` le dit.
        "throttled": motif.is_some() && !bridage_leve,
        "reason": motif,
        "throttle_lifted_by_user": motif.is_some() && bridage_leve,
        "unthrottle_unreadable_temperature": bridage_leve,
        "speed": vitesse.id(),
        "width_configured": reglee,
        "width_effective": effective,
        // Au-delà de `pause_above_c`, la garde ARRÊTE les passes qui
        // décodent, jusqu'au retour sous `resume_below_c`.
        "held_passes": gardes_chaudes,
        "pause_above_c": thermal::PAUSE_ABOVE_C,
        "resume_below_c": thermal::RESUME_BELOW_C,
    })
}

/// La lecture de la garde, bornée à 300 ms comme `GET /system/diagnostics`.
async fn lecture_bornee() -> Option<LectureThermique> {
    let lecture = tokio::task::spawn_blocking(thermal::lecture_thermique);
    match tokio::time::timeout(std::time::Duration::from_millis(300), lecture).await {
        Ok(Ok(l)) => Some(l),
        _ => None,
    }
}

/// Le corps de la route, la lecture thermique fournie.
pub(crate) fn ce_qui_tourne(state: &AppState, lecture: Option<LectureThermique>) -> Value {
    let priorite = tune_core::taches_de_fond::priorite::releve();
    let gardes_chaudes = thermal::passes_retenues_par_la_chaleur();

    let mut passes: Vec<Value> = Vec::new();
    let mut tournent: Vec<&'static str> = Vec::new();
    let mut freinees: Vec<&'static str> = Vec::new();
    for tache in Tache::TOUTES {
        let etat = super::taches_de_fond::etat_de(state, tache);
        let activite = activite_de(
            tache.id(),
            etat == Etat::EnPause,
            etat == Etat::EnCours,
            &priorite,
            &gardes_chaudes,
        );
        if activite.tourne() {
            tournent.push(tache.id());
        }
        if activite.freinee_par_la_lecture() {
            freinees.push(tache.id());
        }
        passes.push(json!({
            "id": tache.id(),
            "activity": activite.code(),
            "reason": activite.raison(),
            "progress": avancement(state, tache),
        }));
    }
    // Le scan n'est pas suspendable, mais il cède à la lecture comme les
    // autres (#4681).
    let activite_scan = activite_de(
        ID_SCAN,
        false,
        tune_core::scanner::activite::scan_bibliotheque_en_cours(),
        &priorite,
        &gardes_chaudes,
    );
    if activite_scan.tourne() {
        tournent.push(ID_SCAN);
    }
    if activite_scan.freinee_par_la_lecture() {
        freinees.push(ID_SCAN);
    }
    passes.push(json!({
        "id": ID_SCAN,
        "activity": activite_scan.code(),
        "reason": activite_scan.raison(),
        "progress": Value::Null,
    }));

    json!({
        "running": tournent,
        "throttled_for_playback": freinees,
        "passes": passes,
        "playback": {
            "active": priorite.playback_active,
            "playing_zone_ids": priorite.playing_zone_ids,
            "since_epoch_s": priorite.since_epoch_s,
            "brakes_enabled": priorite.policy.enabled,
            "pause_between_items_ms": priorite.pause_between_items_ms,
        },
        "thermal": bloc_thermique(
            lecture,
            tune_core::taches_de_fond::vitesse::vitesse(&state.backend),
            tune_core::taches_de_fond::vitesse::coeurs(),
            &gardes_chaudes,
            tune_core::taches_de_fond::vitesse::bridage_leve(&state.backend),
        ),
    })
}

/// `GET /system/background-tasks/running`
pub(crate) async fn ce_qui_tourne_route(State(state): State<AppState>) -> impl IntoResponse {
    let lecture = lecture_bornee().await;
    Json(ce_qui_tourne(&state, lecture))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::audio::thermal::MotifIllisible;

    fn priorite(playback_active: bool, throttled: Vec<&'static str>) -> ReleveDePriorite {
        let mut r = tune_core::taches_de_fond::priorite::releve();
        r.playback_active = playback_active;
        r.throttled = throttled;
        r
    }

    /// Une passe qui décode CÈDE, une passe qui ne décode pas RALENTIT ; la
    /// cession ne vaut que tant qu'une zone joue ; la pause de l'utilisateur
    /// et la chaleur l'emportent.
    #[test]
    fn l_activite_dit_qui_cede_qui_ralentit_et_pourquoi_4681() {
        let p = priorite(true, vec!["replaygain", "enrichment", "scan"]);
        let a = |id, pause, cours, chaud: &[String]| activite_de(id, pause, cours, &p, chaud);
        assert_eq!(a("replaygain", false, true, &[]), Activite::Cedee);
        assert_eq!(a("enrichment", false, true, &[]), Activite::Ralentie);
        assert_eq!(a("scan", false, true, &[]), Activite::Ralentie);
        assert_eq!(a("acoustic", false, true, &[]), Activite::AuTravail);
        assert_eq!(a("acoustic", false, false, &[]), Activite::AuRepos);
        assert_eq!(a("replaygain", true, true, &[]), Activite::EnPause);
        // #6285 — la passe des faux hi-res décode : elle CÈDE, et sa garde
        // thermique `faux_hires` la retient.
        let p_hires = priorite(true, vec!["hires_check"]);
        assert_eq!(
            activite_de("hires_check", false, true, &p_hires, &[]),
            Activite::Cedee
        );
        assert_eq!(
            a("hires_check", false, true, &["faux_hires".to_string()]),
            Activite::Retenue
        );
        let chaud = vec!["replaygain".to_string()];
        assert_eq!(a("fingerprints", false, true, &chaud), Activite::Retenue);
        assert_eq!(a("enrichment", false, true, &chaud), Activite::Ralentie);
        assert_eq!(Activite::Retenue.raison(), Some("thermal"));

        // La lecture s'est arrêtée : la fenêtre « a cédé » ne compte plus.
        let p = priorite(false, vec!["replaygain"]);
        assert_eq!(
            activite_de("replaygain", false, true, &p, &[]),
            Activite::AuTravail
        );
    }

    /// #6450 — température illisible : le bridage à un fichier à la fois est
    /// MAINTENU, et la route le dit, avec le motif. Lisible : la largeur
    /// réglée s'applique.
    #[test]
    fn le_bridage_thermique_est_dit_et_maintenu_sans_temperature_6450() {
        let illisible = bloc_thermique(
            Some(LectureThermique::Illisible(MotifIllisible::AucunCapteur)),
            Vitesse::Rapide,
            16,
            &[],
            false,
        );
        assert_eq!(illisible["readable"], false, "{illisible}");
        assert_eq!(illisible["throttled"], true, "{illisible}");
        assert_eq!(illisible["reason"], "temperature_illisible", "{illisible}");
        assert_eq!(illisible["width_configured"], 4, "{illisible}");
        assert_eq!(illisible["width_effective"], 1, "{illisible}");

        let figee = bloc_thermique(
            Some(LectureThermique::Illisible(MotifIllisible::Figee)),
            Vitesse::Normale,
            16,
            &[],
            false,
        );
        assert_eq!(figee["reason"], "temperature_figee", "{figee}");
        assert_eq!(figee["width_effective"], 1, "{figee}");

        let lisible = bloc_thermique(
            Some(LectureThermique::Lisible(61.26)),
            Vitesse::Rapide,
            16,
            &["replaygain".to_string()],
            false,
        );
        assert_eq!(lisible["readable"], true, "{lisible}");
        assert_eq!(lisible["temp_c"], 61.3, "{lisible}");
        assert_eq!(lisible["throttled"], false, "{lisible}");
        assert_eq!(lisible["reason"], Value::Null, "{lisible}");
        assert_eq!(lisible["width_effective"], 4, "{lisible}");
        assert_eq!(lisible["held_passes"], json!(["replaygain"]), "{lisible}");

        // Capteur trop lent : rien n'est prétendu.
        let muet = bloc_thermique(None, Vitesse::Normale, 16, &[], false);
        assert_eq!(muet["readable"], Value::Null, "{muet}");
        assert_eq!(muet["width_effective"], Value::Null, "{muet}");
        assert_eq!(muet["width_configured"], 2, "{muet}");
    }

    /// #6713 — le bridage levé par l'utilisateur rend la vitesse réglée quand
    /// la température est ILLISIBLE, et le dit. Il ne change rien quand une
    /// température est lue : la garde chaude retient toujours ses passes.
    #[test]
    fn le_bridage_leve_ne_vaut_que_sans_temperature_6713() {
        let leve = bloc_thermique(
            Some(LectureThermique::Illisible(MotifIllisible::AucunCapteur)),
            Vitesse::Rapide,
            16,
            &[],
            true,
        );
        assert_eq!(leve["throttled"], false, "{leve}");
        assert_eq!(leve["throttle_lifted_by_user"], true, "{leve}");
        assert_eq!(leve["reason"], "temperature_illisible", "{leve}");
        assert_eq!(leve["width_effective"], 4, "{leve}");
        assert_eq!(leve["unthrottle_unreadable_temperature"], true, "{leve}");

        let figee = bloc_thermique(
            Some(LectureThermique::Illisible(MotifIllisible::Figee)),
            Vitesse::Normale,
            16,
            &[],
            true,
        );
        assert_eq!(figee["width_effective"], 2, "{figee}");
        assert_eq!(figee["throttle_lifted_by_user"], true, "{figee}");

        // Température lue, et chaude : le réglage n'y touche pas.
        let chaude = bloc_thermique(
            Some(LectureThermique::Lisible(85.0)),
            Vitesse::Rapide,
            16,
            &["replaygain".to_string()],
            true,
        );
        assert_eq!(chaude["throttled"], false, "{chaude}");
        assert_eq!(chaude["throttle_lifted_by_user"], false, "{chaude}");
        assert_eq!(chaude["held_passes"], json!(["replaygain"]), "{chaude}");
        assert_eq!(chaude["width_effective"], 4, "{chaude}");

        // Sans le réglage, le bridage par défaut reste (#6450).
        let defaut = bloc_thermique(
            Some(LectureThermique::Illisible(MotifIllisible::AucunCapteur)),
            Vitesse::Rapide,
            16,
            &[],
            false,
        );
        assert_eq!(defaut["throttle_lifted_by_user"], false, "{defaut}");
        assert_eq!(defaut["width_effective"], 1, "{defaut}");
    }
}
