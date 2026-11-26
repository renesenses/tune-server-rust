//! Retour de Didier (rc13.90) : « Mesurer avec la correction » montrait une
//! courbe « après » identique à la courbe « avant ». La mesure de
//! vérification contournait, comme toute mesure depuis #8513, l'égaliseur
//! qu'elle devait entendre : avant et après étaient deux mesures de la pièce
//! nue. Une mesure `avec_correction` joue avec les traitements de la zone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_calibration::sim::RoomSim;
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::hote::TraitementsDeZone;
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE};
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "UMIK-1 simulé";
const VITESSE: f64 = 8.0;

struct Banc {
    controleur: Arc<Controleur>,
    hote: Arc<HoteSimule>,
    _dossier: tune_core::test_scratch::ScratchDir,
}

fn banc(nom: &str, traitements: TraitementsDeZone) -> Banc {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote =
        HoteSimule::avec_traitements(fournisseur.clone(), boucle.clone(), Some(60.0), traitements);
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-{nom}"));
    let controleur = Controleur::new(
        micro,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    Banc {
        controleur,
        hote,
        _dossier: dossier,
    }
}

fn verification(zone: i64) -> Demande {
    let mut d = Demande::defaut(zone, MICRO);
    d.duree_s = 2.0;
    d.avec_correction = true;
    d
}

fn attendre_fin(c: &Controleur, id: &str) -> Etat {
    let t0 = Instant::now();
    loop {
        let e = c.mesure(id).expect("mesure connue").etat();
        if e.finale() {
            return e;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "la mesure ne finit pas : {e:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn la_mesure_de_verification_entend_la_correction_de_la_zone() {
    const ZONE: i64 = 9601;
    let b = banc(
        "verification",
        TraitementsDeZone {
            egaliseur: true,
            ..Default::default()
        },
    );
    let id = b.controleur.lancer(verification(ZONE)).expect("lancée");
    assert_eq!(attendre_fin(&b.controleur, &id), Etat::Terminee);
    let j = b.hote.journal();
    assert!(!j.contournes_au_jeu.is_empty());
    assert!(
        j.contournes_au_jeu.iter().all(|c| !*c),
        "vérification jouée SANS la correction de la zone : {:?}",
        j.contournes_au_jeu
    );
    drop(j);
    assert!(!tune_core::audio::mesure_nue::active(ZONE));
    let av = b
        .controleur
        .mesure(&id)
        .unwrap()
        .avancement()
        .avertissements;
    assert!(!av.iter().any(|a| a.contains("contourné")), "{av:?}");
}

#[test]
fn une_verification_sans_correction_active_le_dit() {
    const ZONE: i64 = 9602;
    let b = banc("verification-sans", TraitementsDeZone::default());
    let id = b.controleur.lancer(verification(ZONE)).expect("lancée");
    assert_eq!(attendre_fin(&b.controleur, &id), Etat::Terminee);
    let av = b
        .controleur
        .mesure(&id)
        .unwrap()
        .avancement()
        .avertissements;
    assert!(
        av.iter()
            .any(|a| a.contains("aucune correction n'est active")),
        "{av:?}"
    );
}

#[test]
fn en_pure_la_verification_est_refusee_en_le_disant() {
    let b = banc(
        "verification-pure",
        TraitementsDeZone {
            egaliseur: true,
            pure: true,
            ..Default::default()
        },
    );
    let e = b.controleur.lancer(verification(9603)).unwrap_err();
    assert!(e.contains("PURE"), "{e}");
    // Une mesure ordinaire, elle, est lancée en PURE.
    let mut d = verification(9603);
    d.avec_correction = false;
    let id = b.controleur.lancer(d).expect("mesure ordinaire en PURE");
    assert_eq!(attendre_fin(&b.controleur, &id), Etat::Terminee);
}
