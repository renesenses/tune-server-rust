//! #9044 — oublier les appareils partis du réseau.

use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::Value;
use tune_core::db::zone_repo::{MotifMasquage, ZoneDAppareil, ZoneRepo};
use tune_core::outputs::mock::MockOutput;

use super::*;
use crate::auth::RequireAdmin;
use crate::state::AppState;

fn zone(
    id: i64,
    device: &str,
    ty: &str,
    online: bool,
    hidden: bool,
    age: Option<i64>,
) -> ZoneDAppareil {
    ZoneDAppareil {
        id,
        name: format!("Zone {id}"),
        output_type: Some(ty.to_string()),
        output_device_id: device.to_string(),
        online,
        hidden,
        last_seen_at: None,
        age_secs: age,
    }
}

#[test]
fn absente_suit_la_regle_de_get_zones() {
    let presents: HashSet<String> = ["uuid:la".to_string()].into();
    // Réseau : présent = annoncé, quelle que soit la colonne.
    assert!(!zone_absente(
        &zone(1, "uuid:la", "dlna", false, false, None),
        &presents
    ));
    assert!(zone_absente(
        &zone(2, "uuid:partie", "dlna", true, false, None),
        &presents
    ));
    // Sortie locale : la colonne `online`, seule source.
    assert!(zone_absente(
        &zone(3, "local:DAC", "local", false, false, None),
        &presents
    ));
    assert!(!zone_absente(
        &zone(4, "local:DAC", "local", true, false, None),
        &presents
    ));
    // Navigateur : jamais absente.
    assert!(!zone_absente(
        &zone(5, "browser:x", "browser", false, false, None),
        &presents
    ));
}

#[test]
fn verdict_ne_casse_jamais_une_zone_en_lecture_ni_celle_d_un_appareil_present() {
    let visible = zone(7, "d", "dlna", false, false, Some(10));
    let masquee = zone(8, "d", "dlna", false, true, Some(10));
    let joue: HashSet<i64> = [7].into();
    let rien = HashSet::new();

    // En lecture : refus, même avec l'accord.
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &joue, false, true),
        Err(Refus::ZoneEnLecture {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    // Appareil présent, zone visible : refus, même avec l'accord.
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, true, true),
        Err(Refus::AppareilPresent {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    // Absent, zone visible, sans accord : la zone est nommée.
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, false, false),
        Err(Refus::ZoneLiee {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    // Avec l'accord : supprimée.
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, false, true),
        Ok(vec![7])
    );
    // Zone masquée seule : supprimée sans condition, même appareil présent.
    assert_eq!(
        verdict(std::slice::from_ref(&masquee), &rien, true, false),
        Ok(vec![8])
    );
    // Pas de zone : rien à supprimer.
    assert_eq!(verdict(&[], &rien, true, false), Ok(vec![]));
}

#[test]
fn oublier_les_absents_depuis_n_jours_saute_les_recents_et_les_non_dates() {
    let zs = [
        zone(1, "vieux", "dlna", false, false, Some(30 * 86_400)),
        zone(2, "recent", "dlna", false, false, Some(3600)),
        zone(3, "jamais-vu", "dlna", false, false, None),
        zone(4, "vieux", "airplay", false, true, Some(9 * 86_400)),
    ];
    let presents = HashSet::new();
    let abs = absentes(&zs, &presents);
    assert_eq!(abs.len(), 4);
    assert_eq!(
        abs[0].id, 3,
        "jamais vu : la plus ancienne absence connue, en tête"
    );
    assert_eq!(a_oublier(&abs, 7), vec!["vieux".to_string()]);
    assert_eq!(a_oublier(&abs, 0).len(), 2, "0 jour : tout ce qui est daté");
}

// --- Bancs avec un vrai AppState --------------------------------------------

fn banc() -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    (dossier, state)
}

fn vieillir(state: &AppState, zone_id: i64) {
    state
        .backend
        .execute(
            &format!("UPDATE zones SET online = 0, last_seen_at = '2026-01-01T00:00:00Z' WHERE id = {zone_id}"),
            &[],
        )
        .unwrap();
}

async fn corps(r: axum::response::Response) -> (StatusCode, Value) {
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn oublier_route(state: &AppState, id: &str, with_zone: bool) -> (StatusCode, Value) {
    corps(
        delete_device(
            RequireAdmin,
            State(state.clone()),
            Path(id.to_string()),
            Query(OublierQuery { with_zone }),
        )
        .await,
    )
    .await
}

const PARTI: &str = "uuid:9044-ampli-parti";

#[tokio::test]
async fn oublier_un_appareil_absent_nomme_sa_zone_puis_la_supprime_et_il_revient_comme_neuf() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("Ampli du salon", Some("dlna"), Some(PARTI))
        .unwrap();
    vieillir(&state, zid);

    // Listé parmi les absents, avec sa date.
    let (s, liste) = corps(list_absent_devices(State(state.clone())).await).await;
    assert_eq!(s, StatusCode::OK);
    let items = liste["items"].as_array().unwrap();
    let ligne = items
        .iter()
        .find(|i| i["device_id"] == PARTI)
        .expect("absent listé");
    assert_eq!(ligne["zone_name"], "Ampli du salon");
    assert_eq!(ligne["last_seen_at"], "2026-01-01T00:00:00Z");
    assert!(ligne["absent_for_s"].as_i64().unwrap() > 7 * 86_400);

    // Sans accord : 409 qui nomme la zone ; rien n'est touché.
    let (s, refus) = oublier_route(&state, PARTI, false).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(refus["code"], "zone_liee");
    assert_eq!(refus["zone_name"], "Ampli du salon");
    assert!(
        refus["message"]
            .as_str()
            .unwrap()
            .contains("Ampli du salon"),
        "{refus}"
    );
    assert!(repo.get(zid).unwrap().is_some());

    // Avec l'accord : la ligne part.
    let (s, bilan) = oublier_route(&state, PARTI, true).await;
    assert_eq!(s, StatusCode::OK, "{bilan}");
    assert_eq!(bilan["zones_deleted"], serde_json::json!([zid]));
    assert!(repo.get(zid).unwrap().is_none());
    let (_, liste) = corps(list_absent_devices(State(state.clone())).await).await;
    assert!(
        !liste["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["device_id"] == PARTI)
    );

    // Il revient : zone neuve, visible.
    let (neuve, nee) = repo.get_or_create("Ampli", Some("dlna"), PARTI).unwrap();
    assert!(nee && neuve != zid);
    assert!(!repo.is_device_hidden(PARTI));
}

#[tokio::test]
async fn une_zone_supprimee_masquee_part_sans_confirmation() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("Vieux Cast", Some("chromecast"), Some("cast-9044"))
        .unwrap();
    repo.masquer(zid, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    let (s, bilan) = oublier_route(&state, "cast-9044", false).await;
    assert_eq!(s, StatusCode::OK, "{bilan}");
    assert!(repo.get(zid).unwrap().is_none());
    let (_, nee) = repo
        .get_or_create("Cast", Some("chromecast"), "cast-9044")
        .unwrap();
    assert!(
        nee,
        "la zone masquée ne bloque plus le retour de l'appareil"
    );
}

#[tokio::test]
async fn un_appareil_present_garde_sa_zone() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("Enceinte", Some("airplay"), Some("airplay-9044"))
        .unwrap();
    state
        .outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new("airplay-9044", "Enceinte")));
    let (s, refus) = oublier_route(&state, "airplay-9044", true).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(refus["code"], "appareil_present");
    assert!(repo.get(zid).unwrap().is_some());
    assert!(
        state.outputs.lock().await.contains("airplay-9044"),
        "rien n'est retiré"
    );
    let (_, liste) = corps(list_absent_devices(State(state.clone())).await).await;
    assert!(
        !liste["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["device_id"] == "airplay-9044")
    );
}

#[tokio::test]
async fn oublier_les_absents_depuis_7_jours_ne_prend_que_les_vieux() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let vieux = repo
        .create("Vieux", Some("dlna"), Some("uuid:vieux-9044"))
        .unwrap();
    let recent = repo
        .create("Récent", Some("dlna"), Some("uuid:recent-9044"))
        .unwrap();
    vieillir(&state, vieux);
    repo.set_online_by_device("uuid:recent-9044", true).unwrap();
    repo.set_online_by_device("uuid:recent-9044", false)
        .unwrap();

    let (s, bilan) = corps(
        forget_absent_devices(
            RequireAdmin,
            State(state.clone()),
            Query(OublierAbsentsQuery::default()),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{bilan}");
    assert_eq!(bilan["older_than_days"], 7);
    assert_eq!(bilan["forgotten"], serde_json::json!(["uuid:vieux-9044"]));
    assert!(repo.get(vieux).unwrap().is_none());
    assert!(
        repo.get(recent).unwrap().is_some(),
        "absent depuis une heure : gardé"
    );
}
