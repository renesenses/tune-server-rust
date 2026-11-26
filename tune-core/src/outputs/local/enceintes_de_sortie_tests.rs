//! Témoins du réglage des enceintes d'une sortie locale.
//!
//! Méthode de la relecture de #9523 : un sinus de 50 Hz d'amplitude 0,8, et
//! le plus grand saut d'un échantillon au suivant, qui ne doit jamais
//! dépasser TROIS fois le pas naturel du sinus.

use super::*;
use crate::audio::reglage_enceintes::VoieEnceinte;

const CADENCE: u32 = 48_000;
const BLOC: usize = 512;

fn sinus(n: usize, cadence: u32) -> f64 {
    0.8 * (2.0 * std::f64::consts::PI * 50.0 * n as f64 / f64::from(cadence)).sin()
}

/// Le plus grand pas d'un échantillon au suivant du sinus lui-même.
fn pas_naturel(cadence: u32) -> f64 {
    0.8 * 2.0 * std::f64::consts::PI * 50.0 / f64::from(cadence)
}

/// La voie 0 à `loin` cm, la voie 1 à `pres` cm (gain `gain_1` dB).
fn deux_voies(loin: f64, pres: f64, gain_1: f64) -> ReglagesEnceintes {
    ReglagesEnceintes {
        channels: vec![
            VoieEnceinte {
                distance_cm: Some(loin),
                ..VoieEnceinte::default()
            },
            VoieEnceinte {
                distance_cm: Some(pres),
                gain_db: gain_1,
                invert: false,
            },
        ],
        invert_all: false,
    }
}

/// Une sortie stéréo qui joue le sinus, bloc par bloc, comme l'étage de
/// conversion (`convertir`).
struct Sortie {
    cle: Box<AtomicBool>,
    n: usize,
    entree: Vec<f64>,
    sortie: Vec<f64>,
}

impl Sortie {
    fn new() -> Self {
        Self {
            cle: Box::new(AtomicBool::new(false)),
            n: 0,
            entree: Vec::new(),
            sortie: Vec::new(),
        }
    }

    fn jouer(&mut self, secondes: f64) {
        let total = (secondes * f64::from(CADENCE)) as usize;
        let mut fait = 0;
        while fait < total {
            let k = BLOC.min(total - fait);
            let mut b: Vec<f64> = (0..k)
                .flat_map(|i| {
                    let x = sinus(self.n + i, CADENCE);
                    [x, x]
                })
                .collect();
            self.entree.extend_from_slice(&b);
            traiter(&self.cle, &mut b, 2, CADENCE, false);
            self.sortie.extend_from_slice(&b);
            self.n += k;
            fait += k;
        }
    }

    /// Le plus grand saut d'une voie, à partir de la trame `depuis`.
    fn saut_max(&self, voie: usize, depuis: usize) -> f64 {
        let v: Vec<f64> = self.sortie.iter().skip(voie).step_by(2).copied().collect();
        v[depuis.max(1) - 1..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max)
    }

    fn trames(&self) -> usize {
        self.sortie.len() / 2
    }
}

impl Drop for Sortie {
    fn drop(&mut self) {
        oublier(&self.cle);
    }
}

#[test]
fn une_sortie_sans_reglage_n_est_pas_touchee() {
    let s = Sortie::new();
    let mut t = vec![0.5_f32; 64];
    assert!(!traiter(&s.cle, &mut t, 2, 44_100, false));
    assert!(!agit(&s.cle, false));
    // Le premier `poser` (neutre, au démarrage d'une lecture) crée le
    // créneau : la ligne à retard est désormais alimentée, sans rien toucher.
    poser(&s.cle, true, None);
    assert_eq!(reglages(&s.cle), Some(ReglagesEnceintes::default()));
    assert!(!traiter(&s.cle, &mut t, 2, 44_100, false));
    assert!(t.iter().all(|x| *x == 0.5));
    assert!(!agit(&s.cle, false));
    assert_eq!(voies_vues(&s.cle), Some(2));
}

/// M2 — du neutre vers 100 cm, 300 cm et 10 m d'écart, pendant la lecture :
/// la ligne à retard était alimentée au repos, le fondu ne lit jamais de
/// silence ; aucun saut au-delà de 3 fois le pas naturel, puis la voie
/// proche est EXACTEMENT le sinus retardé.
#[test]
fn du_neutre_a_un_delai_sans_saut_100_300_et_1000_cm() {
    for ecart in [100.0, 300.0, 1000.0] {
        let mut s = Sortie::new();
        poser(&s.cle, true, None);
        s.jouer(1.0);
        let pose = s.trames();
        poser(&s.cle, true, Some(deux_voies(ecart, 0.0, 0.0)));
        s.jouer(1.0);
        let saut = s.saut_max(0, 1).max(s.saut_max(1, 1));
        eprintln!(
            "saut_mesure neutre->{ecart}cm : {saut:.5} (pas {:.5})",
            pas_naturel(CADENCE)
        );
        assert!(
            saut <= 3.0 * pas_naturel(CADENCE),
            "{ecart} cm : saut {saut} (pas naturel {})",
            pas_naturel(CADENCE)
        );
        let d = deux_voies(ecart, 0.0, 0.0).retards_en_echantillons(2, CADENCE)[1];
        let fin = s.trames();
        for n in fin - 4_800..fin {
            assert_eq!(s.sortie[n * 2], s.entree[n * 2], "{ecart} cm, lointaine");
            assert_eq!(
                s.sortie[n * 2 + 1],
                s.entree[(n - d) * 2 + 1],
                "{ecart} cm, proche"
            );
        }
        assert!(pose > 0);
    }
}

/// M1 — premier réglage d'une sortie qui joue déjà, créneau créé en pleine
/// lecture (aucun `poser` avant) : le processeur part du neutre, attend que
/// l'historique couvre le délai (10 m : 29 ms, plus long que le fondu), puis
/// fond vers le réglage. Aucun saut.
#[test]
fn premier_reglage_pendant_la_lecture_sans_saut() {
    let mut s = Sortie::new();
    s.jouer(0.5);
    poser(&s.cle, true, Some(deux_voies(1000.0, 0.0, -3.0)));
    s.jouer(1.0);
    let saut = s.saut_max(0, 1).max(s.saut_max(1, 1));
    eprintln!("saut_mesure premier reglage 10 m : {saut:.5}");
    assert!(saut <= 3.0 * pas_naturel(CADENCE), "saut {saut}");
    let d = deux_voies(1000.0, 0.0, -3.0).retards_en_echantillons(2, CADENCE)[1];
    let f = 10f64.powf(-3.0 / 20.0);
    let fin = s.trames();
    for n in fin - 1_000..fin {
        assert!((s.sortie[n * 2 + 1] - f * s.entree[(n - d) * 2 + 1]).abs() < 1e-12);
    }
}

/// M3 — sortie de PURE : pendant PURE, rien n'est touché (au bit près) et la
/// ligne est alimentée ; au retour, fondu depuis le neutre, sans saut et
/// sans rejouer un son d'avant PURE.
#[test]
fn sortie_de_pure_sans_saut() {
    let mut s = Sortie::new();
    poser(&s.cle, true, Some(deux_voies(300.0, 0.0, -6.0)));
    s.jouer(0.5);
    s.cle.store(true, Ordering::Relaxed);
    let debut_pure = s.trames();
    s.jouer(0.3);
    for n in debut_pure * 2..s.sortie.len() {
        assert_eq!(
            s.sortie[n].to_bits(),
            s.entree[n].to_bits(),
            "PURE : intact"
        );
    }
    assert!(!agit(&s.cle, false));
    s.cle.store(false, Ordering::Relaxed);
    let retour = s.trames();
    s.jouer(0.5);
    let saut = s.saut_max(0, retour).max(s.saut_max(1, retour));
    eprintln!("saut_mesure sortie de PURE : {saut:.5}");
    assert!(saut <= 3.0 * pas_naturel(CADENCE), "saut au retour {saut}");
    let d = deux_voies(300.0, 0.0, -6.0).retards_en_echantillons(2, CADENCE)[1];
    let f = 10f64.powf(-6.0 / 20.0);
    let fin = s.trames();
    for n in fin - 1_000..fin {
        assert!((s.sortie[n * 2 + 1] - f * s.entree[(n - d) * 2 + 1]).abs() < 1e-12);
    }
}

/// Retrait d'un réglage actif, et changement de distance à chaud : fondus,
/// aucun saut ; au bout du retrait, le flux est intact au bit près.
#[test]
fn retrait_et_changement_a_chaud_sans_saut() {
    let mut s = Sortie::new();
    poser(&s.cle, true, Some(deux_voies(300.0, 0.0, 0.0)));
    s.jouer(0.5);
    poser(&s.cle, true, Some(deux_voies(1000.0, 0.0, 2.0)));
    s.jouer(0.5);
    poser(&s.cle, true, None);
    s.jouer(0.5);
    let saut = s.saut_max(0, 1).max(s.saut_max(1, 1));
    eprintln!("saut_mesure retrait et 3 m -> 10 m : {saut:.5}");
    assert!(saut <= 3.0 * pas_naturel(CADENCE), "saut {saut}");
    let fin = s.trames();
    for n in (fin - 1_000) * 2..fin * 2 {
        assert_eq!(s.sortie[n].to_bits(), s.entree[n].to_bits());
    }
    assert!(!agit(&s.cle, false));
}

#[test]
fn le_reglage_est_propre_a_chaque_sortie() {
    let a = Sortie::new();
    let b = Sortie::new();
    poser(&a.cle, true, Some(deux_voies(300.0, 300.0, -6.0)));
    assert!(agit(&a.cle, false));
    assert!(!agit(&b.cle, false), "l'autre sortie n'a rien");
    assert!(!agit(&a.cle, true), "jamais sur un porteur DoP");
}

/// Le bras natif (ALSA natif, WASAPI, ASIO) : sans réglage, mots bruts
/// (bit-perfect) — et la ligne à retard est tout de même alimentée ; un
/// réglage posé en pleine lecture passe par la branche flottante, sans saut,
/// puis décale la voie au mot près.
#[test]
fn le_bras_natif_reste_bit_perfect_sans_reglage_et_fond_sans_saut() {
    use crate::audio::volume_scale::UNITE_DU_GAIN_DE_RENDU;
    let eq = std::sync::Mutex::new(None);
    let conv = std::sync::Mutex::new(None);
    let cf = std::sync::Mutex::new(None);
    let mono = AtomicBool::new(false);
    let pure = Box::new(AtomicBool::new(false));
    let cadence = 44_100;
    let bloc = 441;
    let total = cadence as usize; // 1 s
    let mot = |n: usize| (sinus(n, cadence) * 32_767.0).round() as i16;
    let mut sortie: Vec<i32> = Vec::new();
    let mut entree: Vec<i32> = Vec::new();
    let mut tous_bruts_avant = true;
    poser(&pure, true, None);
    for debut in (0..total).step_by(bloc) {
        if debut == 13_230 {
            // 0,3 s : la voie droite 100 cm plus près (129 trames).
            poser(&pure, true, Some(deux_voies(300.0, 200.0, 0.0)));
        }
        let mut octets = Vec::new();
        for n in debut..debut + bloc {
            octets.extend_from_slice(&mot(n).to_le_bytes());
            octets.extend_from_slice(&mot(n).to_le_bytes());
            entree.push(i32::from(mot(n)) << 16);
            entree.push(i32::from(mot(n)) << 16);
        }
        let p = super::super::prepare_windows_native_pcm(
            &octets,
            16,
            2,
            2,
            false,
            false,
            UNITE_DU_GAIN_DE_RENDU,
            &eq,
            &conv,
            &cf,
            &pure,
            &mono,
            None,
            cadence,
        )
        .unwrap();
        if debut < 13_230 {
            tous_bruts_avant &= p.bit_perfect;
        }
        sortie.extend(p.samples);
    }
    assert!(tous_bruts_avant, "sans réglage : mots bruts");
    assert_eq!(&sortie[..13_230 * 2], &entree[..13_230 * 2]);
    let pas = pas_naturel(cadence) * 2_147_483_648.0;
    for voie in 0..2 {
        let v: Vec<f64> = sortie
            .iter()
            .skip(voie)
            .step_by(2)
            .map(|x| f64::from(*x))
            .collect();
        let saut = v
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        eprintln!(
            "saut_mesure bras natif voie {voie} : {:.5}",
            saut / 2_147_483_648.0
        );
        assert!(saut <= 3.0 * pas, "voie {voie} : saut {saut}, pas {pas}");
    }
    for n in total - 2_000..total {
        assert_eq!(sortie[n * 2], entree[n * 2], "gauche {n}");
        assert_eq!(sortie[n * 2 + 1], entree[(n - 129) * 2 + 1], "droite {n}");
    }
    oublier(&pure);
}

/// Relecture de #9523 — aucun créneau (coût nul) quand le greffon n'est pas
/// dans la chaîne de la zone ; une nouvelle lecture le retire.
#[test]
fn hors_de_la_chaine_aucun_creneau() {
    let s = Sortie::new();
    poser(&s.cle, false, None);
    assert!(reglages(&s.cle).is_none());
    nouvelle_lecture(&s.cle, true, Some(deux_voies(300.0, 0.0, -6.0)));
    assert!(reglages(&s.cle).is_some());
    nouvelle_lecture(&s.cle, false, None);
    assert!(reglages(&s.cle).is_none(), "retiré à la lecture suivante");
}

/// Relecture de #9523 — une nouvelle lecture (arrêt, seek, autre piste) ou un
/// arrêt vident l'historique : rien de l'écoute précédente n'est rejoué,
/// même au même format, même longtemps après.
#[test]
fn une_nouvelle_lecture_ne_rejoue_rien_de_la_precedente() {
    for vider_par_arret in [false, true] {
        let mut s = Sortie::new();
        nouvelle_lecture(&s.cle, true, Some(deux_voies(1000.0, 0.0, 0.0)));
        s.jouer(0.5);
        if vider_par_arret {
            vider(&s.cle);
        } else {
            nouvelle_lecture(&s.cle, true, Some(deux_voies(1000.0, 0.0, 0.0)));
        }
        // La lecture suivante commence par du silence : la voie retardée ne
        // doit rien rendre de l'ancien sinus.
        let mut silence = vec![0.0_f64; 2 * 2_000];
        traiter(&s.cle, &mut silence, 2, CADENCE, false);
        assert!(
            silence.iter().all(|x| *x == 0.0),
            "son de l'écoute précédente rejoué (arrêt : {vider_par_arret})"
        );
    }
}

/// Niveau (RMS relatif au sinus d'entrée) de la voie `voie`, fenêtre de
/// 20 ms (une période de 50 Hz) commençant à la trame `debut`.
fn niveau(s: &Sortie, voie: usize, debut: usize) -> f64 {
    let n = (CADENCE / 50) as usize;
    let e: f64 = (debut..debut + n)
        .map(|t| s.sortie[t * 2 + voie].powi(2))
        .sum::<f64>()
        / n as f64;
    e.sqrt() / (0.8 / std::f64::consts::SQRT_2)
}

/// rc14 (sécurité auditive, relecture de #9556, M) — ENTRÉE en PURE d'un
/// réglage qui agit (voie 1 à +6 dB, d'où une réserve de −6 dB sur la voie
/// 0 ; voie 1 retardée), volume inchangé : la voie baissée ne remonte pas
/// d'un coup (+6 dB d'un bloc à l'autre avant) ; elle TIENT son niveau le
/// temps du fondu, puis rejoint le sec en ~450 ms, sans marche ; aucune
/// voie ne dépasse son niveau d'avant ; aucun saut d'échantillon ; puis
/// PURE est bit-perfect et le réglage n'agit plus.
#[test]
fn entree_en_pure_fondu_puis_niveau_d_avant_sans_hausse_rc14() {
    let mut s = Sortie::new();
    poser(&s.cle, true, Some(deux_voies(300.0, 0.0, 6.0)));
    s.jouer(0.5);
    let fenetre = (CADENCE / 50) as usize;
    let avant_0 = niveau(&s, 0, s.trames() - fenetre);
    let avant_1 = niveau(&s, 1, s.trames() - fenetre);
    assert!(
        (avant_0 - 0.5).abs() < 0.01,
        "prémisse : voie 0 à {avant_0:.3}"
    );
    assert!(
        (avant_1 - 1.0).abs() < 0.01,
        "prémisse : voie 1 à {avant_1:.3}"
    );
    // Comme `LocalOutput::set_pure_bypass(true)` puis l'orchestrateur.
    entrer_en_pure(&s.cle, 1.0);
    s.cle.store(true, Ordering::Relaxed);
    poser(&s.cle, true, None);
    assert!(
        agit(&s.cle, false),
        "l'entrée en PURE touche encore le signal"
    );
    let entree = s.trames();
    s.jouer(1.2);
    let mut precedent = avant_0;
    for k in 0..50 {
        let debut = entree + k * fenetre;
        let v0 = niveau(&s, 0, debut);
        let v1 = niveau(&s, 1, debut);
        assert!(v1 <= avant_1 * 1.01, "voie 1 à {v1:.3} (fenêtre {k})");
        if k < 15 {
            assert!(
                v0 <= avant_0 * 1.02,
                "voie 0 à {v0:.3} pendant le fondu (fenêtre {k}) : la réserve revient d'un coup"
            );
        }
        let marche = 20.0 * (v0 / precedent).log10();
        assert!(
            marche < 1.0,
            "marche de {marche:.2} dB sur la voie 0 (fenêtre {k})"
        );
        precedent = v0;
    }
    let saut = s.saut_max(0, entree).max(s.saut_max(1, entree));
    assert!(saut <= 3.0 * pas_naturel(CADENCE), "saut {saut}");
    let fin = s.trames();
    for n in (fin - 4_800) * 2..fin * 2 {
        assert_eq!(
            s.sortie[n].to_bits(),
            s.entree[n].to_bits(),
            "PURE : intact"
        );
    }
    assert!(!agit(&s.cle, false));
    assert!(garde_en_pure(&s.cle).is_none());
}

/// rc14 (relecture de #9556, M) — volume MONTÉ depuis le réglage (borne
/// 0,5 : le niveau d'avant le réglage est à −6 dB au volume courant) : la
/// voie baissée reste tenue, le voyant le dit ; une borne relevée la fait
/// remonter jusqu'à elle, pas au-delà ; « Rendre le niveau » rend le sec.
#[test]
fn entree_en_pure_volume_monte_tient_puis_rend_le_niveau_rc14() {
    let mut s = Sortie::new();
    poser(&s.cle, true, Some(deux_voies(300.0, 0.0, 6.0)));
    s.jouer(0.5);
    entrer_en_pure(&s.cle, 0.5);
    s.cle.store(true, Ordering::Relaxed);
    poser(&s.cle, true, None);
    s.jouer(1.0);
    let fenetre = (CADENCE / 50) as usize;
    let v0 = niveau(&s, 0, s.trames() - fenetre);
    assert!((v0 - 0.5).abs() < 0.01, "tenu à {v0:.3}");
    let (db, en_retour) = garde_en_pure(&s.cle).expect("voyant « niveau réduit »");
    assert!((db + 6.02).abs() < 0.1 && !en_retour, "{db:.2} dB");
    borner_en_pure(&s.cle, 0.75);
    let releve = s.trames();
    s.jouer(1.0);
    for k in 0..45 {
        let v = niveau(&s, 0, releve + k * fenetre);
        assert!(v <= 0.75 * 1.01, "au-dessus de la borne : {v:.3}");
    }
    let v0 = niveau(&s, 0, s.trames() - fenetre);
    assert!((v0 - 0.75).abs() < 0.01, "tenu à la borne : {v0:.3}");
    liberer_en_pure(&s.cle);
    s.jouer(1.0);
    let fin = s.trames();
    for n in (fin - 4_800) * 2..fin * 2 {
        assert_eq!(
            s.sortie[n].to_bits(),
            s.entree[n].to_bits(),
            "rendu : intact"
        );
    }
    assert!(!agit(&s.cle, false));
}

/// rc14 (relecture de #9556) — on QUITTE PURE pendant son entrée : le
/// réglage revient par le fondu habituel, sans saut ni dépassement.
#[test]
fn quitter_pure_pendant_l_entree_sans_saut_rc14() {
    let mut s = Sortie::new();
    let r = deux_voies(300.0, 0.0, 6.0);
    poser(&s.cle, true, Some(r.clone()));
    s.jouer(0.5);
    entrer_en_pure(&s.cle, 1.0);
    s.cle.store(true, Ordering::Relaxed);
    poser(&s.cle, true, None);
    s.jouer(0.1);
    s.cle.store(false, Ordering::Relaxed);
    poser(&s.cle, true, Some(r));
    let retour = s.trames();
    s.jouer(0.5);
    let saut = s.saut_max(0, retour).max(s.saut_max(1, retour));
    assert!(saut <= 3.0 * pas_naturel(CADENCE), "saut au retour {saut}");
    let fenetre = (CADENCE / 50) as usize;
    let v0 = niveau(&s, 0, s.trames() - fenetre);
    assert!((v0 - 0.5).abs() < 0.01, "réglage revenu : {v0:.3}");
}
