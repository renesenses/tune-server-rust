//! #4384 (GgB, 0.9.152, Lenovo X230, fil 1797) — le crête-mètre d'une zone
//! locale publie la crête des échantillons tels qu'ils partent vers le DAC.
//!
//! Depuis #4450 et #4685, le forwarder multipliait la fenêtre prélevée AU
//! DÉCODEUR par le gain de rendu et par le gain MOYEN de l'égaliseur et du
//! crossfeed. Exact pour un scalaire (volume, préampli), faux pour un filtre :
//! une bande à −12 dB sur la fréquence de la crête baisse la crête de 12 dB,
//! le gain moyen de la courbe d'à peine un demi-dB. L'aiguille ne bougeait
//! donc pas quand le son, lui, baissait de 12 dB.
//!
//! Banc de COMPORTEMENT sur la chaîne réelle (forwarder cadencé → bus) : la
//! sortie locale est représentée par ce qu'elle partage réellement avec le
//! `PlaybackManager` — son gain de rendu, son gain moyen de DSP et, depuis ce
//! correctif, le registre des crêtes relevées après son DSP.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::audio::crete_de_sortie::CretesDeSortie;
use crate::playback::{NowPlaying, PlaybackManager};

const CADENCE: u32 = 44_100;
const AMPLITUDE: f64 = 0.9;
/// −12 dB en linéaire : ce qu'une bande d'égaliseur à −12 dB fait à un sinus
/// posé sur sa fréquence centrale.
const MOINS_12_DB: f64 = 0.251_188_643;

fn sinus(amplitude: f64, trames: u32) -> Vec<f64> {
    (0..trames)
        .map(|n| {
            amplitude
                * (2.0 * std::f64::consts::PI * 1_000.0 * f64::from(n) / f64::from(CADENCE)).sin()
        })
        .collect()
}

/// PCM 16 bits stéréo du DÉCODEUR (avant tout DSP).
fn pcm_source(trames: u32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(trames as usize * 4);
    for s in sinus(AMPLITUDE, trames) {
        let v = (s * 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

/// Les échantillons f32 que la boucle producteur voit APRÈS l'égaliseur.
fn apres_egaliseur(trames: u32) -> Vec<f32> {
    sinus(AMPLITUDE * MOINS_12_DB, trames)
        .into_iter()
        .flat_map(|s| [s as f32, s as f32])
        .collect()
}

fn db(lineaire: f64) -> f64 {
    20.0 * lineaire.log10()
}

/// `(peak_left_db, peak_hold_left_db)` de la première fenêtre publiée.
async fn mesurer(
    zone_id: i64,
    rendu: u32,
    gain_moyen_dsp: u32,
    releve: Option<Arc<CretesDeSortie>>,
) -> (f64, f64) {
    let playback = Arc::new(PlaybackManager::new());
    playback.brancher_le_gain_de_sortie(zone_id, Arc::new(AtomicU32::new(rendu)));
    playback.brancher_le_gain_moyen_du_dsp(zone_id, Arc::new(AtomicU32::new(gain_moyen_dsp)));
    if let Some(cretes) = releve {
        playback.brancher_les_cretes_de_sortie(zone_id, cretes);
    }
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    crate::audio::tap::send_windowed_pcm(&levels_tx, &pcm_source(4_410), 16, 2, CADENCE);

    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        assert!(!reste.is_zero(), "aucun playback.audio_levels publié");
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => {
                return (
                    ev.data["peak_left_db"].as_f64().expect("peak_left_db"),
                    ev.data["peak_hold_left_db"]
                        .as_f64()
                        .expect("peak_hold_left_db"),
                );
            }
            Ok(Ok(_)) => {}
            autre => panic!("bus muet : {autre:?}"),
        }
    }
}

/// Le registre tel que la boucle producteur l'a rempli : 100 ms de piste, à
/// partir de 0, après une bande d'égaliseur à −12 dB sur 1 kHz.
fn releve_apres_egaliseur() -> Arc<CretesDeSortie> {
    let cretes = Arc::new(CretesDeSortie::new());
    cretes.relever(&apres_egaliseur(4_410), 4_410, CADENCE, 0.0);
    cretes
}

/// Gain moyen de la courbe « −12 dB à 1 kHz, Q 1 » que le forwarder reportait
/// jusqu'ici, arrondi au millième : ~ −0,45 dB. C'est lui qui laissait
/// l'aiguille à la crête du fichier.
const GAIN_MOYEN_DE_LA_COURBE: u32 = 950;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4384_moins_12_db_d_egaliseur_sur_la_crete_baissent_l_aiguille_de_12_db() {
    let (crete, tenue) = mesurer(
        984_384,
        1000,
        GAIN_MOYEN_DE_LA_COURBE,
        Some(releve_apres_egaliseur()),
    )
    .await;
    let attendu = db(AMPLITUDE) - 12.0;
    assert!(
        (crete - attendu).abs() < 0.3,
        "−12 dB d'égaliseur sur la crête ⇒ {attendu:.2} dBFS attendus, publié {crete:.2} \
         (la crête du fichier au gain moyen près vaudrait {:.2})",
        db(AMPLITUDE * f64::from(GAIN_MOYEN_DE_LA_COURBE) / 1000.0)
    );
    assert!(
        (tenue - attendu).abs() < 0.3,
        "la crête TENUE suit la même mesure, publié {tenue:.2}"
    );
}

/// Le gain de rendu (volume, ReplayGain, préampli) s'applique par-dessus, et
/// le gain moyen du DSP ne s'y ajoute PAS une seconde fois : il est déjà dans
/// les échantillons relevés.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4384_le_gain_de_rendu_s_ajoute_a_la_crete_apres_dsp_sans_le_gain_moyen() {
    let (crete, _) = mesurer(
        984_385,
        500,
        GAIN_MOYEN_DE_LA_COURBE,
        Some(releve_apres_egaliseur()),
    )
    .await;
    let attendu = db(AMPLITUDE) - 12.0 - 6.02;
    assert!(
        (crete - attendu).abs() < 0.3,
        "−12 dB d'égaliseur puis volume ×0,5 ⇒ {attendu:.2} dBFS, publié {crete:.2}"
    );
}

/// Sans registre (chemin compressé décodé d'un bloc, bras exclusifs, sortie
/// réseau), la mesure d'avant est conservée telle quelle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i4384_sans_releve_la_mesure_d_avant_reste() {
    let (crete, _) = mesurer(984_386, 500, GAIN_MOYEN_DE_LA_COURBE, None).await;
    let attendu = db(AMPLITUDE * 0.5 * f64::from(GAIN_MOYEN_DE_LA_COURBE) / 1000.0);
    assert!(
        (crete - attendu).abs() < 0.2,
        "sans relevé : source × rendu × gain moyen = {attendu:.2}, publié {crete:.2}"
    );
}
