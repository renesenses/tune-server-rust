//! #4755 — l'état dénormalisé des biquads pendant le silence, **mesuré**.
//!
//! Module enfant de [`super`] (`engine`) : il voit donc l'état privé
//! `EqProcessor::states`, ce qu'aucun banc placé dans `tests/` ne pourrait
//! faire. C'est ce qui permet de constater le dénormal **là où il vit**, au
//! lieu d'en déduire l'existence d'une mesure de temps.
//!
//! Les deux questions de l'issue, séparées :
//!
//! 1. **Le cas se produit-il ?** Après excitation puis silence numérique
//!    exact, l'état du biquad décroît géométriquement. Combien de secondes de
//!    silence avant qu'il n'entre dans la plage dénormale de `f64`
//!    (< 2,225e−308), et combien de temps y reste-t-il ?
//! 2. **Combien ça coûte ?** Le même processeur, la même boucle, le même
//!    nombre d'échantillons, avec l'état réinjecté à chaque bloc en régime
//!    NORMAL (1e−300) puis en régime DÉNORMAL (1e−320). L'écart de temps est
//!    la pénalité, isolée de tout le reste.
//!
//! Aucun `unsafe` : le crate porte `forbid(unsafe_code)`, donc pas de
//! `ldmxcsr`. Le régime est imposé en écrivant l'état, pas en changeant le
//! mode du processeur — ce qui est de toute façon plus honnête, puisque Tune
//! ne met pas FTZ/DAZ sur son fil audio.
//!
//! Tous les témoins sont `#[ignore]` : ce sont des MESURES, pas des gardes.
//! Elles varient d'une machine à l'autre et n'ont rien à faire dans la CI.
//!
//! ```text
//! cargo test --release -p tune-plugin-equalizer --lib -- --ignored --nocapture banc_denormal
//! ```

use super::*;
use std::hint::black_box;
use std::time::Instant;

const SR: u32 = 44_100;
const CANAUX: u16 = 2;
/// Le bloc du chemin local (`outputs/local.rs` travaille par paquets cpal).
const BLOC_TRAMES: usize = 1_024;

// ───────────────────────────── les profils ─────────────────────────────

fn bande(freq: f64, gain: f64, q: f64) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: "peak".into(),
        channel: None,
    }
}

/// Le profil historique : trois filtres de tilt, ce que rend l'interface
/// « simple » dès qu'un curseur bouge.
fn profil_tilt() -> EqProfile {
    EqProfile {
        enabled: true,
        bass_gain_db: 6.0,
        treble_gain_db: 4.0,
        ..Default::default()
    }
}

/// Égaliseur graphique 10 bandes, octave, Q = 1,41 — le préréglage le plus
/// courant du mode expert.
fn profil_graphique_10() -> EqProfile {
    let freqs = [
        31.25, 62.5, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
    ];
    EqProfile {
        enabled: true,
        bands: freqs.iter().map(|&f| bande(f, 3.0, 1.41)).collect(),
        ..Default::default()
    }
}

/// Graphique 31 bandes, tiers d'octave, Q = 4,3 — le pire cas livré.
fn profil_graphique_31() -> EqProfile {
    let bands: Vec<EqBandSpec> = (0..31)
        .map(|i| {
            let f = 20.0 * 2f64.powf(i as f64 / 3.0);
            bande(f.min(SR as f64 * 0.45), 3.0, 4.3)
        })
        .collect();
    EqProfile {
        enabled: true,
        bands,
        ..Default::default()
    }
}

/// Le cas le plus défavorable qu'un utilisateur puisse construire : une bande
/// très grave et très étroite. Son pôle est le plus lent, donc sa fenêtre
/// dénormale la plus longue.
fn profil_grave_etroit() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![bande(20.0, 6.0, 30.0)],
        ..Default::default()
    }
}

fn profils() -> Vec<(&'static str, EqProfile)> {
    vec![
        ("tilt 3 filtres (+6/+4 dB)", profil_tilt()),
        ("graphique 10 bandes Q=1,41", profil_graphique_10()),
        ("graphique 31 bandes Q=4,3", profil_graphique_31()),
        ("1 bande 20 Hz Q=30 (pire cas)", profil_grave_etroit()),
    ]
}

// ───────────────────────────── les outils ─────────────────────────────

fn est_denormal(x: f64) -> bool {
    x != 0.0 && x.abs() < f64::MIN_POSITIVE
}

impl EqProcessor {
    /// Combien de mots d'état sont dans la plage dénormale de `f64`.
    fn etats_denormaux(&self) -> usize {
        self.states
            .iter()
            .flatten()
            .map(|s| {
                [s.x1, s.x2, s.y1, s.y2]
                    .iter()
                    .filter(|v| est_denormal(**v))
                    .count()
            })
            .sum()
    }

    /// Le plus grand module non nul de l'état — l'échelle où il vit.
    fn etat_max(&self) -> f64 {
        self.states
            .iter()
            .flatten()
            .flat_map(|s| [s.x1.abs(), s.x2.abs(), s.y1.abs(), s.y2.abs()])
            .fold(0.0f64, f64::max)
    }

    /// Impose la même valeur à TOUT l'état. Sert à tenir un régime
    /// arithmétique constant pendant une mesure de temps.
    fn imposer_etat(&mut self, v: f64) {
        for s in self.states.iter_mut().flatten() {
            s.x1 = v;
            s.x2 = -v;
            s.y1 = v;
            s.y2 = -v;
        }
    }

    fn a_des_filtres(&self) -> bool {
        self.filters.iter().any(|f| !f.is_empty())
    }
}

/// Bruit blanc déterministe à pleine échelle (xorshift), entrelacé.
fn bruit(trames: usize) -> Vec<f32> {
    let mut e: u64 = 0x2545_F491_4F6C_DD1D;
    (0..trames * CANAUX as usize)
        .map(|_| {
            e ^= e << 13;
            e ^= e >> 7;
            e ^= e << 17;
            ((e >> 11) as f64 / (1u64 << 53) as f64 * 1.8 - 0.9) as f32
        })
        .collect()
}

fn silence(trames: usize) -> Vec<f32> {
    vec![0.0f32; trames * CANAUX as usize]
}

/// Meilleur de `passes` : `f` est relancée entière, on garde le temps le plus
/// court. Le minimum est la bonne statistique ici — le bruit d'une machine
/// partagée n'ajoute jamais de la vitesse.
fn meilleur_ns_par_trame(passes: usize, trames: usize, mut f: impl FnMut()) -> f64 {
    let mut meilleur = f64::INFINITY;
    for _ in 0..passes {
        let t0 = Instant::now();
        f();
        let ns = t0.elapsed().as_nanos() as f64 / trames as f64;
        meilleur = meilleur.min(ns);
    }
    meilleur
}

// ────────────── partie 1 : le cas se produit-il, et quand ? ──────────────

/// Après excitation puis silence numérique, l'état entre-t-il réellement dans
/// la plage dénormale, et pour combien de temps ?
///
/// Mesure et AFFIRME seulement ce qui est structurel : l'état décroît, donc
/// s'il passe sous 2,225e−308 il finit par atteindre zéro. Les instants, eux,
/// sont rapportés, pas gardés.
#[test]
#[ignore = "mesure #4755, pas une garde"]
fn banc_denormal_quand_l_etat_devient_il_denormal() {
    println!(
        "\n=== #4755 partie 1 — l'état dénormalisé existe-t-il ? ({} Hz, {} canaux) ===",
        SR, CANAUX
    );
    println!(
        "{:<32} {:>12} {:>12} {:>12} {:>10}",
        "profil", "1er dénorm.", "dernier", "fenêtre", "états"
    );

    for (nom, profil) in profils() {
        let mut eq = EqProcessor::new(&profil, SR, CANAUX);
        assert!(eq.a_des_filtres(), "{nom} : profil sans filtre, banc vide");

        // Excitation : 1 s de bruit à pleine échelle.
        let mut x = bruit(SR as usize);
        eq.process_interleaved(&mut x);

        // Puis silence numérique exact, bloc par bloc, jusqu'à 600 s.
        let mut z = silence(BLOC_TRAMES);
        let mut premier: Option<f64> = None;
        let mut dernier: Option<f64> = None;
        let mut pic_denormaux = 0usize;
        let blocs_max = (600.0 * SR as f64 / BLOC_TRAMES as f64) as usize;
        for b in 0..blocs_max {
            z.fill(0.0);
            eq.process_interleaved(&mut z);
            let n = eq.etats_denormaux();
            let t = (b + 1) as f64 * BLOC_TRAMES as f64 / SR as f64;
            if n > 0 {
                premier.get_or_insert(t);
                dernier = Some(t);
                pic_denormaux = pic_denormaux.max(n);
            } else if premier.is_some() && eq.etat_max() == 0.0 {
                break; // l'état est retombé à zéro : c'est fini pour de bon
            }
        }

        match (premier, dernier) {
            (Some(p), Some(d)) => println!(
                "{nom:<32} {p:>10.2} s {d:>10.2} s {:>10.2} s {pic_denormaux:>10}",
                d - p
            ),
            _ => println!("{nom:<32} {:>12} {:>12} {:>12} {:>10}", "—", "—", "—", 0),
        }
    }
    println!(
        "\nRappel : f64 dénormal = |x| < {:e}. L'état part de ~1 après excitation.",
        f64::MIN_POSITIVE
    );
}

// ──────────────── partie 2 : combien coûte ce régime ? ────────────────

/// Le coût du régime dénormal, isolé : même processeur, même boucle, même
/// nombre d'échantillons, seul l'ORDRE DE GRANDEUR de l'état change.
///
/// L'état est réimposé avant chaque bloc pour que le régime tienne pendant
/// toute la mesure — sinon 1e−300 glisse vers le dénormal et 1e−320 vers zéro,
/// et les deux colonnes se rejoignent.
#[test]
#[ignore = "mesure #4755, pas une garde"]
fn banc_denormal_ce_que_coute_le_regime_denormal() {
    const BLOCS: usize = 400;
    const PASSES: usize = 7;
    let trames = BLOCS * BLOC_TRAMES;

    println!(
        "\n=== #4755 partie 2 — le coût du régime, ns par TRAME ({} canaux) ===",
        CANAUX
    );
    println!(
        "{:<32} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "profil", "musique", "sil. zéro", "sil. 1e-300", "sil. 1e-320", "pénalité"
    );

    for (nom, profil) in profils() {
        let mut eq = EqProcessor::new(&profil, SR, CANAUX);
        let source = bruit(BLOC_TRAMES);
        let mut tampon = vec![0.0f32; BLOC_TRAMES * CANAUX as usize];

        let mut regime = |graine: Option<f64>, musique: bool| {
            meilleur_ns_par_trame(PASSES, trames, || {
                for _ in 0..BLOCS {
                    if musique {
                        tampon.copy_from_slice(&source);
                    } else {
                        tampon.fill(0.0);
                    }
                    if let Some(v) = graine {
                        eq.imposer_etat(v);
                    } else {
                        eq.imposer_etat(0.0);
                    }
                    black_box(eq.process_interleaved(black_box(&mut tampon)));
                }
            })
        };

        let musique = regime(None, true);
        let zero = regime(None, false);
        let normal = regime(Some(1e-300), false);
        let denormal = regime(Some(1e-320), false);

        println!(
            "{nom:<32} {musique:>10.2} {zero:>10.2} {normal:>10.2} {denormal:>10.2} {:>8.2}×",
            denormal / normal.max(1e-9)
        );
    }

    let trames_par_s = SR as f64 * 1.0;
    println!(
        "\nBudget : 1 s d'audio {} Hz = {trames_par_s:.0} trames. 1 ns/trame = {:.4} % d'un cœur.",
        SR,
        trames_par_s * 1e-9 * 100.0
    );
}

/// La mesure de bout en bout, sans artifice : on excite, puis on laisse couler
/// du vrai silence, et on chronomètre ce silence-là contre le même silence
/// joué sur un processeur neuf (état à zéro d'un bout à l'autre).
///
/// C'est la question telle que l'issue la pose : « benchmark silence after
/// excitation against fresh silence ».
#[test]
#[ignore = "mesure #4755, pas une garde"]
fn banc_denormal_silence_apres_excitation_contre_silence_frais() {
    const SECONDES: f64 = 120.0;
    const PASSES: usize = 5;
    let blocs = (SECONDES * SR as f64 / BLOC_TRAMES as f64) as usize;
    let trames = blocs * BLOC_TRAMES;

    println!("\n=== #4755 partie 3 — {SECONDES} s de silence, après excitation vs frais ===",);
    println!(
        "{:<32} {:>12} {:>12} {:>10}",
        "profil", "frais ns/tr", "après ns/tr", "écart"
    );

    for (nom, profil) in profils() {
        let mut tampon = vec![0.0f32; BLOC_TRAMES * CANAUX as usize];

        let excitation = bruit(SR as usize);
        // L'excitation est HORS du chronomètre : seul le silence est mesuré.
        let mut mesurer = |exciter: bool| {
            let mut meilleur = f64::INFINITY;
            for _ in 0..PASSES {
                let mut eq = EqProcessor::new(&profil, SR, CANAUX);
                if exciter {
                    let mut x = excitation.clone();
                    eq.process_interleaved(&mut x);
                }
                let t0 = Instant::now();
                for _ in 0..blocs {
                    tampon.fill(0.0);
                    black_box(eq.process_interleaved(black_box(&mut tampon)));
                }
                meilleur = meilleur.min(t0.elapsed().as_nanos() as f64 / trames as f64);
            }
            meilleur
        };

        let frais = mesurer(false);
        let apres = mesurer(true);

        println!(
            "{nom:<32} {frais:>12.2} {apres:>12.2} {:>9.2}×",
            apres / frais.max(1e-9)
        );
    }
}

// ──────────────────── la garde, elle, n'est pas ignorée ────────────────────

/// #4755 — après excitation puis silence numérique, **aucun mot d'état ne doit
/// jamais séjourner dans la plage dénormale de `f64`**.
///
/// C'est le témoin du plancher [`PLANCHER_ANTI_DENORMAL`], et il est
/// délibérément écrit sur la PROPRIÉTÉ et non sur un temps : un témoin de
/// durée mesurerait la machine, celui-ci mesure l'arithmétique. Retirer le
/// plancher le fait rougir en 2,35 s de silence simulé sur le profil de tilt
/// et en 11,94 s sur le graphique 10 bandes (mesuré, cf. `banc_denormal_*`).
///
/// Les durées de silence balayées dépassent chacune leur seuil d'apparition
/// mesuré, avec de la marge : sans cela le témoin serait vert pour la mauvaise
/// raison — trop court pour voir le défaut.
#[test]
fn i4755_l_etat_du_biquad_ne_sejourne_jamais_dans_la_plage_denormale() {
    for (nom, profil, silence_s) in [
        ("tilt 3 filtres", profil_tilt(), 15.0_f64),
        ("graphique 10 bandes", profil_graphique_10(), 25.0),
    ] {
        let mut eq = EqProcessor::new(&profil, SR, CANAUX);
        assert!(
            eq.a_des_filtres(),
            "{nom} : profil sans filtre, témoin vide"
        );

        let mut x = bruit(SR as usize);
        eq.process_interleaved(&mut x);
        assert!(
            eq.etat_max() > 1e-6,
            "{nom} : l'excitation n'a pas chargé l'état ({:e}) — témoin vide",
            eq.etat_max()
        );

        let blocs = (silence_s * SR as f64 / BLOC_TRAMES as f64) as usize;
        let mut z = silence(BLOC_TRAMES);
        for b in 0..blocs {
            z.fill(0.0);
            eq.process_interleaved(&mut z);
            assert_eq!(
                eq.etats_denormaux(),
                0,
                "{nom} : état dénormalisé après {:.2} s de silence (max |état| = {:e})",
                (b + 1) as f64 * BLOC_TRAMES as f64 / SR as f64,
                eq.etat_max()
            );
        }
    }
}

/// Le plancher ne doit pas se contenter d'éviter le dénormal : il doit rendre
/// le zéro EXACT, sinon l'état resterait vivant à jamais dans son cycle limite
/// et le filtre garderait une queue qui ne finit pas.
#[test]
fn i4755_l_etat_du_biquad_retombe_a_zero_exact_pendant_le_silence() {
    let profil = profil_tilt();
    let mut eq = EqProcessor::new(&profil, SR, CANAUX);
    let mut x = bruit(SR as usize);
    eq.process_interleaved(&mut x);

    let blocs = (30.0 * SR as f64 / BLOC_TRAMES as f64) as usize;
    let mut z = silence(BLOC_TRAMES);
    let mut atteint = None;
    for b in 0..blocs {
        z.fill(0.0);
        eq.process_interleaved(&mut z);
        if eq.etat_max() == 0.0 {
            atteint = Some((b + 1) as f64 * BLOC_TRAMES as f64 / SR as f64);
            break;
        }
    }
    assert!(
        atteint.is_some(),
        "l'état n'est pas retombé à zéro exact en 30 s de silence (max |état| = {:e})",
        eq.etat_max()
    );
}

/// Le plancher est 155 décades sous le plus petit dénormal d'un `f32` : il ne
/// doit RIEN changer au signal. Ce témoin compare échantillon par échantillon
/// la sortie d'un bruit à pleine échelle contre la même sortie calculée sans
/// aucun plancher, et exige l'égalité BINAIRE.
#[test]
fn i4755_le_plancher_ne_change_aucun_echantillon_musical() {
    for (nom, profil) in profils() {
        let mut eq = EqProcessor::new(&profil, SR, CANAUX);
        let mut avec = bruit(10 * SR as usize / 100);
        eq.process_interleaved(&mut avec);

        // La même cascade, la même entrée, mais la récurrence écrite ici SANS
        // plancher : la référence de ce que le filtre rendait avant #4755.
        let mut sans = bruit(10 * SR as usize / 100);
        let filtres = EqProcessor::new(&profil, SR, CANAUX);
        let mut etats: Vec<Vec<BiquadState>> = filtres
            .filters
            .iter()
            .map(|f| vec![BiquadState::default(); f.len()])
            .collect();
        let ch_count = CANAUX as usize;
        for frame in sans.chunks_exact_mut(ch_count) {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let mut s = *sample as f64 * filtres.preamp_gains[ch];
                for (st, c) in etats[ch].iter_mut().zip(filtres.filters[ch].iter()) {
                    let y = c.b0 * s + c.b1 * st.x1 + c.b2 * st.x2 - c.a1 * st.y1 - c.a2 * st.y2;
                    st.x2 = st.x1;
                    st.x1 = s;
                    st.y2 = st.y1;
                    st.y1 = y;
                    s = y;
                }
                *sample = s as f32;
            }
        }

        assert_eq!(
            avec, sans,
            "{nom} : le plancher #4755 a modifié un échantillon musical"
        );
    }
}
