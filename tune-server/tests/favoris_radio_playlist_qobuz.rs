//! Favoris radio → playlist Qobuz (chantier du 14/10/2026) : le BRANCHEMENT
//! des routes.
//!
//! - `POST /api/v1/radio-favorites/match-preview` cherche chaque favori chez
//!   le service et rend la correspondance, SANS rien créer ;
//! - `POST /api/v1/radio-favorites/create-playlist` avec `track_ids` crée la
//!   playlist avec exactement les titres retenus, sans rapparier.
//!
//! Un service SIMULÉ nommé « qobuz » est inscrit au registre ; aucun réseau.
//! L'appariement fin (accents, « feat. », remaster, introuvable) contre le
//! vrai connecteur Qobuz et un faux serveur HTTP est gardé dans
//! `tune-core/src/streaming/appariement_favoris_radio_tests.rs`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

#[derive(Default)]
struct Journal {
    recherches: Vec<String>,
    creations: Vec<String>,
    ajouts: Vec<Vec<String>>,
    /// Qobuz refuse la création (`/playlist/create` en 401).
    refuser_creation: bool,
    /// Qobuz crée la playlist puis refuse l'ajout des titres.
    refuser_ajout: bool,
}

struct QobuzSimule {
    journal: Arc<Mutex<Journal>>,
    connecte: bool,
    ecriture: bool,
}

fn piste(id: &str, titre: &str, artiste: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: Some("Album d'essai".into()),
        album_id: None,
        duration_ms: 200_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn catalogue() -> Vec<StreamTrack> {
    vec![
        piste("q-heroes", "Heroes", "David Bowie"),
        piste("q-heroes-rm", "Heroes (2017 Remaster)", "David Bowie"),
        piste("q-hal-jb", "Hallelujah", "Jeff Buckley"),
        piste("q-hal-lc", "Hallelujah", "Leonard Cohen"),
    ]
}

#[async_trait]
impl StreamingService for QobuzSimule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "qobuz"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(Default::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    /// Recherche stricte : chaque mot de la requête doit figurer dans
    /// « artiste titre ».
    async fn search(&self, q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        self.journal.lock().unwrap().recherches.push(q.to_string());
        let mots: Vec<String> = q
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let tracks = catalogue()
            .into_iter()
            .filter(|t| {
                let botte = format!("{} {}", t.artist, t.title).to_lowercase();
                let botte: Vec<&str> = botte.split_whitespace().collect();
                mots.iter().all(|m| botte.contains(&m.as_str()))
            })
            .collect();
        Ok(SearchResults {
            tracks,
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    /// La playlist existante « pl-existante » contient déjà « q-heroes ».
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        if id == "pl-existante" {
            Ok(vec![piste("q-heroes", "Heroes", "David Bowie")])
        } else {
            Err("playlist inconnue".into())
        }
    }
    /// « Mes playlists » : celles que ce simulé a créées.
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(self
            .journal
            .lock()
            .unwrap()
            .creations
            .iter()
            .enumerate()
            .map(|(i, nom)| StreamPlaylist {
                id: format!("pl-essai-{}", i + 1),
                name: nom.clone(),
                description: None,
                cover_path: None,
                track_count: 0,
                owner: None,
                editable: Some(true),
                covers: Vec::new(),
            })
            .collect())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
    fn supports_write(&self) -> bool {
        self.ecriture
    }
    async fn create_playlist(&self, name: &str, _d: Option<&str>) -> Result<String, TuneError> {
        let mut j = self.journal.lock().unwrap();
        if j.refuser_creation {
            return Err("qobuz /playlist/create: 401 {\"code\":401}".into());
        }
        j.creations.push(name.to_string());
        Ok(format!("pl-essai-{}", j.creations.len()))
    }
    async fn add_tracks_to_playlist(&self, _p: &str, ids: &[String]) -> Result<usize, TuneError> {
        let mut j = self.journal.lock().unwrap();
        if j.refuser_ajout {
            return Err("qobuz /playlist/addTracks: 400 {\"code\":400}".into());
        }
        j.ajouts.push(ids.to_vec());
        Ok(ids.len())
    }
}

async fn appel(app: &axum::Router, path: &str, corps: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn banc(
    connecte: bool,
    ecriture: bool,
) -> (axum::Router, Arc<Mutex<Journal>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let journal = Arc::new(Mutex::new(Journal::default()));
    state.services.lock().await.register(Box::new(QobuzSimule {
        journal: journal.clone(),
        connecte,
        ecriture,
    }));
    let app = tune_server::routes::router(state.clone());
    for (title, artist, station) in [
        ("Heroes", "David Bowie", "FIP"),
        ("David Bowie - Heroes", "", "Nova"),
        ("Hallelujah", "", "FIP"),
        ("Un titre inconnu", "Personne", "FIP"),
    ] {
        let (st, _) = appel(
            &app,
            "/api/v1/radio-favorites",
            json!({ "title": title, "artist": artist, "station_name": station }),
        )
        .await;
        assert!(st.is_success(), "favori « {title} » refusé ({st})");
    }
    (app, journal, dir)
}

fn par_titre<'a>(items: &'a [Value], titre: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["title"] == titre)
        .unwrap_or_else(|| panic!("favori « {titre} » absent de l'aperçu : {items:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn l_apercu_rend_chaque_correspondance_sans_rien_creer() {
    let (app, journal, _dir) = banc(true, true).await;
    // Sans `service` : Qobuz par défaut.
    let (st, body) = appel(&app, "/api/v1/radio-favorites/match-preview", json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["service"], "qobuz");
    assert_eq!(body["favorites_count"], 4, "{body}");
    let items = body["items"].as_array().expect("items[]");

    let heroes = par_titre(items, "Heroes");
    assert_eq!(heroes["status"], "matched", "{heroes}");
    assert_eq!(heroes["match"]["id"], "q-heroes", "{heroes}");
    assert!(heroes["favorite_id"].as_i64().is_some(), "{heroes}");
    assert_eq!(heroes["station_name"], "FIP");

    // Le titre ICY composite est scindé, et la même piste est signalée
    // comme doublon du favori précédent.
    let icy = par_titre(items, "David Bowie - Heroes");
    assert_eq!(icy["match"]["id"], "q-heroes", "{icy}");
    assert!(
        !icy["duplicate_of"].is_null() || !heroes["duplicate_of"].is_null(),
        "deux favoris, une seule piste : l'un des deux est doublon de l'autre : {items:?}"
    );

    let hal = par_titre(items, "Hallelujah");
    assert_eq!(hal["status"], "ambiguous", "{hal}");
    assert!(!hal["alternatives"].as_array().unwrap().is_empty(), "{hal}");

    let inconnu = par_titre(items, "Un titre inconnu");
    assert_eq!(inconnu["status"], "not_found", "{inconnu}");
    assert!(inconnu["match"].is_null());

    assert_eq!(body["ambiguous"], 1, "{body}");
    assert_eq!(body["not_found"], 1, "{body}");

    let j = journal.lock().unwrap();
    assert!(
        !j.recherches.is_empty(),
        "l'aperçu doit chercher chez le service"
    );
    assert!(
        j.creations.is_empty() && j.ajouts.is_empty(),
        "l'aperçu ne crée RIEN chez le service : créations {:?}, ajouts {:?}",
        j.creations,
        j.ajouts
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn la_creation_verse_exactement_les_titres_retenus() {
    let (app, journal, _dir) = banc(true, true).await;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({
            "service": "qobuz",
            "playlist_name": "Favoris radio — 14/10/2026",
            "track_ids": ["q-hal-lc", "q-heroes", "q-hal-lc"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["playlist_id"], "pl-essai-1");
    assert_eq!(body["added"], 2, "{body}");
    assert_eq!(body["duplicates_skipped"], 1, "{body}");
    assert!(body["add_error"].is_null(), "{body}");

    let j = journal.lock().unwrap();
    assert_eq!(j.creations, vec!["Favoris radio — 14/10/2026".to_string()]);
    assert_eq!(
        j.ajouts,
        vec![vec!["q-hal-lc".to_string(), "q-heroes".to_string()]],
        "l'ordre et le choix de l'utilisateur sont respectés, doublon retiré"
    );
    assert!(
        j.recherches.is_empty(),
        "avec `track_ids`, aucun nouvel appariement : {:?}",
        j.recherches
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn les_refus_sont_nets() {
    // Aucun titre retenu.
    let (app, journal, _dir) = banc(true, true).await;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "x", "track_ids": [] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "aucun_titre_retenu");
    // Nom vide.
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "  ", "track_ids": ["q-heroes"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    // Identifiants de service vers la bibliothèque locale.
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "local", "playlist_name": "x", "track_ids": ["q-heroes"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(journal.lock().unwrap().creations.is_empty());

    // Service inconnu.
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/match-preview",
        json!({ "service": "nulle-part" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "service_inconnu");
}

#[tokio::test(flavor = "multi_thread")]
async fn un_qobuz_deconnecte_refuse_l_apercu_sans_chercher() {
    let (app, journal, _dir) = banc(false, true).await;
    let (st, body) = appel(&app, "/api/v1/radio-favorites/match-preview", json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "service_non_connecte");
    assert!(journal.lock().unwrap().recherches.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn un_service_sans_ecriture_refuse_l_apercu() {
    let (app, journal, _dir) = banc(true, false).await;
    let (st, body) = appel(&app, "/api/v1/radio-favorites/match-preview", json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "service_sans_ecriture");
    assert!(journal.lock().unwrap().recherches.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn l_ajout_a_une_playlist_existante_ne_remet_pas_ce_qu_elle_contient() {
    let (app, journal, _dir) = banc(true, true).await;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({
            "service": "qobuz",
            "target_playlist_id": "pl-existante",
            "track_ids": ["q-heroes", "q-hal-jb"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["playlist_id"], "pl-existante", "{body}");
    assert_eq!(body["existing_playlist"], true, "{body}");
    assert_eq!(body["added"], 1, "{body}");
    assert_eq!(body["already_present"], 1, "{body}");
    let j = journal.lock().unwrap();
    assert!(
        j.creations.is_empty(),
        "aucune création : {:?}",
        j.creations
    );
    assert_eq!(
        j.ajouts,
        vec![vec!["q-hal-jb".to_string()]],
        "« q-heroes » est déjà dans la playlist cible : il ne repart pas"
    );
}

/// La création « en un geste » d'avant (sans `track_ids`) passe désormais
/// par `apparier_favori_radio` : elle doit toujours pousser les pistes sûres.
#[tokio::test(flavor = "multi_thread")]
async fn la_creation_en_un_geste_marche_toujours() {
    let (app, journal, _dir) = banc(true, true).await;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "Un geste", "limit": 10 }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["favorites_count"], 4, "{body}");
    assert_eq!(body["not_found"], 1, "{body}");
    let j = journal.lock().unwrap();
    assert_eq!(j.creations, vec!["Un geste".to_string()]);
    let ajoutes = j.ajouts.concat();
    assert!(ajoutes.contains(&"q-heroes".to_string()), "{ajoutes:?}");
}

async fn lire(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Radio → Qobuz, 01/11/2026 — la playlist existait chez Qobuz mais pas dans
/// « Mes playlists » de Tune. La modale lit la liste À L'OUVERTURE (choix de
/// la cible), ce qui la met en cache 120 s juste AVANT la création. Après la
/// création, la liste relue doit la contenir.
///
/// Contre-épreuve (faite à la main, retirer la purge de
/// `creer_playlist_depuis_titres_retenus`) : la seconde lecture rend la
/// liste mémorisée, vide, et ce témoin échoue.
#[tokio::test(flavor = "multi_thread")]
async fn la_playlist_creee_apparait_aussitot_dans_mes_playlists() {
    let (app, journal, _dir) = banc(true, true).await;
    // Ce que fait la modale à l'ouverture.
    let (st, avant) = lire(&app, "/api/v1/streaming/qobuz/playlists").await;
    assert_eq!(st, StatusCode::OK, "{avant}");
    assert!(
        !avant.to_string().contains("Favoris radio — 01/11/2026"),
        "{avant}"
    );

    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({
            "service": "qobuz",
            "playlist_name": "Favoris radio — 01/11/2026",
            "track_ids": ["q-heroes"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["added"], 1, "{body}");
    assert_eq!(journal.lock().unwrap().creations.len(), 1);

    let (st, apres) = lire(&app, "/api/v1/streaming/qobuz/playlists").await;
    assert_eq!(st, StatusCode::OK, "{apres}");
    let noms: Vec<&str> = apres
        .as_array()
        .expect("liste de playlists")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        noms.contains(&"Favoris radio — 01/11/2026"),
        "la playlist créée doit figurer dans « Mes playlists » : {noms:?}"
    );
    let p = apres
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Favoris radio — 01/11/2026")
        .unwrap();
    assert_eq!(
        p["editable"], true,
        "c'est SA playlist, pas un abonnement : {p}"
    );
}

/// Un refus de Qobuz à la création n'est JAMAIS un succès : 502, rien de
/// créé, et une phrase traduite à montrer (`message` + `message_key`, #7077)
/// en plus du code ; le motif brut du service va dans `cause`. Sans langue
/// dans la requête, la phrase est en anglais (repli du serveur).
#[tokio::test(flavor = "multi_thread")]
async fn un_refus_de_creation_remonte_au_client_avec_un_message() {
    let (app, journal, _dir) = banc(true, true).await;
    journal.lock().unwrap().refuser_creation = true;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "x", "track_ids": ["q-heroes"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body["playlist_id"].is_null(), "{body}");
    assert_eq!(body["code"], "creation_playlist_refusee", "{body}");
    assert_eq!(
        body["message_key"], "api.radioFavorites.createPlaylistRefused",
        "{body}"
    );
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("Qobuz refused to create"), "{body}");
    assert!(
        body["cause"].as_str().unwrap_or_default().contains("401"),
        "le motif du service est rendu : {body}"
    );

    // Contre-épreuve : la même demande, Qobuz d'accord → 201.
    journal.lock().unwrap().refuser_creation = false;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "x", "track_ids": ["q-heroes"] }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(body["message"].is_null(), "{body}");
}

/// La liste vide est expliquée : code stable ET phrase lisible.
#[tokio::test(flavor = "multi_thread")]
async fn une_selection_vide_est_expliquee() {
    let (app, journal, _dir) = banc(true, true).await;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "x", "track_ids": ["", "  "] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "aucun_titre_retenu");
    assert_eq!(body["message_key"], "api.radioFavorites.noTrackSelected");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no playlist was created"),
        "{body}"
    );
    assert!(journal.lock().unwrap().creations.is_empty());
}

/// En un geste : un ajout refusé APRÈS la création était journalisé puis
/// avalé (la réponse annonçait les titres). Il est rendu dans `add_error`.
/// Contre-épreuve : ajout accepté → `add_error` nul.
#[tokio::test(flavor = "multi_thread")]
async fn en_un_geste_l_ajout_refuse_n_est_plus_avale() {
    let (app, journal, _dir) = banc(true, true).await;
    journal.lock().unwrap().refuser_ajout = true;
    let (st, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "Un geste", "limit": 10 }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(
        body["add_error"]
            .as_str()
            .unwrap_or_default()
            .contains("addTracks"),
        "{body}"
    );
    journal.lock().unwrap().refuser_ajout = false;
    let (_, body) = appel(
        &app,
        "/api/v1/radio-favorites/create-playlist",
        json!({ "service": "qobuz", "playlist_name": "Un geste 2", "limit": 10 }),
    )
    .await;
    assert!(body["add_error"].is_null(), "{body}");
}
