//! #9174 — la même zone navigateur : WAV chez soi, FLAC indexé par le relais.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use super::flux_a_distance_9174::{
    DELAI_A_DISTANCE, ZONES_RETENUES_MAX, provenance_d_une_requete_de_zone,
};
use super::resolve_stream::{format_dash_impose, reglages_dash};
use crate::audio::seektable_flac_9174::{
    Indexeur, blocs_de_metadonnees, ecrire_flac_indexe, indexer_les_trames,
};
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

/// Ce que fait la couche `provenance_navigateur_9174` d'une requête réussie.
fn requete(orch: &PlaybackOrchestrator, methode: &str, chemin: &str, relayee: bool) {
    if let Some((zone, a_distance)) = provenance_d_une_requete_de_zone(methode, chemin, relayee) {
        orch.noter_provenance_de_zone(zone, a_distance);
    }
}

fn zone_navigateur(orch: &PlaybackOrchestrator) -> i64 {
    ZoneRepo::with_backend(orch.db.clone())
        .create("Ce téléphone", Some("browser"), None)
        .unwrap()
}

#[test]
fn la_meme_zone_navigateur_recoit_du_wav_chez_soi_et_du_flac_par_le_relais() {
    let orch = orchestrateur();
    let zone = zone_navigateur(&orch);
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

    // Le téléphone revient sur le Wi-Fi de la maison.
    requete(
        &orch,
        "POST",
        &format!("/api/v1/zones/{zone}/browser-next"),
        false,
    );
    assert_eq!(format_servi(&orch, zone), "wav");
}

/// Une télécommande en 5G qui lit l'état de la zone (ou la commande) ne dit
/// rien de l'endroit où joue son lecteur.
#[test]
fn une_telecommande_relayee_ne_met_pas_la_zone_a_distance() {
    let orch = orchestrateur();
    let zone = zone_navigateur(&orch);
    for (m, c) in [
        ("GET", format!("/api/v1/zones/{zone}/status")),
        ("POST", format!("/api/v1/zones/{zone}/play")),
        ("POST", format!("/api/v1/zones/{zone}/volume")),
        ("GET", format!("/api/v1/zones/{zone}/browser-playing")),
    ] {
        requete(&orch, m, &c, true);
        assert_eq!(format_servi(&orch, zone), "wav", "{m} {c}");
    }
    // Et la même télécommande, locale, ne ramène pas un lecteur distant.
    requete(
        &orch,
        "POST",
        &format!("/api/v1/zones/{zone}/browser-playing"),
        true,
    );
    requete(&orch, "GET", &format!("/api/v1/zones/{zone}/status"), false);
    assert_eq!(format_servi(&orch, zone), "flac");
}

/// Un lecteur qui s'est tu depuis plus de 30 s n'emporte plus la zone.
#[test]
fn sans_signal_relaye_la_zone_revient_en_local() {
    let orch = orchestrateur();
    let zones = orch.zones_a_distance();
    let t0 = Instant::now();
    zones.noter(7, true, t0);
    assert!(zones.a_distance(7, t0 + DELAI_A_DISTANCE - Duration::from_secs(1)));
    assert!(
        !zones.a_distance(7, t0 + DELAI_A_DISTANCE + Duration::from_secs(1)),
        "après le délai, la zone doit revenir en local"
    );
    // Un signal relayé la remet à distance, pour un nouveau délai.
    let t1 = t0 + DELAI_A_DISTANCE + Duration::from_secs(5);
    zones.noter(7, true, t1);
    assert!(zones.a_distance(7, t1 + Duration::from_secs(20)));
}

#[test]
fn les_zones_retenues_sont_bornees() {
    let orch = orchestrateur();
    let zones = orch.zones_a_distance();
    let t0 = Instant::now();
    for z in 0..(ZONES_RETENUES_MAX as i64 + 20) {
        zones.noter(z, true, t0 + Duration::from_millis(z as u64));
    }
    assert_eq!(zones.nombre(), ZONES_RETENUES_MAX);
    // La plus récente est gardée, la plus ancienne oubliée.
    let fin = t0 + Duration::from_secs(1);
    assert!(zones.a_distance(ZONES_RETENUES_MAX as i64 + 19, fin));
    assert!(!zones.a_distance(0, fin));
}

#[test]
fn une_zone_reseau_n_est_pas_concernee() {
    let orch = orchestrateur();
    let zone = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("uuid:salon"))
        .unwrap();
    orch.noter_provenance_de_zone(zone, true);
    // Hors navigateur, le relais ne change rien : la sonde du renderer décide.
    assert_eq!(
        format_dash_impose(
            false,
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
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/7/browser-playing", true),
        Some((7, true))
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/7/browser-playing", false),
        Some((7, false))
    );
    // Le chemin vu sous `/zones`, par la couche du routeur des zones.
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/7/browser-next", true),
        Some((7, true))
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("GET", "/api/v1/zones/7/status", true),
        None
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/7/play", true),
        None
    );
    assert_eq!(
        provenance_d_une_requete_de_zone("POST", "/api/v1/zones/now-listening", true),
        None
    );
}

// ─────────────────────────── SEEKTABLE ───────────────────────────
//
// Les fixtures sont de VRAIS FLAC libFLAC stéréo : leur affectation des
// canaux change d'une trame à l'autre (16/44,1 : 1, puis 10 ; 24/96 : 9,
// puis 10). Comparer l'affectation entière, et non le nombre de canaux,
// abandonnait l'index dès la deuxième trame.

const FIXTURES: [&str; 2] = ["ref_16_44100_stereo.flac", "ref_24_96000_stereo.flac"];

fn fixture(nom: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/flac/{nom}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Les blocs (type, corps) et l'octet de la première trame.
fn blocs(flac: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    let (b, debut) = blocs_de_metadonnees(flac).expect("FLAC");
    (b.into_iter().map(|(k, c)| (k, c.to_vec())).collect(), debut)
}

/// Les points de la SEEKTABLE : (échantillon, décalage, échantillons).
fn points(table: &[u8]) -> Vec<(u64, u64, u16)> {
    assert_eq!(table.len() % 18, 0);
    table
        .chunks(18)
        .map(|p| {
            (
                u64::from_be_bytes(p[0..8].try_into().unwrap()),
                u64::from_be_bytes(p[8..16].try_into().unwrap()),
                u16::from_be_bytes([p[16], p[17]]),
            )
        })
        .collect()
}

fn pcm(chemin: &std::path::Path) -> Vec<i32> {
    crate::audio::decode::decode_to_pcm(chemin.to_str().unwrap(), None, None, 0.0, 0.0)
        .unwrap()
        .samples_i32
}

/// Le FLAC libFLAC passé par `remux_flac_dash` (en fMP4, comme TIDAL le
/// livre) sort avec une SEEKTABLE, des trames intactes et le même PCM.
#[test]
fn un_flac_libflac_remuxe_pour_le_relais_porte_sa_seektable() {
    use super::tidal_cd_dash_jusqu_au_play_7266::{segment_init, segment_media, trames_flac};
    for nom in FIXTURES {
        let flac = fixture(nom);
        let (info, trames) = trames_flac(&flac);
        let fmp4 = [segment_init(&info), segment_media(1, 0, &trames)].concat();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("piste.mp4");
        std::fs::write(&source, &fmp4).unwrap();
        let sans = dir.path().join("sans.flac");
        let avec = dir.path().join("avec.flac");
        let s = source.to_str().unwrap();
        crate::audio::decode::remux_flac_dash_indexe(s, sans.to_str().unwrap(), false).unwrap();
        let (taille, _, _) =
            crate::audio::decode::remux_flac_dash_indexe(s, avec.to_str().unwrap(), true).unwrap();

        let brut = std::fs::read(&sans).unwrap();
        let indexe = std::fs::read(&avec).unwrap();
        assert_eq!(taille, indexe.len() as u64);
        let (b_sans, d_sans) = blocs(&brut);
        assert!(
            b_sans.iter().all(|(k, _)| *k != 3),
            "{nom} : sans index demandé"
        );
        let (b_avec, d_avec) = blocs(&indexe);
        assert_eq!(b_avec[0].0, 0, "{nom} : STREAMINFO d'abord");
        assert_eq!(
            b_avec.get(1).map(|b| b.0),
            Some(3),
            "{nom} : la SEEKTABLE doit suivre STREAMINFO (index abandonné ?)"
        );
        assert_eq!(points(&b_avec[1].1), vec![(0, 0, 4096)], "{nom}");
        assert_eq!(
            &indexe[d_avec..],
            &brut[d_sans..],
            "{nom} : trames intactes"
        );
        assert_eq!(pcm(&avec), pcm(&sans), "{nom} : même PCM");
    }
}

/// Le ré-encodage : les trames retrouvées dans le tampon (CRC-16), un point
/// par trame avec un intervalle d'une trame — TOUTES les trames du flux.
#[test]
fn chaque_trame_libflac_est_retrouvee_malgre_l_affectation_variable() {
    for nom in FIXTURES {
        let flac = fixture(nom);
        let (b, debut) = blocs(&flac);
        let info = &b[0].1;
        let total = (((info[13] & 0x0F) as u64) << 32)
            | u32::from_be_bytes([info[14], info[15], info[16], info[17]]) as u64;
        let attendues = total.div_ceil(4096);
        let mut ix = Indexeur::depuis_streaminfo(info)
            .unwrap()
            .avec_intervalle(4096);
        assert!(
            indexer_les_trames(&flac, debut, &mut ix),
            "{nom} : une trame décorrélée autrement que la première a été refusée"
        );
        let p = points(ix.table());
        assert_eq!(p.len() as u64, attendues, "{nom} : {p:?}");
        for (k, &(echantillon, decalage, _)) in p.iter().enumerate() {
            assert_eq!(echantillon, k as u64 * 4096, "{nom}");
            let t = &flac[debut + decalage as usize..];
            assert_eq!(
                (t[0], t[1] & 0xFE),
                (0xFF, 0xF8),
                "{nom} : point {k} hors trame"
            );
        }
    }
}

/// Le chemin du ré-encodage écrit le fichier indexé en flux ; un tampon que
/// l'index ne sait pas lire est écrit tel quel, sans faire échouer la piste.
#[test]
fn le_flac_reencode_est_ecrit_indexe_ou_tel_quel() {
    let dir = tempfile::tempdir().unwrap();
    for nom in FIXTURES {
        let flac = fixture(nom);
        let sortie = dir.path().join(nom);
        let taille = ecrire_flac_indexe(sortie.to_str().unwrap(), &flac).unwrap();
        let ecrit = std::fs::read(&sortie).unwrap();
        assert_eq!(taille, ecrit.len() as u64);
        let (b, _) = blocs(&ecrit);
        assert_eq!(b[1].0, 3, "{nom}");
        assert_eq!(b.iter().filter(|(k, _)| *k == 3).count(), 1, "{nom}");
        let source = dir.path().join(format!("source-{nom}"));
        std::fs::write(&source, &flac).unwrap();
        assert_eq!(pcm(&sortie), pcm(&source), "{nom}");
    }
    // Un faux FLAC : écrit tel quel.
    let faux = dir.path().join("faux.flac");
    let octets = b"fLaC\x80\x00\x00\x02xx".to_vec();
    assert_eq!(
        ecrire_flac_indexe(faux.to_str().unwrap(), &octets).unwrap(),
        octets.len() as u64
    );
    assert_eq!(std::fs::read(&faux).unwrap(), octets);
    // Une trame corrompue (CRC-16 faux) : l'index renonce, le fichier part.
    let mut abime = fixture(FIXTURES[0]);
    let n = abime.len();
    abime[n - 1] ^= 0xFF;
    let sortie = dir.path().join("abime.flac");
    let taille = ecrire_flac_indexe(sortie.to_str().unwrap(), &abime).unwrap();
    assert_eq!(std::fs::read(&sortie).unwrap(), abime);
    assert_eq!(taille, n as u64);
}
