//! #7100 (Alex, 26/10/2026 : Qobuz FLAC 44,1/24, Docker, client web) : le
//! crête-mètre annonçait un écrêtage à 20-30 % de volume.
//!
//! `over_*` décrivait la SOURCE : trois échantillons consécutifs à pleine
//! échelle dans le fichier, mesurés au décodeur, avant le volume de l'appareil
//! et avant la chaîne DSP du relais. Un master limité à 0 dBFS allumait donc
//! le rouge quel que soit le volume.
//!
//! Bancs de COMPORTEMENT sur la chaîne réelle : relais progressif
//! (`spawn_streaming_dsp_relay_releve`), puis forwarder cadencé, puis bus. La
//! valeur PUBLIÉE est celle qu'on lit.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::audio::ecretage_de_traitement::{EcretagesDeTraitement, ReleveDEcretage};
use crate::playback::{NowPlaying, PlaybackManager};

const CADENCE: u32 = 44_100;

/// 0,5 s de stéréo 24 bits collée au rail négatif, comme la crête aplatie d'un
/// master « loudness » : la source est à 0 dBFS, et `over_run` y dépasse de
/// loin les trois échantillons de l'OVER.
fn source_a_0_dbfs() -> Vec<u8> {
    let rail = (-8_388_608_i32).to_le_bytes();
    let mut pcm = Vec::with_capacity(22_050 * 6);
    for _ in 0..22_050 * 2 {
        pcm.extend_from_slice(&rail[..3]);
    }
    pcm
}

/// 0,5 s d'un sinus à 80 Hz, à −0,45 dBFS, en stéréo 16 bits. Il n'est nulle
/// part au rail : la source n'écrête pas.
fn grave_fort() -> Vec<u8> {
    let mut pcm = Vec::with_capacity(22_050 * 4);
    for i in 0..22_050 {
        let s =
            (2.0 * std::f64::consts::PI * 80.0 * f64::from(i) / f64::from(CADENCE)).sin() * 0.95;
        let v = (s * 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

fn egaliseur_qui_pousse() -> crate::audio::eq::EqProcessor {
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bass_gain_db: 12.0,
        ..Default::default()
    };
    crate::audio::eq::EqProcessor::new(&profile, CADENCE, 2)
}

/// Fait passer `pcm` dans le relais (en-tête WAV factice d'abord, comme le
/// décodeur), en relevant l'écrêtage dans `registre`, et attend la sortie.
async fn traverser_le_relais(
    dsp: super::StreamingDsp,
    pcm: &[u8],
    profondeur: u16,
    registre: Arc<EcretagesDeTraitement>,
) {
    let (aval_tx, mut aval_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let releve = ReleveDEcretage::new(registre, 0.0, CADENCE, 2, profondeur);
    let amont =
        super::spawn_streaming_dsp_relay_releve(dsp, profondeur, true, aval_tx, Some(releve));
    amont.send(b"RIFF....WAVEfmt ".to_vec()).await.unwrap();
    let bloc = 4096 * usize::from(profondeur / 8) * 2;
    let mut attendus = 1;
    for morceau in pcm.chunks(bloc) {
        amont.send(morceau.to_vec()).await.unwrap();
        attendus += 1;
    }
    drop(amont);
    for _ in 0..attendus {
        aval_rx
            .recv()
            .await
            .expect("le relais a rendu tous ses blocs");
    }
}

/// Publie `pcm` (la fenêtre prélevée AU DÉCODEUR) dans la chaîne réelle et
/// rend chaque `(over_left, over_right, source_over_left)` publié.
async fn publier(
    zone_id: i64,
    pcm: &[u8],
    profondeur: u16,
    gain_de_sortie: Option<u32>,
    ecretages: Option<Arc<EcretagesDeTraitement>>,
) -> Vec<(bool, bool, bool)> {
    let playback = Arc::new(PlaybackManager::new());
    if let Some(g) = gain_de_sortie {
        playback.brancher_le_gain_de_sortie(zone_id, Arc::new(AtomicU32::new(g)));
    }
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let tx = super::spawn_paced_levels_forwarder_avec_ecretages(
        bus.clone(),
        playback.clone(),
        zone_id,
        play_seq,
        0,
        ecretages,
    );
    crate::audio::tap::send_windowed_pcm(&tx, pcm, profondeur, 2, CADENCE);
    drop(tx);
    let mut publies = Vec::new();
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            break;
        }
        match tokio::time::timeout(reste.min(std::time::Duration::from_secs(2)), rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => publies.push((
                ev.data["over_left"].as_bool().expect("over_left"),
                ev.data["over_right"].as_bool().expect("over_right"),
                ev.data["source_over_left"]
                    .as_bool()
                    .expect("source_over_left (#7100)"),
            )),
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(!publies.is_empty(), "aucun playback.audio_levels publié");
    publies
}

/// Le cas d'Alex : une source à 0 dBFS, volume à 25 %. Aucune alerte
/// d'écrêtage, ni sur une sortie locale (volume logiciel, gain ×0,25 déclaré)
/// ni sur une zone réseau (volume de l'appareil, aucun gain déclaré, relais
/// sans traitement actif). La crête de la source reste dite, à part.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i7100_une_source_a_0_dbfs_a_25_pour_cent_n_allume_pas_l_ecretage() {
    let source = source_a_0_dbfs();

    let locale = publier(987_100, &source, 24, Some(250), None).await;
    assert!(
        locale.iter().all(|(g, d, _)| !g && !d),
        "sortie locale à 25 % : la source à 0 dBFS ne doit PAS allumer over_*, publié {locale:?}"
    );
    assert!(
        locale.iter().all(|(_, _, s)| *s),
        "la source à pleine échelle doit rester dite dans source_over_*, publié {locale:?}"
    );

    let registre = Arc::new(EcretagesDeTraitement::new());
    traverser_le_relais(
        super::StreamingDsp::default(),
        &source,
        24,
        registre.clone(),
    )
    .await;
    let reseau = publier(987_101, &source, 24, None, Some(registre)).await;
    assert!(
        reseau.iter().all(|(g, d, _)| !g && !d),
        "zone réseau (volume de l'appareil à 25 %) : over_* décrivait la SOURCE, publié {reseau:?}"
    );
    assert!(reseau.iter().all(|(_, _, s)| *s));
}

/// Le gain de correction d'une zone réseau (ReplayGain, préampli : facteur ×2,
/// soit +6 dB) pousse un grave à −0,45 dBFS au-delà du plein : le relais
/// l'écrête au rail (`apply_gain_pcm`, clamp dur, T9 #2218). Cette fois
/// l'alerte est due, alors que la source n'est jamais à pleine échelle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i7100_un_gain_de_correction_qui_pousse_au_dela_du_plein_allume_l_ecretage() {
    let source = grave_fort();
    let registre = Arc::new(EcretagesDeTraitement::new());
    let dsp = super::StreamingDsp {
        replaygain: Some(2.0),
        channels: 2,
        sample_rate: CADENCE,
        ..Default::default()
    };
    traverser_le_relais(dsp, &source, 16, registre.clone()).await;
    let publies = publier(987_102, &source, 16, None, Some(registre)).await;
    assert!(
        publies.iter().any(|(g, d, _)| *g && *d),
        "le gain de correction écrête la sortie : over_* doit s'allumer, publié {publies:?}"
    );
    assert!(
        publies.iter().all(|(_, _, s)| !s),
        "la source n'est jamais à pleine échelle : source_over_* doit rester éteint, publié {publies:?}"
    );
}

/// L'égaliseur intégré pousse le même grave de +12 dB. En mode « Sûr » (le
/// défaut), sa réserve automatique (norme L1 de la cascade) garantit qu'il
/// n'atteint jamais le rail : aucune alerte, et c'est la vérité. Ce banc
/// garde que le relevé ne confond pas « le signal a changé » et « il a
/// écrêté ».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i7100_l_egaliseur_en_mode_sur_ne_declenche_pas_de_fausse_alerte() {
    let source = grave_fort();
    let registre = Arc::new(EcretagesDeTraitement::new());
    let dsp = super::StreamingDsp {
        eq: Some(egaliseur_qui_pousse()),
        channels: 2,
        sample_rate: CADENCE,
        ..Default::default()
    };
    traverser_le_relais(dsp, &source, 16, registre.clone()).await;
    let publies = publier(987_103, &source, 16, None, Some(registre)).await;
    assert!(
        publies.iter().all(|(g, d, _)| !g && !d),
        "l'égaliseur en mode sûr n'écrête pas : over_* doit rester éteint, publié {publies:?}"
    );
}
