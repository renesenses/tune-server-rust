//! Première vague du banc runtime web ↔ serveur (#1897).
//!
//! La carte est générée depuis les types réellement employés par le client
//! web. Ces tests ne recopient donc pas une seconde fois les champs attendus :
//! ils chargent `docs/contrat-web.json`, appellent le vrai routeur Axum et
//! confrontent la réponse à la carte commitée.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

const CARTE_WEB: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/contrat-web.json"
));

#[derive(Debug, Deserialize)]
struct CarteContrats {
    routes: Vec<ContratRoute>,
}

#[derive(Debug, Deserialize)]
struct ContratRoute {
    route: String,
    methode: String,
    #[serde(rename = "type")]
    type_web: String,
    liste: bool,
    champs_obligatoires: Vec<String>,
}

fn contrats_pour<'a>(
    carte: &'a CarteContrats,
    methode: &str,
    route: &str,
) -> Result<Vec<&'a ContratRoute>, String> {
    let contrats: Vec<_> = carte
        .routes
        .iter()
        .filter(|contrat| contrat.methode == methode && contrat.route == route)
        .collect();
    if contrats.is_empty() {
        return Err(format!(
            "aucun contrat web cartographie pour {methode} {route}"
        ));
    }
    Ok(contrats)
}

fn exige_champs(valeur: &Value, champs: &[String], contexte: &str) -> Result<(), String> {
    let objet = valeur
        .as_object()
        .ok_or_else(|| format!("{contexte}: objet JSON attendu, recu {valeur}"))?;
    for champ in champs {
        if !objet.contains_key(champ) {
            return Err(format!("{contexte}: champ obligatoire absent: {champ}"));
        }
    }
    Ok(())
}

fn respecte_contrat(payload: &Value, contrat: &ContratRoute) -> Result<(), String> {
    let contexte = format!(
        "{} {} -> {}",
        contrat.methode, contrat.route, contrat.type_web
    );
    if contrat.liste {
        let elements = payload
            .as_array()
            .ok_or_else(|| format!("{contexte}: tableau JSON attendu, recu {payload}"))?;
        if elements.is_empty() {
            return Err(format!(
                "{contexte}: tableau vide, impossible de prouver les champs de l'element"
            ));
        }
        for (index, element) in elements.iter().enumerate() {
            exige_champs(
                element,
                &contrat.champs_obligatoires,
                &format!("{contexte}, element {index}"),
            )?;
        }
        Ok(())
    } else {
        exige_champs(payload, &contrat.champs_obligatoires, &contexte)
    }
}

fn respecte_tous_les_contrats(
    carte: &CarteContrats,
    methode: &str,
    route: &str,
    payload: &Value,
) -> Result<(), String> {
    for contrat in contrats_pour(carte, methode, route)? {
        respecte_contrat(payload, contrat)?;
    }
    Ok(())
}

async fn get_json(app: &axum::Router, chemin: &str) -> Result<Value, String> {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .map_err(|erreur| format!("{chemin}: routeur en echec: {erreur}"))?;
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .map_err(|erreur| format!("{chemin}: corps illisible: {erreur}"))?;
    if statut != StatusCode::OK {
        return Err(format!(
            "{chemin}: statut {statut}, corps {}",
            String::from_utf8_lossy(&octets)
        ));
    }
    serde_json::from_slice(&octets).map_err(|erreur| {
        format!(
            "{chemin}: JSON invalide ({erreur}), corps {}",
            String::from_utf8_lossy(&octets)
        )
    })
}

async fn post_json(app: &axum::Router, chemin: &str) -> Result<Value, String> {
    let reponse = app
        .clone()
        .oneshot(Request::post(chemin).body(Body::empty()).unwrap())
        .await
        .map_err(|erreur| format!("{chemin}: routeur en echec: {erreur}"))?;
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .map_err(|erreur| format!("{chemin}: corps illisible: {erreur}"))?;
    if statut != StatusCode::OK {
        return Err(format!(
            "{chemin}: statut {statut}, corps {}",
            String::from_utf8_lossy(&octets)
        ));
    }
    serde_json::from_slice(&octets).map_err(|erreur| {
        format!(
            "{chemin}: JSON invalide ({erreur}), corps {}",
            String::from_utf8_lossy(&octets)
        )
    })
}

async fn post_json_body(
    app: &axum::Router,
    chemin: &str,
    body: Value,
    statut_attendu: StatusCode,
) -> Result<Value, String> {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .map_err(|erreur| format!("{chemin}: routeur en echec: {erreur}"))?;
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .map_err(|erreur| format!("{chemin}: corps illisible: {erreur}"))?;
    if statut != statut_attendu {
        return Err(format!(
            "{chemin}: statut {statut}, attendu {statut_attendu}, corps {}",
            String::from_utf8_lossy(&octets)
        ));
    }
    serde_json::from_slice(&octets).map_err(|erreur| {
        format!(
            "{chemin}: JSON invalide ({erreur}), corps {}",
            String::from_utf8_lossy(&octets)
        )
    })
}

/// Routes sans secret, matériel ni service tiers. La vague commence par les
/// écrans les plus centraux (accueil, bibliothèque, diagnostics et réglages).
const VAGUE_INITIALE: &[(&str, &str)] = &[
    ("/devices/catalog", "/api/v1/devices/catalog"),
    ("/eq/expert-settings", "/api/v1/eq/expert-settings"),
    ("/eq/presets", "/api/v1/eq/presets"),
    ("/home", "/api/v1/home"),
    (
        "/library/albums-detailed",
        "/api/v1/library/albums-detailed?limit=10&offset=0",
    ),
    ("/library/ambiances", "/api/v1/library/ambiances"),
    ("/library/browse", "/api/v1/library/browse"),
    ("/library/history", "/api/v1/library/history?limit=10"),
    (
        "/library/history/dashboard",
        "/api/v1/library/history/dashboard?period=30d",
    ),
    ("/library/search", "/api/v1/library/search?q=contrat"),
    (
        "/library/search/acoustic/status",
        "/api/v1/library/search/acoustic/status",
    ),
    ("/library/stats", "/api/v1/library/stats"),
    (
        "/library/stats/completeness",
        "/api/v1/library/stats/completeness",
    ),
    (
        "/library/artwork/enrich-artists/status",
        "/api/v1/library/artwork/enrich-artists/status",
    ),
    (
        "/library/enrich-all/status",
        "/api/v1/library/enrich-all/status",
    ),
    ("/offline/status", "/api/v1/offline/status"),
    ("/onboarding/status", "/api/v1/onboarding/status"),
    ("/spotify-connect/status", "/api/v1/spotify-connect/status"),
    (
        "/streaming/youtube/auth/status",
        "/api/v1/streaming/youtube/auth/status",
    ),
    (
        "/system/admin/connections",
        "/api/v1/system/admin/connections",
    ),
    ("/system/admin/discovery", "/api/v1/system/admin/discovery"),
    ("/system/admin/health", "/api/v1/system/admin/health"),
    (
        "/system/background-tasks",
        "/api/v1/system/background-tasks",
    ),
    ("/system/diagnostics", "/api/v1/system/diagnostics"),
    ("/system/health/monitor", "/api/v1/system/health/monitor"),
    ("/system/scan/schedule", "/api/v1/system/scan/schedule"),
    ("/system/scan/status", "/api/v1/system/scan/status"),
    ("/system/stats", "/api/v1/system/stats"),
    ("/system/youtube/status", "/api/v1/system/youtube/status"),
];

#[tokio::test]
async fn vingt_neuf_reponses_reelles_respectent_les_champs_exiges_par_le_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(etat.backend.clone());
    let mut piste_douteuse = tune_core::db::models::Track::new("Contrat incomplet".into());
    piste_douteuse.file_path = Some("/music/contrat-incomplet.flac".into());
    pistes
        .create(&piste_douteuse)
        .expect("piste temoin sans artiste ni album");
    let app = tune_server::routes::router(etat);

    for (route_contrat, chemin_reel) in VAGUE_INITIALE {
        let payload = get_json(&app, chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
        if *route_contrat == "/library/stats/completeness" {
            assert_eq!(
                payload["doubtful_count"], 1,
                "la pastille et /metadata/doubtful doivent compter la meme piste temoin"
            );
        }
    }
}

#[tokio::test]
async fn desactiver_spotify_connect_rend_le_statut_complet_annonce_au_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let app = tune_server::routes::router(etat);
    let payload = post_json(&app, "/api/v1/spotify-connect/disable")
        .await
        .expect("reponse disable Spotify Connect");

    respecte_tous_les_contrats(&carte, "POST", "/spotify-connect/disable", &payload)
        .unwrap_or_else(|erreur| panic!("{erreur}; payload={payload}"));
}

#[tokio::test]
async fn sept_contrats_de_zone_passent_par_le_routeur_reel() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let app = tune_server::routes::router(etat);

    let creee = post_json_body(
        &app,
        "/api/v1/zones",
        serde_json::json!({"name": "Contrat zone", "output_type": "local"}),
        StatusCode::CREATED,
    )
    .await
    .expect("creation de la zone temoin");
    respecte_tous_les_contrats(&carte, "POST", "/zones", &creee)
        .unwrap_or_else(|erreur| panic!("POST /api/v1/zones: {erreur}; payload={creee}"));
    let zone_id = creee["id"].as_i64().expect("id de la zone temoin");

    // Pins et quality sont volontairement absents : leur réponse réelle
    // contredit déjà la carte et leur comportement est incomplet (#2722,
    // #2723). Les ajouter ici en maquillant seulement le JSON fabriquerait
    // deux preuves vertes pour des réglages toujours inertes.
    for (route_contrat, chemin_reel) in [
        ("/zones/{}", format!("/api/v1/zones/{zone_id}")),
        (
            "/zones/{}/audiophile",
            format!("/api/v1/zones/{zone_id}/audiophile"),
        ),
        (
            "/zones/{}/device-presets",
            format!("/api/v1/zones/{zone_id}/device-presets"),
        ),
        ("/zones/{}/eq", format!("/api/v1/zones/{zone_id}/eq")),
        ("/zones/{}/queue", format!("/api/v1/zones/{zone_id}/queue")),
        ("/zones/{}/sleep", format!("/api/v1/zones/{zone_id}/sleep")),
    ] {
        let payload = get_json(&app, &chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
    }
}

#[test]
fn la_contre_epreuve_refuse_un_champ_obligatoire_absent() {
    let contrat = ContratRoute {
        route: "/library/albums-detailed".into(),
        methode: "GET".into(),
        type_web: "reponse paginee".into(),
        liste: false,
        champs_obligatoires: vec!["items".into(), "total".into()],
    };
    let erreur = respecte_contrat(&serde_json::json!({"items": []}), &contrat)
        .expect_err("une reponse sans total doit casser le contrat");
    assert!(erreur.contains("champ obligatoire absent: total"));
}

#[test]
fn une_liste_vide_ne_peut_pas_prouver_le_contrat_de_ses_elements() {
    let contrat = ContratRoute {
        route: "/radios".into(),
        methode: "GET".into(),
        type_web: "RadioStation".into(),
        liste: true,
        champs_obligatoires: vec!["id".into(), "name".into()],
    };
    let erreur = respecte_contrat(&serde_json::json!([]), &contrat)
        .expect_err("une liste vide serait une fausse preuve des champs elementaires");
    assert!(erreur.contains("tableau vide"));
}
