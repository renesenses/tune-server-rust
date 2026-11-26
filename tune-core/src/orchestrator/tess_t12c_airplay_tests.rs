//! TESS T12c — AirPlay (flux fabriqué par Tune) sous la chaîne TESS, et les
//! sorties hors TESS (décision Q8, amendée le 23/11 : elles gardent
//! l'égaliseur et le ReplayGain).
//!
//! 1. Décision réelle (`decider_la_lecture_locale`) : sur une zone AirPlay 2
//!    ou Devialet AIR, le crossfeed SEUL force le flux traité par le relais ;
//!    sur les sorties hors TESS, un égaliseur armé s'applique toujours.
//! 2. De bout en bout, sur les OCTETS : la vraie porte de lecture
//!    (`PlaybackOrchestrator::play`), la vraie sortie AirPlay 1
//!    ([`AirplayOutput`]), un faux récepteur RTSP/RTP sur la boucle locale qui
//!    garde le PCM L16 de chaque paquet, et le faux serveur `/stream`
//!    ([`super::faux_serveur_de_flux_tess`]). Source à voie droite muette :
//!    crossfeed → la droite reçue s'allume ; Pure → la source à l'échantillon
//!    près ; et le premier paquet RTP part aussi vite avec la chaîne que sans.
//!
//! Aucun son, rien hors de `127.0.0.1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::faux_serveur_de_flux_tess as banc;
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::airplay::AirplayOutput;

const CADENCE: u32 = 44_100;
const TRAMES: usize = 26_460;
const RTP_ENTETE: usize = 12;

fn orchestrateur(streamer: Arc<crate::http::streamer::AudioStreamer>) -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        streamer,
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.advertised_ip = Some("127.0.0.1".into());
    orch.license = Some(crate::license::licence_premium_pour_test(db));
    orch
}

fn demande(zone_id: i64, appareil: &str, track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(appareil.into()),
        track_id: Some(track_id),
        source: Some("local".into()),
        duration_ms: Some((TRAMES as i64 * 1_000) / i64::from(CADENCE)),
        media_format: Some("wav".into()),
        sample_rate: Some(CADENCE),
        bit_depth: Some(16),
        ..Default::default()
    }
}

fn armer_l_egaliseur(orch: &PlaybackOrchestrator, zone_id: i64) {
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 6.0,
            q: 1.0,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&profil).unwrap(),
    )
    .unwrap();
    assert!(
        orch.zone_has_active_eq(zone_id),
        "prémisse : égaliseur actif"
    );
}

// ── 1. La décision réelle ──────────────────────────────────────────────

/// 🔴 → 🟢 Crossfeed SEUL sur AirPlay 1, AirPlay 2 et Devialet AIR : la piste
/// passe par le relais, qui exécute le crossfeed. Avant T12c, elle partait
/// telle quelle (le crossfeed seul ne forçait rien sur une sortie PULL).
#[tokio::test]
async fn crossfeed_seul_force_le_flux_traite_sur_airplay_et_air_t12c() {
    for (type_, appareil) in [
        ("airplay", "airplay-127.0.0.1-7000"),
        ("airplay2", "airplay2:salon-t12c"),
        ("devialet_air", "devialet_air:expert-t12c"),
    ] {
        let orch = orchestrateur(Arc::new(crate::http::streamer::AudioStreamer::new(0)));
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Salon T12c", Some(type_), Some(appareil))
            .unwrap();
        let (wav, _) = banc::wav_gauche_seule("t12c-decision", CADENCE, 4_410);
        let track_id = banc::inscrire_la_piste(&orch.db, wav.as_str().as_ref(), CADENCE, 100);
        let req = demande(zone_id, appareil, track_id);
        assert!(
            !orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : sans chaîne, la piste part telle quelle"
        );
        banc::armer_le_crossfeed(&orch, zone_id);
        assert!(
            orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "T12c ({type_}) : le crossfeed seul doit forcer le flux traité"
        );
        let (actif, relais, crossfeed) = orch.relais_dsp_pour_test(&req).await.unwrap();
        assert!(
            actif && relais && crossfeed,
            "T12c ({type_}) : le relais doit exécuter le crossfeed ({actif}, {relais}, {crossfeed})"
        );
        // Pure : plus rien n'est traité, la piste repart telle quelle.
        banc::armer_pure(&orch, zone_id);
        assert!(
            !orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : Pure, rien n'est traité"
        );
    }
}

/// 🔴 → 🟢 Décision du 23/11 : HQPlayer et l'autre transport hors TESS
/// GARDENT l'égaliseur et le ReplayGain (#1216). Égaliseur armé : la piste
/// passe par le relais, qui l'exécute. Rouge sur la première version de T12c,
/// qui les retirait.
#[tokio::test]
async fn hqplayer_avec_egaliseur_l_applique_t12c() {
    for (type_, appareil) in [
        ("hqplayer", "hqplayer:salon-t12c"),
        ("diretta", "diretta:salon-t12c"),
    ] {
        let orch = orchestrateur(Arc::new(crate::http::streamer::AudioStreamer::new(0)));
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Hors TESS T12c", Some(type_), Some(appareil))
            .unwrap();
        let (wav, _) = banc::wav_gauche_seule("t12c-hors-tess", CADENCE, 4_410);
        let track_id = banc::inscrire_la_piste(&orch.db, wav.as_str().as_ref(), CADENCE, 100);
        let req = demande(zone_id, appareil, track_id);
        assert!(
            !orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : sans traitement, la piste part telle quelle"
        );
        armer_l_egaliseur(&orch, zone_id);
        assert!(
            orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : l'égaliseur armé doit s'appliquer (#1216, décision du 23/11)"
        );
        let (actif, relais, crossfeed) = orch.relais_dsp_pour_test(&req).await.unwrap();
        assert!(
            actif && relais,
            "{type_} : le relais doit exécuter l'égaliseur ({actif}, {relais})"
        );
        assert!(!crossfeed, "{type_} : crossfeed non coché");
        assert!(orch.zone_traite_le_flux_hors_tess(zone_id), "{type_}");
        // Crossfeed avec l'égaliseur : le même relais l'exécute aussi.
        banc::armer_le_crossfeed(&orch, zone_id);
        let (actif, relais, crossfeed) = orch.relais_dsp_pour_test(&req).await.unwrap();
        assert!(
            actif && relais && crossfeed,
            "{type_} : avec l'égaliseur, le crossfeed passe par le relais \
             ({actif}, {relais}, {crossfeed})"
        );
    }
}

/// Une réponse impulsionnelle WAV float 32 mono à 44,1 kHz (la cadence du
/// flux) : un Dirac de gain 0,5, que le convolveur accepte.
fn ecrire_ri() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-t12c-44k", ".wav");
    let taps = 32usize;
    let donnees = taps * 4;
    let mut wav = Vec::with_capacity(44 + donnees);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CADENCE.to_le_bytes());
    wav.extend_from_slice(&(CADENCE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.5 } else { 0.0 };
        wav.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), wav).unwrap();
    f
}

/// Décision du 23/11 — la correction de pièce et le ReplayGain restent
/// appliqués sur HQPlayer et l'autre transport hors TESS, comme l'égaliseur.
#[tokio::test]
async fn hqplayer_correction_de_piece_et_replaygain_s_appliquent_t12c() {
    for (type_, appareil) in [
        ("hqplayer", "hqplayer:salon-rc-t12c"),
        ("diretta", "diretta:salon-rc-t12c"),
    ] {
        // Correction de pièce.
        let orch = orchestrateur(Arc::new(crate::http::streamer::AudioStreamer::new(0)));
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Hors TESS RC T12c", Some(type_), Some(appareil))
            .unwrap();
        let (wav, _) = banc::wav_gauche_seule("t12c-hors-tess-rc", CADENCE, 4_410);
        let track_id = banc::inscrire_la_piste(&orch.db, wav.as_str().as_ref(), CADENCE, 100);
        let req = demande(zone_id, appareil, track_id);
        let ri = ecrire_ri();
        SettingsRepo::with_backend(orch.db.clone())
            .set(&format!("ir_path_{zone_id}"), ri.as_str().as_ref())
            .unwrap();
        assert!(orch.zone_has_active_ir(zone_id), "prémisse : RI réglée");
        assert!(
            orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : la correction de pièce doit s'appliquer (décision du 23/11)"
        );
        assert!(orch.zone_traite_le_flux_hors_tess(zone_id), "{type_}");

        // ReplayGain : un gain tagué sur la piste ET le mode armé.
        let orch = orchestrateur(Arc::new(crate::http::streamer::AudioStreamer::new(0)));
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Hors TESS RG T12c", Some(type_), Some(appareil))
            .unwrap();
        let (wav, _) = banc::wav_gauche_seule("t12c-hors-tess-rg", CADENCE, 4_410);
        let track_id = banc::inscrire_la_piste(&orch.db, wav.as_str().as_ref(), CADENCE, 100);
        let req = demande(zone_id, appareil, track_id);
        assert!(!orch.transcodage_decide_pour_test(&req).await.unwrap());
        crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(orch.db.clone())
            .set(track_id, "rg_track_gain", "-6.00 dB")
            .unwrap();
        SettingsRepo::with_backend(orch.db.clone())
            .set(crate::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        assert!(
            orch.transcodage_decide_pour_test(&req).await.unwrap(),
            "{type_} : le ReplayGain doit s'appliquer (décision du 23/11)"
        );
        assert!(orch.zone_traite_le_flux_hors_tess(zone_id), "{type_}");
    }
}

/// Contre-épreuve : AirPlay 2 avec l'égaliseur garde le flux traité (#1216).
#[tokio::test]
async fn airplay2_avec_l_egaliseur_garde_le_flux_traite_t12c() {
    let orch = orchestrateur(Arc::new(crate::http::streamer::AudioStreamer::new(0)));
    let appareil = "airplay2:salon-eq-t12c";
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon EQ T12c", Some("airplay2"), Some(appareil))
        .unwrap();
    let (wav, _) = banc::wav_gauche_seule("t12c-eq", CADENCE, 4_410);
    let track_id = banc::inscrire_la_piste(&orch.db, wav.as_str().as_ref(), CADENCE, 100);
    armer_l_egaliseur(&orch, zone_id);
    assert!(
        orch.transcodage_decide_pour_test(&demande(zone_id, appareil, track_id))
            .await
            .unwrap()
    );
}

// ── 2. De bout en bout, sur un faux récepteur AirPlay 1 ───────────────────

#[derive(Default)]
struct Releve {
    record: Option<Instant>,
    premier_rtp: Option<Instant>,
    /// `(numéro de séquence, charge L16)`.
    paquets: Vec<(u16, Vec<u8>)>,
}

/// Un faux récepteur AirPlay 1 : RTSP (200 à tout, port audio annoncé au
/// SETUP), et chaque datagramme audio gardé.
async fn faux_recepteur_airplay() -> (u16, Arc<std::sync::Mutex<Releve>>) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let audio = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port_audio = audio.local_addr().unwrap().port();
    let releve = Arc::new(std::sync::Mutex::new(Releve::default()));

    let r = releve.clone();
    tokio::spawn(async move {
        let mut tampon = vec![0u8; 4_096];
        while let Ok(n) = audio.recv(&mut tampon).await {
            if n <= RTP_ENTETE {
                continue;
            }
            let seq = u16::from_be_bytes([tampon[2], tampon[3]]);
            let mut r = r.lock().unwrap();
            r.premier_rtp.get_or_insert_with(Instant::now);
            r.paquets.push((seq, tampon[RTP_ENTETE..n].to_vec()));
        }
    });

    let r = releve.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = ecoute.accept().await {
            let r = r.clone();
            tokio::spawn(async move {
                let mut lecteur = BufReader::new(socket);
                loop {
                    let mut ligne = String::new();
                    if lecteur.read_line(&mut ligne).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let methode = ligne.split_whitespace().next().unwrap_or("").to_owned();
                    let mut taille = 0usize;
                    loop {
                        let mut entete = String::new();
                        if lecteur.read_line(&mut entete).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let entete = entete.trim_end().to_owned();
                        if entete.is_empty() {
                            break;
                        }
                        if let Some((cle, valeur)) = entete.split_once(':')
                            && cle.trim().eq_ignore_ascii_case("Content-Length")
                        {
                            taille = valeur.trim().parse().unwrap_or(0);
                        }
                    }
                    if taille > 0 {
                        let mut corps = vec![0u8; taille];
                        if lecteur.read_exact(&mut corps).await.is_err() {
                            return;
                        }
                    }
                    let supplement = if methode == "SETUP" {
                        format!(
                            "Session: 1\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
                             server_port={port_audio};control_port=0;timing_port=0\r\n"
                        )
                    } else {
                        String::new()
                    };
                    let reponse = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: 1\r\n{supplement}Content-Length: 0\r\n\r\n"
                    );
                    if lecteur
                        .get_mut()
                        .write_all(reponse.as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if methode == "RECORD" {
                        r.lock().unwrap().record.get_or_insert_with(Instant::now);
                    }
                }
            });
        }
    });
    (port, releve)
}

#[derive(Clone, Copy, Debug)]
enum Chaine {
    Aucune,
    Crossfeed,
    CrossfeedSousPure,
}

struct Lecture {
    trames: Vec<[i32; 2]>,
    source: Vec<[i16; 2]>,
    /// RECORD → premier paquet RTP.
    premier_paquet: Duration,
}

async fn jouer_sur_airplay(chaine: Chaine) -> Lecture {
    let orch = orchestrateur(banc::demarrer().await);
    let (port, releve) = faux_recepteur_airplay().await;
    let appareil = format!("airplay-127.0.0.1-{port}");
    let sortie = AirplayOutput::new(
        "Banc T12c".into(),
        appareil.clone(),
        "127.0.0.1".into(),
        port,
    );
    orch.outputs.lock().await.register(Box::new(sortie));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon AirPlay T12c", Some("airplay"), Some(&appareil))
        .unwrap();
    let (wav, source) = banc::wav_gauche_seule("t12c-airplay", CADENCE, TRAMES);
    let track_id = banc::inscrire_la_piste(
        &orch.db,
        wav.as_str().as_ref(),
        CADENCE,
        (TRAMES as i64 * 1_000) / i64::from(CADENCE),
    );
    match chaine {
        Chaine::Aucune => {}
        Chaine::Crossfeed => banc::armer_le_crossfeed(&orch, zone_id),
        Chaine::CrossfeedSousPure => {
            banc::armer_le_crossfeed(&orch, zone_id);
            banc::armer_pure(&orch, zone_id);
        }
    }
    let r = orch.play(demande(zone_id, &appareil, track_id)).await;
    assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());

    let mut vu = 0usize;
    let mut calme = 0u32;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = releve.lock().unwrap().paquets.len();
        if n > 0 && n == vu {
            calme += 1;
            if calme >= 15 {
                break;
            }
        } else {
            calme = 0;
        }
        vu = n;
    }
    if let Some(s) = { orch.outputs.lock().await.get(&appareil) } {
        let _ = s.lock().await.stop().await;
    }
    let r = releve.lock().unwrap();
    let record = r.record.expect("le faux récepteur a répondu à RECORD");
    let premier = r.premier_rtp.expect("aucun paquet RTP reçu");
    // L'ordre d'émission : les numéros de séquence, sans doublon.
    let mut paquets = r.paquets.clone();
    let base = paquets[0].0;
    paquets.sort_by_key(|(seq, _)| seq.wrapping_sub(base));
    paquets.dedup_by_key(|(seq, _)| *seq);
    let trames = paquets
        .iter()
        .flat_map(|(_, charge)| {
            charge
                .chunks_exact(4)
                .map(|t| {
                    [
                        i32::from(i16::from_be_bytes([t[0], t[1]])),
                        i32::from(i16::from_be_bytes([t[2], t[3]])),
                    ]
                })
                .collect::<Vec<_>>()
        })
        .collect();
    Lecture {
        trames,
        source,
        premier_paquet: premier.saturating_duration_since(record),
    }
}

/// Le délai que le récepteur tolère entre RECORD et le premier RTP (#6673).
const DELAI_TOLERE: Duration = Duration::from_millis(2_500);

/// Crossfeed sur AirPlay 1 : la voie droite muette de la source s'allume
/// dans les paquets RTP, et le premier paquet part sans attendre la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossfeed_sur_airplay_allume_la_voie_droite_t12c() {
    let l = jouer_sur_airplay(Chaine::Crossfeed).await;
    let c = banc::constater(&l.trames, 16, &l.source, 1.0);
    println!(
        "T12c crossfeed AirPlay 1 : {c:?}, premier paquet {:?}",
        l.premier_paquet
    );
    assert!(
        c.droite_max > 500,
        "T12c : le crossfeed doit allumer la voie droite des paquets RTP ({c:?})"
    );
    assert!(
        l.premier_paquet < DELAI_TOLERE,
        "latence : premier RTP {:?} après RECORD avec la chaîne",
        l.premier_paquet
    );
}

/// Pure, crossfeed coché : les paquets portent la source à l'échantillon près.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_sur_airplay_rien_n_est_traite_t12c() {
    let l = jouer_sur_airplay(Chaine::CrossfeedSousPure).await;
    let c = banc::constater(&l.trames, 16, &l.source, 1.0);
    println!(
        "T12c Pure AirPlay 1 : {c:?}, premier paquet {:?}",
        l.premier_paquet
    );
    assert!(c.identique, "Pure : la source à l'échantillon près ({c:?})");
}

/// Contre-épreuve : sans chaîne, la source à l'échantillon près, et la même
/// latence au premier paquet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_chaine_airplay_recoit_la_source_t12c() {
    let l = jouer_sur_airplay(Chaine::Aucune).await;
    let c = banc::constater(&l.trames, 16, &l.source, 1.0);
    println!(
        "T12c sans chaîne AirPlay 1 : {c:?}, premier paquet {:?}",
        l.premier_paquet
    );
    assert!(c.identique, "{c:?}");
    assert!(l.premier_paquet < DELAI_TOLERE);
}
