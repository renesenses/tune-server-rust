//! Le serveur DIT sur quel album le geste de lecture portait (#1361).
//!
//! Cyrille Moutia demande depuis le 30/06/2026, et redemande le 09/08, un
//! raccourci « Retour à l'album en cours » : il lance un album Qobuz, navigue
//! ailleurs, et doit refaire genre → artiste → album pour revenir.
//!
//! Le serveur SAIT déjà. `POST /zones/:id/play` déduit du corps de la requête
//! ce que l'auditeur a demandé (`contexte_de_lecture`) et le pose sur la
//! session de la zone (`set_session_context`, #2441) — nature et identifiant,
//! `("album", "<id Qobuz>")`. C'est un cas net d'« écrit mais pas branché » :
//! l'unique lecteur était l'orchestrateur, pour tamponner `listen_history`.
//!
//! Le client ne POUVAIT donc pas construire le raccourci :
//!
//! - `current_track.album_id` / `artist_id` sont des `i64` de BIBLIOTHÈQUE
//!   (`tune-core/src/playback/mod.rs`), donc toujours `null` sur une piste de
//!   service — il n'y a pas de ligne en base pour un album Qobuz ;
//! - il ne restait que `source` + `source_id`, l'identifiant de la PISTE,
//!   d'où un aller-retour `GET /streaming/{service}/tracks/{track_id}` à
//!   chaque changement de piste pour en tirer l'album.
//!
//! Ce fichier cloue le contrat sur les surfaces de zone qui portent
//! `current_track`. Le contexte vit au niveau de la ZONE et non de la piste :
//! il survit aux avances automatiques — la deuxième piste d'un album reste
//! une écoute « album » — alors que `current_track` change à chaque piste.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::builder()
            .method("POST")
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// Crée une zone et rend son id.
async fn zone(app: &axum::Router) -> i64 {
    let (status, body) = post(app, "/api/v1/zones", json!({"name": "Salon"})).await;
    assert_eq!(status, StatusCode::CREATED, "création de zone : {body}");
    body["id"].as_i64().expect("un id de zone")
}

/// L'identifiant d'album Qobuz que porte le geste de lecture.
///
/// Un vrai : Qobuz identifie ses albums par le code-barres de l'édition, pas
/// par un entier. C'est précisément pourquoi le contexte est une CHAÎNE et
/// pourquoi `current_track.album_id`, un `i64` de bibliothèque, ne pouvait pas
/// le porter.
const ALBUM_QOBUZ: &str = "0060254735822";

/// Le geste de l'auditeur : « lire cet album Qobuz sur cette zone ».
///
/// La résolution du flux échoue dans ce test — aucun service Qobuz n'est
/// enregistré sur un serveur `:memory:`, et il n'y a pas de réseau. C'est sans
/// effet sur ce qui est mesuré : `set_session_context` est appelé AVANT toute
/// branche de lecture (`playback.rs`, « poser CE QUE l'auditeur vient de
/// demander sur la session de la zone, avant toute branche »), exactement pour
/// que le contexte décrive le GESTE et non le succès du décodage.
async fn demander_l_album(app: &axum::Router, id: i64) {
    let (status, _) = post(
        app,
        &format!("/api/v1/zones/{id}/play"),
        json!({
            "source": "qobuz",
            "streaming_album_id": ALBUM_QOBUZ,
        }),
    )
    .await;
    // On n'exige PAS un 200 : le flux ne se résout pas ici. On exige que le
    // serveur ait reconnu la demande, pas qu'il l'ait honorée.
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "la route de lecture doit exister"
    );
}

/// Les surfaces de zone qui portent `current_track`, c'est-à-dire celles où un
/// client lit « ce qui joue ».
fn surfaces(id: i64) -> [String; 3] {
    [
        "/api/v1/zones".to_string(),
        format!("/api/v1/zones/{id}"),
        format!("/api/v1/zones/{id}/status"),
    ]
}

/// Extrait la zone d'une charge utile qui peut être une liste ou un objet.
fn la_zone(body: &Value) -> &Value {
    if body.is_array() { &body[0] } else { body }
}

/// LE contrat de #1361 : après un geste « lire cet album Qobuz », chaque
/// surface de zone nomme l'album.
#[tokio::test]
async fn chaque_surface_de_zone_nomme_l_album_demande() {
    let app = app();
    let id = zone(&app).await;
    demander_l_album(&app, id).await;
    for chemin in surfaces(id) {
        let (status, body) = get(&app, &chemin).await;
        assert_eq!(status, StatusCode::OK, "{chemin} : {body}");
        let z = la_zone(&body);
        assert_eq!(
            z["session_context_type"].as_str(),
            Some("album"),
            "{chemin} : la nature du geste manque — {body}"
        );
        assert_eq!(
            z["session_context_id"].as_str(),
            Some(ALBUM_QOBUZ),
            "{chemin} : l'identifiant de l'album manque — {body}"
        );
    }
}

/// Le champ est TOUJOURS écrit, `null` compris.
///
/// Un champ ABSENT dit « ce serveur ne connaît pas la notion » ; un champ
/// `null` dit « aucun contexte pour cette session ». Le client a besoin de
/// distinguer les deux pour décider s'il MASQUE le raccourci « Retour à
/// l'album en cours » ou s'il le GRISE. Sans cette garantie il ne peut que
/// deviner, et un `?? null` silencieux masquerait la disparition du champ au
/// lieu de la signaler.
#[tokio::test]
async fn sans_geste_de_lecture_le_contexte_est_nul_et_non_absent() {
    let app = app();
    let id = zone(&app).await;
    for chemin in surfaces(id) {
        let (status, body) = get(&app, &chemin).await;
        assert_eq!(status, StatusCode::OK, "{chemin} : {body}");
        let z = la_zone(&body);
        let obj = z.as_object().expect("une zone est un objet");
        assert!(
            obj.contains_key("session_context_type"),
            "{chemin} : le champ doit être présent même vide — {body}"
        );
        assert!(
            obj.contains_key("session_context_id"),
            "{chemin} : le champ doit être présent même vide — {body}"
        );
        assert!(
            z["session_context_type"].is_null(),
            "{chemin} : sans geste, la nature doit être nulle — {body}"
        );
        assert!(
            z["session_context_id"].is_null(),
            "{chemin} : sans geste, l'identifiant doit être nul — {body}"
        );
    }
}

/// Témoin anti-régression : VERT des deux côtés du correctif.
///
/// L'ajout est strictement additif. Ce témoin garde ce que les clients
/// déployés lisent déjà sur ces mêmes surfaces — s'il rougit, l'ajout a
/// déplacé ou écrasé quelque chose, et ce n'est plus un ajout.
#[tokio::test]
async fn temoin_les_champs_historiques_de_zone_sont_intacts() {
    let app = app();
    let id = zone(&app).await;
    demander_l_album(&app, id).await;
    for chemin in surfaces(id) {
        let (status, body) = get(&app, &chemin).await;
        assert_eq!(status, StatusCode::OK, "{chemin} : {body}");
        let z = la_zone(&body);
        let obj = z.as_object().expect("une zone est un objet");
        for champ in ["state", "position_ms", "volume"] {
            assert!(
                obj.contains_key(champ),
                "{chemin} : le champ historique `{champ}` a disparu — {body}"
            );
        }
    }
}
