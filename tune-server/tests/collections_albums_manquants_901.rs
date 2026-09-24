//! La LISTE des albums manquants d'un dossier « Collections » (#901).
//!
//! Demandé par Lulu (JLuc), fils 1664 puis 1891 : « serait-il possible d'avoir
//! également la liste de ces albums par dossier ». Le serveur la CALCULAIT
//! déjà — `partager_ids` sépare les identifiants vivants des morts, et les
//! écrit en toutes lettres dans le journal — puis il n'en publiait que la
//! LONGUEUR, sous une clé pourtant nommée `orphan_album_ids`.
//!
//! Deux gestes, indissociables :
//!   1. publier la liste au lieu de sa longueur ;
//!   2. conserver le TITRE de l'album au moment où on le range, parce qu'un
//!      album disparu de la base n'a plus de nom nulle part — sans lui, la
//!      « liste » serait une suite de numéros illisibles.
//!
//! ⚠️ Compatibilité : `orphan_album_ids` change de forme (nombre → liste). Le
//! nombre reste publié, sous `orphan_album_count`. Un client d'avant #901 lit
//! `orphan_album_ids` derrière `typeof === 'number'` : il masque la mention,
//! il ne plante pas et n'affiche aucun compte faux — c'est le test
//! `un_client_qui_attend_un_nombre_ne_lit_jamais_un_compte_faux`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

fn make_app_with_state() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn delete(app: &axum::Router, path: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::delete(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

fn seed_album(state: &tune_server::state::AppState, artist: &str, title: &str) -> i64 {
    let artists = ArtistRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let a = artists.get_or_create(artist, None, None).unwrap();
    let album = albums
        .get_or_create(title, a.id.unwrap(), None)
        .unwrap_or_else(|e| panic!("album {title}: {e}"));
    album.id.unwrap()
}

/// Le dossier tel que la LISTE des dossiers le sert.
async fn dossier_servi(app: &axum::Router, cid: i64) -> Value {
    let (st, liste) = get(app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK, "liste des dossiers: {liste}");
    liste
        .as_array()
        .expect("un tableau de dossiers")
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .unwrap_or_else(|| panic!("le dossier {cid} est dans la liste: {liste}"))
        .clone()
}

/// Le réglage `collections` tel qu'il est STOCKÉ.
fn collections_stockees(state: &tune_server::state::AppState) -> Vec<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let brut = settings.get("collections").unwrap().unwrap_or_default();
    serde_json::from_str(&brut).unwrap_or_default()
}

fn dossier_stocke(state: &tune_server::state::AppState, cid: i64) -> Value {
    collections_stockees(state)
        .into_iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .expect("le dossier est dans le réglage")
}

/// Trois albums vivants, deux morts : cinq rangés, puis deux qui disparaissent
/// de la base sans que personne ne touche au dossier. Rend l'identifiant du
/// dossier et les deux disparus avec le nom qu'ils avaient.
async fn dossier_avec_deux_disparus(
    app: &axum::Router,
    state: &tune_server::state::AppState,
) -> (i64, Vec<(i64, &'static str, &'static str)>) {
    let vivants = [
        seed_album(state, "ABBA", "Arrival"),
        seed_album(state, "Beethoven", "Symphonies"),
        seed_album(state, "Frank Zappa", "Hot Rats"),
    ];
    let condamnes = [
        (
            seed_album(state, "Disque Retiré", "Rescan Perdu"),
            "Rescan Perdu",
            "Disque Retiré",
        ),
        (
            seed_album(state, "Chemin Changé", "Volume Démonté"),
            "Volume Démonté",
            "Chemin Changé",
        ),
    ];

    let (st, col) = post_json(
        app,
        "/api/v1/library/collections",
        json!({"name": "Coffret"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création du dossier: {col}");
    let cid = col["id"].as_i64().unwrap();

    // Rangés dans le désordre : les condamnés au milieu, pas en queue.
    for id in [
        vivants[0],
        condamnes[0].0,
        vivants[1],
        condamnes[1].0,
        vivants[2],
    ] {
        let (st, _) = post_json(
            app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "ajout de l'album {id}");
    }

    let albums = AlbumRepo::with_backend(state.backend.clone());
    for (id, _, _) in condamnes {
        albums.delete(id).unwrap();
    }
    (cid, condamnes.to_vec())
}

/// 🔴 L'ÉPREUVE QUI TRANCHE — ce que Lulu demande est publié : les
/// IDENTIFIANTS, pas leur nombre.
#[tokio::test]
async fn la_liste_des_manquants_est_publiee_et_non_sa_seule_longueur() {
    let (app, state) = make_app_with_state();
    let (cid, disparus) = dossier_avec_deux_disparus(&app, &state).await;

    let dossier = dossier_servi(&app, cid).await;
    let ids = dossier["orphan_album_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("`orphan_album_ids` doit porter une LISTE: {dossier}"));

    let vus: Vec<i64> = ids.iter().filter_map(|v| v.as_i64()).collect();
    assert_eq!(vus.len(), 2, "deux identifiants morts: {dossier}");
    for (id, _, _) in &disparus {
        assert!(
            vus.contains(id),
            "l'identifiant {id} de l'album disparu doit être NOMMÉ dans la liste: {dossier}"
        );
    }
    assert_eq!(
        dossier["orphan_album_count"].as_i64(),
        Some(2),
        "le compte reste publié, sous un nom qui le dit: {dossier}"
    );
    assert_eq!(dossier["album_count"].as_i64(), Some(3));
}

/// 🔴 LE SECOND GESTE, sans lequel le premier ne sert à rien : les albums
/// disparus portent le TITRE qu'ils avaient au rangement. Ils ne sont plus en
/// base — aucune requête ne peut plus le retrouver.
#[tokio::test]
async fn les_manquants_portent_le_titre_conserve_au_rangement() {
    let (app, state) = make_app_with_state();
    let (cid, disparus) = dossier_avec_deux_disparus(&app, &state).await;

    let dossier = dossier_servi(&app, cid).await;
    let manquants = dossier["orphan_albums"]
        .as_array()
        .unwrap_or_else(|| panic!("`orphan_albums` doit être servi: {dossier}"));
    assert_eq!(manquants.len(), 2, "{dossier}");

    for (id, titre, artiste) in &disparus {
        let trouve = manquants
            .iter()
            .find(|m| m["id"].as_i64() == Some(*id))
            .unwrap_or_else(|| panic!("album {id} absent de la liste nommée: {dossier}"));
        assert_eq!(
            trouve["title"].as_str(),
            Some(*titre),
            "un titre, pas un numéro: {trouve}"
        );
        assert_eq!(trouve["artist"].as_str(), Some(*artiste), "{trouve}");
    }
}

/// ⚠️ COMPATIBILITÉ. Un client d'avant #901 lit `orphan_album_ids` derrière un
/// `typeof === 'number'` : il ne doit jamais tomber sur un compte FAUX. Cette
/// garde tient les deux bouts — la clé ancienne n'est plus un nombre (donc
/// aucun client ne peut l'afficher comme tel), et le nombre reste disponible,
/// entier et juste, sous `orphan_album_count`.
#[tokio::test]
async fn un_client_qui_attend_un_nombre_ne_lit_jamais_un_compte_faux() {
    let (app, state) = make_app_with_state();
    let (cid, _) = dossier_avec_deux_disparus(&app, &state).await;

    let dossier = dossier_servi(&app, cid).await;
    assert!(
        dossier["orphan_album_ids"].as_i64().is_none(),
        "la clé ne doit plus se lire comme un nombre: {dossier}"
    );
    assert!(
        dossier["orphan_album_count"].is_number(),
        "le nombre doit rester lisible quelque part: {dossier}"
    );
    assert_eq!(
        dossier["orphan_album_count"].as_i64().unwrap(),
        dossier["orphan_album_ids"].as_array().unwrap().len() as i64,
        "le compte et la liste ne peuvent pas diverger: {dossier}"
    );

    // La réserve d'étiquettes est un STOCK, pas une donnée d'écran : elle ne
    // doit pas doubler la taille de la réponse d'un dossier de 2 000 albums.
    assert!(
        dossier.get("album_labels").is_none(),
        "les étiquettes ne sont pas servies: {dossier}"
    );
    assert!(
        !dossier_stocke(&state, cid)["album_labels"].is_null(),
        "… mais elles sont bien STOCKÉES"
    );
}

/// Un dossier sain ne déclare aucun manquant — le témoin qui interdit une
/// garde verte contre une liste toujours vide.
#[tokio::test]
async fn un_dossier_sain_ne_declare_aucun_manquant() {
    let (app, state) = make_app_with_state();
    let a = seed_album(&state, "Nina Simone", "Pastel Blues");
    let b = seed_album(&state, "Ella Fitzgerald", "Songbook");

    let (st, col) = post_json(&app, "/api/v1/library/collections", json!({"name": "Sain"})).await;
    assert_eq!(st, StatusCode::CREATED, "{col}");
    let cid = col["id"].as_i64().unwrap();
    for id in [a, b] {
        let (st, _) = post_json(
            &app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    let dossier = dossier_servi(&app, cid).await;
    assert_eq!(dossier["orphan_album_ids"].as_array().unwrap().len(), 0);
    assert_eq!(dossier["orphan_album_count"].as_i64(), Some(0));
    assert_eq!(dossier["orphan_albums"].as_array().unwrap().len(), 0);
}

/// 🔴 LE RATTRAPAGE. Les dossiers existants ont été rangés AVANT que
/// l'étiquette n'existe : leurs albums n'ont pas de nom conservé. Ouvrir le
/// dossier pendant que l'album vit encore le relève — c'est le seul moment où
/// il est encore lisible, et il ne coûte aucune requête de plus.
///
/// Le dossier est écrit à la main dans le réglage, exactement comme une
/// v0.9.161 l'aurait laissé : des identifiants, rien d'autre.
#[tokio::test]
async fn ouvrir_un_dossier_ancien_releve_le_nom_des_albums_encore_vivants() {
    let (app, state) = make_app_with_state();
    let condamne = seed_album(&state, "Chemin Changé", "Volume Démonté");
    let vivant = seed_album(&state, "ABBA", "Arrival");

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "collections",
            &json!([{
                "id": 7,
                "name": "Rangé avant #901",
                "album_ids": [vivant, condamne],
                "created_at": "2026-09-01T00:00:00Z",
            }])
            .to_string(),
        )
        .unwrap();

    // Témoin : rien n'est conservé tant que personne n'a ouvert le dossier.
    assert!(
        dossier_stocke(&state, 7)["album_labels"].is_null(),
        "un dossier d'avant #901 n'a aucune étiquette"
    );

    // L'utilisateur ouvre son dossier. Les deux albums vivent encore.
    let (st, rendus) = get(&app, "/api/v1/library/collections/7/albums").await;
    assert_eq!(st, StatusCode::OK, "{rendus}");
    assert_eq!(rendus.as_array().unwrap().len(), 2);

    let etiquettes = dossier_stocke(&state, 7)["album_labels"].clone();
    assert_eq!(
        etiquettes[condamne.to_string()]["title"].as_str(),
        Some("Volume Démonté"),
        "le nom a été relevé à l'ouverture: {etiquettes}"
    );

    // L'album disparaît PLUS TARD. Son nom, lui, est déjà à l'abri.
    AlbumRepo::with_backend(state.backend.clone())
        .delete(condamne)
        .unwrap();

    let dossier = dossier_servi(&app, 7).await;
    let manquant = &dossier["orphan_albums"][0];
    assert_eq!(manquant["id"].as_i64(), Some(condamne), "{dossier}");
    assert_eq!(
        manquant["title"].as_str(),
        Some("Volume Démonté"),
        "un dossier ancien finit par nommer ses manquants: {dossier}"
    );
    // Et l'ouverture n'a PAS purgé le rangement.
    assert_eq!(
        dossier_stocke(&state, 7)["album_ids"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "on signale, on ne détruit pas"
    );
}

/// Un identifiant mort sans étiquette n'invente rien : il sort avec son numéro
/// et un titre nul, et le client dira ce qu'il peut.
#[tokio::test]
async fn un_manquant_sans_etiquette_sort_avec_un_titre_nul_pas_invente() {
    let (app, state) = make_app_with_state();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "collections",
            &json!([{ "id": 4, "name": "Jamais ouvert", "album_ids": [424242] }]).to_string(),
        )
        .unwrap();

    let dossier = dossier_servi(&app, 4).await;
    assert_eq!(dossier["orphan_album_ids"], json!([424242]), "{dossier}");
    let manquant = &dossier["orphan_albums"][0];
    assert_eq!(manquant["id"].as_i64(), Some(424242));
    assert!(
        manquant["title"].is_null(),
        "aucun titre inventé: {manquant}"
    );
}

/// Un album sorti du dossier n'y manque plus : son étiquette part avec lui, et
/// la réserve n'enfle pas à chaque rangement défait.
#[tokio::test]
async fn retirer_un_album_du_dossier_emporte_son_etiquette() {
    let (app, state) = make_app_with_state();
    let id = seed_album(&state, "Nina Simone", "Pastel Blues");
    let (st, col) = post_json(&app, "/api/v1/library/collections", json!({"name": "R"})).await;
    assert_eq!(st, StatusCode::CREATED, "{col}");
    let cid = col["id"].as_i64().unwrap();
    let (st, _) = post_json(
        &app,
        &format!("/api/v1/library/collections/{cid}/albums/{id}"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        dossier_stocke(&state, cid)["album_labels"][id.to_string()]["title"].as_str(),
        Some("Pastel Blues"),
        "l'étiquette est posée AU RANGEMENT, pas plus tard"
    );

    let st = delete(
        &app,
        &format!("/api/v1/library/collections/{cid}/albums/{id}"),
    )
    .await;
    assert!(st.is_success(), "retrait: {st}");
    assert!(
        dossier_stocke(&state, cid)["album_labels"][id.to_string()].is_null(),
        "l'étiquette part avec l'album: {}",
        dossier_stocke(&state, cid)
    );
}
