//! #6778 (prérequis de tune-web-client#2295, Tades, fil 2370) — « Pourquoi
//! pas d'interrupteur Lancer - Pause sur le ReplayGain alors que tous les
//! autres en ont ». Aucune route ne lançait la passe à la demande.
//!
//! Témoins : la décision (pure), puis la route montée, qui doit RÉVEILLER la
//! boucle de fond, pas seulement répondre 202.

use super::{EtatDuLancement, decider_le_lancement};
use crate::state::AppState;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// La campagne et le rang au travail sont globaux au processus : les témoins
/// montés de ce fichier les prennent tour à tour.
static ETAT_GLOBAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn etat() -> EtatDuLancement {
    EtatDuLancement {
        armee: true,
        en_pause: false,
        au_travail: false,
        candidates: 12,
        zone_qui_joue: None,
    }
}

#[test]
fn des_candidates_au_repos_lancent_la_passe_6778() {
    let (s, c, reveil) = decider_le_lancement(&etat());
    assert_eq!(s, StatusCode::ACCEPTED, "{c}");
    assert_eq!(c["status"], "started");
    assert_eq!(c["candidates"], 12);
    assert!(c["waiting_reason"].is_null(), "{c}");
    assert!(
        reveil,
        "#6778 : 202 sans réveiller la boucle, qui dort 15 min"
    );
}

#[test]
fn une_zone_qui_joue_est_dite_et_la_lecture_garde_sa_priorite_6778() {
    let (s, c, reveil) = decider_le_lancement(&EtatDuLancement {
        zone_qui_joue: Some("Salon".into()),
        ..etat()
    });
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(c["waiting_reason"], "playback", "{c}");
    assert_eq!(c["playing_zone"], "Salon");
    assert!(reveil);
}

#[test]
fn les_refus_et_le_rien_a_faire_ne_reveillent_rien_6778() {
    let cas = [
        (
            EtatDuLancement {
                armee: false,
                ..etat()
            },
            StatusCode::CONFLICT,
            "refused",
            Some("analysis_disabled"),
        ),
        (
            EtatDuLancement {
                en_pause: true,
                ..etat()
            },
            StatusCode::CONFLICT,
            "refused",
            Some("paused"),
        ),
        (
            EtatDuLancement {
                au_travail: true,
                ..etat()
            },
            StatusCode::CONFLICT,
            "already_running",
            None,
        ),
        (
            EtatDuLancement {
                candidates: 0,
                ..etat()
            },
            StatusCode::OK,
            "nothing_to_do",
            None,
        ),
    ];
    for (e, statut, mot, raison) in cas {
        let (s, c, reveil) = decider_le_lancement(&e);
        assert_eq!(s, statut, "{e:?} → {c}");
        assert_eq!(c["status"], mot, "{e:?} → {c}");
        if let Some(r) = raison {
            assert_eq!(c["reason"], r, "{c}");
        }
        assert!(!reveil, "{e:?} ne doit rien réveiller");
    }
}

async fn lancer(state: &AppState) -> (StatusCode, Value) {
    let response = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/system/replaygain/analyze")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// La route MONTÉE : coupée, rien à faire, puis une piste nouvelle qui la
/// lance et réveille la boucle.
#[tokio::test]
async fn la_route_montee_lance_la_passe_et_reveille_la_boucle_6778() {
    use tune_core::audio::replaygain as rg;
    let _garde = ETAT_GLOBAL.lock().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();

    // Installation neuve : `replaygain_mode` absent vaut « off ».
    let (s, c) = lancer(&state).await;
    assert_eq!(s, StatusCode::CONFLICT, "{c}");
    assert_eq!(c["reason"], "analysis_disabled", "{c}");

    SettingsRepo::with_backend(state.backend.clone())
        .set(rg::MODE_KEY, "track")
        .unwrap();
    // Le miroir des pauses et la campagne sont globaux au processus : un
    // autre banc peut les tenir. Le témoin ne conclut que s'ils sont libres.
    let libre = || {
        !tune_core::taches_de_fond::est_en_pause(tune_core::taches_de_fond::Tache::ReplayGain)
            && !rg::progression::releve().actif
    };
    if !libre() {
        eprintln!("#6778 : pause ou campagne tenue par un autre banc, témoin monté sauté");
        return;
    }
    let (s, c) = lancer(&state).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["status"], "nothing_to_do", "{c}");

    let mut piste = tune_core::db::models::Track::new("Neuve".into());
    piste.file_path = Some("/m-6778/neuve.flac".into());
    piste.format = Some("flac".into());
    tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .create(&piste)
        .unwrap();
    assert_eq!(rg::compter_les_candidats_replaygain(&state.backend), 1);

    let avant = rg::reessai::nombre_de_reveils();
    let (s, c) = lancer(&state).await;
    if !libre() {
        return;
    }
    assert_eq!(s, StatusCode::ACCEPTED, "{c}");
    assert_eq!(c["status"], "started");
    assert_eq!(c["candidates"], 1);
    assert!(
        rg::reessai::nombre_de_reveils() > avant,
        "#6778 : « Lancer » a répondu 202 sans réveiller la boucle de fond"
    );
}

async fn progression(state: &AppState) -> Value {
    let response = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/system/replaygain/progress")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

/// #7724 (testeur Premium, Windows, ~600 000 pistes) — la carte disait « au
/// repos » (`working:false`) et « Lancer » répondait « tourne déjà » (409
/// `already_running`) : la route lisait la campagne ouverte, qui le reste
/// quand la passe cède à la lecture, à la garde thermique ou à la plage
/// dynamique « En premier ». Ici : campagne ouverte, personne au travail,
/// plus aucune candidate.
#[tokio::test]
async fn une_campagne_ouverte_sans_personne_au_travail_n_est_pas_en_cours_7724() {
    use tune_core::audio::replaygain as rg;
    use tune_core::taches_de_fond::ordre::{Rang, noter_rang_au_travail};
    let _garde = ETAT_GLOBAL.lock().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(rg::MODE_KEY, "track")
        .unwrap();
    if tune_core::taches_de_fond::est_en_pause(tune_core::taches_de_fond::Tache::ReplayGain) {
        eprintln!("#7724 : pause tenue par un autre banc, témoin sauté");
        return;
    }
    rg::progression::reinitialiser_pour_les_essais();
    // Une passe interrompue : campagne ouverte sur 3 pistes, puis la boucle a
    // cédé (lecture, chaleur, autre rang) sans la refermer.
    rg::progression::ouvrir_si_besoin(|| 3);
    noter_rang_au_travail(None);
    assert_eq!(rg::compter_les_candidats_replaygain(&state.backend), 0);

    let avant = progression(&state).await;
    assert_eq!(avant["working"], false, "{avant}");

    let (s, c) = lancer(&state).await;
    let apres = progression(&state).await;
    rg::progression::reinitialiser_pour_les_essais();
    assert_ne!(
        c["status"], "already_running",
        "#7724 : « Lancer » répond « tourne déjà » alors que la carte publie \
         working:false — {s} {c}"
    );
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["status"], "nothing_to_do", "{c}");
    assert_eq!(
        apres["active"], false,
        "#7724 : plus rien à mesurer, la campagne doit être refermée — {apres}"
    );
    assert_eq!(apres["working"], false, "{apres}");

    // Le refus demeure quand la passe décode vraiment.
    rg::progression::ouvrir_si_besoin(|| 3);
    noter_rang_au_travail(Some(Rang::ReplayGain));
    let (s, c) = lancer(&state).await;
    noter_rang_au_travail(None);
    rg::progression::reinitialiser_pour_les_essais();
    assert_eq!(s, StatusCode::CONFLICT, "{c}");
    assert_eq!(c["status"], "already_running", "{c}");
}
