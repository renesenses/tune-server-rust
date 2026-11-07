//! #6132 — la sortie double sur deux périphériques SIMULÉS, d'horloges
//! différentes, pendant plusieurs minutes.
//!
//! Le banc ne réplique pas le recopieur : il l'appelle tel que le rappel de A
//! l'appelle en production, et simule seulement ce que le matériel fait —
//! deux rappels périodiques, chacun cadencé par son propre quartz, l'anneau
//! SPSC entre les deux, et le rappel de B qui attend sa consigne avant de lire
//! puis comble de zéros quand l'anneau est vide.
//!
//! L'écart est mesuré par ce que chaque périphérique JOUE : à l'instant `t`,
//! A joue la trame source `cadence_A × t − retard`, B joue la trame source
//! portée par l'échantillon qu'il lit à cet instant (retrouvée par le repère
//! `(trame produite, position d'entrée)` que le recopieur publie).

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

struct AnneauSimule {
    file: RefCell<VecDeque<f32>>,
    capacite: usize,
    periode_lue: std::cell::Cell<usize>,
    periode_vue: std::cell::Cell<usize>,
    latence_b: std::cell::Cell<Option<f64>>,
    avance_a: std::cell::Cell<f64>,
    maintenant: std::cell::Cell<f64>,
    derniere_lecture: std::cell::Cell<Option<f64>>,
}

impl AnneauSimule {
    fn nouveau(capacite: usize) -> Self {
        Self {
            file: RefCell::new(VecDeque::with_capacity(capacite)),
            capacite,
            periode_lue: std::cell::Cell::new(0),
            periode_vue: std::cell::Cell::new(0),
            latence_b: std::cell::Cell::new(None),
            avance_a: std::cell::Cell::new(0.0),
            maintenant: std::cell::Cell::new(0.0),
            derniere_lecture: std::cell::Cell::new(None),
        }
    }
}

impl AnneauSecondaire for AnneauSimule {
    fn pousser(&self, echantillons: &[f32]) -> usize {
        let mut f = self.file.borrow_mut();
        let n = echantillons.len().min(self.capacite - f.len());
        f.extend(&echantillons[..n]);
        n
    }
    fn en_attente(&self) -> usize {
        self.file.borrow().len()
    }
    fn periode_du_lecteur(&self) -> usize {
        self.periode_lue.get()
    }
    fn latence_du_lecteur_s(&self) -> Option<f64> {
        self.latence_b.get()
    }
    fn periode_annoncee(&self) -> usize {
        self.periode_vue.get()
    }
    fn depuis_la_derniere_lecture_s(&self) -> Option<f64> {
        self.derniere_lecture
            .get()
            .map(|t| self.maintenant.get() - t)
    }
    fn publier_l_avance_de_a(&self, avance_s: f64) {
        self.avance_a.set(avance_s);
    }
}

/// Un périphérique simulé : cadence nominale, écart de son quartz en ppm,
/// taille de période, latence matérielle (du rappel au convertisseur), et si
/// son pilote la donne dans les horodatages du rappel.
#[derive(Clone, Copy)]
struct Peripherique {
    cadence: u32,
    ppm: f64,
    periode: usize,
    latence_s: f64,
    horodate: bool,
}

impl Peripherique {
    fn latence_annoncee(&self) -> Option<f64> {
        self.horodate.then_some(self.latence_s)
    }
}

impl Peripherique {
    fn cadence_reelle(&self) -> f64 {
        f64::from(self.cadence) * (1.0 + self.ppm * 1e-6)
    }
}

struct Bilan {
    /// Écart A − B en ms, un point par période de B après le préchargement.
    ecarts_ms: Vec<(f64, f64)>,
    /// Trames de B comblées de zéros APRÈS le démarrage de B.
    famines_b: u64,
    releve: Releve,
    /// Le retard commun du groupe à la fin, en ms.
    retard_commun_ms: f64,
}

/// Fait tourner A et B `duree_s` secondes de temps simulé.
fn simuler(a: Peripherique, b: Peripherique, canaux: usize, duree_s: f64) -> Bilan {
    simuler_avec_decalage(a, b, canaux, duree_s, 0.0)
}

/// Même banc, avec le décalage manuel `sync_delay_ms` (B − A) en secondes.
///
/// L'écart est mesuré à la SORTIE des convertisseurs : la trame qu'un rappel
/// livre à `t` s'entend à `t + latence` de son périphérique.
fn simuler_avec_decalage(
    a: Peripherique,
    b: Peripherique,
    canaux: usize,
    duree_s: f64,
    manuel_s: f64,
) -> Bilan {
    simuler_groupe(a, &[(b, manuel_s)], canaux, duree_s)
        .pop()
        .unwrap()
}

/// Le groupe : A et N sorties B, chacune avec son décalage manuel, servies
/// par UNE [`SortieMultiple`] comme en production.
fn simuler_groupe(
    a: Peripherique,
    bs: &[(Peripherique, f64)],
    canaux: usize,
    duree_s: f64,
) -> Vec<Bilan> {
    simuler_scenario(a, bs, canaux, duree_s, &|_| Moment::default()).0
}

/// Ce que le scénario impose à l'instant `t` : la source se tait-elle (pause,
/// blanc entre deux pistes), un réajustement est-il demandé (début de pause,
/// piste suivante), la latence réelle des B change-t-elle.
#[derive(Default, Clone)]
struct Moment {
    silence: bool,
    reajuster: bool,
    latences_b: Option<Vec<f64>>,
}

/// Le retard commun relevé à chaque période de A : `(t, ms)`.
type HistoireDuRetard = Vec<(f64, f64)>;

fn simuler_scenario(
    a: Peripherique,
    bs: &[(Peripherique, f64)],
    canaux: usize,
    duree_s: f64,
    scenario: &dyn Fn(f64) -> Moment,
) -> (Vec<Bilan>, HistoireDuRetard) {
    use std::rc::Rc;
    let mut bs: Vec<(Peripherique, f64)> = bs.to_vec();
    let mut histoire = Vec::new();
    let consigne = CONSIGNE_PAR_DEFAUT_S;
    let capacite = ((consigne + RETARD_MAX_S) * 48_000.0 * 2.0) as usize * canaux;
    let mut membres = Vec::new();
    let mut anneaux = Vec::new();
    let mut consignes = Vec::new();
    for (b, manuel_s) in &bs {
        let mut rec = Recopieur::nouveau(canaux, a.cadence, b.cadence, consigne);
        rec.regler_le_decalage_manuel(*manuel_s);
        consignes.push(rec.consigne());
        let anneau = Rc::new(AnneauSimule::nouveau(capacite));
        anneaux.push(anneau.clone());
        membres.push((rec, anneau));
    }
    let mut sortie = SortieMultiple::nouvelle(canaux, a.cadence, consigne, membres);

    // Repères (trame produite, position d'entrée, pas) de chaque B au début
    // de chaque période de A, pour retrouver la position source d'une trame.
    let n = bs.len();
    let mut reperes: Vec<Vec<(u64, f64, f64)>> = vec![Vec::new(); n];
    let mut periode_a = vec![0f32; a.periode * canaux];
    let ra = a.cadence_reelle();
    let mut na = 0u64;
    let mut nb = vec![0u64; n];
    let mut source = 0u64;
    let mut demarre = vec![false; n];
    let mut sortis = vec![0u64; n];
    let mut famines = vec![0u64; n];
    let mut ecarts: Vec<Vec<(f64, f64)>> = vec![Vec::new(); n];

    loop {
        let ta = (na * a.periode as u64) as f64 / ra;
        // Le prochain rappel de B, tous B confondus.
        let (j, tb) = bs
            .iter()
            .enumerate()
            .map(|(j, (b, _))| (j, (nb[j] * b.periode as u64) as f64 / b.cadence_reelle()))
            .fold((0, f64::INFINITY), |m, x| if x.1 < m.1 { x } else { m });
        if ta.min(tb) > duree_s {
            break;
        }
        if ta <= tb {
            let moment = scenario(ta);
            if let Some(l) = &moment.latences_b {
                for (b, nouvelle) in bs.iter_mut().zip(l) {
                    b.0.latence_s = *nouvelle;
                }
            }
            if moment.reajuster {
                sortie.demander_un_reajustement();
            }
            for f in 0..a.periode {
                let x = if moment.silence {
                    0.0
                } else {
                    ((source + f as u64) as f64 * 0.01).sin() as f32 * 0.5
                };
                for c in 0..canaux {
                    periode_a[f * canaux + c] = x;
                }
            }
            source += a.periode as u64;
            let avant: Vec<(u64, f64)> = sortie.membres().iter().map(|(r, _)| r.repere()).collect();
            for anneau in &anneaux {
                anneau.maintenant.set(ta);
            }
            sortie.traiter(&mut periode_a, a.latence_annoncee());
            for (i, (r, _)) in sortie.membres().iter().enumerate() {
                reperes[i].push((avant[i].0, avant[i].1, 1.0 / r.rapport()));
            }
            histoire.push((ta, sortie.retard_commun_ms()));
            na += 1;
        } else {
            let b = bs[j].0;
            let anneau = &anneaux[j];
            let lu = b.periode * canaux;
            anneau.latence_b.set(b.latence_annoncee());
            anneau.periode_vue.set(lu);
            let compensation = anneau.avance_a.get() - b.latence_annoncee().unwrap_or(0.0);
            if !demarre[j] && anneau.en_attente() >= consignes[j].seuil_de_depart(lu, compensation)
            {
                demarre[j] = true;
            }
            if demarre[j] {
                // La trame que B joue au début de cette période.
                let k = sortis[j];
                let r = &reperes[j];
                let i = r.partition_point(|x| x.0 <= k).saturating_sub(1);
                let (k0, x0, pas) = r[i];
                let pos_b = x0 + (k - k0) as f64 * pas;
                // Ce que A fait entendre à l'instant où cette trame de B sort
                // de son convertisseur.
                let pos_a = ra * (tb + b.latence_s - a.latence_s) - sortie.retard_trames() as f64;
                // Pendant un silence, il n'y a rien à aligner (et les recalages
                // y insèrent ou y sautent des zéros) : on ne mesure que le son.
                if tb > 0.5 && !scenario(tb).silence && !scenario(tb - 0.5).silence {
                    ecarts[j].push((tb, (pos_a - pos_b) / f64::from(a.cadence) * 1000.0));
                }
                anneau.periode_lue.set(lu);
                anneau.derniere_lecture.set(Some(tb));
                let mut f = anneau.file.borrow_mut();
                let pris = lu.min(f.len());
                for _ in 0..pris {
                    f.pop_front();
                }
                sortis[j] += (pris / canaux) as u64;
                famines[j] += ((lu - pris) / canaux) as u64;
            }
            nb[j] += 1;
        }
    }
    let retard_commun_ms = sortie.retard_commun_ms();
    let bilans = sortie
        .membres()
        .iter()
        .enumerate()
        .map(|(j, (r, _))| Bilan {
            ecarts_ms: std::mem::take(&mut ecarts[j]),
            famines_b: famines[j],
            releve: r.releve(),
            retard_commun_ms,
        })
        .collect();
    (bilans, histoire)
}

fn pire_ecart(bilan: &Bilan, apres_s: f64) -> f64 {
    bilan
        .ecarts_ms
        .iter()
        .filter(|(t, _)| *t >= apres_s)
        .map(|(_, e)| e.abs())
        .fold(0.0, f64::max)
}

/// Le témoin principal : 48 kHz des deux côtés, A à +100 ppm, B à −80 ppm
/// (180 ppm d'écart relatif, soit 10,8 ms par minute sans correction), des
/// périodes différentes (10 ms contre 512 trames), dix minutes. Sans
/// correction, l'écart atteindrait 108 ms à la fin.
#[test]
fn deux_horloges_differentes_restent_synchrones_dix_minutes() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 100.0,
        periode: 480,
        latence_s: 0.0,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: -80.0,
        periode: 512,
        latence_s: 0.0,
        horodate: true,
    };
    let bilan = simuler(a, b, 1, 600.0);
    let pire = pire_ecart(&bilan, 20.0);
    let depart = pire_ecart(&bilan, 0.0);
    let dernier = bilan.ecarts_ms.last().unwrap().1;
    eprintln!(
        "sortie_double 48k/48k +100/-80 ppm, 600 s : écart au départ ≤ {depart:.3} ms, \
         pire écart de 20 s à 600 s = {pire:.3} ms, écart final = {dernier:.3} ms, \
         dérive estimée = {:.1} ppm, famines B = {}",
        bilan.releve.derive_ppm, bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "écart A/B non borné : {pire:.3} ms entre 20 s et 10 min (attendu < 0,5 ms)"
    );
    // Au départ, B ne sait pas où A en est dans sa période : l'écart initial
    // tient dans une période de A (10 ms) ; l'accrochage le résorbe.
    assert!(
        depart < 10.0 + 1.0,
        "écart A/B au départ : {depart:.3} ms (attendu < une période de A + 1 ms)"
    );
    assert_eq!(
        bilan.famines_b, 0,
        "B a manqué d'échantillons après son départ"
    );
    // La correction a retrouvé la dérive : B consomme 180 ppm moins vite.
    assert!(
        (bilan.releve.derive_ppm + 180.0).abs() < 5.0,
        "dérive estimée {:.1} ppm, attendu ≈ −180 ppm",
        bilan.releve.derive_ppm
    );
}

/// Cadences nominales différentes (44,1 kHz vers 48 kHz), dérive forte
/// (±250 ppm), stéréo : le même régulateur tient.
#[test]
fn cadences_differentes_et_derive_forte_en_stereo() {
    let a = Peripherique {
        cadence: 44_100,
        ppm: -250.0,
        periode: 441,
        latence_s: 0.0,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 250.0,
        periode: 1024,
        latence_s: 0.0,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 240.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double 44,1k/48k -250/+250 ppm stéréo, 240 s : pire écart de 20 s à 240 s = {pire:.3} ms, \
         pire total = {:.3} ms, dérive estimée = {:.1} ppm, famines B = {}",
        pire_ecart(&bilan, 0.0),
        bilan.releve.derive_ppm,
        bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "écart A/B non borné : {pire:.3} ms (attendu < 0,5 ms)"
    );
    assert!(
        (bilan.releve.derive_ppm - 500.0).abs() < 5.0,
        "dérive estimée {:.1} ppm, attendu ≈ +500 ppm",
        bilan.releve.derive_ppm
    );
    assert_eq!(
        bilan.famines_b, 0,
        "B a manqué d'échantillons après son départ"
    );
}

/// Le rééchantillonneur rend le signal : un sinus de 1 kHz passé de 48 kHz à
/// 48 048 Hz (+1 000 ppm), correction en mouvement, se retrouve à la position
/// d'entrée annoncée par le repère, à −80 dB près. C'est ce qui rend le repère du banc ci-dessus
/// digne de confiance : il dit où est vraiment le signal.
#[test]
fn le_signal_recopie_est_a_la_position_annoncee() {
    let canaux = 2;
    let mut rec = Recopieur::nouveau(canaux, 48_000, 48_048, 0.0);
    let anneau = AnneauSimule::nouveau(1 << 22);
    let w = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
    let mut periode = vec![0f32; 480 * canaux];
    let mut source = 0u64;
    let mut reperes = Vec::new();
    for _ in 0..200 {
        for f in 0..480 {
            let x = (w * (source + f as u64) as f64).sin() as f32;
            periode[f * canaux] = x;
            periode[f * canaux + 1] = -x;
        }
        source += 480;
        let (k, x) = rec.repere();
        rec.recopier(&mut periode, &anneau, None);
        reperes.push((k, x, 1.0 / rec.rapport()));
    }
    // Consigne nulle : le régulateur voit un anneau qui se remplit et pousse
    // la correction à sa borne — le pas change d'une période à l'autre, et
    // chaque trame est jugée au pas de SA période.
    let f = anneau.file.borrow();
    let produites = f.len() / canaux;
    let mut pire = 0f64;
    for k in 64..produites as u64 - 64 {
        let i = reperes.partition_point(|r: &(u64, f64, f64)| r.0 <= k) - 1;
        let (k0, x0, pas) = reperes[i];
        let x = x0 + (k - k0) as f64 * pas;
        let attendu = (w * x).sin();
        let g = f[(k as usize) * canaux] as f64;
        let d = f[(k as usize) * canaux + 1] as f64;
        pire = pire.max((g - attendu).abs()).max((d + attendu).abs());
    }
    eprintln!("sortie_double : erreur max du signal recopié = {pire:.2e}");
    assert!(
        pire < 1e-4,
        "signal recopié décalé ou déformé : erreur {pire:.2e}"
    );
}

/// Ce que A joue est la période d'entrée retardée de la consigne, échantillon
/// pour échantillon.
#[test]
fn la_ligne_a_retard_de_a_est_exacte() {
    let mut rec = Recopieur::nouveau(2, 48_000, 48_000, 0.001); // 48 trames
    let anneau = AnneauSimule::nouveau(1 << 16);
    let mut joue = Vec::new();
    for p in 0..4 {
        let mut periode: Vec<f32> = (0..64).map(|i| (p * 64 + i + 1) as f32).collect();
        rec.recopier(&mut periode, &anneau, None);
        joue.extend(periode);
    }
    assert!(joue[..96].iter().all(|s| *s == 0.0));
    let attendu: Vec<f32> = (1..=(256 - 96)).map(|i| i as f32).collect();
    assert_eq!(&joue[96..], &attendu[..]);
}

#[test]
fn la_valeur_du_reglage_donne_le_nom_du_peripherique() {
    assert_eq!(
        nom_de_peripherique("local:SMSL SU-8"),
        Some("SMSL SU-8".into())
    );
    assert_eq!(
        nom_de_peripherique(" TASCAM US-366 "),
        Some("TASCAM US-366".into())
    );
    assert_eq!(nom_de_peripherique("local:"), None);
    assert_eq!(nom_de_peripherique(""), None);
    assert_eq!(cle_de_zone(7), "zone_7_sortie_double");
}

/// #6132 étape 2 — deux cartes de latences matérielles différentes (A : 5 ms,
/// B : 25 ms), horodatées par leur pilote : la compensation automatique
/// nourrit B 20 ms plus tôt, et ce qui sort des deux convertisseurs reste
/// aligné. Sans elle, B s'entendrait 20 ms après A pendant toute l'écoute.
#[test]
fn la_latence_de_chaque_carte_est_compensee_par_les_horodatages() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 60.0,
        periode: 480,
        latence_s: 0.005,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: -40.0,
        periode: 512,
        latence_s: 0.025,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 180.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double latences 5/25 ms horodatées, 180 s : pire écart de 20 s à 180 s = {pire:.3} ms, \
         compensation = {:.3} ms, latences lues A = {:?} ms, B = {:?} ms, famines B = {}",
        bilan.releve.compensation_ms,
        bilan.releve.latence_a_ms,
        bilan.releve.latence_b_ms,
        bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "latences matérielles non compensées : écart entendu {pire:.3} ms (attendu < 0,5 ms)"
    );
    assert!((bilan.releve.compensation_ms + 20.0).abs() < 0.1);
    assert_eq!(bilan.famines_b, 0);
}

/// Le réglage de secours : pilote sans horodatage, mêmes latences. Le
/// décalage manuel `sync_delay_ms` (B − A = −20 ms : A retardé de 20 ms) fait
/// le même travail.
#[test]
fn sans_horodatage_le_decalage_manuel_compense() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 0.0,
        periode: 480,
        latence_s: 0.005,
        horodate: false,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 100.0,
        periode: 480,
        latence_s: 0.025,
        horodate: false,
    };
    let sans = simuler_avec_decalage(a, b, 1, 60.0, 0.0);
    let avec = simuler_avec_decalage(a, b, 1, 60.0, -0.020);
    let (e_sans, e_avec) = (pire_ecart(&sans, 20.0), pire_ecart(&avec, 20.0));
    eprintln!(
        "sortie_double sans horodatage, 60 s : écart sans réglage = {e_sans:.3} ms, \
         avec sync_delay −20 ms = {e_avec:.3} ms"
    );
    assert!(
        e_sans > 19.0,
        "le banc doit voir l'écart matériel : {e_sans:.3} ms"
    );
    assert!(
        e_avec < 0.5,
        "le décalage manuel ne compense pas : {e_avec:.3} ms"
    );
    assert_eq!(avec.releve.latence_a_ms, None);
}

/// Deux cartes aux périodes ÉGALES (10 ms, le cas WASAPI partagé courant) et
/// aux quartz presque égaux (+10 ppm) : leurs rappels gardent la même phase
/// pendant des minutes. Mesuré à l'instant où A passe, le remplissage de B
/// dépendait de cette phase, et l'écart se calait jusqu'à 5 ms à côté.
#[test]
fn periodes_egales_et_phase_figee_restent_alignees() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 0.0,
        periode: 480,
        latence_s: 0.010,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 10.0,
        periode: 480,
        latence_s: 0.010,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 300.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double périodes égales, +10 ppm, 300 s : pire écart de 20 s à 300 s = {pire:.3} ms"
    );
    assert!(
        pire < 0.5,
        "écart calé sur la phase des rappels : {pire:.3} ms"
    );
}

/// #6132 — trois cartes : A et deux suiveuses, chacune avec son quartz, sa
/// cadence, sa période et sa latence. Chacune est corrigée de SA dérive par
/// son propre recopieur, et toutes sortent alignées sur A.
#[test]
fn trois_cartes_chacune_corrigee_de_sa_derive() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 50.0,
        periode: 480,
        latence_s: 0.005,
        horodate: true,
    };
    let b1 = Peripherique {
        cadence: 48_000,
        ppm: -100.0,
        periode: 512,
        latence_s: 0.008,
        horodate: true,
    };
    let b2 = Peripherique {
        cadence: 44_100,
        ppm: 200.0,
        periode: 441,
        latence_s: 0.012,
        horodate: true,
    };
    let bilans = simuler_groupe(a, &[(b1, 0.0), (b2, 0.0)], 1, 300.0);
    for (nom, bilan, derive) in [("B1", &bilans[0], -150.0), ("B2", &bilans[1], 150.0)] {
        let pire = pire_ecart(bilan, 20.0);
        eprintln!(
            "sortie_double trois cartes, {nom}, 300 s : pire écart de 20 s à 300 s = {pire:.3} ms, \
             dérive estimée = {:.1} ppm (vraie {derive}), retard commun = {:.1} ms, famines = {}",
            bilan.releve.derive_ppm, bilan.retard_commun_ms, bilan.famines_b
        );
        assert!(
            pire < 0.5,
            "{nom} non aligné sur A : {pire:.3} ms (attendu < 0,5 ms)"
        );
        assert!(
            (bilan.releve.derive_ppm - derive).abs() < 5.0,
            "{nom} : dérive estimée {:.1} ppm, vraie {derive} ppm",
            bilan.releve.derive_ppm
        );
        assert_eq!(bilan.famines_b, 0, "{nom} a manqué d'échantillons");
    }
}

/// #6132 — une carte bien plus lente que les autres (70 ms de latence contre
/// 5 et 10) : la consigne de 40 ms ne suffit pas à la nourrir assez tôt. Le
/// retard commun s'adapte AUTOMATIQUEMENT à elle, et les trois restent
/// alignées.
#[test]
fn le_retard_commun_s_adapte_a_la_carte_la_plus_lente() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 0.0,
        periode: 480,
        latence_s: 0.005,
        horodate: true,
    };
    let rapide = Peripherique {
        cadence: 48_000,
        ppm: 80.0,
        periode: 480,
        latence_s: 0.010,
        horodate: true,
    };
    let lente = Peripherique {
        cadence: 48_000,
        ppm: -60.0,
        periode: 1024,
        latence_s: 0.070,
        horodate: true,
    };
    let bilans = simuler_groupe(a, &[(rapide, 0.0), (lente, 0.0)], 1, 120.0);
    let retard = bilans[0].retard_commun_ms;
    for (nom, bilan) in [("rapide", &bilans[0]), ("lente", &bilans[1])] {
        let pire = pire_ecart(bilan, 20.0);
        eprintln!(
            "sortie_double carte lente (latences 5/10/70 ms), {nom}, 120 s : pire écart = {pire:.3} ms, \
             retard commun = {retard:.1} ms, famines = {}",
            bilan.famines_b
        );
        assert!(
            pire < 0.5,
            "{nom} non alignée : {pire:.3} ms — le retard commun ({retard:.1} ms) ne couvre pas la carte la plus lente"
        );
        assert_eq!(bilan.famines_b, 0, "{nom} a manqué d'échantillons");
    }
    assert!(
        retard > 80.0 && retard < 120.0,
        "retard commun {retard:.1} ms : attendu ≈ 65 ms d'écart de latence + la période de la lente + marges"
    );
}

#[test]
fn le_reglage_lit_une_ou_plusieurs_sorties() {
    assert_eq!(
        lire_la_valeur(r#"["local:SMSL SU-8","local:TASCAM US-366"]"#),
        vec!["SMSL SU-8".to_string(), "TASCAM US-366".to_string()]
    );
    assert_eq!(
        lire_la_valeur("local:SMSL SU-8"),
        vec!["SMSL SU-8".to_string()]
    );
    assert!(lire_la_valeur("[]").is_empty());
    assert_eq!(
        lire_la_valeur(&ecrire_la_valeur(&["local:A, B".to_string()])),
        vec!["A, B".to_string()]
    );
}

/// #6132 — le retard commun ne bouge JAMAIS pendant le son. Une carte voit
/// sa latence passer de 10 à 75 ms en pleine lecture : le retard reste figé.
/// À la pause, il est recalculé pour elle, et les trois cartes repartent
/// alignées. Puis sa latence redescend : le retard reste figé — y compris à
/// un enchaînement sans blanc demandé en plein son — jusqu'à la piste
/// suivante (un blanc de 300 ms), où il revient à la consigne.
#[test]
fn le_retard_commun_se_reajuste_a_la_pause_et_a_la_piste_suivante() {
    let carte = |ppm: f64, latence_s: f64| Peripherique {
        cadence: 48_000,
        ppm,
        periode: 512,
        latence_s,
        horodate: true,
    };
    let a = Peripherique {
        periode: 480,
        ..carte(0.0, 0.005)
    };
    let scenario = |t: f64| Moment {
        silence: (40.0..42.0).contains(&t) || (100.0..100.3).contains(&t),
        // À 90 s, une piste suivante sans blanc (gapless) : la demande doit
        // attendre le silence de 100 s, pas s'appliquer pendant le son.
        reajuster: (40.0..40.02).contains(&t)
            || (90.0..90.02).contains(&t)
            || (100.0..100.02).contains(&t),
        latences_b: Some(vec![
            0.010,
            if (20.0..80.0).contains(&t) {
                0.075
            } else {
                0.010
            },
        ]),
    };
    let (bilans, histoire) = simuler_scenario(
        a,
        &[(carte(70.0, 0.010), 0.0), (carte(-90.0, 0.010), 0.0)],
        1,
        130.0,
        &scenario,
    );
    let retard_entre = |de: f64, a: f64| -> (f64, f64) {
        histoire
            .iter()
            .filter(|(t, _)| *t >= de && *t < a)
            .fold((f64::INFINITY, 0.0f64), |(mn, mx), (_, r)| {
                (mn.min(*r), mx.max(*r))
            })
    };
    let (avant_min, avant_max) = retard_entre(1.0, 40.0);
    let (lente_min, lente_max) = retard_entre(42.0, 100.0);
    let (apres_min, apres_max) = retard_entre(100.3, 130.0);
    eprintln!(
        "sortie_double réajustement : retard {avant_min:.1}–{avant_max:.1} ms jusqu'à la pause \
         (latence passée à 75 ms à 20 s), {lente_min:.1}–{lente_max:.1} ms après la pause, \
         {apres_min:.1}–{apres_max:.1} ms après la piste suivante"
    );
    // Pendant le son, jamais de changement : ni à 20 s, ni à 80 s.
    assert_eq!(
        (avant_min, avant_max),
        (40.0, 40.0),
        "le retard commun a bougé pendant le son avant la pause"
    );
    assert_eq!(
        lente_min, lente_max,
        "le retard commun a bougé pendant le son"
    );
    assert!(
        lente_min > 80.0,
        "à la pause, le retard commun n'a pas été recalculé pour la carte lente : {lente_min:.1} ms"
    );
    assert_eq!(
        (apres_min, apres_max),
        (40.0, 40.0),
        "à la piste suivante, le retard commun n'est pas revenu à la consigne"
    );
    for (nom, bilan) in [("B1", &bilans[0]), ("B2", &bilans[1])] {
        let ecart_entre = |de: f64, a: f64| {
            bilan
                .ecarts_ms
                .iter()
                .filter(|(t, _)| *t >= de && *t < a)
                .map(|(_, e)| e.abs())
                .fold(0.0, f64::max)
        };
        let (apres_pause, apres_piste) = (ecart_entre(45.0, 80.0), ecart_entre(105.0, 130.0));
        eprintln!(
            "sortie_double réajustement, {nom} : écart après la pause = {apres_pause:.3} ms, \
             après la piste suivante = {apres_piste:.3} ms"
        );
        assert!(
            apres_pause < 0.5 && apres_piste < 0.5,
            "{nom} non aligné après réajustement : {apres_pause:.3} / {apres_piste:.3} ms"
        );
    }
}
