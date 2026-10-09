//! `POST /library/identify-all?mode=artistes` — la passe artistes par le
//! réseau (#4805, étape C).
//!
//! Le MBID des fiches d'artiste que l'étape B (crédits du pressage identifié)
//! n'a pas rattachées, par une recherche MusicBrainz confirmée par la
//! bibliothèque. La décision est dans
//! [`tune_core::metadata::artistes_par_le_reseau`] ; ce module n'en est que le
//! pilote, sur le modèle de la passe « labels seulement » :
//!
//! - **mêmes gardes** que les autres modes, vérifiées par l'appelant : Premium,
//!   pause, une seule passe à la fois (même clé d'état) ;
//! - **même pause** ([`Tache::Identification`]), relue entre deux artistes ;
//! - **même disjoncteur** : douze refus de MusicBrainz d'affilée arrêtent la
//!   passe et le disent. Un artiste écarté sans requête ne le touche pas ;
//! - **un tour borné** : au plus `limite` fiches ([`LIMITE_PAR_DEFAUT`], au
//!   plus [`LIMITE_MAX`]) ;
//! - **reprise** : l'état garde la dernière fiche traitée
//!   (`dernier_artiste_id`). Un tour en pause, arrêté, ou fini avant la fin de
//!   la liste, repart après elle. Un tour qui atteint la fin de la liste
//!   (`fin_de_liste`) remet le curseur à zéro : le suivant reprend les fiches
//!   restées sans MBID depuis le début.
//!
//! Chaque requête attend son créneau du limiteur MusicBrainz partagé (1
//! requête/s), avec le User-Agent de Tune.

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::metadata::artistes_par_le_reseau::{
    self, BilanReseau, Entite, interroger_musicbrainz, traiter_un_artiste,
};
use tune_core::metadata::musicbrainz_release::RefusMusicBrainz;
use tune_core::taches_de_fond::{Tache, est_en_pause};

use super::{ALBUMS_PAR_ECRITURE, CLE_ETAT, Disjoncteur, EffetSurLeDisjoncteur};
use crate::state::AppState;

/// Le nombre de fiches d'un tour, sans `limite`.
pub(super) const LIMITE_PAR_DEFAUT: usize = 200;
/// La borne de `limite`.
pub(super) const LIMITE_MAX: usize = 2000;

/// Le coût annoncé d'une fiche, en secondes. Le banc de l'étape C compte
/// 2,50 requêtes par fiche interrogée (30 pour 12), à au moins 1,1 s chacune,
/// plus l'aller-retour : 3 s. Une estimation tirée du banc, pas une mesure de
/// service ; les fiches écartées d'office ne coûtent rien.
const SECONDES_PAR_ARTISTE: f64 = 3.0;

/// Le curseur de reprise : la dernière fiche d'un tour `artistes` en pause,
/// arrêté, ou fini avant la fin de la liste ; 0 sinon.
pub(super) fn curseur_de_reprise_artistes(deja: Option<&Value>) -> i64 {
    let Some(e) = deja else { return 0 };
    if e["mode"].as_str() != Some("artistes") {
        return 0;
    }
    let reprenable = match e["status"].as_str() {
        Some("paused") | Some("stopped") => true,
        Some("done") => e["fin_de_liste"].as_bool() == Some(false),
        _ => false,
    };
    if reprenable {
        e["dernier_artiste_id"].as_i64().unwrap_or(0)
    } else {
        0
    }
}

/// L'avancement d'un tour.
#[derive(Debug, Default)]
pub(super) struct CompteArtistes {
    pub(super) total: usize,
    pub(super) bilan: BilanReseau,
    pub(super) dernier_artiste_id: i64,
    pub(super) fin_de_liste: bool,
}

fn ecrire_etat_artistes(
    backend: &std::sync::Arc<dyn DbBackend>,
    task_id: &str,
    status: &str,
    c: &CompteArtistes,
    raison: Option<&str>,
) {
    let b = &c.bilan;
    SettingsRepo::with_backend(backend.clone())
        .set(
            CLE_ETAT,
            &json!({
                "status": status,
                "mode": "artistes",
                "task_id": task_id,
                "total": c.total,
                "traites": b.traites,
                "mbid_poses": b.poses,
                "departages": b.departages,
                "ecartes": b.ecartes,
                "sans_correspondance": b.sans_correspondance,
                "ambigus": b.ambigus,
                "non_confirmes": b.non_confirmes,
                "refuses_par_la_base": b.refuses_par_la_base,
                "pannes": b.pannes,
                "requetes": b.requetes,
                "dernier_artiste_id": c.dernier_artiste_id,
                "fin_de_liste": c.fin_de_liste,
                "raison": raison,
            })
            .to_string(),
        )
        .ok();
}

/// Sélectionne et lance un tour. Droit, pause et passe déjà en cours ont été
/// vérifiés par l'appelant.
pub(super) async fn lancer_la_passe_artistes(
    state: AppState,
    deja: Option<Value>,
    limite: Option<usize>,
) -> (StatusCode, Json<Value>) {
    let limite = limite.unwrap_or(LIMITE_PAR_DEFAUT).clamp(1, LIMITE_MAX);
    let apres = curseur_de_reprise_artistes(deja.as_ref());
    let backend_selection = state.backend.clone();
    let fiches = match tokio::task::spawn_blocking(move || {
        artistes_par_le_reseau::candidats(&backend_selection, apres, limite)
    })
    .await
    {
        Ok(Ok(f)) => f,
        erreur => {
            warn!(error = ?erreur, "artistes_lot_selection_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "code": "identification_candidats_indisponibles",
                    "error": "identification_candidats_indisponibles",
                })),
            );
        }
    };
    let total = fiches.len();
    let task_id = uuid::Uuid::new_v4().to_string();
    let duree_estimee_s = (total as f64 * SECONDES_PAR_ARTISTE).round() as i64;
    info!(task_id = %task_id, candidats = total, apres, limite, duree_estimee_s, "artistes_lot_demarre");

    let compte = CompteArtistes {
        total,
        dernier_artiste_id: apres,
        // Moins de fiches que la limite : ce tour atteint la fin de la liste.
        fin_de_liste: total < limite,
        ..Default::default()
    };
    ecrire_etat_artistes(&state.backend, &task_id, "running", &compte, None);

    let backend = state.backend.clone();
    let task_id_tache = task_id.clone();
    let garde = state.background_tasks.begin(
        "identification_lot",
        "Identifiants MusicBrainz des artistes…",
        "identification",
    );
    tokio::spawn(async move {
        let _garde = garde;
        executer_la_passe_artistes(
            &backend,
            &task_id_tache,
            fiches,
            compte,
            interroger_musicbrainz,
        )
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "started",
            "mode": "artistes",
            "task_id": task_id,
            "total": total,
            "limite": limite,
            "reprise_apres_artiste_id": apres,
            "duree_estimee_s": duree_estimee_s,
            "statut": "GET /library/identify-all/status",
            "arreter": "POST /system/taches-de-fond/identification/pause",
        })),
    )
}

/// La boucle : une frontière de pause, un artiste (une à onze requêtes, chacune
/// à son créneau), un disjoncteur. `interroger` est le réseau en service, un
/// faux dans les témoins. Rend le statut final écrit.
pub(super) async fn executer_la_passe_artistes<F, Fut>(
    backend: &std::sync::Arc<dyn DbBackend>,
    task_id: &str,
    fiches: Vec<(i64, String)>,
    mut c: CompteArtistes,
    mut interroger: F,
) -> &'static str
where
    F: FnMut(Entite, String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let mut disjoncteur = Disjoncteur::default();

    for (id, nom) in fiches {
        if est_en_pause(Tache::Identification) {
            info!(task_id = %task_id, traites = c.bilan.traites, "artistes_lot_en_pause");
            ecrire_etat_artistes(backend, task_id, "paused", &c, Some("pause_utilisateur"));
            return "paused";
        }

        let requetes_avant = c.bilan.requetes;
        let effet = match traiter_un_artiste(backend, id, &nom, &mut interroger, &mut c.bilan).await
        {
            Ok(Some(refus)) => {
                warn!(task_id = %task_id, artiste_id = id, refus = %refus, "artistes_lot_panne_musicbrainz");
                EffetSurLeDisjoncteur::Refus
            }
            // Écarté d'office : MusicBrainz n'a pas été interrogé.
            Ok(None) if c.bilan.requetes == requetes_avant => EffetSurLeDisjoncteur::Inchange,
            Ok(None) => EffetSurLeDisjoncteur::Remise,
            Err(e) => {
                warn!(task_id = %task_id, artiste_id = id, error = %e, "artistes_lot_base_echouee");
                EffetSurLeDisjoncteur::Inchange
            }
        };
        disjoncteur.enregistrer(effet);
        // Le curseur n'avance que hors panne : la reprise après le
        // disjoncteur retente la série refusée.
        if effet != EffetSurLeDisjoncteur::Refus {
            c.dernier_artiste_id = id;
        }

        if disjoncteur.a_saute() {
            warn!(task_id = %task_id, traites = c.bilan.traites, "artistes_lot_arret_musicbrainz_injoignable");
            ecrire_etat_artistes(
                backend,
                task_id,
                "stopped",
                &c,
                Some(super::RAISON_MUSICBRAINZ_INJOIGNABLE),
            );
            return "stopped";
        }

        if c.bilan.traites.is_multiple_of(ALBUMS_PAR_ECRITURE) {
            ecrire_etat_artistes(backend, task_id, "running", &c, None);
        }
    }

    let b = &c.bilan;
    info!(
        task_id = %task_id,
        total = c.total,
        mbid_poses = b.poses,
        departages = b.departages,
        ambigus = b.ambigus,
        non_confirmes = b.non_confirmes,
        sans_correspondance = b.sans_correspondance,
        ecartes = b.ecartes,
        refuses_par_la_base = b.refuses_par_la_base,
        pannes = b.pannes,
        requetes = b.requetes,
        fin_de_liste = c.fin_de_liste,
        "artistes_lot_termine"
    );
    if c.fin_de_liste {
        // Le tour suivant repart du début de la liste.
        c.dernier_artiste_id = 0;
    }
    ecrire_etat_artistes(backend, task_id, "done", &c, None);
    "done"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::ToSqlValue;

    #[test]
    fn le_curseur_ne_reprend_qu_un_tour_artistes_inacheve() {
        let e = |status: &str, fin: bool| json!({"mode": "artistes", "status": status, "dernier_artiste_id": 42, "fin_de_liste": fin});
        assert_eq!(curseur_de_reprise_artistes(Some(&e("paused", false))), 42);
        assert_eq!(curseur_de_reprise_artistes(Some(&e("stopped", true))), 42);
        assert_eq!(curseur_de_reprise_artistes(Some(&e("done", false))), 42);
        assert_eq!(curseur_de_reprise_artistes(Some(&e("done", true))), 0);
        assert_eq!(curseur_de_reprise_artistes(Some(&e("running", false))), 0);
        let labels = json!({"mode": "labels", "status": "paused", "dernier_album_id": 7});
        assert_eq!(curseur_de_reprise_artistes(Some(&labels)), 0);
        assert_eq!(curseur_de_reprise_artistes(None), 0);
    }

    fn base(n: i64) -> Arc<dyn DbBackend> {
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = etat.backend.clone();
        for id in 1..=n {
            b.execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[
                    &id as &dyn ToSqlValue,
                    &format!("Artiste {id}") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.execute(
                "INSERT INTO albums (id, title, artist_id) VALUES (?, 'Album', ?)",
                &[&id as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .unwrap();
        }
        b
    }

    fn etat(b: &Arc<dyn DbBackend>) -> Value {
        let s = SettingsRepo::with_backend(b.clone())
            .get(CLE_ETAT)
            .unwrap()
            .unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[tokio::test]
    async fn douze_refus_d_affilee_arretent_le_tour_et_gardent_le_curseur() {
        let b = base(20);
        let fiches = artistes_par_le_reseau::candidats(&b, 0, 100).unwrap();
        let mut appels = 0;
        // La première fiche reçoit une réponse vide, puis plus rien ne passe.
        let interroger = |_: Entite, _: String| {
            appels += 1;
            std::future::ready(if appels == 1 {
                Ok(json!({"artists": []}))
            } else {
                Err(RefusMusicBrainz::Statut(503))
            })
        };
        let fin = executer_la_passe_artistes(
            &b,
            "t",
            fiches,
            CompteArtistes {
                total: 20,
                ..Default::default()
            },
            interroger,
        )
        .await;
        assert_eq!(fin, "stopped");
        let e = etat(&b);
        assert_eq!(e["raison"], "musicbrainz_injoignable");
        assert_eq!(e["traites"], 13);
        assert_eq!(e["pannes"], 12);
        assert_eq!(
            e["dernier_artiste_id"], 1,
            "le curseur reste avant la série refusée"
        );
        assert_eq!(curseur_de_reprise_artistes(Some(&e)), 1);
    }

    #[tokio::test]
    async fn des_fiches_ecartees_sans_requete_ne_touchent_pas_au_disjoncteur() {
        let b = base(0);
        let mut fiches = Vec::new();
        for id in 1..=12i64 {
            b.execute(
                "INSERT INTO artists (id, name) VALUES (?, 'Unknown Artist')",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap();
            fiches.push((id, "Unknown Artist".to_string()));
        }
        let interroger =
            |_: Entite, _: String| std::future::ready(Err::<Value, _>(RefusMusicBrainz::Transport));
        let fin = executer_la_passe_artistes(
            &b,
            "t",
            fiches,
            CompteArtistes {
                total: 12,
                fin_de_liste: true,
                dernier_artiste_id: 5,
                ..Default::default()
            },
            interroger,
        )
        .await;
        assert_eq!(fin, "done");
        let e = etat(&b);
        assert_eq!(e["ecartes"], 12);
        assert_eq!(e["requetes"], 0);
        assert_eq!(
            e["dernier_artiste_id"], 0,
            "fin de liste : le tour suivant repart du début"
        );
    }
}
