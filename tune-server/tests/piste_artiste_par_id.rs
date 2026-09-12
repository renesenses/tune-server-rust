//! #2574 — `PUT /library/tracks/{id}` disait « c'est fait » sans rien faire.
//!
//! FabienM, fil forum 1580 (27/08/2026) : « On ne retrouve pas les mêmes
//! fonctions/options disponibles sur un titre en fonction du menu. » L'une de
//! ces fonctions est le crayon « éditer les métadonnées ». Sur le chemin
//! d'assignation d'artiste, elle ne faisait rien du tout.
//!
//! `MetadataView.svelte` appelle `api.updateTrack(id, { artist_id })` à trois
//! endroits — l. 338 (`applyArtistAndAlbum`), l. 1042 et l. 1135 — et
//! `artist_id` y est la SEULE clé du corps ; `api.ts:1579` la déclare dans la
//! signature. Or `struct TrackEdit` ne déclarait pas ce champ, et serde écarte
//! en silence tout champ inconnu (aucun `deny_unknown_fields` dans ce dépôt).
//!
//! Conséquence : les onze champs restaient `None`, `edit_track` réécrivait la
//! piste INCHANGÉE, et répondait `200 {"status":"ok"}`. L'écran met sa liste à
//! jour de façon optimiste — l'artiste s'affichait donc assigné, et un simple
//! rechargement le remettait à « Unknown ».
//!
//! Rien ne s'y opposait en base : `Track::artist_id` existe et
//! `TrackRepo::update` écrit bien `artist_id` (`UPDATE tracks SET …
//! artist_id = …`). C'est exactement le jumeau non corrigé d'`album_id`, dont
//! le commentaire dans `TrackEdit` documente déjà le même défaut.
//!
//! Même motif que #1848/#2979 sur `POST /playlists/{id}/tracks` : le test qui
//! vaut est celui qui **envoie le corps que le client envoie** et vérifie que
//! le serveur ne répond pas « c'est fait » quand il n'a rien fait.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Artist, Track};
use tune_core::db::track_repo::TrackRepo;

// --- socle -------------------------------------------------------------

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

/// Une piste SANS `file_path` : `edit_track` ne touche alors à aucun fichier
/// et le test porte sur la base seule, sans écrire sur le disque.
fn piste(state: &tune_server::state::AppState, titre: &str) -> i64 {
    TrackRepo::with_backend(state.backend.clone())
        .create(&Track::new(titre.into()))
        .expect("insert track")
}

fn artiste(state: &tune_server::state::AppState, nom: &str) -> i64 {
    ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new(nom.into()))
        .expect("insert artist")
}

fn relis(state: &tune_server::state::AppState, id: i64) -> Track {
    TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .expect("lecture piste")
        .expect("piste presente")
}

async fn put(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::put(path)
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
    // Les refus partent en texte brut : on rend alors la chaîne, pour que le
    // test puisse lire la RAISON et pas seulement le code.
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

// --- la contre-épreuve -------------------------------------------------

/// Le corps EXACT que `MetadataView.svelte` envoie : `artist_id` seul.
///
/// ROUGE sans le correctif — le champ tombe, la piste garde `artist_id: None`,
/// et la route répond quand même `200 {"status":"ok"}`.
#[tokio::test]
async fn artiste_assigne_par_id_est_reellement_enregistre() {
    let state = etat();
    let app = appli(&state);

    let aid = artiste(&state, "Serge Lama");
    let tid = piste(&state, "Les ballons rouges");

    assert_eq!(
        relis(&state, tid).artist_id,
        None,
        "temoin de depart : la piste n'a pas encore d'artiste"
    );

    let (st, corps) = put(
        &app,
        &format!("/api/v1/library/tracks/{tid}"),
        json!({ "artist_id": aid }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "corps rendu : {corps}");

    let piste = relis(&state, tid);
    assert_eq!(
        piste.artist_id,
        Some(aid),
        "la route a repondu OK : la piste DOIT porter l'artiste demande"
    );
    assert_eq!(
        piste.artist_name.as_deref(),
        Some("Serge Lama"),
        "le nom porte par la piste doit suivre le rattachement, sinon l'ecran \
         qui la relit affiche encore l'ancien artiste"
    );
}

/// Un id d'artiste qui ne désigne rien est refusé, avec la raison.
///
/// ROUGE sans le correctif : la route répondait `200 {"status":"ok"}`. Écrire
/// la clé étrangère sans vérifier serait l'autre moitié du même défaut — la
/// piste disparaîtrait de la bibliothèque et le client verrait encore un 200.
#[tokio::test]
async fn artiste_inconnu_est_refuse_avec_la_raison() {
    let state = etat();
    let app = appli(&state);
    let tid = piste(&state, "Les ballons rouges");

    let (st, corps) = put(
        &app,
        &format!("/api/v1/library/tracks/{tid}"),
        json!({ "artist_id": 987_654 }),
    )
    .await;

    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "un artiste inexistant ne doit pas passer pour un succes : {corps}"
    );
    let texte = corps.as_str().unwrap_or_default();
    assert!(
        texte.contains("987654") || texte.contains("987 654"),
        "le refus doit nommer l'identifiant refuse, sinon il n'apprend rien : {texte:?}"
    );

    assert_eq!(
        relis(&state, tid).artist_id,
        None,
        "et la piste ne doit surtout pas porter une cle etrangere morte"
    );
}

// --- témoins anti-régression (verts des DEUX côtés) --------------------

/// Le chemin qui marchait doit continuer de marcher : un titre seul.
#[tokio::test]
async fn temoin_le_titre_seul_reste_applique() {
    let state = etat();
    let app = appli(&state);
    let tid = piste(&state, "Titre d'origine");

    let (st, corps) = put(
        &app,
        &format!("/api/v1/library/tracks/{tid}"),
        json!({ "title": "Les ballons rouges" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "corps rendu : {corps}");
    assert_eq!(relis(&state, tid).title, "Les ballons rouges");
}

/// Le jumeau DÉJÀ corrigé — `album_id` — ne doit pas régresser en chemin.
#[tokio::test]
async fn temoin_album_id_reste_applique() {
    let state = etat();
    let app = appli(&state);
    let tid = piste(&state, "Les ballons rouges");

    let album_id = {
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::models::Album;
        AlbumRepo::with_backend(state.backend.clone())
            .create(&Album::new("Sinfonie".into()))
            .expect("insert album")
    };

    let (st, corps) = put(
        &app,
        &format!("/api/v1/library/tracks/{tid}"),
        json!({ "album_id": album_id }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "corps rendu : {corps}");

    let piste = relis(&state, tid);
    assert_eq!(piste.album_id, Some(album_id));
    assert_eq!(piste.album_title.as_deref(), Some("Sinfonie"));
}
