//! #4283 — `POST /zones/{id}/queue/jump` : la position est validée AVANT que
//! le curseur ne bouge, et le refus nomme le champ.
//!
//! `play_from_queue` écrivait `set_current_pos(position)` puis lisait
//! `get_at(position)` : une position hors bornes effaçait la ligne courante de
//! la zone (plus AUCUNE ligne `is_current`), puis la route rendait un 500
//! `playback_error` anonyme. La route refuse désormais le négatif en 400
//! (`refuser_un_entier_negatif`, #3966) et traduit la sentinelle de
//! l'orchestrateur en 404 nommant `position` et la longueur de la file
//! ([`super::refus_de_position_hors_file`]) ; le curseur, lui, n'a pas bougé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;

use super::refus_de_position_hors_file;

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

/// La ROUTE, traversée par le routeur complet.
async fn poster(app: &axum::Router, uri: &str, corps: Value) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Une zone et une file de `n` pistes locales, la première courante.
fn zone_avec_une_file(state: &crate::state::AppState, n: usize) -> i64 {
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for i in 1..=n {
        let mut piste = tune_core::db::models::Track::new(format!("Piste {i}"));
        piste.file_path = Some(format!("/aucun/chemin/4283/piste{i}.flac"));
        piste.track_number = i as i32;
        piste.duration_ms = 180_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    if !ids.is_empty() {
        PlayQueueRepo::with_backend(state.backend.clone())
            .set_queue(zone_id, &ids)
            .unwrap();
    }
    zone_id
}

/// La position de la ligne courante en base, `None` quand aucune ligne ne
/// porte `is_current` — l'état que le défaut laissait.
fn curseur_en_base(state: &crate::state::AppState, zone_id: i64) -> Option<i64> {
    PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zone_id)
        .unwrap()
        .into_iter()
        .find(|e| e.is_current)
        .map(|e| e.position)
}

/// Le 404 DOIT nommer le champ, la position reçue et la longueur de la file.
fn verifier_le_404(statut: StatusCode, corps: &Value, position: i64, longueur: i64) {
    assert_eq!(statut, StatusCode::NOT_FOUND, "corps : {corps}");
    assert_eq!(
        corps["error"], "queue_position_out_of_range",
        "corps : {corps}"
    );
    assert_eq!(corps["field"], "position", "corps : {corps}");
    assert_eq!(corps["position"], position, "corps : {corps}");
    assert_eq!(corps["queue_length"], longueur, "corps : {corps}");
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("position")
            && message.contains(&position.to_string())
            && message.contains(&longueur.to_string()),
        "le message doit nommer `position`, la valeur {position} et la longueur {longueur} : {message:?}"
    );
}

/// La traduction seule : la sentinelle et rien d'autre.
#[test]
fn seule_la_sentinelle_devient_un_404_nomme() {
    let reponse = refus_de_position_hors_file("queue_position_out_of_range:7:3")
        .expect("la sentinelle doit être traduite");
    assert_eq!(reponse.status(), StatusCode::NOT_FOUND);
    for autre in [
        "no queue item at position",
        "file_not_found:/x.flac",
        "queue_position_out_of_range:",
        "queue_position_out_of_range:sept:3",
        "",
    ] {
        assert!(
            refus_de_position_hors_file(autre).is_none(),
            "{autre:?} n'est pas la sentinelle : l'appelant doit retomber sur `play_error_response`"
        );
    }
}

/// Le fait de l'issue, par la route : un saut hors bornes rend 404 nommant
/// `position`, et la ligne courante en base est TOUJOURS la piste 0.
#[tokio::test]
async fn un_saut_hors_bornes_rend_404_et_laisse_le_curseur_en_place() {
    let (app, state) = serveur();
    let zone_id = zone_avec_une_file(&state, 3);
    let uri = format!("/api/v1/zones/{zone_id}/queue/jump");
    assert_eq!(curseur_en_base(&state, zone_id), Some(0));

    for position in [3_i64, 4, 1_000, i64::MAX] {
        let (statut, corps) = poster(&app, &uri, json!({ "position": position })).await;
        verifier_le_404(statut, &corps, position, 3);
        assert_eq!(
            curseur_en_base(&state, zone_id),
            Some(0),
            "après un saut refusé vers {position} le curseur a été DÉPLACÉ (#4283)"
        );
    }
}

/// Négatif : 400 nommant `position` (même règle que `position_ms`, #3966),
/// avant l'orchestrateur — le curseur n'a pas bougé.
#[tokio::test]
async fn un_saut_negatif_rend_400_nommant_le_champ() {
    let (app, state) = serveur();
    let zone_id = zone_avec_une_file(&state, 3);
    let uri = format!("/api/v1/zones/{zone_id}/queue/jump");

    for position in [-1_i64, -1_000, i64::MIN] {
        let (statut, corps) = poster(&app, &uri, json!({ "position": position })).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "corps : {corps}");
        assert_eq!(corps["error"], "negative_value", "corps : {corps}");
        assert_eq!(corps["field"], "position", "corps : {corps}");
        assert_eq!(
            curseur_en_base(&state, zone_id),
            Some(0),
            "après un saut négatif ({position}) le curseur a été DÉPLACÉ (#4283)"
        );
    }
}

/// File VIDE : 404 nommant une longueur de 0, pas de panique, pas de 500.
#[tokio::test]
async fn un_saut_sur_une_file_vide_rend_404_sans_paniquer() {
    let (app, state) = serveur();
    let zone_id = zone_avec_une_file(&state, 0);
    let uri = format!("/api/v1/zones/{zone_id}/queue/jump");

    for position in [0_i64, 1] {
        let (statut, corps) = poster(&app, &uri, json!({ "position": position })).await;
        verifier_le_404(statut, &corps, position, 0);
    }
    assert_eq!(curseur_en_base(&state, zone_id), None);
}

/// CONTRE-ÉPREUVE du garde-fou : un saut VALIDE n'est pas refusé — le curseur
/// suit et la position est publiée à la zone. La lecture elle-même échoue
/// ensuite (aucun appareil de sortie, aucun fichier), mais jamais avec le
/// motif du refus.
#[tokio::test]
async fn un_saut_valide_deplace_le_curseur() {
    let (app, state) = serveur();
    let zone_id = zone_avec_une_file(&state, 3);
    let uri = format!("/api/v1/zones/{zone_id}/queue/jump");

    let (statut, corps) = poster(&app, &uri, json!({ "position": 2 })).await;
    assert_ne!(statut, StatusCode::BAD_REQUEST, "corps : {corps}");
    assert_ne!(
        corps["error"], "queue_position_out_of_range",
        "corps : {corps}"
    );
    assert_ne!(corps["error"], "negative_value", "corps : {corps}");
    assert_eq!(
        curseur_en_base(&state, zone_id),
        Some(2),
        "un saut valide doit déplacer le curseur"
    );
    assert_eq!(state.playback.get_state(zone_id).await.queue_position, 2);
}
