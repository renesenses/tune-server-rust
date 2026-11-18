//! Preuves de T03 sur la chaîne locale réelle (moteurs d'origine, sortie
//! factice, aucun son).

use super::*;
use crate::audio::convolver::Convolver;
use crate::audio::crossfeed::CrossfeedProcessor;
use crate::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use crate::tess::QuantificationPcm;
use crate::tess::etage::{EtapeFinale, SortieFinale};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

// ---------------------------------------------------------------------------
// L'ANCIEN chemin, recopié tel quel (corps d'`apply_local_dsp` et de
// `flush_local_dsp` avant T03) : l'oracle de la non-régression et la
// contre-épreuve « 64 bits réels ».
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn ancien_apply_local_dsp(
    samples: &mut [f32],
    eq: &Mutex<Option<EqProcessor>>,
    convolver: &Mutex<Option<Convolver>>,
    crossfeed: &Mutex<Option<CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    channels: u16,
    dop: bool,
) {
    if dop || pure_bypass.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut e) = eq.lock()
        && let Some(ref mut p) = *e
    {
        p.process_interleaved(samples);
        if p.est_neutre_au_repos() {
            *e = None;
        }
    }
    if let Ok(mut conv) = convolver.lock()
        && let Some(ref mut c) = *conv
    {
        c.process_interleaved(samples);
    }
    if channels == 2
        && let Ok(mut cf) = crossfeed.lock()
        && let Some(ref mut c) = *cf
    {
        c.process_interleaved(samples);
    }
    if channels == 2 && mono_downmix.load(Ordering::Relaxed) {
        crate::audio::channels::fold_stereo_to_mono_in_place(samples);
    }
}

fn ancien_flush_local_dsp(
    convolver: &Mutex<Option<Convolver>>,
    crossfeed: &Mutex<Option<CrossfeedProcessor>>,
    mono_downmix: &AtomicBool,
    channels: u16,
) -> Vec<f32> {
    let mut queue = match convolver.lock() {
        Ok(mut c) => match c.as_mut() {
            Some(conv) => conv.flush(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    if queue.is_empty() {
        return queue;
    }
    if channels == 2
        && let Ok(mut cf) = crossfeed.lock()
        && let Some(c) = cf.as_mut()
    {
        c.process_interleaved(&mut queue);
    }
    if channels == 2 && mono_downmix.load(Ordering::Relaxed) {
        crate::audio::channels::fold_stereo_to_mono_in_place(&mut queue);
    }
    queue
}

// ---------------------------------------------------------------------------
// Moteurs réels et signaux de test
// ---------------------------------------------------------------------------

fn egaliseur() -> EqProcessor {
    let profile = EqProfile {
        enabled: true,
        bands: vec![
            EqBandSpec {
                freq: 2000.0,
                gain: -12.0,
                q: 0.71,
                band_type: "high_shelf".into(),
                ..Default::default()
            },
            EqBandSpec {
                freq: 120.0,
                gain: 6.0,
                q: 1.0,
                band_type: "peaking".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    EqProcessor::new(&profile, 44_100, 2)
}

/// Réponse impulsionnelle stéréo courte, non triviale (écho et atténuation).
fn convolveur() -> Convolver {
    let mut g = vec![0.0f32; 96];
    g[0] = 0.8;
    g[7] = 0.3;
    g[40] = -0.2;
    let mut d = vec![0.0f32; 96];
    d[0] = 0.7;
    d[11] = 0.25;
    d[63] = 0.1;
    Convolver::new(&[g, d], 64)
}

fn crossfeed() -> CrossfeedProcessor {
    CrossfeedProcessor::new(44_100, 0.6, 0.3)
}

struct Chaine {
    eq: Mutex<Option<EqProcessor>>,
    conv: Mutex<Option<Convolver>>,
    cf: Mutex<Option<CrossfeedProcessor>>,
    pure: AtomicBool,
    mono: AtomicBool,
}

impl Chaine {
    fn new(eq: bool, conv: bool, cf: bool, mono: bool) -> Self {
        Self {
            eq: Mutex::new(eq.then(egaliseur)),
            conv: Mutex::new(conv.then(convolveur)),
            cf: Mutex::new(cf.then(crossfeed)),
            pure: AtomicBool::new(false),
            mono: AtomicBool::new(mono),
        }
    }

    fn moteurs(&self) -> MoteursLocaux<'_> {
        MoteursLocaux {
            eq: Some(&self.eq),
            convolver: &self.conv,
            crossfeed: &self.cf,
            pure_bypass: &self.pure,
            mono_downmix: &self.mono,
        }
    }

    /// Nouveau chemin, depuis les octets PCM : `f64` de bout en bout.
    fn nouveau(&self, octets: &[u8], bits: u16, ordre: OrdreLocal, bloc: usize) -> Vec<f64> {
        let mut sortie = Vec::new();
        let trame = 2 * usize::from(bits / 8);
        for morceau in octets.chunks(bloc * trame) {
            let mut f = crate::outputs::local::pcm_bytes_to_f64(morceau, bits);
            executer_chaine_locale(&mut f, &self.moteurs(), ordre, 2, false);
            sortie.extend_from_slice(&f);
        }
        sortie.extend(vidanger_chaine_locale(&self.moteurs(), ordre, 2, false));
        sortie
    }

    /// Ancien chemin, depuis les mêmes octets : `f32` entre chaque étage.
    fn ancien(&self, octets: &[u8], bits: u16, bloc: usize) -> Vec<f32> {
        let mut sortie = Vec::new();
        let trame = 2 * usize::from(bits / 8);
        for morceau in octets.chunks(bloc * trame) {
            let mut f = crate::outputs::local::pcm_bytes_to_f32(morceau, bits);
            ancien_apply_local_dsp(
                &mut f, &self.eq, &self.conv, &self.cf, &self.pure, &self.mono, 2, false,
            );
            sortie.extend_from_slice(&f);
        }
        sortie.extend(ancien_flush_local_dsp(&self.conv, &self.cf, &self.mono, 2));
        sortie
    }
}

/// Mots signés → octets PCM petit-boutistes de `bits` bits.
fn octets(mots: &[i32], bits: u16) -> Vec<u8> {
    let n = usize::from(bits / 8);
    mots.iter()
        .flat_map(|m| m.to_le_bytes()[..n].to_vec())
        .collect()
}

/// Signal stéréo déterministe pleine échelle de `bits` bits (sinus + bruit,
/// voies différentes).
fn mots(trames: usize, bits: u16) -> Vec<i32> {
    let echelle = ((1i64 << (bits - 1)) - 1) as f64;
    crate::tess::executeur::etages_de_test::pcm_24_bits(trames, 2)
        .into_iter()
        .map(|m| (f64::from(m) / 8_388_607.0 * echelle).round() as i32)
        .collect()
}

fn bits_f32(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

// ---------------------------------------------------------------------------
// Preuves
// ---------------------------------------------------------------------------

/// NON-RÉGRESSION : avec les moteurs d'avant (égaliseur, convolveur,
/// crossfeed, repli mono) dans l'ordre d'avant, le nouveau chemin rend, après
/// la frontière `f32` du producteur, EXACTEMENT les mêmes bits que l'ancien,
/// vidange comprise. Sources 16 et 24 bits, blocs petits, moyens et plus
/// grands que la pile d'un adaptateur.
#[test]
fn la_chaine_d_avant_rend_les_memes_bits_que_l_ancien_chemin() {
    for bits in [16u16, 24] {
        let source = octets(&mots(30_000, bits), bits);
        for bloc in [37usize, 1_024, 9_000] {
            for (eq, conv, cf, mono) in [
                (true, true, true, true),
                (true, false, true, false),
                (false, true, false, true),
                (false, false, false, false),
            ] {
                let nouveau = Chaine::new(eq, conv, cf, mono);
                let ancien = Chaine::new(eq, conv, cf, mono);
                let n =
                    frontiere_f32(&nouveau.nouveau(&source, bits, OrdreLocal::HISTORIQUE, bloc));
                let a = ancien.ancien(&source, bits, bloc);
                assert_eq!(
                    bits_f32(&n),
                    bits_f32(&a),
                    "{bits} bits, bloc {bloc}, eq={eq} conv={conv} cf={cf} mono={mono}"
                );
            }
        }
    }
}

/// L'entrée `f32` historique (`apply_local_dsp`, celle des témoins et du
/// chemin compressé) rend aussi les bits d'avant.
#[test]
fn l_entree_f32_historique_rend_les_memes_bits() {
    let source: Vec<f32> = mots(5_000, 24)
        .iter()
        .map(|&m| m as f32 / 8_388_608.0)
        .collect();
    let (n, a) = (
        Chaine::new(true, true, true, true),
        Chaine::new(true, true, true, true),
    );
    let mut x = source.clone();
    crate::outputs::local::apply_local_dsp(
        &mut x, &n.eq, &n.conv, &n.cf, &n.pure, &n.mono, 2, false,
    );
    let mut y = source;
    ancien_apply_local_dsp(&mut y, &a.eq, &a.conv, &a.cf, &a.pure, &a.mono, 2, false);
    assert_eq!(bits_f32(&x), bits_f32(&y));
    let qx = crate::outputs::local::flush_local_dsp(&n.conv, &n.cf, &n.pure, &n.mono, 2, false);
    let qy = ancien_flush_local_dsp(&a.conv, &a.cf, &a.mono, 2);
    assert_eq!(bits_f32(&qx), bits_f32(&qy));
}

/// « 64 bits réels » sur la chaîne locale : une source 32 bits passe par
/// l'étage `f64` natif (repli mono) et colle à la référence calculée hors
/// ligne en double précision à moins de 1e-12. CONTRE-ÉPREUVE : l'ancien
/// chemin (`pcm_bytes_to_f32` puis la chaîne `f32`) échoue au même banc.
#[test]
fn soixante_quatre_bits_reels_sur_la_chaine_locale() {
    let m = mots(8_000, 32);
    let source = octets(&m, 32);
    let reference: Vec<f64> = m
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|t| {
            let v = (f64::from(t[0]) + f64::from(t[1])) / 2.0 / 2_147_483_648.0;
            [v, v]
        })
        .collect();
    let ecart = |sortie: &[f64]| {
        sortie
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max)
    };

    let chaine = Chaine::new(false, false, false, true);
    let nouveau = chaine.nouveau(&source, 32, OrdreLocal::HISTORIQUE, 1_024);
    assert_eq!(nouveau.len(), reference.len());
    let e = ecart(&nouveau);
    assert!(
        e < 1e-12,
        "écart {e:e} : la chaîne locale n'est pas en 64 bits"
    );

    let chaine = Chaine::new(false, false, false, true);
    let ancien: Vec<f64> = chaine
        .ancien(&source, 32, 1_024)
        .into_iter()
        .map(f64::from)
        .collect();
    let e_ancien = ecart(&ancien);
    assert!(
        e_ancien >= 1e-12,
        "le banc ne discrimine pas : l'ancien chemin f32 passe ({e_ancien:e})"
    );
}

/// ORDRE LIBRE sur la chaîne locale réelle : égaliseur puis convolution ≠
/// convolution puis égaliseur, et chacune des deux sorties est exactement la
/// composition des moteurs dans SON ordre. (La non-commutation sur un étage
/// non linéaire est prouvée au niveau de l'exécuteur :
/// `tess::executeur::tests::l_ordre_des_etages_est_celui_de_la_chaine`.)
#[test]
fn l_ordre_de_la_chaine_tess_est_celui_de_la_sortie_locale() {
    let source = octets(&mots(6_000, 24), 24);
    let eq_puis_conv = OrdreLocal::depuis_ids(["equalizer-parametric", "convolution"], false);
    let conv_puis_eq = OrdreLocal::depuis_ids(["convolution", "equalizer-parametric"], false);
    assert_eq!(eq_puis_conv, OrdreLocal::HISTORIQUE);
    assert_ne!(conv_puis_eq, eq_puis_conv);

    let a = Chaine::new(true, true, false, false);
    let sortie_a = a.nouveau(&source, 24, eq_puis_conv, 512);
    let b = Chaine::new(true, true, false, false);
    let sortie_b = b.nouveau(&source, 24, conv_puis_eq, 512);
    assert_ne!(sortie_a, sortie_b, "l'ordre doit changer la sortie");

    // Oracle : les moteurs appelés directement, dans l'ordre demandé, par
    // blocs de 512 trames comme la chaîne.
    let (mut eq, mut conv) = (egaliseur(), convolveur());
    let mut attendu: Vec<f32> = Vec::new();
    for morceau in source.chunks(512 * 6) {
        let mut f = crate::outputs::local::pcm_bytes_to_f32(morceau, 24);
        conv.process_interleaved(&mut f);
        eq.process_interleaved(&mut f);
        attendu.extend_from_slice(&f);
    }
    // La queue du convolveur passe par ce qui le suit : l'égaliseur.
    let mut queue = conv.flush();
    eq.process_interleaved(&mut queue);
    attendu.extend_from_slice(&queue);
    assert_eq!(bits_f32(&frontiere_f32(&sortie_b)), bits_f32(&attendu));
}

/// « Bypass all » et PURE : aucun étage n'agit, le PCM ressort à l'octet ;
/// passé par la quantification finale, le hash de sortie est celui de la
/// source. CONTRE-ÉPREUVE : la même chaîne active change le hash.
#[test]
fn bit_perfect_a_l_octet_en_pure_et_en_bypass_all() {
    for bits in [16u16, 24, 32] {
        let source = octets(&mots(4_000, bits), bits);
        let empreinte_source = Sha256::digest(&source);
        let sortie = |chaine: &Chaine, ordre: OrdreLocal| -> Vec<u8> {
            let mut f = crate::outputs::local::pcm_bytes_to_f64(&source, bits);
            executer_chaine_locale(&mut f, &chaine.moteurs(), ordre, 2, false);
            let mut q = QuantificationPcm::new(true);
            let trames = f.len() / 2;
            q.preparer(
                FormatFlux {
                    channels: 2,
                    sample_rate_hz: 44_100,
                },
                trames,
            )
            .unwrap();
            let mut mots = vec![0i32; f.len()];
            q.finaliser(
                &f,
                trames,
                SortieFinale::Pcm {
                    echantillons: &mut mots,
                    bits: bits as u8,
                },
            )
            .unwrap();
            let decale = 32 - u32::from(bits);
            octets(&mots.iter().map(|m| m >> decale).collect::<Vec<_>>(), bits)
        };

        let pure = Chaine::new(true, true, true, true);
        pure.pure.store(true, Ordering::Relaxed);
        assert_eq!(
            Sha256::digest(sortie(&pure, OrdreLocal::HISTORIQUE)),
            empreinte_source,
            "{bits} bits : PURE doit être bit-perfect"
        );
        let contournee = Chaine::new(true, true, true, true);
        assert_eq!(
            Sha256::digest(sortie(&contournee, OrdreLocal::depuis_ids([], true))),
            empreinte_source,
            "{bits} bits : « Bypass all » doit être bit-perfect"
        );
        let vide = Chaine::new(false, false, false, false);
        assert_eq!(
            Sha256::digest(sortie(&vide, OrdreLocal::HISTORIQUE)),
            empreinte_source,
            "{bits} bits : une chaîne vide doit être bit-perfect"
        );
        // Contre-épreuve : un seul étage actif, et le hash change.
        let active = Chaine::new(false, false, true, false);
        assert_ne!(
            Sha256::digest(sortie(&active, OrdreLocal::HISTORIQUE)),
            empreinte_source,
            "{bits} bits : un crossfeed actif doit modifier la sortie"
        );
    }
}

/// Aucune allocation dans le traitement : une fois l'exécuteur du fil
/// préparé (premier bloc), un bloc de même taille traverse l'égaliseur, le
/// crossfeed et le repli mono sans appeler l'allocateur. (Le moteur `f32` du
/// convolveur alloue par bloc FFT : il est porté par T09b.)
#[test]
fn le_traitement_n_alloue_pas() {
    let chaine = Chaine::new(true, false, true, true);
    let mut f = crate::outputs::local::pcm_bytes_to_f64(&octets(&mots(512, 24), 24), 24);
    executer_chaine_locale(&mut f, &chaine.moteurs(), OrdreLocal::HISTORIQUE, 2, false);
    super::super::ringbuf_tests::assert_no_allocation(|| {
        executer_chaine_locale(&mut f, &chaine.moteurs(), OrdreLocal::HISTORIQUE, 2, false);
    });
}

/// La correspondance des identifiants TESS vers les étages locaux.
#[test]
fn l_ordre_local_se_lit_dans_la_chaine_tess() {
    use EtageLocal::*;
    let chaine: ChaineTess = serde_json::from_value(serde_json::json!({
        "version": 1,
        "plugins": [
            { "id": "crosstalk", "enabled": true },
            { "id": "convolution", "enabled": true },
            { "id": "equalizer-graphic", "enabled": false },
            { "id": "crossfeed", "enabled": true },
            { "id": "channel-remap", "enabled": true }
        ]
    }))
    .unwrap();
    let ordre = OrdreLocal::depuis_chaine(&chaine);
    assert_eq!(
        ordre.etages(),
        [Crossfeed, Convolution, Egaliseur, RepliMono]
    );
    assert!(!ordre.bypass_all());
    // Les étages non nommés gardent leur ordre relatif d'avant, le repli
    // mono reste dernier.
    assert_eq!(
        OrdreLocal::depuis_ids(["crossfeed-pro"], true).etages(),
        [Crossfeed, Egaliseur, Convolution, RepliMono]
    );
    assert!(OrdreLocal::depuis_ids([], true).bypass_all());
    assert_eq!(OrdreLocal::depuis_ids([], false), OrdreLocal::HISTORIQUE);
}
