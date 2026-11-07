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

// ---------------------------------------------------------------------------
// #7100 × #7642 — le relais qui RELÈVE l'écrêtage doit traiter le flux comme
// le relais sans relevé : `process_flux`, l'état du convolveur passant d'un
// tampon à l'autre. Avant, la branche du relevé appelait `process` sur chaque
// tampon, ce qui remettait le convolveur à zéro à chaque tampon (#7642
// défait sur la bibliothèque et les services, les deux chemins qui relèvent).
// ---------------------------------------------------------------------------

const BLOC_CONV: usize = 1_024;

fn convolveur(ir: Vec<f32>) -> super::StreamingDsp {
    super::StreamingDsp {
        convolver: Some(crate::audio::convolver::Convolver::new(
            &[ir.clone(), ir],
            BLOC_CONV,
        )),
        channels: 2,
        sample_rate: CADENCE,
        ..Default::default()
    }
}

fn i16_en_octets(x: &[i16]) -> Vec<u8> {
    x.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Passe `x` (16 bits stéréo) dans le relais QUI RELÈVE, en tampons de
/// `tailles` trames répétées, et rend tout ce qui en sort après l'en-tête.
async fn par_le_relais_qui_releve(
    dsp: super::StreamingDsp,
    x: &[i16],
    tailles: &[usize],
    registre: Arc<EcretagesDeTraitement>,
) -> Vec<i16> {
    let (aval_tx, mut aval_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let releve = ReleveDEcretage::new(registre, 0.0, CADENCE, 2, 16);
    let amont = super::spawn_streaming_dsp_relay_releve(dsp, 16, true, aval_tx, Some(releve));
    let lecteur = tokio::spawn(async move {
        let mut tout = Vec::new();
        let premier = aval_rx.recv().await.expect("en-tête attendu");
        assert_eq!(&premier[0..4], b"RIFF", "l'en-tête doit passer intact");
        while let Some(t) = aval_rx.recv().await {
            tout.extend_from_slice(&t);
        }
        tout
    });
    amont.send(b"RIFF....WAVEfmt ".to_vec()).await.unwrap();
    let trames = x.len() / 2;
    let mut debut = 0usize;
    let mut i = 0usize;
    while debut < trames {
        let fin = trames.min(debut + tailles[i % tailles.len()]);
        amont
            .send(i16_en_octets(&x[debut * 2..fin * 2]))
            .await
            .unwrap();
        debut = fin;
        i += 1;
    }
    drop(amont);
    lecteur
        .await
        .unwrap()
        .chunks_exact(2)
        .map(|p| i16::from_le_bytes([p[0], p[1]]))
        .collect()
}

/// LE témoin : des impulsions isolées à travers une IR à deux taps (300 et
/// 1 500 trames, queue plus longue que le bloc et que les tampons). Le relais
/// qui relève doit rendre la convolution exacte, comme celui qui ne relève
/// pas (#7642).
#[tokio::test]
async fn i7100_le_relais_qui_releve_garde_l_etat_du_convolveur_7642() {
    let mut h = vec![0.0f32; 2_000];
    h[300] = 0.5;
    h[1_500] = 1.0;
    let trames = 8_000;
    let mut x = vec![0i16; trames * 2];
    for (t, c, v) in [
        (900, 0, 8_192i16),
        (3_100, 0, 8_192),
        (5_000, 0, -8_192),
        (1_200, 1, 8_192),
        (4_321, 1, -8_192),
    ] {
        x[t * 2 + c] = v;
    }
    let mut attendu = vec![0i16; x.len()];
    for c in 0..2 {
        for n in 0..trames {
            let mut acc = 0.0f64;
            for (k, &hk) in h.iter().enumerate() {
                if hk != 0.0 && k <= n {
                    acc += f64::from(hk) * f64::from(x[(n - k) * 2 + c]);
                }
            }
            attendu[n * 2 + c] = acc.round() as i16;
        }
    }
    let registre = Arc::new(EcretagesDeTraitement::new());
    let obtenu = par_le_relais_qui_releve(
        convolveur(h),
        &x,
        &[700, 1_500, 333, 2_048, 1_000, 64],
        registre.clone(),
    )
    .await;
    assert_eq!(
        obtenu.len(),
        attendu.len(),
        "#7642 — le relais qui relève doit rendre autant d'échantillons qu'il en reçoit"
    );
    let ecarts: Vec<(usize, i16, i16)> = obtenu
        .iter()
        .zip(&attendu)
        .enumerate()
        .filter(|(_, (o, a))| (i32::from(**o) - i32::from(**a)).abs() > 2)
        .map(|(i, (o, a))| (i / 2, *o, *a))
        .take(8)
        .collect();
    assert!(
        ecarts.is_empty(),
        "#7100 × #7642 — le relais qui relève remet le convolveur à zéro entre deux \
         tampons, la sortie diverge de la convolution directe (trame, obtenu, attendu) : {ecarts:?}"
    );
    assert_eq!(
        registre.ecretage_entre(0.0, 1_000.0),
        (false, false),
        "aucun échantillon n'atteint le rail : rien à relever"
    );
}

/// Derrière un convolveur (IR identité, latence d'un bloc), chaque
/// échantillon sorti est comparé à SA source, pas à l'octet de même rang du
/// tampon d'entrée : une source au rail que la chaîne laisse au rail n'est pas
/// un écrêtage ; un gain ×2 qui pousse un 0,6 au rail en est un, au bon
/// endroit de la piste.
#[tokio::test]
async fn i7100_derriere_le_convolveur_le_releve_compare_chaque_echantillon_a_sa_source() {
    let mut identite = vec![0.0f32; 64];
    identite[0] = 1.0;

    // 1) Alternance silence / rail par plages de 1 500 trames (34 ms).
    let trames = 22_050;
    let mut x = vec![0i16; trames * 2];
    for t in 0..trames {
        if (t / 1_500) % 2 == 1 {
            let v = if t % 2 == 0 { i16::MAX } else { i16::MIN };
            x[t * 2] = v;
            x[t * 2 + 1] = v;
        }
    }
    let registre = Arc::new(EcretagesDeTraitement::new());
    let obtenu = par_le_relais_qui_releve(
        convolveur(identite.clone()),
        &x,
        &[700, 333, 2_048],
        registre.clone(),
    )
    .await;
    assert_eq!(obtenu.len(), x.len());
    assert_eq!(
        registre.ecretage_entre(0.0, 1_000.0),
        (false, false),
        "la source au rail, rendue telle quelle par l'IR identité, n'est pas un écrêtage du traitement"
    );

    // 2) Silence 0,25 s puis 0,25 s à 0,6 ; ReplayGain ×2 avant le convolveur.
    let moitie = trames / 2;
    let mut y = vec![0i16; trames * 2];
    for v in y.iter_mut().skip(moitie * 2) {
        *v = (0.6 * 32_767.0) as i16;
    }
    let mut dsp = convolveur(identite);
    dsp.replaygain = Some(2.0);
    let registre = Arc::new(EcretagesDeTraitement::new());
    par_le_relais_qui_releve(dsp, &y, &[700, 333, 2_048], registre.clone()).await;
    assert_eq!(
        registre.ecretage_entre(0.0, 240.0),
        (false, false),
        "le silence du début n'est pas écrêté : le relevé doit rester à sa place dans la piste"
    );
    assert_eq!(
        registre.ecretage_entre(260.0, 500.0),
        (true, true),
        "le 0,6 poussé à 1,2 par le gain est écrêté au rail : il doit être relevé"
    );
}
