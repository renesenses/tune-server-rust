//! #4889 — une playlist Tune peut porter un TITRE DE SERVICE.
//!
//! FabienM, fil forum 1906, réponse 6704 : « une playlist peut être locale et
//! peut contenir des titres de services, oui ou non ? Si oui […] on doit
//! pouvoir à partir d'un titre de service l'ajouter à une playlist locale ».
//!
//! Historique de ce banc : il gardait d'abord #1848 — le serveur répondait
//! `201 Created` sans rien ajouter, parce que `streaming_tracks` n'était pas
//! déclaré —, puis le REFUS en 422 qui lui avait succédé :
//! `playlist_tracks.track_id` était `NOT NULL REFERENCES tracks(id)`, une
//! ligne ne savait nommer qu'une piste de la bibliothèque.
//!
//! La migration SQLite 109 / PG 072 donne à une ligne de quoi porter un titre
//! de service (`source`, `source_id` et ses colonnes d'affichage). Ce banc
//! garde désormais le contrat de #4889, route par route :
//!
//! - l'AJOUT enregistre le titre (le corps EXACT d'`AddToPlaylistModal`), le
//!   dédoublonne, et refuse en le disant un titre incomplet ;
//! - la LISTE le rend au format d'une piste de streaming (`id` nul,
//!   `source`, `source_id`) ;
//! - le RÉORDONNANCEMENT le déplace (`positions`) ou le garde à son rang
//!   (ancienne forme `track_ids`) ;
//! - la LECTURE enfile les deux sortes de lignes, dans l'ordre ;
//! - DUPLIQUER, EXPORTER et VERSER DANS LA FILE le portent aussi ;
//! - et le chemin tout local reste celui d'avant (témoins).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// --- socle -------------------------------------------------------------

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

fn zone(state: &tune_server::state::AppState, nom: &str) -> i64 {
    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("local"), None)
        .expect("creation de zone")
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    // Les refus partent en texte brut : on rend alors la chaîne, pour que le
    // test puisse lire la RAISON.
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

async fn envoie(app: &axum::Router, methode: &str, path: &str, body: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::builder()
            .method(methode)
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn poste(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    envoie(app, "POST", path, body).await
}

async fn lis(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn playlist_vide(app: &axum::Router, nom: &str) -> i64 {
    let (st, v) = poste(app, "/api/v1/playlists", json!({ "name": nom })).await;
    assert_eq!(st, StatusCode::CREATED, "création de playlist: {v}");
    v.get("id").and_then(Value::as_i64).expect("id de playlist")
}

async fn pistes(app: &axum::Router, playlist: i64) -> Vec<Value> {
    let (st, v) = lis(app, &format!("/api/v1/playlists/{playlist}/tracks")).await;
    assert_eq!(st, StatusCode::OK, "lecture des pistes: {v}");
    v.as_array().cloned().unwrap_or_default()
}

/// Ce qui identifie chaque ligne rendue : `L<id>` pour une piste locale,
/// `<source>:<source_id>` pour un titre de service.
async fn sortes(app: &axum::Router, playlist: i64) -> Vec<String> {
    pistes(app, playlist)
        .await
        .iter()
        .map(|p| match p.get("id").and_then(Value::as_i64) {
            Some(id) => format!("L{id}"),
            None => format!(
                "{}:{}",
                p["source"].as_str().unwrap_or("?"),
                p["source_id"].as_str().unwrap_or("?")
            ),
        })
        .collect()
}

/// Le corps EXACT que `AddToPlaylistModal.buildAddArgs()` produit pour une
/// piste de service : `track_ids` vide, tout le contenu dans
/// `streaming_tracks` (type `StreamingTrackInfo`).
fn corps_de_service(source: &str, source_id: &str, titre: &str) -> Value {
    json!({
        "track_ids": [],
        "streaming_tracks": [{
            "source": source,
            "source_id": source_id,
            "title": titre,
            "artist_name": "Giovanni Battista Sammartini",
            "album_title": "Sinfonie",
            "duration_ms": 431_000,
            "cover_path": "https://exemple.invalid/pochette.jpg",
        }]
    })
}

async fn ajoute(app: &axum::Router, playlist: i64, corps: Value) -> (StatusCode, Value) {
    poste(app, &format!("/api/v1/playlists/{playlist}/tracks"), corps).await
}

// --- l'ajout -------------------------------------------------------------

/// Le cœur de #4889 : un titre Bandcamp (le cas de FabienM) entre dans une
/// playlist Tune, et la liste le rend au format d'une piste de streaming.
#[tokio::test]
async fn un_titre_de_service_entre_dans_une_playlist_tune() {
    let st = etat();
    let app = appli(&st);
    let pl = playlist_vide(&app, "Découvertes").await;

    let (status, corps) = ajoute(&app, pl, corps_de_service("bandcamp", "bc-42", "Nuit")).await;
    assert_eq!(status, StatusCode::CREATED, "corps rendu {corps}");
    assert_eq!(
        corps["track_count"], 1,
        "la playlist compte la ligne : {corps}"
    );
    assert!(
        corps.get("skipped_streaming").is_none(),
        "rien n'a été écarté : {corps}"
    );

    let lignes = pistes(&app, pl).await;
    assert_eq!(lignes.len(), 1, "{lignes:?}");
    let l = &lignes[0];
    assert_eq!(l["id"], Value::Null, "pas de `tracks.id` : {l}");
    assert_eq!(l["source"], "bandcamp");
    assert_eq!(l["source_id"], "bc-42");
    assert_eq!(l["title"], "Nuit");
    assert_eq!(l["artist_name"], "Giovanni Battista Sammartini");
    assert_eq!(l["album_title"], "Sinfonie");
    assert_eq!(l["duration_ms"], 431_000);
    assert_eq!(l["cover_path"], "https://exemple.invalid/pochette.jpg");
}

#[tokio::test]
async fn le_meme_titre_de_service_n_entre_qu_une_fois() {
    let st = etat();
    let app = appli(&st);
    let pl = playlist_vide(&app, "Qobuz").await;
    for _ in 0..2 {
        let (status, corps) =
            ajoute(&app, pl, corps_de_service("qobuz", "52818331", "Sinfonia")).await;
        assert_eq!(status, StatusCode::CREATED, "{corps}");
    }
    assert_eq!(sortes(&app, pl).await, vec!["qobuz:52818331"]);
}

/// Un titre sans titre (ou sans identifiant, ou annoncé `local`) ne peut
/// pas être affiché sans rappeler le service : il est refusé, et le refus le
/// DIT (doctrine #1959).
#[tokio::test]
async fn un_titre_de_service_incomplet_est_refuse_en_le_disant() {
    let st = etat();
    let app = appli(&st);
    let pl = playlist_vide(&app, "Incomplets").await;

    let (status, corps) = ajoute(
        &app,
        pl,
        json!({
            "track_ids": [],
            "streaming_tracks": [
                { "source": "qobuz", "source_id": "1" },
                { "source": "local", "source_id": "2", "title": "Déguisée" },
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
    let texte = corps.as_str().unwrap_or_default();
    assert!(
        texte.contains("incomplets") && texte.contains("titre"),
        "{texte}"
    );
    assert!(pistes(&app, pl).await.is_empty(), "rien ne doit être écrit");
}

/// Une demande MIXTE écrit les pistes locales et les titres de service
/// complets, dans cet ordre, et compte ce qu'elle a écarté.
#[tokio::test]
async fn une_demande_mixte_enregistre_tout_ce_qui_est_complet() {
    let st = etat();
    let app = appli(&st);
    let t1 = piste(&st, "Prelude", "/musique/prelude.flac");
    let t2 = piste(&st, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(&app, "Mélange").await;

    let (status, corps) = ajoute(
        &app,
        pl,
        json!({
            "track_ids": [t1, t2],
            "streaming_tracks": [
                { "source": "tidal", "source_id": "77120044", "title": "Aria" },
                { "source": "qobuz", "source_id": "52818331" },
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{corps}");
    assert_eq!(
        sortes(&app, pl).await,
        vec![format!("L{t1}"), format!("L{t2}"), "tidal:77120044".into()]
    );
    assert_eq!(
        corps.get("skipped_streaming").and_then(Value::as_i64),
        Some(1),
        "le titre Qobuz sans titre est écarté, et c'est dit — {corps}"
    );
}

// --- réordonner, dupliquer, exporter ------------------------------------

async fn playlist_mixte(st: &tune_server::state::AppState, app: &axum::Router) -> (i64, i64, i64) {
    let t1 = piste(st, "Prelude", "/musique/prelude.flac");
    let t2 = piste(st, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(app, "Mixte").await;
    let (s, c) = ajoute(app, pl, json!({ "track_ids": [t1] })).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let (s, c) = ajoute(app, pl, corps_de_service("bandcamp", "bc-42", "Nuit")).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let (s, c) = ajoute(app, pl, json!({ "track_ids": [t2] })).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    (pl, t1, t2)
}

/// `positions` (les rangs affichés) déplace AUSSI le titre de service.
#[tokio::test]
async fn reordonner_par_rangs_deplace_le_titre_de_service() {
    let st = etat();
    let app = appli(&st);
    let (pl, t1, t2) = playlist_mixte(&st, &app).await;

    let chemin = format!("/api/v1/playlists/{pl}/tracks");
    let (status, corps) = envoie(&app, "PUT", &chemin, json!({ "positions": [1, 2, 0] })).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{corps}");
    assert_eq!(
        sortes(&app, pl).await,
        vec![
            "bandcamp:bc-42".to_string(),
            format!("L{t2}"),
            format!("L{t1}")
        ]
    );

    // Une liste périmée n'écrit rien, et le dit.
    let (status, corps) = envoie(&app, "PUT", &chemin, json!({ "positions": [0, 1] })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
    assert_eq!(
        sortes(&app, pl).await,
        vec![
            "bandcamp:bc-42".to_string(),
            format!("L{t2}"),
            format!("L{t1}")
        ]
    );
}

/// L'ancienne forme (`track_ids`, celle des clients déployés) ne sait pas
/// nommer le titre de service : il GARDE son rang au lieu d'être effacé.
#[tokio::test]
async fn l_ancien_reordonnancement_n_efface_pas_le_titre_de_service() {
    let st = etat();
    let app = appli(&st);
    let (pl, t1, t2) = playlist_mixte(&st, &app).await;

    let (status, corps) = envoie(
        &app,
        "PUT",
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "track_ids": [t2, t1] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{corps}");
    assert_eq!(
        sortes(&app, pl).await,
        vec![
            format!("L{t2}"),
            "bandcamp:bc-42".to_string(),
            format!("L{t1}")
        ]
    );
}

#[tokio::test]
async fn dupliquer_recopie_le_titre_de_service() {
    let st = etat();
    let app = appli(&st);
    let (pl, _, _) = playlist_mixte(&st, &app).await;

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/duplicate"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{corps}");
    assert_eq!(corps["track_count"], 3, "{corps}");
    let copie = corps["id"].as_i64().unwrap();
    assert_eq!(sortes(&app, copie).await, sortes(&app, pl).await);
}

#[tokio::test]
async fn l_export_json_porte_le_titre_de_service() {
    let st = etat();
    let app = appli(&st);
    let (pl, _, _) = playlist_mixte(&st, &app).await;

    let (status, corps) = lis(&app, &format!("/api/v1/playlists/{pl}/export?format=json")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let lignes = corps["tracks"].as_array().cloned().unwrap_or_default();
    assert_eq!(lignes.len(), 3, "{corps}");
    assert_eq!(lignes[1]["source"], "bandcamp");
    assert_eq!(lignes[1]["source_id"], "bc-42");
    assert_eq!(lignes[1]["title"], "Nuit");
}

// --- lecture ----------------------------------------------------------------

/// « Lire » une playlist mixte enfile les DEUX sortes de lignes, dans l'ordre
/// affiché : le titre de service y est une ligne de streaming, résolue comme
/// n'importe quelle piste de service. La zone de ce banc n'a pas de sortie :
/// la lecture elle-même peut échouer, la FILE, elle, doit être écrite.
#[tokio::test]
async fn lire_une_playlist_mixte_enfile_les_titres_de_service() {
    let st = etat();
    let app = appli(&st);
    let (pl, t1, t2) = playlist_mixte(&st, &app).await;
    let zid = zone(&st, "Salon");

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "playlist_id": pl, "start_index": 1 }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "une playlist mixte n'est pas « sans piste » — {corps}"
    );

    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(st.backend.clone())
        .get_ordered(zid)
        .unwrap();
    let vues: Vec<String> = file
        .iter()
        .map(|e| match e.track_id {
            Some(id) => format!("L{id}"),
            None => format!(
                "{}:{}",
                e.source.as_deref().unwrap_or("?"),
                e.source_id.as_deref().unwrap_or("?")
            ),
        })
        .collect();
    assert_eq!(
        vues,
        vec![
            format!("L{t1}"),
            "bandcamp:bc-42".to_string(),
            format!("L{t2}")
        ],
        "la file porte toute la playlist, dans l'ordre"
    );
    assert_eq!(file[1].title.as_deref(), Some("Nuit"), "le titre voyage");
}

/// « Verser dans la file » (POST /playlists/transfer) porte aussi le titre
/// de service.
#[tokio::test]
async fn verser_une_playlist_mixte_dans_la_file() {
    let st = etat();
    let app = appli(&st);
    let (pl, _, _) = playlist_mixte(&st, &app).await;
    let zid = zone(&st, "Cuisine");

    let (status, corps) = poste(
        &app,
        "/api/v1/playlists/transfer",
        json!({ "playlist_id": pl, "zone_id": zid }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["transferred"], 3, "{corps}");
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(st.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert!(
        file.iter()
            .any(|e| e.source.as_deref() == Some("bandcamp")
                && e.source_id.as_deref() == Some("bc-42")),
        "{file:?}"
    );
}

// --- témoins anti-régression -------------------------------------------
//
// Verts AVANT comme APRÈS #4889 : ils gardent le chemin tout local.

#[tokio::test]
async fn temoin_l_ajout_de_pistes_locales_reste_intact() {
    let st = etat();
    let app = appli(&st);
    let t1 = piste(&st, "Prelude", "/musique/prelude.flac");
    let t2 = piste(&st, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(&app, "Locale").await;

    let (status, corps) = ajoute(&app, pl, json!({ "track_ids": [t1, t2] })).await;
    assert_eq!(status, StatusCode::CREATED, "corps rendu {corps}");
    assert_eq!(
        sortes(&app, pl).await,
        vec![format!("L{t1}"), format!("L{t2}")]
    );
    assert!(
        corps.get("skipped_streaming").is_none(),
        "sans piste de service, le compteur n'a pas à apparaître : {corps}"
    );
    // Réajouter la même piste locale : dédoublonnée, comme avant.
    let (status, _) = ajoute(&app, pl, json!({ "track_ids": [t1] })).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(pistes(&app, pl).await.len(), 2);
}

#[tokio::test]
async fn temoin_une_demande_vide_garde_son_comportement() {
    let st = etat();
    let app = appli(&st);
    let pl = playlist_vide(&app, "Vide").await;

    let (status, corps) = ajoute(&app, pl, json!({ "track_ids": [] })).await;
    assert_eq!(status, StatusCode::CREATED, "corps rendu {corps}");
    assert!(pistes(&app, pl).await.is_empty());
}

#[tokio::test]
async fn temoin_une_piste_locale_inexistante_n_entre_pas() {
    // La clé étrangère des lignes LOCALES tient toujours après la 109.
    let st = etat();
    let app = appli(&st);
    let pl = playlist_vide(&app, "Fantome").await;

    let (_, _) = ajoute(&app, pl, json!({ "track_ids": [987_654_321i64] })).await;
    assert!(
        pistes(&app, pl).await.is_empty(),
        "une piste absente de `tracks` ne peut pas entrer dans `playlist_tracks`"
    );
}

#[tokio::test]
async fn temoin_lire_une_playlist_locale_garde_le_chemin_d_avant() {
    let st = etat();
    let app = appli(&st);
    let t1 = piste(&st, "Prelude", "/musique/prelude.flac");
    let t2 = piste(&st, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(&app, "Locale").await;
    ajoute(&app, pl, json!({ "track_ids": [t1, t2] })).await;
    let zid = zone(&st, "Bureau");

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "playlist_id": pl }),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "{corps}");
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(st.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert_eq!(
        file.iter().map(|e| e.track_id).collect::<Vec<_>>(),
        vec![Some(t1), Some(t2)]
    );
}
