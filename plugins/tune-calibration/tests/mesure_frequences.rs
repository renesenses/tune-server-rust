//! rc14 — la mesure à la fréquence native du micro.
//!
//! La zone joue le balayage à 48 kHz ; le micro capte à 44,1, 48 ou 96 kHz,
//! éventuellement avec une horloge qui dérive de ±40 ppm (deux appareils
//! USB séparés). La réponse mesurée et la correction proposée doivent être
//! celles d'une mesure à 48 kHz pile, à ±0,5 dB sur la bande de correction,
//! et l'étalonnage du micro doit tomber aux mêmes fréquences.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_calibration::analysis::LogResponse;
use tune_calibration::sim::RoomSim;
use tune_calibration_greffon::correction::{DemandeCorrection, proposer};
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE, Resultat};
use tune_calibration_greffon::simulation::micro_a_cadence::MicroACadence;
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "Interface simulée";
const VITESSE: f64 = 8.0;
/// La bande de correction la plus large que l'écran propose.
const BANDE: (f64, f64) = (20.0, 1_000.0);
const TOLERANCE_DB: f64 = 0.5;

/// Un micro : `None` pour le micro de référence à 48 kHz pile, sinon sa
/// fréquence et la dérive de son horloge.
fn controleur(
    micro: Option<(u32, f64)>,
    nom: &str,
) -> (Arc<Controleur>, tune_core::test_scratch::ScratchDir) {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(50.0));
    let micro: Arc<dyn Peripheriques> = match micro {
        None => Arc::new(MicroSimule {
            nom: MICRO.into(),
            frequence: FREQUENCE_MESURE,
            boucle,
            demarrages: Default::default(),
        }),
        Some((frequence, derive_ppm)) => Arc::new(MicroACadence {
            nom: MICRO.into(),
            frequence,
            derive_ppm,
            frequence_zone: FREQUENCE_MESURE,
            boucle,
        }),
    };
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-mf-{nom}"));
    let c = Controleur::new(micro, hote, fournisseur, dossier.path().to_path_buf());
    (c, dossier)
}

fn mesurer(c: &Arc<Controleur>, etalonnage: Option<&str>) -> Arc<Resultat> {
    let mut d = Demande::defaut(7, MICRO);
    d.duree_s = 2.0;
    d.etalonnage = etalonnage.map(str::to_string);
    let id = c.lancer(d).expect("lancée");
    let t0 = Instant::now();
    loop {
        let m = c.mesure(&id).unwrap();
        let e = m.etat();
        if e.finale() {
            assert_eq!(e, Etat::Terminee, "{:?}", m.avancement().avertissements);
            return m.resultat().expect("résultat");
        }
        assert!(
            t0.elapsed() < Duration::from_secs(120),
            "la mesure ne finit pas : {e:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn mesure_seule(micro: Option<(u32, f64)>, nom: &str) -> Arc<Resultat> {
    let (c, _d) = controleur(micro, nom);
    mesurer(&c, None)
}

fn db_a(r: &Resultat, f: f64) -> f64 {
    let i = r.reponse.freqs_hz.iter().position(|g| *g >= f).unwrap();
    r.reponse.db[i]
}

/// Le plus grand écart entre deux réponses sur la bande de correction.
fn ecart_reponse(a: &Resultat, b: &Resultat) -> (f64, f64) {
    assert_eq!(
        a.reponse.freqs_hz, b.reponse.freqs_hz,
        "même grille d'analyse"
    );
    a.reponse
        .freqs_hz
        .iter()
        .zip(a.reponse.db.iter().zip(&b.reponse.db))
        .filter(|(f, _)| (BANDE.0..=BANDE.1).contains(*f))
        .map(|(f, (x, y))| (*f, (x - y).abs()))
        .fold((0.0, 0.0), |m, e| if e.1 > m.1 { e } else { m })
}

/// La correction (gain de l'égaliseur proposé, par fréquence).
fn correction(r: &Resultat) -> (Vec<f64>, Vec<f64>) {
    let p = proposer(
        &LogResponse {
            freqs: r.reponse.freqs_hz.clone(),
            magnitude_db: r.reponse.db.clone(),
        },
        &DemandeCorrection::default(),
    )
    .expect("proposition");
    let c = &p.courbes;
    let gain = c
        .corrigee_db
        .iter()
        .zip(&c.mesure_db)
        .map(|(a, b)| a - b)
        .collect();
    (c.freqs_hz.clone(), gain)
}

fn comparer(nom: &str, r: &Resultat, reference: &Resultat) {
    assert_eq!(r.frequence, FREQUENCE_MESURE, "{nom} : analyse à 48 kHz");
    let (f, e) = ecart_reponse(r, reference);
    assert!(
        e <= TOLERANCE_DB,
        "{nom} : réponse à {e:.2} dB de la référence à {f:.0} Hz"
    );
    // Les modes de la pièce (42, 68, 115 Hz) sont toujours là.
    assert!(db_a(r, 42.0) > db_a(r, 30.0) + 4.0, "{nom} : mode de 42 Hz");
    let (fa, ga) = correction(r);
    let (fb, gb) = correction(reference);
    assert_eq!(fa, fb);
    let pire = ga
        .iter()
        .zip(&gb)
        .zip(&fa)
        .map(|((a, b), f)| (*f, (a - b).abs()))
        .fold((0.0, 0.0), |m, e| if e.1 > m.1 { e } else { m });
    eprintln!(
        "{nom} : réponse ±{e:.3} dB ({f:.0} Hz), correction ±{:.3} dB ({:.0} Hz), retard {:.2} ms",
        pire.1, pire.0, r.ri.retard_ms
    );
    assert!(
        pire.1 <= TOLERANCE_DB,
        "{nom} : correction à {:.2} dB de la référence à {:.0} Hz",
        pire.1,
        pire.0
    );
}

/// 44,1, 48 et 96 kHz en capture, zone à 48 kHz : même réponse, même
/// correction ; et avec une horloge de micro qui dérive (±40 ppm, le pire
/// relevé entre deux appareils USB).
#[test]
fn la_mesure_est_la_meme_a_toutes_les_frequences_de_capture() {
    let cas: [(&str, Option<(u32, f64)>); 5] = [
        ("48k-cadence", Some((48_000, 0.0))),
        ("44k1", Some((44_100, 0.0))),
        ("96k", Some((96_000, 0.0))),
        ("44k1-derive", Some((44_100, 40.0))),
        ("96k-derive", Some((96_000, -40.0))),
    ];
    let reference = std::thread::spawn(|| mesure_seule(None, "reference"));
    let fils: Vec<_> = cas
        .iter()
        .map(|(nom, micro)| {
            let (nom, micro) = (nom.to_string(), *micro);
            std::thread::spawn(move || (nom.clone(), mesure_seule(micro, &nom)))
        })
        .collect();
    let reference = reference.join().unwrap();
    for f in fils {
        let (nom, r) = f.join().unwrap();
        comparer(&nom, &r, &reference);
        // Le retard reste celui de la pièce (12 ms) plus la latence de la
        // sortie simulée (50 ms au plus) et des blocs de 10 ms du micro : le
        // rééchantillonnage ne le décale pas.
        assert!(
            (12.0..=12.0 + 80.0).contains(&r.ri.retard_ms),
            "{nom} : retard {} ms (référence {})",
            r.ri.retard_ms,
            reference.ri.retard_ms
        );
    }
}

/// L'étalonnage du micro (indexé en Hz) tombe aux mêmes fréquences quand le
/// micro capte à 96 kHz : la bosse déclarée à 100 Hz est retirée à 100 Hz,
/// rien ailleurs.
#[test]
fn l_etalonnage_du_micro_tombe_aux_bonnes_frequences_a_96_khz() {
    let (c, _d) = controleur(Some((96_000, 0.0)), "etalonnage-96k");
    let texte =
        "\"Sens Factor =-1.0dB, SERNO: 7001234\"\n10 0.0\n50 0.0\n100 6.0\n200 0.0\n20000 0.0\n";
    c.deposer_etalonnage("umik-96k", texte).unwrap();
    let avec = mesurer(&c, Some("umik-96k"));
    let sans = mesurer(&c, None);
    let d = |f: f64| db_a(&avec, f) - db_a(&sans, f);
    assert!((d(100.0) + 6.0).abs() < 0.5, "100 Hz : {}", d(100.0));
    assert!((d(70.7) + 3.0).abs() < 0.5, "70,7 Hz : {}", d(70.7));
    for f in [40.0, 300.0, 1_000.0, 5_000.0] {
        assert!(d(f).abs() < 0.3, "{f} Hz : {}", d(f));
    }
}
