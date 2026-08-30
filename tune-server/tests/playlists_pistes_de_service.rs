//! #1848 — « Ajouter à une liste de lecture » sur une piste de service :
//! le serveur répondait `201 Created` sans avoir rien ajouté.
//!
//! Dominique Comet, fil forum #1452 : « lorsqu'on sélectionne une piste nous
//! n'avons pas les mêmes possibilités sur la bibliothèque et sur Qobuz ».
//!
//! Le client OFFRE l'action sur une piste de service — `StreamingView.svelte`
//! l. 1134, 1216 et 1592, `{#if onAddToPlaylist && (t.id || t.source_id)}` :
//! le bouton s'affiche dès qu'il y a un `source_id`, sans `id` local.
//! `AddToPlaylistModal.svelte` construit alors le corps
//! `{ track_ids: [], streaming_tracks: [{source, source_id, …}] }`.
//!
//! Or `struct AddTracks` ne déclarait que `track_ids` et `position`. serde
//! écarte en silence tout champ non déclaré : `streaming_tracks` tombait, la
//! route appelait `add_tracks_deduped(id, &[], …)`, n'ajoutait rien, et
//! répondait `201 Created` avec la playlist. Le modal lisait ce 201 comme un
//! succès (`success = playlist.name`) et affichait « ajoutée ».
//!
//! Ce n'est PAS réparable en stockant la piste : `playlist_tracks.track_id` est
//! `NOT NULL REFERENCES tracks(id)` dans les trois définitions de schéma
//! (`tune-core/src/db/sqlite.rs`, `migrations/postgres/001_initial_schema.sql`,
//! `tune-core/src/db/pg_migrate.rs`). Une playlist locale ne PEUT pas porter
//! une piste de service. Le refus est légitime — c'est de le déguiser en
//! succès qui ne l'était pas. Même doctrine que #1959 (`save_queue_as_playlist`,
//! `playback.rs`) : 422 avec la raison, et `skipped_streaming` sur le cas mixte.

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

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    // Les refus partent en texte brut (comme `save_queue_as_playlist`) : on
    // rend alors la chaîne, pour que le test puisse lire la RAISON.
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

async fn poste(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn lis(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn playlist_vide(app: &axum::Router, nom: &str) -> i64 {
    let (st, v) = poste(app, "/api/v1/playlists", json!({ "name": nom })).await;
    assert_eq!(st, StatusCode::CREATED, "création de playlist: {v}");
    v.get("id").and_then(Value::as_i64).expect("id de playlist")
}

async fn nombre_de_pistes(app: &axum::Router, playlist: i64) -> usize {
    let (st, v) = lis(app, &format!("/api/v1/playlists/{playlist}/tracks")).await;
    assert_eq!(st, StatusCode::OK, "lecture des pistes: {v}");
    v.as_array().map(Vec::len).unwrap_or(0)
}

/// Le corps EXACT que `AddToPlaylistModal.buildAddArgs()` produit pour une
/// piste Qobuz : `track_ids` vide, tout le contenu dans `streaming_tracks`.
fn corps_qobuz() -> Value {
    json!({
        "track_ids": [],
        "streaming_tracks": [{
            "source": "qobuz",
            "source_id": "52818331",
            "title": "Sinfonia in D major",
            "artist_name": "Giovanni Battista Sammartini",
            "album_title": "Sinfonie",
            "duration_ms": 431_000,
        }]
    })
}

// --- le défaut ---------------------------------------------------------

#[tokio::test]
async fn une_piste_de_service_seule_n_est_pas_annoncee_comme_ajoutee() {
    let st_app = etat();
    let app = appli(&st_app);
    let pl = playlist_vide(&app, "Découvertes Qobuz").await;

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        corps_qobuz(),
    )
    .await;

    assert_ne!(
        status,
        StatusCode::CREATED,
        "201 pour une playlist restée vide : c'est le mensonge de #1848 — corps rendu {corps}"
    );
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "le refus doit être un 422, comme #1959 — corps rendu {corps}"
    );
    assert_eq!(
        nombre_de_pistes(&app, pl).await,
        0,
        "rien ne doit être ajouté : le schéma l'interdit"
    );
}

#[tokio::test]
async fn le_refus_dit_sa_raison_et_ne_reste_pas_muet() {
    let st_app = etat();
    let app = appli(&st_app);
    let pl = playlist_vide(&app, "Découvertes Qobuz").await;

    let (_, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        corps_qobuz(),
    )
    .await;

    let texte = corps.as_str().unwrap_or_default().to_string();
    assert!(
        !texte.trim().is_empty(),
        "un refus muet reproduit le défaut de #1959 — corps rendu {corps}"
    );
    assert!(
        texte.contains("service"),
        "le message doit nommer la cause (piste de service) : {texte}"
    );
    assert!(
        texte.contains("locale"),
        "le message doit dire que c'est la playlist LOCALE qui ne peut pas la porter : {texte}"
    );
}

#[tokio::test]
async fn une_demande_mixte_enregistre_les_locales_et_declare_les_ignorees() {
    let st_app = etat();
    let app = appli(&st_app);
    let t1 = piste(&st_app, "Prelude", "/musique/prelude.flac");
    let t2 = piste(&st_app, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(&app, "Mélange").await;

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({
            "track_ids": [t1, t2],
            "streaming_tracks": [
                { "source": "qobuz", "source_id": "52818331" },
                { "source": "tidal", "source_id": "77120044" },
            ]
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "une demande mixte porte des pistes locales : elle doit aboutir — {corps}"
    );
    assert_eq!(
        nombre_de_pistes(&app, pl).await,
        2,
        "les deux pistes locales doivent être enregistrées"
    );
    assert_eq!(
        corps.get("skipped_streaming").and_then(Value::as_i64),
        Some(2),
        "taire les ignorées produit le défaut d'à côté : une playlist plus \
         courte que la demande, sans que rien ne dise pourquoi — corps {corps}"
    );
}

// --- témoins anti-régression -------------------------------------------
//
// Verts AVANT comme APRÈS le correctif : ils gardent le chemin local, qui ne
// doit rien changer.

#[tokio::test]
async fn temoin_l_ajout_de_pistes_locales_reste_intact() {
    let st_app = etat();
    let app = appli(&st_app);
    let t1 = piste(&st_app, "Prelude", "/musique/prelude.flac");
    let t2 = piste(&st_app, "Fugue", "/musique/fugue.flac");
    let pl = playlist_vide(&app, "Locale").await;

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "track_ids": [t1, t2] }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "corps rendu {corps}");
    assert_eq!(nombre_de_pistes(&app, pl).await, 2);
    assert!(
        corps.get("skipped_streaming").is_none(),
        "sans piste de service, le compteur n'a pas à apparaître : {corps}"
    );
}

#[tokio::test]
async fn temoin_une_demande_vide_garde_son_comportement() {
    // Aucune piste, d'aucune sorte : la route répondait 201 sans rien faire.
    // Ce n'est pas le sujet de #1848 et d'autres appelants en dépendent
    // peut-être ; le correctif ne doit PAS y toucher.
    let st_app = etat();
    let app = appli(&st_app);
    let pl = playlist_vide(&app, "Vide").await;

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "track_ids": [] }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "corps rendu {corps}");
    assert_eq!(nombre_de_pistes(&app, pl).await, 0);
}

// --- contre-épreuve du socle -------------------------------------------

#[tokio::test]
async fn contre_epreuve_le_schema_refuse_bien_une_piste_inexistante() {
    // Le fondement du refus : `playlist_tracks.track_id` est
    // `NOT NULL REFERENCES tracks(id)`. Si ce test devenait vert avec un id
    // absent de `tracks`, c'est que la contrainte a sauté — et alors le refus
    // 422 ci-dessus n'aurait plus lieu d'être.
    let st_app = etat();
    let app = appli(&st_app);
    let pl = playlist_vide(&app, "Fantome").await;

    let (_, _) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "track_ids": [987_654_321i64] }),
    )
    .await;

    assert_eq!(
        nombre_de_pistes(&app, pl).await,
        0,
        "une piste absente de `tracks` ne peut pas entrer dans `playlist_tracks`"
    );
}
