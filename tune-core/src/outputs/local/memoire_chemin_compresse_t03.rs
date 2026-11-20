//! TESS T03 — la mémoire du chemin compressé, qui fait passer la piste
//! ENTIÈRE décodée par `apply_local_dsp` d'un seul appel. Relecture de #9024 :
//! le premier socle y copiait toute la piste en `f64` et dimensionnait un
//! tampon de vidange à sa taille (×5 en pointe pour 6 min). La chaîne travaille
//! désormais par tranches bornées dans le tampon du fil, sans copie quand
//! aucun étage n'agit.

use super::*;
use std::sync::Mutex;

/// 6 minutes à 48 kHz, stéréo : 34,56 M d'échantillons, 138 Mo en `f32`.
const TRAMES: usize = 48_000 * 360;

fn egaliseur() -> crate::audio::eq::EqProcessor {
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 2000.0,
            gain: -6.0,
            q: 0.71,
            band_type: "high_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    crate::audio::eq::EqProcessor::new(&profile, 48_000, 2)
}

/// Pic d'octets alloués par `apply_local_dsp` sur la piste entière.
fn pic(eq: bool, crossfeed: bool) -> usize {
    let mut piste: Vec<f32> = (0..TRAMES * 2)
        .map(|i| 0.25 * ((i / 2) as f32 * 0.01).sin())
        .collect();
    let eq = Mutex::new(eq.then(egaliseur));
    let convolver = Mutex::new(None);
    let cf = Mutex::new(
        crossfeed.then(|| crate::audio::crossfeed::CrossfeedProcessor::new(48_000, 0.6, 0.3)),
    );
    let (pure, mono) = (AtomicBool::new(false), AtomicBool::new(false));
    let ((), pic) = super::ringbuf_tests::pic_d_octets_alloues(|| {
        apply_local_dsp(&mut piste, &eq, &convolver, &cf, &pure, &mono, 2, false);
    });
    pic
}

/// Mesure (sortie `T03-MEMOIRE`, lue avec `--nocapture`) et plafonds : sans
/// étage actif, la chaîne n'alloue RIEN ; avec égaliseur et crossfeed, pas
/// plus de 2 Mio pour une piste de 138 Mo (tampons du fil, bornés).
#[test]
fn le_chemin_compresse_ne_copie_pas_la_piste() {
    let sans_dsp = pic(false, false);
    let avec_dsp = pic(true, true);
    eprintln!(
        "T03-MEMOIRE piste={} octets, pic sans DSP={} octets, pic EQ+crossfeed={} octets",
        TRAMES * 2 * 4,
        sans_dsp,
        avec_dsp
    );
    assert_eq!(sans_dsp, 0, "sans DSP, la chaîne ne doit rien allouer");
    assert!(
        avec_dsp < 2 << 20,
        "EQ + crossfeed : {avec_dsp} octets alloués"
    );
}
