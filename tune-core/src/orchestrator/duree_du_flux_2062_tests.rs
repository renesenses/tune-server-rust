//! Fil 2062 / #5550 — une piste de serveur UPnP sans durée : Tune la lit dans
//! les en-têtes du flux.
//!
//! Le banc : un faux serveur multimédia (HTTP/1.1, `Range` honoré comme le
//! fait la Freebox) qui sert un FLAC, un MP3 et un M4A, et une demande de
//! lecture `source = "upnp"` SANS durée — ce que le client envoie quand le
//! DIDL ne porte aucune `res@duration`. La durée résolue doit être celle du
//! fichier, à une seconde près.
//!
//! Sabotage (contre-épreuve) : dans `resolve_direct.rs`, remplacer
//! `self.duree_d_une_piste_upnp(req, audio_url).await` par `duration_ms` fait
//! rougir les cinq témoins de format sur « durée absente ».

use super::*;
use crate::db::zone_repo::ZoneRepo;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Un faux serveur UPnP : sert `corps` sous `/MediaItems/<nom>`, avec ou sans
/// `Range`, et compte les requêtes reçues.
struct FauxServeur {
    url: String,
    requetes: Arc<AtomicUsize>,
}

async fn faux_serveur(nom: &str, mime: &'static str, corps: Vec<u8>) -> FauxServeur {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let corps = Arc::new(corps);
    let requetes = Arc::new(AtomicUsize::new(0));
    let compte = requetes.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let corps = corps.clone();
            let compte = compte.clone();
            tokio::spawn(async move {
                let mut tampon = Vec::new();
                let mut morceau = [0u8; 4096];
                while !tampon.windows(4).any(|w| w == b"\r\n\r\n") {
                    match flux.read(&mut morceau).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => tampon.extend_from_slice(&morceau[..n]),
                    }
                }
                compte.fetch_add(1, Ordering::SeqCst);
                let requete = String::from_utf8_lossy(&tampon).to_ascii_lowercase();
                let total = corps.len();
                let plage = requete
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| {
                        let (a, b) = r.trim().split_once('-')?;
                        let a: usize = a.parse().ok()?;
                        let b: usize = b.parse().unwrap_or(total - 1);
                        Some((a, b.min(total - 1)))
                    });
                let (statut, debut, fin) = match plage {
                    Some((a, b)) if a < total => ("206 Partial Content", a, b),
                    Some(_) => ("416 Range Not Satisfiable", 0, 0),
                    None => ("200 OK", 0, total - 1),
                };
                let tranche: &[u8] = if statut.starts_with("416") {
                    &[]
                } else {
                    &corps[debut..=fin]
                };
                let mut entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
                     Accept-Ranges: bytes\r\ntransferMode.dlna.org: Streaming\r\n",
                    tranche.len()
                );
                if statut.starts_with("206") {
                    entete.push_str(&format!("Content-Range: bytes {debut}-{fin}/{total}\r\n"));
                }
                entete.push_str("Connection: close\r\n\r\n");
                let _ = flux.write_all(entete.as_bytes()).await;
                let _ = flux.write_all(tranche).await;
                let _ = flux.shutdown().await;
            });
        }
    });
    FauxServeur {
        url: format!("http://{adresse}/MediaItems/{nom}"),
        requetes,
    }
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

/// La demande que le client envoie pour un item du serveur : le titre est le
/// nom du fichier (la Freebox n'en dit pas plus), aucune durée.
fn demande(zone_id: i64, url: &str, duration_ms: Option<i64>) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some("dlna:renderer-2062".into()),
        track_id: None,
        source: Some("upnp".into()),
        source_id: Some(url.into()),
        title: Some("01 - piste.flac".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

fn a_une_seconde_pres(obtenue: Option<i64>, attendue_ms: i64, quoi: &str) {
    let d = obtenue.unwrap_or_else(|| {
        panic!("{quoi} : durée absente — la durée du flux n'a pas été déduite des en-têtes")
    });
    assert!(
        (d - attendue_ms).abs() <= 1_000,
        "{quoi} : durée déduite {d} ms, attendue {attendue_ms} ms (± 1 s)"
    );
}

// ─── Fabrication des fichiers ───────────────────────────────────────────────

fn pcm_16_bits(cadence: u32, canaux: u32, secondes: u32) -> Vec<u8> {
    let trames = (cadence * secondes) as usize;
    let mut pcm = Vec::with_capacity(trames * canaux as usize * 2);
    for i in 0..trames {
        let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / cadence as f64).sin() * 0.3;
        for _ in 0..canaux {
            pcm.extend_from_slice(&((v * i16::MAX as f64) as i16).to_le_bytes());
        }
    }
    pcm
}

/// Un vrai FLAC, encodé par l'encodeur de Tune.
fn flac(secondes: u32) -> Vec<u8> {
    let mut e = crate::audio::encoder::AudioEncoder::new("flac", 8_000, 16, 1);
    e.start_sync().unwrap();
    e.write_sync(&pcm_16_bits(8_000, 1, secondes)).unwrap();
    e.finish_sync().unwrap()
}

/// Un vrai M4A (ALAC), `moov` APRÈS `mdat` — la tête du flux ne le contient
/// pas : il faut sauter `mdat` par une seconde requête `Range`.
fn m4a(secondes: u32) -> Vec<u8> {
    let pcm = pcm_16_bits(8_000, 1, secondes);
    let echantillons: Vec<i32> = pcm
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect();
    let m4a = crate::audio::alac_encoder::encode_alac_m4a(&echantillons, 16, 1, 8_000).unwrap();
    assert!(
        m4a.len() > 64 * 1024,
        "le banc doit placer `moov` hors de la tête lue"
    );
    m4a
}

/// En-tête de trame MPEG-1 couche III, 44,1 kHz, stéréo, sans rembourrage.
fn entete_mp3(indice_debit: u8) -> [u8; 4] {
    [0xFF, 0xFB, indice_debit << 4, 0x00]
}

/// Un MP3 à débit CONSTANT de 128 kbit/s précédé d'une balise ID3v2 plus
/// longue que la tête lue (une pochette de 100 Kio).
fn mp3_cbr(secondes: u32) -> Vec<u8> {
    let mut v = b"ID3\x04\x00\x00".to_vec();
    let corps = 100 * 1024u32;
    v.extend_from_slice(&[
        ((corps >> 21) & 0x7F) as u8,
        ((corps >> 14) & 0x7F) as u8,
        ((corps >> 7) & 0x7F) as u8,
        (corps & 0x7F) as u8,
    ]);
    v.resize(v.len() + corps as usize, 0);
    // 144 * 128000 / 44100 = 417 octets par trame, 1152 échantillons.
    let trames = secondes as usize * 44_100 / 1_152;
    for _ in 0..trames {
        v.extend_from_slice(&entete_mp3(9));
        v.resize(v.len() + 417 - 4, 0);
    }
    v
}

/// Un MP3 à débit VARIABLE annoncé par un en-tête Xing (nombre de trames).
/// Les trames qui suivent sont à 320 kbit/s : sans Xing, le calcul « au
/// débit » de la première trame (128 kbit/s) se tromperait.
fn mp3_xing(secondes: u32) -> Vec<u8> {
    let trames = (secondes as usize * 44_100).div_ceil(1_152);
    let mut v = Vec::new();
    let mut xing = vec![0u8; 417];
    xing[..4].copy_from_slice(&entete_mp3(9));
    xing[36..40].copy_from_slice(b"Xing");
    xing[40..44].copy_from_slice(&1u32.to_be_bytes());
    xing[44..48].copy_from_slice(&(trames as u32).to_be_bytes());
    v.extend_from_slice(&xing);
    // 144 * 320000 / 44100 = 1044 octets.
    for _ in 0..trames {
        v.extend_from_slice(&entete_mp3(14));
        v.resize(v.len() + 1_044 - 4, 0);
    }
    v
}

fn wav(secondes: u32) -> Vec<u8> {
    let pcm = pcm_16_bits(8_000, 1, secondes);
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8_000u32.to_le_bytes());
    v.extend_from_slice(&16_000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(&pcm);
    v
}

// ─── Les témoins ────────────────────────────────────────────────────────────

#[tokio::test]
async fn un_flac_sans_duree_prend_celle_de_son_streaminfo() {
    let s = faux_serveur("1.flac", "audio/x-flac", flac(37)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, None))
        .await
        .unwrap();
    a_une_seconde_pres(r.duration_ms, 37_000, "FLAC");
}

#[tokio::test]
async fn un_mp3_cbr_sans_duree_prend_celle_de_son_debit_apres_l_id3() {
    let s = faux_serveur("2.mp3", "audio/mpeg", mp3_cbr(61)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, Some(0)))
        .await
        .unwrap();
    a_une_seconde_pres(r.duration_ms, 61_000, "MP3 CBR (ID3v2 de 100 Kio)");
}

#[tokio::test]
async fn un_mp3_vbr_sans_duree_prend_celle_de_son_entete_xing() {
    let s = faux_serveur("3.mp3", "audio/mpeg", mp3_xing(45)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, None))
        .await
        .unwrap();
    a_une_seconde_pres(r.duration_ms, 45_000, "MP3 VBR (Xing)");
}

#[tokio::test]
async fn un_m4a_sans_duree_prend_celle_de_son_mvhd_place_apres_mdat() {
    let s = faux_serveur("4.m4a", "audio/mp4", m4a(29)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, None))
        .await
        .unwrap();
    a_une_seconde_pres(r.duration_ms, 29_000, "M4A (moov en fin de fichier)");
}

#[tokio::test]
async fn un_wav_sans_duree_prend_celle_de_son_bloc_data() {
    let s = faux_serveur("5.wav", "audio/wav", wav(12)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, None))
        .await
        .unwrap();
    a_une_seconde_pres(r.duration_ms, 12_000, "WAV");
}

/// Le témoin de non-régression : une durée CONNUE n'est ni contredite ni
/// vérifiée — pas une requête vers le serveur.
#[tokio::test]
async fn une_duree_connue_passe_telle_quelle_sans_interroger_le_serveur() {
    let s = faux_serveur("6.flac", "audio/x-flac", flac(37)).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, Some(212_000)))
        .await
        .unwrap();
    assert_eq!(r.duration_ms, Some(212_000));
    assert_eq!(
        s.requetes.load(Ordering::SeqCst),
        0,
        "une durée connue ne doit déclencher aucune sonde"
    );
}

/// Un flux illisible (pas un fichier audio) : rien n'est inventé, la piste
/// part sans durée comme avant.
#[tokio::test]
async fn un_flux_illisible_laisse_la_duree_absente() {
    let s = faux_serveur("7.bin", "audio/mpeg", vec![0x42; 200_000]).await;
    let r = orchestrateur()
        .resolve_direct_url(&demande(1, &s.url, None))
        .await
        .unwrap();
    assert_eq!(r.duration_ms, None);
}

/// La durée déduite entre dans la FILE de la zone (ce que l'interface relit),
/// sans toucher une ligne dont la durée est connue.
#[tokio::test]
async fn la_duree_deduite_est_rangee_dans_la_file() {
    use crate::db::backend::ToSqlValue;
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone 2062", Some("dlna"), None)
        .unwrap();
    let s = faux_serveur("8.flac", "audio/x-flac", flac(37)).await;
    let autre = "http://192.0.2.1/autre.flac";
    for (position, url, duree) in [(0i64, s.url.as_str(), 0i64), (1, autre, 0)] {
        orch.db
            .execute(
                "INSERT INTO queue_items (zone_id, position, source, source_id, title, duration_ms) \
                 VALUES (?, ?, 'upnp', ?, 'x', ?)",
                &[
                    &zone_id as &dyn ToSqlValue,
                    &position as &dyn ToSqlValue,
                    &url as &dyn ToSqlValue,
                    &duree as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }
    orch.resolve_direct_url(&demande(zone_id, &s.url, None))
        .await
        .unwrap();
    let duree_de = |position: i64| -> i64 {
        orch.db
            .query_one(
                "SELECT duration_ms FROM queue_items WHERE zone_id = ? AND position = ?",
                &[&zone_id as &dyn ToSqlValue, &position as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|l| l.first().and_then(|v| v.as_i64()))
            .unwrap_or(-1)
    };
    let d = duree_de(0);
    assert!(
        (d - 37_000).abs() <= 1_000,
        "la ligne de file du flux doit porter la durée déduite, lu {d}"
    );
    assert_eq!(duree_de(1), 0, "une autre ligne de la file ne bouge pas");
}

/// Le repli : la durée rapportée par la sortie (le `TrackDuration` du
/// renderer) devient celle de la piste en cours, et seulement quand elle
/// n'en avait pas.
#[tokio::test]
async fn la_duree_rapportee_par_la_sortie_est_adoptee_si_la_piste_n_en_a_pas() {
    use crate::poller::decisions::duree_rapportee_a_adopter;
    assert_eq!(
        duree_rapportee_a_adopter(Some("upnp"), 0, 245_000, false),
        Some(245_000)
    );
    assert_eq!(
        duree_rapportee_a_adopter(Some("upnp"), 212_000, 245_000, false),
        None,
        "une durée connue n'est jamais contredite"
    );
    assert_eq!(
        duree_rapportee_a_adopter(Some("radio"), 0, 245_000, false),
        None
    );
    assert_eq!(
        duree_rapportee_a_adopter(Some("upnp"), 0, 245_000, true),
        None,
        "suivante armée : le renderer peut déjà parler d'elle"
    );
    assert_eq!(duree_rapportee_a_adopter(Some("upnp"), 0, 0, false), None);

    let pm = PlaybackManager::new();
    let np = NowPlaying {
        title: "01 - piste.flac".into(),
        source: "upnp".into(),
        duration_ms: 0,
        ..Default::default()
    };
    pm.play(9, np).await;
    assert!(pm.adopter_la_duree_rapportee(9, 245_000).await);
    assert_eq!(
        pm.get_state(9).await.now_playing.unwrap().duration_ms,
        245_000
    );
    assert!(
        !pm.adopter_la_duree_rapportee(9, 1_000).await,
        "une fois connue, la durée ne bouge plus"
    );
}
