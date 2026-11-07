//! #7965 — témoins : une radio lancée écrit UNE ligne au nom de la station
//! (logo, `context_type = 'radio'`), qu'aucun changement de titre ICY ne
//! multiplie ; un épisode de podcast écrit sa ligne `podcast` au seuil ; la
//! liste de l'historique les montre ; les statistiques excluent la radio.
//!
//! Aucun son : sortie factice (`MockOutput`), station sur le port 9 local
//! (discard), aucun appel réseau ne sort du banc.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::history_repo::HistoryRepo;
use crate::db::migrations::run_migrations;
use crate::db::radio_repo::{RadioRepo, RadioStation};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;

const RENDERER: &str = "dlna:uuid-historique-7965";
const FLUX: &str = "http://127.0.0.1:9/fip-7965.mp3";
const LOGO: &str = "https://example.org/logos/fip.png";

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

async fn zone_dlna(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(RENDERER))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(RENDERER, "Salon").with_type("dlna"),
    ));
    zone_id
}

/// La requête que `jouer_un_flux` (route des radios) envoie.
fn lancer_la_station(zone_id: i64, titre: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(RENDERER.into()),
        source: Some("radio".into()),
        source_id: Some(FLUX.into()),
        title: Some(titre.into()),
        artist_name: Some("Live Radio".into()),
        album_title: Some(titre.into()),
        cover_url: Some(LOGO.into()),
        duration_ms: None,
        ..Default::default()
    }
}

fn lignes(orch: &PlaybackOrchestrator) -> Vec<crate::db::history_repo::ListenRecord> {
    HistoryRepo::with_backend(orch.db.clone())
        .recent_paginated(50, 0)
        .unwrap()
        .0
}

fn lignes_en_base(orch: &PlaybackOrchestrator, source: &str) -> i64 {
    use crate::db::backend::ToSqlValue;
    orch.db
        .query_one(
            "SELECT COUNT(*) FROM listen_history WHERE source = ?",
            &[&source as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Le sondeur, à un tour où le flux ICY annonce un nouveau titre : il
/// remplace `now_playing` (`poller::radio::refresh_radio_metadata`), puis
/// consulte le point de décision de l'historique (`valider_l_ecoute`).
async fn titre_icy(orch: &PlaybackOrchestrator, zone_id: i64, titre: &str) {
    let etat = orch.playback.get_state(zone_id).await;
    let mut np: NowPlaying = etat.now_playing.clone().expect("la radio joue");
    np.title = titre.into();
    np.artist_name = Some("Artiste ICY".into());
    orch.playback.update_now_playing(zone_id, np).await;
    let etat = orch.playback.get_state(zone_id).await;
    orch.valider_l_ecoute(&etat);
}

#[tokio::test]
async fn une_radio_lancee_ecrit_une_ligne_au_nom_de_la_station_7965() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch).await;

    let resultat = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        orch.play(lancer_la_station(zone_id, "FIP")),
    )
    .await
    .expect("le lancement de la radio ne doit pas rester bloqué")
    .expect("le lancement de la radio aboutit");
    assert!(resultat.output_sent, "la sortie factice accepte le flux");

    titre_icy(&orch, zone_id, "So What").await;
    titre_icy(&orch, zone_id, "Blue in Green").await;

    let liste = lignes(&orch);
    let radios: Vec<_> = liste.iter().filter(|l| l.source == "radio").collect();
    assert_eq!(
        radios.len(),
        1,
        "un lancement de radio doit écrire UNE ligne d'historique, ni zéro ni une par titre ICY : {liste:?}"
    );
    let ligne = radios[0];
    assert_eq!(ligne.title, "FIP", "la ligne porte le nom de la station");
    assert_eq!(ligne.cover_url.as_deref(), Some(LOGO), "et son logo");
    assert_eq!(ligne.context_type.as_deref(), Some("radio"));
    assert_eq!(ligne.source_id.as_deref(), Some(FLUX));
    assert_eq!(
        lignes_en_base(&orch, "radio"),
        1,
        "aucune ligne cachée de plus en base"
    );
}

/// Une station de « Mes radios » : son nom et son logo enregistrés priment
/// sur ce que la requête porte (un titre ICY recopié d'une ancienne ligne).
#[tokio::test]
async fn la_station_enregistree_donne_son_nom_et_son_logo_7965() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch).await;
    RadioRepo::with_backend(orch.db.clone())
        .create(&RadioStation {
            id: None,
            name: "TSF Jazz".into(),
            url: FLUX.into(),
            homepage: None,
            logo_url: Some(LOGO.into()),
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    let mut req = lancer_la_station(zone_id, "Take Five - Dave Brubeck");
    req.album_title = None;
    req.cover_url = None;

    tokio::time::timeout(std::time::Duration::from_secs(30), orch.play(req))
        .await
        .expect("pas de blocage")
        .expect("lancement");

    let liste = lignes(&orch);
    assert_eq!(liste.len(), 1, "{liste:?}");
    assert_eq!(liste[0].title, "TSF Jazz");
    assert_eq!(liste[0].cover_url.as_deref(), Some(LOGO));
}

/// Les anciennes lignes de radio (avant la v0.8.258, titre ICY figé, sans
/// contexte) restent hors de la liste et de son total.
#[test]
fn les_anciennes_lignes_de_radio_restent_cachees_7965() {
    let orch = orchestrateur();
    orch.db
        .execute(
            "INSERT INTO listen_history (title, artist_name, source) \
             VALUES ('Titre ICY figé', 'Artiste', 'radio')",
            &[],
        )
        .unwrap();
    let (liste, total) = HistoryRepo::with_backend(orch.db.clone())
        .recent_paginated(50, 0)
        .unwrap();
    assert!(liste.is_empty(), "{liste:?}");
    assert_eq!(total, 0);
}

#[tokio::test]
async fn un_episode_de_podcast_ecrit_sa_ligne_au_seuil_7965() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch).await;
    let audio = "https://example.org/episodes/42.mp3";
    // Ce que `play_episode` pose sur la zone avant d'appeler l'entrée.
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Épisode 42".into(),
                artist_name: Some("Les Pieds sur terre".into()),
                album_title: Some("Les Pieds sur terre".into()),
                source: "podcast".into(),
                source_id: Some(audio.into()),
                duration_ms: 1_800_000,
                ..Default::default()
            },
        )
        .await;

    orch.enregistrer_ecoute_podcast(
        zone_id,
        "Épisode 42",
        "Les Pieds sur terre",
        Some("https://example.org/pochette.jpg"),
        audio,
        1_800_000,
        Some("17"),
    )
    .await;
    assert!(
        lignes(&orch).is_empty(),
        "#7852 : rien n'est écrit avant le seuil"
    );
    orch.playback.update_position(zone_id, 30_000).await;
    assert!(orch.valider_l_ecoute(&orch.playback.get_state(zone_id).await));

    let liste = lignes(&orch);
    assert_eq!(liste.len(), 1, "{liste:?}");
    let l = &liste[0];
    assert_eq!(l.source, "podcast");
    assert_eq!(l.title, "Épisode 42");
    assert_eq!(l.artist_name.as_deref(), Some("Les Pieds sur terre"));
    assert_eq!(l.album_title.as_deref(), Some("Les Pieds sur terre"));
    assert_eq!(
        l.cover_url.as_deref(),
        Some("https://example.org/pochette.jpg")
    );
    assert_eq!(l.context_type.as_deref(), Some("podcast"));
    assert_eq!(l.context_id.as_deref(), Some("17"));
}

/// Les statistiques d'écoute excluent toujours la radio, même maintenant
/// qu'elle est dans l'historique.
#[test]
fn les_statistiques_excluent_toujours_la_radio_7965() {
    let orch = orchestrateur();
    for (titre, source, contexte) in [
        ("FIP", "radio", Some("radio")),
        ("FIP", "radio", Some("radio")),
        ("Kind of Blue", "local", None),
    ] {
        orch.db
            .execute(
                "INSERT INTO listen_history (title, artist_name, album_title, source, \
                 duration_ms, context_type) VALUES (?, ?, ?, ?, 60000, ?)",
                &[
                    &titre as &dyn crate::db::backend::ToSqlValue,
                    &Some(if source == "radio" {
                        "Live Radio"
                    } else {
                        "Miles Davis"
                    }),
                    &Some(titre),
                    &source,
                    &contexte,
                ],
            )
            .unwrap();
    }
    let repo = HistoryRepo::with_backend(orch.db.clone());
    assert_eq!(
        repo.recent_paginated(50, 0).unwrap().1,
        3,
        "la liste, elle, montre les radios"
    );

    let d = repo.full_dashboard("all", None, None, 10).unwrap();
    let json = serde_json::to_value(&d).unwrap();
    let texte = json.to_string();
    assert!(
        !texte.contains("Live Radio") && !texte.contains("\"radio\""),
        "aucune statistique ne doit compter la radio : {texte}"
    );
    assert_eq!(json["totals"]["plays"], 1, "{texte}");
    assert_eq!(json["totals"]["listening_ms"], 60000, "{texte}");
}

/// La série de jours d'écoute ne compte pas un jour où l'on n'a écouté que la
/// radio : deux jours de radio seule avant un jour de musique laissent une
/// série de un jour, pas de trois.
#[test]
fn la_serie_de_jours_ignore_les_jours_de_radio_seule_7965() {
    let orch = orchestrateur();
    let jour = |n: i64| {
        (chrono::Utc::now() - chrono::Duration::days(n))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string()
    };
    for (titre, source, contexte, quand) in [
        ("FIP", "radio", Some("radio"), jour(2)),
        ("FIP", "radio", Some("radio"), jour(1)),
        ("So What", "local", None, jour(0)),
    ] {
        orch.db
            .execute(
                "INSERT INTO listen_history (title, source, duration_ms, context_type, \
                 listened_at) VALUES (?, ?, 60000, ?, ?)",
                &[
                    &titre as &dyn crate::db::backend::ToSqlValue,
                    &source,
                    &contexte,
                    &quand,
                ],
            )
            .unwrap();
    }
    let d = HistoryRepo::with_backend(orch.db.clone())
        .full_dashboard("all", None, None, 10)
        .unwrap();
    let serie = d.streak.expect("un jour de musique fait une série");
    assert_eq!(
        (serie.best, serie.current),
        (1, 1),
        "les jours de radio seule ne comptent pas dans la série"
    );
}
