//! Phase 2 du chantier `unifier-serveurs-upnp-et-bibliotheque` : indexer UNE
//! source UPnP, purement additive, par une clé d'identité qui DÉDOUBLONNE.
//!
//! # Ce que ce fichier cloue, et pourquoi il fallait le clouer ici
//!
//! Le plan du chantier écrivait `source_id = '<udn>|<objectid>'`. La mesure du
//! 14/09 contre Asset UPnP (`192.168.1.41:26125`) l'a démenti : l'`ObjectID`
//! d'une piste porte l'identifiant de **son conteneur parent**, et change donc
//! d'un axe de navigation à l'autre. La même piste « Wonderwall » :
//!
//! ```text
//! sous « Album »            id = d6120941636376083059-co4E8D6A18CD1AC698
//! sous « Genre > Pop-Rock » id = d6120941636376083059-co679729C874689A62
//! ```
//!
//! **Et son URL de `res` porte le même identifiant contextuel** — c'est la
//! mesure que la phase 0 n'avait pas faite, et elle écarte la seconde clé
//! évidente :
//!
//! ```text
//! .../content/c2/b16/f44100/d6120941636376083059-co4E8D6A18CD1AC698.flac
//! .../content/c2/b16/f44100/d6120941636376083059-co679729C874689A62.flac
//! ```
//!
//! Le banc ci-dessous est ce relevé, en miniature : deux axes, la même piste
//! sous deux `ObjectID` et deux URL, plus deux pistes homonymes que seule
//! `res@size` sépare. Un serveur HTTP réel répond du SOAP réel ; la route est
//! appelée par le routeur réel. Rien n'est simulé côté Tune.
//!
//! # Les quatre choses que le témoin garde
//!
//! 1. **La clé replie les axes** : 4 items vus, 3 pistes indexées.
//! 2. **La clé sépare ce qui doit l'être** : deux encodages de « The edge »
//!    (mesurés : 4 788 505 o en mp3, 17 140 575 o en flac) restent deux lignes.
//!    Sans cette moitié, une clé constante passerait le point 1.
//! 3. **La passe est additive** : une piste LOCALE posée avant l'indexation est
//!    toujours là après, et une seconde indexation n'ajoute ni ne supprime rien.
//! 4. **L'instantané est écrit** : URL de lecture, pochette, `ObjectID` et
//!    serveur d'origine, pour que l'écran se rende serveur éteint.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`. Sans elle il ne serait JAMAIS
//! compilé (cf `tests_orphelins.rs`).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

/// L'UDN du serveur du banc — la forme que Tune emploie comme clé de registre.
const UDN: &str = "uuid:258FC2D5-E2C3-B734-0-123456789abc";

/// Un item DIDL, tel que le banc le publie.
struct ItemDuBanc {
    object_id: &'static str,
    titre: &'static str,
    artiste: &'static str,
    album: &'static str,
    taille: u64,
    duree: &'static str,
    extension: &'static str,
    mime: &'static str,
}

/// **Le banc, relevé sur Asset le 14/09 et réduit à ce qui décide.**
///
/// Axe « Album » et axe « Genre » publient la MÊME Wonderwall sous deux
/// `ObjectID` et deux URL. « The edge » y figure en deux encodages, que seule
/// la taille sépare.
const AXE_ALBUM: &[ItemDuBanc] = &[
    ItemDuBanc {
        object_id: "d6120941636376083059-co4E8D6A18CD1AC698",
        titre: "Wonderwall",
        artiste: "Oasis",
        album: "Morning Glory",
        taille: 31_911_291,
        duree: "0:04:18.000",
        extension: "flac",
        mime: "audio/x-flac",
    },
    ItemDuBanc {
        object_id: "d-8394918739511425397-co4E8D6A18CD1AC698",
        titre: "1- The edge",
        artiste: "Divers",
        album: "Bande originale",
        taille: 4_788_505,
        duree: "0:03:19.000",
        extension: "mp3",
        mime: "audio/mpeg",
    },
];

const AXE_GENRE: &[ItemDuBanc] = &[
    ItemDuBanc {
        // MÊME piste, AUTRE ObjectID, AUTRE URL — le cœur de la mesure.
        object_id: "d6120941636376083059-co679729C874689A62",
        titre: "Wonderwall",
        artiste: "Oasis",
        album: "Morning Glory",
        taille: 31_911_291,
        duree: "0:04:18.000",
        extension: "flac",
        mime: "audio/x-flac",
    },
    ItemDuBanc {
        // Le MÊME titre que ci-dessus, dans l'autre encodage : seule la taille
        // change. Deux lignes attendues, pas une.
        object_id: "d-7483377966117626057-co679729C874689A62",
        titre: "1- The edge",
        artiste: "Divers",
        album: "Bande originale",
        taille: 17_140_575,
        duree: "0:03:19.000",
        extension: "flac",
        mime: "audio/x-flac",
    },
];

/// 4 items publiés, sur deux axes.
const ITEMS_PUBLIES: usize = AXE_ALBUM.len() + AXE_GENRE.len();
/// 3 pistes distinctes : Wonderwall une fois, « The edge » deux fois.
const PISTES_ATTENDUES: usize = 3;
/// Deux albums distants : « Morning Glory » et « Bande originale ».
const ALBUMS_ATTENDUS: usize = 2;

/// Plancher du détecteur : un banc qui ne porterait plus les deux natures
/// (répétition entre axes ET séparation par la taille) ne prouverait plus rien.
fn exige_un_banc_des_deux_natures() {
    assert!(
        ITEMS_PUBLIES > PISTES_ATTENDUES,
        "le banc ne publie plus la même piste sous deux axes : le repli ne \
         serait plus mis à l'épreuve"
    );
    let tailles: std::collections::BTreeSet<u64> = AXE_ALBUM
        .iter()
        .chain(AXE_GENRE)
        .filter(|i| i.titre == "1- The edge")
        .map(|i| i.taille)
        .collect();
    assert_eq!(
        tailles.len(),
        2,
        "le banc ne porte plus deux encodages du même titre : une clé CONSTANTE \
         passerait alors le témoin sans rien garder"
    );
}

fn echapper(texte: &str) -> String {
    texte
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Le DIDL brut d'un item, exactement dans la forme d'Asset : `res@size`,
/// `res@duration`, `protocolInfo`, et l'URL qui porte l'`ObjectID`.
fn didl_item(item: &ItemDuBanc, base: &str) -> String {
    format!(
        "<item id=\"{id}\" parentID=\"x\" restricted=\"0\">\
         <dc:title>{titre}</dc:title><dc:creator>{artiste}</dc:creator>\
         <upnp:artist>{artiste}</upnp:artist><upnp:album>{album}</upnp:album>\
         <upnp:albumArtURI>{base}/aa/{id}/cover.jpg</upnp:albumArtURI>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res duration=\"{duree}\" size=\"{taille}\" bitsPerSample=\"16\" \
         sampleFrequency=\"44100\" nrAudioChannels=\"2\" \
         protocolInfo=\"http-get:*:{mime}:DLNA.ORG_PN=X\">\
         {base}/content/{id}.{ext}</res></item>",
        id = item.object_id,
        titre = echapper(item.titre),
        artiste = echapper(item.artiste),
        album = echapper(item.album),
        duree = item.duree,
        taille = item.taille,
        mime = item.mime,
        ext = item.extension,
    )
}

fn enveloppe_soap(didl: &str, nombre: usize) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:BrowseResponse><Result>{}</Result>\
         <NumberReturned>{nombre}</NumberReturned>\
         <TotalMatches>{nombre}</TotalMatches><UpdateID>1</UpdateID>\
         </u:BrowseResponse></s:Body></s:Envelope>",
        echapper(&format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{didl}</DIDL-Lite>"
        ))
    )
}

/// Le serveur ContentDirectory du banc. Il répond au SOAP `Browse` réel, avec
/// la pagination réelle (la seconde page est vide, ce qui termine la boucle).
async fn lever_le_serveur_du_banc() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse liée");
    let base = format!("http://{adresse}");
    let base_pour_le_service = base.clone();

    let app = Router::new().route(
        "/control",
        post(move |corps: String| {
            let base = base_pour_le_service.clone();
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
                // Page suivante : rien. C'est ce que la boucle de parcours
                // attend pour s'arrêter.
                if depart > 0 {
                    return enveloppe_soap("", 0);
                }
                match object_id.as_str() {
                    "0" => enveloppe_soap(
                        "<container id=\"axe-album\" parentID=\"0\" childCount=\"2\">\
                         <dc:title>Album</dc:title></container>\
                         <container id=\"axe-genre\" parentID=\"0\" childCount=\"2\">\
                         <dc:title>Genre</dc:title></container>",
                        2,
                    ),
                    "axe-album" => {
                        let didl: String = AXE_ALBUM.iter().map(|i| didl_item(i, &base)).collect();
                        enveloppe_soap(&didl, AXE_ALBUM.len())
                    }
                    "axe-genre" => {
                        let didl: String = AXE_GENRE.iter().map(|i| didl_item(i, &base)).collect();
                        enveloppe_soap(&didl, AXE_GENRE.len())
                    }
                    _ => enveloppe_soap("", 0),
                }
            }
        }),
    );

    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("{base}/control")
}

/// Inscrit le serveur du banc au registre en mémoire, comme le ferait la
/// découverte SSDP.
async fn inscrire_le_serveur(etat: &AppState, control_url: &str) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: UDN.to_string(),
        name: "Banc UPnP".to_string(),
        manufacturer: "Illustrate Ltd".to_string(),
        model: "Asset UPnP Server".to_string(),
        location: control_url.to_string(),
        content_directory_url: control_url.to_string(),
        host: "127.0.0.1".to_string(),
        port: 0,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    };
    etat.media_servers
        .lock()
        .await
        .insert(UDN.to_string(), info);
}

async fn poster(app: &Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("routeur en échec");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    let corps = serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "{chemin} : JSON illisible ({e}) — {}",
            String::from_utf8_lossy(&octets)
        )
    });
    (statut, corps)
}

fn compte(etat: &AppState, sql: &str) -> i64 {
    etat.backend
        .query_one(sql, &[])
        .expect("requête de comptage")
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

#[tokio::test(flavor = "multi_thread")]
async fn une_source_upnp_s_indexe_sans_doublon_et_sans_rien_supprimer() {
    exige_un_banc_des_deux_natures();

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");

    // Une piste LOCALE, posée AVANT. Elle est le témoin du « purement
    // additif » : si l'indexation la touchait, c'est ici que ça rougirait.
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    let mut locale = Track::new("Une piste à moi".into());
    locale.file_path = Some("/musique/a-moi.flac".into());
    pistes.create(&locale).expect("piste locale du banc");

    let control_url = lever_le_serveur_du_banc().await;
    inscrire_le_serveur(&etat, &control_url).await;
    let app = tune_server::routes::router(etat.clone());

    let chemin = format!("/api/v1/network/media-servers/{UDN}/indexer");
    let (statut, corps) = poster(&app, &chemin).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["indexe"], json!(true), "corps : {corps}");

    // --- 1. Le parcours a bien vu les quatre items des deux axes ---
    assert_eq!(
        corps["parcours"]["items_vus"],
        json!(ITEMS_PUBLIES),
        "le parcours doit traverser les DEUX axes — corps {corps}"
    );

    // --- 2. …et la clé d'identité les a repliés à trois ---
    assert_eq!(
        corps["pistes"]["distinctes"],
        json!(PISTES_ATTENDUES),
        "la même piste vue sous deux ObjectID et deux URL doit compter POUR UNE : \
         c'est toute la décision de ce lot — corps {corps}"
    );
    assert_eq!(
        corps["pistes"]["ajoutees"],
        json!(PISTES_ATTENDUES),
        "corps {corps}"
    );
    assert_eq!(
        corps["supprimees"],
        json!(0),
        "la première passe est purement additive — corps {corps}"
    );

    // --- 3. La base dit la même chose que la réponse ---
    assert_eq!(
        compte(&etat, "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'"),
        PISTES_ATTENDUES as i64,
        "les lignes réellement posées"
    );
    assert_eq!(
        compte(&etat, "SELECT COUNT(*) FROM albums WHERE source = 'upnp'"),
        ALBUMS_ATTENDUS as i64,
        "les albums distants"
    );
    assert_eq!(
        compte(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp' AND file_path IS NOT NULL"
        ),
        0,
        "une piste distante n'a PAS de chemin sur le disque : c'est ce qui la met \
         hors de portée du scan local et de son adoption (`adopter_en_local`)"
    );
    assert_eq!(
        compte(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'local' AND file_path = '/musique/a-moi.flac'"
        ),
        1,
        "la piste locale posée avant l'indexation doit être intacte"
    );

    // --- 4. Deux encodages du même titre restent DEUX pistes ---
    assert_eq!(
        compte(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp' AND title = '1- The edge'"
        ),
        2,
        "seule `res@size` les sépare : si la clé les repliait, elle replierait \
         aussi ce qu'il ne faut pas"
    );

    // --- 5. L'instantané d'affichage est écrit ---
    assert_eq!(
        compte(
            &etat,
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'upnp_res_url'"
        ),
        PISTES_ATTENDUES as i64,
        "chaque piste indexée doit porter son URL de lecture : sans elle, \
         l'écran ne pourrait rien tenter serveur éteint"
    );
    assert_eq!(
        compte(
            &etat,
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'upnp_serveur' \
             AND value = 'uuid:258FC2D5-E2C3-B734-0-123456789abc'"
        ),
        PISTES_ATTENDUES as i64,
        "l'instantané doit nommer le serveur d'origine (D1bis)"
    );

    // --- 6. Les compteurs de la bibliothèque ventilent la nouvelle source ---
    let reponse = app
        .clone()
        .oneshot(
            Request::get("/api/v1/library/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("routeur");
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats: Value = serde_json::from_slice(&octets).unwrap();
    assert_eq!(
        stats["tracks_by_source"]["upnp"],
        json!(PISTES_ATTENDUES),
        "la bibliothèque doit compter la source `upnp` comme les autres — {stats}"
    );
    assert_eq!(
        stats["tracks_by_source"]["local"],
        json!(1),
        "la part locale ne bouge pas — {stats}"
    );

    // --- 7. Rejouer l'indexation n'ajoute rien, ne supprime rien ---
    let revision_avant = compte(
        &etat,
        "SELECT value FROM upnp_catalog_revision WHERE id = 1",
    );
    let (_, deuxieme) = poster(&app, &chemin).await;
    assert_eq!(
        compte(
            &etat,
            "SELECT value FROM upnp_catalog_revision WHERE id = 1"
        ),
        revision_avant,
        "réindexation UPnP identique : le SystemUpdateID doit rester stable"
    );
    assert_eq!(
        deuxieme["pistes"]["ajoutees"],
        json!(0),
        "une seconde passe ne doit RIEN ajouter — corps {deuxieme}"
    );
    assert_eq!(
        deuxieme["pistes"]["mises_a_jour"],
        json!(PISTES_ATTENDUES),
        "elle rafraîchit l'instantané, c'est tout — corps {deuxieme}"
    );
    assert_eq!(
        compte(&etat, "SELECT COUNT(*) FROM tracks"),
        PISTES_ATTENDUES as i64 + 1,
        "le nombre total de pistes ne bouge pas d'une passe à l'autre"
    );

    // --- 8. La réponse DIT ses dégradations, sortie par sortie (D4) ---
    //
    // Depuis la phase 3, une ligne indexée SE JOUE : la réserve « la lecture
    // est la phase 3 » serait devenue un mensonge dans l'autre sens. Ce que la
    // route doit dire maintenant, ce sont les quatre comportements de D4.
    let reserves = deuxieme["reserves"]
        .as_array()
        .unwrap_or_else(|| panic!("reserves doit être une liste — {deuxieme}"));
    let dites: Vec<&str> = reserves.iter().filter_map(Value::as_str).collect();
    assert!(
        !dites.iter().any(|r| r.contains("phase 3")),
        "la réserve « la lecture est la phase 3 » doit avoir DISPARU : elle \
         est fausse depuis que la lecture est branchée — {deuxieme}"
    );
    for attendu in ["réseau", "navigateur", "locale", "OAAT"] {
        assert!(
            dites.iter().any(|r| r.contains(attendu)),
            "la réponse doit dire ce que vaut la lecture sur la sortie \
             « {attendu} » — ne jamais faire semblant : {deuxieme}"
        );
    }
    assert!(
        dites.iter().any(|r| r.contains("ReplayGain")),
        "la sortie locale joue sans ReplayGain : il faut le dire — {deuxieme}"
    );
}

/// Un serveur absent du registre se refuse en le nommant — il ne rend pas un
/// succès vide, qui laisserait croire à un catalogue de zéro piste.
#[tokio::test(flavor = "multi_thread")]
async fn un_serveur_inconnu_se_refuse_en_le_disant() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let app = tune_server::routes::router(etat);
    let (statut, corps) = poster(&app, "/api/v1/network/media-servers/uuid:fantome/indexer").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["indexe"], json!(false), "corps : {corps}");
    assert_eq!(corps["raison"], json!("serveur inconnu"), "corps : {corps}");
}

/// A complete catalogue, a partial HTTP failure, and guarded removal through
/// the public subscription API. The same real SOAP endpoint changes over time.
#[tokio::test]
async fn synchronisation_durable_refuse_les_pannes_et_confirme_les_retraits_massifs() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let stage = Arc::new(AtomicUsize::new(6));
    let value = stage.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = Router::new().route("/control", post(move || {
        let count = value.load(Ordering::SeqCst);
        async move {
            if count == 99 { return (StatusCode::BAD_GATEWAY, "upstream unavailable".to_string()); }
            let content: String = (0..count).map(|i| format!(
                "<item id=\"p{i}\"><dc:title>Piste {i}</dc:title><upnp:artist>Artiste</upnp:artist><upnp:album>Album</upnp:album><res duration=\"0:01:00\" size=\"100\" protocolInfo=\"http-get:*:audio/flac:*\">http://example.invalid/{i}.flac</res></item>"
            )).collect();
            (StatusCode::OK, enveloppe_soap(&content, count))
        }
    }));
    let remote = tokio::spawn(async move {
        axum::serve(listener, endpoint).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    inscrire_le_serveur(&state, &format!("http://{address}/control")).await;
    let local_id = TrackRepo::with_backend(state.backend.clone())
        .create(&Track::new("Local protégé".into()))
        .unwrap();
    let app = tune_server::routes::network::router().with_state(state.clone());

    async fn call(app: &Router, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn settled(app: &Router) -> Value {
        for _ in 0..200 {
            let (status, body) = call(app, "/library-sources", None).await;
            assert_eq!(status, StatusCode::OK);
            let s = &body["items"][0];
            if !matches!(s["status"].as_str(), Some("pending" | "running")) {
                return s.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("la synchronisation ne rend pas son bilan");
    }
    let (status, _) = call(
        &app,
        &format!("/media-servers/{UDN}/library-source"),
        Some(json!({"container":"0"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut source = settled(&app).await;
    assert_eq!(source["status"], "ready", "première indexation complète");
    assert_eq!(source["report"]["pistes"]["distinctes"], 6);
    let key = source["key"].clone();
    let count = || {
        state
            .backend
            .query_one_strong("SELECT COUNT(*) FROM tracks WHERE source = 'upnp'", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count(), 6);

    stage.store(99, Ordering::SeqCst);
    call(
        &app,
        "/library-sources",
        Some(json!({"key":key,"action":"sync"})),
    )
    .await;
    source = settled(&app).await;
    assert_eq!(source["status"], "partial");
    assert_eq!(
        source["report"]["complet"], false,
        "un HTTP 502 n’est pas un catalogue vide complet"
    );
    assert_eq!(count(), 6, "une panne ne supprime aucune piste");

    stage.store(5, Ordering::SeqCst);
    call(
        &app,
        "/library-sources",
        Some(json!({"key":key,"action":"sync"})),
    )
    .await;
    source = settled(&app).await;
    assert_eq!(source["status"], "ready");
    assert_eq!(
        source["report"]["supprimees"], 1,
        "un retrait inférieur à 20 % est réconcilié"
    );
    assert_eq!(count(), 5);

    stage.store(3, Ordering::SeqCst);
    call(
        &app,
        "/library-sources",
        Some(json!({"key":key,"action":"sync"})),
    )
    .await;
    source = settled(&app).await;
    assert_eq!(
        source["status"], "confirmation",
        "40 % exige une confirmation"
    );
    assert_eq!(source["pending_count"], 2);
    assert_eq!(count(), 5, "aucun retrait massif avant confirmation");
    let (status, _) = call(
        &app,
        "/library-sources",
        Some(json!({"key":key,"action":"confirm","count":2,"generation":"stale"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "un ancien bilan ne confirme pas le nouveau"
    );
    assert_eq!(count(), 5);
    let (status, _) = call(
        &app,
        "/library-sources",
        Some(json!({"key":key,"action":"confirm","count":2,"generation":source["generation"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count(), 3);
    assert!(
        state
            .backend
            .query_one_strong("SELECT id FROM tracks WHERE id = ?", &[&local_id])
            .unwrap()
            .is_some(),
        "le local reste intact"
    );
    assert!(
        state
            .backend
            .query_one_strong(
                "SELECT state_json FROM upnp_library_sources WHERE source_key = ?",
                &[&key.as_str().unwrap()]
            )
            .unwrap()
            .is_some(),
        "le bilan est persistant"
    );
    remote.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn une_deuxieme_page_manquante_ne_devient_pas_un_catalogue_complet() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = Router::new().route(
        "/control",
        post(|body: String| async move {
            if body.contains("<StartingIndex>0</StartingIndex>") {
                let didl: String = AXE_ALBUM
                    .iter()
                    .map(|i| didl_item(i, "http://example.invalid"))
                    .collect();
                enveloppe_soap(&didl, 2).replace(
                    "<TotalMatches>2</TotalMatches>",
                    "<TotalMatches>3</TotalMatches>",
                )
            } else {
                enveloppe_soap("", 0).replace(
                    "<TotalMatches>0</TotalMatches>",
                    "<TotalMatches>3</TotalMatches>",
                )
            }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, endpoint).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    inscrire_le_serveur(&state, &format!("http://{address}/control")).await;
    let app = tune_server::routes::network::router().with_state(state);
    let (_, report) = poster(&app, &format!("/media-servers/{UDN}/indexer")).await;
    assert_eq!(
        report["pistes"]["distinctes"], 2,
        "les pages déjà reçues restent utilisables"
    );
    assert_eq!(
        report["complet"], false,
        "une page manquante interdit la réconciliation"
    );
    assert!(
        report["erreurs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("pagination interrompue"))
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn un_retrait_ne_supprime_ni_une_autre_source_ni_une_piste_locale() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut remote = Track::new("Partagée".into());
    remote.source = "upnp".into();
    remote.source_id = Some(format!("{UDN}|shared"));
    let remote_id = repo.create(&remote).unwrap();
    let local_id = repo.create(&Track::new("Locale".into())).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for key in ["a", "b"] {
        let source = json!({"key":key,"udn":UDN,"container":key,"name":key,"enabled":true,
            "status":"confirmation","last_attempt":now,"last_success":now,"report":{},
            "generation":"new","pending":[remote_id,local_id]});
        state.backend.execute("INSERT INTO upnp_library_sources (source_key,udn,container,state_json) VALUES (?,?,?,?)", &[&key,&UDN,&key,&source.to_string()]).unwrap();
        for id in [remote_id, local_id] {
            state.backend.execute("INSERT INTO upnp_library_members (source_key,track_id,generation) VALUES (?,?,?)", &[&key,&id,&"old"]).unwrap();
        }
    }
    let app = tune_server::routes::network::router().with_state(state.clone());
    for (key, expected_remote) in [("a", true), ("b", false)] {
        let request = Request::builder()
            .method("POST")
            .uri("/library-sources")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({"key":key,"action":"confirm","generation":"new","count":2}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            repo.get(remote_id).unwrap().is_some(),
            expected_remote,
            "la piste distante reste tant qu’une autre source la possède"
        );
        assert!(
            repo.get(local_id).unwrap().is_some(),
            "une piste locale ne peut jamais être supprimée par la réconciliation UPnP"
        );
    }
}

#[tokio::test]
async fn identites_upnp_la_synchronisation_conserve_liens_et_appartenances_apres_correction() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    async fn appel(app: &Router, body: Option<Value>) -> Value {
        let request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri("/library-sources")
            .header("Content-Type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    async fn bilan(app: &Router) -> Value {
        for _ in 0..300 {
            let body = appel(app, None).await;
            let s = &body["items"][0];
            if !matches!(s["status"].as_str(), Some("pending" | "running")) {
                return s.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("la synchronisation doit finir");
    }
    let stage = Arc::new(AtomicUsize::new(0));
    let value = stage.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = Router::new().route("/control", post(move || {
        let stage = value.load(Ordering::SeqCst);
        async move {
            let didl: String = (0..6).map(|i| {
                let suffixe = if stage == 0 { "" } else { " corrigé" };
                let taille = if stage == 0 { 1000 } else { 1100 };
                let objet = if stage == 4 && i == 0 { "sans-aucun-indice".into() } else if stage < 2 { format!("p{i}") } else { format!("rescan-{i}") };
                let duree = if stage == 3 && i == 0 { "0:02:00" } else { "0:01:00" };
                let titre = if stage >= 3 && i == 0 { "Fichier étranger".into() } else { format!("Piste {i}{suffixe}") };
                format!("<item id=\"{objet}\"><dc:title>{titre}</dc:title><upnp:artist>Artiste{suffixe}</upnp:artist><upnp:album>Album{suffixe}</upnp:album><res duration=\"{duree}\" size=\"{taille}\" protocolInfo=\"http-get:*:audio/flac:*\">http://example.invalid/{objet}.flac</res></item>")
            }).collect();
            enveloppe_soap(&didl, 6)
        }
    }));
    let remote = tokio::spawn(async move {
        axum::serve(listener, endpoint).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    inscrire_le_serveur(&state, &format!("http://{address}/control")).await;
    let app = tune_server::routes::network::router().with_state(state.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/media-servers/{UDN}/library-source"))
        .header("Content-Type", "application/json")
        .body(Body::from("{\"container\":\"0\"}"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
    let premier = bilan(&app).await;
    assert_eq!(premier["status"], "ready");
    let key = premier["key"].clone();
    let membres = || {
        state
            .backend
            .query_many(
                "SELECT track_id FROM upnp_library_members ORDER BY track_id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    let avant = membres();
    assert_eq!(avant.len(), 6);
    let favori = state
        .backend
        .query_one("SELECT id FROM tracks WHERE title = 'Piste 0'", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO favorites (profile_id,item_type,item_id) VALUES (1,'track',?)",
            &[&favori.to_string()],
        )
        .unwrap();
    let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
    let playlist = repo.create("Conserver", None, 1).unwrap();
    repo.add_tracks(playlist, &[favori, avant[1], favori], None)
        .unwrap();
    for etape in 1..=2 {
        stage.store(etape, Ordering::SeqCst);
        appel(&app, Some(json!({"key":key,"action":"sync"}))).await;
        let resultat = bilan(&app).await;
        assert_eq!(
            resultat["status"], "ready",
            "un changement de tags/adresse n'est pas un retrait : {resultat}"
        );
        assert_eq!(
            resultat["report"]["pistes"]["mises_a_jour"], 6,
            "réutiliser les lignes, pas les recréer"
        );
        assert_eq!(resultat["report"]["pistes"]["ajoutees"], 0);
        assert_eq!(resultat["report"]["supprimees"], 0);
        assert_eq!(
            membres(),
            avant,
            "les appartenances suivent les identifiants durables"
        );
    }
    stage.store(3, Ordering::SeqCst);
    appel(&app, Some(json!({"key":key,"action":"sync"}))).await;
    let ambigu = bilan(&app).await;
    assert_eq!(
        ambigu["status"], "partial",
        "un indice réutilisé interdit même un retrait de 1/6 : {ambigu}"
    );
    assert_eq!(membres(), avant);
    assert_eq!(
        TrackRepo::with_backend(state.backend.clone())
            .get(favori)
            .unwrap()
            .unwrap()
            .title,
        "Piste 0 corrigé"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM favorites f JOIN tracks t ON CAST(f.item_id AS INTEGER) = t.id WHERE f.item_type = 'track'"
        ),
        1
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM playlist_tracks pt JOIN tracks t ON pt.track_id = t.id"
        ),
        3
    );
    // Plus aucun indice pour l'ancienne piste favorite : une nouvelle piste
    // peut entrer, mais le retrait de l'ancienne exige un examen même à 1/6.
    stage.store(4, Ordering::SeqCst);
    appel(&app, Some(json!({"key":key,"action":"sync"}))).await;
    let disparu = bilan(&app).await;
    assert_eq!(
        disparu["status"], "confirmation",
        "les liens utilisateur protègent aussi un retrait inférieur à 20 % : {disparu}"
    );
    assert_eq!(disparu["pending_count"], 1);
    assert_eq!(disparu["report"]["retrait_avec_liens_utilisateur"], true);
    assert!(
        TrackRepo::with_backend(state.backend.clone())
            .get(favori)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM playlist_tracks pt JOIN tracks t ON pt.track_id = t.id"
        ),
        3
    );
    remote.abort();
}
