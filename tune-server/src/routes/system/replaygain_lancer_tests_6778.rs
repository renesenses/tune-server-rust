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

fn etat() -> EtatDuLancement {
    EtatDuLancement {
        armee: true,
        en_pause: false,
        campagne_ouverte: false,
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
                campagne_ouverte: true,
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
