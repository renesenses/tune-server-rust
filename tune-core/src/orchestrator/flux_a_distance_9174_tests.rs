//! #9174 — la même zone navigateur : WAV chez soi, FLAC indexé par le relais.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use super::flux_a_distance_9174::provenance_d_une_requete_de_zone;
use super::resolve_stream::{format_dash_impose, reglages_dash};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        Some("127.0.0.1".into()),
    )
}

/// Le format du pré-transcodage DASH que reçoit la zone, par la même décision
/// que `preparer_le_dash` / `choisir_l_encodage_dash` (zone sans appareil,
/// FLAC natif non forcé : la sonde du renderer n'est jamais posée).
fn format_servi(orch: &PlaybackOrchestrator, zone: i64) -> &'static str {
    let navigateur = ZoneRepo::with_backend(orch.db.clone())
        .get(zone)
        .unwrap()
        .unwrap()
        .output_type
        .as_deref()
        == Some("browser");
    let a_distance = navigateur && orch.zone_tiree_a_distance(zone);
    format_dash_impose(
        navigateur,
        reglages_dash(&orch.db, zone, 24),
        true,
        false,
        a_distance,
    )
    .expect("sans appareil, le format est imposé")
}

/// Ce que fait `garde_du_relais` pour chaque requête.
fn requete(orch: &PlaybackOrchestrator, methode: &str, chemin: &str, relayee: bool) {
    if let Some((zone, a_distance)) = provenance_d_une_requete_de_zone(methode, chemin, relayee) {
        orch.noter_provenance_de_zone(zone, a_distance);
    }
}

#[test]
fn la_meme_zone_navigateur_recoit_du_wav_chez_soi_et_du_flac_par_le_relais() {
    let orch = orchestrateur();
    let zone = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce téléphone", Some("browser"), None)
        .unwrap();
    let signal = format!("/api/v1/zones/{zone}/browser-playing");

    // Chez soi : le lecteur parle au serveur directement.
    requete(&orch, "POST", &signal, false);
    assert_eq!(
        format_servi(&orch, zone),
        "wav",
        "en local, la zone garde son WAV"
    );

    // En 5G : le même lecteur, par le relais.
    requete(&orch, "POST", &signal, true);
    assert_eq!(
        format_servi(&orch, zone),
        "flac",
        "tirée par le relais, la zone doit recevoir un format compressé"
    );

    // Une télécommande du salon qui lit l'état de la zone ne dit rien du
    // lecteur : la zone reste servie à distance.
    requete(&orch, "GET", &format!("/api/v1/zones/{zone}/status"), false);
    assert_eq!(format_servi(&orch, zone), "flac");

    // Le téléphone revient sur le Wi-Fi de la maison.
    requete(
        &orch,
        "POST",
        &format!("/api/v1/zones/{zone}/browser-next"),
        false,
    );
    assert_eq!(format_servi(&orch, zone), "wav");
}

#[test]
fn une_zone_reseau_n_est_pas_concernee() {
    let orch = orchestrateur();
    let zone = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("uuid:salon"))
        .unwrap();
    let navigateur = false;
    orch.noter_provenance_de_zone(zone, true);
    // Hors navigateur, le relais ne change rien : la sonde du renderer décide.
    assert_eq!(
        format_dash_impose(
            navigateur,
            reglages_dash(&orch.db, zone, 24),
            false,
            false,
            orch.zone_tiree_a_distance(zone),
        ),
        None
    );
}

#[test]
fn provenance_des_requetes() {
    assert_eq!(
        provenance_d_une_requete_de_zone("GET", "/api/v1/zones/7/status", true),
        Some((7, true))
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/7/browser-playing", false),
        Some((7, false))
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/7/play", false),
        None
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("GET", "/api/v1/zones/7/browser-playing", false),
        None
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/now-listening", true),
        None
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("GET", "/stream/abc.wav", true),
        None
    );
}

/// Le bras DASH décide bien avec la provenance de la zone, au cache chaud
/// comme à la lecture, et indexe le FLAC qu'il sert à distance.
#[test]
fn le_bras_dash_consulte_la_provenance_de_la_zone() {
    let site = include_str!("resolve_stream.rs");
    assert!(
        site.contains(
            "let a_distance = is_browser_output && self.zone_tiree_a_distance(req.zone_id);"
        ),
        "le bras DASH doit savoir si la zone navigateur est tirée à distance"
    );
    assert!(site.contains("let indexer = a_distance && dash_enc_format == \"flac\";"));
    assert_eq!(
        site.matches("depart_natif::indexer_un_flac(").count()
            + site.matches("depart_natif::avec_seektable(").count(),
        2,
        "remux ET ré-encodage doivent indexer le FLAC servi à distance"
    );
}

// ─────────────────────────── SEEKTABLE ───────────────────────────

const HZ: u32 = 44_100;

fn flac_sans_index(secondes: u32) -> Vec<u8> {
    let n = (HZ * secondes) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    let mut x: u32 = 1;
    for i in 0..n {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let g = ((i as f32 * 0.05).sin() * 8_000.0) as i16 ^ ((x >> 20) as i16 & 0x3F);
        pcm.extend_from_slice(&g.to_le_bytes());
        pcm.extend_from_slice(&g.wrapping_neg().to_le_bytes());
    }
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", HZ, 16, 2);
    enc.start_sync().unwrap();
    enc.write_sync(&pcm).unwrap();
    enc.finish_sync().unwrap()
}

/// Les blocs de métadonnées : (type, corps), et l'octet de la première trame.
fn blocs(flac: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    let mut out = Vec::new();
    let mut pos = 4;
    loop {
        let h = &flac[pos..pos + 4];
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        out.push((h[0] & 0x7F, flac[pos + 4..pos + 4 + len].to_vec()));
        pos += 4 + len;
        if h[0] & 0x80 != 0 {
            return (out, pos);
        }
    }
}

#[test]
fn le_flac_servi_a_distance_porte_une_seektable_juste() {
    let brut = flac_sans_index(35);
    let (avant, _) = blocs(&brut);
    assert!(
        avant.iter().all(|(k, _)| *k != 3),
        "l'encodeur natif n'écrit pas de SEEKTABLE"
    );

    let indexe = crate::audio::depart_natif::avec_seektable(&brut).expect("FLAC lisible");
    let (apres, debut) = blocs(&indexe);
    assert_eq!(apres[0].0, 0, "STREAMINFO d'abord");
    assert_eq!(apres[1].0, 3, "SEEKTABLE juste après");
    let table = &apres[1].1;
    assert_eq!(table.len() % 18, 0);
    let points: Vec<(u64, u64, u16)> = table
        .chunks(18)
        .map(|p| {
            (
                u64::from_be_bytes(p[0..8].try_into().unwrap()),
                u64::from_be_bytes(p[8..16].try_into().unwrap()),
                u16::from_be_bytes([p[16], p[17]]),
            )
        })
        .collect();
    // 0, 10, 20 et 30 s : un point par tranche de 10 s, sur la première trame
    // qui la commence.
    assert_eq!(points.len(), 4, "{points:?}");
    for (k, &(echantillon, decalage, dans_la_trame)) in points.iter().enumerate() {
        let cible = k as u64 * 10 * HZ as u64;
        assert!(
            echantillon >= cible && echantillon < cible + 4096,
            "point {k} : {echantillon}"
        );
        assert_eq!(dans_la_trame, 4096);
        // Le décalage désigne un en-tête de trame réel, de ce numéro-là.
        let t = &indexe[debut + decalage as usize..];
        assert_eq!((t[0], t[1] & 0xFE), (0xFF, 0xF8), "point {k} hors trame");
        assert_eq!(echantillon % 4096, 0);
    }
    // Les trames sont celles de l'original, octet pour octet.
    let (_, debut_brut) = blocs(&brut);
    assert_eq!(&indexe[debut..], &brut[debut_brut..]);

    // Et le fichier se décode toujours, au même PCM.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("brut.flac");
    let b = dir.path().join("indexe.flac");
    std::fs::write(&a, &brut).unwrap();
    std::fs::write(&b, &indexe).unwrap();
    let pa =
        crate::audio::decode::decode_to_pcm(a.to_str().unwrap(), None, None, 0.0, 0.0).unwrap();
    let pb =
        crate::audio::decode::decode_to_pcm(b.to_str().unwrap(), None, None, 0.0, 0.0).unwrap();
    assert_eq!(pa.samples_i32, pb.samples_i32);

    // Indexer un fichier déjà indexé ne double pas la table.
    let taille = crate::audio::depart_natif::indexer_un_flac(b.to_str().unwrap()).unwrap();
    let relu = std::fs::read(&b).unwrap();
    assert_eq!(taille, relu.len() as u64);
    let (encore, _) = blocs(&relu);
    assert_eq!(encore.iter().filter(|(k, _)| *k == 3).count(), 1);
}

#[test]
fn ce_qui_n_est_pas_un_flac_reste_tel_quel() {
    assert!(crate::audio::depart_natif::avec_seektable(b"RIFF\0\0\0\0WAVE").is_none());
    assert!(crate::audio::depart_natif::avec_seektable(b"fLaC").is_none());
}
