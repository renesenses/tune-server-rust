//! #9044 (rc14) — l'oubli automatique, l'oubli manuel qui garde la zone, le
//! retour sans doublon, et le réglage « jamais ».

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::Request;
use chrono::TimeZone;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::outputs::mock::MockOutput;

use super::*;
use crate::routes::devices::oubli_9044;
use crate::routes::devices::{DiscoveredDlnaDevice, load_discovered_dlna, persist_discovered_dlna};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()
}

fn etat(id: &str, absent: bool) -> EtatAppareil {
    EtatAppareil {
        device_id: id.into(),
        absent,
        en_lecture: false,
        manuel: false,
    }
}

fn absence(id: &str, jours: i64) -> Absence {
    Absence {
        device_id: id.into(),
        depuis: horodatage(t0() - chrono::Duration::days(jours)),
    }
}

// --- La règle, sans effet ----------------------------------------------------

#[test]
fn le_premier_balayage_date_le_depart_et_le_retour_l_efface() {
    let p = planifier(&[], &[], &[etat("a", true), etat("b", false)], t0(), 30);
    assert_eq!(
        p.absences,
        vec![Absence {
            device_id: "a".into(),
            depuis: horodatage(t0())
        }]
    );
    assert!(p.a_oublier.is_empty());
    // Un départ déjà daté garde sa date ; un appareil revenu perd la sienne.
    let p = planifier(
        &[absence("a", 3), absence("b", 3)],
        &[],
        &[etat("a", true), etat("b", false)],
        t0(),
        30,
    );
    assert_eq!(p.absences, vec![absence("a", 3)]);
}

#[test]
fn oublie_au_bout_de_la_duree_jamais_avant_jamais_sous_une_lecture_ni_un_manuel() {
    let appareils = [etat("a", true)];
    assert!(
        planifier(&[absence("a", 29)], &[], &appareils, t0(), 30)
            .a_oublier
            .is_empty()
    );
    assert_eq!(
        planifier(&[absence("a", 30)], &[], &appareils, t0(), 30).a_oublier,
        vec!["a".to_string()]
    );
    let mut joue = etat("a", true);
    joue.en_lecture = true;
    assert!(
        planifier(&[absence("a", 90)], &[], &[joue], t0(), 30)
            .a_oublier
            .is_empty()
    );
    let mut manuel = etat("a", true);
    manuel.manuel = true;
    assert!(
        planifier(&[absence("a", 90)], &[], &[manuel], t0(), 30)
            .a_oublier
            .is_empty()
    );
    // Déjà oublié : pas deux fois.
    let deja = Oubli {
        device_id: "a".into(),
        le: horodatage(t0()),
        auto: true,
    };
    let p = planifier(&[absence("a", 90)], &[deja.clone()], &appareils, t0(), 30);
    assert!(p.a_oublier.is_empty());
    // Revenu : la marque s'efface.
    let p = planifier(&[], &[deja], &[etat("a", false)], t0(), 30);
    assert_eq!(p.revenus, vec!["a".to_string()]);
}

#[test]
fn zero_jour_n_oublie_jamais_rien() {
    let p = planifier(&[absence("a", 3650)], &[], &[etat("a", true)], t0(), 0);
    assert!(p.a_oublier.is_empty());
    assert_eq!(p.absences, vec![absence("a", 3650)], "le départ reste daté");
}

// --- Bancs avec un vrai AppState --------------------------------------------

const PARTI: &str = "uuid:9044-rc14-parti";

fn banc() -> (tempfile::TempDir, AppState, ZoneRepo, SettingsRepo, i64) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("Ampli du salon", Some("dlna"), Some(PARTI))
        .unwrap();
    // Un réglage de la zone, qui doit survivre à l'oubli.
    repo.update_volume(zid, 0.42).unwrap();
    persist_discovered_dlna(
        &state.backend,
        &DiscoveredDlnaDevice::new(
            PARTI,
            "http://192.168.1.40:8019/root.xml",
            "Ampli",
            "192.168.1.40",
            8019,
        ),
    );
    (dossier, state, repo, settings, zid)
}

fn dater_le_depart(settings: &SettingsRepo, jours: i64) {
    settings
        .update_json_list::<Absence, _, _>(CLE_ABSENCES, |l| {
            *l = vec![absence(PARTI, jours)];
            Ok(())
        })
        .unwrap();
}

fn dlna_connu(state: &AppState) -> bool {
    load_discovered_dlna(&state.backend)
        .iter()
        .any(|d| d.uuid == PARTI)
}

async fn zone_json(state: &AppState, zid: i64) -> Value {
    let app = Router::new()
        .nest("/zones", crate::routes::zones::router())
        .with_state(state.clone());
    let r = app
        .oneshot(
            Request::builder()
                .uri("/zones")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap();
    let zones = v
        .as_array()
        .cloned()
        .or_else(|| v["zones"].as_array().cloned())
        .unwrap_or_default();
    zones
        .into_iter()
        .find(|z| z["id"] == zid)
        .unwrap_or_else(|| panic!("zone {zid} absente de GET /zones : {v}"))
}

/// Témoin 1 — oubli automatique.
#[tokio::test]
async fn temoin_oubli_automatique_apres_la_duree() {
    let (_d, state, repo, settings, zid) = banc();
    dater_le_depart(&settings, 31);

    let oublies = balayer(&state, t0()).await;

    assert_eq!(oublies, vec![PARTI.to_string()]);
    assert!(!dlna_connu(&state), "plus re-sondé au démarrage");
    let o = lire_oublis(&settings);
    assert_eq!(o.len(), 1);
    assert!(o[0].auto);
    assert!(
        repo.get(zid).unwrap().is_some(),
        "la zone n'est jamais supprimée"
    );
}

/// Témoin 2 — oubli manuel, sans `with_zone` : 200, l'appareil est oublié,
/// la zone reste.
#[tokio::test]
async fn temoin_oubli_manuel_garde_la_zone() {
    let (_d, state, repo, settings, zid) = banc();
    let r = oubli_9044::delete_device(
        RequireAdmin,
        State(state.clone()),
        Path(PARTI.to_string()),
        Query(Default::default()),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(!dlna_connu(&state));
    assert!(repo.get(zid).unwrap().is_some());
    let o = lire_oublis(&settings);
    assert_eq!(o.len(), 1, "{o:?}");
    assert!(!o[0].auto);
}

/// Témoin 3 — la zone d'un appareil oublié reste, hors ligne, marquée
/// « appareil oublié », avec ses réglages.
#[tokio::test]
async fn temoin_zone_conservee_hors_ligne_avec_ses_reglages() {
    let (_d, state, repo, settings, zid) = banc();
    dater_le_depart(&settings, 31);
    balayer(&state, t0()).await;

    let z = zone_json(&state, zid).await;
    assert_eq!(z["online"], false, "{z}");
    assert_eq!(z["device_forgotten"], true, "{z}");
    assert!(z["device_forgotten_at"].is_string(), "{z}");
    let zone = repo.get(zid).unwrap().unwrap();
    assert!(
        (zone.volume - 0.42).abs() < 1e-9,
        "réglage perdu : {}",
        zone.volume
    );

    // Et seule la suppression demandée l'emporte.
    let r = oubli_9044::delete_device(
        RequireAdmin,
        State(state.clone()),
        Path(PARTI.to_string()),
        Query(serde_json::from_value(serde_json::json!({"with_zone": true})).unwrap()),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(repo.get(zid).unwrap().is_none());
    assert!(
        lire_oublis(&settings).is_empty(),
        "plus de marque sans zone"
    );
}

/// Témoin 4 — l'appareil oublié revient : sa zone, pas une nouvelle, et la
/// marque s'efface.
#[tokio::test]
async fn temoin_reapparition_sans_doublon() {
    let (_d, state, repo, settings, zid) = banc();
    dater_le_depart(&settings, 31);
    balayer(&state, t0()).await;
    assert_eq!(lire_oublis(&settings).len(), 1, "prémisse");

    // Retour par la découverte.
    let (id, creee) = repo.get_or_create("Ampli", Some("dlna"), PARTI).unwrap();
    assert_eq!((id, creee), (zid, false), "doublon de zone");
    state
        .outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new(PARTI, "Ampli")));

    let z = zone_json(&state, zid).await;
    assert_eq!(z["online"], true, "{z}");
    assert_eq!(z["device_forgotten"], false, "en ligne, jamais « oublié »");

    balayer(&state, t0() + chrono::Duration::minutes(15)).await;
    assert!(
        lire_oublis(&settings).is_empty(),
        "la marque reste après le retour"
    );
    assert!(lire_absences(&settings).is_empty());
    let zones = repo.zones_de_l_appareil(PARTI).unwrap();
    assert_eq!(zones.len(), 1, "{zones:?}");
}

/// Témoin 5 — réglage « jamais » (0) : rien n'est oublié.
#[tokio::test]
async fn temoin_reglage_desactive_n_oublie_rien() {
    let (_d, state, _repo, settings, _zid) = banc();
    settings.set(CLE_JOURS, "0").unwrap();
    dater_le_depart(&settings, 400);

    assert!(balayer(&state, t0()).await.is_empty());
    assert!(dlna_connu(&state));
    assert!(lire_oublis(&settings).is_empty());
}

#[tokio::test]
async fn la_regle_se_lit_et_s_ecrit() {
    let (_d, state, _repo, settings, _zid) = banc();
    assert_eq!(jours_avant_oubli(&settings), JOURS_PAR_DEFAUT);
    let r = ecrire_la_regle(
        RequireAdmin,
        State(state.clone()),
        Json(NouvelleRegle { after_days: 0 }),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(jours_avant_oubli(&settings), 0);
    let r = ecrire_la_regle(
        RequireAdmin,
        State(state.clone()),
        Json(NouvelleRegle {
            after_days: JOURS_MAX + 1,
        }),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}
