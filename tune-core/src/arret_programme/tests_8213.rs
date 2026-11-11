//! #8213 — l'arrêt programmé arrête la lecture à l'heure dite, les jours
//! cochés, une seule fois par échéance, et rien d'autre.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use chrono::NaiveDate;
use tokio::sync::Mutex;

/// Mardi 10/11/2026 à `h:m`.
fn mardi(h: u32, m: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, 10)
        .unwrap()
        .and_hms_opt(h, m, 30)
        .unwrap()
}

fn reglage(time: &str, jours: &str, zones: Option<Vec<i64>>) -> ArretProgramme {
    ArretProgramme {
        enabled: true,
        time: time.into(),
        days_of_week: jours.into(),
        zone_ids: zones,
    }
}

#[test]
fn le_mardi_10_11_2026_est_bien_un_mardi() {
    assert_eq!(mardi(0, 0).weekday(), chrono::Weekday::Tue);
}

// ─── Validation ────────────────────────────────────────────────

#[test]
fn un_reglage_invalide_est_refuse_a_l_enregistrement() {
    for t in ["", "24:00", "23:60", "2300", "23:5", "aa:bb", "123:00"] {
        assert!(reglage(t, "1111111", None).valider().is_err(), "time {t:?}");
    }
    for j in ["", "111111", "11111111", "1111112", "lundi"] {
        assert!(reglage("23:00", j, None).valider().is_err(), "jours {j:?}");
    }
    assert!(reglage("23:00", "1111111", Some(vec![])).valider().is_err());
    assert!(
        reglage("23:00", "1111111", Some(vec![0]))
            .valider()
            .is_err()
    );
    assert_eq!(
        reglage(" 7:05 ", "1111100", Some(vec![2]))
            .valider()
            .unwrap()
            .time,
        "7:05"
    );
}

#[test]
fn les_jours_et_l_heure_par_defaut_a_la_lecture_du_json() {
    let r: ArretProgramme = serde_json::from_str(r#"{"enabled":true,"time":"23:00"}"#).unwrap();
    assert_eq!(r.days_of_week, "1111111");
    assert_eq!(r.zone_ids, None);
}

// ─── Échéance ──────────────────────────────────────────────────

#[test]
fn l_echeance_tombe_a_la_minute_dite_un_jour_coche() {
    let tous = reglage("23:00", "1111111", None);
    assert!(tous.echeance(mardi(23, 0)));
    assert!(!tous.echeance(mardi(22, 59)));
    assert!(!tous.echeance(mardi(23, 1)));

    // Week-end seulement : rien le mardi.
    assert!(!reglage("23:00", "0000011", None).echeance(mardi(23, 0)));
    // Mardi seul.
    assert!(reglage("23:00", "0100000", None).echeance(mardi(23, 0)));

    let mut inactif = tous.clone();
    inactif.enabled = false;
    assert!(!inactif.echeance(mardi(23, 0)));
}

#[test]
fn la_prochaine_echeance_saute_les_jours_non_coches() {
    // Mardi 22 h, réglage 23 h tous les jours → ce soir.
    assert_eq!(
        reglage("23:00", "1111111", None).prochaine_echeance(mardi(22, 0)),
        Some(mardi(23, 0).with_second(0).unwrap())
    );
    // Mardi 23 h 30, week-end seulement → samedi 14/11 23 h.
    assert_eq!(
        reglage("23:00", "0000011", None).prochaine_echeance(mardi(23, 30)),
        NaiveDate::from_ymd_opt(2026, 11, 14)
            .unwrap()
            .and_hms_opt(23, 0, 0)
    );
    // Mardi seul, déjà passé → mardi suivant.
    assert_eq!(
        reglage("23:00", "0100000", None).prochaine_echeance(mardi(23, 30)),
        NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_opt(23, 0, 0)
    );
    assert_eq!(
        reglage("23:00", "0000000", None).prochaine_echeance(mardi(0, 0)),
        None
    );
}

// ─── Planificateur ─────────────────────────────────────────────

struct Banc {
    db: Arc<dyn DbBackend>,
    playback: Arc<PlaybackManager>,
    planificateur: PlanificateurArretProgramme,
    salon: i64,
    chambre: i64,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let zones = ZoneRepo::with_backend(db.clone());
        let salon = zones
            .create("Salon", Some("dlna"), Some("mock-salon"))
            .unwrap();
        let chambre = zones
            .create("Chambre", Some("dlna"), Some("mock-chambre"))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        {
            let mut o = outputs.lock().await;
            o.register(Box::new(MockOutput::new("mock-salon", "Salon")));
            o.register(Box::new(MockOutput::new("mock-chambre", "Chambre")));
        }
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs,
            None,
        ));
        Self {
            planificateur: PlanificateurArretProgramme::new(db.clone(), orchestrator),
            db,
            playback,
            salon,
            chambre,
        }
    }

    async fn jouer(&self, zone: i64) {
        self.playback
            .play(
                zone,
                NowPlaying {
                    title: "Nocturne".into(),
                    source: "local".into(),
                    ..Default::default()
                },
            )
            .await;
    }

    async fn etat(&self, zone: i64) -> PlayState {
        self.playback.get_state(zone).await.state
    }
}

#[tokio::test]
async fn a_l_heure_dite_toutes_les_zones_qui_jouent_s_arretent() {
    let banc = Banc::monter().await;
    ecrire(&banc.db, reglage("23:00", "1111111", None)).unwrap();
    banc.jouer(banc.salon).await;
    banc.jouer(banc.chambre).await;
    banc.playback.pause(banc.chambre).await;

    let mut derniere = None;
    // Contre-épreuve : une minute avant, rien ne bouge.
    assert!(
        banc.planificateur
            .passage(mardi(22, 59), &mut derniere)
            .await
            .is_empty()
    );
    assert_eq!(banc.etat(banc.salon).await, PlayState::Playing);

    let mut arretees = banc
        .planificateur
        .passage(mardi(23, 0), &mut derniere)
        .await;
    arretees.sort();
    assert_eq!(arretees, vec![banc.salon, banc.chambre]);
    assert_eq!(banc.etat(banc.salon).await, PlayState::Stopped);
    assert_eq!(banc.etat(banc.chambre).await, PlayState::Stopped);
}

#[tokio::test]
async fn une_echeance_ne_sert_qu_une_fois() {
    let banc = Banc::monter().await;
    ecrire(&banc.db, reglage("23:00", "1111111", None)).unwrap();
    banc.jouer(banc.salon).await;
    let mut derniere = None;
    assert_eq!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await,
        vec![banc.salon]
    );
    // L'utilisateur relance la musique dans la même minute : le second
    // passage du planificateur ne la recoupe pas.
    banc.jouer(banc.salon).await;
    assert!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await
            .is_empty()
    );
    assert_eq!(banc.etat(banc.salon).await, PlayState::Playing);
}

#[tokio::test]
async fn seules_les_zones_choisies_s_arretent() {
    let banc = Banc::monter().await;
    ecrire(
        &banc.db,
        reglage("23:00", "1111111", Some(vec![banc.chambre])),
    )
    .unwrap();
    banc.jouer(banc.salon).await;
    banc.jouer(banc.chambre).await;
    let mut derniere = None;
    assert_eq!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await,
        vec![banc.chambre]
    );
    assert_eq!(banc.etat(banc.salon).await, PlayState::Playing);
    assert_eq!(banc.etat(banc.chambre).await, PlayState::Stopped);
}

#[tokio::test]
async fn sans_reglage_ou_desactive_ou_jour_non_coche_rien_ne_s_arrete() {
    let banc = Banc::monter().await;
    banc.jouer(banc.salon).await;
    let mut derniere = None;
    assert!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await
            .is_empty()
    );

    let mut r = reglage("23:00", "1111111", None);
    r.enabled = false;
    ecrire(&banc.db, r).unwrap();
    assert!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await
            .is_empty()
    );

    ecrire(&banc.db, reglage("23:00", "0000011", None)).unwrap();
    assert!(
        banc.planificateur
            .passage(mardi(23, 0), &mut derniere)
            .await
            .is_empty()
    );
    assert_eq!(banc.etat(banc.salon).await, PlayState::Playing);

    effacer(&banc.db).unwrap();
    assert_eq!(lire(&banc.db), None);
}
