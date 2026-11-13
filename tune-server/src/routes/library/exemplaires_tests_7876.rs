//! #7876 — Réglages › Bibliothèque : les flèches d'ordre des dossiers
//! semblaient sans effet.
//!
//! L'écran affiche `music_dirs` tels que `GET /system/config` les rend (bruts)
//! et ne garde de l'ordre que les entrées présentes À L'IDENTIQUE. La route
//! d'ordre rendait l'écriture normalisée : dès que les deux différaient
//! (séparateur final, barres obliques sous Windows), le PUT réussissait et la
//! liste redessinée restait la même.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;
use tune_core::db::settings_repo::SettingsRepo;

use axum::extract::Path;

use super::{CorpsOrdre, CorpsPreference, ordre_get, ordre_put, preference_put};
use crate::config::TuneConfig;
use crate::state::AppState;

fn etat(config: TuneConfig) -> AppState {
    AppState::new(":memory:", 0, config).unwrap()
}

fn poser_music_dirs(state: &AppState, bruts: &[&str]) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &serde_json::to_string(bruts).unwrap())
        .unwrap();
}

async fn corps(r: Response) -> (StatusCode, Value) {
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&octets).unwrap())
}

async fn poser_ordre(state: &AppState, ordre: &[&str]) -> (StatusCode, Value) {
    let r = ordre_put(
        State(state.clone()),
        Json(CorpsOrdre {
            ordre: ordre.iter().map(|s| s.to_string()).collect(),
        }),
    )
    .await;
    corps(r).await
}

fn liste(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn l_ordre_affiche_reprend_l_ecriture_que_l_ecran_affiche() {
    let state = etat(TuneConfig::default());
    // Écrits par `PATCH /system/config`, qui ne normalise pas.
    poser_music_dirs(&state, &["/musique/nas/", "/musique/hires/"]);

    let Json(avant) = ordre_get(State(state.clone())).await;
    assert_eq!(liste(&avant["ordre"]), ["/musique/nas", "/musique/hires"]);
    assert_eq!(
        liste(&avant["ordre_affiche"]),
        ["/musique/nas/", "/musique/hires/"],
        "l'écran compare à l'écriture brute de /system/config : {avant}"
    );

    // Le client renvoie ce qu'il affiche.
    let (statut, apres) = poser_ordre(&state, &["/musique/hires/", "/musique/nas/"]).await;
    assert_eq!(statut, StatusCode::OK, "{apres}");
    assert_eq!(
        liste(&apres["ordre_affiche"]),
        ["/musique/hires/", "/musique/nas/"],
        "la flèche doit changer l'ordre affiché : {apres}"
    );

    // Et l'ordre est conservé : une relecture le rend tel quel.
    let Json(relu) = ordre_get(State(state.clone())).await;
    assert_eq!(
        liste(&relu["ordre_affiche"]),
        ["/musique/hires/", "/musique/nas/"]
    );
    assert_eq!(liste(&relu["ordre"]), ["/musique/hires", "/musique/nas"]);
}

#[tokio::test]
async fn sans_reglage_les_dossiers_du_fichier_de_configuration_se_classent() {
    let config = TuneConfig {
        music_dirs: vec!["/conf/a".into(), "/conf/b".into()],
        ..TuneConfig::default()
    };
    let state = etat(config);
    // Réglage absent : `GET /system/config` affiche ceux du fichier.
    SettingsRepo::with_backend(state.backend.clone())
        .delete("music_dirs")
        .ok();

    let (statut, v) = poser_ordre(&state, &["/conf/b", "/conf/a"]).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "le repli sur le fichier évite le 400 not_music_dirs : {v}"
    );
    assert_eq!(liste(&v["ordre_affiche"]), ["/conf/b", "/conf/a"]);
}

#[tokio::test]
async fn un_dossier_inconnu_reste_refuse() {
    let state = etat(TuneConfig::default());
    poser_music_dirs(&state, &["/musique/nas"]);
    let (statut, v) = poser_ordre(&state, &["/ailleurs"]).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "not_music_dirs");
}

#[tokio::test]
async fn sans_reglage_la_preference_d_album_accepte_un_dossier_du_fichier_7876() {
    let config = TuneConfig {
        music_dirs: vec!["/conf/a".into(), "/conf/b".into()],
        ..TuneConfig::default()
    };
    let state = etat(config);
    SettingsRepo::with_backend(state.backend.clone())
        .delete("music_dirs")
        .ok();
    state
        .backend
        .execute("INSERT INTO albums (id, title) VALUES (5, 'Album')", &[])
        .unwrap();
    let r = preference_put(
        State(state.clone()),
        Path(5),
        Json(CorpsPreference {
            racine: "/conf/b".into(),
        }),
    )
    .await;
    let (statut, v) = corps(r).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "#7876 : plus de 400 root_not_music_dir pour un dossier du fichier : {v}"
    );
    assert_eq!(v["racine"], "/conf/b");
}
