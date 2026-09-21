//! #4573 — le BRANCHEMENT, de bout en bout : un renderer qui déclare deux
//! canaux, une piste 5.1, et ce que l'orchestrateur décide de servir.
//!
//! La .159 avait livré la lecture de la déclaration sans la brancher : « la
//! réduction n'est pas branchée, `audio::canaux_reseau_4573` n'a aucun
//! appelant hors de ses propres témoins » (commentaire de clôture sur
//! l'issue). Ce fichier est la garde de l'autre moitié.
//!
//! Le renderer est un VRAI serveur SOAP sur `127.0.0.1`, qui répond le Sink
//! relevé par Xavier Joly sur son Denon AVR-X1600H le 20/09/2026 — LPCM en
//! `channels=1` et `channels=2`, `audio/flac:*` sans aucun `channels=`. Rien
//! n'est simulé côté orchestrateur : la zone, la file, la piste et la sonde
//! sont les vraies.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

const UDN: &str = "uuid:denon-x1600h-4573";

/// Un ConnectionManager qui répond toujours le même Sink.
///
/// Une tâche par connexion, et `Connection: close` : la résolution enchaîne
/// DEUX sondes SOAP (`supports_mime` pour la négociation FLAC, puis les
/// canaux), et `reqwest` réemploie sa connexion. Un serveur qui n'en sert
/// qu'une à la fois rendait la seconde sonde INCONCLUANTE — donc `None`,
/// donc aucune réduction : un faux rouge qui ressemblait trait pour trait au
/// défaut qu'on corrige.
async fn renderer_qui_annonce(sink: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Lire la requête jusqu'au bout de ses en-têtes : une pile
                // SOAP qui répond avant d'avoir lu fait échouer l'écriture
                // du client.
                let mut brut = Vec::new();
                let mut tampon = [0u8; 4096];
                loop {
                    let Ok(n) = sock.read(&mut tampon).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    brut.extend_from_slice(&tampon[..n]);
                    let texte = String::from_utf8_lossy(&brut);
                    let Some(fin) = texte.find("\r\n\r\n") else {
                        continue;
                    };
                    let attendu: usize = texte
                        .lines()
                        .find_map(|l| {
                            let (nom, valeur) = l.split_once(':')?;
                            nom.eq_ignore_ascii_case("content-length")
                                .then(|| valeur.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if brut.len() >= fin + 4 + attendu {
                        break;
                    }
                }
                let corps = format!(
                    concat!(
                        r#"<?xml version="1.0"?>"#,
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                        "<s:Body><u:GetProtocolInfoResponse><Sink>{}</Sink>",
                        "</u:GetProtocolInfoResponse></s:Body></s:Envelope>"
                    ),
                    sink
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://127.0.0.1:{port}"), handle)
}

/// Une base avec une zone DLNA pointée sur `UDN` et une piste déclarée à
/// `canaux` voies. Le FICHIER est la stéréo de la caisse : la décision se
/// prend sur la ligne `tracks`, pas sur les octets.
fn base_avec_piste(canaux: i32) -> (Arc<dyn DbBackend>, i64, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let zone_id = zones
        .create("Home Theater", Some("dlna"), Some(UDN))
        .unwrap();

    let chemin = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");
    let mut t = tune_core::db::models::Track::new("Piste 5.1".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(48_000);
    t.bit_depth = Some(24);
    t.channels = canaux;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap();

    tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(backend.clone())
        .append(
            zone_id,
            &[tune_core::db::play_queue_repo::QueueInput::Local { track_id }],
        )
        .unwrap();
    (backend, zone_id, track_id)
}

/// Ce que l'orchestrateur décide de servir à ce renderer, pour cette piste.
async fn canaux_servis(canaux_source: i32, sink: &'static str) -> Option<u32> {
    let (base, handle) = renderer_qui_annonce(sink).await;
    let (backend, zone_id, _track_id) = base_avec_piste(canaux_source);

    let mut registre = tune_core::outputs::registry::OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
        "Home Theater".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    )));

    let orch = tune_core::orchestrator::PlaybackOrchestrator::new(
        backend.clone(),
        Arc::new(tune_core::playback::PlaybackManager::new()),
        Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            tune_core::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(registre)),
        None,
    );
    let r = orch.resolve_queue_item_url(zone_id, 0).await.unwrap();
    handle.abort();
    r.channels
}

/// Le Sink RÉEL du Denon AVR-X1600H de Xavier : du LPCM en 1 et 2 canaux, et
/// du FLAC sans aucun `channels=`. Le maximum déclaré est donc DEUX.
const SINK_DENON: &str = "http-get:*:audio/L16;rate=44100;channels=1:*,\
                          http-get:*:audio/L16;rate=44100;channels=2:*,\
                          http-get:*:audio/L16;rate=48000;channels=2:*,\
                          http-get:*:audio/flac:*,\
                          http-get:*:audio/wav:*";

/// Un renderer qui ne dit RIEN de ses canaux — le cas le plus fréquent, et
/// celui où Tune ne doit surtout pas décider à la place de l'auditeur.
const SINK_MUET_SUR_LES_CANAUX: &str = "http-get:*:audio/flac:*,\
                                        http-get:*:audio/wav:*,\
                                        http-get:*:audio/mpeg:*";

/// 🔴 LE témoin du lot. Rouge avant : la règle existait depuis la .159 mais
/// personne ne l'appelait — `DecisionLocale.channels` valait `track.channels`,
/// donc 6, et le FLAC 5.1 partait tel quel (`dlna_set_uri_ok …
/// advertised_mime=audio/flac`, journal de Xavier du 20/09).
#[tokio::test]
async fn un_51_vers_un_renderer_qui_annonce_deux_canaux_part_en_stereo() {
    assert_eq!(
        canaux_servis(6, SINK_DENON).await,
        Some(2),
        "le Denon annonce deux canaux au plus : le 5.1 doit être replié avant \
         de partir, pas laissé à la charge de l'ampli"
    );
}

/// 🔴 LA contre-épreuve, sur le MÊME chemin et la même piste : un renderer
/// dont le Sink ne porte aucun `channels=` ne fait RIEN réduire. C'est
/// « on ne sait pas », jamais « deux ».
#[tokio::test]
async fn un_renderer_muet_sur_ses_canaux_ne_declenche_aucune_reduction() {
    assert_eq!(
        canaux_servis(6, SINK_MUET_SUR_LES_CANAUX).await,
        Some(6),
        "aucune déclaration : les six voies partent intactes"
    );
}

/// Une stéréo ordinaire vers le même Denon ne change pas de chemin — et la
/// sonde SOAP n'est même pas lancée. Sans cette garde, toute la bibliothèque
/// aurait payé un aller-retour SOAP par piste.
#[tokio::test]
async fn une_stereo_vers_le_meme_denon_ne_change_rien() {
    assert_eq!(canaux_servis(2, SINK_DENON).await, Some(2));
}

/// Un 7.1 vers ce Denon tombe aussi à deux : la matrice BS.775 étendue
/// existe, et la déclaration est la même.
#[tokio::test]
async fn un_71_vers_le_meme_denon_part_aussi_en_stereo() {
    assert_eq!(canaux_servis(8, SINK_DENON).await, Some(2));
}

/// Garde de l'appareillage lui-même : le faux Denon répond bien, et ce que
/// Tune lit de son Sink est bien « deux canaux au plus ». Sans elle, un
/// témoin vert ne prouverait rien — il pourrait l'être parce que la sonde
/// échoue et que personne ne réduit jamais.
#[tokio::test]
async fn le_faux_denon_est_bien_lu_par_la_sonde() {
    let (base, handle) = renderer_qui_annonce(SINK_DENON).await;
    let sortie = tune_core::outputs::dlna::DlnaOutput::new(
        "Home Theater".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    );
    let caps = sortie.probe_capabilities().await;
    assert!(caps.probed, "la sonde doit aboutir : {caps:?}");
    assert_eq!(caps.canaux_max, Some(2), "Sink lu : {:?}", caps.sink);
    handle.abort();
}
