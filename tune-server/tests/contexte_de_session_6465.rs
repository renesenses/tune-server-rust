//! #6465 — un geste de lecture qui ne désigne aucun conteneur efface le
//! contexte de session laissé par le geste précédent.
//!
//! FabienM, fil 2302, points 7, 8 et 9 : après un album, une radio ou un
//! podcast se rangeait DANS cet album dans l'Historique, et la vignette « en
//! cours » renvoyait à lui. Seul `POST /zones/{id}/play` réécrivait le
//! contexte ; `POST /radios/{id}/play/{zone}`, `POST /podcasts/play/{zone}`
//! et `POST /zones/{id}/pins/{index}/invoke` laissaient en place celui de
//! l'album. La radio d'artiste est gardée dans `radio_artiste_5395.rs`, qui a
//! le service simulé qu'il lui faut.
//!
//! Hermétique : les adresses visent un port fermé de la boucle locale.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ZONE: i64 = 1;

async fn banc() -> (tune_server::state::AppState, axum::Router) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO zones (id, name) VALUES (1, 'Salon');
             INSERT INTO radio_stations (id, name, url) VALUES (9001, 'Station', 'http://127.0.0.1:9/flux');
             INSERT INTO settings (key, value) VALUES ('zone_1_pins',
               '[{\"index\":0,\"title\":\"Préréglage\",\"uri\":\"http://127.0.0.1:9/pin\",\"type\":\"radio\"}]');",
        )
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// Le contexte que laisse « Lire » sur l'album 101 (point 8 : « radio Agoria
/// suite à lecture album 101 »).
async fn apres_un_album(state: &tune_server::state::AppState) {
    state
        .playback
        .set_session_context(
            ZONE,
            Some("album".into()),
            Some("101".into()),
            Some("local".into()),
            Some("101".into()),
            None,
        )
        .await;
}

async fn poster(app: &axum::Router, uri: &str, corps: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn contexte(state: &tune_server::state::AppState) -> (Option<String>, Option<String>) {
    let etat = state.playback.get_state(ZONE).await;
    (etat.session_context_type, etat.session_context_id)
}

#[tokio::test]
async fn une_radio_jouee_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/radios/9001/play/1", json!({})).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "la station existe");
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "la radio a gardé le contexte de l'album 101 : l'Historique la range dans l'album"
    );
}

#[tokio::test]
async fn un_podcast_joue_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(
        &app,
        "/api/v1/podcasts/play/1",
        json!({"audio_url": "http://127.0.0.1:9/episode.mp3", "title": "Épisode", "podcast_id": "podcast-sans-album", "podcast_name": "Podcast"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        contexte(&state).await,
        (Some("podcast".into()), Some("podcast-sans-album".into())),
        "le podcast a gardé le contexte de l'album 101 : l'Historique le range dans l'album"
    );
    // Ancien client sans identifiant : aucun album ni podcast précédent ne fuit.
    assert_eq!(
        poster(
            &app,
            "/api/v1/podcasts/play/1",
            json!({
                "audio_url": "http://127.0.0.1:9/autre-episode.mp3", "title": "Autre épisode"
            })
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(contexte(&state).await, (Some("podcast".into()), None));
}

#[tokio::test]
async fn un_prereglage_invoque_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/zones/1/pins/0/invoke", json!({})).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "le préréglage existe");
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "le préréglage a gardé le contexte de l'album 101"
    );
}

/// Le refus ne touche à rien : une station inconnue laisse la session telle
/// qu'elle était.
#[tokio::test]
async fn une_station_inconnue_laisse_le_contexte_en_place() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/radios/999/play/1", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        contexte(&state).await,
        (Some("album".into()), Some("101".into()))
    );
}

/// L'album 202 de la bibliothèque, trois pistes.
fn un_album_de_trois_pistes(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO albums (id, title, source) VALUES (202, 'Album 202', 'local');
             INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) VALUES
               (2021, 'piste 1', 202, '/music/202-1.flac', 200000, 'flac'),
               (2022, 'piste 2', 202, '/music/202-2.flac', 200000, 'flac'),
               (2023, 'piste 3', 202, '/music/202-3.flac', 200000, 'flac');",
        )
        .unwrap();
}

/// Fil 2379, point 1 (FabienM, rc7) : « un album en lecture aléatoire → dans
/// l'historique on n'a pas le conteneur ». La lecture aléatoire d'un album
/// (`POST /playback/shuffle-all?album_id=`) pose l'album sur la session, comme
/// « Lire » : sans cela, la session gardait l'album 101 du geste précédent.
///
/// Le banc n'a pas de sortie : la lecture elle-même peut échouer, le contexte
/// est posé avant elle, comme dans `POST /zones/{id}/play`.
#[tokio::test]
async fn un_album_lu_en_aleatoire_est_le_conteneur_de_la_session() {
    let (state, app) = banc().await;
    un_album_de_trois_pistes(&state);
    apres_un_album(&state).await;
    let status = poster(
        &app,
        "/api/v1/playback/shuffle-all?zone_id=1&album_id=202",
        json!({}),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "l'album a des pistes");
    assert_eq!(
        contexte(&state).await,
        (Some("album".into()), Some("202".into())),
        "un album lu en aléatoire n'est pas le conteneur de l'Historique"
    );
}

/// La bibliothèque entière lue en aléatoire n'est pas un conteneur, et
/// n'hérite pas de l'album d'avant.
#[tokio::test]
async fn la_bibliotheque_lue_en_aleatoire_n_herite_pas_de_l_album() {
    let (state, app) = banc().await;
    un_album_de_trois_pistes(&state);
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/playback/shuffle-all?zone_id=1", json!({})).await;
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "la bibliothèque a des pistes"
    );
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "un tirage de la bibliothèque s'est rangé dans l'album 101"
    );
}

// ─── « Lecture » après Stop : la relance garde le conteneur ──────────────────
//
// Fil 2559, point 2 (FabienM) : « l'historique ne garde plus les conteneurs ».
// Après Stop, la barre de transport et le mini-lecteur relancent par
// `{track_id}` (bibliothèque) ou `{source, source_id}` (service). Le serveur
// gardait la file de l'album ou de la playlist (#4298, #2569), mais
// remplaçait le conteneur de la session par « track » : le titre relancé et
// toute la suite s'inscrivaient comme des titres isolés dans l'Historique.
//
// Banc de bout en bout : vraie base, vrais fichiers WAV, routes HTTP ; seule
// la sortie audio est factice (`MockOutput`, modèle `relance_locale_file_4298.rs`).

struct BancDeLecture {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
    _dir: tempfile::TempDir,
}

impl BancDeLecture {
    /// L'album 1 (pistes 1 à 3), une piste 4 hors album, la playlist 55
    /// (pistes 3 puis 1).
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Salon 6465", Some("mock"), Some("mock-6465"))
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                "mock-6465",
                "Sortie de banc",
            )));
        state
            .backend
            .execute_batch(
                "INSERT INTO albums (id, title) VALUES (1, 'Album 6465');
                 INSERT INTO playlists (id, name, profile_id) VALUES (55, 'Liste 6465', 1);",
            )
            .unwrap();
        for id in 1..=4i64 {
            let path = dir.path().join(format!("{id}.wav"));
            let pcm = vec![0u8; 44100 * 4];
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&44100u32.to_le_bytes());
            wav.extend_from_slice(&176400u32.to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
            wav.extend_from_slice(&pcm);
            std::fs::write(&path, wav).unwrap();
            let mut track = tune_core::db::models::Track::new(format!("Titre {id}"));
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.format = Some("wav".into());
            track.duration_ms = 1000;
            track.sample_rate = Some(44100);
            track.bit_depth = Some(16);
            track.album_id = (id <= 3).then_some(1);
            track.track_number = id as i32;
            assert_eq!(
                tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
                    .create(&track)
                    .unwrap(),
                id
            );
        }
        state
            .backend
            .execute_batch(
                "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (55, 3, 0), (55, 1, 1);",
            )
            .unwrap();
        let app = tune_server::routes::router(state.clone());
        Self {
            state,
            app,
            zone,
            _dir: dir,
        }
    }

    async fn play(&self, corps: Value) -> StatusCode {
        poster(
            &self.app,
            &format!("/api/v1/zones/{}/play", self.zone),
            corps,
        )
        .await
    }

    async fn stop(&self) {
        let status = poster(
            &self.app,
            &format!("/api/v1/zones/{}/stop", self.zone),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "Stop refusé");
    }

    async fn contexte(&self) -> (Option<String>, Option<String>) {
        let etat = self.state.playback.get_state(self.zone).await;
        (etat.session_context_type, etat.session_context_id)
    }

    fn file(&self) -> Vec<Option<i64>> {
        tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_ordered(self.zone)
            .unwrap()
            .iter()
            .map(|e| e.track_id)
            .collect()
    }

    /// Les écoutes écrites pour une piste : (nature, identifiant) du
    /// conteneur. Le MOMENT de l'écriture relève de la règle d'écoute validée
    /// (#7852) ; ce banc ne regarde que le conteneur de ce qui est écrit.
    fn ecoutes(&self, titre: &str) -> Vec<(Option<String>, Option<String>)> {
        self.state
            .backend
            .query_many(
                "SELECT context_type, context_id FROM listen_history WHERE title = ? ORDER BY id",
                &[&titre as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap()
            .iter()
            .map(|ligne| (ligne[0].as_string(), ligne[1].as_string()))
            .collect()
    }
}

#[tokio::test]
async fn la_relance_apres_stop_d_un_titre_d_album_garde_l_album() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"album_id": 1})).await, StatusCode::OK);
    assert_eq!(
        banc.contexte().await,
        (Some("album".into()), Some("1".into()))
    );
    banc.stop().await;

    // La barre de transport, zone arrêtée : `{track_id}` seul.
    assert_eq!(banc.play(json!({"track_id": 2})).await, StatusCode::OK);
    assert_eq!(
        banc.file(),
        vec![Some(1), Some(2), Some(3)],
        "file de l'album gardée (#4298)"
    );
    assert_eq!(
        banc.contexte().await,
        (Some("album".into()), Some("1".into())),
        "la relance nue a gardé la file de l'album mais remplacé son conteneur"
    );
    for ecoute in banc.ecoutes("Titre 2") {
        assert_eq!(
            ecoute,
            (Some("album".into()), Some("1".into())),
            "l'écoute du titre relancé s'est inscrite hors de son album"
        );
    }
}

#[tokio::test]
async fn la_relance_apres_stop_d_un_titre_de_playlist_garde_la_playlist() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"playlist_id": 55})).await, StatusCode::OK);
    assert_eq!(
        banc.contexte().await,
        (Some("playlist".into()), Some("55".into()))
    );
    banc.stop().await;

    assert_eq!(banc.play(json!({"track_id": 1})).await, StatusCode::OK);
    assert_eq!(
        banc.file(),
        vec![Some(3), Some(1)],
        "file de la playlist gardée"
    );
    assert_eq!(
        banc.contexte().await,
        (Some("playlist".into()), Some("55".into())),
        "la relance nue a gardé la file de la playlist mais remplacé son conteneur"
    );
    for ecoute in banc.ecoutes("Titre 1") {
        assert_eq!(
            ecoute,
            (Some("playlist".into()), Some("55".into())),
            "l'écoute du titre relancé s'est inscrite hors de sa playlist"
        );
    }
}

/// Garde-fou : un titre ABSENT de la file est un nouveau geste. Il remplace
/// la file et devient « track », comme avant.
#[tokio::test]
async fn un_titre_absent_de_la_file_devient_un_titre_isole() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"album_id": 1})).await, StatusCode::OK);
    banc.stop().await;

    assert_eq!(banc.play(json!({"track_id": 4})).await, StatusCode::OK);
    assert_eq!(banc.file(), vec![Some(4)]);
    assert_eq!(
        banc.contexte().await,
        (Some("track".into()), Some("4".into())),
        "un titre hors de la file a hérité de l'album d'avant"
    );
}

/// Garde-fou : « Lire » l'album depuis sa fiche avec un départ choisi reste
/// un nouveau geste, même si la piste est déjà dans la file.
#[tokio::test]
async fn un_contenant_designe_reste_un_nouveau_geste() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"playlist_id": 55})).await, StatusCode::OK);
    banc.stop().await;

    assert_eq!(
        banc.play(json!({"album_id": 1, "start_index": 0})).await,
        StatusCode::OK
    );
    assert_eq!(
        banc.contexte().await,
        (Some("album".into()), Some("1".into()))
    );
}

/// Titre de service déjà dans la file (Stop puis Lecture sur une playlist
/// Qobuz) : `{source, source_id}` garde la file (#2569) et doit garder la
/// playlist. Les lignes de file sont écrites directement : le banc n'a pas
/// de service connecté, la lecture elle-même peut échouer après la décision.
#[tokio::test]
async fn la_relance_d_un_titre_de_service_deja_en_file_garde_la_playlist() {
    let banc = BancDeLecture::new().await;
    let ligne = |id: &str| tune_core::db::play_queue_repo::QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: id.into(),
        title: format!("Service {id}"),
        artist: "Artiste".into(),
        album: None,
        cover_url: None,
        duration_ms: 1000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    };
    tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(banc.state.backend.clone())
        .append(banc.zone, &[ligne("q-1"), ligne("q-2")])
        .unwrap();
    banc.state
        .playback
        .set_session_context(
            banc.zone,
            Some("playlist".into()),
            Some("pl-qobuz".into()),
            Some("qobuz".into()),
            Some("Liste Qobuz".into()),
            None,
        )
        .await;

    let _ = banc
        .play(json!({"source": "qobuz", "source_id": "q-2"}))
        .await;
    assert_eq!(
        banc.contexte().await,
        (Some("playlist".into()), Some("pl-qobuz".into())),
        "la relance d'un titre de service déjà en file a remplacé le conteneur de la playlist"
    );
}

// ─── Le conteneur posé sur les lignes de file de « Lire » ────────────────────
//
// Fil 2302 point 7, fil 2559 point 2 (FabienM) : « le menu historique
// n'historise plus la lecture des conteneurs, uniquement des titres ». Les
// lignes posées par `POST /zones/{id}/play` restaient à `context_type` NULL et
// suivaient le contexte de SESSION, effacé par une radio, un podcast, une
// radio d'artiste, un préréglage (#6558) ou un redémarrage (#7889). Une radio
// écoutée entre deux, puis un clic sur un titre de l'album resté en file : ce
// titre et la suite s'inscrivaient au premier niveau. L'écoute lit d'abord le
// conteneur de la ligne jouée (`contexte_de_la_piste_jouee`, celui
// qu'`ecrire_l_ecoute` écrit) : il doit être là, sans la session.

impl BancDeLecture {
    /// Le conteneur que l'écoute de chaque ligne de file écrirait, la session
    /// de la zone EFFACÉE (ce que font une radio, un podcast, un redémarrage).
    async fn conteneurs_sans_session(&self) -> Vec<Option<(String, Option<String>)>> {
        self.state
            .playback
            .effacer_contexte_de_session(self.zone)
            .await;
        tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_ordered(self.zone)
            .unwrap()
            .iter()
            .map(|e| {
                tune_core::db::contexte_de_file::contexte_de_la_piste_jouee(
                    &self.state.backend,
                    self.zone,
                    e.position,
                    e.track_id,
                    e.source_id.as_deref(),
                )
                .map(|c| (c.nature, c.id))
            })
            .collect()
    }
}

#[tokio::test]
async fn les_titres_d_un_album_lu_gardent_l_album_sans_la_session() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"album_id": 1})).await, StatusCode::OK);
    let album = Some(("album".to_string(), Some("1".to_string())));
    assert_eq!(
        banc.conteneurs_sans_session().await,
        vec![album.clone(), album.clone(), album],
        "après une radio ou un redémarrage, les titres de l'album en file \
         s'inscrivaient au premier niveau de l'Historique"
    );
}

#[tokio::test]
async fn les_titres_d_une_playlist_lue_gardent_la_playlist_sans_la_session() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"playlist_id": 55})).await, StatusCode::OK);
    assert_eq!(banc.file(), vec![Some(3), Some(1)]);
    let liste = Some(("playlist".to_string(), Some("55".to_string())));
    assert_eq!(
        banc.conteneurs_sans_session().await,
        vec![liste.clone(), liste],
        "un titre lu depuis la playlist s'inscrivait au premier niveau (fil 2559, point 2)"
    );
}

/// Garde-fou : un titre lancé SEUL n'a pas de conteneur à poser ; sa ligne
/// suit la session (« track »), comme avant.
#[tokio::test]
async fn un_titre_lance_seul_ne_pose_aucun_conteneur_sur_la_file() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"track_id": 4})).await, StatusCode::OK);
    assert_eq!(banc.conteneurs_sans_session().await, vec![None]);
}

/// Garde-fou : « Lire » une playlist après un album ne réécrit pas les lignes
/// d'avant — seules celles du geste prennent son conteneur.
#[tokio::test]
async fn un_nouveau_lire_ne_repeint_que_ses_propres_lignes() {
    let banc = BancDeLecture::new().await;
    assert_eq!(banc.play(json!({"album_id": 1})).await, StatusCode::OK);
    let avant =
        tune_core::db::contexte_de_file::dernier_identifiant(&banc.state.backend, banc.zone);
    assert_eq!(banc.play(json!({"playlist_id": 55})).await, StatusCode::OK);
    let lignes = banc
        .state
        .backend
        .query_many(
            "SELECT id, context_type FROM queue_items WHERE zone_id = ? ORDER BY id",
            &[&banc.zone as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap();
    assert!(!lignes.is_empty());
    for l in lignes {
        let id = l[0].as_i64().unwrap();
        assert!(id > avant, "la file de l'album n'a pas été remplacée");
        assert_eq!(l[1].as_string().as_deref(), Some("playlist"));
    }
}
