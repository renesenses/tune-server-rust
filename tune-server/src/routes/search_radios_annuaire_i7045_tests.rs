//! #7045 — la recherche globale rend aussi l'annuaire des radios.
//!
//! `radios` (« Mes radios ») existait déjà dans `GET /search` ; l'annuaire
//! public, lui, n'y figurait pas : « FIP » ou « Radio Paradise » absentes de
//! « Mes radios » ne se trouvaient qu'en ouvrant l'écran Radio (rapport de
//! Rémi, RC7, 25/10/2026). Le bloc `radios_annuaire` ajoute les fiches de
//! l'annuaire, dans la forme de `GET /radios/directory`.
//!
//! Témoins SANS réseau : l'annuaire est posé directement dans l'état.

use axum::extract::{Query, State};
use serde_json::Value;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

use super::federated_search;
use crate::routes::radios::StationAnnuaire;

fn fiche(id: &str, nom: &str, flux: &str) -> StationAnnuaire {
    StationAnnuaire {
        name: nom.to_string(),
        stream_url: flux.to_string(),
        external_id: Some(id.to_string()),
        genre: Some("eclectic".to_string()),
        country: Some("fr".to_string()),
        stream_urls: vec![flux.to_string()],
        ..Default::default()
    }
}

fn etat() -> crate::state::AppState {
    crate::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn poser_l_annuaire(state: &crate::state::AppState, fiches: Vec<StationAnnuaire>) {
    *state.annuaire_radios.write().unwrap() = fiches;
}

fn dans_mes_radios(state: &crate::state::AppState, nom: &str, flux: &str) -> i64 {
    RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: nom.to_string(),
            url: flux.to_string(),
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
        })
        .unwrap()
}

async fn chercher(state: &crate::state::AppState, chaine: &str) -> Value {
    let uri: axum::http::Uri = format!("/?{chaine}").parse().expect("URI valide");
    let Query(params) = Query::try_from_uri(&uri).expect("paramètres lisibles");
    federated_search(
        State(state.clone()),
        crate::routes::active_profile::ActiveProfile(1),
        Query(params),
    )
    .await
    .0
}

fn noms(bloc: &Value) -> Vec<String> {
    bloc["items"]
        .as_array()
        .unwrap_or_else(|| panic!("pas de tableau `items` : {bloc}"))
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Une station de l'annuaire absente de « Mes radios » est trouvée par la
/// recherche globale, avec la forme de `GET /radios/directory`.
#[tokio::test]
async fn une_station_de_l_annuaire_est_trouvee() {
    let state = etat();
    poser_l_annuaire(
        &state,
        vec![
            fiche("S1", "Radio Paradise", "https://paradise.example/flux"),
            fiche("S2", "FIP", "https://fip.example/flux"),
        ],
    );
    let r = chercher(&state, "q=paradise").await;
    let bloc = &r["radios_annuaire"];
    assert_eq!(noms(bloc), vec!["Radio Paradise"], "{r}");
    assert_eq!(bloc["total"], 1, "{r}");
    assert_eq!(bloc["has_more"], false, "{r}");
    let f = &bloc["items"][0];
    assert_eq!(f["external_id"], "S1", "{f}");
    assert_eq!(f["stream_url"], "https://paradise.example/flux", "{f}");
    assert!(f["my_radio_id"].is_null(), "pas dans Mes radios : {f}");
}

/// Une station déjà dans « Mes radios » est signalée par `my_radio_id`, pour
/// que le client ne la propose pas deux fois.
#[tokio::test]
async fn une_station_deja_dans_mes_radios_porte_son_identifiant() {
    let state = etat();
    let id = dans_mes_radios(&state, "FIP", "https://fip.example/flux");
    poser_l_annuaire(&state, vec![fiche("S2", "FIP", "https://fip.example/flux")]);
    let r = chercher(&state, "q=fip").await;
    assert_eq!(r["radios_annuaire"]["items"][0]["my_radio_id"], id, "{r}");
    assert_eq!(r["radios"][0]["name"], "FIP", "{r}");
}

/// Un aperçu borné : au-delà de la limite, `total` et `has_more` disent le
/// reste, et l'annuaire n'est pas déversé dans la réponse.
#[tokio::test]
async fn l_apercu_est_borne_et_le_total_dit_le_reste() {
    let state = etat();
    let fiches = (0..45)
        .map(|i| {
            fiche(
                &format!("S{i}"),
                &format!("Jazz {i}"),
                &format!("https://jazz.example/{i}"),
            )
        })
        .collect();
    poser_l_annuaire(&state, fiches);
    let r = chercher(&state, "q=jazz").await;
    let bloc = &r["radios_annuaire"];
    assert_eq!(bloc["total"], 45, "{bloc}");
    assert_eq!(bloc["count"], 40, "{bloc}");
    assert_eq!(bloc["items"].as_array().unwrap().len(), 40, "{bloc}");
    assert_eq!(bloc["has_more"], true, "{bloc}");
}

/// Un annuaire jamais relevé (hors ligne) : la clé est PRÉSENTE et vide. Une
/// forme qui dépend de l'état du serveur obligerait chaque client à deviner.
#[tokio::test]
async fn sans_annuaire_la_cle_est_presente_et_vide() {
    let state = etat();
    let r = chercher(&state, "q=fip").await;
    let bloc = &r["radios_annuaire"];
    assert_eq!(bloc["items"], serde_json::json!([]), "{r}");
    assert_eq!(bloc["total"], 0, "{r}");
    assert_eq!(bloc["has_more"], false, "{r}");
}

/// `sources=qobuz` n'est pas une demande du local : comme `radios`, l'annuaire
/// n'est ni cherché ni rendu (#3226).
#[tokio::test]
async fn un_service_seul_ne_cherche_pas_l_annuaire() {
    let state = etat();
    poser_l_annuaire(&state, vec![fiche("S2", "FIP", "https://fip.example/flux")]);
    let r = chercher(&state, "q=fip&sources=qobuz").await;
    assert_eq!(r["radios_annuaire"]["items"], serde_json::json!([]), "{r}");
    let r = chercher(&state, "q=fip&sources=local").await;
    assert_eq!(noms(&r["radios_annuaire"]), vec!["FIP"], "{r}");
}

/// `sources=radios` : les radios SEULES. Ni la bibliothèque (le bloc `local`
/// reste présent, vide), ni un service. C'est ce que demande l'écran de
/// recherche, qui tient déjà son local de `/library/search`.
#[tokio::test]
async fn sources_radios_rend_les_radios_sans_la_bibliotheque() {
    let state = etat();
    dans_mes_radios(&state, "FIP", "https://fip.example/flux");
    poser_l_annuaire(
        &state,
        vec![fiche("S3", "FIP Jazz", "https://fipjazz.example/flux")],
    );
    let r = chercher(&state, "q=fip&sources=radios").await;
    assert_eq!(r["radios"][0]["name"], "FIP", "{r}");
    assert_eq!(noms(&r["radios_annuaire"]), vec!["FIP Jazz"], "{r}");
    assert_eq!(r["local"]["tracks"], serde_json::json!([]), "{r}");
    assert_eq!(r["services"], serde_json::json!({}), "{r}");
}

/// La recherche globale emprunte le même filtre multi-mots que l'annuaire.
#[tokio::test]
async fn recherche_7198_globale_trouve_tous_les_mots_sans_ordre() {
    let state = etat();
    poser_l_annuaire(
        &state,
        vec![
            fiche("S1", "BBC Radio 3 Unwind", "https://bbc.example/unwind"),
            fiche("S2", "BBC Radio 3", "https://bbc.example/radio3"),
            fiche("S3", "FIP Groove", "https://fip.example/groove"),
            fiche("S4", "FIP", "https://fip.example/flux"),
            fiche("S5", "Café del Mar", "https://cafe.example/flux"),
        ],
    );
    for (q, attendus) in [
        ("BBC unwind", vec!["BBC Radio 3 Unwind"]),
        ("uNwInD BBC", vec!["BBC Radio 3 Unwind"]),
        ("  BBC\t unwind  ", vec!["BBC Radio 3 Unwind"]),
        ("groove FIP", vec!["FIP Groove"]),
        ("MAR cafe", vec!["Café del Mar"]),
        ("mar CAFE\u{301}", vec!["Café del Mar"]),
        ("BBC groove", vec![]),
        ("BBC unwind absent", vec![]),
        ("bbc", vec!["BBC Radio 3 Unwind", "BBC Radio 3"]),
        ("fip", vec!["FIP Groove", "FIP"]),
        ("cafE", vec!["Café del Mar"]),
    ] {
        for source in ["", "&sources=radios", "&sources=local"] {
            let r = chercher(&state, &format!("q={}{source}", urlencoding::encode(q))).await;
            let bloc = &r["radios_annuaire"];
            assert_eq!(
                noms(bloc),
                attendus,
                "La recherche globale {q:?} ({source}) doit retrouver tous les mots du nom : {bloc}"
            );
            assert_eq!(bloc["total"], attendus.len(), "{bloc}");
            assert_eq!(bloc["has_more"], false, "{bloc}");
        }
    }
}
