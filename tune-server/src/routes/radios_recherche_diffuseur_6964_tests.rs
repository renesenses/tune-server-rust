//! #6964 — une recherche « ARD » ne rendait rien (castalla, 24/10/2026).
//!
//! Aucune station de l'ARD ne porte « ARD » dans son nom (« WDR 2 »,
//! « NDR Kultur », « Bayern 3 »). L'annuaire v2 range le diffuseur parmi les
//! mots-clés de la fiche (`keywords` : `["Radio France"]` pour FIP, relevé du
//! 19/10), et `annuaire_depuis_json` les jetait : ni `/radios/search` ni
//! l'écran « Annuaire » ne pouvaient trouver une station par son réseau.
//!
//! Les fiches ARD de ces essais sont écrites à la main sur le modèle exact de
//! la fiche FIP du relevé : le relevé figé ne contient aucune station allemande.

use super::*;
use crate::routes::radios_annuaire::{EtatAnnuaire, FiltresAnnuaire, corps_annuaire};
use axum::http::Request;
use tower::ServiceExt;

const EXTRAIT: &str = include_str!("../../tests/fixtures/annuaire_v2_extrait_20261019.json");
const U_WDR2: &str =
    "https://wdr-wdr2-rheinland.icecastssl.wdr.de/wdr/wdr2/rheinland/mp3/128/stream.mp3";
const U_NDR: &str = "https://icecast.ndr.de/ndr/ndrkultur/live/mp3/128/stream.mp3";

fn fiches_du_releve() -> Vec<StationAnnuaire> {
    let corps: Value = serde_json::from_str(EXTRAIT).unwrap();
    annuaire_depuis_json(corps["radios"].as_array().unwrap())
}

fn fiche_ard(nom: &str, ext: &str, flux: &str) -> Value {
    json!({
        "external_id": ext,
        "name": nom,
        "country": "DE",
        "genre": "public",
        "keywords": ["ARD"],
        "status": "active",
        "retired_at": null,
        "stream_url": flux,
        "streams": [],
    })
}

/// FIP du relevé, plus deux fiches ARD.
fn annuaire_avec_l_ard() -> Vec<StationAnnuaire> {
    let mut fiches = fiches_du_releve();
    fiches.extend(annuaire_depuis_json(&[
        fiche_ard("WDR 2", "S990601", U_WDR2),
        fiche_ard("NDR Kultur", "S990602", U_NDR),
    ]));
    fiches
}

fn noms(fiches: &[StationAnnuaire]) -> Vec<&str> {
    fiches.iter().map(|f| f.name.as_str()).collect()
}

#[test]
fn les_mots_cles_de_la_fiche_sont_lus_et_non_plus_jetes() {
    let f = fiches_du_releve();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    assert_eq!(fip.keywords, vec!["Radio France".to_string()]);
    let paradise = f
        .iter()
        .find(|s| s.name.starts_with("Radio Paradise"))
        .unwrap();
    assert!(paradise.keywords.is_empty(), "{:?}", paradise.keywords);
}

#[test]
fn la_recherche_trouve_les_stations_d_un_diffuseur_dans_l_annuaire() {
    let annuaire = annuaire_avec_l_ard();
    let ard = stations_annuaire_absentes(&annuaire, "ARD", &[]);
    assert_eq!(
        noms(&ard),
        vec!["WDR 2", "NDR Kultur"],
        "« ARD » doit trouver les stations dont l'ARD est le diffuseur"
    );
    // Sans casse, et le diffuseur du relevé réel aussi.
    assert_eq!(
        noms(&stations_annuaire_absentes(&annuaire, "ard", &[])).len(),
        2
    );
    let radio_france = stations_annuaire_absentes(&annuaire, "radio france", &[]);
    assert_eq!(
        noms(&radio_france),
        vec!["FIP", "FIP: Jazz", "FIP: Nouveautés"],
        "« radio france » doit trouver les stations de Radio France du relevé"
    );
}

#[test]
fn l_ecran_annuaire_trouve_les_stations_d_un_diffuseur() {
    let annuaire = annuaire_avec_l_ard();
    let filtres = FiltresAnnuaire {
        q: Some("ARD".into()),
        ..Default::default()
    };
    let corps = corps_annuaire(&annuaire, &[], &filtres, &EtatAnnuaire::default());
    let trouves: Vec<&str> = corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(trouves, vec!["WDR 2", "NDR Kultur"], "{corps}");
}

fn station(id: i64, nom: &str, url: &str) -> RadioStation {
    RadioStation {
        id: Some(id),
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

fn ids(stations: &[RadioStation]) -> Vec<i64> {
    stations.iter().filter_map(|s| s.id).collect()
}

#[test]
fn une_station_de_mes_radios_se_trouve_par_le_diffuseur_de_sa_fiche() {
    let annuaire = annuaire_avec_l_ard();
    let wdr = station(1, "WDR 2 Rheinland", U_WDR2);
    let autre = station(2, "Radio sans fiche", "https://exemple.invalid/x");
    let catalogue = vec![wdr.clone(), autre];
    let liens = std::collections::HashMap::new();

    let trouvees = stations_locales_par_mot_cle(&annuaire, "ARD", &catalogue, &liens, &[]);
    assert_eq!(ids(&trouvees), vec![1]);
    // Déjà rendue par la recherche locale : pas une seconde fois.
    assert!(stations_locales_par_mot_cle(&annuaire, "ARD", &catalogue, &liens, &[wdr]).is_empty());
    // Liée à sa fiche, la station se trouve même si son flux a changé.
    let deplacee = station(3, "Mon WDR", "https://nouveau.example/wdr2");
    let liens = std::collections::HashMap::from([(3, "S990601".to_string())]);
    let trouvees = stations_locales_par_mot_cle(&annuaire, "ard", &[deplacee], &liens, &[]);
    assert_eq!(ids(&trouvees), vec![3]);
}

async fn chercher(app: &axum::Router, q: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/radios/search?q={q}"))
                .header("Accept-Language", "fr-FR")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

#[tokio::test]
async fn la_route_de_recherche_rend_mes_stations_ard_et_propose_les_autres() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let id_wdr = RadioRepo::with_backend(state.backend.clone())
        .create(&station(0, "WDR 2", U_WDR2))
        .unwrap();
    *state.annuaire_radios.write().unwrap() = annuaire_avec_l_ard();

    let corps = chercher(&app, "ARD").await;
    let ids: Vec<i64> = corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_i64())
        .collect();
    assert!(
        ids.contains(&id_wdr),
        "le WDR 2 de « Mes radios » doit sortir pour « ARD » : {corps}"
    );
    assert_eq!(corps["statut"], "resultats", "{corps}");
    let proposees: Vec<&str> = corps["annuaire"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(
        proposees,
        vec!["NDR Kultur"],
        "l'annuaire propose la station ARD absente, pas celle déjà là : {corps}"
    );
}
