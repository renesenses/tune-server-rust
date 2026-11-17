//! #8958 — « Marquer comme normale » : l'alerte ne revient plus, la liste des
//! ignorées la montre, l'annulation la rend.

use crate::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

async fn route(state: &AppState, methode: &str, uri: &str, corps: &str) -> (StatusCode, Value) {
    let r = super::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    let code = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .expect("corps");
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

const DOSSIER: &str = "/data/recordings/Tidal/Prince/Ultimate";

/// Pose la bibliothèque avec des identifiants décalés de `decalage` — un
/// rescan complet (`delete_all` puis réinsertion) donne de NOUVEAUX ids.
/// Album « Sound Effects » sans artiste : pistes 1 (normale) et 2 (2 s).
/// Piste 3 : artiste connu, très courte, sans album.
fn poser(state: &AppState, decalage: i64) {
    let b = &state.backend;
    let album = 1 + decalage;
    b.execute(
        "INSERT INTO albums (id, title) VALUES (?1, 'Sound Effects')",
        &[&album as &dyn ToSqlValue],
    )
    .expect("album");
    for (id, artiste, album, duree, chemin) in [
        (
            1 + decalage,
            1i64,
            Some(album),
            180_000i64,
            format!("{DOSSIER}/01.flac"),
        ),
        (
            2 + decalage,
            1,
            Some(album),
            2_000,
            format!("{DOSSIER}/02.flac"),
        ),
        (
            3 + decalage,
            2,
            None,
            3_000,
            "/m/divers/bip.flac".to_string(),
        ),
    ] {
        b.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, file_path) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'local', ?6)",
            &[
                &id as &dyn ToSqlValue,
                &format!("p{}", id - decalage),
                &artiste,
                &album,
                &duree,
                &chemin,
            ],
        )
        .expect("piste");
    }
}

fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let b = &state.backend;
    b.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Unknown Artist')",
        &[],
    )
    .expect("artiste inconnu");
    b.execute("INSERT INTO artists (id, name) VALUES (2, 'Didier')", &[])
        .expect("artiste");
    poser(&state, 0);
    state
}

/// Le rescan complet : tout est effacé puis réinséré sous d'autres ids.
fn rescanner(state: &AppState) {
    state.backend.execute("DELETE FROM tracks", &[]).unwrap();
    state.backend.execute("DELETE FROM albums", &[]).unwrap();
    poser(state, 100);
}

async fn douteuses(state: &AppState) -> Value {
    let (code, v) = route(state, "GET", "/doubtful?limit=100", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        v["total"].as_i64(),
        Some(v["items"].as_array().map(|a| a.len()).unwrap_or(0) as i64),
        "le total doit valoir la liste : {v}"
    );
    v
}

/// Les raisons de la piste de chemin `fin` (les ids changent au rescan).
fn raisons(v: &Value, fin: &str) -> Option<Vec<String>> {
    v["items"]
        .as_array()?
        .iter()
        .find(|i| i["file_path"].as_str().is_some_and(|p| p.ends_with(fin)))
        .map(|i| {
            i["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect()
        })
}

async fn ignorer(state: &AppState, piste: i64, raison: &str) -> (StatusCode, Value) {
    route(
        state,
        "POST",
        "/doubtful/ignored",
        &format!(r#"{{"track_id":{piste},"reason":"{raison}"}}"#),
    )
    .await
}

#[tokio::test]
async fn la_liste_annonce_la_portee_de_chaque_alerte() {
    let state = etat();
    let v = douteuses(&state).await;
    assert_eq!(
        v["can_ignore"], true,
        "authentification coupée : tout le monde est administrateur"
    );
    let p2 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == 2)
        .unwrap();
    assert_eq!(p2["ignore_scopes"]["missing_artist"], "album", "{p2}");
    assert_eq!(p2["ignore_scopes"]["very_short"], "track", "{p2}");
    let p3 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == 3)
        .unwrap();
    assert_eq!(p3["ignore_scopes"]["missing_album"], "track", "{p3}");
}

/// Une piste courte voulue : ignorée pour ELLE seule, par son CHEMIN ; elle
/// reste listée tant que son autre alerte (artiste) tient.
#[tokio::test]
async fn piste_tres_courte_ignoree_garde_son_autre_alerte() {
    let state = etat();
    assert_eq!(douteuses(&state).await["total"], 3);
    let (code, r) = ignorer(&state, 2, "very_short").await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["scope"], "track");
    assert_eq!(r["key"], format!("{DOSSIER}/02.flac"));
    let v = douteuses(&state).await;
    assert_eq!(v["total"], 3, "la piste 2 manque toujours d'artiste : {v}");
    assert_eq!(
        raisons(&v, "/02.flac"),
        Some(vec!["missing_artist".to_string()])
    );

    let (_, r) = ignorer(&state, 2, "very_short").await;
    assert_eq!(r["added"], false, "rejouée : rien de plus");
}

/// L'artiste manquant se mémorise pour l'ALBUM, par son dossier et son titre.
#[tokio::test]
async fn artiste_manquant_ignore_pour_tout_l_album() {
    let state = etat();
    let (_, avant) = route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"dry_run":true}"#,
    )
    .await;
    assert_eq!(avant["tracks"], 2, "aperçu avant : {avant}");
    let (code, r) = ignorer(&state, 1, "missing_artist").await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["scope"], "album");
    assert_eq!(r["key"], DOSSIER);
    assert_eq!(r["title"], "Sound Effects");
    let v = douteuses(&state).await;
    assert_eq!(
        raisons(&v, "/01.flac"),
        None,
        "piste 1 : plus aucune alerte : {v}"
    );
    assert_eq!(
        raisons(&v, "/02.flac"),
        Some(vec!["very_short".to_string()])
    );
    let compte = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .count_doubtful()
        .unwrap();
    assert_eq!(compte, 2, "la pastille compte pareil");
    let (_, apercu) = route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"dry_run":true}"#,
    )
    .await;
    assert_eq!(apercu["tracks"], 0, "aperçu : {apercu}");
}

/// 🔴 Le rescan complet efface et réinsère tout sous d'AUTRES identifiants :
/// les choix tiennent par la clé stable (chemin, dossier + titre).
#[tokio::test]
async fn les_choix_survivent_a_un_rescan_complet() {
    let state = etat();
    ignorer(&state, 1, "missing_artist").await;
    ignorer(&state, 3, "very_short").await;
    rescanner(&state);
    let v = douteuses(&state).await;
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"].as_i64().unwrap() > 100),
        "{v}"
    );
    assert_eq!(raisons(&v, "/01.flac"), None, "album : {v}");
    assert_eq!(
        raisons(&v, "/02.flac"),
        Some(vec!["very_short".to_string()])
    );
    assert_eq!(
        raisons(&v, "/bip.flac"),
        Some(vec!["missing_album".to_string()])
    );

    // La liste les retrouve sous leurs nouveaux ids.
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 2, "{l}");
    for it in l["items"].as_array().unwrap() {
        assert_eq!(it["found"], true, "{it}");
    }
    let album = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["scope"] == "album")
        .unwrap();
    assert_eq!(album["album_id"], 101);
    assert_eq!(album["track_count"], 2);

    // Un scan complet sain ne purge que ce qui ne correspond plus à rien.
    assert_eq!(
        tune_core::db::alertes_douteuses_ignorees::purger_introuvables(&state.backend).unwrap(),
        0
    );
}

/// Un autre album du même titre, ailleurs : le dossier le départage.
#[tokio::test]
async fn un_homonyme_ailleurs_n_est_pas_couvert() {
    let state = etat();
    ignorer(&state, 1, "missing_artist").await;
    let b = &state.backend;
    b.execute(
        "INSERT INTO albums (id, title) VALUES (9, 'Sound Effects')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, file_path) \
         VALUES (9, 'autre', 1, 9, 180000, 'local', '/data/recordings/Tidal/Prince/Ultimate 2/01.flac')",
        &[],
    )
    .unwrap();
    let v = douteuses(&state).await;
    assert_eq!(
        raisons(&v, "Ultimate 2/01.flac"),
        Some(vec!["missing_artist".to_string()]),
        "{v}"
    );
}

/// La liste des ignorées la montre ; l'annulation la rend.
#[tokio::test]
async fn revoir_puis_annuler() {
    let state = etat();
    ignorer(&state, 3, "very_short").await;
    ignorer(&state, 3, "missing_album").await;
    let v = douteuses(&state).await;
    assert_eq!(
        raisons(&v, "/bip.flac"),
        None,
        "les deux alertes de la piste 3 sont ignorées : {v}"
    );

    let (code, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(l["total"], 2, "{l}");
    assert_eq!(l["items"][0]["track_title"], "p3", "{l}");
    assert_eq!(l["items"][0]["file_path"], "/m/divers/bip.flac", "{l}");

    let corps = r#"{"scope":"track","key":"/m/divers/bip.flac","title":"","reason":"very_short"}"#;
    let (code, r) = route(&state, "DELETE", "/doubtful/ignored", corps).await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(
        raisons(&douteuses(&state).await, "/bip.flac"),
        Some(vec!["very_short".to_string()])
    );
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 1);

    let (code, _) = route(&state, "DELETE", "/doubtful/ignored", corps).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "déjà annulée");
}

#[tokio::test]
async fn demandes_refusees() {
    let state = etat();
    let (code, _) = ignorer(&state, 2, "mauvaise").await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = ignorer(&state, 99, "very_short").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // Une raison qui ne s'applique pas à la piste : la piste 1 dure 3 min.
    let (code, r) = ignorer(&state, 1, "very_short").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{r}");
    let (code, _) = route(
        &state,
        "DELETE",
        "/doubtful/ignored",
        r#"{"scope":"disque","key":"/x","reason":"very_short"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(douteuses(&state).await["total"], 3, "rien n'a été retenu");
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 0);
}

/// La lecture ne purge JAMAIS : des fichiers momentanément absents gardent
/// leur choix (signalé introuvable) ; seule la purge de fin de scan complet
/// retire ce qui ne correspond plus à rien.
#[tokio::test]
async fn la_lecture_ne_purge_pas_la_fin_de_scan_complet_si() {
    let state = etat();
    ignorer(&state, 3, "very_short").await;
    ignorer(&state, 1, "missing_artist").await;
    state
        .backend
        .execute("DELETE FROM tracks WHERE id = 3", &[])
        .unwrap();
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 2, "{l}");
    let piste = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["scope"] == "track")
        .unwrap();
    assert_eq!(piste["found"], false, "{piste}");
    assert_eq!(piste["label"], "p3", "le nom figé au choix reste lisible");

    let db = &state.backend;
    tune_core::db::alertes_douteuses_ignorees::purger_en_fin_de_scan(db, false, "test");
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 2, "scan partiel : rien n'est purgé");
    tune_core::db::alertes_douteuses_ignorees::purger_en_fin_de_scan(db, true, "test");
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(
        l["total"], 1,
        "scan complet : seul l'introuvable part : {l}"
    );
    assert_eq!(l["items"][0]["scope"], "album");
}

/// Authentification active, appel sans jeton : l'écran n'offre pas le geste,
/// et le serveur le refuse.
#[tokio::test]
async fn reserve_a_l_administrateur() {
    let state = etat();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    let (_, v) = route(&state, "GET", "/doubtful?limit=100", "").await;
    assert_eq!(v["can_ignore"], false, "{v}");
    let (code, _) = ignorer(&state, 2, "very_short").await;
    assert!(
        code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN,
        "{code}"
    );
}
