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
use tune_calibration_greffon::hote::TraitementsDeZone;
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE};
use tune_calibration_greffon::simulation::{
    Boucle, CheminDeLecture, HoteSimule, MicroSimule, PieceEnDirect,
};
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "UMIK-1 simulé";
/// 8× le temps réel : la déconvolution de chaque salve (quelques centaines
/// de ms en debug) compte dans le budget de silence du générateur, multiplié
/// par la vitesse ; au-delà, six salves ne tiennent plus dans le flux promis.
const VITESSE: f64 = 8.0;

struct Banc {
    controleur: Arc<Controleur>,
    hote: Arc<HoteSimule>,
    fournisseur: Arc<FournisseurBalayage>,
    boucle: Arc<Boucle>,
    _dossier: tune_core::test_scratch::ScratchDir,
}

fn banc(sim: &RoomSim, gain_micro: f64, volume: Option<f64>, nom: &str) -> Banc {
    banc_traite(sim, gain_micro, volume, nom, TraitementsDeZone::default())
}

fn banc_traite(
    sim: &RoomSim,
    gain_micro: f64,
    volume: Option<f64>,
    nom: &str,
    traitements: TraitementsDeZone,
) -> Banc {
    let boucle = Boucle::new(PieceEnDirect::depuis(sim, gain_micro), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote =
        HoteSimule::avec_traitements(fournisseur.clone(), boucle.clone(), volume, traitements);
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle: boucle.clone(),
        demarrages: Default::default(),
    });
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-{nom}"));
    let controleur = Controleur::new(
        micro,
        hote.clone(),
        fournisseur.clone(),
        dossier.path().to_path_buf(),
    );
    Banc {
        controleur,
        hote,
        fournisseur,
        boucle,
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

/// #9066 (fil 2914) — la zone passe par le chemin d'une vraie sortie
/// locale : la vraie pompe (`source_pcm::pomper`), le canal de 256 tronçons
/// de la session, puis l'anneau de 2 s de la sortie. Ce chemin laissait le
/// générateur prendre ~4,5 s de silence d'avance pendant l'analyse de la
/// salve ; le balayage complet sortait après la fin de la fenêtre d'écoute
/// (1 s + 3,5 s) et la mesure était refusée sur le « rapport crête/bruit ».
/// La salve, elle, partait avant que l'avance ne s'accumule — exactement ce
/// que le testeur a entendu. Cadencé sur l'écoute, le balayage arrive à
/// l'heure et la RI retrouve le retard de la pièce.
#[test]
fn le_balayage_sort_pendant_l_ecoute_malgre_le_chemin_de_lecture_9066() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::avec_tampon(
        PieceEnDirect::depuis(&sim, 1.0),
        VITESSE,
        2 * FREQUENCE_MESURE as usize,
    );
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::avec_chemin(
        fournisseur.clone(),
        boucle.clone(),
        Some(50.0),
        CheminDeLecture::session_de_source_pcm(),
    );
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-chemin-9066");
    let controleur = Controleur::new(
        micro,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let mut d = demande(9);
    d.duree_s = 1.0;
    let id = controleur.lancer(d).expect("lancée");
    let etat = attendre_fin(&controleur, &id, Duration::from_secs(120));
    let m = controleur.mesure(&id).unwrap();
    assert_eq!(etat, Etat::Terminee, "{:?}", m.avancement());
    let r = m.resultat().expect("résultat");
    assert_eq!(r.salves.len(), 1, "{:?}", r.salves);
    // Vol acoustique (12 ms) plus l'avance permise sur l'écoute (300 ms),
    // par blocs de 10 ms : rien de l'anneau ni du canal.
    assert!(
        (12.0..=12.0 + 300.0 + 30.0).contains(&r.ri.retard_ms),
        "retard {} ms",
        r.ri.retard_ms
    );
    assert!(r.ri.rsb_db > 50.0, "RSB {}", r.ri.rsb_db);
    assert_eq!(hote.journal().arrets, 1);
}

/// #9066 (relecture) — le micro se fige pendant le balayage, sans erreur
/// (USB décroché, veille) : la mesure s'arrête en deux secondes en nommant
/// le micro, au lieu d'attendre une minute puis « capture trop lente ».
#[test]
fn un_micro_fige_arrete_la_mesure_tot_en_le_disant_9066() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(50.0));
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle: boucle.clone(),
        demarrages: Default::default(),
    });
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-micro-fige-9066");
    let controleur = Controleur::new(
        micro,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let id = controleur.lancer(demande(10)).expect("lancée");
    let e = attendre_etat(
        &controleur,
        &id,
        |e| matches!(e, Etat::Balayage { .. }),
        Duration::from_secs(60),
    );
    assert!(matches!(e, Etat::Balayage { .. }), "{e:?}");
    boucle.fige.store(true, std::sync::atomic::Ordering::SeqCst);
    let t0 = Instant::now();
    let etat = attendre_fin(&controleur, &id, Duration::from_secs(20));
    match &etat {
        Etat::Erreur { message } => {
            assert!(
                message.contains("ne capte plus") && message.contains(MICRO),
                "{message}"
            )
        }
        autre => panic!("{autre:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
    assert_eq!(hote.journal().arrets, 1);
    // Le code stable et le micro ouvert (#9070), pour que l'écran traduise.
    let av = controleur.mesure(&id).unwrap().avancement();
    assert_eq!(av.code, Some("micro_fige"));
    assert!(av.peripherique.is_some(), "{av:?}");
    let json = serde_json::to_value(&av).unwrap();
    assert_eq!(json["code"], "micro_fige", "{json}");
    assert_eq!(json["etat"], "erreur", "{json}");
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
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(180));
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
    // #8513 — refus : les traitements de la zone sont rétablis.
    assert!(!tune_core::audio::mesure_nue::active(3));
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
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-volume-fixe");
    let c = Controleur::new(
        micro,
        hote.clone(),
        fournisseur.clone(),
        dossier.path().to_path_buf(),
    );
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
    // #8513 — annulation : les traitements de la zone sont rétablis.
    assert!(!tune_core::audio::mesure_nue::active(5));
    // Annuler une mesure finie ne fait rien.
    assert!(!b.controleur.annuler(&id));
}

/// #8513 — égaliseur et correction IR actifs : la mesure entend la pièce
/// NUE. Les traitements sont contournés pendant tout le balayage (à chaque
/// lecture et jusqu'à l'arrêt), rétablis à la fin, et le résultat le dit au
/// lieu d'avertir que la mesure inclut la correction.
#[test]
fn les_traitements_de_la_zone_sont_contournes_pendant_la_mesure_8513() {
    const ZONE: i64 = 8513;
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc_traite(
        &sim,
        1.0,
        Some(60.0),
        "mesure-nue",
        TraitementsDeZone {
            egaliseur: true,
            correction_ir: true,
            ..Default::default()
        },
    );
    assert!(!tune_core::audio::mesure_nue::active(ZONE));
    let id = b.controleur.lancer(demande(ZONE)).expect("lancée");
    let etat = attendre_fin(&b.controleur, &id, Duration::from_secs(60));
    assert_eq!(etat, Etat::Terminee);
    {
        let j = b.hote.journal();
        assert!(!j.contournes_au_jeu.is_empty());
        assert!(
            j.contournes_au_jeu.iter().all(|c| *c),
            "balayage joué avec les traitements de la zone : {:?}",
            j.contournes_au_jeu
        );
        assert_eq!(j.contournes_a_l_arret, vec![true], "rétablis avant l'arrêt");
    }
    assert!(
        !tune_core::audio::mesure_nue::active(ZONE),
        "traitements restés contournés après la mesure"
    );
    let av = b
        .controleur
        .mesure(&id)
        .unwrap()
        .avancement()
        .avertissements;
    assert!(
        av.iter()
            .any(|a| a.contains("égaliseur de la zone contourné")),
        "{av:?}"
    );
    assert!(!av.iter().any(|a| a.contains("la mesure inclut")), "{av:?}");
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
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-cadence");
    let c = Controleur::new(
        micro,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let e = c.lancer(demande(2)).unwrap_err();
    assert!(e.contains("44100"), "{e}");
    assert!(hote.journal().volumes.is_empty());
}

/// C2 de bout en bout, par les routes : la mesure du salon sec donne un
/// égaliseur qui coupe ses modes (42 et 68 Hz) sans rien relever, vérifié sur
/// la VRAIE réponse de la pièce simulée (pas sur la mesure qui l'a produit) ;
/// les bornes sont refusées ; le profil de pièce est enregistré pour la zone
/// mesurée et rien d'autre n'est touché.
#[test]
fn la_mesure_simulee_donne_un_egaliseur_qui_coupe_les_modes_sans_relever() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_calibration::eq::{Band, bands_response_db};
    use tune_calibration_greffon::routes::{EtatRoutes, router};

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "correction");
    let id = b.controleur.lancer(demande(7)).expect("lancée");
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(60)),
        Etat::Terminee
    );
    // Le greffon est Premium (04/11/2026) : ce banc porte un compte Premium.
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let licence = Arc::new(tune_core::license::LicenseManager::new(Arc::new(db)));
    let app = router(EtatRoutes {
        controleur: b.controleur.clone(),
        license: Some(licence.clone()),
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(licence.set_account_premium(true, None));
    let poster = |chemin: String, corps: serde_json::Value| {
        let app = app.clone();
        rt.block_on(async move {
            let r = app
                .oneshot(
                    Request::post(chemin)
                        .header("content-type", "application/json")
                        .body(Body::from(corps.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let code = r.status();
            let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                code,
                serde_json::from_slice::<serde_json::Value>(&octets).unwrap(),
            )
        })
    };

    let (code, p) = poster(format!("/mesures/{id}/correction"), serde_json::json!({}));
    assert_eq!(code, StatusCode::OK, "{p}");
    let bandes: Vec<Band> = serde_json::from_value(p["bandes"].clone()).unwrap();
    assert!(!bandes.is_empty(), "{p}");
    assert!(
        bandes.iter().all(|b| b.gain <= 0.0),
        "relance proposée : {bandes:?}"
    );
    assert!(bandes.len() <= 8);
    assert_eq!(
        p["egaliseur"]["bands"].as_array().unwrap().len(),
        bandes.len()
    );

    // Vérification sur la pièce vraie : modes + égaliseur proposé.
    let fs = FREQUENCE_MESURE as f64;
    let grille = tune_calibration::analysis::LogResponse::grid(20.0, 300.0, 48);
    let vraie = bands_response_db(fs, &sim.modes_as_bands(), &grille);
    let eq = bands_response_db(fs, &bandes, &grille);
    let a = |v: &[f64], f: f64| v[grille.iter().position(|g| *g >= f).unwrap()];
    for f in [42.0, 68.0] {
        let apres = a(&vraie, f) + a(&eq, f);
        assert!(
            apres < a(&vraie, f) - 3.0,
            "mode {f} Hz : {:.1} dB avant, {apres:.1} dB après ({bandes:?})",
            a(&vraie, f)
        );
    }
    assert!(
        eq.iter().all(|g| *g <= 1e-9),
        "l'égaliseur relève quelque part entre 20 et 300 Hz : {:.2} dB",
        eq.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
    );
    assert!(p["coupe_cumulee_db"].as_f64().unwrap() >= -12.0 - 1e-6);

    // Bornes : refus motivé, pas d'écrêtage silencieux.
    let (code, e) = poster(
        format!("/mesures/{id}/correction"),
        serde_json::json!({ "f_max_hz": 2000.0 }),
    );
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert_eq!(e["error"], "correction_refusee");
    let (code, _) = poster("/mesures/inconnue/correction".into(), serde_json::json!({}));
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(
        b.hote.journal().profils.is_empty(),
        "une proposition n'écrit rien"
    );

    // Le profil de pièce de la zone 7.
    let (code, r) = poster(
        format!("/mesures/{id}/correction/profil"),
        serde_json::json!({ "nom": "Salon", "cible": { "type": "plate", "pente_db_oct": -0.5 } }),
    );
    assert_eq!(code, StatusCode::CREATED, "{r}");
    assert_eq!(r["appliquer"], "/api/v1/room-correction/profiles/7/apply");
    let j = b.hote.journal();
    assert_eq!(j.profils.len(), 1);
    assert_eq!(j.profils[0].zone_id, "7");
    assert_eq!(j.profils[0].name, "Salon");
    // D1 (TESS) : une mesure enregistre son origine.
    assert_eq!(j.profils[0].origin.as_deref(), Some("measured"));
    assert!(!j.profils[0].filters.is_empty());
    assert!(j.profils[0].filters.iter().all(|f| f.gain_db <= 0.0));
}

/// Tout le greffon est Premium (décision du 04/11/2026) : sans licence, ou
/// avec un compte gratuit, CHAQUE route répond le refus nommé
/// `premium_required` (402) et aucune mesure ne part — rien n'est joué ;
/// avec Premium, les mêmes routes servent.
#[test]
fn sans_premium_refus_nomme_avec_premium_les_routes_servent() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_calibration_greffon::routes::{EtatRoutes, router};
    use tune_core::license::LicenseManager;

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "premium");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let appeler =
        |licence: Option<Arc<LicenseManager>>, methode: &str, chemin: &str, corps: &str| {
            let app = router(EtatRoutes {
                controleur: b.controleur.clone(),
                license: licence,
            });
            let req = Request::builder()
                .method(methode)
                .uri(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap();
            rt.block_on(async move {
                let r = app.oneshot(req).await.unwrap();
                let code = r.status();
                let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    code,
                    serde_json::from_slice::<serde_json::Value>(&octets).unwrap_or_default(),
                )
            })
        };
    let corps_mesure = format!(r#"{{"entree":"{MICRO}","duree_s":2.0}}"#);
    let routes = [
        ("GET", "/microphones", ""),
        (
            "POST",
            "/microphones/etalonnages",
            r#"{"nom":"x","texte":"20 0\n20000 0"}"#,
        ),
        ("POST", "/zones/7/mesures", corps_mesure.as_str()),
        ("GET", "/mesures", ""),
        ("GET", "/mesures/inconnue", ""),
        ("GET", "/mesures/inconnue/resultat", ""),
        ("GET", "/mesures/inconnue/ri.wav", ""),
        ("DELETE", "/mesures/inconnue", ""),
    ];

    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let licence = Arc::new(LicenseManager::new(Arc::new(db)));

    // Sans licence fournie, puis avec un compte gratuit : refus nommé partout.
    for (cas, l) in [
        ("sans licence", None),
        ("compte gratuit", Some(licence.clone())),
    ] {
        for (m, chemin, corps) in routes {
            let (code, v) = appeler(l.clone(), m, chemin, corps);
            assert_eq!(
                code,
                StatusCode::PAYMENT_REQUIRED,
                "{cas} : {m} {chemin} → {v}"
            );
            assert_eq!(v["error"], "premium_required", "{cas} : {m} {chemin} → {v}");
            assert_eq!(v["code"], "room_correction", "{cas} : {m} {chemin}");
        }
    }
    assert!(
        b.hote.journal().joues.is_empty(),
        "une mesure est partie sans Premium"
    );
    assert!(b.controleur.mesures().is_empty());

    // Avec Premium : les routes servent, la mesure part.
    rt.block_on(licence.set_account_premium(true, None));
    let (code, v) = appeler(Some(licence.clone()), "GET", "/microphones", "");
    assert_eq!(code, StatusCode::OK, "{v}");
    let (code, v) = appeler(Some(licence.clone()), "GET", "/mesures/inconnue", "");
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
    let (code, v) = appeler(
        Some(licence.clone()),
        "POST",
        "/zones/7/mesures",
        &corps_mesure,
    );
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(60)),
        Etat::Terminee
    );
}

/// C5 (#7919) : la simulation sans micro, par les routes. Sans Premium (sans
/// licence ou compte gratuit), les trois routes rendent `premium_required` ;
/// avec Premium, la simulation donne les modes de la pièce 5 × 4 × 2,5 m, la
/// correction ne fait que couper sous le plafond prudent et se dit
/// estimation, le profil est enregistré pour la zone — et rien n'est joué.
#[test]
fn la_simulation_sans_micro_est_premium_et_ne_propose_que_des_coupes_prudentes() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_calibration_greffon::modes::{BANDES_DEFAUT_ESTIMATION, COUPE_DEFAUT_DB_ESTIMATION};
    use tune_calibration_greffon::routes::{EtatRoutes, router};
    use tune_core::license::LicenseManager;

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "simulation-c5");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let poster = |licence: Option<Arc<LicenseManager>>, chemin: &str, corps: serde_json::Value| {
        let app = router(EtatRoutes {
            controleur: b.controleur.clone(),
            license: licence,
        });
        let req = Request::post(chemin)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap();
        rt.block_on(async move {
            let r = app.oneshot(req).await.unwrap();
            let code = r.status();
            let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                code,
                serde_json::from_slice::<serde_json::Value>(&octets).unwrap_or_default(),
            )
        })
    };
    let piece = |zone: Option<i64>| {
        serde_json::json!({
            "piece": { "longueur_m": 5.0, "largeur_m": 4.0, "hauteur_m": 2.5 },
            "surfaces": {
                "avant": "reflechissante", "arriere": "reflechissante",
                "gauche": "amortie", "droite": "amortie",
                "sol": "reflechissante", "plafond": "traitee"
            },
            "auditeur": { "x": 3.2, "y": 1.6, "z": 1.1 },
            "enceintes": [ { "x": 0.6, "y": 0.9, "z": 0.9 }, { "x": 0.6, "y": 3.1, "z": 0.9 } ],
            "zone_id": zone,
        })
    };

    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let licence = Arc::new(LicenseManager::new(Arc::new(db)));
    for (cas, l) in [
        ("sans licence", None),
        ("compte gratuit", Some(licence.clone())),
    ] {
        for chemin in [
            "/simulations",
            "/simulations/sim-1/correction",
            "/simulations/sim-1/correction/profil",
        ] {
            let (code, v) = poster(l.clone(), chemin, piece(Some(7)));
            assert_eq!(code, StatusCode::PAYMENT_REQUIRED, "{cas} : {chemin} → {v}");
            assert_eq!(v["error"], "premium_required", "{cas} : {chemin} → {v}");
        }
    }

    rt.block_on(licence.set_account_premium(true, None));
    let l = || Some(licence.clone());
    let (code, s) = poster(l(), "/simulations", piece(Some(7)));
    assert_eq!(code, StatusCode::CREATED, "{s}");
    assert_eq!(s["estimation"], true);
    let id = s["id"].as_str().unwrap().to_string();
    let modes = s["modes"].as_array().unwrap();
    let premiers: Vec<f64> = modes
        .iter()
        .filter(|m| m["type"] == "axial")
        .take(3)
        .map(|m| m["frequence_hz"].as_f64().unwrap())
        .collect();
    for (f, attendu) in premiers.iter().zip([34.3, 42.9, 68.6]) {
        assert!((f - attendu).abs() < 0.1, "axiaux {premiers:?}");
    }
    assert!(modes.iter().any(|m| m["type"] == "tangentiel"));
    assert!(modes.iter().any(|m| m["type"] == "oblique"));
    assert!(s["reponse"]["db"].as_array().unwrap().len() > 100);

    let (code, p) = poster(
        l(),
        &format!("/simulations/{id}/correction"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::OK, "{p}");
    assert_eq!(p["estimation"], true);
    assert_eq!(p["zone_id"], 7);
    let gains: Vec<f64> = p["bandes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["gain"].as_f64().unwrap())
        .collect();
    assert!(!gains.is_empty(), "{p}");
    assert!(
        gains.iter().all(|g| *g <= 0.0),
        "relance proposée : {gains:?}"
    );
    assert!(gains.len() <= BANDES_DEFAUT_ESTIMATION, "{gains:?}");
    assert!(p["gain_max_db"].as_f64().unwrap() <= 1e-9);
    assert!(p["coupe_cumulee_db"].as_f64().unwrap() >= -COUPE_DEFAUT_DB_ESTIMATION - 1e-6);
    assert!(
        p["avertissements"][0]
            .as_str()
            .unwrap()
            .starts_with("estimation sans micro")
    );

    // Ce qu'une mesure permet (12 dB, 8 bandes) est refusé sur une estimation.
    let (code, e) = poster(
        l(),
        &format!("/simulations/{id}/correction"),
        serde_json::json!({ "coupe_max_db": 12.0 }),
    );
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert_eq!(e["error"], "correction_refusee");
    // #8332 : en mode expert, les bornes de la mesure sont acceptées, la
    // correction ne fait toujours que couper et se dit expert.
    assert_eq!(s["bornes_correction"]["prudentes"]["bandes_max"], 4);
    assert_eq!(s["bornes_correction"]["expert"]["bandes_max"], 10);
    assert_eq!(s["bornes_correction"]["expert"]["coupe_max_db"], 12.0);
    let (code, p) = poster(
        l(),
        &format!("/simulations/{id}/correction"),
        serde_json::json!({ "coupe_max_db": 12.0, "bandes_max": 10, "expert": true }),
    );
    assert_eq!(code, StatusCode::OK, "{p}");
    assert_eq!(p["expert"], true);
    assert!(p["gain_max_db"].as_f64().unwrap() <= 1e-9);
    assert!(p["coupe_cumulee_db"].as_f64().unwrap() >= -12.0 - 1e-6);
    assert!(
        p["avertissements"][1]
            .as_str()
            .unwrap()
            .starts_with("mode expert")
    );
    let (code, e) = poster(
        l(),
        &format!("/simulations/{id}/correction"),
        serde_json::json!({ "coupe_max_db": 13.0, "expert": true }),
    );
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (code, _) = poster(
        l(),
        "/simulations/sim-999/correction",
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, e) = poster(
        l(),
        "/simulations",
        serde_json::json!({
            "piece": { "longueur_m": 5.0, "largeur_m": 4.0, "hauteur_m": 2.5 },
            "auditeur": { "x": 9.0, "y": 1.0, "z": 1.0 },
            "enceintes": [ { "x": 1.0, "y": 1.0, "z": 1.0 } ],
        }),
    );
    assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(e["error"], "simulation_invalide");
    assert!(
        b.hote.journal().profils.is_empty(),
        "une proposition n'écrit rien"
    );

    // Sans zone : le profil la demande.
    let (_, s2) = poster(l(), "/simulations", piece(None));
    let id2 = s2["id"].as_str().unwrap();
    let (code, e) = poster(
        l(),
        &format!("/simulations/{id2}/correction/profil"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(e["error"], "zone_requise");

    let (code, r) = poster(
        l(),
        &format!("/simulations/{id}/correction/profil"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::CREATED, "{r}");
    assert_eq!(r["estimation"], true);
    assert_eq!(r["appliquer"], "/api/v1/room-correction/profiles/7/apply");
    let j = b.hote.journal();
    assert_eq!(j.profils.len(), 1);
    assert_eq!(j.profils[0].zone_id, "7");
    assert_eq!(j.profils[0].name, format!("Estimation {id}"));
    // D1 (TESS) : une estimation C5 enregistre son origine.
    assert_eq!(j.profils[0].origin.as_deref(), Some("estimation"));
    assert!(j.profils[0].filters.iter().all(|f| f.gain_db <= 0.0));
    assert!(j.joues.is_empty(), "une simulation a joué quelque chose");
}

// ── C4 (lot TESS T10a) : mesure à plusieurs positions ──────────────────────

/// Un client HTTP Premium sur le contrôleur du banc.
struct Client {
    app: axum::Router,
    rt: tokio::runtime::Runtime,
    prefixe: &'static str,
}

/// Le préfixe et la limite de corps globale de l'API de l'hôte
/// (`DefaultBodyLimit::max(50 Mio)` dans `tune-server/src/routes/mod.rs`).
const PREFIXE_HOTE: &str = "/api/v1/ext/calibration";
const LIMITE_HOTE: usize = 50 * 1024 * 1024;

impl Client {
    fn new(b: &Banc) -> Self {
        Self::monte(b, false)
    }

    /// Le routeur du greffon monté comme l'hôte le monte : sous son préfixe,
    /// derrière la limite de corps globale de 50 Mio.
    fn comme_l_hote(b: &Banc) -> Self {
        Self::monte(b, true)
    }

    fn monte(b: &Banc, hote: bool) -> Self {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let licence = Arc::new(tune_core::license::LicenseManager::new(Arc::new(db)));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(licence.set_account_premium(true, None));
        let app = tune_calibration_greffon::routes::router(
            tune_calibration_greffon::routes::EtatRoutes {
                controleur: b.controleur.clone(),
                license: Some(licence),
            },
        );
        if hote {
            let app = axum::Router::new()
                .nest(PREFIXE_HOTE, app)
                .layer(axum::extract::DefaultBodyLimit::max(LIMITE_HOTE));
            Self {
                app,
                rt,
                prefixe: PREFIXE_HOTE,
            }
        } else {
            Self {
                app,
                rt,
                prefixe: "",
            }
        }
    }

    fn appel(
        &self,
        methode: &str,
        chemin: &str,
        corps: Option<serde_json::Value>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method(methode)
            .uri(format!("{}{chemin}", self.prefixe))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                corps.map(|c| c.to_string()).unwrap_or_default(),
            ))
            .unwrap();
        let app = self.app.clone();
        self.rt.block_on(async move {
            let r = app.oneshot(req).await.unwrap();
            let code = r.status();
            let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                code,
                serde_json::from_slice(&octets).unwrap_or(serde_json::Value::Null),
            )
        })
    }
}

fn courbe(
    freqs: &serde_json::Value,
    db: &serde_json::Value,
) -> tune_calibration::analysis::LogResponse {
    tune_calibration::analysis::LogResponse {
        freqs: serde_json::from_value(freqs.clone()).unwrap(),
        magnitude_db: serde_json::from_value(db.clone()).unwrap(),
    }
}

fn mesurer_la_position(
    b: &Banc,
    c: &Client,
    session: &str,
    piece: &RoomSim,
    gain_micro: f64,
) -> (usize, String) {
    use axum::http::StatusCode;
    // Le micro change de place : la pièce qu'il entend change.
    b.boucle
        .changer_piece(PieceEnDirect::depuis(piece, gain_micro));
    let (code, v) = c.appel(
        "POST",
        &format!("/sessions/{session}/positions"),
        Some(serde_json::json!({ "entree": MICRO, "duree_s": 2.0 })),
    );
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(120)),
        Etat::Terminee
    );
    (v["position"].as_u64().unwrap() as usize, id)
}

/// La preuve de T10a, de bout en bout par les routes : cinq positions
/// mesurées (balayage, capture simulée, déconvolution), moyenne pondérée,
/// position centrale prépondérante. L'écart-type des creux de la moyenne
/// est nettement plus bas que celui de la seule place d'écoute. Une sixième
/// position aberrante (micro sous un coussin, −14 dB) est REJETÉE et la
/// moyenne reste exactement celle des cinq. Puis la correction (défauts
/// spatiaux) et l'A/B.
#[test]
fn cinq_positions_reduisent_les_creux_et_l_aberrante_est_rejetee() {
    use axum::http::StatusCode;
    use tune_calibration::positions::{creux_max_db, ecart_type_creux_db, ecart_type_db};
    use tune_calibration::sim::POSITIONS_SIMULEES;

    let base = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&base, 1.0, Some(63.0), "positions");
    let c = Client::new(&b);

    // Bornes : 3 à 9 positions, centrale dans la session.
    let (code, e) = c.appel(
        "POST",
        "/zones/7/sessions",
        Some(serde_json::json!({ "positions": 2 })),
    );
    assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
    let (code, e) = c.appel(
        "POST",
        "/zones/7/sessions",
        Some(serde_json::json!({ "positions": 5, "centrale": 5 })),
    );
    assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");

    let (code, s) = c.appel(
        "POST",
        "/zones/7/sessions",
        Some(serde_json::json!({ "positions": 6, "ponderation": { "poids_central": 2.0 } })),
    );
    assert_eq!(code, StatusCode::CREATED, "{s}");
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(s["version"], 2);

    let mut mesures = Vec::new();
    for (k, u) in POSITIONS_SIMULEES[..5].iter().enumerate() {
        let (position, id) = mesurer_la_position(&b, &c, &sid, &base.a_la_position(*u), 1.0);
        assert_eq!(position, k);
        mesures.push(id);
        let (_, v) = c.appel("GET", &format!("/sessions/{sid}"), None);
        if k + 1 < 3 {
            assert!(v["moyenne"].is_null(), "moyenne avant 3 positions : {v}");
            assert!(v["moyenne_indisponible"].is_string());
        }
    }
    let (code, v) = c.appel("GET", &format!("/sessions/{sid}"), None);
    assert_eq!(code, StatusCode::OK);
    let moy = &v["moyenne"];
    assert_eq!(moy["positions_retenues"], 5, "{moy}");
    let cinq = courbe(&moy["freqs_hz"], &moy["db"]);
    let (_, r0) = c.appel("GET", &format!("/mesures/{}/resultat", mesures[0]), None);
    let une = courbe(&r0["reponse"]["freqs_hz"], &r0["reponse"]["db"]);

    let (lo, hi) = (30.0, 300.0);
    let (creux_1, creux_5) = (
        ecart_type_creux_db(&une, lo, hi),
        ecart_type_creux_db(&cinq, lo, hi),
    );
    let (et_1, et_5) = (ecart_type_db(&une, lo, hi), ecart_type_db(&cinq, lo, hi));
    let (max_1, max_5) = (creux_max_db(&une, lo, hi), creux_max_db(&cinq, lo, hi));
    println!(
        "T10a bout en bout {lo}-{hi} Hz | écart-type des creux : 1 position {creux_1:.2} dB, 5 positions {creux_5:.2} dB | \
         écart-type {et_1:.2} → {et_5:.2} dB | creux max {max_1:.2} → {max_5:.2} dB | verdicts {}",
        moy["verdicts"]
    );
    assert!(
        creux_5 < 0.7 * creux_1,
        "écart-type des creux : {creux_1:.2} dB en un point, {creux_5:.2} dB sur 5 positions"
    );
    assert!(et_5 < et_1 && max_5 < max_1);

    // Sixième position : micro étouffé, 14 dB plus bas. Rejetée, nommément ;
    // la moyenne est EXACTEMENT celle des cinq.
    let (position, _) = mesurer_la_position(&b, &c, &sid, &base.a_la_position(0.3), 0.2);
    assert_eq!(position, 5);
    let (_, v6) = c.appel("GET", &format!("/sessions/{sid}"), None);
    let verdicts = v6["moyenne"]["verdicts"].as_array().unwrap();
    println!("T10a rejet bout en bout : {}", v6["moyenne"]["verdicts"]);
    let rejetees: Vec<u64> = verdicts
        .iter()
        .filter(|x| x["rejetee"] == true)
        .map(|x| x["index"].as_u64().unwrap())
        .collect();
    assert_eq!(rejetees, vec![5], "{verdicts:?}");
    assert!(verdicts[5]["motif"].as_str().unwrap().contains("Refaire"));
    assert_eq!(v6["moyenne"]["db"], moy["db"], "la position rejetée a pesé");

    // Correction sur la moyenne : défauts d'une moyenne spatiale, pas
    // d'avertissement « mesure en un point » au-delà de Schroeder.
    let (code, p) = c.appel(
        "POST",
        &format!("/sessions/{sid}/correction"),
        Some(serde_json::json!({})),
    );
    assert_eq!(code, StatusCode::OK, "{p}");
    assert_eq!(p["zone_hz"][1], 500.0, "{p}");
    assert_eq!(p["bandes_max"], 10);
    let av: Vec<String> = serde_json::from_value(p["avertissements"].clone()).unwrap();
    assert!(
        av.iter().all(|a| !a.contains("mesure en un point")),
        "{av:?}"
    );
    assert!(
        av.iter().any(|a| a.contains("position 5 écartée")),
        "{av:?}"
    );
    let (code, prof) = c.appel(
        "POST",
        &format!("/sessions/{sid}/correction/profil"),
        Some(serde_json::json!({ "nom": "Salon 5 places" })),
    );
    assert_eq!(code, StatusCode::CREATED, "{prof}");
    assert_eq!(b.hote.journal().profils[0].name, "Salon 5 places");

    // A/B : la place d'écoute seule contre la moyenne des positions.
    let (code, ab) = c.appel(
        "POST",
        "/comparaisons",
        Some(serde_json::json!({ "avant": mesures[0], "apres": sid, "f_min_hz": 30.0, "f_max_hz": 300.0 })),
    );
    assert_eq!(code, StatusCode::OK, "{ab}");
    assert_eq!(ab["amelioree"], true, "{ab}");
    assert!((ab["avant"]["ecart_type_creux_db"].as_f64().unwrap() - creux_1).abs() < 1e-9);
    assert!((ab["apres"]["ecart_type_creux_db"].as_f64().unwrap() - creux_5).abs() < 1e-9);
    let (code, _) = c.appel(
        "POST",
        "/comparaisons",
        Some(serde_json::json!({ "avant": "inconnue", "apres": sid })),
    );
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(b.hote.journal().joues.len() >= 6);
}

/// Contre-épreuve et compatibilité, sur une vraie mesure simulée :
/// * une ancienne mesure mono-position (le document de
///   `GET /mesures/{id}/resultat`, format v1, sans `version`) se relit comme
///   une session d'une position, SANS PERTE : son document v1 rendu est
///   identique au document lu, sa moyenne est exactement sa réponse, et la
///   correction proposée est la même que sur la mesure d'origine ;
/// * cinq positions identiques donnent exactement la courbe d'une seule ;
/// * un export v2 se relit à l'identique ; un format plus récent est refusé
///   nommément.
#[test]
fn anciennes_mesures_relues_sans_perte_et_positions_identiques() {
    use axum::http::StatusCode;
    use tune_calibration_greffon::session::Session;

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "sessions-v1");
    let c = Client::new(&b);
    let id = b.controleur.lancer(demande(7)).expect("lancée");
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(60)),
        Etat::Terminee
    );
    let (_, v1) = c.appel("GET", &format!("/mesures/{id}/resultat"), None);
    assert!(v1.get("version").is_none());
    // Le document passe par du texte, comme un fichier gardé par le client.
    let v1: serde_json::Value = serde_json::from_str(&v1.to_string()).unwrap();

    let (code, imp) = c.appel("POST", "/sessions/import", Some(v1.clone()));
    assert_eq!(code, StatusCode::CREATED, "{imp}");
    assert_eq!(imp["version_lue"], 1);
    assert_eq!(imp["positions_prevues"], 1);
    let sid = imp["id"].as_str().unwrap().to_string();
    assert_eq!(
        imp["moyenne"]["db"], v1["reponse"]["db"],
        "moyenne d'une position"
    );
    let (_, export) = c.appel("GET", &format!("/sessions/{sid}/export"), None);
    let s: Session = serde_json::from_value(export.clone()).unwrap();
    assert_eq!(
        s.positions[0].document_v1(s.zone_id).unwrap(),
        v1,
        "perte à la relecture v1"
    );
    let (_, p_mesure) = c.appel(
        "POST",
        &format!("/mesures/{id}/correction"),
        Some(serde_json::json!({})),
    );
    let (_, p_session) = c.appel(
        "POST",
        &format!("/sessions/{sid}/correction"),
        Some(serde_json::json!({})),
    );
    // La relecture est exacte au niveau du document (ci-dessus). Entre la
    // mesure EN MÉMOIRE et sa relecture depuis du texte, serde_json (sans
    // `float_roundtrip`) peut décaler un flottant d'un ULP : l'ajustement de
    // l'égaliseur, itératif, peut alors bouger d'un cheveu. Même nombre de
    // bandes, mêmes fréquences à 1 % près, gains à 0,1 dB près.
    let r_memoire = b.controleur.mesure(&id).unwrap().resultat().unwrap();
    let relu: Vec<f64> = serde_json::from_value(v1["reponse"]["db"].clone()).unwrap();
    let ulp = r_memoire
        .reponse
        .db
        .iter()
        .zip(&relu)
        .filter(|(a, b)| a != b)
        .count();
    println!(
        "T10a relecture v1 : {ulp} valeur(s) sur {} décalée(s) d'un ULP par le texte",
        relu.len()
    );
    let bm = p_mesure["bandes"].as_array().unwrap();
    let bs = p_session["bandes"].as_array().unwrap();
    assert_eq!(bm.len(), bs.len(), "{bm:?} / {bs:?}");
    for (x, y) in bm.iter().zip(bs) {
        let f = |v: &serde_json::Value, k: &str| v[k].as_f64().unwrap();
        assert!(
            (f(x, "freq") / f(y, "freq") - 1.0).abs() < 0.01,
            "{x} / {y}"
        );
        assert!((f(x, "gain") - f(y, "gain")).abs() < 0.1, "{x} / {y}");
    }

    // v2 relu à l'identique (hors id, réattribué si pris).
    let (code, re) = c.appel("POST", "/sessions/import", Some(export.clone()));
    assert_eq!(code, StatusCode::CREATED, "{re}");
    let (_, export2) = c.appel(
        "GET",
        &format!("/sessions/{}/export", re["id"].as_str().unwrap()),
        None,
    );
    let mut a = export.clone();
    let mut z = export2.clone();
    a["id"] = serde_json::Value::Null;
    z["id"] = serde_json::Value::Null;
    assert_eq!(a, z);

    // Cinq positions identiques : exactement la courbe d'une seule.
    let mut cinq = export.clone();
    cinq["positions_prevues"] = 5.into();
    cinq["id"] = "cinq-identiques".into();
    let p0 = export["positions"][0].clone();
    cinq["positions"] = (0..5)
        .map(|k| {
            let mut p = p0.clone();
            p["index"] = k.into();
            p
        })
        .collect::<Vec<_>>()
        .into();
    let (code, v) = c.appel("POST", "/sessions/import", Some(cinq));
    assert_eq!(code, StatusCode::CREATED, "{v}");
    assert_eq!(v["moyenne"]["positions_retenues"], 5);
    assert_eq!(v["moyenne"]["db"], v1["reponse"]["db"], "5 identiques ≠ 1");

    // Un format plus récent : refus nommé, rien de lu à moitié.
    let mut v3 = export.clone();
    v3["version"] = 3.into();
    let (code, e) = c.appel("POST", "/sessions/import", Some(v3));
    assert_eq!(code, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["error"], "unsupported_version");
    let (code, e) = c.appel(
        "POST",
        "/sessions/import",
        Some(serde_json::json!({ "version": 2 })),
    );
    assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
}

/// Un résultat mono-position plausible, fabriqué sans mesure.
fn resultat_fabrique() -> serde_json::Value {
    use tune_calibration_greffon::mesure::{Reponse, Resultat, ResumeRi};
    let freqs_hz = tune_calibration::analysis::LogResponse::grid(20.0, 20_000.0, 48);
    let db = freqs_hz
        .iter()
        .map(|f| -20.0 + (f / 50.0).ln().sin())
        .collect();
    serde_json::to_value(Resultat {
        frequence: FREQUENCE_MESURE,
        niveau_joue_dbfs: -18.0,
        salves: Vec::new(),
        ri: ResumeRi {
            retard_ms: 12.0,
            crete_dbfs: -25.0,
            rsb_db: 60.0,
            trames: 72_000,
            crete_a_la_trame: 576,
        },
        reponse: Reponse {
            lissage: "1/12 octave".into(),
            freqs_hz,
            db,
        },
        rt60: Vec::new(),
        etalonnage: None,
        traitements_zone: TraitementsDeZone::default(),
        avertissements: Vec::new(),
        duree_analyse_ms: 1,
    })
    .unwrap()
}

/// Relecture de #9026, point 3a : l'import est BORNÉ. Routeur monté comme
/// par l'hôte (préfixe, limite globale de 50 Mio) :
/// * un corps de 5 Mio est refusé (413) par la limite propre à la route,
///   alors que la limite globale l'aurait laissé passer ;
/// * une position trop longue, inégale, non croissante, hors bornes
///   physiques (±1e300, niveau joué +40 dBFS) est refusée (400) ;
/// * une session v2 de 2 positions est refusée (1 ou 3 à 9) ;
/// * un id importé `m…` ne masque pas la mesure du même nom ;
/// * un v1 sans `zone_id` ne vise aucune zone pour le profil (400).
#[test]
fn l_import_de_session_est_borne() {
    use axum::http::StatusCode;

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some(63.0), "import-borne");
    let c = Client::comme_l_hote(&b);
    let v1 = resultat_fabrique();
    let mut avec_zone = v1.clone();
    avec_zone["zone_id"] = 7.into();
    avec_zone["id"] = "m1-1".into();
    let (code, e) = c.appel("POST", "/sessions/import", Some(avec_zone.clone()));
    assert_eq!(code, StatusCode::CREATED, "document plausible refusé : {e}");

    // 1. Taille : 5 Mio de JSON valide, sous la limite globale de l'hôte.
    let mut gros = avec_zone.clone();
    gros["avertissements"] = serde_json::json!(vec!["x".repeat(1000); 5 * 1024]);
    let (code, e) = c.appel("POST", "/sessions/import", Some(gros));
    assert_eq!(
        code,
        StatusCode::PAYLOAD_TOO_LARGE,
        "corps de 5 Mio accepté par l'import : {e}"
    );

    // 2. Bornes de chaque position.
    let refuse = |nom: &str, doc: serde_json::Value| {
        let (code, e) = c.appel("POST", "/sessions/import", Some(doc));
        assert_eq!(code, StatusCode::BAD_REQUEST, "{nom} accepté : {e}");
        assert_eq!(e["error"], "session_invalide", "{nom} : {e}");
    };
    let mut d = avec_zone.clone();
    d["reponse"]["freqs_hz"] = serde_json::json!((1..=5000).map(|i| i as f64).collect::<Vec<_>>());
    d["reponse"]["db"] = serde_json::json!(vec![0.0; 5000]);
    refuse("5000 points", d);
    let mut d = avec_zone.clone();
    d["reponse"]["db"].as_array_mut().unwrap().pop();
    refuse("longueurs inégales", d);
    let mut d = avec_zone.clone();
    d["reponse"]["freqs_hz"][3] = d["reponse"]["freqs_hz"][2].clone();
    refuse("fréquences non croissantes", d);
    let mut d = avec_zone.clone();
    d["reponse"]["db"][10] = 1e300.into();
    refuse("niveau 1e300 dB", d);
    let mut d = avec_zone.clone();
    d["niveau_joue_dbfs"] = 40.0.into();
    refuse("niveau joué +40 dBFS", d);
    let mut d = avec_zone.clone();
    d["salves"] = serde_json::json!(vec![
        serde_json::json!({"niveau_joue_dbfs": -30.0, "crete_captee_dbfs": -40.0, "rsb_db": 30.0, "spl_db": null, "decision": "step_up"});
        100
    ]);
    refuse("100 salves", d);

    // 3. Nombre de positions d'une session v2 : 1 ou 3 à 9.
    let (_, s) = c.appel("POST", "/sessions/import", Some(avec_zone.clone()));
    let (_, export) = c.appel(
        "GET",
        &format!("/sessions/{}/export", s["id"].as_str().unwrap()),
        None,
    );
    let mut deux = export.clone();
    deux["positions_prevues"] = 2.into();
    let mut p1 = export["positions"][0].clone();
    p1["index"] = 1.into();
    deux["positions"].as_array_mut().unwrap().push(p1);
    refuse("session v2 de 2 positions", deux);

    // 4. Un id importé `m…` ne peut pas masquer une mesure.
    let mut masque = export.clone();
    masque["id"] = "m1-1".into();
    let (code, v) = c.appel("POST", "/sessions/import", Some(masque));
    assert_eq!(code, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap();
    assert!(id.starts_with('s'), "id importé gardé tel quel : {id}");

    // 5. Un v1 sans `zone_id` ne vise aucune zone.
    let (code, v) = c.appel("POST", "/sessions/import", Some(v1.clone()));
    assert_eq!(code, StatusCode::CREATED, "{v}");
    assert!(v["zone_id"].is_null(), "{v}");
    let sid = v["id"].as_str().unwrap().to_string();
    let (code, e) = c.appel(
        "POST",
        &format!("/sessions/{sid}/correction/profil"),
        Some(serde_json::json!({})),
    );
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "profil sur une session sans zone : {e}"
    );
    assert_eq!(e["error"], "zone_requise");
    assert!(b.hote.journal().profils.is_empty(), "un profil a été écrit");
    // La proposition seule reste possible, sans zone.
    let (code, e) = c.appel(
        "POST",
        &format!("/sessions/{sid}/correction"),
        Some(serde_json::json!({})),
    );
    assert_eq!(code, StatusCode::OK, "{e}");
}

/// #9070 (fil 2770) — l'UMIK-1 s'ouvrait en ALSA direct mais ne remettait
/// aucune trame : la mesure attendait une minute et finissait en « capture
/// trop lente : 0 trames sur 216000 attendues », après avoir joué une salve.
/// Elle échoue désormais en ~4 s, AVANT toute salve, avec un code que
/// l'écran traduit et le périphérique ouvert.
#[test]
fn un_micro_qui_ne_renvoie_rien_arrete_la_mesure_en_quatre_secondes_9070() {
    use std::sync::atomic::Ordering;
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle, Some(50.0));
    let micro = tune_entree_audio::simule::Simulees::avec(MICRO, FREQUENCE_MESURE);
    micro.sans_trames.store(true, Ordering::SeqCst);
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-muet-9070");
    let controleur = Controleur::new(
        micro,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let t0 = Instant::now();
    let id = controleur.lancer(demande(4)).expect("lancée");
    let etat = attendre_fin(&controleur, &id, Duration::from_secs(20));
    let duree = t0.elapsed();
    let Etat::Erreur { message } = etat else {
        panic!("#9070 : erreur attendue, obtenu {etat:?}");
    };
    assert!(
        message.contains("ne renvoie aucun son") && message.contains(MICRO),
        "{message}"
    );
    assert!(
        duree < Duration::from_secs(10),
        "#9070 : la mesure a attendu {duree:?} un micro muet (attendu ~4 s)"
    );
    let av = controleur.mesure(&id).unwrap().avancement();
    assert_eq!(av.code, Some("micro_muet"));
    assert_eq!(
        av.peripherique.as_deref(),
        Some(format!("simule:{MICRO}").as_str())
    );
    let j = hote.journal();
    assert!(
        j.joues.is_empty(),
        "une salve a été jouée vers un micro muet"
    );
    assert!(j.volumes.is_empty(), "le volume de la zone a été touché");
}
