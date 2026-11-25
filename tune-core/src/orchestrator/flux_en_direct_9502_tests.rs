//! #9502 (fils 3049 et 3066) — une radio en direct poussée au MediaRenderer
//! par un point de contrôle (JPlay) se joue par le chemin radio.
//!
//! Le banc : un faux serveur HTTP/1.1 qui sert un FLAC « en direct » (aucun
//! `Content-Length`, `icy-name`, total d'échantillons à 0 dans STREAMINFO,
//! connexion tenue ouverte), et la demande que la façade envoie : `source =
//! "upnp"`, l'adresse en `source_id`, aucune durée, sortie LOCALE.
//!
//! Avant le correctif, la résolution rendait l'adresse brute à la sortie
//! locale, qui la lisait comme un fichier compressé : démarrage figé, relance,
//! et ainsi de suite. Le témoin exige une session WAV du chemin radio, et que
//! le décodeur radio y ait bien lu le FLAC de la station.
//!
//! Contre-épreuves : un fichier avec `Content-Length`, ou un flux sans
//! longueur dont la durée est connue, restent sur le chemin fichier.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const SORTIE_LOCALE: &str = "local:temoin-9502";
/// Ce que le faux serveur annonce.
#[derive(Clone, Copy)]
struct Reponse {
    longueur: bool,
    icy: bool,
    /// Garder la connexion ouverte après le corps (un direct ne finit pas).
    tenir: bool,
}
async fn faux_serveur(corps: Vec<u8>, reponse: Reponse) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let corps = Arc::new(corps);
    tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let corps = corps.clone();
            tokio::spawn(async move {
                let mut tampon = Vec::new();
                let mut morceau = [0u8; 4096];
                while !tampon.windows(4).any(|w| w == b"\r\n\r\n") {
                    match flux.read(&mut morceau).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => tampon.extend_from_slice(&morceau[..n]),
                    }
                }
                // Comme Icecast : `Range` ignoré, toujours `200`.
                let mut entete = String::from("HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\n");
                if reponse.longueur {
                    entete.push_str(&format!("Content-Length: {}\r\n", corps.len()));
                }
                if reponse.icy {
                    entete.push_str("icy-name: Faux direct 9502\r\nicy-pub: 1\r\n");
                }
                entete.push_str("Connection: close\r\n\r\n");
                if flux.write_all(entete.as_bytes()).await.is_err() {
                    return;
                }
                if flux.write_all(&corps).await.is_err() {
                    return;
                }
                if reponse.tenir {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
                let _ = flux.shutdown().await;
            });
        }
    });
    format!("http://{adresse}/flac")
}
fn pcm_16_bits(cadence: u32, secondes: u32) -> Vec<u8> {
    let trames = (cadence * secondes) as usize;
    let mut pcm = Vec::with_capacity(trames * 4);
    for i in 0..trames {
        let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / cadence as f64).sin() * 0.3;
        let e = ((v * i16::MAX as f64) as i16).to_le_bytes();
        pcm.extend_from_slice(&e);
        pcm.extend_from_slice(&e);
    }
    pcm
}
/// Un vrai FLAC 44,1 kHz stéréo, encodé par l'encodeur de Tune. `direct` met
/// le total d'échantillons de STREAMINFO à 0, comme un flux sans fin.
fn flac(secondes: u32, direct: bool) -> Vec<u8> {
    let mut e = crate::audio::encoder::AudioEncoder::new("flac", 44_100, 16, 2);
    e.start_sync().unwrap();
    e.write_sync(&pcm_16_bits(44_100, secondes)).unwrap();
    let mut octets = e.finish_sync().unwrap();
    assert_eq!(&octets[..4], b"fLaC");
    if direct {
        // STREAMINFO commence à l'octet 8 ; le total (36 bits) occupe les
        // 4 bits bas de l'octet 13 et les octets 14 à 17.
        octets[8 + 13] &= 0xF0;
        for o in &mut octets[8 + 14..8 + 18] {
            *o = 0;
        }
    }
    octets
}
fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}
/// La demande de la façade MediaRenderer (`routes::upnp_media_renderer`).
fn demande_de_la_facade(url: &str, duration_ms: Option<i64>) -> PlayRequest {
    PlayRequest {
        zone_id: 1,
        output_device_id: Some(SORTIE_LOCALE.into()),
        source: Some("upnp".into()),
        source_id: Some(url.into()),
        title: Some("Faux direct".into()),
        duration_ms,
        ..Default::default()
    }
}
/// Le chemin RADIO : une session WAV, et le décodeur radio qui y lit le FLAC
/// de la station. Rend la source publiée par le décodeur.
async fn attendre_le_chemin_radio(orch: &PlaybackOrchestrator, resolu: &ResolvedStream, url: &str) {
    assert_eq!(
        resolu.source, "upnp",
        "la façade reconnaît sa lecture à sa source"
    );
    assert_ne!(
        resolu.url, url,
        "un direct ne doit pas partir brut vers la sortie locale (chemin fichier)"
    );
    assert_eq!(resolu.mime_type, "audio/wav", "chemin radio : WAV décodé");
    let sid = resolu
        .stream_id
        .clone()
        .expect("chemin radio : une session de flux");
    let mut format = None;
    for _ in 0..100 {
        format = orch
            .streamer
            .stream_output_wire(&sid)
            .await
            .and_then(|w| w.radio_source)
            .and_then(|s| s.format);
        if format.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    orch.streamer.remove_session(&sid).await;
    assert_eq!(
        format,
        Some("flac"),
        "le décodeur radio doit avoir lu le FLAC de la station"
    );
}
/// **Le témoin de l'issue.** FLAC continu, sans `Content-Length`, avec
/// `icy-name`, aucune durée dans le DIDL : le chemin radio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_direct_pousse_par_un_controleur_passe_par_le_chemin_radio_9502() {
    let url = faux_serveur(
        flac(20, true),
        Reponse {
            longueur: false,
            icy: true,
            tenir: true,
        },
    )
    .await;
    let orch = orchestrateur();
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(&url, None))
        .await
        .expect("le direct se résout");
    attendre_le_chemin_radio(&orch, &resolu, &url).await;
}
/// Sans `icy-*` : ni longueur ni durée lisible suffisent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_flux_sans_longueur_ni_duree_passe_par_le_chemin_radio_9502() {
    let url = faux_serveur(
        flac(20, true),
        Reponse {
            longueur: false,
            icy: false,
            tenir: true,
        },
    )
    .await;
    let orch = orchestrateur();
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(&url, None))
        .await
        .expect("le direct se résout");
    attendre_le_chemin_radio(&orch, &resolu, &url).await;
}
/// Le DIDL classe l'item en `audioBroadcast` : le chemin radio, même si le
/// serveur annonce une longueur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_diffusion_annoncee_par_le_didl_passe_par_le_chemin_radio_9502() {
    let corps = flac(20, true);
    let url = faux_serveur(
        corps,
        Reponse {
            longueur: true,
            icy: false,
            tenir: false,
        },
    )
    .await;
    super::flux_en_direct_9502::annoncer_une_diffusion(&url);
    let orch = orchestrateur();
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(&url, None))
        .await
        .expect("le direct se résout");
    attendre_le_chemin_radio(&orch, &resolu, &url).await;
}
/// Contre-épreuve : un FICHIER avec `Content-Length` reste sur le chemin
/// fichier — l'adresse part telle quelle vers la sortie locale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_fichier_avec_longueur_reste_sur_le_chemin_fichier_9502() {
    let url = faux_serveur(
        flac(3, false),
        Reponse {
            longueur: true,
            icy: false,
            tenir: false,
        },
    )
    .await;
    let orch = orchestrateur();
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(&url, None))
        .await
        .expect("le fichier se résout");
    assert_eq!(resolu.url, url, "un fichier reste sur le chemin fichier");
    assert!(resolu.stream_id.is_none(), "aucune session radio");
    assert_eq!(
        resolu.duration_ms,
        Some(3_000),
        "la durée lue dans les en-têtes"
    );
}
/// Contre-épreuve : une durée connue (DIDL `res@duration`) suffit, même sans
/// longueur annoncée, et sans aucune requête au serveur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_duree_connue_reste_sur_le_chemin_fichier_9502() {
    let url = faux_serveur(
        flac(3, false),
        Reponse {
            longueur: false,
            icy: false,
            tenir: false,
        },
    )
    .await;
    let orch = orchestrateur();
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(&url, Some(3_000)))
        .await
        .expect("le fichier se résout");
    assert_eq!(resolu.url, url, "une durée connue garde le chemin fichier");
    assert!(resolu.stream_id.is_none(), "aucune session radio");
}
