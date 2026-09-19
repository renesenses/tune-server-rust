//! #4362 — une piste dont le SERVEUR est absent ne part pas au renderer.
//!
//! # Le défaut que ce fichier ferme
//!
//! Le 17/09/2026 sur le `.18`, Asset n'était pas lancé sur le Mac Studio : le
//! port 26125 refusait la connexion, et l'écran Bibliothèque affichait
//! « Serveur absent » sur les 27 albums de ce serveur. Lancer l'un d'eux vers
//! l'Eversolo écrivait au journal `upnp_url_de_lecture_lue_dans_l_instantane`,
//! puis `output_play_sent`, puis `orchestrator_play` — **aucune erreur** — la
//! zone passait « en lecture », et il n'y avait aucun son.
//!
//! Tune SAVAIT. La connaissance vit dans le registre durable `media_servers`
//! (#2219 phase 1) et dans la qualification qui en tire `presence` /
//! `proposable` — c'est elle que `GET /network/media-servers` publie, et c'est
//! de là que vient le badge. Le chemin de lecture ne la consultait pas.
//!
//! # Ce que ce fichier garde, et dans quel ordre
//!
//! Le témoin ne fabrique **aucune** décision : il pose l'ÉTAT (une observation
//! datée de deux jours, écrite par l'API du registre), puis il fait constater
//! cet état **par la route que la bibliothèque interroge**, puis il demande la
//! lecture **par la route que le bouton Lecture appelle**. La décision, entre
//! les deux, est celle du produit.
//!
//! 1. **Témoin de contraste** — serveur vu à l'instant : la lecture passe. Sans
//!    cette étape, un refus posé sans condition validerait tout le reste en
//!    cassant chaque lecture UPnP du produit.
//! 2. **Le badge** : la route dit `presence: "absent"`, `proposable: false`.
//!    C'est exactement ce que l'écran montre.
//! 3. **La lecture** : la même piste, la même zone réseau — refusée, et le
//!    refus NOMME le serveur et son adresse.
//!
//! # Ce que ce fichier NE fait pas
//!
//! Il ne coupe aucun port et ne sonde rien : le banc HTTP reste debout pendant
//! l'étape 3. C'est délibéré. Le sujet de #4362 n'est pas « savoir sonder »,
//! c'est « consulter ce qu'on sait déjà » — et un témoin qui dépendrait d'un
//! accès réseau réel n'aurait rien à faire sur une machine de compilation.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

const UDN: &str = "uuid:4362C2D5-E2C3-B734-0-0123456789ab";
const OBJECT_ID: &str = "d6120941636376083059-co4E8D6A18CD1AC698";
const NOM_DU_SERVEUR: &str = "Asset UPnP: Banc-4362";

fn echapper(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn enveloppe(didl: &str, n: usize) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:BrowseResponse><Result>{}</Result><NumberReturned>{n}</NumberReturned>\
         <TotalMatches>{n}</TotalMatches><UpdateID>1</UpdateID>\
         </u:BrowseResponse></s:Body></s:Envelope>",
        echapper(&format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{didl}</DIDL-Lite>"
        ))
    )
}

/// Rend l'URL de contrôle du banc ContentDirectory.
async fn lever_le_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse"));
    let pour_le_service = base.clone();

    let app = Router::new()
        .route(
            "/control",
            post(move |corps: String| {
                let base = pour_le_service.clone();
                async move {
                    let object_id = corps
                        .split_once("<ObjectID>")
                        .and_then(|(_, r)| r.split_once("</ObjectID>"))
                        .map(|(v, _)| v.to_string())
                        .unwrap_or_default();
                    let depart: usize = corps
                        .split_once("<StartingIndex>")
                        .and_then(|(_, r)| r.split_once("</StartingIndex>"))
                        .and_then(|(v, _)| v.trim().parse().ok())
                        .unwrap_or(0);
                    if depart > 0 || object_id != "0" {
                        return enveloppe("", 0);
                    }
                    let didl = format!(
                        "<item id=\"{OBJECT_ID}\" parentID=\"0\" restricted=\"0\">\
                         <dc:title>Pretty Fly</dc:title><dc:creator>Mono Puff</dc:creator>\
                         <upnp:artist>Mono Puff</upnp:artist>\
                         <upnp:album>It's Fun To Steal</upnp:album>\
                         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
                         <res duration=\"0:03:12.000\" size=\"21911291\" bitsPerSample=\"16\" \
                         sampleFrequency=\"44100\" nrAudioChannels=\"2\" \
                         protocolInfo=\"http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC\">\
                         {base}/content/{OBJECT_ID}.flac</res></item>"
                    );
                    enveloppe(&didl, 1)
                }
            }),
        )
        .route(
            "/content/{fichier}",
            get(|| async { ([("content-type", "audio/flac")], vec![0u8; 4096]) }),
        );

    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("{base}/control")
}

async fn inscrire(etat: &AppState, control: &str) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: UDN.to_string(),
        name: NOM_DU_SERVEUR.into(),
        manufacturer: "Illustrate Ltd".into(),
        model: "Asset UPnP Server".into(),
        location: control.to_string(),
        content_directory_url: control.to_string(),
        host: "192.168.1.41".into(),
        port: 26_125,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    };
    etat.media_servers
        .lock()
        .await
        .insert(UDN.to_string(), info);
}

async fn appel(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let reponse = app.clone().oneshot(req).await.expect("routeur");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

async fn jouer(app: &Router, zone: i64, track_id: i64) -> (StatusCode, String) {
    appel(
        app,
        Request::post(format!("/api/v1/zones/{zone}/play"))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "track_id": track_id }).to_string()))
            .unwrap(),
    )
    .await
}

/// Ce que la bibliothèque lit pour afficher — ou non — « Serveur absent ».
async fn badge(app: &Router) -> Value {
    let (statut, corps) = appel(
        app,
        Request::get("/api/v1/network/media-servers")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let liste: Value = serde_json::from_str(&corps).expect("JSON");
    liste["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["id"] == json!(UDN))
        .unwrap_or_else(|| panic!("le banc doit figurer au registre — {corps}"))
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn un_serveur_absent_refuse_la_lecture_au_lieu_de_jouer_du_silence() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let control = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat.clone());

    // Une zone RÉSEAU : c'est la sortie du constat (Eversolo DMP-A8), celle où
    // Tune ne voit pas passer un octet et ne peut donc rien constater après
    // coup. Si le refus ne tombe pas AVANT l'envoi, il ne tombe jamais.
    etat.backend
        .execute_batch(
            "INSERT INTO zones (id, name, output_type, output_device_id) \
               VALUES (3, 'Chaine', 'dlna', 'uuid:renderer-du-banc');",
        )
        .expect("une zone réseau sur une base neuve");

    // --- Indexation : une vraie ligne, posée par la vraie route ---
    let (statut, corps) = appel(
        &app,
        Request::post(format!("/api/v1/network/media-servers/{UDN}/indexer"))
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let bilan: Value = serde_json::from_str(&corps).expect("JSON");
    assert_eq!(bilan["pistes"]["ajoutees"], json!(1), "{bilan}");

    let ligne = etat
        .backend
        .query_one(
            "SELECT id, source_id FROM tracks WHERE source = 'upnp'",
            &[],
        )
        .expect("requête")
        .expect("une ligne indexée");
    let track_id = ligne[0].as_i64().expect("id");
    let source_id = ligne[1].as_string().unwrap_or_default();
    assert!(
        source_id.starts_with(&format!("{UDN}|")),
        "le lien entre la piste et son serveur EST le préfixe du `source_id` — \
         sans lui la lecture n'a aucun moyen de savoir de qui elle parle \
         (lu : {source_id})"
    );

    // --- 1. TÉMOIN DE CONTRASTE : serveur vu à l'instant, la lecture passe ---
    //
    // Cet appel fait aussi le travail d'amorçage : c'est lui qui écrit
    // l'observation « présent » dans le registre durable.
    let present = badge(&app).await;
    assert_eq!(
        present["presence"],
        json!("present"),
        "amorçage : le banc vient d'être vu — {present}"
    );
    let (_statut, corps_present) = jouer(&app, 3, track_id).await;
    assert!(
        !corps_present.contains("ne répond pas"),
        "un serveur PRÉSENT doit continuer de jouer : un refus sans condition \
         casserait toutes les lectures UPnP du produit — {corps_present}"
    );

    // --- 2. Le serveur s'éteint : deux jours de silence au registre ---
    //
    // On écrit l'ÉTAT par l'API du registre, et rien de plus : ni verdict, ni
    // marquage « absent » posé à la main. La bascule est celle du produit.
    etat.media_servers.lock().await.remove(UDN);
    tune_core::db::media_server_repo::MediaServerRepo::with_backend(etat.backend.clone())
        .enregistrer_observation_a(
            &tune_core::db::media_server_repo::ObservationServeurRecue {
                udn: UDN.into(),
                name: NOM_DU_SERVEUR.into(),
                manufacturer: Some("Illustrate Ltd".into()),
                model: Some("Asset UPnP Server".into()),
                device_type: "upnp_media_server".into(),
                location: control.clone(),
                content_directory_url: Some(control.clone()),
                host: Some("192.168.1.41".into()),
                port: Some(26_125),
                max_age_secs: Some(1_800),
            },
            &tune_core::db::media_server_repo::horodatage_il_y_a(2 * 86_400),
        )
        .expect("observation datée d'il y a deux jours");

    // --- 3. LE BADGE : ce que la bibliothèque affiche ---
    let absent = badge(&app).await;
    assert_eq!(
        absent["presence"],
        json!("absent"),
        "c'est cette valeur qui produit « Serveur absent » à l'écran — {absent}"
    );
    assert_eq!(absent["proposable"], json!(false), "{absent}");

    // --- 4. LA LECTURE : refusée, et le refus NOMME le serveur ---
    let (statut, corps) = jouer(&app, 3, track_id).await;
    assert!(
        statut.is_client_error() || statut.is_server_error(),
        "la lecture d'une piste dont le serveur est absent doit ÉCHOUER, pas \
         partir au renderer et se taire (statut {statut}) — {corps}"
    );
    for attendu in [NOM_DU_SERVEUR, "192.168.1.41", "ne répond pas"] {
        assert!(
            corps.contains(attendu),
            "le refus doit dire « {attendu} » : « lecture impossible » sans \
             nommer QUI laisse l'auditeur devant sa bibliothèque entière — {corps}"
        );
    }

    // --- 5. « TOUT LIRE » SUR UNE FILE MIXTE (point 2 de #4362) ---
    //
    // Même piste injoignable EN TÊTE, suivie d'une piste locale. Avant le
    // correctif, la route rendait le refus de la première et rien ne partait :
    // toute la file était perdue pour une piste. Elle doit maintenant ENJAMBER
    // la piste injoignable et partir sur la suivante.
    //
    // La piste locale pointe un fichier absent : ce qui compte ici n'est pas
    // qu'elle SONNE, c'est que la route ait quitté la piste injoignable — le
    // refus « ne répond pas » ne doit plus être la réponse, et le curseur de
    // la file doit être sur la position 1.
    etat.backend
        .execute_batch(
            "INSERT INTO tracks (id, title, file_path, source) \
               VALUES (9001, 'Piste locale du banc', '/banc-4362/absent.flac', 'local');",
        )
        .expect("une piste locale");
    let (_statut, corps) = appel(
        &app,
        Request::post("/api/v1/zones/3/play")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "track_ids": [track_id, 9001] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert!(
        !corps.contains("ne répond pas"),
        "« Tout lire » sur une file mixte ne doit plus buter sur la première \
         piste injoignable : elle est enjambée — {corps}"
    );
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(etat.backend.clone())
        .get_ordered(3)
        .expect("file lisible");
    assert_eq!(file.len(), 2, "la file garde les deux pistes");
    let courante = file
        .iter()
        .find(|e| e.is_current)
        .map(|e| (e.position, e.track_id));
    assert_eq!(
        courante,
        Some((1, Some(9001))),
        "le curseur doit être sur la piste jouable, pas sur l'injoignable"
    );

    // --- 6. Rien de jouable après : le refus ordinaire parle, comme avant ---
    let (statut, corps) = appel(
        &app,
        Request::post("/api/v1/zones/3/play")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "track_ids": [track_id] }).to_string()))
            .unwrap(),
    )
    .await;
    assert!(
        (statut.is_client_error() || statut.is_server_error()) && corps.contains("ne répond pas"),
        "une file sans aucune piste jouable garde le refus nommé (statut {statut}) — {corps}"
    );
}
