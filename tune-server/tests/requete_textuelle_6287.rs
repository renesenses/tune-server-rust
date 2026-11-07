//! #6287 — la requête TEXTUELLE des règles intelligentes, par le vrai routeur :
//! `POST /api/v1/library/smart-playlists/requete`.
//!
//! Le moteur et sa grammaire ont leurs témoins dans `tune-smart-http`
//! (`requete_texte_tests.rs`) ; ici, la route : ce qu'elle rend, et ce
//! qu'elle refuse.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ROUTE: &str = "/api/v1/library/smart-playlists/requete";

fn app() -> axum::Router {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (61, 'Miles Davis'), (62, 'Autre')",
        "INSERT INTO albums (id, title, artist_id) VALUES (61, 'Kind of Blue', 61), (62, 'Seul', 62)",
        "INSERT INTO tracks (id, album_id, artist_id, title, genre, year, file_path) VALUES \
           (61, 61, 61, 'Blue in Green', 'Jazz', 1959, '/m/6287/1.flac'), \
           (62, 61, 61, 'So What', 'Jazz', 1959, '/m/6287/2.flac'), \
           (63, 62, 62, 'Solo', 'Rock', 2001, '/m/6287/3.flac')",
        "INSERT INTO track_credits (track_id, artist_name, role, instrument) VALUES \
           (61, 'Bill Evans', 'performer', 'piano'), \
           (62, 'Wynton Kelly', 'performer', 'piano')",
    ] {
        etat.backend.execute(sql, &[]).unwrap();
    }
    tune_server::routes::router(etat)
}

async fn requete(app: &axum::Router, q: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(ROUTE)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "q": q }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn titres(corps: &Value) -> Vec<String> {
    corps["tracks"]
        .as_array()
        .unwrap_or_else(|| panic!("pas de tracks : {corps}"))
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn la_requete_rend_les_pistes_et_leur_forme_d_editeur_6287() {
    let app = app();
    let (statut, corps) = requete(
        &app,
        r#"credit(artiste: "Bill Evans", instrument: piano) genre:jazz"#,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(titres(&corps), vec!["Blue in Green"]);
    assert_eq!(corps["total"], 1);
    assert_eq!(corps["regles_refusees"], json!([]));
    assert_eq!(corps["match_mode"], "all");
    assert_eq!(
        corps["rules"],
        json!([
            {"field": "credit", "op": "has", "value": {"artist_name": "Bill Evans", "instrument": "piano"}},
            {"field": "genre", "op": "contains", "value": "jazz"}
        ])
    );
}

#[tokio::test]
async fn une_requete_imbriquee_s_execute_sans_forme_d_editeur_6287() {
    let app = app();
    let (statut, corps) = requete(&app, "non credit(instrument: piano) ou titre:what").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let mut t = titres(&corps);
    t.sort();
    assert_eq!(t, vec!["So What", "Solo"]);
    assert_eq!(corps["rules"], Value::Null);
    assert_eq!(corps["match_mode"], Value::Null);
}

#[tokio::test]
async fn une_requete_fautive_rend_400_avec_sa_position_6287() {
    let app = app();
    let (statut, corps) = requete(&app, "genre:jazz couleur:bleu").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "requete_invalide");
    assert_eq!(corps["position"], 11);
    assert!(
        corps["message"]
            .as_str()
            .unwrap_or_default()
            .contains("couleur"),
        "{corps}"
    );
    // Vide : jamais « toute la bibliothèque ».
    let (statut, corps) = requete(&app, "  ").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
}

/// Relecture rc7 : sous NON, un critère refusé rendrait toute la
/// bibliothèque. La route refuse la requête entière.
#[tokio::test]
async fn un_refus_sous_une_negation_rend_400_6287() {
    let app = app();
    let (statut, corps) = requete(&app, "non ajout:foo..bar").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "requete_invalide");
    assert!(
        corps["message"]
            .as_str()
            .unwrap_or_default()
            .contains("négation"),
        "{corps}"
    );
    // Sans NON, le refus reste signalé et ne retient rien.
    let (statut, corps) = requete(&app, "ajout:foo..bar").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["total"], 0, "{corps}");
    assert_ne!(corps["regles_refusees"], json!([]), "{corps}");
}

/// Relecture rc7 : longueur et profondeur bornées, en 400.
#[tokio::test]
async fn une_requete_demesuree_rend_400_6287() {
    let app = app();
    let longue = format!("titre:{}", "a".repeat(5000));
    let profonde = format!("{}genre:jazz{}", "(".repeat(500), ")".repeat(500));
    for q in [longue, profonde] {
        let (statut, corps) = requete(&app, &q).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
        assert_eq!(corps["error"], "requete_invalide", "{corps}");
    }
}

// ── Étape 4 : enregistrer une requête imbriquée ────────────────────────────

const PLAYLISTS: &str = "/api/v1/library/smart-playlists";

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(uri)
        .header("content-type", "application/json")
        .body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Essayer, enregistrer ce que la route dit d'enregistrer, recharger, rendre
/// les pistes : la playlist rend ce que l'essai rendait.
#[tokio::test]
async fn une_requete_imbriquee_s_enregistre_se_recharge_et_s_evalue_6287() {
    let app = app();
    let q = "non credit(instrument: piano) ou titre:what";
    let (statut, essai) = requete(&app, q).await;
    assert_eq!(statut, StatusCode::OK, "{essai}");
    let enregistrement = &essai["enregistrement"];
    assert_eq!(
        *enregistrement,
        json!({"rules": [{"field": "requete", "op": "is", "value": q}], "match_mode": "all"})
    );

    let (statut, cree) = appel(
        &app,
        "POST",
        PLAYLISTS,
        Some(json!({
            "name": "Imbriquée",
            "rules": enregistrement["rules"],
            "match_mode": enregistrement["match_mode"],
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{cree}");
    let id = cree["id"].as_i64().expect("id");

    let (statut, relue) = appel(&app, "GET", &format!("{PLAYLISTS}/{id}"), None).await;
    assert_eq!(statut, StatusCode::OK, "{relue}");
    assert_eq!(relue["rules"], enregistrement["rules"], "{relue}");
    assert_eq!(relue["match_mode"], "all", "{relue}");

    let (statut, pistes) = appel(&app, "GET", &format!("{PLAYLISTS}/{id}/tracks"), None).await;
    assert_eq!(statut, StatusCode::OK, "{pistes}");
    let mut t: Vec<String> = pistes
        .as_array()
        .unwrap_or_else(|| panic!("pas de pistes : {pistes}"))
        .iter()
        .map(|p| p["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    let mut attendu = titres(&essai);
    attendu.sort();
    assert_eq!(t, attendu);
    assert_eq!(t, vec!["So What", "Solo"]);
}

/// Une requête plate garde son chemin : des règles d'éditeur ordinaires.
#[tokio::test]
async fn une_requete_plate_s_enregistre_en_regles_d_editeur_6287() {
    let app = app();
    let (statut, essai) = requete(&app, "genre:jazz annee:1959").await;
    assert_eq!(statut, StatusCode::OK, "{essai}");
    assert_eq!(essai["enregistrement"]["rules"], essai["rules"], "{essai}");
    assert_eq!(essai["enregistrement"]["match_mode"], "all", "{essai}");
}

/// Une requête fautive n'est pas écrite : 400 `requete_invalide`, à la
/// création comme à la modification, et la playlist garde ses règles.
#[tokio::test]
async fn une_requete_fautive_n_est_pas_enregistree_6287() {
    let app = app();
    let fautive = json!([{"field": "requete", "op": "is", "value": "genre:jazz couleur:bleu"}]);
    let (statut, corps) = appel(
        &app,
        "POST",
        PLAYLISTS,
        Some(json!({"name": "Fautive", "rules": fautive})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "requete_invalide", "{corps}");
    assert_eq!(corps["position"], 11, "{corps}");
    let (_, liste) = appel(&app, "GET", PLAYLISTS, None).await;
    assert!(
        !liste
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Fautive"),
        "{liste}"
    );

    let bonne = json!([{"field": "requete", "op": "is", "value": "non genre:rock"}]);
    let (statut, cree) = appel(
        &app,
        "POST",
        PLAYLISTS,
        Some(json!({"name": "Bonne", "rules": bonne})),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{cree}");
    let uri = format!("{PLAYLISTS}/{}", cree["id"]);
    let sous_non = json!([{"field": "requete", "op": "is", "value": "non ajout:foo..bar"}]);
    let (statut, corps) = appel(&app, "PUT", &uri, Some(json!({"rules": sous_non}))).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(corps["error"], "requete_invalide", "{corps}");
    let (_, relue) = appel(&app, "GET", &uri, None).await;
    assert_eq!(relue["rules"], bonne, "{relue}");
}
