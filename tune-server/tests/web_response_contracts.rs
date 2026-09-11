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
    // `/radios/search` a quitté cette liste le 11/09/2026 : le client web ne
    // l'appelle plus du tout, donc la carte ne la décrit plus et le contrat
    // n'existe plus. Le comportement de #2119 (distinguer « aucune station de
    // ce nom » d'une panne) reste gardé par `tests/radios_recherche_distinction.rs`,
    // qui joue la route sans passer par la carte — aucune couverture perdue.
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

/// `GET /library/tracks/{id}/lyrics` — la seconde dérive de #3002.
///
/// La carte exigeait `lyrics`, un champ que le serveur n'a jamais envoyé : il
/// rend `lines`. L'exigence venait de `api.ts:getTrackLyrics`, dont le type
/// annonce la forme « historique » `{lyrics, synced, source}` — et **cette
/// fonction n'a aucun appelant**. Les vrais consommateurs (`NowPlaying`,
/// `TvView`) passent par `lib/lyrics.ts:fetchTrackLyrics` et lisent
/// `data.lines`. La carte recopiait donc un type mort, et personne ne s'en
/// apercevait parce que la route n'était jouée nulle part.
///
/// Elle l'est désormais. La piste témoin porte ses paroles dans
/// `track_metadata`, ce qui emprunte l'étage 2 de la cascade — ni fichier
/// annexe, ni LRCLIB, donc aucune E/S ni réseau.
#[tokio::test]
async fn les_paroles_rendent_les_lignes_annoncees_au_web() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");

    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(etat.backend.clone());
    let mut piste = tune_core::db::models::Track::new("Chelsea Girl".into());
    piste.file_path = Some("/music/chelsea-girl.flac".into());
    let id = pistes.create(&piste).expect("piste temoin");

    let metas =
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(etat.backend.clone());
    metas
        .set(id, "lyrics", "Il etait une fois\nune carte qui mentait")
        .expect("paroles posees dans track_metadata");

    let app = tune_server::routes::router(etat);
    let payload = get_json(&app, &format!("/api/v1/library/tracks/{id}/lyrics"))
        .await
        .unwrap_or_else(|erreur| panic!("{erreur}"));

    respecte_tous_les_contrats(&carte, "GET", "/library/tracks/{}/lyrics", &payload)
        .unwrap_or_else(|erreur| panic!("{erreur}; payload={payload}"));

    // Et la forme, pas seulement la présence des clés : c'est `lines` que les
    // deux consommateurs parcourent.
    assert_eq!(payload["synced"], false, "des paroles non horodatees");
    assert_eq!(payload["source"], "tag");
    assert_eq!(
        payload["lines"]
            .as_array()
            .expect("`lines` est le tableau que NowPlaying et TvView parcourent")
            .len(),
        2
    );
    assert!(
        payload.get("lyrics").is_none(),
        "`lyrics` n'existe pas cote serveur : l'exiger etait une dette de carte, \
         pas un defaut de reponse"
    );
}

// ── La carte ne doit pas survivre au serveur qu'elle décrit ──────────────────
//
// Tout ce fichier CHARGE `docs/contrat-web.json` et lui fait confiance. Rien,
// jusqu'ici, ne vérifiait que cette carte décrit encore le serveur du jour :
// elle n'a pas bougé du 31/08 au 11/09 pendant que `api.ts` recevait 71
// commits, et elle citait encore quatre routes retirées depuis (#3662, #3637).
//
// Le défaut n'est pas qu'elle vieillisse — c'est que son vieillissement soit
// MUET. Les tests ci-dessus ne jouent que les routes de `VAGUE_INITIALE` : une
// entrée périmée ailleurs dans la carte n'est jamais interrogée, donc jamais
// démentie. On valide contre un document figé en croyant vérifier un contrat,
// ce qui est pire que ne rien vérifier : ça donne l'illusion de la preuve.
//
// CE QUE LA SONDE INTERROGE — ET LE PIÈGE QU'ELLE ÉVITE
//     Le ROUTEUR ASSEMBLÉ, jamais les sources. Le chemin soumis est celui
//     qu'Axum SERT, `nest()` appliqués, pas celui qu'une caisse DÉCLARE.
//
//     La différence n'est pas théorique, et elle a déjà produit un faux
//     diagnostic dans la relecture de ce lot : `tune-streaming-http` déclare
//     `/youtube/moods`, mais `routes/mod.rs` monte cette caisse sous
//     `.nest("/streaming", …)`, si bien que le chemin servi est
//     `/api/v1/streaming/youtube/moods` — et il rend 200. Lire la déclaration
//     et conclure « le vrai chemin est /api/v1/youtube/moods » revient à se
//     tromper de référentiel, donc à déclarer morte une route vivante.
//
//     `la_sonde_interroge_le_routeur_assemble_pas_les_chemins_declares_en_caisse`
//     garde cette propriété sur ce cas précis, dans les deux sens : le chemin
//     complet répond, le chemin interne à la caisse ne répond pas. Un témoin
//     qui ne vérifierait que le premier passerait au vert avec une sonde qui
//     lit les sources.
//
// COMMENT LA SONDE ÉVITE D'EXÉCUTER QUOI QUE CE SOIT
//     Interroger chaque route avec sa vraie méthode ferait tourner les vrais
//     gestionnaires — réseau, disque, effets de bord — pour une question qui
//     ne porte que sur l'existence du chemin. La sonde emploie donc une
//     méthode d'extension (`REPORT`) qu'aucun gestionnaire ne déclare : Axum
//     répond 405 si le CHEMIN existe, 404 sinon, sans appeler personne.
//
// POURQUOI CETTE GARDE ET PAS UNE ÉGALITÉ AVEC UNE RÉGÉNÉRATION
//     Une garde « régénérer et comparer » rougirait à chaque commit du dépôt
//     web, hors du contrôle de l'auteur de la PR serveur. Celle-ci ne rougit
//     que lorsque le serveur RETIRE un chemin que la carte cite encore —
//     exactement le cas où la carte ment. Le versant fraîcheur (le web exige
//     un champ que la carte ignore) est mesuré au préflight, qui dispose du
//     SHA web publié : `scripts/verifier-carte-web.py`.

/// Le point de montage des greffons. `router_with_plugins` monte chaque
/// greffon sous `/api/v1/ext/{nom}` ; `routes::router()` — celui que ce banc
/// construit — n'en monte AUCUN. Un chemin `/ext/…` ne peut donc jamais
/// répondre ici, et son absence ne prouve rien sur le serveur livré. La garde
/// s'en tait plutôt que d'accuser huit routes Bandcamp et deux Concerts
/// parfaitement servies : un contrôle qui crie au loup est pire qu'absent.
const PREFIXE_GREFFONS: &str = "/ext/";

/// Routes tolérées NOMMÉMENT, jamais par une règle vague. Une entrée se
/// justifie par sa cause et se retire dès que la dette est payée.
///
/// Ces deux-là sont des chemins que la carte cite et que le routeur assemblé
/// ne sert pas — c'est MESURÉ, par cette garde et par `curl` sur le .18. Ce
/// qui suit chaque entrée est le relevé, pas un correctif : la première
/// rédaction de ce fichier proposait une cause plausible et fausse pour
/// chacune, et une cause fausse coûte plus cher qu'un simple constat.
const FANTOMES_TOLERES: &[(&str, &str)] = &[
    (
        "/dj/waveform/{}",
        "#1897 — RELEVÉ. Le serveur de série ne sert RIEN sous `/api/v1/dj/…` : \
         `routes/mod.rs` l'écrit en toutes lettres depuis #917 (« the stock \
         server no longer serves /dj »). Les mêmes chemins existent, declares \
         par la caisse `plugins/tune-dj`, montee sous `/api/v1/ext/dj/…` — \
         mais seulement si le binaire est compile avec `--features dj` ET le \
         greffon installe (`plugin_dj_installed`). Sur le .18, \
         `/api/v1/ext/dj/status/1` rend 404 lui AUSSI : prefixer l'appel web \
         par `/ext` ne corrigerait rien. Ce qu'il faut trancher d'abord : le \
         greffon DJ doit-il etre livre et installe, ou l'ecran retire ?",
    ),
    (
        "/streaming/youtube/moods/{}",
        "#1897 — RELEVÉ. Le segment `streaming` est le BON : \
         `/api/v1/streaming/youtube/moods` rend 200 sur le .18, et cette garde \
         ne le signale pas. Seule la variante a parametre \
         `/streaming/youtube/moods/{params}`, qu'appelle \
         `api.ts:getYoutubeMoodPlaylists`, n'a aucune route. Le gestionnaire de \
         base rend `{\"moods\":[],\"message\":\"YouTube moods not yet \
         implemented\"}` : c'est un TALON serveur a finir, pas un chemin faux \
         cote client.",
    ),
];

/// `/streaming/{}/albums{}` → `/api/v1/streaming/1/albums`.
///
/// Un `{}` qui occupe TOUT un segment est un paramètre de chemin. Un `{}`
/// collé à la fin d'un segment vient d'une interpolation de chaîne de requête
/// (`/radios${qs ? '?' + qs : ''}`) : il ne fait pas partie du chemin, et le
/// garder fabriquerait un 404 imaginaire.
fn chemin_de_sonde(route: &str) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    for segment in route.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "{}" {
            segments.push("1".to_string());
        } else if let Some(prefixe) = segment.strip_suffix("{}") {
            if prefixe.is_empty() || prefixe.contains("{}") {
                return None;
            }
            segments.push(prefixe.to_string());
        } else if segment.contains("{}") {
            return None; // forme inattendue : se taire plutôt qu'accuser
        } else {
            segments.push(segment.to_string());
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(format!("/api/v1/{}", segments.join("/")))
}

async fn statut_de_sonde(app: &axum::Router, chemin: &str) -> StatusCode {
    let requete = Request::builder()
        .method("REPORT")
        .uri(chemin)
        .body(Body::empty())
        .expect("requete de sonde");
    app.clone()
        .oneshot(requete)
        .await
        .expect("routeur en echec")
        .status()
}

/// Le témoin qui manquait : une caisse NICHÉE, dont le chemin servi diffère
/// du chemin déclaré.
///
/// `tune-streaming-http` déclare `.route("/youtube/moods", …)`. `routes/mod.rs`
/// monte cette caisse sous `.nest("/streaming", …)`. Le chemin SERVI est donc
/// `/api/v1/streaming/youtube/moods` — mesuré à 200 sur le .18 — et le chemin
/// DÉCLARÉ, `/api/v1/youtube/moods`, ne répond pas.
///
/// Sans ce témoin, une sonde qui lirait les sources au lieu d'interroger le
/// routeur assemblé passerait au vert tout en se trompant de référentiel, et
/// déclarerait morte une route qui rend 200. C'est exactement le faux
/// diagnostic qu'a produit la relecture de ce lot.
///
/// Les DEUX sens comptent : n'éprouver que le chemin complet laisserait passer
/// une sonde qui répond « servi » à tout.
#[tokio::test]
async fn la_sonde_interroge_le_routeur_assemble_pas_les_chemins_declares_en_caisse() {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let app = tune_server::routes::router(etat);

    // Assemblés à l'exécution : une aiguille écrite en clair se trouverait
    // elle-même si la garde venait un jour à relire les sources.
    let interne = ["youtube", "moods"].join("/");
    let prefixe_nest = "streaming";

    let servi = format!("/api/v1/{prefixe_nest}/{interne}");
    assert_ne!(
        statut_de_sonde(&app, &servi).await,
        StatusCode::NOT_FOUND,
        "{servi} doit repondre : c'est le chemin que le routeur SERT, \
         `nest(\"/{prefixe_nest}\", …)` applique. Une sonde qui le rate lit les \
         sources au lieu d'interroger le routeur."
    );

    let declare = format!("/api/v1/{interne}");
    assert_eq!(
        statut_de_sonde(&app, &declare).await,
        StatusCode::NOT_FOUND,
        "{declare} est le chemin DECLARE dans la caisse, pas celui qui est \
         servi. S'il repond, la sonde ne distingue plus les deux et ce banc ne \
         prouve plus rien sur les caisses nichees."
    );
}

#[test]
fn la_sonde_distingue_un_parametre_de_chemin_d_une_chaine_de_requete() {
    assert_eq!(
        chemin_de_sonde("/zones/{}/dsp").as_deref(),
        Some("/api/v1/zones/1/dsp")
    );
    // `/radios{}` vient de `${BASE}/radios${qs ? '?' + qs : ''}` : le `{}` est
    // une chaîne de requête, pas un segment. Le traiter comme un paramètre
    // interrogerait `/api/v1/radios/1`, qui existe, et la garde raterait une
    // vraie disparition de `/radios`.
    assert_eq!(
        chemin_de_sonde("/radios{}").as_deref(),
        Some("/api/v1/radios")
    );
    assert_eq!(
        chemin_de_sonde("/streaming/{}/artists/{}/albums{}").as_deref(),
        Some("/api/v1/streaming/1/artists/1/albums")
    );
}

#[tokio::test]
async fn la_carte_web_ne_cite_que_des_routes_encore_servies() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let app = tune_server::routes::router(etat);

    // ── D'abord éprouver la SONDE, sinon la garde ne prouve rien ──
    //
    // Une sonde qui répondrait 404 partout rendrait la garde rouge en
    // permanence ; une sonde qui ne répondrait jamais 404 la rendrait verte
    // contre n'importe quelle carte. Les deux témoins sont assemblés à
    // l'exécution : une aiguille écrite en clair dans ce fichier se trouverait
    // elle-même si un jour la garde venait à lire les sources.
    let temoin_servi = format!("/api/v1/{}", ["zo", "nes"].concat());
    assert_ne!(
        statut_de_sonde(&app, &temoin_servi).await,
        StatusCode::NOT_FOUND,
        "la sonde rend 404 sur une route servie : elle mesure autre chose que \
         l'existence du chemin, et la garde qui s'en sert ne prouve rien"
    );
    let temoin_absent = format!(
        "/api/v1/{}",
        ["chemin", "qui", "n", "existe", "pas"].join("-")
    );
    assert_eq!(
        statut_de_sonde(&app, &temoin_absent).await,
        StatusCode::NOT_FOUND,
        "la sonde ne rend jamais 404 : elle serait verte contre une carte \
         entierement fausse"
    );

    let mut deja_sondes = std::collections::BTreeSet::new();
    let mut fantomes: Vec<String> = Vec::new();
    let mut toleres_sans_objet: Vec<String> = Vec::new();

    for contrat in &carte.routes {
        if contrat.route.starts_with(PREFIXE_GREFFONS) {
            continue;
        }
        let Some(chemin) = chemin_de_sonde(&contrat.route) else {
            continue;
        };
        if !deja_sondes.insert(chemin.clone()) {
            continue;
        }
        let tolere = FANTOMES_TOLERES
            .iter()
            .any(|(route, _)| *route == contrat.route);
        let absente = statut_de_sonde(&app, &chemin).await == StatusCode::NOT_FOUND;
        match (tolere, absente) {
            (false, true) => fantomes.push(format!("{} (sonde {chemin})", contrat.route)),
            // La dette est payée : le dire, sinon la liste fossilise une
            // exception sans objet et finit par tout tolérer.
            (true, false) => toleres_sans_objet.push(contrat.route.clone()),
            _ => {}
        }
    }

    // Une tolérance dont la route a quitté la carte est morte elle aussi —
    // c'est ce qui arrivera quand le client web sera corrigé, puisque la route
    // changera de nom (`/dj/…` → `/ext/dj/…`).
    let routes_de_la_carte: std::collections::BTreeSet<&str> =
        carte.routes.iter().map(|c| c.route.as_str()).collect();
    for (route, _) in FANTOMES_TOLERES {
        if !routes_de_la_carte.contains(route) {
            toleres_sans_objet.push((*route).to_string());
        }
    }
    assert!(
        toleres_sans_objet.is_empty(),
        "FANTOMES_TOLERES garde {} entree(s) sans objet — la ou les retirer :\n    {}",
        toleres_sans_objet.len(),
        toleres_sans_objet.join("\n    ")
    );

    assert!(
        fantomes.is_empty(),
        "docs/contrat-web.json cite {} route(s) que le serveur ne sert plus :\n    {}\n\n\
         Regenerer la carte : scripts/web-contract-map.py --web <tune-web-client> \
         -o docs/contrat-web.json",
        fantomes.len(),
        fantomes.join("\n    ")
    );
}
