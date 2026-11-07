//! #7974 — témoins du bail d'écoute dans l'orchestrateur : chaque entrée de
//! lecture, sur chaque type de sortie, refuse hors plage et budget épuisé ;
//! sans politique rien ne change ; le décompte exclut la pause ; la fin de
//! bail est annoncée avant l'arrêt.
//!
//! Sorties factices (`MockOutput`), aucun son.
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Datelike;
use tokio::sync::Mutex;

use super::bail_parental_7974::{DELAI_D_ANNONCE, GesteDeFinDeBail, geste_de_fin_de_bail};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::bail_d_ecoute::{self, SENTINELLE};
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::politique_parentale::{PlageHoraire, PolitiqueParentale, poser};
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlayState, PlaybackManager, ZoneState};
use crate::streaming::ServiceRegistry;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
/// Les types de sortie dont le choix vient APRÈS la garde.
const SORTIES: &[&str] = &["local", "dlna", "airplay", "chromecast", "openhome"];
const PREMIERE_ZONE: i64 = 79741;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let mut registre = OutputRegistry::new();
    for (i, sortie) in SORTIES.iter().enumerate() {
        let zone = PREMIERE_ZONE + i as i64;
        let appareil = format!("appareil-7974-{sortie}");
        db.execute(
            "INSERT INTO zones (id, name, output_type, output_device_id) VALUES (?, ?, ?, ?)",
            &[
                &zone as &dyn ToSqlValue,
                &format!("Zone {sortie}"),
                sortie,
                &appareil,
            ],
        )
        .unwrap();
        registre.register(Box::new(
            MockOutput::new(&appareil, "Sortie factice").with_type(sortie),
        ));
    }
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(registre)),
        None,
    )
}

/// Une plage qui ne couvre jamais aujourd'hui : demain, toute la journée.
fn hors_plage() -> PolitiqueParentale {
    let aujourd_hui = chrono::Local::now().weekday().number_from_monday() as u8;
    PolitiqueParentale {
        plages_horaires: vec![PlageHoraire {
            jours: vec![aujourd_hui % 7 + 1],
            debut: "00:00".into(),
            fin: "23:59".into(),
        }],
        ..Default::default()
    }
}

fn budget(minutes: u32) -> PolitiqueParentale {
    PolitiqueParentale {
        budget_quotidien_minutes: Some(minutes),
        ..Default::default()
    }
}

/// Les demandes de lecture des chemins : bibliothèque, service de
/// streaming, radio, podcast, serveur média UPnP.
fn demandes(zone_id: i64) -> Vec<(&'static str, PlayRequest)> {
    let base = PlayRequest {
        zone_id,
        title: Some("Titre".into()),
        ..Default::default()
    };
    vec![
        (
            "bibliotheque",
            PlayRequest {
                track_id: Some(1),
                ..base.clone()
            },
        ),
        (
            "qobuz",
            PlayRequest {
                source: Some("qobuz".into()),
                source_id: Some("123".into()),
                ..base.clone()
            },
        ),
        (
            "tidal",
            PlayRequest {
                source: Some("tidal".into()),
                source_id: Some("456".into()),
                ..base.clone()
            },
        ),
        (
            "spotify",
            PlayRequest {
                source: Some("spotify".into()),
                source_id: Some("abc".into()),
                ..base.clone()
            },
        ),
        (
            "radio",
            PlayRequest {
                source: Some("radio".into()),
                source_id: Some("http://127.0.0.1:9/flux".into()),
                ..base.clone()
            },
        ),
        (
            "podcast",
            PlayRequest {
                source: Some("podcast".into()),
                source_id: Some("http://127.0.0.1:9/episode.mp3".into()),
                ..base.clone()
            },
        ),
        (
            "upnp",
            PlayRequest {
                source: Some("upnp".into()),
                source_id: Some("http://127.0.0.1:9/piste.flac".into()),
                ..base
            },
        ),
    ]
}

async fn mock_de(orch: &PlaybackOrchestrator, sortie: &str) -> (usize, u64) {
    let arc = orch
        .outputs
        .lock()
        .await
        .get(&format!("appareil-7974-{sortie}"))
        .unwrap();
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().unwrap();
    (mock.play_titles().await.len(), mock.resume_call_count())
}

fn en_file(db: &Arc<dyn DbBackend>, zone_id: i64) {
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[QueueInput::Streaming {
                source: "qobuz".into(),
                source_id: "789".into(),
                title: "En file".into(),
                artist: "Artiste".into(),
                album: None,
                cover_url: None,
                duration_ms: 200_000,
                track_number: None,
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            }],
        )
        .unwrap();
}

/// Toutes les entrées de lecture d'une zone, et ce qu'elles ont rendu.
async fn toutes_les_entrees(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
) -> Vec<(String, Result<(), String>)> {
    let mut issues = Vec::new();
    for (chemin, req) in demandes(zone_id) {
        let r = orch.play(req.clone()).await.map(|_| ());
        issues.push((format!("play/{chemin}"), r));
        let r = orch.play_context_resume(req.clone()).await.map(|_| ());
        issues.push((format!("reprise_de_contexte/{chemin}"), r));
        let r = orch
            .play_transfere_sans_historique(req.clone())
            .await
            .map(|_| ());
        issues.push((format!("transfert/{chemin}"), r));
        let r = orch.play_without_history(req).await.map(|_| ());
        issues.push((format!("recreation_du_flux/{chemin}"), r));
    }
    let r = orch.play_from_queue(zone_id, 0).await.map(|_| ());
    issues.push(("file".into(), r));
    let r = orch
        .play_from_queue_en_fin_de_piste(zone_id, 0)
        .await
        .map(|_| ());
    issues.push(("fin_de_piste".into(), r));
    let r = orch.resume(zone_id, None).await.map_err(|e| e.to_string());
    issues.push(("reprise_apres_pause".into(), r));
    issues
}

async fn chaque_chemin_refuse(politique: PolitiqueParentale, raison: &str) {
    let orch = orchestrateur();
    poser(&orch.db, ENFANT, ADMIN, 0, &politique).unwrap();
    for (i, sortie) in SORTIES.iter().enumerate() {
        let zone = PREMIERE_ZONE + i as i64;
        en_file(&orch.db, zone);
        orch.playback.set_session_profile(zone, Some(ENFANT)).await;
        for (entree, issue) in toutes_les_entrees(&orch, zone).await {
            let e = issue.expect_err(&format!(
                "#7974 : {entree} sur une sortie {sortie} a joué pour un profil {raison}"
            ));
            assert!(
                e.contains(&format!("{SENTINELLE}{raison}")),
                "#7974 : {entree} sur une sortie {sortie} : refus attendu « {raison} », reçu « {e} »"
            );
        }
        assert_eq!(
            mock_de(&orch, sortie).await,
            (0, 0),
            "#7974 : la sortie {sortie} a reçu une lecture ou une reprise"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chaque_chemin_refuse_hors_plage_7974() {
    chaque_chemin_refuse(hors_plage(), "hors_plage").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chaque_chemin_refuse_budget_epuise_7974() {
    chaque_chemin_refuse(budget(0), "budget_quotidien_epuise").await;
}

/// Budget consommé par l'écoute du jour (et non posé à zéro).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_consomme_refuse_7974() {
    let orch = orchestrateur();
    poser(&orch.db, ENFANT, ADMIN, 0, &budget(30)).unwrap();
    let zone = PREMIERE_ZONE + 1;
    orch.playback.set_session_profile(zone, Some(ENFANT)).await;
    assert!(orch.verifier_le_bail(zone).await.is_ok());
    bail_d_ecoute::crediter(
        &orch.db,
        ENFANT,
        bail_d_ecoute::maintenant_local().date(),
        30 * 60,
    )
    .unwrap();
    let e = orch.play(demandes(zone).remove(1).1).await.unwrap_err();
    assert!(
        e.contains("parental_lease_refused:budget_quotidien_epuise"),
        "#7974 : {e}"
    );
}

/// Sans politique, ou pour un autre profil que le profil supervisé, aucune
/// entrée ne rend le refus du bail : le comportement est celui d'avant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_politique_aucun_changement_7974() {
    let orch = orchestrateur();
    // Une politique qui ne limite que les listes ne demande pas de bail.
    poser(
        &orch.db,
        ENFANT,
        ADMIN,
        0,
        &PolitiqueParentale {
            radios_autorisees: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    // Un autre profil supervisé, hors plage : il ne touche pas les autres.
    poser(&orch.db, 3, ADMIN, 0, &hors_plage()).unwrap();
    for (i, profil) in [None, Some(ADMIN), Some(ENFANT)].into_iter().enumerate() {
        let zone = PREMIERE_ZONE + i as i64;
        en_file(&orch.db, zone);
        orch.playback.set_session_profile(zone, profil).await;
        assert_eq!(orch.verifier_le_bail(zone).await, Ok(()));
        for (entree, issue) in toutes_les_entrees(&orch, zone).await {
            if let Err(e) = issue {
                assert!(
                    !e.contains(SENTINELLE),
                    "#7974 : {entree} refusé par le bail sans politique limitante ({profil:?}) : {e}"
                );
            }
        }
    }
    let etat = ZoneState {
        zone_id: PREMIERE_ZONE,
        state: PlayState::Playing,
        session_profile_id: Some(ENFANT),
        ..Default::default()
    };
    let t0 = Instant::now();
    let heure = bail_d_ecoute::maintenant_local();
    for s in 0..30 {
        orch.decompter_le_bail_a(&etat, t0 + Duration::from_secs(s), heure)
            .await;
    }
    assert_eq!(
        bail_d_ecoute::ecoute_entre(&orch.db, ENFANT, heure.date(), heure.date()).unwrap(),
        0,
        "#7974 : rien n'est compté pour un profil sans politique limitante"
    );
}

/// Le temps écouté avance quand la zone joue, pas quand elle est en pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_decompte_exclut_la_pause_7974() {
    let orch = orchestrateur();
    poser(&orch.db, ENFANT, ADMIN, 0, &budget(120)).unwrap();
    let mut etat = ZoneState {
        zone_id: PREMIERE_ZONE,
        state: PlayState::Playing,
        session_profile_id: Some(ENFANT),
        ..Default::default()
    };
    let t0 = Instant::now();
    let heure = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    let tour = async |etat: &ZoneState, s: u64| {
        orch.decompter_le_bail_a(etat, t0 + Duration::from_secs(s), heure)
            .await
    };
    // 20 s de lecture (tours 0 à 20)…
    for s in 0..=20 {
        tour(&etat, s).await;
    }
    // … 60 s de pause…
    etat.state = PlayState::Paused;
    for s in 21..=80 {
        tour(&etat, s).await;
    }
    // … 10 s de lecture, puis arrêt.
    etat.state = PlayState::Playing;
    for s in 81..=91 {
        tour(&etat, s).await;
    }
    etat.state = PlayState::Stopped;
    tour(&etat, 92).await;
    assert_eq!(
        bail_d_ecoute::ecoute_entre(&orch.db, ENFANT, heure.date(), heure.date()).unwrap(),
        30,
        "#7974 : 20 s + 10 s de lecture ; les 60 s de pause ne comptent pas"
    );
}

/// La décision de fin de bail : la piste qui finit dans le délai d'annonce va
/// à son terme ; sinon la fin est annoncée, puis la zone s'arrête.
#[test]
fn geste_de_fin_de_bail_7974() {
    let t0 = Instant::now();
    let fin = Some(t0 + Duration::from_secs(60));
    assert_eq!(
        geste_de_fin_de_bail(t0, fin, None, None),
        GesteDeFinDeBail::Rien
    );
    assert_eq!(
        geste_de_fin_de_bail(t0 + Duration::from_secs(60), fin, None, Some(20_000)),
        GesteDeFinDeBail::LaisserFinirLaPiste
    );
    assert_eq!(
        geste_de_fin_de_bail(t0 + Duration::from_secs(60), fin, None, None),
        GesteDeFinDeBail::Annoncer,
        "un flux sans durée est annoncé, pas coupé"
    );
    assert_eq!(
        geste_de_fin_de_bail(t0 + Duration::from_secs(60), fin, None, Some(200_000)),
        GesteDeFinDeBail::Annoncer
    );
    let arret = Some(t0 + Duration::from_secs(60) + DELAI_D_ANNONCE);
    assert_eq!(
        geste_de_fin_de_bail(t0 + Duration::from_secs(70), fin, arret, None),
        GesteDeFinDeBail::Rien
    );
    assert_eq!(
        geste_de_fin_de_bail(t0 + Duration::from_secs(90), fin, arret, None),
        GesteDeFinDeBail::Arreter
    );
}

/// Bout en bout : une radio jouée par un profil à qui il reste une minute.
/// À l'échéance, la fin est annoncée (la zone joue encore), puis la zone est
/// arrêtée après le délai d'annonce.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fin_de_bail_annoncee_puis_arret_7974() {
    let orch = orchestrateur();
    poser(&orch.db, ENFANT, ADMIN, 0, &budget(1)).unwrap();
    let zone = PREMIERE_ZONE + 1;
    orch.playback.set_session_profile(zone, Some(ENFANT)).await;
    orch.playback
        .play(
            zone,
            NowPlaying {
                title: "Radio".into(),
                source: "radio".into(),
                ..Default::default()
            },
        )
        .await;
    let mut evenements = orch.playback.subscribe();
    let t0 = Instant::now();
    let heure = bail_d_ecoute::maintenant_local();
    for s in 0..=62 {
        let etat = orch.playback.get_state(zone).await;
        orch.decompter_le_bail_a(&etat, t0 + Duration::from_secs(s), heure)
            .await;
    }
    assert_eq!(
        orch.playback.get_state(zone).await.state,
        PlayState::Playing,
        "#7974 : l'échéance annonce la fin, elle ne coupe pas le son"
    );
    let mut annonce = false;
    while let Ok(e) = evenements.try_recv() {
        annonce |= e.event == "parental_lease_ending" && e.zone_id == zone;
    }
    assert!(annonce, "#7974 : la fin de bail n'a pas été annoncée");
    for s in 63..=95 {
        let etat = orch.playback.get_state(zone).await;
        orch.decompter_le_bail_a(&etat, t0 + Duration::from_secs(s), heure)
            .await;
    }
    assert_ne!(
        orch.playback.get_state(zone).await.state,
        PlayState::Playing,
        "#7974 : la zone joue encore après le délai annoncé"
    );
}

/// La piste suivante n'est armée sur le renderer que si le bail la couvre.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enchainement_non_arme_apres_la_fin_du_bail_7974() {
    let orch = orchestrateur();
    let zone = PREMIERE_ZONE + 1;
    orch.playback.set_session_profile(zone, Some(ENFANT)).await;
    assert!(
        orch.bail_couvre_l_enchainement(zone, Duration::from_secs(120))
            .await,
        "sans politique, l'enchaînement est armé comme avant"
    );
    poser(&orch.db, ENFANT, ADMIN, 0, &budget(1)).unwrap();
    assert!(
        orch.bail_couvre_l_enchainement(zone, Duration::from_secs(30))
            .await
    );
    assert!(
        !orch
            .bail_couvre_l_enchainement(zone, Duration::from_secs(120))
            .await,
        "#7974 : la suivante commencerait après la fin du bail"
    );
}
