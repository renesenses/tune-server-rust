//! #3966 — un `position_ms` négatif passait la frontière HTTP.
//!
//! `POST /zones/{id}/seek` prenait `SeekRequest.position_ms: i64` et faisait
//! `as u64` avant l'orchestrateur : `-1` devenait 18 446 744 073 709 551 615,
//! la sortie recevait cette position, puis la borne de durée la reposait en
//! fin de piste. Le refus se fait désormais AVANT toute mutation
//! ([`super::refuser_un_entier_negatif`]) et nomme le champ.
//!
//! Jumeau traité dans la même PR : `start_index` de `POST /zones/{id}/play`
//! (`as usize` puis `.min(len - 1)` → dernière piste de l'album). Les autres
//! entiers signés des routes de file (`QueueMoveRequest`, `QueueAddRequest`)
//! sont déjà bornés (`from < 0 || to < 0` → 400 ; `clamp(0, …)`), pas de cast.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

use super::refuser_un_entier_negatif;

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

/// Le refus DOIT nommer le champ et la valeur reçue : un client qui envoie un
/// négatif a un bogue, le message est ce qui le lui montre.
fn verifier_le_refus(statut: StatusCode, corps: &Value, champ: &str, valeur: i64) {
    assert_eq!(
        corps["field"], champ,
        "le refus doit nommer le champ `{champ}` (valeur {valeur}) — statut {statut}, corps : {corps}"
    );
    assert_eq!(statut, StatusCode::BAD_REQUEST, "corps : {corps}");
    assert_eq!(corps["error"], "negative_value", "corps : {corps}");
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(champ) && message.contains(&valeur.to_string()),
        "le message doit nommer `{champ}` et la valeur {valeur} : {message:?}"
    );
}

/// La règle seule : zéro et positif passent, négatif refusé en nommant le champ.
#[test]
fn la_regle_garde_zero_et_positif_et_refuse_le_negatif() {
    assert!(refuser_un_entier_negatif("position_ms", 0).is_ok());
    assert!(refuser_un_entier_negatif("position_ms", 1).is_ok());
    assert!(refuser_un_entier_negatif("position_ms", i64::MAX).is_ok());
    for valeur in [-1, -1000, i64::MIN] {
        let refus = refuser_un_entier_negatif("position_ms", valeur)
            .expect_err("un négatif doit être refusé");
        assert_eq!(refus.status(), StatusCode::BAD_REQUEST);
    }
}

/// Le seek de l'issue, par la route : `-1` → 400 nommant `position_ms`,
/// et la position de lecture n'a PAS bougé ; `0` et un positif passent
/// toujours et sont renvoyés tels quels.
#[tokio::test]
async fn le_seek_refuse_le_negatif_avant_de_toucher_la_position() {
    let (app, state) = serveur();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    let uri = format!("/api/v1/zones/{zone_id}/seek");
    // Une piste en cours de 5 min : c'est elle que la borne de durée de
    // l'orchestrateur lisait pour reposer le `u64::MAX` en fin de piste.
    state
        .playback
        .restore_position(
            zone_id,
            0,
            tune_core::playback::NowPlaying {
                track_id: Some(1),
                title: "Piste".into(),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;

    // Positif : passe, et la position de lecture suit.
    let (statut, corps) = poster(&app, &uri, json!({ "position_ms": 5000 })).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["position_ms"], 5000);
    assert_eq!(state.playback.get_state(zone_id).await.position_ms, 5000);

    // Négatif : 400 nommant le champ, et la position est restée à 5000 —
    // le refus a eu lieu avant l'orchestrateur.
    for valeur in [-1_i64, -60_000, i64::MIN] {
        let (statut, corps) = poster(&app, &uri, json!({ "position_ms": valeur })).await;
        verifier_le_refus(statut, &corps, "position_ms", valeur);
        assert_eq!(
            state.playback.get_state(zone_id).await.position_ms,
            5000,
            "un seek négatif ne doit pas déplacer la lecture"
        );
    }

    // Zéro : c'est un retour au début, pas un négatif.
    let (statut, corps) = poster(&app, &uri, json!({ "position_ms": 0 })).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["position_ms"], 0);
    assert_eq!(state.playback.get_state(zone_id).await.position_ms, 0);
}

/// Le jumeau : `start_index` négatif sur la lecture → 400 nommant le champ,
/// avant même que le profil de session ne soit marqué sur la zone (c'est la
/// première écriture du gestionnaire : la zone n'existe alors pas encore en
/// mémoire). `0` n'est pas refusé par cette règle.
#[tokio::test]
async fn la_lecture_refuse_un_start_index_negatif_avant_toute_ecriture() {
    let (app, state) = serveur();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    let uri = format!("/api/v1/zones/{zone_id}/play");

    let (statut, corps) = poster(&app, &uri, json!({ "album_id": 1, "start_index": -1 })).await;
    verifier_le_refus(statut, &corps, "start_index", -1);
    assert!(
        state
            .playback
            .all_states()
            .await
            .iter()
            .all(|z| z.zone_id != zone_id),
        "le refus doit précéder le marquage du profil de session"
    );

    // Zéro : la règle ne le refuse pas — la requête va plus loin (et échoue
    // pour une autre raison ici : l'album 1 n'existe pas), sans jamais
    // nommer `start_index`.
    let (_, corps) = poster(&app, &uri, json!({ "album_id": 1, "start_index": 0 })).await;
    assert_ne!(corps["field"], "start_index", "corps : {corps}");
    assert_ne!(corps["error"], "negative_value", "corps : {corps}");
}
