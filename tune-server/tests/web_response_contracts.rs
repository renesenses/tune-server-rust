//! Première vague du banc runtime web ↔ serveur (#1897).
//!
//! La carte est générée depuis les types réellement employés par le client
//! web. Ces tests ne recopient donc pas une seconde fois les champs attendus :
//! ils chargent `docs/contrat-web.json`, appellent le vrai routeur Axum et
//! confrontent la réponse à la carte commitée.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
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

async fn mutation_json(
    app: &axum::Router,
    methode: axum::http::Method,
    chemin: &str,
    body: Value,
    statut_attendu: StatusCode,
) -> Result<Value, String> {
    let reponse = app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
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
    ("/zones", "/api/v1/zones"),
];

#[tokio::test]
async fn trente_reponses_reelles_respectent_les_champs_exiges_par_le_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(etat.backend.clone());
    let mut piste_douteuse = tune_core::db::models::Track::new("Contrat incomplet".into());
    piste_douteuse.file_path = Some("/music/contrat-incomplet.flac".into());
    pistes
        .create(&piste_douteuse)
        .expect("piste temoin sans artiste ni album");
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(etat.backend.clone());
    zones
        .create("Zone du contrat", Some("browser"), Some("browser-contract"))
        .expect("zone temoin pour prouver le contrat de liste");
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
async fn objets_persistes_respectent_leurs_contrats_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(etat.backend.clone());
    let piste_id = pistes
        .create(&tune_core::db::models::Track::new(
            "Piste du contrat persiste".into(),
        ))
        .expect("creation de la piste temoin");
    let app = tune_server::routes::router(etat);

    let playlist = mutation_json(
        &app,
        Method::POST,
        "/api/v1/playlists",
        serde_json::json!({"name": "Contrat initial", "description": "Temoin"}),
        StatusCode::CREATED,
    )
    .await
    .expect("creation de la playlist temoin");
    respecte_tous_les_contrats(&carte, "POST", "/playlists", &playlist)
        .unwrap_or_else(|erreur| panic!("POST /api/v1/playlists: {erreur}; payload={playlist}"));
    let playlist_id = playlist["id"].as_i64().expect("id de playlist");

    let playlist = mutation_json(
        &app,
        Method::PUT,
        &format!("/api/v1/playlists/{playlist_id}"),
        serde_json::json!({"name": "Contrat renomme"}),
        StatusCode::OK,
    )
    .await
    .expect("mise a jour de la playlist temoin");
    respecte_tous_les_contrats(&carte, "PUT", "/playlists/{}", &playlist).unwrap_or_else(
        |erreur| panic!("PUT /api/v1/playlists/{{id}}: {erreur}; payload={playlist}"),
    );
    assert_eq!(playlist["name"], "Contrat renomme");

    let playlist = mutation_json(
        &app,
        Method::POST,
        &format!("/api/v1/playlists/{playlist_id}/tracks"),
        serde_json::json!({"track_ids": [piste_id]}),
        StatusCode::CREATED,
    )
    .await
    .expect("ajout de la piste temoin");
    respecte_tous_les_contrats(&carte, "POST", "/playlists/{}/tracks", &playlist).unwrap_or_else(
        |erreur| panic!("POST /playlists/{{id}}/tracks: {erreur}; payload={playlist}"),
    );
    assert_eq!(playlist["track_count"], 1);

    for (route_contrat, chemin_reel) in [
        ("/playlists", "/api/v1/playlists".to_string()),
        ("/playlists/{}", format!("/api/v1/playlists/{playlist_id}")),
        (
            "/playlists/{}/tracks",
            format!("/api/v1/playlists/{playlist_id}/tracks"),
        ),
    ] {
        let payload = get_json(&app, &chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
    }

    let radio = mutation_json(
        &app,
        Method::POST,
        "/api/v1/radios",
        serde_json::json!({
            "name": "Radio contrat",
            "stream_url": "https://example.invalid/contrat.aac",
            "genre": "Test"
        }),
        StatusCode::CREATED,
    )
    .await
    .expect("creation de la radio temoin");
    respecte_tous_les_contrats(&carte, "POST", "/radios", &radio)
        .unwrap_or_else(|erreur| panic!("POST /api/v1/radios: {erreur}; payload={radio}"));
    let radio_id = radio["id"].as_i64().expect("id de radio");

    let radio = mutation_json(
        &app,
        Method::PUT,
        &format!("/api/v1/radios/{radio_id}"),
        serde_json::json!({"favorite": true}),
        StatusCode::OK,
    )
    .await
    .expect("mise a jour de la radio temoin");
    respecte_tous_les_contrats(&carte, "PUT", "/radios/{}", &radio)
        .unwrap_or_else(|erreur| panic!("PUT /api/v1/radios/{{id}}: {erreur}; payload={radio}"));
    assert_eq!(radio["favorite"], true);

    for (route_contrat, chemin_reel) in [
        ("/radios{}", "/api/v1/radios".to_string()),
        ("/radios/{}", format!("/api/v1/radios/{radio_id}")),
    ] {
        let payload = get_json(&app, &chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
    }

    let tag = mutation_json(
        &app,
        Method::POST,
        "/api/v1/tags",
        serde_json::json!({"name": "Tag contrat", "color": "#123456"}),
        StatusCode::CREATED,
    )
    .await
    .expect("creation du tag temoin");
    respecte_tous_les_contrats(&carte, "POST", "/tags", &tag)
        .unwrap_or_else(|erreur| panic!("POST /api/v1/tags: {erreur}; payload={tag}"));
    let tag_id = tag["id"].as_i64().expect("id de tag");

    let ajout = mutation_json(
        &app,
        Method::POST,
        &format!("/api/v1/tags/{tag_id}/items/batch"),
        serde_json::json!({"item_type": "track", "item_ids": [piste_id]}),
        StatusCode::OK,
    )
    .await
    .expect("etiquetage de la piste temoin");
    respecte_tous_les_contrats(&carte, "POST", "/tags/{}/items/batch", &ajout).unwrap_or_else(
        |erreur| panic!("POST /tags/{{id}}/items/batch: {erreur}; payload={ajout}"),
    );

    for (route_contrat, chemin_reel) in [
        ("/tags/{}", "/api/v1/tags?item_type=track".to_string()),
        ("/tags/search", "/api/v1/tags/search?q=contrat".to_string()),
        (
            "/tags/for/{}/{}",
            format!("/api/v1/tags/for/track/{piste_id}"),
        ),
        ("/tags/{}/albums", format!("/api/v1/tags/{tag_id}/albums")),
    ] {
        let payload = get_json(&app, &chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
    }
}

#[tokio::test]
async fn smart_collections_conservent_la_limite_du_contrat_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let app = tune_server::routes::router(etat);

    let collection = mutation_json(
        &app,
        Method::POST,
        "/api/v1/library/smart-collections",
        serde_json::json!({
            "name": "Contrat borne",
            "description": "Collection témoin",
            "icon": "folder",
            "color": "#123456",
            "rules": [],
            "match_mode": "all",
            "sort_by": "title",
            "sort_order": "asc",
            "max_limit": 7
        }),
        StatusCode::CREATED,
    )
    .await
    .expect("creation de la smart collection temoin");
    respecte_tous_les_contrats(&carte, "POST", "/library/smart-collections", &collection)
        .unwrap_or_else(|erreur| panic!("POST smart collection: {erreur}; payload={collection}"));
    assert_eq!(collection["max_limit"], 7);
    assert!(collection["created_at"].is_string());
    let id = collection["id"].as_i64().expect("id de smart collection");

    let collection = mutation_json(
        &app,
        Method::PUT,
        &format!("/api/v1/library/smart-collections/{id}"),
        serde_json::json!({"name": "Contrat borne relu", "max_limit": 3}),
        StatusCode::OK,
    )
    .await
    .expect("mise a jour de la smart collection temoin");
    respecte_tous_les_contrats(&carte, "PUT", "/library/smart-collections/{}", &collection)
        .unwrap_or_else(|erreur| panic!("PUT smart collection: {erreur}; payload={collection}"));
    assert_eq!(collection["max_limit"], 3);

    for (route_contrat, chemin_reel) in [
        (
            "/library/smart-collections",
            "/api/v1/library/smart-collections".to_string(),
        ),
        (
            "/library/smart-collections/{}",
            format!("/api/v1/library/smart-collections/{id}"),
        ),
    ] {
        let payload = get_json(&app, &chemin_reel)
            .await
            .unwrap_or_else(|erreur| panic!("{erreur}"));
        respecte_tous_les_contrats(&carte, "GET", route_contrat, &payload)
            .unwrap_or_else(|erreur| panic!("{chemin_reel}: {erreur}; payload={payload}"));
    }

    let preview = mutation_json(
        &app,
        Method::POST,
        "/api/v1/library/smart-collections/preview",
        serde_json::json!({"rules": [], "max_limit": 1}),
        StatusCode::OK,
    )
    .await
    .expect("preview de la smart collection temoin");
    respecte_tous_les_contrats(
        &carte,
        "POST",
        "/library/smart-collections/preview",
        &preview,
    )
    .unwrap_or_else(|erreur| panic!("POST preview smart collection: {erreur}; payload={preview}"));
}

#[tokio::test]
async fn les_alertes_de_sante_sont_la_liste_annoncee_au_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");

    // Produit une vraie alerte sans dépendre de la mémoire ou du disque de la
    // machine qui exécute le test. Quinze erreurs récentes dépassent le seuil
    // du moniteur et garantissent une liste non vide, nécessaire pour prouver
    // aussi les champs de chaque élément du contrat TypeScript.
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("horloge système")
        .as_secs();
    etat.health_monitor
        .check_error_spike(&[maintenant; 15])
        .await;

    let app = tune_server::routes::router(etat);
    let payload = get_json(&app, "/api/v1/system/health/alerts")
        .await
        .expect("réponse des alertes de santé");

    respecte_tous_les_contrats(&carte, "GET", "/system/health/alerts", &payload)
        .unwrap_or_else(|erreur| panic!("{erreur}; payload={payload}"));
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
fn la_contre_epreuve_refuse_l_ancienne_enveloppe_des_alertes() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let ancienne_reponse = serde_json::json!({
        "alerts": [{
            "timestamp": "2026-08-29T00:00:00Z",
            "level": "warning",
            "category": "errors",
            "message": "alerte témoin"
        }]
    });

    let erreur =
        respecte_tous_les_contrats(&carte, "GET", "/system/health/alerts", &ancienne_reponse)
            .expect_err("une enveloppe objet ne doit pas satisfaire un contrat de liste");
    assert!(erreur.contains("tableau JSON attendu"));
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
