//! #2211 — **la superposition se prouve sur les ÉCHANTILLONS**, pas sur une
//! courbe de gain.
//!
//! Le défaut de #2211 n'est pas un réglage : c'est qu'il n'y a jamais eu deux
//! pistes décodées en même temps. `CrossfadeHandler` (retiré en v0.9.146)
//! baissait le volume de la sortie à zéro **puis** remontait celui de la
//! suivante. Un témoin qui vérifie des gains ne distingue pas les deux
//! implémentations : les deux ont une rampe descendante et une rampe
//! montante. Ce qui les distingue est ailleurs — dans le **flux de sortie**,
//! où l'un porte deux sources dans le même mot et l'autre jamais.
//!
//! Ces témoins mesurent donc le PCM livré au puits, et rien d'autre :
//!
//! * [`deux_tons_coexistent_dans_le_recouvrement`] — deux sinus de fréquences
//!   distinctes, une analyse de Goertzel sur trois fenêtres. Avant : le ton
//!   sortant seul. Pendant : **les deux**. Après : le ton entrant seul ;
//! * [`la_contre_epreuve_sequentielle_ne_superpose_rien`] — **la
//!   contre-épreuve**. Le même banc, le même signal, la même courbe, mais le
//!   mécanisme de #2211 : fondu descendant complet, PUIS fondu montant. Le
//!   témoin précédent devient ROUGE — jamais les deux tons ensemble, et un
//!   creux de niveau au milieu. C'est ce qui établit que le premier témoin
//!   mesure bien la superposition et pas la présence de rampes ;
//! * [`deux_decodages_reels_simultanes_se_melangent_dans_le_meme_mot`] —
//!   deux fichiers du dépôt, décodés par le vrai décodeur, dans **deux fils
//!   qui tournent en même temps** (le témoin le vérifie sur les horloges),
//!   branchés sur les deux voies d'un [`AtelierDeFondu`]. Chaque mot de la
//!   zone de recouvrement est comparé à `a·g_sortant + b·g_entrant` et aux
//!   deux sources prises seules ;
//! * [`la_continuite_et_la_duree_sont_exactes`] — la durée du recouvrement au
//!   nombre de trames près, et l'absence de marche aux deux bornes ;
//! * [`le_fondu_ne_touche_a_aucun_volume`] — garde de texte sur le module.
//!
//! Aucun réseau, aucune base, aucun périphérique : le puits est le
//! [`CaptureOutput`] de la caisse de contrat, celui de #2218.
//!
//! ⚠️ `autotests = false` dans `tune-core/Cargo.toml` : ce fichier n'existe
//! pour cargo que par sa cible `[[test]]`. Sans elle il ne serait jamais
//! compilé, et son vert ne voudrait rien dire.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tune_core::audio::fondu_enchaine::{AtelierDeFondu, CourbeDeFondu, FonduEnchaine};
use tune_core::outputs::traits::{CaptureOutput, FormatOuvert, PuitsDEchantillons};

const CADENCE: u32 = 48_000;
const CANAUX: u16 = 2;

// ───────────────────────────────────────────────────────────────────────────
// Outillage
// ───────────────────────────────────────────────────────────────────────────

/// Le puits de capture, atteignable après coup **et** depuis deux fils.
///
/// [`CaptureOutput`] est le puits ; ce type ne fait que le partager. Sans lui
/// il faudrait le donner à l'atelier et ne jamais le revoir, ou recopier son
/// hachage — deux façons de mesurer autre chose que ce qui est livré.
#[derive(Clone)]
struct PuitsPartage(Arc<Mutex<CaptureOutput>>);

impl PuitsPartage {
    fn nouveau(format: FormatOuvert, plafond: usize) -> Self {
        Self(Arc::new(Mutex::new(CaptureOutput::avec_retenue(
            format, plafond,
        ))))
    }

    /// Les mots livrés, dans l'ordre. Panique si la retenue a débordé : une
    /// retenue tronquée comparerait une référence entière à un tronçon.
    fn mots(&self) -> Vec<f32> {
        let capture = self.0.lock().expect("puits non empoisonné");
        assert!(
            capture.retenue_complete(),
            "la retenue du puits a débordé : le plafond du témoin est trop bas, \
             et comparer un tronçon à une référence entière est un faux vert"
        );
        capture.mots_livres().expect("le puits retient").to_vec()
    }
}

impl PuitsDEchantillons for PuitsPartage {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        match self.0.lock() {
            Ok(mut capture) => capture.ecrire(mots),
            Err(_) => false,
        }
    }
}

/// Énergie du canal 0 à `frequence`, par Goertzel, normalisée par la longueur.
///
/// Pour un sinus pur d'amplitude `A` tombant exactement sur un casier, la
/// valeur rendue vaut `A²/4`. Choisir des fréquences multiples de
/// `cadence / n` évite l'étalement spectral qui rendrait le seuil arbitraire.
fn energie(mots: &[f32], canaux: usize, cadence: f64, frequence: f64) -> f64 {
    let x: Vec<f64> = mots.iter().step_by(canaux).map(|&v| f64::from(v)).collect();
    let n = x.len();
    assert!(n > 1, "fenêtre d'analyse vide");
    let k = (0.5 + n as f64 * frequence / cadence).floor();
    let omega = 2.0 * std::f64::consts::PI * k / n as f64;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in &x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let puissance = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    puissance / (n as f64 * n as f64)
}

/// `trames` trames stéréo d'un sinus de `frequence`, amplitude `amplitude`.
fn ton(frequence: f64, trames: usize, amplitude: f32) -> Vec<f32> {
    let mut mots = Vec::with_capacity(trames * CANAUX as usize);
    for i in 0..trames {
        let phase = 2.0 * std::f64::consts::PI * frequence * i as f64 / f64::from(CADENCE);
        let v = amplitude * phase.sin() as f32;
        for _ in 0..CANAUX {
            mots.push(v);
        }
    }
    mots
}

/// Une tranche de trames `[debut, fin)` d'un flux entrelacé.
fn tranche(mots: &[f32], debut: usize, fin: usize) -> &[f32] {
    let c = CANAUX as usize;
    &mots[debut * c..fin * c]
}

fn format() -> FormatOuvert {
    FormatOuvert::new(CADENCE, CANAUX)
}

// ───────────────────────────────────────────────────────────────────────────
// Le témoin central : deux sources dans le même mot
// ───────────────────────────────────────────────────────────────────────────

/// Trames de recouvrement : 0,1 s. `1000` et `5000` Hz tombent tous deux sur
/// un casier exact pour les fenêtres d'analyse employées ici.
const RECOUVREMENT: usize = 4_800;
const SORTANT_HZ: f64 = 1_000.0;
const ENTRANT_HZ: f64 = 5_000.0;
/// Trames par piste : 0,3 s.
const TRAMES_PISTE: usize = 14_400;

/// Là où commence et finit le recouvrement dans le flux LIVRÉ.
const DEBUT_RECOUVREMENT: usize = TRAMES_PISTE - RECOUVREMENT;
const FIN_RECOUVREMENT: usize = TRAMES_PISTE;

/// Une fenêtre de 1 200 trames au milieu du recouvrement. `1000` et `5000`
/// sont multiples de `48000 / 1200 = 40` Hz : casiers exacts.
const FENETRE: usize = 1_200;

fn joue_le_fondu(courbe: CourbeDeFondu) -> Vec<f32> {
    let puits = PuitsPartage::nouveau(format(), 4 * TRAMES_PISTE * CANAUX as usize);
    let moteur =
        FonduEnchaine::nouveau(format(), RECOUVREMENT, courbe).expect("recouvrement non nul");
    let atelier = AtelierDeFondu::nouveau(moteur, Box::new(puits.clone()));

    let mut voie_sortante = atelier.voie_sortante();
    let mut voie_entrante = atelier.voie_entrante();

    // La sortante joue jusqu'au bout ; l'entrante a déjà décodé sa tête.
    assert!(voie_sortante.ecrire(&ton(SORTANT_HZ, TRAMES_PISTE, 0.5)));
    assert!(voie_entrante.ecrire(&ton(ENTRANT_HZ, TRAMES_PISTE, 0.5)));
    atelier.fin_de_la_sortante();
    atelier.vider();

    assert_eq!(
        atelier.trames_melangees(),
        RECOUVREMENT,
        "le recouvrement doit faire exactement les trames demandées"
    );
    puits.mots()
}

#[test]
fn deux_tons_coexistent_dans_le_recouvrement() {
    let livre = joue_le_fondu(CourbeDeFondu::PuissanceConstante);
    let c = CANAUX as usize;

    // Le flux fait exactement deux pistes moins le recouvrement.
    assert_eq!(
        livre.len(),
        (2 * TRAMES_PISTE - RECOUVREMENT) * c,
        "durée totale : deux pistes moins la zone superposée, à la trame près"
    );

    // Référence : la même fenêtre dans la zone où le ton sortant est SEUL.
    let avant = tranche(&livre, 4_800, 4_800 + FENETRE);
    let e_sortant_pur = energie(avant, c, f64::from(CADENCE), SORTANT_HZ);
    let e_entrant_avant = energie(avant, c, f64::from(CADENCE), ENTRANT_HZ);
    assert!(
        e_entrant_avant < e_sortant_pur / 1_000.0,
        "avant le fondu, le ton entrant ne doit pas exister : \
         sortant={e_sortant_pur:.3e} entrant={e_entrant_avant:.3e}"
    );

    // Après : le ton entrant seul.
    let apres = tranche(
        &livre,
        FIN_RECOUVREMENT + 2_400,
        FIN_RECOUVREMENT + 2_400 + FENETRE,
    );
    let e_entrant_pur = energie(apres, c, f64::from(CADENCE), ENTRANT_HZ);
    let e_sortant_apres = energie(apres, c, f64::from(CADENCE), SORTANT_HZ);
    assert!(
        e_sortant_apres < e_entrant_pur / 1_000.0,
        "après le fondu, le ton sortant ne doit plus exister : \
         entrant={e_entrant_pur:.3e} sortant={e_sortant_apres:.3e}"
    );

    // ── LE point du ticket : au milieu du recouvrement, les DEUX. ──
    let milieu_debut = DEBUT_RECOUVREMENT + RECOUVREMENT / 2 - FENETRE / 2;
    let milieu = tranche(&livre, milieu_debut, milieu_debut + FENETRE);
    let e_sortant = energie(milieu, c, f64::from(CADENCE), SORTANT_HZ);
    let e_entrant = energie(milieu, c, f64::from(CADENCE), ENTRANT_HZ);

    // À mi-course, la puissance constante pose les deux gains à √2/2 : chaque
    // ton doit rendre au moins le quart de son énergie pleine. Le seuil est
    // large exprès — c'est une PRÉSENCE qu'on mesure, pas un gain.
    assert!(
        e_sortant > e_sortant_pur / 4.0,
        "le ton SORTANT a disparu du recouvrement : {e_sortant:.3e} contre \
         {e_sortant_pur:.3e} en zone pure. C'est le défaut de #2211 : la piste \
         courante est éteinte avant que la suivante ne monte."
    );
    assert!(
        e_entrant > e_entrant_pur / 4.0,
        "le ton ENTRANT est absent du recouvrement : {e_entrant:.3e} contre \
         {e_entrant_pur:.3e} en zone pure. Les deux pistes ne sont pas \
         superposées."
    );
}

// ───────────────────────────────────────────────────────────────────────────
// La contre-épreuve
// ───────────────────────────────────────────────────────────────────────────

/// Le mécanisme de #2211, reconstitué : deux fondus SÉQUENTIELS.
///
/// Même courbe, même durée, mêmes signaux — mais la sortante s'éteint
/// entièrement avant que l'entrante ne monte, exactement comme
/// `CrossfadeHandler` le faisait avec le volume de la sortie. Le flux produit
/// ici est celui que l'ancien code aurait donné, et il doit faire ÉCHOUER la
/// mesure du témoin précédent.
fn flux_sequentiel(courbe: CourbeDeFondu) -> Vec<f32> {
    let c = CANAUX as usize;
    let sortant = ton(SORTANT_HZ, TRAMES_PISTE, 0.5);
    let entrant = ton(ENTRANT_HZ, TRAMES_PISTE, 0.5);
    let mut flux = Vec::with_capacity((2 * TRAMES_PISTE) * c);

    // La sortante, intacte puis éteinte sur RECOUVREMENT trames.
    flux.extend_from_slice(&sortant[..DEBUT_RECOUVREMENT * c]);
    for i in 0..RECOUVREMENT {
        let t = i as f32 / (RECOUVREMENT - 1) as f32;
        let (g, _) = courbe.gains(t);
        for canal in 0..c {
            flux.push(sortant[(DEBUT_RECOUVREMENT + i) * c + canal] * g);
        }
    }
    // PUIS l'entrante, montée sur RECOUVREMENT trames, puis intacte.
    for i in 0..RECOUVREMENT {
        let t = i as f32 / (RECOUVREMENT - 1) as f32;
        let (_, g) = courbe.gains(t);
        for canal in 0..c {
            flux.push(entrant[i * c + canal] * g);
        }
    }
    flux.extend_from_slice(&entrant[RECOUVREMENT * c..]);
    flux
}

#[test]
fn la_contre_epreuve_sequentielle_ne_superpose_rien() {
    let courbe = CourbeDeFondu::PuissanceConstante;
    let livre = flux_sequentiel(courbe);
    let c = CANAUX as usize;

    let avant = tranche(&livre, 4_800, 4_800 + FENETRE);
    let e_sortant_pur = energie(avant, c, f64::from(CADENCE), SORTANT_HZ);
    let e_entrant_pur = e_sortant_pur; // même amplitude, même fenêtre

    // La transition séquentielle occupe 2 × RECOUVREMENT trames. On balaie
    // TOUTE la transition : nulle part les deux tons ne coexistent.
    let debut = DEBUT_RECOUVREMENT;
    let fin = DEBUT_RECOUVREMENT + 2 * RECOUVREMENT - FENETRE;
    let mut coexistences = 0usize;
    let mut creux = false;
    let mut pas = debut;
    while pas <= fin {
        let f = tranche(&livre, pas, pas + FENETRE);
        let e_s = energie(f, c, f64::from(CADENCE), SORTANT_HZ);
        let e_e = energie(f, c, f64::from(CADENCE), ENTRANT_HZ);
        if e_s > e_sortant_pur / 4.0 && e_e > e_entrant_pur / 4.0 {
            coexistences += 1;
        }
        if e_s + e_e < (e_sortant_pur + e_entrant_pur) / 20.0 {
            creux = true;
        }
        pas += FENETRE / 4;
    }

    assert_eq!(
        coexistences, 0,
        "la contre-épreuve superpose quelque chose : le flux séquentiel de \
         #2211 ne doit JAMAIS porter les deux tons ensemble. Si ce compte \
         n'est pas nul, la mesure de `deux_tons_coexistent_dans_le_recouvrement` \
         ne distingue pas les deux implémentations et ne prouve rien."
    );
    assert!(
        creux,
        "la contre-épreuve devrait montrer le CREUX de #2211 — l'instant où \
         la sortante est à zéro et l'entrante n'a pas encore monté. Ne pas le \
         trouver voudrait dire que le témoin lui-même ne mesure rien (#2082)."
    );

    // Et le vrai fondu, lui, n'a pas ce creux : le même balayage, sur le flux
    // superposé, ne trouve aucune fenêtre effondrée.
    let vrai = joue_le_fondu(courbe);
    let mut creux_vrai = false;
    let mut pas = DEBUT_RECOUVREMENT;
    while pas <= FIN_RECOUVREMENT - FENETRE {
        let f = tranche(&vrai, pas, pas + FENETRE);
        let e_s = energie(f, c, f64::from(CADENCE), SORTANT_HZ);
        let e_e = energie(f, c, f64::from(CADENCE), ENTRANT_HZ);
        if e_s + e_e < (e_sortant_pur + e_entrant_pur) / 20.0 {
            creux_vrai = true;
        }
        pas += FENETRE / 4;
    }
    assert!(
        !creux_vrai,
        "le fondu superposé ne doit avoir AUCUN creux : c'est le défaut que \
         #2211 décrit et que la superposition supprime"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Deux décodages RÉELS, simultanés
// ───────────────────────────────────────────────────────────────────────────

fn fixture(nom: &str) -> String {
    let mut chemin = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    chemin.push("tests");
    chemin.push("fixtures");
    chemin.push(nom);
    chemin.to_string_lossy().into_owned()
}

/// Décode un fichier du dépôt en `f32` entrelacés à la cadence de sortie.
///
/// Rend aussi l'intervalle de temps du décodage : c'est lui qui prouve que
/// les deux décodages se recouvrent, et donc qu'il y a bien eu **deux
/// producteurs en même temps** — ce qui n'a jamais existé dans la chaîne
/// locale, où la piste N+1 n'est pas même ouverte avant l'EOF de N.
fn decoder(nom: &str, secondes: f64) -> (Vec<f32>, Instant, Instant) {
    let debut = Instant::now();
    let audio = tune_core::audio::decode::decode_to_pcm(
        &fixture(nom),
        Some(CADENCE),
        Some(u32::from(CANAUX)),
        0.0,
        secondes,
    )
    .unwrap_or_else(|e| panic!("décodage de {nom} : {e}"));
    let echelle = match audio.bit_depth {
        16 => 1.0 / 32_768.0,
        24 => 1.0 / 8_388_608.0,
        _ => 1.0 / 2_147_483_648.0,
    };
    let mots: Vec<f32> = audio
        .samples_i32
        .iter()
        .map(|&s| (f64::from(s) * echelle) as f32)
        .collect();
    let fin = Instant::now();
    (mots, debut, fin)
}

#[test]
fn deux_decodages_reels_simultanes_se_melangent_dans_le_meme_mot() {
    let c = CANAUX as usize;
    let courbe = CourbeDeFondu::PuissanceConstante;

    // Ce que les deux fils vont décoder, mesuré ici aussi pour disposer de la
    // référence exacte des deux sources.
    let (attendu_a, _, _) = decoder("test.wav", 1.0);
    let (attendu_b, _, _) = decoder("test.flac", 1.0);
    let trames_a = attendu_a.len() / c;
    let trames_b = attendu_b.len() / c;
    assert!(
        trames_a > RECOUVREMENT && trames_b > RECOUVREMENT,
        "les deux fixtures doivent dépasser le recouvrement : {trames_a} et {trames_b} trames"
    );

    let puits = PuitsPartage::nouveau(format(), 4 * (attendu_a.len() + attendu_b.len()));
    let moteur =
        FonduEnchaine::nouveau(format(), RECOUVREMENT, courbe).expect("recouvrement non nul");
    let atelier = AtelierDeFondu::nouveau(moteur, Box::new(puits.clone()));

    // Deux fils, deux décodeurs, deux voies. Le décodage a lieu DANS le fil :
    // c'est la simultanéité qu'on mesure, pas seulement le mélange.
    let (bornes_a, bornes_b) = std::thread::scope(|portee| {
        let mut voie_sortante = atelier.voie_sortante();
        let mut voie_entrante = atelier.voie_entrante();
        let fil_a = portee.spawn(move || {
            let (mots, debut, fin) = decoder("test.wav", 1.0);
            for bloc in mots.chunks(4_096 * c) {
                assert!(voie_sortante.ecrire(bloc), "puits mort côté sortante");
            }
            (debut, fin)
        });
        let fil_b = portee.spawn(move || {
            let (mots, debut, fin) = decoder("test.flac", 1.0);
            for bloc in mots.chunks(4_096 * c) {
                assert!(voie_entrante.ecrire(bloc), "puits mort côté entrante");
            }
            (debut, fin)
        });
        let a = fil_a.join().expect("le fil sortant ne panique pas");
        let b = fil_b.join().expect("le fil entrant ne panique pas");
        (a, b)
    });

    // Les deux décodages se recouvrent dans le temps.
    assert!(
        bornes_a.0 < bornes_b.1 && bornes_b.0 < bornes_a.1,
        "les deux décodages ne se sont pas recouverts : \
         a=[{:?}] b=[{:?}] — sans recouvrement il n'y a pas deux producteurs, \
         et c'est exactement l'état que #2211 dénonce",
        bornes_a.1.duration_since(bornes_a.0),
        bornes_b.1.duration_since(bornes_b.0)
    );

    atelier.fin_de_la_sortante();
    atelier.vider();
    assert_eq!(atelier.trames_melangees(), RECOUVREMENT);

    let livre = puits.mots();
    assert_eq!(
        livre.len(),
        attendu_a.len() + attendu_b.len() - RECOUVREMENT * c,
        "conservation : rien n'est perdu ni dupliqué"
    );

    // Avant le recouvrement : la sortante, mot pour mot.
    let avant = (trames_a - RECOUVREMENT) * c;
    assert_eq!(
        &livre[..avant],
        &attendu_a[..avant],
        "hors du recouvrement, le fondu ne doit toucher à RIEN"
    );
    // Après : l'entrante, mot pour mot.
    assert_eq!(
        &livre[avant + RECOUVREMENT * c..],
        &attendu_b[RECOUVREMENT * c..],
        "après le recouvrement, l'entrante doit traverser intacte"
    );

    // ── La zone de recouvrement, échantillon par échantillon. ──
    //
    // Deux mesures distinctes, et il faut les deux :
    //
    //   * chaque mot vaut EXACTEMENT `a·g_sortant + b·g_entrant`. C'est la
    //     somme pondérée, pas une atténuation ;
    //   * chaque mot diffère de la contribution de la sortante SEULE et de
    //     celle de l'entrante SEULE. Une rampe de volume, elle, rend toujours
    //     un mot proportionnel à UNE source — c'est le défaut de #2211, et
    //     c'est ce compte-là qui l'exclut, quel que soit le contenu des deux
    //     fichiers.
    let seuil = 1e-4f32;
    let mut deux_contributions = 0usize;
    let mut total = 0usize;
    for i in 0..RECOUVREMENT {
        let t = i as f32 / (RECOUVREMENT - 1) as f32;
        let (g_sortant, g_entrant) = courbe.gains(t);
        for canal in 0..c {
            let a = attendu_a[(trames_a - RECOUVREMENT + i) * c + canal];
            let b = attendu_b[i * c + canal];
            let obtenu = livre[avant + i * c + canal];
            let part_sortante = a * g_sortant;
            let part_entrante = b * g_entrant;
            let attendu = part_sortante + part_entrante;
            assert!(
                (obtenu - attendu).abs() <= 1e-6,
                "trame {i} canal {canal} : {obtenu} au lieu de \
                 {a}·{g_sortant} + {b}·{g_entrant} = {attendu}"
            );
            total += 1;
            if part_sortante.abs() > seuil && part_entrante.abs() > seuil {
                deux_contributions += 1;
                assert!(
                    (obtenu - part_sortante).abs() > seuil / 2.0,
                    "trame {i} canal {canal} : le mot livré vaut la sortante \
                     seule — l'entrante n'a rien apporté"
                );
                assert!(
                    (obtenu - part_entrante).abs() > seuil / 2.0,
                    "trame {i} canal {canal} : le mot livré vaut l'entrante \
                     seule — la sortante n'a rien apporté"
                );
            }
        }
    }
    assert!(
        deux_contributions * 2 > total,
        "la majorité des mots du recouvrement doit porter une contribution \
         NON NULLE des deux sources : {deux_contributions}/{total}. En dessous, \
         ce témoin ne prouverait pas la superposition — il faudrait deux \
         fixtures moins silencieuses."
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Bornes et durée
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn la_continuite_et_la_duree_sont_exactes() {
    let c = CANAUX as usize;
    for courbe in [CourbeDeFondu::Lineaire, CourbeDeFondu::PuissanceConstante] {
        let puits = PuitsPartage::nouveau(format(), 4 * TRAMES_PISTE * c);
        let moteur =
            FonduEnchaine::nouveau(format(), RECOUVREMENT, courbe).expect("recouvrement non nul");
        let atelier = AtelierDeFondu::nouveau(moteur, Box::new(puits.clone()));

        let sortant = ton(SORTANT_HZ, TRAMES_PISTE, 0.5);
        let entrant = ton(ENTRANT_HZ, TRAMES_PISTE, 0.5);
        let mut voie_sortante = atelier.voie_sortante();
        let mut voie_entrante = atelier.voie_entrante();
        assert!(voie_sortante.ecrire(&sortant));
        assert!(voie_entrante.ecrire(&entrant));
        atelier.fin_de_la_sortante();
        atelier.vider();

        let livre = puits.mots();
        assert_eq!(
            atelier.trames_melangees(),
            RECOUVREMENT,
            "{courbe:?} : durée du recouvrement"
        );
        assert_eq!(
            livre.len(),
            (2 * TRAMES_PISTE - RECOUVREMENT) * c,
            "{courbe:?} : durée totale"
        );

        // Première trame du recouvrement : la sortante PURE — pas de marche.
        for canal in 0..c {
            let obtenu = livre[DEBUT_RECOUVREMENT * c + canal];
            let attendu = sortant[DEBUT_RECOUVREMENT * c + canal];
            assert!(
                (obtenu - attendu).abs() <= 1e-6,
                "{courbe:?} : marche à l'ENTRÉE du fondu ({obtenu} ≠ {attendu})"
            );
        }
        // Dernière trame : l'entrante PURE — pas de marche non plus.
        let derniere = FIN_RECOUVREMENT - 1;
        for canal in 0..c {
            let obtenu = livre[derniere * c + canal];
            let attendu = entrant[(RECOUVREMENT - 1) * c + canal];
            assert!(
                (obtenu - attendu).abs() <= 1e-6,
                "{courbe:?} : marche à la SORTIE du fondu ({obtenu} ≠ {attendu})"
            );
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// La garde du volume
// ───────────────────────────────────────────────────────────────────────────

/// L'arbitrage du 02/09/2026 : le volume matériel ne doit plus être touché.
///
/// `tune-core/tests/crossfade_pas_de_rampe_de_volume.rs` interdit déjà le
/// retour de la rampe dans tout `src/`. Celui-ci vise le module neuf et dit
/// pourquoi : un fondu qui atteindrait un volume de sortie serait de nouveau
/// le défaut de #2211, quelle que soit la finesse de sa courbe.
#[test]
fn le_fondu_ne_touche_a_aucun_volume() {
    let mut chemin = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    chemin.push("src");
    chemin.push("audio");
    chemin.push("fondu_enchaine.rs");
    let source = std::fs::read_to_string(&chemin)
        .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
    // Seules les lignes de CODE : l'en-tête du module doit rester libre
    // d'expliquer ce qu'il ne fait pas.
    let code: String = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for aiguille in [
        format!("set_{}", "volume"),
        format!("checked_set_{}", "volume"),
        format!("OutputT{}", "arget"),
    ] {
        assert!(
            !code.contains(&aiguille),
            "le module de fondu atteint « {aiguille} » : un fondu enchaîné \
             mélange des échantillons, il ne pilote AUCUN volume de sortie. \
             C'est le défaut exact de #2211, et sur une sortie matérielle \
             c'est le volume PERSISTANT de la zone qui bouge."
        );
    }
}
