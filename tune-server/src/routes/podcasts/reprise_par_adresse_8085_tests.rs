//! #8085 — un épisode relancé depuis la carte « Récemment écoutés » de
//! l'accueil (ou l'Historique) : le client web envoie
//! `POST /zones/{id}/play {source: "podcast", source_id: <URL du média>}`
//! (`accueilWidgets.ts`, `gesteHistorique`). Constat du .18 le 07/11 : la
//! barre affichait « Episode », sans pochette, et l'épisode repartait à 0.
//!
//! Aucun son : sortie factice (`MockOutput`, zone DLNA comme sur le .18),
//! épisode sur le port 9 local (discard).
use crate::state::AppState;
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::history_repo::{HistoryRepo, ListenRecord};
use tune_core::db::listening_resume as reprises;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;

const APPAREIL: &str = "dlna:uuid-reprise-8085";
const EPISODE: &str = "http://127.0.0.1:9/episode-8085.m4a";
const TITRE: &str = "Rencontre de témoin 8085";
const PODCAST: &str = "Émission témoin";
const POCHETTE: &str = "/artwork/pochette-8085.jpg";
const DUREE_MS: i64 = 1_800_000;

async fn banc() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin 8085", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    (state, zone)
}

fn episode() -> NowPlaying {
    NowPlaying {
        title: TITRE.into(),
        artist_name: Some(PODCAST.into()),
        album_title: Some(PODCAST.into()),
        cover_path: Some(POCHETTE.into()),
        duration_ms: DUREE_MS,
        source: "podcast".into(),
        source_id: Some(EPISODE.into()),
        ..Default::default()
    }
}

/// Le geste de la carte de l'accueil : la source et l'adresse, rien d'autre.
async fn relancer_depuis_l_accueil(state: &AppState, zone: i64) -> Value {
    let rep = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/zones/{zone}/play"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "source": "podcast", "source_id": EPISODE }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert!(statut.is_success(), "{statut} {v}");
    v
}

/// `(durée annoncée au dernier play_media, cibles des seek reçus)`.
async fn recu_par_la_sortie(state: &AppState) -> (Option<u64>, Vec<u64>) {
    let arc = { state.outputs.lock().await.get(APPAREIL) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.last_play_duration_ms().await, mock.seek_calls())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8085_depuis_l_accueil_titre_podcast_pochette_et_position_moins_le_recul() {
    let (state, zone) = banc().await;
    reprises::podcast_started_in_context(
        &state.backend,
        zone,
        &episode(),
        320_000,
        Some("emission-8085"),
    )
    .unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(reprises::REWIND_KEY, "15")
        .unwrap();

    let r = relancer_depuis_l_accueil(&state, zone).await;

    assert_eq!(r["current_track"]["title"], TITRE, "{r}");
    assert_eq!(r["current_track"]["artist_name"], PODCAST, "{r}");
    assert_eq!(r["current_track"]["cover_path"], POCHETTE, "{r}");
    // #8152 — sur un renderer réseau, le Seek de reprise part d'une tâche
    // détachée, une fois le transport en lecture.
    for _ in 0..40 {
        if !recu_par_la_sortie(&state).await.1.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (duree, seeks) = recu_par_la_sortie(&state).await;
    assert_eq!(duree, Some(DUREE_MS as u64), "le renderer reçoit l'épisode");
    assert_eq!(
        seeks,
        vec![305_000],
        "reprise à 320 s moins le recul réglé (15 s)"
    );
    let np = state.playback.get_state(zone).await.now_playing.unwrap();
    assert_eq!(np.source, "podcast");
    assert_eq!(np.source_id.as_deref(), Some(EPISODE));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8085_sans_reprise_enregistree_l_historique_donne_titre_et_podcast() {
    let (state, zone) = banc().await;
    HistoryRepo::with_backend(state.backend.clone())
        .record(&ListenRecord {
            title: TITRE.into(),
            artist_name: Some(PODCAST.into()),
            album_title: Some(PODCAST.into()),
            source: "podcast".into(),
            source_id: Some(EPISODE.into()),
            duration_ms: DUREE_MS,
            zone_id: Some(zone),
            cover_url: Some(POCHETTE.into()),
            context_type: Some("podcast".into()),
            context_id: Some("emission-8085".into()),
            ..Default::default()
        })
        .unwrap();

    let r = relancer_depuis_l_accueil(&state, zone).await;

    assert_eq!(r["current_track"]["title"], TITRE, "{r}");
    assert_eq!(r["current_track"]["artist_name"], PODCAST, "{r}");
    let (duree, seeks) = recu_par_la_sortie(&state).await;
    assert_eq!(duree, Some(DUREE_MS as u64));
    assert!(seeks.is_empty(), "rien à reprendre : du début, {seeks:?}");
}

#[test]
fn reprise_8085_la_reprise_de_la_zone_prime_puis_la_plus_recente_ailleurs() {
    let state = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { AppState::new(":memory:", 0, Default::default()).unwrap() });
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let a = zones.create("A — 8085", None, None).unwrap();
    let b = zones.create("B — 8085", None, None).unwrap();
    let c = zones.create("C — 8085", None, None).unwrap();
    reprises::podcast_started_in_context(&state.backend, a, &episode(), 100_000, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    reprises::podcast_started_in_context(&state.backend, b, &episode(), 200_000, None).unwrap();
    let position = |zone| {
        reprises::podcast_de_l_adresse(&state.backend, zone, EPISODE)
            .unwrap()
            .map(|r| r.position_ms)
    };
    assert_eq!(position(a), Some(100_000), "celle de la zone d'abord");
    assert_eq!(position(b), Some(200_000));
    assert_eq!(position(c), Some(200_000), "sinon la plus récente");
    assert_eq!(
        reprises::podcast_de_l_adresse(&state.backend, a, "http://127.0.0.1:9/autre.mp3")
            .unwrap()
            .map(|r| r.position_ms),
        None
    );
}

/// Une reprise de l'épisode écrite sous un contexte donné, comme les quatre
/// lignes du .18 : `(context_type, context_id)` varie, l'épisode non.
fn reprise_sous(
    context_type: &str,
    context_id: &str,
    position_ms: i64,
    quand: &str,
) -> reprises::Resume {
    reprises::Resume {
        resume_key: format!("cle-{context_type}-{context_id}"),
        context_type: context_type.into(),
        context_id: context_id.into(),
        source: "podcast".into(),
        title: PODCAST.into(),
        last_track_title: TITRE.into(),
        cover_path: Some(POCHETTE.into()),
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: Vec::new(),
        position_ms,
        updated_at: quand.into(),
        items: vec![tune_core::db::play_queue_repo::QueueInput::Streaming {
            source: "podcast".into(),
            source_id: EPISODE.into(),
            title: TITRE.into(),
            artist: PODCAST.into(),
            album: Some(PODCAST.into()),
            cover_url: Some(POCHETTE.into()),
            duration_ms: DUREE_MS,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        }],
    }
}

async fn reprendre_l_ecoute(state: &AppState) -> Vec<Value> {
    let rep = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/home/continue-listening?limit=50")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap();
    v.as_array()
        .cloned()
        .or_else(|| v["items"].as_array().cloned())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8085_quatre_reprises_du_meme_episode_font_une_seule_carte() {
    let (state, zone) = banc().await;
    // Les quatre clés mesurées sur le .18 le 07/11 : fil d'abonnement,
    // identifiant resté en session, adresse du média, ligne « track ».
    for r in [
        reprise_sous(
            "podcast",
            "https://exemple.invalid/flux.xml",
            113_000,
            "2026-11-07T14:01:00+00:00",
        ),
        reprise_sous("podcast", "90758043", 114_000, "2026-11-07T14:02:00+00:00"),
        reprise_sous("track", EPISODE, 700_000, "2026-11-07T14:03:00+00:00"),
        reprise_sous("podcast", EPISODE, 764_000, "2026-11-07T14:04:00+00:00"),
    ] {
        reprises::save(&state.backend, zone, &r).unwrap();
    }
    let cartes = reprendre_l_ecoute(&state).await;
    let episodes: Vec<_> = cartes
        .iter()
        .filter(|c| c["last_track_title"] == TITRE)
        .collect();
    assert_eq!(
        episodes.len(),
        1,
        "une seule carte par épisode : {cartes:?}"
    );
    assert_eq!(episodes[0]["position_ms"], 764_000, "la plus récente gagne");
    let lignes = state
        .backend
        .query_one("SELECT COUNT(*) FROM listening_resumes", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    assert_eq!(lignes, 4, "dédoublonnage à la lecture : rien n'est effacé");
}

#[tokio::test]
async fn reprise_8085_un_contexte_de_session_different_garde_la_meme_reprise() {
    let (state, zone) = banc().await;
    for (contexte, position) in [
        (Some("https://exemple.invalid/flux.xml"), 100_000),
        (Some("90758043"), 200_000),
        (Some(EPISODE), 300_000),
        (None, 400_000),
    ] {
        reprises::podcast_started_in_context(&state.backend, zone, &episode(), position, contexte)
            .unwrap();
    }
    let lignes = state
        .backend
        .query_one("SELECT COUNT(*) FROM listening_resumes", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    assert_eq!(lignes, 1, "une seule reprise pour l'épisode");
    let r = reprises::podcast_de_l_adresse(&state.backend, zone, EPISODE)
        .unwrap()
        .unwrap();
    assert_eq!(r.position_ms, 400_000);
}

/// Une reprise de service quelconque (album, playlist, piste seule).
fn reprise_de_service(
    context_type: &str,
    context_id: &str,
    source: &str,
    titre_contexte: &str,
    piste: (&str, &str, &str, &str),
    quand: &str,
) -> reprises::Resume {
    let (source_id, titre, artiste, album) = piste;
    reprises::Resume {
        resume_key: format!("cle-{source}-{context_id}"),
        context_type: context_type.into(),
        context_id: context_id.into(),
        source: source.into(),
        title: titre_contexte.into(),
        last_track_title: titre.into(),
        cover_path: None,
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: Vec::new(),
        position_ms: 60_000,
        updated_at: quand.into(),
        items: vec![tune_core::db::play_queue_repo::QueueInput::Streaming {
            source: source.into(),
            source_id: source_id.into(),
            title: titre.into(),
            artist: artiste.into(),
            album: Some(album.into()),
            cover_url: None,
            duration_ms: 300_000,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8085_la_lecture_en_cours_ne_figure_pas_dans_reprendre_l_ecoute() {
    let (state, zone) = banc().await;
    reprises::save(
        &state.backend,
        zone,
        &reprise_de_service(
            "playlist",
            "mix:temoin-8085",
            "tidal",
            "Mix témoin",
            (
                "356421418",
                "Morceau du mix",
                "Interprète du mix",
                "Album du mix",
            ),
            "2026-11-07T15:00:00+00:00",
        ),
    )
    .unwrap();
    reprises::save(
        &state.backend,
        zone,
        &reprise_de_service(
            "album",
            "alb-8085",
            "qobuz",
            "Album témoin",
            (
                "111",
                "Piste de l'album",
                "Interprète témoin",
                "Album témoin",
            ),
            "2026-11-07T14:00:00+00:00",
        ),
    )
    .unwrap();
    let titres = |cartes: &[Value]| {
        cartes
            .iter()
            .map(|c| c["title"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
    };
    // Contre-épreuve dans le témoin : zone arrêtée, le mix est une reprise.
    let avant = reprendre_l_ecoute(&state).await;
    assert!(
        titres(&avant).contains(&"Mix témoin".to_string()),
        "{avant:?}"
    );
    // Le mix JOUE : il ne figure plus parmi les reprises, l'album si.
    state
        .playback
        .set_session_context(
            zone,
            Some("playlist".into()),
            Some("mix:temoin-8085".into()),
            Some("tidal".into()),
            Some("Mix témoin".into()),
            None,
        )
        .await;
    state
        .playback
        .play(
            zone,
            NowPlaying {
                title: "Morceau du mix".into(),
                source: "tidal".into(),
                source_id: Some("356421418".into()),
                ..Default::default()
            },
        )
        .await;
    let pendant = reprendre_l_ecoute(&state).await;
    assert!(
        !titres(&pendant).contains(&"Mix témoin".to_string()),
        "{pendant:?}"
    );
    assert!(
        titres(&pendant).contains(&"Album témoin".to_string()),
        "{pendant:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8085_chaque_carte_porte_l_interprete_et_l_album_de_sa_piste() {
    let (state, zone) = banc().await;
    reprises::save(
        &state.backend,
        zone,
        &reprise_de_service(
            "track",
            "32597028",
            "qobuz",
            "Titre seul",
            (
                "32597028",
                "Titre seul",
                "Interprète seul",
                "Album du titre",
            ),
            "2026-11-07T15:00:00+00:00",
        ),
    )
    .unwrap();
    let cartes = reprendre_l_ecoute(&state).await;
    let c = cartes
        .iter()
        .find(|c| c["title"] == "Titre seul")
        .expect("la carte");
    assert_eq!(c["artist_name"], "Interprète seul", "{c}");
    assert_eq!(c["album_title"], "Album du titre", "{c}");
}
