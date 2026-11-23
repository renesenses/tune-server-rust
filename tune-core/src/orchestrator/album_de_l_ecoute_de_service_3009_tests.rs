//! Fil 3009, point 15 (FabienM, v1.0.0-rc13) — « Menu accueil : Récemment
//! écoutés : il manque les boutons cœur / actions / étiquettes sur les
//! vignettes des titres Qobuz non favoris ».
//!
//! La vignette d'une écoute de service n'a ses gestes que si `GET
//! /library/history` sert l'album CHEZ LE SERVICE (`album_id_service`, la
//! colonne `listen_history.album_ref`). Mesuré sur le .18 le 23/11/2026 : 74
//! des 123 dernières écoutes Qobuz n'en portaient pas. L'écoute n'écrivait la
//! référence que si la DEMANDE de lecture la portait : `POST /zones/{id}/play`
//! ne la passe que pour Bandcamp, l'avance sans blanc et la zone navigateur
//! n'en passent aucune — alors que la ligne de file l'a (#6079).
//!
//! CONTRE-ÉPREUVE (mesurée) : sans le repli de `ecrire_l_ecoute` sur
//! `reference_d_album_de_service`, `l_ecoute_d_un_titre_qobuz_reprend_l_album_de_sa_ligne_de_file_3009`
//! rougit (`album_ref` NULL).
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{ContexteEcoute, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;

const SALON: i64 = 30091;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    db.execute(
        "INSERT INTO zones (id, name) VALUES (?, ?)",
        &[&SALON as &dyn ToSqlValue, &"Salon"],
    )
    .unwrap();
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

fn piste(source: &str, source_id: &str) -> NowPlaying {
    NowPlaying {
        title: "Sugar Man".into(),
        artist_name: Some("Rodriguez".into()),
        album_title: Some("Cold Fact".into()),
        duration_ms: 230_000,
        source: source.into(),
        source_id: Some(source_id.into()),
        ..Default::default()
    }
}

/// La ligne de file telle que `POST /zones/{id}/play` l'enfile pour un titre
/// seul (#6079) : avec l'album du service.
fn en_file(orch: &PlaybackOrchestrator, np: &NowPlaying, album_ref: Option<&str>) {
    PlayQueueRepo::with_backend(orch.db.clone())
        .append(
            SALON,
            &[QueueInput::Streaming {
                media_format: None,
                sample_rate: None,
                bit_depth: None,
                source: np.source.clone(),
                source_id: np.source_id.clone().unwrap(),
                title: np.title.clone(),
                artist: "Rodriguez".into(),
                album: np.album_title.clone(),
                cover_url: None,
                duration_ms: np.duration_ms,
                track_number: None,
                disc_number: None,
                album_ref: album_ref.map(Into::into),
                artist_ref: None,
            }],
        )
        .unwrap();
}

/// Un lanceur qui ne connaît PAS la référence (`album_ref: None`), comme
/// `play_inner` pour un titre Qobuz seul, l'avance sans blanc ou la zone
/// navigateur ; puis la zone atteint le seuil d'écoute (#7852).
async fn ecouter(orch: &PlaybackOrchestrator, np: &NowPlaying) {
    orch.playback.play(SALON, np.clone()).await;
    orch.record_listen(
        &np.title,
        np.artist_name.as_deref(),
        np.album_title.as_deref(),
        &np.source,
        np.source_id.as_deref(),
        None,
        np.duration_ms,
        SALON,
        None,
        None,
        ContexteEcoute {
            nature: Some("track"),
            id: None,
            rang: Some(0),
            service: Some(np.source.as_str()),
            titre: None,
            pochette: None,
        },
        None,
        None,
    );
    assert!(
        orch.amener_au_seuil(SALON).await,
        "l'écoute doit être validée"
    );
}

fn album_ref_ecrit(orch: &PlaybackOrchestrator) -> Option<String> {
    orch.db
        .query_one(
            "SELECT album_ref FROM listen_history ORDER BY id DESC LIMIT 1",
            &[],
        )
        .unwrap()
        .and_then(|cols| cols.first().and_then(|v| v.as_string()))
}

#[tokio::test]
async fn l_ecoute_d_un_titre_qobuz_reprend_l_album_de_sa_ligne_de_file_3009() {
    let orch = orchestrateur();
    let np = piste("qobuz", "59983347");
    en_file(&orch, &np, Some("0060253753283"));
    ecouter(&orch, &np).await;
    assert_eq!(
        album_ref_ecrit(&orch).as_deref(),
        Some("0060253753283"),
        "sans l'album du service, « Récemment écoutés » n'offre ni cœur, ni « … », ni étiquettes"
    );
}

#[tokio::test]
async fn sans_album_connu_rien_n_est_devine_3009() {
    let orch = orchestrateur();
    let np = piste("qobuz", "59983347");
    en_file(&orch, &np, None);
    // Une AUTRE piste du même service a son album : il ne déteint pas.
    en_file(&orch, &piste("qobuz", "82542200"), Some("autre-album"));
    // Le même identifiant chez un AUTRE service non plus.
    en_file(&orch, &piste("tidal", "59983347"), Some("album-tidal"));
    ecouter(&orch, &np).await;
    assert_eq!(album_ref_ecrit(&orch), None);
}
