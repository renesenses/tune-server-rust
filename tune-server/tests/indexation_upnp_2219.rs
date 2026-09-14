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
    let (_, deuxieme) = poster(&app, &chemin).await;
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

    // --- 8. La réponse DIT ce qu'elle ne fait pas ---
    let reserves = deuxieme["reserves"]
        .as_array()
        .unwrap_or_else(|| panic!("reserves doit être une liste — {deuxieme}"));
    assert!(
        reserves
            .iter()
            .filter_map(Value::as_str)
            .any(|r| r.contains("phase 3")),
        "la réponse doit dire que la LECTURE d'une ligne indexée n'est pas \
         livrée par ce lot — ne jamais faire semblant : {deuxieme}"
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
