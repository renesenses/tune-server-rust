//! #9044 — oublier les appareils partis du réseau.

use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::{MotifMasquage, ZoneDAppareil, ZoneRepo};
use tune_core::outputs::mock::MockOutput;

use super::*;
use crate::auth::RequireAdmin;
use crate::state::AppState;

fn zone(id: i64, device: &str, ty: &str, online: bool, hidden: bool) -> ZoneDAppareil {
    ZoneDAppareil {
        id,
        name: format!("Zone {id}"),
        output_type: Some(ty.to_string()),
        output_device_id: device.to_string(),
        online,
        hidden,
        motif: hidden.then(|| "suppression_utilisateur".to_string()),
        sortie_liberee: false,
    }
}

#[test]
fn absente_suit_la_regle_de_get_zones_sauf_pour_les_sorties_locales() {
    let presents: HashSet<String> = ["uuid:la".to_string()].into();
    // Réseau : présent = annoncé, quelle que soit la colonne.
    assert!(!zone_absente(
        &zone(1, "uuid:la", "dlna", false, false),
        &presents
    ));
    assert!(zone_absente(
        &zone(2, "uuid:partie", "dlna", true, false),
        &presents
    ));
    // Sortie locale : la colonne `online`.
    assert!(zone_absente(
        &zone(3, "local:DAC", "local", false, false),
        &presents
    ));
    assert!(!zone_absente(
        &zone(4, "local:DAC", "local", true, false),
        &presents
    ));
    // Navigateur : jamais absente.
    assert!(!zone_absente(
        &zone(5, "browser:x", "browser", false, false),
        &presents
    ));
}

#[test]
fn verdict_ne_casse_jamais_une_zone_en_lecture_ni_celle_d_un_appareil_present() {
    let visible = zone(7, "d", "dlna", false, false);
    let masquee = zone(8, "d", "dlna", false, true);
    let joue: HashSet<i64> = [7].into();
    let rien = HashSet::new();

    assert_eq!(
        verdict(std::slice::from_ref(&visible), &joue, false, true),
        Err(Refus::ZoneEnLecture {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, true, true),
        Err(Refus::AppareilPresent {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, false, false),
        Err(Refus::ZoneLiee {
            zone_id: 7,
            zone: "Zone 7".into()
        })
    );
    assert_eq!(
        verdict(std::slice::from_ref(&visible), &rien, false, true),
        Ok(vec![7])
    );
    assert_eq!(
        verdict(std::slice::from_ref(&masquee), &rien, true, false),
        Ok(vec![8])
    );
    assert_eq!(verdict(&[], &rien, true, false), Ok(vec![]));
}

#[test]
fn les_garde_fous_de_la_decouverte_ne_sont_ni_supprimes_ni_bloquants() {
    let rien = HashSet::new();
    let mut reflet = zone(9, "d", "dlna", false, true);
    reflet.motif = Some("zone_reflet".into());
    let mut ignore = zone(10, "d", "dlna", false, true);
    ignore.motif = Some("appareil_ignore".into());
    let mut fusion = zone(11, "d", "dlna", false, true);
    fusion.motif = Some("fusion".into());
    let mut sans_motif = zone(12, "d", "dlna", false, true);
    sans_motif.motif = None;
    let utilisateur = zone(13, "d", "dlna", false, true);
    let joue_reflet: HashSet<i64> = [9].into();
    let zs = [reflet, ignore, fusion, sans_motif, utilisateur];
    // Seules 12 (sans motif) et 13 (suppression de l'utilisateur) partent.
    assert_eq!(verdict(&zs, &rien, false, false), Ok(vec![12, 13]));
    // Une lecture sur un garde-fou ne bloque pas : il n'est pas touché.
    assert_eq!(verdict(&zs, &joue_reflet, false, false), Ok(vec![12, 13]));
    // Un appareil dont il ne reste que des garde-fous n'est pas listé.
    assert!(absents(&zs[..3], &rien).is_empty());
    let liste = absents(&zs, &rien);
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0]["zones"].as_array().unwrap().len(), 2);
}

// --- Bancs avec un vrai AppState --------------------------------------------

fn banc() -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    (dossier, state)
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

async fn liste(state: &AppState) -> Vec<Value> {
    let (s, v) = corps(list_absent_devices(State(state.clone())).await).await;
    assert_eq!(s, StatusCode::OK);
    v["items"].as_array().cloned().unwrap_or_default()
}

const PARTI: &str = "uuid:9044-ampli-parti";

#[tokio::test]
async fn oublier_un_appareil_absent_nomme_sa_zone_puis_la_supprime_et_il_revient_comme_neuf() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("Ampli du salon", Some("dlna"), Some(PARTI))
        .unwrap();

    // Listé parmi les absents, avec sa zone, SANS durée (pas de date de départ).
    let items = liste(&state).await;
    let ligne = items
        .iter()
        .find(|i| i["device_id"] == PARTI)
        .expect("absent listé");
    assert_eq!(
        ligne["zones"],
        json!([{"zone_id": zid, "zone_name": "Ampli du salon", "zone_hidden": false}])
    );
    assert!(ligne.get("absent_for_s").is_none() && ligne.get("last_seen_at").is_none());

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

    // Avec l'accord : la ligne part, et la réponse la NOMME.
    let (s, bilan) = oublier_route(&state, PARTI, true).await;
    assert_eq!(s, StatusCode::OK, "{bilan}");
    assert_eq!(
        bilan["zones_deleted"],
        json!([{"zone_id": zid, "zone_name": "Ampli du salon"}])
    );
    assert!(repo.get(zid).unwrap().is_none());
    assert!(!liste(&state).await.iter().any(|i| i["device_id"] == PARTI));

    // Il revient : zone neuve, visible.
    let (neuve, nee) = repo.get_or_create("Ampli", Some("dlna"), PARTI).unwrap();
    assert!(nee && neuve != zid);
    assert!(!repo.is_device_hidden(PARTI));
}

#[tokio::test]
async fn une_zone_supprimee_par_l_utilisateur_part_et_ne_bloque_plus_le_retour() {
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
async fn une_zone_reflet_ou_ignoree_n_est_jamais_supprimee() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let reflet = repo
        .create("Reflet", Some("dlna"), Some("reflet-9044"))
        .unwrap();
    repo.masquer(reflet, MotifMasquage::ZoneReflet).unwrap();
    let ignoree = repo
        .create("Ignorée", Some("airplay"), Some("ignore-9044"))
        .unwrap();
    repo.masquer(ignoree, MotifMasquage::AppareilIgnore)
        .unwrap();
    assert!(
        !liste(&state)
            .await
            .iter()
            .any(|i| i["device_id"] == "reflet-9044" || i["device_id"] == "ignore-9044")
    );
    for (id, zid) in [("reflet-9044", reflet), ("ignore-9044", ignoree)] {
        let (s, bilan) = oublier_route(&state, id, true).await;
        assert_eq!(s, StatusCode::OK, "{bilan}");
        assert_eq!(bilan["zones_deleted"], json!([]));
        assert!(repo.get(zid).unwrap().is_some(), "garde-fou {id} supprimé");
        assert!(repo.is_device_hidden(id), "garde-fou {id} démasqué");
    }
}

#[tokio::test]
async fn la_zone_qui_a_lache_sa_sortie_part_avec_l_appareil_et_sa_cle() {
    let (_d, state) = banc();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    // #8987 : l'ancienne zone (supprimée) a lâché l'appareil, la nouvelle le tient.
    let ancienne = repo
        .create("Ancienne", Some("dlna"), Some("uuid:repris-9044"))
        .unwrap();
    repo.masquer(ancienne, MotifMasquage::SuppressionUtilisateur)
        .unwrap();
    state
        .backend
        .execute(
            &format!("UPDATE zones SET output_device_id = NULL WHERE id = {ancienne}"),
            &[],
        )
        .unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(&ZoneRepo::cle_sortie_liberee(ancienne), "uuid:repris-9044")
        .unwrap();
    let nouvelle = repo
        .create("Nouvelle", Some("dlna"), Some("uuid:repris-9044"))
        .unwrap();

    let items = liste(&state).await;
    let ligne = items
        .iter()
        .find(|i| i["device_id"] == "uuid:repris-9044")
        .unwrap();
    assert_eq!(ligne["zone_id"], nouvelle, "la zone visible le représente");
    assert_eq!(ligne["zones"].as_array().unwrap().len(), 2, "{ligne}");

    let (s, bilan) = oublier_route(&state, "uuid:repris-9044", true).await;
    assert_eq!(s, StatusCode::OK, "{bilan}");
    assert!(repo.get(ancienne).unwrap().is_none() && repo.get(nouvelle).unwrap().is_none());
    assert_eq!(repo.sortie_liberee(ancienne), None);
    assert_eq!(
        repo.zone_masquee_qui_tenait("uuid:repris-9044"),
        None,
        "POST /zones repartira à neuf"
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
    assert!(
        !liste(&state)
            .await
            .iter()
            .any(|i| i["device_id"] == "airplay-9044")
    );
}
