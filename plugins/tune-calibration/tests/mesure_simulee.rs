//! Mesure de bout en bout sur pièces simulées (#7632, C1) : fournisseur,
//! générateur piloté, capture vers tampon, montée de niveau, déconvolution,
//! analyse — et les garde-fous, chacun avec le refus qu'il doit produire.
//! Aucun son n'est joué ni capté : la « zone » dépose dans une boucle, le
//! « micro » la tire à travers une pièce synthétique.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_calibration::sim::RoomSim;
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::generateur::Consigne;
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE};
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "UMIK-1 simulé";
const VITESSE: f64 = 25.0;

struct Banc {
    controleur: Arc<Controleur>,
    hote: Arc<HoteSimule>,
    fournisseur: Arc<FournisseurBalayage>,
    _dossier: tempdir::Dossier,
}

mod tempdir {
    use std::path::PathBuf;

    pub struct Dossier(pub PathBuf);
    impl Dossier {
        pub fn neuf(nom: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "tune-calibration-{nom}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Dossier {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn banc(sim: &RoomSim, gain_micro: f64, volume: Option<f64>, nom: &str) -> Banc {
    let boucle = Boucle::new(PieceEnDirect::depuis(sim, gain_micro), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), volume);
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tempdir::Dossier::neuf(nom);
    let controleur = Controleur::new(micro, hote.clone(), fournisseur.clone(), dossier.0.clone());
    Banc {
        controleur,
        hote,
        fournisseur,
        _dossier: dossier,
    }
}

fn demande(zone: i64) -> Demande {
    let mut d = Demande::defaut(zone, MICRO);
    d.duree_s = 2.0;
    d
}

fn attendre_fin(c: &Controleur, id: &str, delai: Duration) -> Etat {
    let t0 = Instant::now();
    loop {
        let m = c.mesure(id).expect("mesure connue");
        let e = m.etat();
        if e.finale() {
            return e;
        }
        assert!(t0.elapsed() < delai, "la mesure ne finit pas : {e:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn attendre_etat(c: &Controleur, id: &str, pred: impl Fn(&Etat) -> bool, delai: Duration) -> Etat {
    let t0 = Instant::now();
    loop {
        let e = c.mesure(id).unwrap().etat();
        if pred(&e) || e.finale() {
            return e;
        }
        assert!(t0.elapsed() < delai, "état non atteint : {e:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Le salon sec du prototype (retard 12 ms, trois modes, bruit −80 dBFS) :
/// la montée de niveau s'arrête dès que le RSB visé est atteint, le balayage
/// part à ce niveau, la réponse impulsionnelle retrouve le retard au
/// dixième de milliseconde, les modes sont des maxima de la réponse, le
/// RT60 est là, et la zone est rendue telle qu'elle était.
#[test]
fn une_piece_simulee_est_mesuree_de_bout_en_bout() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "bout-en-bout");
    let id = b.controleur.lancer(demande(7)).expect("lancée");
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(60));
    assert_eq!(
        etat,
        Etat::Terminee,
        "{:?}",
        b.controleur
            .mesure(&id)
            .unwrap()
            .avancement()
            .avertissements
    );
    let m = b.controleur.mesure(&id).unwrap();
    let r = m.resultat().expect("résultat");

    // Niveau : départ à −30 dBFS, jamais au-dessus de −6, accepté dès le RSB visé.
    assert_eq!(r.salves[0].niveau_joue_dbfs, -30.0);
    assert!(
        r.niveau_joue_dbfs <= -6.0,
        "niveau joué {}",
        r.niveau_joue_dbfs
    );
    assert_eq!(r.salves.last().unwrap().decision, "accept");
    assert!(r.salves.last().unwrap().rsb_db >= 50.0);
    let emissions = m.deroule.emissions();
    assert!(emissions.iter().all(|e| e.niveau_dbfs <= -6.0));
    assert!(matches!(
        emissions.last().unwrap().consigne,
        Consigne::Balayage { .. }
    ));
    assert_eq!(
        m.deroule
            .consignes_ecretees
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // Réponse impulsionnelle : le retard de 12 ms, au dixième de ms.
    assert!(
        // Vol acoustique (12 ms) PLUS la latence de la sortie : ici l'avance
        // de la zone simulée sur le micro, 50 ms au plus, par blocs de 10 ms.
        (12.0..=12.0 + 70.0).contains(&r.ri.retard_ms),
        "retard {} ms",
        r.ri.retard_ms
    );
    assert!(r.ri.rsb_db > 50.0, "RSB {}", r.ri.rsb_db);

    // Les trois modes (42, 68, 115 Hz) ressortent comme bosses.
    let a = |f: f64| {
        let i = r.reponse.freqs_hz.iter().position(|g| *g >= f).unwrap();
        r.reponse.db[i]
    };
    assert!(
        a(42.0) > a(30.0) + 4.0,
        "42 Hz {} vs 30 Hz {}",
        a(42.0),
        a(30.0)
    );
    assert!(
        a(42.0) > a(55.0) + 2.0,
        "42 Hz {} vs 55 Hz {}",
        a(42.0),
        a(55.0)
    );
    assert!(
        a(68.0) > a(55.0) + 1.5,
        "68 Hz {} vs 55 Hz {}",
        a(68.0),
        a(55.0)
    );
    assert!(
        a(115.0) > a(150.0) + 2.0,
        "115 Hz {} vs 150 Hz {}",
        a(115.0),
        a(150.0)
    );
    // Au-dessus des modes, la pièce sèche est plate à ±1,5 dB.
    assert!(
        (a(1000.0) - a(4000.0)).abs() < 1.5,
        "1 kHz {} vs 4 kHz {}",
        a(1000.0),
        a(4000.0)
    );
    assert!(!r.rt60.is_empty());
    assert!(r.rt60.iter().any(|b| b.centre_hz == 1000.0));

    // La zone : volume forcé à 40 %, puis rendu à 63 % ; arrêtée une fois ;
    // le fournisseur désarmé.
    let j = b.hote.journal();
    assert_eq!(j.volumes, vec![40.0, 63.0]);
    assert_eq!(j.arrets, 1);
    assert_eq!(j.joues.len(), 1);
    assert_eq!(j.joues[0].mesure_id, id);
    assert_eq!(b.fournisseur.armes(), 0);
    assert!(m.ri_wav().is_some());
    let av = m.avancement();
    assert!(av.progression > 0.0);
}

/// Garde-fou du niveau joué : une pièce trop sourde (enceinte presque
/// muette, bruit élevé) n'atteint jamais le RSB visé ; la montée s'arrête
/// à −6 dBFS et la mesure est REFUSÉE en le disant, sans balayage complet,
/// zone rendue.
#[test]
fn le_plafond_joue_arrete_la_montee_sur_une_piece_trop_sourde() {
    let mut sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    sim.direct_gain = 0.002;
    sim.noise_floor_dbfs = -60.0;
    let b = banc(&sim, 1.0, Some(50.0), "trop-sourde");
    let id = b.controleur.lancer(demande(3)).unwrap();
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(90));
    match &etat {
        Etat::Refusee { motif } => assert!(motif.contains("plafonné"), "{motif}"),
        autre => panic!("attendu un refus, obtenu {autre:?}"),
    }
    let m = b.controleur.mesure(&id).unwrap();
    let emissions = m.deroule.emissions();
    assert!(!emissions.is_empty());
    assert!(
        emissions.iter().all(|e| e.niveau_dbfs <= -6.0),
        "niveaux joués : {:?}",
        emissions.iter().map(|e| e.niveau_dbfs).collect::<Vec<_>>()
    );
    assert!(
        emissions
            .iter()
            .all(|e| matches!(e.consigne, Consigne::Salve { .. }))
    );
    assert_eq!(emissions.last().unwrap().niveau_dbfs, -6.0);
    assert!(m.resultat().is_none());
    let j = b.hote.journal();
    assert_eq!(j.volumes, vec![40.0, 50.0]);
    assert_eq!(j.arrets, 1);
    assert_eq!(b.fournisseur.armes(), 0);
}

/// Garde-fou de la capture : un micro trop chaud écrête dès la première
/// salve à −30 dBFS ; refus immédiat, une seule émission.
#[test]
fn une_capture_qui_ecrete_est_refusee_des_la_premiere_salve() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    // Gain de micro ×200 : à −30 dBFS, le son direct (×0,3) dépasse la
    // pleine échelle dès la première salve.
    let b = banc(&sim, 200.0, Some(50.0), "ecretage");
    let id = b.controleur.lancer(demande(4)).unwrap();
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(60));
    match &etat {
        Etat::Refusee { motif } => assert!(motif.contains("écrêtage"), "{motif}"),
        autre => panic!("attendu un refus, obtenu {autre:?}"),
    }
    let m = b.controleur.mesure(&id).unwrap();
    assert_eq!(m.deroule.emissions().len(), 1);
    assert_eq!(m.avancement().salves.len(), 1);
    assert!(m.avancement().salves[0].crete_captee_dbfs > -0.1);
    assert_eq!(b.hote.journal().volumes, vec![40.0, 50.0]);
}

/// Garde-fou du volume : zone au volume inconnu ⇒ rien ne part, rien n'est
/// armé, rien n'est joué.
#[test]
fn sans_volume_connu_rien_ne_part() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "volume-inconnu");
    let e = b.controleur.lancer(demande(9)).unwrap_err();
    assert!(e.contains("volume"), "{e}");
    let j = b.hote.journal();
    assert!(j.joues.is_empty());
    assert!(j.volumes.is_empty());
    assert_eq!(b.fournisseur.armes(), 0);
    assert!(b.controleur.mesures().is_empty());
}

/// Une zone en volume fixe n'est pas mesurable : le niveau joué ne serait
/// pas maîtrisé.
#[test]
fn une_zone_en_volume_fixe_est_refusee() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let mut hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(30.0));
    Arc::get_mut(&mut hote).unwrap().traitements.volume_fixe = true;
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tempdir::Dossier::neuf("volume-fixe");
    let c = Controleur::new(micro, hote.clone(), fournisseur.clone(), dossier.0.clone());
    let e = c.lancer(demande(2)).unwrap_err();
    assert!(e.contains("volume fixe"), "{e}");
    assert!(hote.journal().joues.is_empty());
}

/// L'annulation pendant le balayage : état Annulée, zone arrêtée, volume
/// rendu, fournisseur désarmé, pas de résultat.
#[test]
fn l_annulation_coupe_le_balayage_et_rend_la_zone() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(55.0), "annulation");
    let mut d = demande(5);
    d.duree_s = 8.0;
    let id = b.controleur.lancer(d).unwrap();
    let e = attendre_etat(
        &b.controleur,
        &id,
        |e| matches!(e, Etat::Balayage { .. }),
        Duration::from_secs(60),
    );
    assert!(matches!(e, Etat::Balayage { .. }), "{e:?}");
    assert!(b.controleur.annuler(&id));
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(30));
    assert_eq!(etat, Etat::Annulee);
    let m = b.controleur.mesure(&id).unwrap();
    assert!(m.resultat().is_none());
    let j = b.hote.journal();
    assert_eq!(j.volumes, vec![40.0, 55.0]);
    assert_eq!(j.arrets, 1);
    assert_eq!(b.fournisseur.armes(), 0);
    // Annuler une mesure finie ne fait rien.
    assert!(!b.controleur.annuler(&id));
}

/// Deux mesures sur la même zone ne se chevauchent pas.
#[test]
fn une_seule_mesure_a_la_fois_par_zone() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(50.0), "une-a-la-fois");
    let id = b.controleur.lancer(demande(6)).unwrap();
    let e = b.controleur.lancer(demande(6)).unwrap_err();
    assert!(e.contains("déjà en cours"), "{e}");
    b.controleur.annuler(&id);
    attendre_fin(&b.controleur, &id, Duration::from_secs(60));
}

/// L'étalonnage du micro : un fichier miniDSP déposé est relu, et la mesure
/// corrigée annule la coloration qu'il décrit (ici une bosse de +6 dB à
/// 100 Hz côté micro, déclarée dans le fichier).
#[test]
fn l_etalonnage_du_micro_est_depose_puis_applique() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(50.0), "etalonnage");
    assert!(
        b.controleur
            .deposer_etalonnage("../x", "10 0\n20 0\n")
            .is_err()
    );
    assert!(b.controleur.deposer_etalonnage("vide", "* rien\n").is_err());
    let texte =
        "\"Sens Factor =-1.0dB, SERNO: 7001234\"\n10 0.0\n50 0.0\n100 6.0\n200 0.0\n20000 0.0\n";
    let cal = b.controleur.deposer_etalonnage("umik-test", texte).unwrap();
    assert_eq!(cal.serial.as_deref(), Some("7001234"));
    assert_eq!(b.controleur.etalonnages(), vec!["umik-test".to_string()]);

    let mut d = demande(8);
    d.etalonnage = Some("umik-test".into());
    let id = b.controleur.lancer(d).unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(60)),
        Etat::Terminee
    );
    let r = b.controleur.mesure(&id).unwrap().resultat().unwrap();
    assert_eq!(r.etalonnage.as_deref(), Some("umik-test"));
    // Le micro a un facteur de sensibilité : le SPL est estimé à chaque salve.
    assert!(r.salves.iter().all(|s| s.spl_db.is_some()));

    // Sans étalonnage, la même pièce : la différence à 100 Hz est −6 dB
    // (la correction retire la bosse déclarée), nulle ailleurs.
    let id2 = b.controleur.lancer(demande(8)).unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id2, Duration::from_secs(60)),
        Etat::Terminee
    );
    let r2 = b.controleur.mesure(&id2).unwrap().resultat().unwrap();
    let a = |r: &tune_calibration_greffon::mesure::Resultat, f: f64| {
        let i = r.reponse.freqs_hz.iter().position(|g| *g >= f).unwrap();
        r.reponse.db[i]
    };
    assert!(
        (a(&r, 100.0) - a(&r2, 100.0) + 6.0).abs() < 0.5,
        "{} vs {}",
        a(&r, 100.0),
        a(&r2, 100.0)
    );
    assert!((a(&r, 1000.0) - a(&r2, 1000.0)).abs() < 0.3);

    let e = b.controleur.lancer({
        let mut d = demande(1);
        d.etalonnage = Some("inconnu".into());
        d
    });
    assert!(e.unwrap_err().contains("introuvable"));
}

/// Une entrée qui ne capte pas à 48 kHz est refusée avant tout.
#[test]
fn une_entree_a_la_mauvaise_cadence_est_refusee() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(30.0));
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: 44_100,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tempdir::Dossier::neuf("cadence");
    let c = Controleur::new(micro, hote.clone(), fournisseur, dossier.0.clone());
    let e = c.lancer(demande(2)).unwrap_err();
    assert!(e.contains("44100"), "{e}");
    assert!(hote.journal().volumes.is_empty());
}
