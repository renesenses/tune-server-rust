//! rc14 (Didier, Windows + ADC) — le TEST DE NIVEAU avant la mesure.
//!
//! L'écran « Mesure au micro » laisse choisir l'entrée, la voie et
//! l'étalonnage, mais rien ne montrait que la bonne entrée capte, ni à quel
//! niveau : la mesure ne refusait qu'après coup (`micro_muet`, RSB
//! insuffisant, écrêtage). Deux gestes, avant de mesurer :
//!
//! * **écoute seule** (`mode = "ecoute"`) : l'entrée et la voie choisies sont
//!   ouvertes, sans rien jouer ; le vumètre bouge quand on parle. C'est le
//!   moyen de vérifier l'entrée et la voie. Arrêt tout seul après
//!   [`ECOUTE_MAX_S`] secondes au plus.
//! * **tester le niveau** (`mode = "signal"`) : une seconde de bruit de fond
//!   captée sans rien jouer, puis un bruit rose limité à la bande de mesure
//!   ([`DUREE_SIGNAL_S`] s) joué sur la zone, au volume de mesure et sous les
//!   mêmes gardes que le balayage ; puis un [`Verdict`] : trop faible, bon,
//!   trop fort, avec les valeurs et la plage conseillée.
//!
//! ```text
//! POST /niveaux ─► Controleur::tester_niveau
//!    │ entrée connue ? voie ? zone connue, volume réglable, sortie active ?
//!    │ entrée et zone libres (ni test ni mesure) ?          sinon : refus
//!    │ capture vers tampon (entree-audio)          ─┐
//!    │ première trame (micro_muet sinon)            │ fil « calibration-niveau »
//!    │ 1 s captée sans rien jouer → bruit de fond   │ relevé du vumètre / 50 ms
//!    │ traitements contournés, volume de mesure     │
//!    │ signal armé, zone lancée SANS FILE NI        │
//!    │   HISTORIQUE (`ZoneDeTest::jouer_signal`)    │
//!    │ fenêtre captée → crête, efficace → verdict   │
//!    │ zone arrêtée et vidée, volume rendu, capture close
//!    └─► état Terminee (verdict) / Annulee / Erreur
//! ```
//!
//! Ce qui est réutilisé : la capture vers tampon et son attente de la
//! première trame (#9070), le micro figé (#9066), la cadence du signal sur
//! l'écoute (#9066), le contournement des traitements (`mesure_nue`, #8513),
//! le plafond de niveau joué (−6 dBFS) et le refus d'une zone en volume
//! fixe. Le déroulé du balayage, lui, n'est pas touché.
//!
//! Pourquoi une source à part (`calibration-niveau`) et non une consigne de
//! plus au générateur du balayage : le signal de test ne passe NI par la
//! file de la zone NI par l'historique. La mesure remplace la file par son
//! balayage (fil 3094 pour la rendre) ; le test, lui, lance la zone par
//! `play_transfere_sans_historique`, sans toucher à la file, puis arrête la
//! zone et efface ce qu'elle affiche. Sa durée annoncée reste sous les 30 s
//! d'une écoute annoncée (scrobble).

use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tune_calibration::fft;
use tune_calibration::level::LevelPolicy;
use tune_core::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use tune_entree_audio::tampon::{CaptureVersTampon, Tampon};

use crate::generateur::{HorlogeDEcoute, Voies};
use crate::mesure::{
    AVANCE_SUR_L_ECOUTE_S, CODE_MICRO_FIGE, Controleur, FREQUENCE_MESURE, MICRO_FIGE,
};

/// Le nom de la source du signal de test, dans `NowPlaying` le temps du
/// test. Jamais dans la file.
pub const SOURCE_NIVEAU: &str = tune_core::source_pcm::SOURCE_CALIBRATION_NIVEAU;

// ─── Le signal de test ─────────────────────────────────────────────────────

/// Bruit de fond capté avant le signal, rien n'étant joué.
pub const DUREE_BRUIT_DE_FOND_S: f64 = 1.0;
/// Les premières trames captées sont écartées (mise en route du pilote).
pub const MISE_EN_ROUTE_S: f64 = 0.1;
/// Durée du bruit rose joué : la montée ([`PALIERS_DB`]) puis le palier de
/// référence.
pub const DUREE_SIGNAL_S: f64 = 4.5;
/// Relecture m1 — le signal MONTE comme la mesure : départ 12 dB sous la
/// référence (l'équivalent du premier palier de la mesure, −30 dBFS), puis
/// +6 dB toutes les [`DUREE_PALIER_S`], jusqu'à la référence. Pendant toute
/// la montée, la veille coupe le signal en fondu si le micro sature ou si
/// la pression dépasse [`SPL_MAX_DB`] (étalonnage fourni).
pub const PALIERS_DB: [f64; 3] = [-12.0, -6.0, 0.0];
pub const DUREE_PALIER_S: f64 = 0.5;
/// Plafond de pression acoustique, celui de la mesure
/// (`LevelPolicy::default().max_spl_db`).
pub const SPL_MAX_DB: f64 = 90.0;
/// Fondu de sortie du signal, à l'annulation comme à la coupure de sécurité.
pub const FONDU_S: f64 = 0.02;
/// Après un fondu, le temps que le chemin de lecture se vide (avance du
/// générateur et latence de sortie) avant d'arrêter la zone : l'arrêt ne
/// tombe jamais en plein signal.
pub const VIDAGE_APRES_FONDU: Duration = Duration::from_millis(600);
/// Silence joué avant le signal : la zone démarre sans craquer.
pub const AMORCE_S: f64 = 0.3;
/// Fenêtre captée au-delà de la fin théorique du signal : la latence de la
/// sortie (une sortie locale reste bien en dessous).
pub const MARGE_LATENCE_S: f64 = 2.0;
/// Durée du signal retenue pour l'efficace : les blocs de 100 ms les plus
/// forts de la fenêtre, quelle que soit la latence de la sortie.
pub const DUREE_RETENUE_S: f64 = 3.0;
/// La durée annoncée à la zone : le signal tient dedans avec de la marge,
/// la zone est arrêtée bien avant. Sous les 30 s d'une écoute annoncée
/// (`scrobble::SCROBBLE_MIN_DURATION_MS`) : le test n'est jamais scrobblé.
pub const DUREE_ANNONCEE_MS: u64 = 20_000;
/// Bande du bruit rose : celle du balayage (20 Hz – 20 kHz).
pub const BANDE_HZ: (f64, f64) = (20.0, 20_000.0);
/// Niveau de référence : le bruit rose a l'EFFICACE d'un balayage joué à
/// ce niveau crête (efficace d'un sinus = crête − 3 dB). −18 dBFS est le
/// troisième palier de la montée de niveau de la mesure (−30, −24, −18…),
/// à 12 dB du plafond joué (−6 dBFS) : la mesure garde 12 dB pour monter
/// et 12 dB pour descendre autour du niveau testé.
pub const NIVEAU_REFERENCE_DBFS: f64 = -18.0;
/// L'efficace du bruit rose joué (dBFS).
pub const EFFICACE_SIGNAL_DBFS: f64 = NIVEAU_REFERENCE_DBFS - 3.0103;

// ─── Seuils du verdict ─────────────────────────────────────────────────────
//
// Ce que la mesure exige (`mesure.rs`, `tune_calibration::level`) :
//  * écrêtage capté (crête ≥ −0,1 dBFS) ⇒ refus ;
//  * la montée de niveau ne passe au palier suivant (+6 dB) que si la crête
//    captée reste sous −12 dBFS (plafond capté −6 dBFS moins un pas) ;
//  * RSB de la réponse impulsionnelle ≥ 50 dB visé, refus sous 20 dB
//    (`RSB_MIN_DB`), avertissement sous 40 dB.
//
// Le bruit rose a un facteur de crête d'environ 12 dB, le balayage (sinus)
// de 3 dB : à efficace égal, la crête du bruit rose dépasse d'environ 9 dB
// celle du balayage. Une crête de bruit rose captée à −3 dBFS correspond
// donc à un balayage capté vers −12 dBFS au niveau de référence : la limite
// où la mesure cesse de monter. Au-delà, l'ADC est trop près de l'écrêtage.

/// Crête captée (dBFS) à partir de laquelle le micro ou l'ADC sature.
pub const CRETE_SATURATION_DBFS: f64 = -1.0;
/// Crête captée maximale conseillée (dBFS) — voir plus haut.
pub const CRETE_MAX_DBFS: f64 = -3.0;
/// Rapport signal/bruit (efficace du signal capté sur efficace du bruit de
/// fond, en large bande) sous lequel le micro n'entend PAS la zone : mauvaise
/// entrée, mauvaise voie, ou zone muette. Relecture m7 : 1 dB et non 3 —
/// dans la pièce simulée, la mesure aboutit encore avec 3 à 6 dB ici.
pub const RSB_ABSENT_DB: f64 = 1.0;
/// Rapport signal/bruit minimal. La déconvolution d'une salve (1 s de
/// balayage) ajoute un gain de traitement à ce rapport large bande : relevé
/// sur la pièce simulée (bruit de fond de −80 à −30 dBFS,
/// `tests/test_de_niveau.rs`, `exploration_seuil_rsb`), le RSB de la salve
/// jouée au niveau de référence vaut le RSB du test plus 40 dB environ. À
/// 10 dB, la salve atteint donc le RSB visé (50 dB) AU niveau de référence :
/// verdict « bon ». Entre [`RSB_ABSENT_DB`] et 10 dB : « faible mais
/// mesurable », la mesure devra jouer plus fort que le test (jusqu'à 12 dB
/// de plus ; elle aboutit encore vers 3 dB dans la pièce simulée).
pub const RSB_MIN_DB: f64 = 10.0;
/// Rapport signal/bruit conseillé : 10 dB de marge, la mesure se fait sous
/// le niveau de référence.
pub const RSB_CONSEILLE_DB: f64 = 20.0;
/// Crête sous laquelle une fenêtre captée est du silence NUMÉRIQUE (des
/// zéros) : une entrée vivante, même 16 bits dans une pièce calme, reste
/// au-dessus (un pas de quantification 16 bits vaut −90 dBFS).
pub const SILENCE_NUMERIQUE_DBFS: f64 = -110.0;
/// Fil de Didier (TASCAM US-366, zone ASIO, micro en WASAPI sur la même
/// interface) — quand ASIO tient l'interface, son entrée Windows peut ne
/// plus rien rendre pendant la lecture. Tune ne capte pas en ASIO
/// (`tune-entree-audio` passe par cpal en WASAPI, ALSA ou CoreAudio).
pub const AVERTISSEMENT_ASIO: &str = "La zone joue en ASIO : l'entrée Windows de la même interface peut rester muette \
     pendant la lecture ; essayez une zone WASAPI pour la mesure";
/// Plancher d'affichage : un silence numérique n'a pas de niveau, on le
/// borne (un `-inf` deviendrait `null` en JSON).
pub const PLANCHER_DBFS: f64 = -120.0;

// ─── Bornes de session ─────────────────────────────────────────────────────

/// L'écoute seule s'arrête toute seule au plus tard après cette durée.
pub const ECOUTE_MAX_S: f64 = 30.0;
/// Le test avec signal s'arrête tout seul au plus tard après cette durée
/// (première trame, bruit de fond, ouverture par la zone, signal, marge).
pub const SIGNAL_MAX_S: f64 = 30.0;
/// Attente maximale de l'ouverture du signal par la zone.
pub const DELAI_OUVERTURE: Duration = Duration::from_secs(10);
/// Pas du relevé du vumètre.
pub const PAS_DU_RELEVE: Duration = Duration::from_millis(50);
/// Fenêtre du vumètre en direct.
pub const FENETRE_VUMETRE_S: f64 = 0.1;
/// Les tests finis gardés en mémoire (les plus récents).
const TESTS_GARDES: usize = 16;

// ─── Niveaux et verdict (purs) ─────────────────────────────────────────────

fn borne(db: f64) -> f64 {
    if db.is_finite() {
        db.max(PLANCHER_DBFS)
    } else {
        PLANCHER_DBFS
    }
}

/// Crête et efficace d'un segment capté, en dBFS. L'efficace est pris sans
/// la composante continue (un ADC décalé ne passe pas pour un signal).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Niveaux {
    pub crete_dbfs: f64,
    pub rms_dbfs: f64,
}

impl Niveaux {
    pub const SILENCE: Niveaux = Niveaux {
        crete_dbfs: PLANCHER_DBFS,
        rms_dbfs: PLANCHER_DBFS,
    };
}

fn energie_sans_continue(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let n = x.len() as f64;
    let moyenne = x.iter().sum::<f64>() / n;
    x.iter().map(|v| (v - moyenne) * (v - moyenne)).sum::<f64>() / n
}

fn crete(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

pub fn niveaux(x: &[f64]) -> Niveaux {
    if x.is_empty() {
        return Niveaux::SILENCE;
    }
    Niveaux {
        crete_dbfs: borne(fft::to_db(crete(x))),
        rms_dbfs: borne(10.0 * energie_sans_continue(x).max(1e-30).log10()),
    }
}

/// Les niveaux du SIGNAL dans une fenêtre captée plus longue que lui : la
/// crête de toute la fenêtre, et l'efficace des `retenus` blocs de `bloc`
/// trames les plus forts. La latence de la sortie (inconnue) déplace le
/// signal dans la fenêtre sans fausser l'efficace.
pub fn niveaux_du_signal(x: &[f64], bloc: usize, retenus: usize) -> Niveaux {
    if x.is_empty() || bloc == 0 {
        return Niveaux::SILENCE;
    }
    let mut energies: Vec<f64> = x.chunks(bloc).map(energie_sans_continue).collect();
    energies.sort_by(|a, b| b.total_cmp(a));
    let pris = &energies[..retenus.clamp(1, energies.len())];
    let energie = pris.iter().sum::<f64>() / pris.len() as f64;
    Niveaux {
        crete_dbfs: borne(fft::to_db(crete(x))),
        rms_dbfs: borne(10.0 * energie.max(1e-30).log10()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Jugement {
    TropFaible,
    /// Relecture m7 — faible mais mesurable.
    Faible,
    Bon,
    TropFort,
}

/// La plage conseillée, rendue avec chaque verdict.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Plage {
    pub crete_max_dbfs: f64,
    pub rsb_absent_db: f64,
    pub rsb_min_db: f64,
    pub rsb_conseille_db: f64,
    pub spl_max_db: f64,
}

pub const PLAGE: Plage = Plage {
    crete_max_dbfs: CRETE_MAX_DBFS,
    rsb_absent_db: RSB_ABSENT_DB,
    rsb_min_db: RSB_MIN_DB,
    rsb_conseille_db: RSB_CONSEILLE_DB,
    spl_max_db: SPL_MAX_DB,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub jugement: Jugement,
    /// Code stable que le web traduit : `saturation`, `trop_fort_spl`,
    /// `trop_fort`, `entree_muette`, `signal_absent`, `rsb_faible`, `bon`.
    pub motif: &'static str,
    pub signal: Niveaux,
    pub bruit: Niveaux,
    /// Efficace du signal capté moins efficace du bruit de fond (dB).
    pub rsb_db: f64,
    /// Pression de crête au micro (dB SPL), avec un étalonnage seulement.
    pub spl_db: Option<f64>,
    /// La veille a coupé le signal pendant la montée (saturation ou SPL).
    pub coupe: bool,
    pub plage: Plage,
    /// La phrase, en français comme les autres refus du greffon.
    pub message: String,
}

/// Le verdict d'un test de niveau, d'après le bruit de fond et le signal
/// captés. Voir les seuils plus haut.
pub fn juger(bruit: Niveaux, signal: Niveaux, spl_db: Option<f64>, coupe: bool) -> Verdict {
    let rsb_db = signal.rms_dbfs - bruit.rms_dbfs;
    let (jugement, motif, message) = if signal.crete_dbfs >= CRETE_SATURATION_DBFS {
        (
            Jugement::TropFort,
            "saturation",
            format!(
                "saturation : crête captée {:.1} dBFS. Baissez le gain du micro (ou de l'interface), \
                 ou le volume de mesure",
                signal.crete_dbfs
            ),
        )
    } else if let Some(spl) = spl_db.filter(|s| *s > SPL_MAX_DB) {
        (
            Jugement::TropFort,
            "trop_fort_spl",
            format!(
                "trop fort : {spl:.0} dB SPL au micro, {SPL_MAX_DB:.0} dB SPL au plus. Le signal a été \
                 coupé ; baissez le volume de mesure"
            ),
        )
    } else if signal.crete_dbfs >= CRETE_MAX_DBFS {
        (
            Jugement::TropFort,
            "trop_fort",
            format!(
                "trop fort : crête captée {:.1} dBFS, {CRETE_MAX_DBFS:.0} dBFS au plus. Baissez le gain \
                 du micro ou le volume de mesure",
                signal.crete_dbfs
            ),
        )
    } else if bruit.crete_dbfs > SILENCE_NUMERIQUE_DBFS
        && signal.crete_dbfs <= SILENCE_NUMERIQUE_DBFS
    {
        (
            Jugement::TropFaible,
            "entree_muette",
            "l'entrée captait avant le signal, puis n'a rendu que du silence numérique pendant que \
             la zone jouait : la lecture coupe cette entrée (zone en ASIO sur la même interface ?)"
                .to_string(),
        )
    } else if rsb_db < RSB_ABSENT_DB {
        (
            Jugement::TropFaible,
            "signal_absent",
            format!(
                "le micro n'entend pas la zone ({rsb_db:.0} dB au-dessus du bruit de fond). Vérifiez \
                 l'entrée et la voie choisies, et que la zone joue (enceintes allumées)"
            ),
        )
    } else if rsb_db < RSB_MIN_DB {
        (
            Jugement::Faible,
            "rsb_faible",
            format!(
                "faible mais mesurable : {rsb_db:.0} dB au-dessus du bruit de fond ({RSB_MIN_DB:.0} dB \
                 pour un bon niveau, {RSB_CONSEILLE_DB:.0} dB conseillés). La mesure devra jouer plus \
                 fort que ce test ; pour plus de marge, montez le volume de mesure ou rapprochez le micro"
            ),
        )
    } else {
        (
            Jugement::Bon,
            "bon",
            format!(
                "bon niveau : crête {:.1} dBFS, {rsb_db:.0} dB au-dessus du bruit de fond",
                signal.crete_dbfs
            ),
        )
    };
    Verdict {
        jugement,
        motif,
        signal,
        bruit,
        rsb_db,
        spl_db,
        coupe,
        plage: PLAGE,
        message,
    }
}

/// Le signal du test : le bruit rose ([`bruit_rose`]) sous l'enveloppe de
/// la montée ([`PALIERS_DB`]), chaque marche lissée sur 10 ms.
pub fn signal_de_test(fs: u32, plafond_dbfs: f64) -> Vec<f64> {
    let mut x = bruit_rose(fs, DUREE_SIGNAL_S, EFFICACE_SIGNAL_DBFS, plafond_dbfs);
    let palier = (DUREE_PALIER_S * fs as f64) as usize;
    let lissage = (0.01 * fs as f64) as usize;
    let gain_a = |i: usize| -> f64 {
        let k = (i / palier.max(1)).min(PALIERS_DB.len() - 1);
        fft::from_db(PALIERS_DB[k])
    };
    let mut g = gain_a(0);
    for (i, v) in x.iter_mut().enumerate() {
        let cible = gain_a(i);
        // Glissement vers la marche suivante, sans clic.
        g += (cible - g) / lissage.max(1) as f64;
        *v *= g;
    }
    x
}

// ─── Le bruit rose ─────────────────────────────────────────────────────────

/// Un biquad RBJ du second ordre (Butterworth).
struct Filtre {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Filtre {
    fn nouveau(fs: f64, f0: f64, passe_haut: bool) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = if passe_haut {
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0]
        } else {
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0]
        };
        Self {
            b: [b[0] / a0, b[1] / a0, b[2] / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn filtrer(&mut self, v: f64) -> f64 {
        let y = self.b[0] * v + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [v, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// Le bruit rose du test : `duree_s` secondes à `fs`, limité à [`BANDE_HZ`],
/// d'efficace `efficace_dbfs`, fondu à l'entrée et à la sortie (50 ms),
/// jamais au-dessus de `plafond_dbfs` (crête). Déterministe.
pub fn bruit_rose(fs: u32, duree_s: f64, efficace_dbfs: f64, plafond_dbfs: f64) -> Vec<f64> {
    let fsf = fs as f64;
    let n = (duree_s * fsf).round() as usize;
    // Une demi-seconde de plus, jetée : le temps que les filtres se posent.
    let chauffe = (0.5 * fsf) as usize;
    let mut graine: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut b = [0.0f64; 7];
    let mut hp = Filtre::nouveau(fsf, BANDE_HZ.0, true);
    let mut lp = Filtre::nouveau(fsf, BANDE_HZ.1.min(0.45 * fsf), false);
    let mut x: Vec<f64> = (0..n + chauffe)
        .map(|_| {
            graine ^= graine << 13;
            graine ^= graine >> 7;
            graine ^= graine << 17;
            let w = (graine >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            // Filtre « rose » de Paul Kellet (−3 dB/octave à ±0,05 dB).
            b[0] = 0.99886 * b[0] + w * 0.055_517_9;
            b[1] = 0.99332 * b[1] + w * 0.075_075_9;
            b[2] = 0.96900 * b[2] + w * 0.153_852;
            b[3] = 0.86650 * b[3] + w * 0.310_485_6;
            b[4] = 0.55000 * b[4] + w * 0.532_952_2;
            b[5] = -0.7616 * b[5] - w * 0.016_898;
            let rose = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
            b[6] = w * 0.115_926;
            lp.filtrer(hp.filtrer(rose))
        })
        .collect();
    x.drain(..chauffe);
    let efficace = (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt();
    let k = fft::from_db(efficace_dbfs) / efficace.max(1e-30);
    let fondu = ((0.05 * fsf) as usize).min(n / 2).max(1);
    let plafond = fft::from_db(plafond_dbfs);
    for (i, v) in x.iter_mut().enumerate() {
        let bord = i.min(n - 1 - i);
        let g = if bord < fondu {
            0.5 - 0.5 * (std::f64::consts::PI * bord as f64 / fondu as f64).cos()
        } else {
            1.0
        };
        *v = (*v * k * g).clamp(-plafond, plafond);
    }
    x
}

// ─── Le fournisseur PCM du signal ──────────────────────────────────────────

/// Un signal de test armé : l'orchestrateur l'ouvre par son id.
pub struct SignalArme {
    pub format: FormatPcm,
    echantillons: Arc<Vec<f64>>,
    voies: Voies,
    amorce: u64,
    trames_annoncees: u64,
    cadence: Option<(HorlogeDEcoute, u64)>,
    annule: AtomicBool,
    /// Relecture m1 — la veille de sécurité a coupé le signal.
    coupe: AtomicBool,
    ouvert: Mutex<bool>,
    signal: Condvar,
    rendues: AtomicU64,
}

impl SignalArme {
    pub fn new(
        echantillons: Vec<f64>,
        voies: Voies,
        cadence: Option<(HorlogeDEcoute, u64)>,
    ) -> Arc<Self> {
        let format = FormatPcm {
            frequence: FREQUENCE_MESURE,
            canaux: 2,
            bits: 24,
        };
        Arc::new(Self {
            format,
            echantillons: Arc::new(echantillons),
            voies,
            amorce: (AMORCE_S * FREQUENCE_MESURE as f64) as u64,
            trames_annoncees: DUREE_ANNONCEE_MS * FREQUENCE_MESURE as u64 / 1000,
            cadence,
            annule: AtomicBool::new(false),
            coupe: AtomicBool::new(false),
            ouvert: Mutex::new(false),
            signal: Condvar::new(),
            rendues: AtomicU64::new(0),
        })
    }

    pub fn octets(&self) -> u64 {
        self.trames_annoncees * self.format.octets_par_trame()
    }

    /// Annule : le signal s'éteint en fondu ([`FONDU_S`]), le flux continue
    /// en silence jusqu'à l'arrêt de la zone.
    pub fn annuler(&self) {
        self.annule.store(true, Ordering::SeqCst);
    }

    /// Relecture m1 — coupe de sécurité : même fondu que l'annulation.
    pub fn couper(&self) {
        self.coupe.store(true, Ordering::SeqCst);
    }

    fn eteint(&self) -> bool {
        self.annule.load(Ordering::SeqCst) || self.coupe.load(Ordering::SeqCst)
    }

    pub fn ouvert(&self) -> bool {
        *self.ouvert.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Trames rendues à la zone jusqu'ici.
    pub fn trames_rendues(&self) -> u64 {
        self.rendues.load(Ordering::SeqCst)
    }

    pub fn attendre_ouverture(&self, delai: Duration) -> bool {
        let echeance = Instant::now() + delai;
        let mut o = self.ouvert.lock().unwrap_or_else(|e| e.into_inner());
        while !*o {
            if self.annule.load(Ordering::SeqCst) {
                return false;
            }
            let reste = echeance.saturating_duration_since(Instant::now());
            if reste.is_zero() {
                return false;
            }
            o = self
                .signal
                .wait_timeout(o, reste.min(Duration::from_millis(50)))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }

    fn marquer_ouvert(&self) {
        *self.ouvert.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signal.notify_all();
    }
}

/// Le `Read` du signal : amorce de silence, bruit rose, silence jusqu'à la
/// longueur annoncée (jamais de fin avant elle, comme #9526 pour le
/// balayage). Cadencé sur l'écoute comme le générateur de la mesure (#9066) :
/// jamais plus de [`AVANCE_SUR_L_ECOUTE_S`] d'avance sur le micro.
pub struct LecteurSignal {
    arme: Arc<SignalArme>,
    rendues: u64,
    /// Position dans le signal (amorce comprise) : elle n'avance pas quand
    /// l'écoute ne suit pas — le repli ne rend que du silence.
    position: u64,
    origine_ecoute: Option<u64>,
    reste: Vec<u8>,
    /// Gain de sortie : 1, puis vers 0 en [`FONDU_S`] une fois éteint.
    gain: f64,
}

impl LecteurSignal {
    fn attendre_l_ecoute(&mut self, bloc: u64) -> bool {
        let Some((horloge, avance)) = self.arme.cadence.clone() else {
            return true;
        };
        let origine = *self.origine_ecoute.get_or_insert_with(|| horloge());
        let echeance = Instant::now() + Duration::from_millis(100);
        loop {
            let ecoute = horloge().saturating_sub(origine);
            if self.position + bloc <= ecoute + avance || self.arme.eteint() {
                return true;
            }
            if Instant::now() >= echeance {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn produire(&mut self, n: u64, avancer: bool) -> Vec<u8> {
        let a = &self.arme;
        let n = n.min(a.trames_annoncees.saturating_sub(self.rendues));
        let mut sortie = Vec::with_capacity((n * a.format.octets_par_trame()) as usize);
        let eteint = a.eteint();
        let pas = 1.0 / (FONDU_S * a.format.frequence as f64).max(1.0);
        for _ in 0..n {
            let v = if !avancer || self.gain <= 0.0 {
                0.0
            } else {
                // Relecture m2 — éteint : fondu, jamais d'arrêt net.
                if eteint {
                    self.gain = (self.gain - pas).max(0.0);
                }
                let i = self.position;
                self.position += 1;
                i.checked_sub(a.amorce)
                    .and_then(|j| a.echantillons.get(j as usize).copied())
                    .unwrap_or(0.0)
                    * self.gain
            };
            let v = v.clamp(-1.0, 1.0);
            let (g, d) = match a.voies {
                Voies::Gauche => (v, 0.0),
                Voies::Droite => (0.0, v),
                Voies::LesDeux => (v, v),
            };
            for x in [g, d] {
                let e = (x * 8_388_607.0).round() as i32;
                sortie.extend_from_slice(&e.to_le_bytes()[..3]);
            }
        }
        self.rendues += n;
        a.rendues.store(self.rendues, Ordering::SeqCst);
        sortie
    }
}

impl Read for LecteurSignal {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.reste.is_empty() {
            if self.rendues >= self.arme.trames_annoncees {
                return Ok(0);
            }
            let bloc = (FREQUENCE_MESURE / 100) as u64;
            let suit = self.attendre_l_ecoute(bloc);
            self.reste = self.produire(bloc, suit);
        }
        let n = buf.len().min(self.reste.len());
        buf[..n].copy_from_slice(&self.reste[..n]);
        self.reste.drain(..n);
        Ok(n)
    }
}

/// Le fournisseur PCM `calibration-niveau`, inscrit auprès de
/// l'orchestrateur. Sans test armé, `ouvrir` refuse : aucune lecture sans le
/// geste de l'utilisateur.
#[derive(Default)]
pub struct FournisseurNiveau {
    armes: Mutex<HashMap<String, Arc<SignalArme>>>,
}

impl FournisseurNiveau {
    fn verrou(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SignalArme>>> {
        self.armes.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn armer(&self, id: &str, s: Arc<SignalArme>) {
        self.verrou().insert(id.to_string(), s);
    }
    pub fn desarmer(&self, id: &str) {
        if let Some(s) = self.verrou().remove(id) {
            s.annuler();
        }
    }
    pub fn armes(&self) -> usize {
        self.verrou().len()
    }
}

impl FournisseurPcm for FournisseurNiveau {
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        let arme = self
            .verrou()
            .get(source_id)
            .cloned()
            .ok_or_else(|| format!("aucun test de niveau armé sous « {source_id} »"))?;
        if depuis_ms != 0 {
            return Err("un signal de test ne s'ouvre qu'à son début".into());
        }
        if arme.annule.load(Ordering::SeqCst) {
            return Err("test de niveau annulé".into());
        }
        arme.marquer_ouvert();
        Ok(FluxPcm {
            format: arme.format,
            octets: arme.octets(),
            duree_ms: DUREE_ANNONCEE_MS,
            lecteur: Box::new(LecteurSignal {
                arme,
                rendues: 0,
                position: 0,
                origine_ecoute: None,
                reste: Vec::new(),
                gain: 1.0,
            }),
        })
    }

    fn pre_armable(&self) -> bool {
        false
    }

    /// Fil 3094 — `calibration-niveau` est une source technique : une ligne
    /// d'un test fini est écartée au lieu d'être rejouée.
    fn encore_jouable(&self, source_id: &str) -> bool {
        self.verrou().contains_key(source_id)
    }
}

// ─── Ce que le test demande à la zone ──────────────────────────────────────

/// Le signal à jouer, tel que la zone le reçoit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementDeTest {
    pub id: String,
    pub format: FormatPcm,
    pub duree_ms: u64,
}

impl ElementDeTest {
    pub fn titre(&self) -> String {
        "Calibration — test de niveau du micro".into()
    }
}

/// Ce que le test de niveau demande à l'hôte, en plus de [`crate::hote::HoteZone`]
/// (volume, traitements). Synchrone, comme `HoteZone`.
///
/// La zone orpheline (fil 3094) est celle de [`crate::hote::HoteZone::sortie_inactive`],
/// qui nomme aussi la zone porteuse de l'appareil.
pub trait ZoneDeTest: Send + Sync {
    /// Lance le signal sur la zone : ni la file ni l'historique ne changent.
    fn jouer_signal(&self, zone_id: i64, element: &ElementDeTest) -> Result<(), String>;
    /// Arrête la zone et efface le signal de ce qu'elle affiche.
    fn arreter_signal(&self, zone_id: i64, id: &str);
    /// La zone sort-elle en ASIO ? (voir [`sortie_asio`]).
    fn sortie_asio(&self, _zone_id: i64) -> bool {
        false
    }
    /// Relecture m3 — la zone joue (ou est en pause) : le test la couperait
    /// et ferait perdre la piste et sa position. Le test est alors refusé.
    fn zone_en_lecture(&self, _zone_id: i64) -> bool {
        false
    }
}

/// Une sortie ouverte en ASIO, d'après ce que la base en sait : son
/// identifiant d'endpoint cpal (`asio:<pilote>`), à défaut son nom (les
/// pilotes ASIO le portent presque tous : « TASCAM US-3x6 ASIO »).
/// L'entrée, elle, n'est jamais ASIO : Tune ne capte pas en ASIO. On ne
/// cherche pas à apparier les noms de l'entrée et de la sortie : Windows et
/// le pilote ne nomment pas l'interface de la même façon (« Microphone
/// (US-366) » contre « TASCAM US-3x6 ASIO »).
pub fn sortie_asio(output_endpoint_id: Option<&str>, output_device_id: Option<&str>) -> bool {
    let endpoint = output_endpoint_id.map(str::trim).unwrap_or_default();
    if endpoint.to_ascii_lowercase().starts_with("asio:") {
        return true;
    }
    output_device_id.is_some_and(|d| {
        d.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|mot| mot == "asio")
    })
}

/// La zone réelle, par l'orchestrateur.
pub struct ZoneOrchestrateur {
    pub backend: Arc<dyn tune_core::db::backend::DbBackend>,
    pub orchestrator: Arc<tune_core::orchestrator::PlaybackOrchestrator>,
    pub playback: Arc<tune_core::playback::PlaybackManager>,
    pub runtime: tokio::runtime::Handle,
    /// Relecture m3 — ce qu'une zone ARRÊTÉE affichait avant le test (piste,
    /// position) : rendu après, au lieu d'un affichage vide.
    pub affichages_avant: Mutex<HashMap<i64, (tune_core::playback::NowPlaying, i64)>>,
}

impl ZoneOrchestrateur {
    fn zone(&self, zone_id: i64) -> Option<tune_core::db::zone_repo::Zone> {
        tune_core::db::zone_repo::ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
    }
}

impl ZoneDeTest for ZoneOrchestrateur {
    fn zone_en_lecture(&self, zone_id: i64) -> bool {
        use tune_core::playback::PlayState;
        let etat = self.runtime.block_on(self.playback.get_state(zone_id));
        matches!(etat.state, PlayState::Playing | PlayState::Paused)
    }

    fn jouer_signal(&self, zone_id: i64, e: &ElementDeTest) -> Result<(), String> {
        let output_device_id = self.zone(zone_id).and_then(|z| z.output_device_id);
        let avant = self.runtime.block_on(self.playback.get_state(zone_id));
        if let Some(np) = avant.now_playing {
            self.affichages_avant
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(zone_id, (np, avant.position_ms));
        }
        let r = self
            .runtime
            .block_on(self.orchestrator.play_transfere_sans_historique(
                tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: None,
                    source: Some(SOURCE_NIVEAU.into()),
                    source_id: Some(e.id.clone()),
                    title: Some(e.titre()),
                    artist_name: None,
                    album_title: None,
                    cover_url: None,
                    duration_ms: Some(e.duree_ms as i64),
                    seek_ms: None,
                    temp_file_path: None,
                    sample_rate: Some(e.format.frequence),
                    bit_depth: Some(e.format.bits),
                    media_format: Some("wav".into()),
                    track_number: None,
                    disc_number: None,
                    album_ref: None,
                },
            ))?;
        match r.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn sortie_asio(&self, zone_id: i64) -> bool {
        let Some(zone) = self.zone(zone_id) else {
            return false;
        };
        if zone.output_type.as_deref() != Some("local") {
            return false;
        }
        let endpoint: Option<String> =
            tune_core::db::zone_repo::ZoneRepo::with_backend(self.backend.clone())
                .sorties_locales_et_leur_identite()
                .ok()
                .and_then(|l| l.into_iter().find(|z| z.id == zone_id))
                .and_then(|z| z.output_endpoint_id);
        sortie_asio(endpoint.as_deref(), zone.output_device_id.as_deref())
    }

    fn arreter_signal(&self, zone_id: i64, id: &str) {
        self.runtime.block_on(async {
            // Relecture m4 — seulement si la zone joue ENCORE le signal de
            // ce test : une musique lancée entre-temps par un autre client
            // n'est pas coupée.
            let affiche = self.playback.get_state(zone_id).await.now_playing;
            if !affiche
                .is_some_and(|np| np.source == SOURCE_NIVEAU && np.source_id.as_deref() == Some(id))
            {
                return;
            }
            let avant = self
                .affichages_avant
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&zone_id);
            self.orchestrator.stop(zone_id, None).await;
            // La zone n'affiche plus le signal : Lecture ne le relancera pas.
            self.playback.stop_and_clear(zone_id).await;
            // Relecture m3 — la piste qu'affichait la zone arrêtée revient.
            // `restore_position` remet aussi la reprise à cette position.
            if let Some((np, position_ms)) = avant {
                self.playback
                    .restore_position(zone_id, position_ms, np)
                    .await;
            }
        });
    }
}

// ─── La session ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Bruit de fond, bruit rose joué sur la zone, verdict.
    Signal,
    /// Le micro seul, sans rien jouer.
    Ecoute,
}

impl Mode {
    pub fn depuis(nom: &str) -> Option<Self> {
        match nom {
            "signal" => Some(Self::Signal),
            "ecoute" => Some(Self::Ecoute),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "etat", rename_all = "snake_case")]
pub enum EtatTest {
    Preparation,
    BruitDeFond,
    Signal,
    Ecoute,
    Terminee,
    Annulee,
    Erreur { message: String },
}

impl EtatTest {
    pub fn finale(&self) -> bool {
        matches!(self, Self::Terminee | Self::Annulee | Self::Erreur { .. })
    }
}

/// Ce que la route reçoit.
#[derive(Debug, Clone)]
pub struct DemandeNiveau {
    pub mode: Mode,
    pub entree: String,
    pub voie_micro: usize,
    /// Requise en mode signal.
    pub zone_id: Option<i64>,
    /// Volume de zone forcé pendant le signal (0–100), comme la mesure.
    pub volume_mesure_pct: f64,
    pub voies: Voies,
    /// Durée de l'écoute seule (s), bornée à [1, `ECOUTE_MAX_S`].
    pub duree_s: f64,
    /// Relecture m1 — l'étalonnage du micro : avec lui, la pression au micro
    /// est connue et plafonnée à [`SPL_MAX_DB`], comme pour la mesure.
    pub etalonnage: Option<String>,
}

impl DemandeNiveau {
    pub fn ecoute(entree: &str) -> Self {
        Self {
            mode: Mode::Ecoute,
            entree: entree.to_string(),
            voie_micro: 0,
            zone_id: None,
            volume_mesure_pct: crate::mesure::Demande::defaut(0, entree).volume_mesure_pct,
            voies: Voies::LesDeux,
            duree_s: ECOUTE_MAX_S,
            etalonnage: None,
        }
    }

    pub fn signal(zone_id: i64, entree: &str) -> Self {
        Self {
            mode: Mode::Signal,
            zone_id: Some(zone_id),
            ..Self::ecoute(entree)
        }
    }

    /// Les mêmes bornes que la mesure (`Demande::borner`).
    fn borner(mut self) -> Self {
        let mut m = crate::mesure::Demande::defaut(0, &self.entree);
        m.volume_mesure_pct = self.volume_mesure_pct;
        self.volume_mesure_pct = m.borner().volume_mesure_pct;
        self.duree_s = if self.duree_s.is_finite() {
            self.duree_s.clamp(1.0, ECOUTE_MAX_S)
        } else {
            ECOUTE_MAX_S
        };
        self.entree = self.entree.trim().to_string();
        self
    }
}

/// Pourquoi un test n'a pas démarré.
#[derive(Debug, Clone, PartialEq)]
pub enum RefusNiveau {
    /// Un test ou une mesure tient déjà l'entrée ou la zone (409).
    Occupe(String),
    /// Fil 3094 — la zone n'a pas de sortie active (409
    /// `zone_sans_sortie_active`), avec la zone qui porte l'appareil.
    SortieInactive(crate::hote::SortieInactive),
    /// Relecture m3 — la zone joue ou est en pause (409 `zone_en_lecture`).
    ZoneEnLecture,
    /// Tout autre refus motivé (422).
    Refus(String),
}

impl RefusNiveau {
    pub fn message(&self) -> String {
        match self {
            Self::Occupe(m) | Self::Refus(m) => m.clone(),
            Self::ZoneEnLecture => {
                "Arrêtez la lecture sur cette zone avant le test : il la couperait.".to_string()
            }
            Self::SortieInactive(inactive) => inactive.message(),
        }
    }
}

/// Le vumètre en direct : la dernière fenêtre de 100 ms, et la crête la
/// plus haute depuis le début du test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Vumetre {
    pub crete_dbfs: f64,
    pub rms_dbfs: f64,
    pub crete_max_dbfs: f64,
}

struct Interne {
    etat: EtatTest,
    code: Option<&'static str>,
    peripherique: Option<String>,
    vumetre: Option<Vumetre>,
    /// Trames déjà vues par le relevé : la crête maximale les couvre toutes.
    releve: u64,
    alerte_asio: bool,
    bruit: Option<Niveaux>,
    verdict: Option<Verdict>,
    avertissements: Vec<String>,
}

pub struct TestDeNiveau {
    pub id: String,
    pub demande: DemandeNiveau,
    debut: Instant,
    annule: AtomicBool,
    interne: Mutex<Interne>,
}

/// Ce que `GET /niveaux/{id}` rend.
#[derive(Debug, Clone, Serialize)]
pub struct AvancementNiveau {
    pub id: String,
    pub mode: Mode,
    pub zone_id: Option<i64>,
    pub entree: String,
    pub voie_micro: usize,
    #[serde(flatten)]
    pub etat: EtatTest,
    /// Code stable d'une erreur que l'écran traduit (`micro_muet`, `micro_fige`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peripherique: Option<String>,
    pub depuis_s: f64,
    /// Durée prévue du test (s) : la barre de l'écran.
    pub duree_prevue_s: f64,
    pub vumetre: Option<Vumetre>,
    pub bruit: Option<Niveaux>,
    pub verdict: Option<Verdict>,
    /// La zone sort en ASIO : l'entrée Windows de la même interface peut
    /// rester muette pendant la lecture ([`AVERTISSEMENT_ASIO`]).
    pub alerte_asio: bool,
    pub avertissements: Vec<String>,
}

impl TestDeNiveau {
    fn verrou(&self) -> std::sync::MutexGuard<'_, Interne> {
        self.interne.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn poser(&self, etat: EtatTest) {
        let mut i = self.verrou();
        if !i.etat.finale() {
            tracing::info!(test = %self.id, etat = ?etat, "calibration_niveau_etat");
            i.etat = etat;
        }
    }

    pub fn etat(&self) -> EtatTest {
        self.verrou().etat.clone()
    }

    pub fn verdict(&self) -> Option<Verdict> {
        self.verrou().verdict.clone()
    }

    pub fn annuler(&self) {
        self.annule.store(true, Ordering::SeqCst);
    }

    pub fn annule(&self) -> bool {
        self.annule.load(Ordering::SeqCst)
    }

    pub fn duree_prevue_s(&self) -> f64 {
        match self.demande.mode {
            Mode::Ecoute => self.demande.duree_s,
            Mode::Signal => {
                MISE_EN_ROUTE_S
                    + DUREE_BRUIT_DE_FOND_S
                    + AMORCE_S
                    + DUREE_SIGNAL_S
                    + MARGE_LATENCE_S
            }
        }
    }

    pub fn avancement(&self) -> AvancementNiveau {
        let i = self.verrou();
        AvancementNiveau {
            id: self.id.clone(),
            mode: self.demande.mode,
            zone_id: self.demande.zone_id,
            entree: self.demande.entree.clone(),
            voie_micro: self.demande.voie_micro,
            etat: i.etat.clone(),
            code: i.code,
            peripherique: i.peripherique.clone(),
            depuis_s: self.debut.elapsed().as_secs_f64(),
            duree_prevue_s: self.duree_prevue_s(),
            vumetre: i.vumetre,
            bruit: i.bruit,
            verdict: i.verdict.clone(),
            alerte_asio: i.alerte_asio,
            avertissements: i.avertissements.clone(),
        }
    }

    /// Le vumètre, d'après les trames captées jusqu'ici : la dernière
    /// fenêtre de 100 ms, et la crête de TOUTES les trames depuis le relevé
    /// précédent (aucune crête ne passe entre deux relevés).
    fn relever(&self, tampon: &Tampon) {
        let fs = tampon.format().frequence as f64;
        let n = (FENETRE_VUMETRE_S * fs) as u64;
        let fin = tampon.trames();
        let voie = self.demande.voie_micro;
        let x = tampon.extraire(voie, fin.saturating_sub(n), n as usize);
        let v = niveaux(&x);
        let depuis = self.verrou().releve.min(fin.saturating_sub(n));
        let nouvelles = tampon.extraire(voie, depuis, (fin - depuis) as usize);
        let crete_nouvelles = borne(fft::to_db(crete(&nouvelles)));
        let mut i = self.verrou();
        i.releve = fin;
        let max = i
            .vumetre
            .map_or(PLANCHER_DBFS, |p| p.crete_max_dbfs)
            .max(v.crete_dbfs)
            .max(crete_nouvelles);
        i.vumetre = Some(Vumetre {
            crete_dbfs: v.crete_dbfs,
            rms_dbfs: v.rms_dbfs,
            crete_max_dbfs: max,
        });
    }
}

/// Les tests en cours et récents, et le fournisseur de leur signal. Tenu
/// par le [`Controleur`], comme les simulations et les sessions.
#[derive(Default)]
pub struct Magasin {
    pub fournisseur: Arc<FournisseurNiveau>,
    zone: Mutex<Option<Arc<dyn ZoneDeTest>>>,
    tests: Mutex<HashMap<String, Arc<TestDeNiveau>>>,
    compteur: AtomicU64,
    /// Relecture m6 — vérification ET enregistrement d'un test ou d'une
    /// mesure sous ce même verrou : deux demandes simultanées (double clic,
    /// test et mesure) ne passent plus toutes les deux.
    admission: Mutex<()>,
}

impl Magasin {
    /// Branche la zone réelle (au `setup` du greffon) ou simulée (essais).
    pub fn brancher(&self, zone: Arc<dyn ZoneDeTest>) {
        *self.zone.lock().unwrap_or_else(|e| e.into_inner()) = Some(zone);
    }

    fn zone(&self) -> Option<Arc<dyn ZoneDeTest>> {
        self.zone.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Le verrou d'admission (voir le champ), tenu par `tester_niveau` et par
    /// `Controleur::lancer` jusqu'à l'enregistrement.
    pub fn admission(&self) -> std::sync::MutexGuard<'_, ()> {
        self.admission.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TestDeNiveau>>> {
        self.tests.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn test(&self, id: &str) -> Option<Arc<TestDeNiveau>> {
        self.verrou().get(id).cloned()
    }

    /// Le test en cours qui tient cette entrée ou cette zone, s'il y en a un.
    pub fn occupant(&self, entree: &str, zone_id: Option<i64>) -> Option<String> {
        let entree = entree.trim();
        self.verrou()
            .values()
            .find(|t| {
                !t.etat().finale()
                    && (t.demande.entree == entree
                        || (zone_id.is_some()
                            && t.demande.mode == Mode::Signal
                            && t.demande.zone_id == zone_id))
            })
            .map(|t| t.id.clone())
    }

    pub fn annuler(&self, id: &str) -> bool {
        match self.test(id) {
            Some(t) if !t.etat().finale() => {
                t.annuler();
                true
            }
            _ => false,
        }
    }

    pub fn annuler_tout(&self) {
        for t in self.verrou().values() {
            t.annuler();
        }
    }

    fn ranger(&self, t: Arc<TestDeNiveau>) {
        let mut m = self.verrou();
        m.insert(t.id.clone(), t);
        if m.len() > TESTS_GARDES {
            let mut finis: Vec<_> = m
                .values()
                .filter(|t| t.etat().finale())
                .map(|t| (t.debut, t.id.clone()))
                .collect();
            finis.sort();
            let trop = m.len() - TESTS_GARDES;
            for (_, id) in finis.into_iter().take(trop) {
                m.remove(&id);
            }
        }
    }
}

/// La zone pendant le signal : traitements contournés et volume de mesure
/// posés, puis rendus — quoi qu'il arrive (`Drop`), comme `ZoneTenue` de la
/// mesure.
struct ZoneTenuePourLeTest<'a> {
    controleur: &'a Controleur,
    zone: Arc<dyn ZoneDeTest>,
    zone_id: i64,
    volume_initial: f64,
    signal: Option<String>,
    _contournement: tune_core::audio::mesure_nue::Contournement,
}

impl Drop for ZoneTenuePourLeTest<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.signal.take() {
            self.zone.arreter_signal(self.zone_id, &id);
            self.controleur.niveaux.fournisseur.desarmer(&id);
        }
        if let Err(e) = self
            .controleur
            .hote
            .regler_volume(self.zone_id, self.volume_initial)
        {
            tracing::warn!(zone = self.zone_id, error = %e, "calibration_niveau_volume_non_restaure");
        }
    }
}

enum Fin {
    Annulee,
    Erreur(String),
}

/// Relecture m5 — si le fil du test panique, ses gardes (capture, zone
/// tenue, contournement) se rendent au déroulement de la pile, et celle-ci,
/// la dernière, pose l'état final : l'entrée et la zone ne restent pas
/// occupées jusqu'au redémarrage.
struct GardeDeFin(Arc<TestDeNiveau>);

impl Drop for GardeDeFin {
    fn drop(&mut self) {
        if !self.0.etat().finale() {
            tracing::error!(test = %self.0.id, "calibration_niveau_fil_interrompu");
            self.0.poser(EtatTest::Erreur {
                message:
                    "le test s'est interrompu (erreur interne) ; l'entrée et la zone sont rendues"
                        .into(),
            });
        }
    }
}

impl Controleur {
    pub fn test_niveau(&self, id: &str) -> Option<Arc<TestDeNiveau>> {
        self.niveaux.test(id)
    }

    /// Vérifie, puis lance le test sur son fil. Rend son id.
    pub fn tester_niveau(self: &Arc<Self>, demande: DemandeNiveau) -> Result<String, RefusNiveau> {
        let demande = demande.borner();
        if demande.entree.is_empty() {
            return Err(RefusNiveau::Refus(
                "préciser l'entrée audio du micro".into(),
            ));
        }
        // Relecture m6 — de la vérification à l'enregistrement, sous un seul
        // verrou, partagé avec `Controleur::lancer`.
        let _admission = self.niveaux.admission();
        // Une seule session par entrée ; ni pendant une mesure sur l'entrée
        // ou la zone (elle tient le micro et la zone).
        // L'écoute seule ne tient pas la zone : elle peut écouter pendant
        // que la zone joue (le cas ASIO).
        let zone_tenue = demande.zone_id.filter(|_| demande.mode == Mode::Signal);
        if let Some(id) = self.niveaux.occupant(&demande.entree, zone_tenue) {
            return Err(RefusNiveau::Occupe(format!(
                "un test de niveau est déjà en cours sur cette entrée ou cette zone ({id})"
            )));
        }
        if let Some(m) = self.mesures().into_iter().find(|m| {
            !m.etat().finale()
                && (m.demande.entree == demande.entree || Some(m.demande.zone_id) == zone_tenue)
        }) {
            return Err(RefusNiveau::Occupe(format!(
                "une mesure est déjà en cours sur cette entrée ou cette zone ({})",
                m.id
            )));
        }
        let (nom, format) = self
            .peripheriques
            .format_natif(&demande.entree)
            .map_err(RefusNiveau::Refus)?;
        if demande.voie_micro >= format.canaux as usize {
            return Err(RefusNiveau::Refus(format!(
                "voie {} demandée, l'entrée « {nom} » n'en a que {}",
                demande.voie_micro + 1,
                format.canaux
            )));
        }
        let etalonnage = match &demande.etalonnage {
            Some(n) => Some(self.etalonnage(n).map_err(RefusNiveau::Refus)?),
            None => None,
        };
        let mut avertissements = Vec::new();
        if format.frequence != FREQUENCE_MESURE {
            avertissements.push(format!(
                "l'entrée « {nom} » capte à {} Hz : la mesure exigera {FREQUENCE_MESURE} Hz",
                format.frequence
            ));
        }
        // Fil de Didier : une zone en ASIO peut couper l'entrée Windows de la
        // même interface. Dit dès le départ, en mode signal comme en écoute
        // (la zone nommée), sans rien refuser.
        let alerte_asio = match (demande.zone_id, self.niveaux.zone()) {
            (Some(z), Some(zone)) => zone.sortie_asio(z),
            _ => false,
        };
        if alerte_asio {
            avertissements.push(AVERTISSEMENT_ASIO.to_string());
        }
        let mut volume_initial = 0.0;
        let mut zone_de_test = None;
        if demande.mode == Mode::Signal {
            let zone_id = demande
                .zone_id
                .ok_or_else(|| RefusNiveau::Refus("préciser la zone qui joue le signal".into()))?;
            let zone = self.niveaux.zone().ok_or_else(|| {
                RefusNiveau::Refus("test de niveau indisponible : greffon non prêt".into())
            })?;
            volume_initial = self.hote.volume(zone_id).ok_or_else(|| {
                RefusNiveau::Refus(format!(
                    "zone {zone_id} inconnue : volume non lisible, test refusé"
                ))
            })?;
            if let Some(inactive) = self.hote.sortie_inactive(zone_id) {
                return Err(RefusNiveau::SortieInactive(inactive));
            }
            if zone.zone_en_lecture(zone_id) {
                return Err(RefusNiveau::ZoneEnLecture);
            }
            if self.hote.traitements(zone_id).volume_fixe {
                return Err(RefusNiveau::Refus(
                    "la zone est en volume fixe : le niveau joué n'est pas maîtrisable, test refusé".into(),
                ));
            }
            if let Some(t) = self.hote.type_de_sortie(zone_id)
                && crate::mesure::sortie_a_latence_propre(Some(&t))
            {
                avertissements.push(format!(
                    "zone en sortie « {t} » : sa latence propre peut empêcher la mesure ; une sortie locale est sûre"
                ));
            }
            zone_de_test = Some(zone);
        }
        let id = format!(
            "n{}-{}",
            self.niveaux.compteur.fetch_add(1, Ordering::Relaxed) + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let test = Arc::new(TestDeNiveau {
            id: id.clone(),
            demande,
            debut: Instant::now(),
            annule: AtomicBool::new(false),
            interne: Mutex::new(Interne {
                etat: EtatTest::Preparation,
                code: None,
                peripherique: None,
                vumetre: None,
                releve: 0,
                alerte_asio,
                bruit: None,
                verdict: None,
                avertissements,
            }),
        });
        self.niveaux.ranger(test.clone());
        let moi = self.clone();
        std::thread::Builder::new()
            .name("calibration-niveau".into())
            .spawn(move || {
                let _garde = GardeDeFin(test.clone());
                let fin = moi.derouler_test(&test, zone_de_test, volume_initial, etalonnage);
                match fin {
                    Ok(()) => test.poser(EtatTest::Terminee),
                    Err(Fin::Annulee) => test.poser(EtatTest::Annulee),
                    Err(Fin::Erreur(message)) => test.poser(EtatTest::Erreur { message }),
                }
            })
            .map_err(|e| RefusNiveau::Refus(format!("fil du test : {e}")))?;
        Ok(id)
    }

    fn derouler_test(
        &self,
        test: &Arc<TestDeNiveau>,
        zone: Option<Arc<dyn ZoneDeTest>>,
        volume_initial: f64,
        etalonnage: Option<tune_calibration::mic_cal::MicCalibration>,
    ) -> Result<(), Fin> {
        let d = &test.demande;
        let capacite_s = match d.mode {
            Mode::Ecoute => d.duree_s + 5.0,
            Mode::Signal => SIGNAL_MAX_S + 5.0,
        };
        let capture = CaptureVersTampon::demarrer(&self.peripheriques, &d.entree, capacite_s)
            .map_err(Fin::Erreur)?;
        test.verrou().peripherique = Some(capture.peripherique().to_string());
        let r = self.ecouter_et_juger(test, &capture, zone, volume_initial, etalonnage.as_ref());
        // L'entrée est TOUJOURS libérée.
        capture.arreter();
        if capture.debordements() > 0 {
            test.verrou().avertissements.push(format!(
                "{} débordement(s) de capture : des trames ont été perdues",
                capture.debordements()
            ));
        }
        r
    }

    /// Attend que le tampon tienne `besoin` trames, en relevant le vumètre ;
    /// s'arrête sur une annulation, un micro figé, l'échéance murale, ou
    /// quand `veille` (consultée à chaque relevé, toutes les 50 ms) le
    /// demande.
    fn capter_jusqu_a(
        &self,
        test: &TestDeNiveau,
        tampon: &Arc<Tampon>,
        besoin: u64,
        echeance: Instant,
        veille: &dyn Fn() -> bool,
    ) -> Result<bool, Fin> {
        let mut vu = tampon.trames();
        let mut vu_a = Instant::now();
        loop {
            if test.annule() {
                return Err(Fin::Annulee);
            }
            let attente = tampon.attendre_trames(besoin, PAS_DU_RELEVE);
            test.relever(tampon);
            if veille() {
                return Ok(true);
            }
            match attente {
                Ok(Some(_)) => return Ok(true),
                Ok(None) => {}
                Err(fin) => return Err(Fin::Erreur(format!("capture interrompue : {fin:?}"))),
            }
            if Instant::now() >= echeance {
                return Ok(false);
            }
            let t = tampon.trames();
            if t != vu {
                vu = t;
                vu_a = Instant::now();
            } else if vu_a.elapsed() >= MICRO_FIGE {
                let mut i = test.verrou();
                i.code = Some(CODE_MICRO_FIGE);
                let asio = if i.alerte_asio {
                    format!(". {AVERTISSEMENT_ASIO}")
                } else {
                    String::new()
                };
                return Err(Fin::Erreur(format!(
                    "le micro « {} » ne capte plus (débranché ou en veille ?) : aucune trame depuis {} s{asio}",
                    test.demande.entree,
                    MICRO_FIGE.as_secs()
                )));
            }
        }
    }

    fn ecouter_et_juger(
        &self,
        test: &Arc<TestDeNiveau>,
        capture: &CaptureVersTampon,
        zone: Option<Arc<dyn ZoneDeTest>>,
        volume_initial: f64,
        etalonnage: Option<&tune_calibration::mic_cal::MicCalibration>,
    ) -> Result<(), Fin> {
        // #9070 — un micro ouvert qui ne rend rien échoue ici, vite.
        if let Err(sans) = capture.attendre_la_premiere_trame(self.delai_premiere_trame) {
            test.verrou().code = sans.code();
            return Err(Fin::Erreur(sans.to_string()));
        }
        let d = &test.demande;
        let tampon = capture.tampon().clone();
        let fs = tampon.format().frequence as f64;
        let trames = |s: f64| (s * fs).round() as u64;

        if d.mode == Mode::Ecoute {
            test.poser(EtatTest::Ecoute);
            let echeance = test.debut + Duration::from_secs_f64(d.duree_s);
            let besoin = tampon.trames() + trames(d.duree_s);
            self.capter_jusqu_a(test, &tampon, besoin, echeance, &|| false)?;
            return Ok(());
        }

        let echeance = test.debut + Duration::from_secs_f64(SIGNAL_MAX_S);
        let zone = zone.ok_or_else(|| Fin::Erreur("aucune zone pour le signal".into()))?;
        let zone_id = d.zone_id.unwrap_or_default();

        // 1. Le bruit de fond, rien n'étant joué.
        test.poser(EtatTest::BruitDeFond);
        let debut_bruit = tampon.trames() + trames(MISE_EN_ROUTE_S);
        let fin_bruit = debut_bruit + trames(DUREE_BRUIT_DE_FOND_S);
        if !self.capter_jusqu_a(test, &tampon, fin_bruit, echeance, &|| false)? {
            return Err(Fin::Erreur(
                "capture trop lente : bruit de fond incomplet".into(),
            ));
        }
        let bruit = niveaux(&tampon.extraire(
            d.voie_micro,
            debut_bruit,
            trames(DUREE_BRUIT_DE_FOND_S) as usize,
        ));
        test.verrou().bruit = Some(bruit);

        // 2. Le signal, au volume de mesure, traitements contournés.
        let contournement = tune_core::audio::mesure_nue::Contournement::poser(zone_id);
        let politique = LevelPolicy::default();
        self.hote
            .regler_volume(zone_id, d.volume_mesure_pct)
            .map_err(|e| Fin::Erreur(format!("volume de mesure refusé : {e}")))?;
        let mut tenue = ZoneTenuePourLeTest {
            controleur: self,
            zone: zone.clone(),
            zone_id,
            volume_initial,
            signal: None,
            _contournement: contournement,
        };
        let horloge: HorlogeDEcoute = {
            let t = tampon.clone();
            Arc::new(move || t.trames())
        };
        let arme = SignalArme::new(
            signal_de_test(FREQUENCE_MESURE, politique.max_play_dbfs),
            d.voies,
            Some((
                horloge,
                (AVANCE_SUR_L_ECOUTE_S * FREQUENCE_MESURE as f64) as u64,
            )),
        );
        self.niveaux.fournisseur.armer(&test.id, arme.clone());
        tenue.signal = Some(test.id.clone());
        test.poser(EtatTest::Signal);
        let element = ElementDeTest {
            id: test.id.clone(),
            format: arme.format,
            duree_ms: DUREE_ANNONCEE_MS,
        };
        zone.jouer_signal(zone_id, &element)
            .map_err(|e| Fin::Erreur(format!("la zone n'a pas joué le signal de test : {e}")))?;
        if !arme.attendre_ouverture(DELAI_OUVERTURE) {
            return Err(if test.annule() {
                Fin::Annulee
            } else {
                Fin::Erreur("la zone n'a pas ouvert le signal de test".into())
            });
        }
        let debut_signal = tampon.trames();
        let fenetre = trames(AMORCE_S + DUREE_SIGNAL_S + MARGE_LATENCE_S);
        // Relecture m1 — la veille : saturation ou pression trop forte ⇒
        // coupure en fondu, sans attendre la fin de la montée.
        let veille = || {
            let Some(m) = test.verrou().vumetre else {
                return false;
            };
            let spl = etalonnage.and_then(|c| c.spl_from_dbfs(m.crete_max_dbfs));
            let danger =
                m.crete_max_dbfs >= CRETE_SATURATION_DBFS || spl.is_some_and(|s| s > SPL_MAX_DB);
            if danger && !arme.coupe.load(Ordering::SeqCst) {
                tracing::warn!(test = %test.id, crete = m.crete_max_dbfs, spl = ?spl, "calibration_niveau_coupure");
                arme.couper();
            }
            danger
        };
        let complet =
            match self.capter_jusqu_a(test, &tampon, debut_signal + fenetre, echeance, &veille) {
                Ok(c) => c,
                Err(fin) => {
                    // Relecture m2 — annulation (ou micro perdu) : fondu, puis
                    // le chemin de lecture se vide, puis seulement l'arrêt.
                    arme.annuler();
                    std::thread::sleep(VIDAGE_APRES_FONDU);
                    return Err(fin);
                }
            };
        let coupe = arme.coupe.load(Ordering::SeqCst);
        if coupe {
            std::thread::sleep(VIDAGE_APRES_FONDU);
            test.verrou()
                .avertissements
                .push("signal coupé en fondu pendant la montée : niveau dangereux au micro".into());
        }
        // La zone est rendue AVANT l'analyse : rien ne joue plus.
        drop(tenue);
        let capte = tampon.extraire(d.voie_micro, debut_signal, fenetre as usize);
        if !complet && !coupe {
            test.verrou()
                .avertissements
                .push("fenêtre captée incomplète : verdict sur ce qui a été capté".into());
        }
        let bloc = trames(0.1) as usize;
        let signal = niveaux_du_signal(&capte, bloc, (DUREE_RETENUE_S / 0.1).round() as usize);
        let spl = etalonnage.and_then(|c| c.spl_from_dbfs(signal.crete_dbfs));
        let verdict = juger(bruit, signal, spl, coupe);
        tracing::info!(
            test = %test.id,
            jugement = ?verdict.jugement,
            motif = verdict.motif,
            crete = signal.crete_dbfs,
            efficace = signal.rms_dbfs,
            bruit = bruit.rms_dbfs,
            rsb = verdict.rsb_db,
            "calibration_niveau_verdict"
        );
        test.verrou().verdict = Some(verdict);
        Ok(())
    }
}

// ─── Les routes ────────────────────────────────────────────────────────────

pub(crate) mod http {
    use axum::Json;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use serde::Deserialize;
    use serde_json::json;

    use super::{DemandeNiveau, Mode, RefusNiveau};
    use crate::generateur::Voies;
    use crate::routes::EtatRoutes;

    fn refus(code: StatusCode, motif: &str, message: String) -> Response {
        (code, Json(json!({ "error": motif, "message": message }))).into_response()
    }

    #[derive(Deserialize, Default)]
    pub(crate) struct Corps {
        mode: Option<String>,
        #[serde(default)]
        entree: String,
        #[serde(default)]
        voie_micro: usize,
        zone_id: Option<i64>,
        volume: Option<f64>,
        voies: Option<String>,
        duree_s: Option<f64>,
        etalonnage: Option<String>,
    }

    /// `POST /niveaux` — `{ mode: "signal" | "ecoute", entree, voie_micro,
    /// zone_id (signal), volume, voies, duree_s (écoute) }` → 202 `{ id }`.
    pub(crate) async fn lancer(State(etat): State<EtatRoutes>, Json(c): Json<Corps>) -> Response {
        let Some(mode) = Mode::depuis(c.mode.as_deref().unwrap_or("signal")) else {
            return refus(
                StatusCode::BAD_REQUEST,
                "mode_invalide",
                "mode : signal ou ecoute".into(),
            );
        };
        if c.entree.trim().is_empty() {
            return refus(
                StatusCode::BAD_REQUEST,
                "entree_requise",
                "préciser l'entrée audio du micro".into(),
            );
        }
        let mut d = match (mode, c.zone_id) {
            (Mode::Signal, Some(z)) => DemandeNiveau::signal(z, &c.entree),
            (Mode::Signal, None) => {
                return refus(
                    StatusCode::BAD_REQUEST,
                    "zone_requise",
                    "préciser la zone qui joue le signal".into(),
                );
            }
            (Mode::Ecoute, z) => DemandeNiveau {
                // La zone, facultative, ne sert qu'à l'avertissement ASIO.
                zone_id: z,
                ..DemandeNiveau::ecoute(&c.entree)
            },
        };
        d.voie_micro = c.voie_micro;
        d.etalonnage = c.etalonnage.filter(|s| !s.trim().is_empty());
        if let Some(v) = c.volume {
            d.volume_mesure_pct = v;
        }
        if let Some(s) = c.duree_s {
            d.duree_s = s;
        }
        if let Some(v) = c.voies.as_deref() {
            match Voies::depuis(v) {
                Some(v) => d.voies = v,
                None => {
                    return refus(
                        StatusCode::BAD_REQUEST,
                        "voies_invalides",
                        "voies : gauche, droite ou deux".into(),
                    );
                }
            }
        }
        let zone_id = d.zone_id;
        let controleur = etat.controleur.clone();
        let r = tokio::task::spawn_blocking(move || controleur.tester_niveau(d))
            .await
            .unwrap_or_else(|e| Err(RefusNiveau::Refus(e.to_string())));
        match r {
            Ok(id) => {
                let alerte_asio = etat
                    .controleur
                    .test_niveau(&id)
                    .is_some_and(|t| t.avancement().alerte_asio);
                (
                    StatusCode::ACCEPTED,
                    Json(json!({ "id": id, "mode": mode, "zone_id": zone_id, "alerte_asio": alerte_asio })),
                )
                    .into_response()
            }
            Err(RefusNiveau::Occupe(m)) => refus(StatusCode::CONFLICT, "test_niveau_en_cours", m),
            Err(e @ RefusNiveau::ZoneEnLecture) => {
                refus(StatusCode::CONFLICT, "zone_en_lecture", e.message())
            }
            // Le refus de la mesure, tel quel (fil 3094) : la zone porteuse
            // nommée en paramètres.
            Err(RefusNiveau::SortieInactive(inactive)) => {
                crate::routes::refus_sortie_inactive(&inactive)
            }
            Err(RefusNiveau::Refus(m)) => {
                refus(StatusCode::UNPROCESSABLE_ENTITY, "test_niveau_refuse", m)
            }
        }
    }

    /// `GET /niveaux/{id}` — l'avancement, le vumètre, le verdict.
    pub(crate) async fn avancement(
        State(etat): State<EtatRoutes>,
        Path(id): Path<String>,
    ) -> Response {
        match etat.controleur.test_niveau(&id) {
            Some(t) => Json(t.avancement()).into_response(),
            None => refus(
                StatusCode::NOT_FOUND,
                "test_niveau_inconnu",
                format!("aucun test de niveau « {id} »"),
            ),
        }
    }

    /// `DELETE /niveaux/{id}` — annuler (la zone et l'entrée sont rendues).
    pub(crate) async fn annuler(
        State(etat): State<EtatRoutes>,
        Path(id): Path<String>,
    ) -> Response {
        match etat.controleur.test_niveau(&id) {
            None => refus(
                StatusCode::NOT_FOUND,
                "test_niveau_inconnu",
                format!("aucun test de niveau « {id} »"),
            ),
            Some(t) => {
                let demandee = etat.controleur.niveaux.annuler(&id);
                Json(json!({ "id": id, "annulation_demandee": demandee, "etat": t.etat() }))
                    .into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sinus(fs: f64, f: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / fs).sin())
            .collect()
    }

    /// Crête et efficace : un sinus à −20 dBFS crête a −23 dBFS d'efficace ;
    /// une composante continue ne compte pas dans l'efficace ; le silence
    /// est borné au plancher (pas de `-inf`, qui deviendrait `null`).
    #[test]
    fn crete_et_efficace_d_un_signal_synthetique() {
        let x = sinus(48_000.0, 1_000.0, fft::from_db(-20.0), 48_000);
        let n = niveaux(&x);
        assert!((n.crete_dbfs + 20.0).abs() < 0.01, "{n:?}");
        assert!((n.rms_dbfs + 23.01).abs() < 0.05, "{n:?}");
        let decale: Vec<f64> = x.iter().map(|v| v + 0.1).collect();
        assert!((niveaux(&decale).rms_dbfs + 23.01).abs() < 0.05);
        assert_eq!(niveaux(&vec![0.0; 480]), Niveaux::SILENCE);
        assert_eq!(niveaux(&[]), Niveaux::SILENCE);
    }

    /// L'efficace du signal ne compte que ses blocs les plus forts : 3 s de
    /// sinus placées n'importe où dans 6 s de silence donnent le même niveau.
    #[test]
    fn l_efficace_du_signal_ignore_la_latence() {
        let fs = 48_000.0;
        let ton = sinus(fs, 500.0, fft::from_db(-12.0), 3 * 48_000);
        for retard in [0usize, 4_800, 96_000] {
            let mut x = vec![0.0; 6 * 48_000];
            x[retard..retard + ton.len()].copy_from_slice(&ton);
            let n = niveaux_du_signal(&x, 4_800, 30);
            assert!((n.rms_dbfs + 15.01).abs() < 0.1, "retard {retard} : {n:?}");
            assert!((n.crete_dbfs + 12.0).abs() < 0.01);
        }
    }

    fn n(crete: f64, rms: f64) -> Niveaux {
        Niveaux {
            crete_dbfs: crete,
            rms_dbfs: rms,
        }
    }

    /// Les cinq verdicts, aux seuils documentés.
    #[test]
    fn le_verdict_suit_les_seuils() {
        let bruit = n(-60.0, -70.0);
        let j = |b, s| juger(b, s, None, false);
        let v = j(bruit, n(-0.2, -12.0));
        assert_eq!((v.jugement, v.motif), (Jugement::TropFort, "saturation"));
        let v = j(bruit, n(-2.0, -14.0));
        assert_eq!((v.jugement, v.motif), (Jugement::TropFort, "trop_fort"));
        assert!(v.message.contains("-2.0 dBFS"), "{}", v.message);
        let v = j(bruit, n(-55.0, -69.5));
        assert_eq!(
            (v.jugement, v.motif),
            (Jugement::TropFaible, "signal_absent")
        );
        // Relecture m7 — 3 à 6 dB : faible mais mesurable, plus « absent ».
        for rsb in [1.5, 3.0, 6.0, 9.9] {
            let v = j(bruit, n(-45.0, -70.0 + rsb));
            assert_eq!(
                (v.jugement, v.motif),
                (Jugement::Faible, "rsb_faible"),
                "{rsb}"
            );
            assert!(v.message.contains("mesurable"), "{}", v.message);
        }
        // Relecture m1 — étalonnage : la pression au micro passe le plafond.
        let v = juger(bruit, n(-12.0, -24.0), Some(95.0), true);
        assert_eq!((v.jugement, v.motif), (Jugement::TropFort, "trop_fort_spl"));
        assert!(v.coupe && v.spl_db == Some(95.0));
        assert_eq!(
            juger(bruit, n(-12.0, -24.0), Some(80.0), false).motif,
            "bon"
        );
        let v = j(bruit, n(-15.0, -27.0));
        assert_eq!((v.jugement, v.motif), (Jugement::Bon, "bon"));
        assert_eq!(v.plage, PLAGE);
        // Fil de Didier : l'entrée vivait avant le signal, zéros pendant.
        let v = j(n(-70.0, -80.0), Niveaux::SILENCE);
        assert_eq!(
            (v.jugement, v.motif),
            (Jugement::TropFaible, "entree_muette")
        );
        // Une voie muette d'un bout à l'autre n'est pas ce cas-là.
        let v = j(Niveaux::SILENCE, Niveaux::SILENCE);
        assert_eq!(v.motif, "signal_absent");
        // Juste aux bornes.
        assert_eq!(
            j(bruit, n(CRETE_MAX_DBFS, -20.0)).jugement,
            Jugement::TropFort
        );
        assert_eq!(
            j(bruit, n(-10.0, -70.0 + RSB_MIN_DB)).jugement,
            Jugement::Bon
        );
    }

    /// Une zone ASIO se reconnaît à son endpoint cpal, à défaut au mot
    /// « ASIO » de son nom ; WASAPI, ALSA et les noms qui contiennent
    /// « asio » dans un autre mot n'en sont pas.
    #[test]
    fn une_sortie_asio_est_reconnue() {
        assert!(sortie_asio(
            Some("asio:TASCAM US-3x6 ASIO"),
            Some("local:US-366")
        ));
        assert!(sortie_asio(None, Some("local:TASCAM US-3x6 ASIO")));
        assert!(sortie_asio(Some("ASIO:Focusrite USB ASIO"), None));
        assert!(!sortie_asio(
            Some("wasapi:{0.0.0.00000000}.{abc}"),
            Some("local:Haut-parleurs (US-366)")
        ));
        assert!(!sortie_asio(None, Some("local:Fantasiom DAC")));
        assert!(!sortie_asio(None, None));
    }

    /// Le bruit rose : la bonne efficace, sous le plafond joué, limité en
    /// bande (rien sous 20 Hz de notable), fondu aux deux bouts.
    #[test]
    fn le_bruit_rose_tient_son_efficace_et_son_plafond() {
        let x = bruit_rose(48_000, DUREE_SIGNAL_S, EFFICACE_SIGNAL_DBFS, -6.0);
        assert_eq!(x.len(), (DUREE_SIGNAL_S * 48_000.0) as usize);
        let milieu = &x[4_800..x.len() - 4_800];
        let e = 10.0 * (milieu.iter().map(|v| v * v).sum::<f64>() / milieu.len() as f64).log10();
        assert!((e - EFFICACE_SIGNAL_DBFS).abs() < 0.5, "efficace {e:.2}");
        assert!(crete(&x) <= fft::from_db(-6.0) + 1e-12);
        let c = fft::to_db(crete(&x)) - e;
        assert!((8.0..16.0).contains(&c), "facteur de crête {c:.1} dB");
        assert!(x[0].abs() < 1e-6 && x[x.len() - 1].abs() < 1e-3);
        assert_eq!(
            x,
            bruit_rose(48_000, DUREE_SIGNAL_S, EFFICACE_SIGNAL_DBFS, -6.0)
        );
    }

    /// Relecture m1 — le signal monte comme la mesure : 12 dB sous la
    /// référence, puis 6 dB sous, puis la référence.
    #[test]
    fn la_montee_part_douze_db_sous_la_reference() {
        let x = signal_de_test(48_000, -6.0);
        let efficace = |a: usize, b: usize| {
            let s = &x[a..b];
            10.0 * (s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64).log10()
        };
        // Premier palier (après le fondu d'entrée), deuxième, référence.
        let p1 = efficace(4_800, 21_600);
        let p2 = efficace(26_400, 45_600);
        let p3 = efficace(60_000, 180_000);
        assert!((p3 - EFFICACE_SIGNAL_DBFS).abs() < 0.7, "{p3:.1}");
        assert!((p3 - p1 - 12.0).abs() < 1.5, "{p1:.1} → {p3:.1}");
        assert!((p3 - p2 - 6.0).abs() < 1.5, "{p2:.1} → {p3:.1}");
    }

    /// Relecture m2 — l'annulation éteint le signal en fondu (20 ms), sans
    /// saut : chaque échantillon ne descend que d'un pas.
    #[test]
    fn l_annulation_eteint_en_fondu() {
        let f = FournisseurNiveau::default();
        let s = SignalArme::new(vec![0.5; 96_000], Voies::Gauche, None);
        f.armer("n-2", s.clone());
        let mut flux = f.ouvrir("n-2", 0).unwrap();
        let amorce = (AMORCE_S * 48_000.0) as usize;
        let mut debut = vec![0u8; (amorce + 4_800) * 6];
        flux.lecteur.read_exact(&mut debut).unwrap();
        s.annuler();
        let mut suite = vec![0u8; 4_800 * 6];
        flux.lecteur.read_exact(&mut suite).unwrap();
        let gauche: Vec<f64> = suite
            .chunks_exact(6)
            .map(|t| (i32::from_le_bytes([0, t[0], t[1], t[2]]) >> 8) as f64 / 8_388_608.0)
            .collect();
        assert!(gauche[0] > 0.45, "{}", gauche[0]);
        assert!(gauche.windows(2).all(|w| w[0] - w[1] < 0.5 / 900.0 + 1e-6));
        assert_eq!(gauche[1_000], 0.0, "le fondu dure 20 ms");
        assert!(gauche[1_000..].iter().all(|v| *v == 0.0));
    }

    /// Le lecteur : amorce de silence, signal, puis silence jusqu'à la
    /// longueur annoncée ; une voie seule.
    #[test]
    fn le_lecteur_rend_la_longueur_annoncee() {
        let f = FournisseurNiveau::default();
        assert!(f.ouvrir("n-1", 0).is_err());
        let s = SignalArme::new(vec![0.5; 4_800], Voies::Droite, None);
        f.armer("n-1", s.clone());
        assert!(f.ouvrir("n-1", 10).is_err());
        let mut flux = f.ouvrir("n-1", 0).unwrap();
        assert!(s.ouvert());
        assert_eq!(flux.duree_ms, DUREE_ANNONCEE_MS);
        let mut tout = Vec::new();
        flux.lecteur.read_to_end(&mut tout).unwrap();
        assert_eq!(tout.len() as u64, flux.octets);
        let trame = |i: usize| (&tout[i * 6..i * 6 + 3], &tout[i * 6 + 3..i * 6 + 6]);
        let amorce = (AMORCE_S * 48_000.0) as usize;
        assert_eq!(trame(amorce - 1), (&[0u8, 0, 0][..], &[0u8, 0, 0][..]));
        let (g, d) = trame(amorce);
        assert_eq!(g, &[0, 0, 0]);
        assert_ne!(d, &[0, 0, 0]);
        assert_eq!(trame(amorce + 4_800).1, &[0, 0, 0]);
        f.desarmer("n-1");
        assert!(f.ouvrir("n-1", 0).is_err());
        assert_eq!(f.armes(), 0);
    }
}
