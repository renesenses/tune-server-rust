//! #7836 — « Zones supprimées (n) » : une zone supprimée par l'utilisateur
//! est listée, et se restaure avec ses réglages ; les autres masquages ne
//! sont ni listés ni restaurables.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use tune_core::db::zone_repo::{MotifMasquage, ZoneRepo};

use super::{lister, restaurer};
use crate::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn corps(r: Response) -> (StatusCode, Value) {
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn zone(state: &AppState, nom: &str, appareil: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("airplay2"), Some(appareil))
        .unwrap()
}

#[tokio::test]
async fn une_zone_supprimee_est_listee_puis_restauree_7836() {
    let state = etat();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mac = zone(&state, "Rémi's MacBook", "airplay-92:61:3B:00:00:01");
    repo.update_volume(mac, 35.0).unwrap();
    // `DELETE /zones/{id}` : la zone est masquée, pas effacée.
    repo.masquer(mac, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    // Un doublon et un appareil ignoré : masqués, mais pas « supprimés ».
    let doublon = zone(&state, "Doublon", "airplay-doublon");
    repo.masquer(doublon, MotifMasquage::DoublonLocalGenerique)
        .unwrap();
    let ignore = zone(&state, "Ignoré", "airplay-ignore");
    repo.masquer(ignore, MotifMasquage::AppareilIgnore).unwrap();

    let (statut, liste) = corps(lister(State(state.clone())).await).await;
    assert_eq!(statut, StatusCode::OK, "{liste}");
    assert_eq!(
        liste["count"], 1,
        "#7836 : seule la zone supprimée par l'utilisateur est listée : {liste}"
    );
    let item = &liste["items"][0];
    assert_eq!(item["id"], mac, "{liste}");
    assert_eq!(item["name"], "Rémi's MacBook");
    assert_eq!(item["motif"], "suppression_utilisateur");
    assert!(
        item["masquee_le"].is_string(),
        "la date de suppression : {item}"
    );
    assert_eq!(item["appareil_detecte"], false);

    let (statut, rendue) = corps(restaurer(State(state.clone()), Path(mac)).await).await;
    assert_eq!(statut, StatusCode::OK, "{rendue}");
    let restauree = repo.get(mac).unwrap().unwrap();
    assert!(!repo.is_device_hidden("airplay-92:61:3B:00:00:01"));
    assert_eq!(
        restauree.volume, 35.0,
        "les réglages de la zone sont gardés"
    );

    // Plus rien à restaurer.
    let (_, liste) = corps(lister(State(state.clone())).await).await;
    assert_eq!(liste["count"], 0, "{liste}");
    let (statut, v) = corps(restaurer(State(state.clone()), Path(mac)).await).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "zone_non_supprimee");
}

#[tokio::test]
async fn un_masquage_qui_n_est_pas_une_suppression_ne_se_restaure_pas_7836() {
    let state = etat();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let ignore = zone(&state, "Ignoré", "airplay-ignore-2");
    repo.masquer(ignore, MotifMasquage::AppareilIgnore).unwrap();
    let (statut, v) = corps(restaurer(State(state.clone()), Path(ignore)).await).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        v,
        json!({"error": "zone_non_restaurable", "motif": "appareil_ignore"})
    );
    assert!(repo.is_device_hidden("airplay-ignore-2"));

    let (statut, v) = corps(restaurer(State(state.clone()), Path(999_999)).await).await;
    assert_eq!(statut, StatusCode::NOT_FOUND, "{v}");
}

#[tokio::test]
async fn la_zone_restauree_sort_de_son_ancien_groupe_7836() {
    let state = etat();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let a = zone(&state, "A", "airplay-a");
    let b = zone(&state, "B", "airplay-b");
    let c = zone(&state, "C", "airplay-c");
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &json!([{"id": 1, "zone_ids": [a, b, c], "leader_id": a}]).to_string(),
        )
        .unwrap();
    repo.masquer(c, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    let (statut, _) = corps(restaurer(State(state.clone()), Path(c)).await).await;
    assert_eq!(statut, StatusCode::OK);
    let groupes = tune_core::zones::groupes_enregistres::lire(
        &tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone()),
    );
    assert!(
        !groupes.iter().any(|g| g["zone_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!(c)))),
        "{groupes:?}"
    );
}
