//! Phase 3 — une piste INDEXÉE depuis un serveur UPnP **se joue**.
//!
//! # Le défaut que ce fichier ferme
//!
//! La phase 2 pose des lignes `source = 'upnp'` dans `tracks`, avec pour
//! `source_id` un **condensat d'identité** (`<udn>|<hex>`) — ni l'`ObjectID` ni
//! l'URL de `res` n'étant stables. L'URL de lecture, elle, vit dans
//! l'instantané : `track_metadata.upnp_res_url`.
//!
//! Deux choses empêchaient ces lignes de jouer, et il fallait les deux :
//!
//! 1. **L'aiguillage.** `PlayRequest.source` vient du CORPS de la demande
//!    (`routes/playback.rs`), et le bouton Lecture n'envoie que `{ track_id }`.
//!    La demande tombait donc dans `resolve_local_track`, qui cherche un
//!    `file_path` que la ligne n'a pas. La source est une propriété de la
//!    LIGNE : `resolve_stream` la lit désormais là où elle vit.
//! 2. **L'adresse.** `resolve_direct_url` lisait l'URL dans `source_id`, où vit
//!    maintenant l'identité. Elle la lit désormais dans l'instantané quand la
//!    demande n'en nomme aucune.
//!
//! Sans ça, la bibliothèque montrait des pistes qui **ne jouaient pas**.
//!
//! # Ce que ce fichier garde
//!
//! 1. **Navigateur** : la lecture aboutit et rend une `stream_url` — et c'est
//!    une adresse **de Tune**, pas celle du serveur distant (#2076 : un onglet
//!    à qui l'on tend une URL tierce réécrit le chemin et perd le domaine).
//! 2. **Locale** et **réseau** : la résolution trouve l'adresse distante. Ce
//!    qu'un banc sans haut-parleur ni renderer peut prouver, c'est que la
//!    RÉSOLUTION aboutit — et elle le dit au journal, au niveau `INFO`, avec
//!    l'URL retenue. Un banc ne peut pas prouver qu'un DAC a fait du bruit ;
//!    il peut prouver que Tune a cessé de chercher un fichier qui n'existe pas.
//! 3. **OAAT mord toujours.** C'est la garde la plus importante de ce lot :
//!    brancher la lecture ne doit surtout pas rouvrir un chemin vers le
//!    silence. La quatrième zone reçoit la même piste et se voit refuser, avec
//!    son motif — alors même que le journal prouve que l'URL a bien été
//!    trouvée pour elle aussi.
//!
//! # Pourquoi un binaire à lui seul, et un seul test dedans
//!
//! `tracing` met en cache, pour tout le processus, la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'un binaire
//! qui lance des tests en parallèle rend des captures vides sans prévenir
//! (leçon de `journal_descriptif_illisible.rs`, reprise par
//! `refus_de_lecture_audible_3733.rs`). Ce fichier ne contient donc **qu'un**
//! test, qui enchaîne ses quatre zones dans l'ordre.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

const UDN: &str = "uuid:258FC2D5-E2C3-B734-0-123456789abc";
const OBJECT_ID: &str = "d6120941636376083059-co4E8D6A18CD1AC698";

// ---------------------------------------------------------------------------
// Capture du journal
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("journal lisible")).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("journal accessible")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// Le banc : un serveur ContentDirectory ET le fichier audio qu'il publie
// ---------------------------------------------------------------------------

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

/// Rend `(url_de_controle, base)`.
async fn lever_le_banc() -> (String, String) {
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
                         <dc:title>Wonderwall</dc:title><dc:creator>Oasis</dc:creator>\
                         <upnp:artist>Oasis</upnp:artist>\
                         <upnp:album>Morning Glory</upnp:album>\
                         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
                         <res duration=\"0:04:18.000\" size=\"31911291\" bitsPerSample=\"16\" \
                         sampleFrequency=\"44100\" nrAudioChannels=\"2\" \
                         protocolInfo=\"http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC\">\
                         {base}/content/{OBJECT_ID}.flac</res></item>"
                    );
                    enveloppe(&didl, 1)
                }
            }),
        )
        // Le fichier lui-même : le banc n'est pas qu'un catalogue, il SERT les
        // octets. Sans cela, « la piste joue » ne serait qu'une affirmation.
        .route(
            "/content/{fichier}",
            get(|| async { ([("content-type", "audio/flac")], vec![0u8; 4096]) }),
        );

    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("{base}/control"), base)
}

async fn inscrire(etat: &AppState, control: &str) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: UDN.to_string(),
        name: "Banc UPnP".into(),
        manufacturer: "Illustrate Ltd".into(),
        model: "Asset UPnP Server".into(),
        location: control.to_string(),
        content_directory_url: control.to_string(),
        host: "127.0.0.1".into(),
        port: 0,
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

#[tokio::test(flavor = "multi_thread")]
async fn une_piste_indexee_se_joue_et_oaat_refuse_toujours() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let (control, base) = lever_le_banc().await;
    inscrire(&etat, &control).await;
    let app = tune_server::routes::router(etat.clone());

    // Les quatre sorties de D4, chacune sa zone.
    etat.backend
        .execute_batch(
            "INSERT INTO zones (id, name, output_type) VALUES (1, 'Onglet', 'browser');\
             INSERT INTO zones (id, name, output_type, output_device_id) \
               VALUES (2, 'Salon', 'local', 'local:banc');\
             INSERT INTO zones (id, name, output_type, output_device_id) \
               VALUES (3, 'Chaine', 'dlna', 'uuid:renderer-du-banc');\
             INSERT INTO zones (id, name, output_type, output_device_id) \
               VALUES (4, 'Endpoint', 'oaat', 'oaat:banc');",
        )
        .expect("quatre zones sur une base neuve");

    // --- Indexation ---
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

    // La ligne posée : son `source_id` EST le condensat d'identité, pas une
    // URL. C'est tout le problème que la phase 3 résout — on l'exige ici, sans
    // quoi le reste du témoin ne prouverait rien.
    let ligne = etat
        .backend
        .query_one(
            "SELECT id, source, source_id FROM tracks WHERE source = 'upnp'",
            &[],
        )
        .expect("requête")
        .expect("une ligne indexée");
    let track_id = ligne[0].as_i64().expect("id");
    let source_id = ligne[2].as_string().unwrap_or_default();
    assert!(
        source_id.starts_with(UDN) && !source_id.starts_with("http"),
        "`source_id` doit porter l'identité, pas l'URL — sinon la phase 3 \
         n'aurait rien à brancher (lu : {source_id})"
    );

    // --- 1. NAVIGATEUR : la lecture aboutit, et l'adresse rendue est celle de Tune ---
    let (statut, corps) = jouer(&app, 1, track_id).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "la lecture sur une zone navigateur doit aboutir — corps {corps}"
    );
    let zone: Value = serde_json::from_str(&corps).expect("JSON");
    let flux = zone["stream_url"].as_str().unwrap_or_default();
    assert!(
        !flux.is_empty(),
        "la zone navigateur doit recevoir une `stream_url` : sans elle \
         l'onglet n'a rien à lire — corps {corps}"
    );
    assert!(
        !flux.contains(base.trim_start_matches("http://")),
        "la `stream_url` rendue à l'onglet doit être une adresse DE TUNE, pas \
         celle du serveur distant (#2076 : le client réécrit le chemin et perd \
         le domaine, puis reçoit le repli SPA) — lu : {flux}"
    );

    // La réponse DIT la dégradation de cette sortie (D4, reste).
    let avertissements = zone["avertissements"]
        .as_array()
        .unwrap_or_else(|| panic!("la zone navigateur doit porter ses avertissements — {corps}"))
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        avertissements.contains("Le DSP de la zone"),
        "la zone navigateur joue sans DSP : il faut le DIRE — lu : {avertissements}"
    );

    // --- 2. LOCALE et 3. RÉSEAU : la résolution trouve l'adresse distante ---
    let (_statut, corps_local) = jouer(&app, 2, track_id).await;
    let (_statut, corps_reseau) = jouer(&app, 3, track_id).await;
    for (ou, corps) in [("locale", &corps_local), ("réseau", &corps_reseau)] {
        assert!(
            !corps.contains("no tracks to play"),
            "sortie {ou} : la demande ne doit plus mourir avant la résolution — {corps}"
        );
        assert!(
            !corps.contains("track has no file_path"),
            "sortie {ou} : la demande ne doit plus chercher un FICHIER — c'est \
             le défaut exact que la phase 3 ferme : {corps}"
        );
    }

    // …et chacune dit SA dégradation, pas celle de la voisine.
    let local: Value = serde_json::from_str(&corps_local).expect("JSON");
    let dits_local = local["avertissements"].to_string();
    assert!(
        dits_local.contains("ReplayGain"),
        "la sortie locale joue sans ReplayGain : il faut le dire — {dits_local}"
    );
    assert!(
        dits_local.contains("saut dans la piste"),
        "la sortie locale relance la piste au début sur un saut : il faut le \
         dire — {dits_local}"
    );
    assert!(
        !dits_local.contains("Le DSP de la zone"),
        "la sortie locale applique BIEN le DSP : lui prêter le défaut du \
         réseau serait un mensonge de plus, dans l'autre sens — {dits_local}"
    );

    let reseau: Value = serde_json::from_str(&corps_reseau).expect("JSON");
    let dits_reseau = reseau["avertissements"].to_string();
    assert!(
        dits_reseau.contains("Le DSP de la zone"),
        "la sortie réseau joue sans DSP : il faut le dire — {dits_reseau}"
    );
    assert!(
        !dits_reseau.contains("ReplayGain"),
        "le manque de ReplayGain est un défaut de la sortie LOCALE — {dits_reseau}"
    );

    // --- 4. OAAT : le refus MORD TOUJOURS ---
    //
    // La garde la plus importante de ce lot. Brancher la lecture ne doit pas
    // rouvrir le chemin du silence.
    let (statut, corps) = jouer(&app, 4, track_id).await;
    assert!(
        statut.is_client_error() || statut.is_server_error(),
        "OAAT doit continuer de REFUSER une piste indexée : la brancher ne \
         doit pas rouvrir un chemin vers le silence (statut {statut}, corps {corps})"
    );
    for attendu in ["OAAT", "WAV", "silence"] {
        assert!(
            corps.contains(attendu),
            "le motif du refus OAAT doit toujours dire « {attendu} » — corps {corps}"
        );
    }

    // --- 5. CONTRE-ÉPREUVE : une piste LOCALE n'hérite d'aucun avertissement ---
    //
    // Sans elle, un champ `avertissements` posé sans condition passerait tous
    // les contrôles ci-dessus en salissant chaque lecture du produit. Les
    // dégradations de D4 sont celles d'une piste DISTANTE, et d'elle seule.
    let locale = tune_core::db::track_repo::TrackRepo::with_backend(etat.backend.clone());
    let mut piste_locale = tune_core::db::models::Track::new("Une piste à moi".into());
    piste_locale.file_path = Some("/musique/a-moi.flac".into());
    let id_local = locale.create(&piste_locale).expect("piste locale");
    let (_statut, corps_local_pur) = jouer(&app, 2, id_local).await;
    assert!(
        !corps_local_pur.contains("avertissements"),
        "une piste locale ne doit porter AUCUN avertissement de D4 : sinon le \
         champ ne dit plus rien de particulier — {corps_local_pur}"
    );

    // --- Le journal : l'URL vient bien de l'INSTANTANÉ, pour les quatre zones ---
    let journal = capture.texte();
    let lues: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("upnp_url_de_lecture_lue_dans_l_instantane"))
        .collect();
    assert_eq!(
        lues.len(),
        4,
        "les QUATRE zones doivent avoir retrouvé l'adresse dans l'instantané — \
         y compris celle qui refuse ensuite, sans quoi on ne saurait pas si \
         OAAT refuse pour la bonne raison.\njournal :\n{journal}"
    );
    for ligne in &lues {
        assert!(
            ligne.contains(&format!("/content/{OBJECT_ID}.flac")),
            "le journal doit NOMMER l'adresse retenue, sinon il n'apprend \
             rien :\n{ligne}"
        );
    }
    for zone in 1..=4 {
        assert!(
            lues.iter().any(|l| l.contains(&format!("zone_id={zone}"))),
            "aucune trace pour la zone {zone} :\n{journal}"
        );
    }
}
