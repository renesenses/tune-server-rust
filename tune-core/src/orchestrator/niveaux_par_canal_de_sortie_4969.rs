//! #4969 (fil 1929) — le bargraphe multicanal suit ce qui SORT.
//!
//! `playback.audio_levels` publie un niveau par canal depuis #5937, mais dans
//! l'ordre de la SOURCE. Sur une sortie locale, la source traverse encore la
//! réaffectation des canaux (#6044) ou le routage par la disposition déclarée
//! (#6057) : un FL ↔ FR réaffecté laissait la barre FL allumée, un 4.0 ouvert
//! en six voies ne montrait que quatre barres.
//!
//! Banc sur la chaîne réelle (forwarder cadencé → bus), nourrie par un VRAI
//! fichier WAV 5.1 (WAVE_FORMAT_EXTENSIBLE, masque 0x3F) écrit sur disque :
//! six segments, un seul canal sonore à la fois, à −6 dBFS. Chaque segment
//! doit allumer UNE barre, la bonne, et elle seule.

use std::sync::Arc;

use crate::audio::carte_des_canaux::{CarteDesCanaux, adapter_vers_la_sortie};
use crate::audio::disposition_canaux::{self, Disposition};
use crate::audio::reaffectation_canaux::{ChannelRemapSettings, Matrice};
use crate::playback::{NowPlaying, PlaybackManager};

const CADENCE: u32 = 48_000;
/// 100 ms par segment : deux fenêtres et demie de niveaux.
const TRAMES_PAR_SEGMENT: usize = 4_800;
const CRETE_ATTENDUE_DB: f64 = -6.02;

/// Un WAV WAVE_FORMAT_EXTENSIBLE de `masque` (un canal par bit), 16 bits,
/// 48 kHz : un segment par canal, un sinus 1 kHz à −6 dBFS sur ce seul canal.
fn wav_un_canal_a_la_fois(masque: u32) -> Vec<u8> {
    let canaux = masque.count_ones() as usize;
    let mut data = Vec::with_capacity(canaux * TRAMES_PAR_SEGMENT * canaux * 2);
    for sonore in 0..canaux {
        for n in 0..TRAMES_PAR_SEGMENT {
            let phase = 2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / f64::from(CADENCE);
            let v = (0.5 * phase.sin() * f64::from(i16::MAX)) as i16;
            for c in 0..canaux {
                let s = if c == sonore { v } else { 0 };
                data.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
    let bloc = (canaux * 2) as u16;
    let mut fmt = Vec::with_capacity(40);
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&(canaux as u16).to_le_bytes());
    fmt.extend_from_slice(&CADENCE.to_le_bytes());
    fmt.extend_from_slice(&(CADENCE * u32::from(bloc)).to_le_bytes());
    fmt.extend_from_slice(&bloc.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&masque.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_PCM
    fmt.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ]);
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    wav.extend_from_slice(&fmt);
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

/// Écrire le fichier, vérifier ce qu'il déclare (lu par #6057), et rendre
/// le PCM de chacun de ses segments.
fn segments_du_fichier(masque: u32) -> (Disposition, Vec<Vec<u8>>) {
    let dossier = tempfile::tempdir().expect("tempdir");
    let chemin = dossier.path().join("un-canal-a-la-fois.wav");
    std::fs::write(&chemin, wav_un_canal_a_la_fois(masque)).expect("écriture");
    let declaree = disposition_canaux::lire_le_fichier(&chemin)
        .expect("le fichier déclare sa disposition (WAVE_FORMAT_EXTENSIBLE)");
    let octets = std::fs::read(&chemin).expect("lecture");
    let debut = octets
        .windows(4)
        .position(|w| w == b"data")
        .expect("bloc data")
        + 8;
    let canaux = usize::from(declaree.canaux());
    let segments = octets[debut..]
        .chunks_exact(TRAMES_PAR_SEGMENT * canaux * 2)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    assert_eq!(segments.len(), canaux, "un segment par canal");
    (declaree, segments)
}

/// Le premier `playback.audio_levels` publié pour ce PCM, avec `carte`
/// branchée comme la sortie locale la brancherait.
async fn evenement(
    zone_id: i64,
    pcm: &[u8],
    canaux: u16,
    carte: Option<CarteDesCanaux>,
) -> serde_json::Value {
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    if let Some(carte) = carte {
        playback.brancher_la_carte_des_canaux(zone_id, Arc::new(move || Some(carte.clone())));
    }
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let tx = super::spawn_paced_levels_forwarder(bus.clone(), playback, zone_id, play_seq, 0);
    assert!(crate::audio::tap::send_windowed_pcm(
        &tx, pcm, 16, canaux, CADENCE
    ));
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => return ev.data,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("bus : {e:?}"),
            Err(_) => panic!("aucun playback.audio_levels publié en 5 s"),
        }
    }
}

/// La seule barre allumée de l'évènement, à −6 dBFS ; toutes les autres au
/// plancher.
fn barre_allumee(ev: &serde_json::Value) -> usize {
    let barres = ev["channel_levels"]
        .as_array()
        .unwrap_or_else(|| panic!("aucun channel_levels : {ev}"));
    let allumees: Vec<usize> = (0..barres.len())
        .filter(|&b| barres[b]["peak_db"].as_f64().expect("peak_db") > -90.0)
        .collect();
    assert_eq!(allumees.len(), 1, "une seule barre allumée attendue : {ev}");
    let b = allumees[0];
    let crete = barres[b]["peak_db"].as_f64().expect("peak_db");
    assert!(
        (crete - CRETE_ATTENDUE_DB).abs() < 0.1,
        "barre {b} : {crete} dB, attendu {CRETE_ATTENDUE_DB}"
    );
    b
}

fn carte(
    source: u16,
    sortie: u16,
    matrice: Option<&Matrice>,
    declaree: Option<&Disposition>,
) -> CarteDesCanaux {
    CarteDesCanaux::depuis_adaptation(source, sortie, |s| {
        adapter_vers_la_sortie(s, source, sortie, matrice, declaree)
    })
    .expect("carte")
}

const NOMS_5_1: [&str; 6] = ["FL", "FR", "FC", "LFE", "BL", "BR"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_5_1_synthetique_allume_la_barre_de_chaque_canal_dans_l_ordre() {
    let (declaree, segments) = segments_du_fichier(0x3F);
    assert!(declaree.est_par_defaut(), "5.1 : FL FR FC LFE BL BR");
    for (c, pcm) in segments.iter().enumerate() {
        let ev = evenement(984_980 + c as i64, pcm, 6, None).await;
        assert_eq!(barre_allumee(&ev), c, "segment du canal {}", NOMS_5_1[c]);
        assert_eq!(ev["channel_names"], serde_json::json!(NOMS_5_1));
    }
}

/// Sortie locale ouverte en six voies, rien de réaffecté : les six barres
/// décrivent la sortie, et l'évènement le dit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sur_une_sortie_5_1_les_barres_decrivent_les_six_voies_qui_sortent() {
    let (_, segments) = segments_du_fichier(0x3F);
    for (c, pcm) in segments.iter().enumerate() {
        let ev = evenement(984_990 + c as i64, pcm, 6, Some(carte(6, 6, None, None))).await;
        assert_eq!(barre_allumee(&ev), c);
        assert_eq!(
            ev["output_channels"], 6,
            "les barres doivent se dire mesurées à la sortie (#4969) : {ev}"
        );
        assert_eq!(ev["channel_names"], serde_json::json!(NOMS_5_1));
    }
}

/// Une réaffectation qui échange l'avant et l'arrière (FL ↔ FR, BL ↔ BR) :
/// chaque segment allume la barre de la voie où il SORT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_reaffectation_deplace_chaque_barre_sur_la_voie_ou_le_canal_sort() {
    // Sortie o ← entrée destination[o].
    let destination = [1usize, 0, 2, 3, 5, 4];
    let reglage = ChannelRemapSettings {
        enabled: true,
        inputs: 6,
        outputs: 6,
        gains_db: (0..6)
            .map(|o| {
                (0..6)
                    .map(|i| (i == destination[o]).then_some(0.0))
                    .collect()
            })
            .collect(),
        normalize: true,
        preset: None,
    };
    let m = Matrice::du_reglage_arme(&reglage).expect("matrice");
    let (_, segments) = segments_du_fichier(0x3F);
    for (c, pcm) in segments.iter().enumerate() {
        let ev = evenement(
            985_000 + c as i64,
            pcm,
            6,
            Some(carte(6, 6, Some(&m), None)),
        )
        .await;
        let attendue = destination.iter().position(|&i| i == c).expect("voie");
        assert_eq!(
            barre_allumee(&ev),
            attendue,
            "le canal {} sort sur {} : sa barre doit y être",
            NOMS_5_1[c],
            NOMS_5_1[attendue]
        );
    }
}

/// Un 4.0 (FL FR BL BR) vers un ampli ouvert en six voies : le routage par
/// position (#6057) le pose sur FL FR BL BR du 5.1 ; FC et LFE restent
/// éteints, et l'écran a six barres, pas quatre.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_4_0_ouvert_en_six_voies_allume_bl_et_br_et_montre_six_barres() {
    let (declaree, segments) = segments_du_fichier(0x33);
    assert_eq!(declaree.canaux(), 4);
    let voies = [0usize, 1, 4, 5];
    for (c, pcm) in segments.iter().enumerate() {
        let ev = evenement(
            985_010 + c as i64,
            pcm,
            4,
            Some(carte(4, 6, None, Some(&declaree))),
        )
        .await;
        assert_eq!(
            ev["channel_levels"].as_array().map(Vec::len),
            Some(6),
            "{ev}"
        );
        assert_eq!(barre_allumee(&ev), voies[c]);
        assert_eq!(ev["channel_names"], serde_json::json!(NOMS_5_1));
    }
}

/// Un 5.1 replié vers une sortie stéréo : ce qui sort est stéréo, l'écran
/// garde ses deux aiguilles — aucun champ par canal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_repli_vers_la_stereo_ne_publie_aucune_barre() {
    let (_, segments) = segments_du_fichier(0x3F);
    let ev = evenement(985_020, &segments[0], 6, Some(carte(6, 2, None, None))).await;
    assert!(
        ev.get("channel_levels").is_none(),
        "repli stéréo : pas de bargraphe, {ev}"
    );
    assert!(ev.get("output_channels").is_none());
    assert!(ev["peak_left_db"].as_f64().is_some());
}
