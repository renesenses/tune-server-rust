//! #7077, seconde passe : les refus qui restaient composés en français, en
//! dur, rendent un code stable et une phrase dans la langue de l'interface.
//!
//! Le témoin passe par le vrai routeur : la langue vient de l'en-tête
//! `Accept-Language` (ou de son absence, repli anglais), comme pour le web.
//! La garde `garde_francais_api` interdit, elle, le retour d'un littéral
//! français dans ces champs ; ce test prouve qu'une route servie rend bien la
//! phrase traduite et sa clé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

async fn demander(app: &axum::Router, chemin: &str, langue: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(chemin);
    if let Some(valeur) = langue {
        req = req.header(axum::http::header::ACCEPT_LANGUAGE, valeur);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

#[tokio::test(flavor = "multi_thread")]
async fn un_refus_de_la_seconde_passe_suit_la_langue_de_l_interface_7077() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let app = tune_server::routes::router(etat);
    let chemin = "/api/v1/streaming/qobuz/debug/raw-keys";

    let (statut, en) = demander(&app, chemin, None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "corps : {en}");
    assert_eq!(en["error"], json!("album_or_track_id_required"), "{en}");
    assert_eq!(en["code"], json!("album_or_track_id_required"), "{en}");
    assert_eq!(
        en["message_key"],
        json!("api.diagQobuz.albumOrTrackRequired"),
        "{en}"
    );
    assert_eq!(
        en["message"],
        json!("album_id or track_id is required."),
        "sans Accept-Language, la phrase est anglaise : {en}"
    );

    let (_, fr) = demander(&app, chemin, Some("fr-FR,fr;q=0.9")).await;
    assert_eq!(fr["message"], json!("album_id ou track_id requis."), "{fr}");
    assert_eq!(
        fr["error"],
        json!("album_or_track_id_required"),
        "le code reste le même dans toutes les langues : {fr}"
    );

    let (_, de) = demander(&app, chemin, Some("de")).await;
    assert_eq!(
        de["message"],
        json!("album_id oder track_id ist erforderlich."),
        "{de}"
    );

    let (statut, invalide) = demander(&app, &format!("{chemin}?album_id=a%20b"), None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "corps : {invalide}");
    assert_eq!(invalide["error"], json!("invalid_identifier"), "{invalide}");
    assert_eq!(
        invalide["message_key"],
        json!("api.diagQobuz.invalidId"),
        "{invalide}"
    );
}
