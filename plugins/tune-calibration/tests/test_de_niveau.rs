//! rc14 (Didier) — le test de niveau du micro avant la mesure, sur pièce
//! simulée : bruit de fond, bruit rose joué, verdict ; l'écoute seule et son
//! arrêt tout seul ; une session par entrée ; la zone orpheline refusée ; et,
//! sur le VRAI orchestrateur, un signal de test qui ne passe ni par la file
//! ni par l'historique.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tune_calibration::sim::RoomSim;
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::generateur::Voies;
use tune_calibration_greffon::hote::{
    ElementBalayage, HoteZone, SortieInactive, TraitementsDeZone,
};
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE};
use tune_calibration_greffon::niveau::{
    DemandeNiveau, EtatTest, FournisseurNiveau, Jugement, RefusNiveau, ZoneDeTest,
};
use tune_calibration_greffon::niveau::{ElementDeTest, NIVEAU_REFERENCE_DBFS, RSB_MIN_DB};
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_core::source_pcm::FournisseurPcm;
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "UMIK-1 simulé";
const VITESSE: f64 = 8.0;
const ZONE: i64 = 7;
/// Une zone à elle : le contournement des traitements est un état global,
/// et les autres essais tiennent la zone 7 en même temps.
const ZONE_BON: i64 = 701;
const ZONE_PANIQUE: i64 = 702;
const ZONE_RENDU: i64 = 703;

/// La zone simulée du test de niveau : elle ouvre le signal par le
/// fournisseur, comme l'orchestrateur, et le dépose dans la boucle que le
/// micro capte à travers la pièce.
struct ZoneSimulee {
    fournisseur: Arc<FournisseurNiveau>,
    boucle: Arc<Boucle>,
    /// La zone sort en ASIO et, comme on le soupçonne chez Didier, l'entrée
    /// Windows de la même interface cesse de capter dès que la zone joue.
    asio: bool,
    /// Relecture m3 — la zone joue de la musique.
    en_lecture: AtomicBool,
    /// Relecture m5 — `jouer_signal` panique (le fil du test aussi).
    panique: AtomicBool,
    vivant: Mutex<Option<Arc<AtomicBool>>>,
    journal: Mutex<Vec<String>>,
    contournee_au_jeu: Mutex<Vec<bool>>,
}

impl ZoneDeTest for ZoneSimulee {
    fn jouer_signal(&self, zone_id: i64, e: &ElementDeTest) -> Result<(), String> {
        if self.panique.load(Ordering::SeqCst) {
            panic!("panique simulée du fil de test");
        }
        self.contournee_au_jeu
            .lock()
            .unwrap()
            .push(tune_core::audio::mesure_nue::active(zone_id));
        self.journal.lock().unwrap().push(format!("jouer {}", e.id));
        let flux = self.fournisseur.ouvrir(&e.id, 0)?;
        if self.asio {
            self.boucle.fige.store(true, Ordering::SeqCst);
        }
        let vivant = Arc::new(AtomicBool::new(true));
        *self.vivant.lock().unwrap() = Some(vivant.clone());
        let boucle = self.boucle.clone();
        let mut lecteur = flux.lecteur;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 6 * 480];
            while vivant.load(Ordering::SeqCst) {
                let n = match lecteur.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let ech: Vec<f64> = buf[..n]
                    .chunks_exact(6)
                    .map(|t| (i32::from_le_bytes([0, t[0], t[1], t[2]]) >> 8) as f64 / 8_388_608.0)
                    .collect();
                boucle.deposer(&ech, &vivant);
            }
        });
        Ok(())
    }

    fn sortie_asio(&self, _zone_id: i64) -> bool {
        self.asio
    }

    fn zone_en_lecture(&self, _zone_id: i64) -> bool {
        self.en_lecture.load(Ordering::SeqCst)
    }

    fn arreter_signal(&self, _zone_id: i64, id: &str) {
        if let Some(v) = self.vivant.lock().unwrap().take() {
            v.store(false, Ordering::SeqCst);
        }
        self.journal.lock().unwrap().push(format!("arreter {id}"));
    }
}

/// L'hôte simulé, plus une sortie inactive au besoin (fil 3094).
struct HoteOrphelin {
    simule: Arc<HoteSimule>,
    inactive: Option<SortieInactive>,
    /// Relecture m1 — la garde de contournement tenait-elle la zone à chaque
    /// réglage du volume ?
    contournee_au_volume: Mutex<Vec<bool>>,
}

impl HoteZone for HoteOrphelin {
    fn volume(&self, zone_id: i64) -> Option<f64> {
        self.simule.volume(zone_id)
    }
    fn regler_volume(&self, zone_id: i64, volume_pct: f64) -> Result<(), String> {
        self.contournee_au_volume
            .lock()
            .unwrap()
            .push(tune_core::audio::mesure_nue::active(zone_id));
        self.simule.regler_volume(zone_id, volume_pct)
    }
    fn jouer(&self, zone_id: i64, element: ElementBalayage) -> Result<(), String> {
        self.simule.jouer(zone_id, element)
    }
    fn arreter(&self, zone_id: i64) {
        self.simule.arreter(zone_id)
    }
    fn traitements(&self, zone_id: i64) -> TraitementsDeZone {
        self.simule.traitements(zone_id)
    }
    fn sortie_inactive(&self, _zone_id: i64) -> Option<SortieInactive> {
        self.inactive.clone()
    }
    fn enregistrer_profil_piece(
        &self,
        profil: &tune_core::room_correction::RoomProfile,
    ) -> Result<(), String> {
        self.simule.enregistrer_profil_piece(profil)
    }
}

struct Banc {
    controleur: Arc<Controleur>,
    hote: Arc<HoteSimule>,
    orphelin: Arc<HoteOrphelin>,
    zone: Arc<ZoneSimulee>,
    micro: Arc<MicroSimule>,
    boucle: Arc<Boucle>,
    _dossier: tune_core::test_scratch::ScratchDir,
}

fn banc(sim: &RoomSim, gain_micro: f64, inactive: Option<&str>, nom: &str) -> Banc {
    banc_asio(sim, gain_micro, inactive, false, nom)
}

fn banc_asio(
    sim: &RoomSim,
    gain_micro: f64,
    inactive: Option<&str>,
    asio: bool,
    nom: &str,
) -> Banc {
    let boucle = Boucle::new(PieceEnDirect::depuis(sim, gain_micro), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(50.0));
    // Fil 3094 (#9528) — la zone orpheline vient de l'hôte de la mesure.
    let orphelin = Arc::new(HoteOrphelin {
        simule: hote.clone(),
        inactive: inactive.map(|d| SortieInactive {
            device_id: d.to_string(),
            porteuse: Some((3, "Salon (DAC)".into())),
        }),
        contournee_au_volume: Mutex::default(),
    });
    let hote_zone: Arc<dyn HoteZone> = orphelin.clone();
    let micro = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle: boucle.clone(),
        demarrages: Default::default(),
    });
    let peripheriques: Arc<dyn Peripheriques> = micro.clone();
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-niveau-{nom}"));
    let controleur = Controleur::new(
        peripheriques,
        hote_zone,
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let zone = Arc::new(ZoneSimulee {
        fournisseur: controleur.niveaux.fournisseur.clone(),
        boucle: boucle.clone(),
        asio,
        en_lecture: AtomicBool::new(false),
        panique: AtomicBool::new(false),
        vivant: Mutex::default(),
        journal: Mutex::default(),
        contournee_au_jeu: Mutex::default(),
    });
    controleur.niveaux.brancher(zone.clone());
    Banc {
        controleur,
        hote,
        orphelin,
        zone,
        micro,
        boucle,
        _dossier: dossier,
    }
}

fn attendre_fin(c: &Controleur, id: &str, delai: Duration) -> EtatTest {
    let t0 = Instant::now();
    loop {
        let e = c.test_niveau(id).expect("test connu").etat();
        if e.finale() {
            return e;
        }
        assert!(t0.elapsed() < delai, "le test ne finit pas : {e:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn demarrages(b: &Banc) -> usize {
    *b.micro.demarrages.lock().unwrap()
}

/// Le salon sec simulé, micro au gain nominal : bruit de fond, signal,
/// verdict « bon » ; la zone a joué SOUS contournement et au volume de
/// mesure, puis a été arrêtée et rendue (volume initial), le signal désarmé.
/// Puis la mesure, sur le même banc, passe : le verdict l'annonçait.
#[test]
fn le_verdict_annonce_la_mesure() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "bon");
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE_BON, MICRO))
        .expect("lancé");
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let a = b.controleur.test_niveau(&id).unwrap().avancement();
    let v = a.verdict.clone().expect("verdict");
    assert_eq!(v.niveau_dbc, None);
    assert_eq!(v.jugement, Jugement::Bon, "{v:?}");
    assert!(v.rsb_db >= RSB_MIN_DB, "{v:?}");
    let bruit = a.bruit.expect("bruit de fond");
    assert!(bruit.rms_dbfs < -70.0, "bruit de fond {bruit:?}");
    assert!(v.signal.crete_dbfs < -3.0, "{v:?}");
    assert!(
        a.vumetre
            .is_some_and(|m| m.crete_max_dbfs >= v.signal.crete_dbfs - 0.01)
    );
    // La zone : contournée au jeu, arrêtée, volume forcé puis rendu.
    assert_eq!(*b.zone.contournee_au_jeu.lock().unwrap(), vec![true]);
    assert!(!tune_core::audio::mesure_nue::active(ZONE_BON));
    assert_eq!(
        *b.zone.journal.lock().unwrap(),
        vec![format!("jouer {id}"), format!("arreter {id}")]
    );
    assert_eq!(b.hote.journal().volumes, vec![40.0, 50.0]);
    assert_eq!(b.controleur.niveaux.fournisseur.armes(), 0);
    // Rien dans la file de la mesure : l'hôte de la mesure n'a rien joué.
    assert!(b.hote.journal().joues.is_empty());
    assert_eq!(demarrages(&b), 1);
    eprintln!("verdict bon : {v:?}");

    let mut d = Demande::defaut(ZONE_BON, MICRO);
    d.duree_s = 2.0;
    let m = b.controleur.lancer(d).expect("mesure lancée");
    let t0 = Instant::now();
    let fin = loop {
        let e = b.controleur.mesure(&m).unwrap().etat();
        if e.finale() {
            break e;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "{e:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(fin, Etat::Terminee);
    // Et sans jouer plus fort que le test.
    let r = b.controleur.mesure(&m).unwrap().resultat().unwrap();
    assert!(
        r.niveau_joue_dbfs <= NIVEAU_REFERENCE_DBFS,
        "{:?}",
        r.salves
    );
}

/// Micro trop chaud (gain ×40, comme le banc d'écrêtage de la mesure) :
/// « trop fort », avec la valeur captée.
#[test]
fn un_micro_trop_chaud_est_juge_trop_fort() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 40.0, None, "fort");
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO))
        .unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
    assert_eq!(v.jugement, Jugement::TropFort, "{v:?}");
    assert!(v.signal.crete_dbfs >= -3.0);
    // Relecture m1 — la saturation est vue pendant la montée : coupure en
    // fondu, sans attendre la référence.
    assert!(v.coupe, "{v:?}");
    assert_eq!(b.hote.journal().volumes.last(), Some(&50.0));
}

/// Relecture m1 — avec un étalonnage, la pression au micro est plafonnée
/// à 90 dB SPL comme pour la mesure : passé le plafond pendant la montée,
/// le signal est coupé et le verdict le dit. Un micro moins sensible (même
/// pièce, pression plus basse) passe.
#[test]
fn le_plafond_spl_coupe_le_signal() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "spl");
    // SPL = dBFS + 112 − sens : le palier de référence (≈ −17,6 dBFS de
    // crête) donne ≈ 94 dB SPL avec un facteur 0, ≈ 84 avec +10.
    let fort = "\"Sens Factor =0.0dB, SERNO: 1\"\n10 0.0\n20000 0.0\n";
    let doux = "\"Sens Factor =10.0dB, SERNO: 2\"\n10 0.0\n20000 0.0\n";
    b.controleur.deposer_etalonnage("fort", fort).unwrap();
    b.controleur.deposer_etalonnage("doux", doux).unwrap();
    let mut d = DemandeNiveau::signal(ZONE, MICRO);
    d.etalonnage = Some("fort".into());
    let id = b.controleur.tester_niveau(d).unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
    assert_eq!(
        (v.jugement, v.motif),
        (Jugement::TropFort, "trop_fort_spl"),
        "{v:?}"
    );
    assert!(v.coupe && v.spl_db.is_some_and(|s| s > 90.0), "{v:?}");
    assert_eq!(v.niveau_dbc, None);
    let mut d = DemandeNiveau::signal(ZONE, MICRO);
    d.etalonnage = Some("doux".into());
    d.gain_relatif_db = Some(-18.0);
    let id = b.controleur.tester_niveau(d).unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
    assert_eq!(v.jugement, Jugement::Bon, "{v:?}");
    assert!(!v.coupe && v.spl_db.is_some_and(|s| s < 90.0), "{v:?}");
    assert!(
        v.niveau_dbc
            .is_some_and(|c| c.is_finite() && c < v.spl_db.unwrap())
    );
    // Un étalonnage inconnu est refusé avant tout.
    let mut d = DemandeNiveau::signal(ZONE, MICRO);
    d.etalonnage = Some("absent".into());
    assert!(matches!(
        b.controleur.tester_niveau(d),
        Err(RefusNiveau::Refus(_))
    ));
}

/// Relecture m3 — une zone qui joue n'est pas coupée par le test : refus
/// clair, rien n'est ouvert ni touché. L'écoute seule, elle, ne touche pas
/// la zone et reste possible.
#[test]
fn une_zone_en_lecture_est_refusee() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "lecture");
    b.zone.en_lecture.store(true, Ordering::SeqCst);
    let r = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO));
    let Err(e @ RefusNiveau::ZoneEnLecture) = r else {
        panic!("{r:?}")
    };
    assert!(
        e.message().contains("Arrêtez la lecture"),
        "{}",
        e.message()
    );
    assert_eq!(demarrages(&b), 0);
    assert!(b.hote.journal().volumes.is_empty());
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.zone_id = Some(ZONE);
    d.duree_s = 1.0;
    let id = b
        .controleur
        .tester_niveau(d)
        .expect("l'écoute seule reste possible");
    attendre_fin(&b.controleur, &id, Duration::from_secs(30));
    assert!(b.zone.journal.lock().unwrap().is_empty());
}

/// Relecture m1 (#9529 avec #9537) — le volume de mesure est posé SOUS la
/// garde de contournement (verrou PURE levé), le volume d'avant est rendu
/// APRÈS elle : en PURE « Forcer à 100 % », le verrou le tient de nouveau.
#[test]
fn le_volume_d_avant_est_rendu_apres_la_garde_de_contournement() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "rendu");
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE_RENDU, MICRO))
        .expect("lancé");
    attendre_fin(&b.controleur, &id, Duration::from_secs(30));
    assert_eq!(b.hote.journal().volumes, vec![40.0, 50.0]);
    assert_eq!(
        *b.orphelin.contournee_au_volume.lock().unwrap(),
        vec![true, false],
        "mesure sous la garde, volume d'avant rendu après elle"
    );
    assert!(!tune_core::audio::mesure_nue::active(ZONE_RENDU));
}

/// Relecture m5 — le fil du test panique : l'état devient « erreur »,
/// l'entrée et la zone sont rendues (volume, contournement), un nouveau test
/// démarre aussitôt.
#[test]
fn une_panique_du_fil_libere_l_entree_et_la_zone() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "panique");
    b.zone.panique.store(true, Ordering::SeqCst);
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE_PANIQUE, MICRO))
        .unwrap();
    let fin = attendre_fin(&b.controleur, &id, Duration::from_secs(30));
    assert!(matches!(fin, EtatTest::Erreur { .. }), "{fin:?}");
    assert_eq!(b.hote.journal().volumes, vec![40.0, 50.0]);
    assert!(!tune_core::audio::mesure_nue::active(ZONE_PANIQUE));
    assert_eq!(b.controleur.niveaux.fournisseur.armes(), 0);
    b.zone.panique.store(false, Ordering::SeqCst);
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 1.0;
    let id2 = b.controleur.tester_niveau(d).expect("entrée libérée");
    attendre_fin(&b.controleur, &id2, Duration::from_secs(30));
}

/// Relecture m6 — deux demandes au même instant (double clic) : une seule
/// passe ; un test et une mesure au même instant : un seul des deux.
#[test]
fn deux_demandes_simultanees_une_seule_passe() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "admission");
    for _ in 0..5 {
        let depart = Arc::new(std::sync::Barrier::new(2));
        let fils: Vec<_> = (0..2)
            .map(|k| {
                let (c, depart) = (b.controleur.clone(), depart.clone());
                std::thread::spawn(move || {
                    depart.wait();
                    let mut d = DemandeNiveau::ecoute(MICRO);
                    d.duree_s = 1.0 + k as f64;
                    c.tester_niveau(d).ok()
                })
            })
            .collect();
        let passes: Vec<String> = fils.into_iter().filter_map(|f| f.join().unwrap()).collect();
        assert_eq!(passes.len(), 1, "{passes:?}");
        attendre_fin(&b.controleur, &passes[0], Duration::from_secs(30));
    }
    // Test et mesure au même instant.
    let depart = Arc::new(std::sync::Barrier::new(2));
    let (c1, d1) = (b.controleur.clone(), depart.clone());
    let test = std::thread::spawn(move || {
        d1.wait();
        let mut d = DemandeNiveau::ecoute(MICRO);
        d.duree_s = 1.0;
        c1.tester_niveau(d).ok()
    });
    let (c2, d2) = (b.controleur.clone(), depart.clone());
    let mesure = std::thread::spawn(move || {
        d2.wait();
        let mut d = Demande::defaut(ZONE, MICRO);
        d.duree_s = 2.0;
        c2.lancer(d).ok()
    });
    let (t, m) = (test.join().unwrap(), mesure.join().unwrap());
    assert!(t.is_some() != m.is_some(), "test {t:?}, mesure {m:?}");
    if let Some(m) = m {
        b.controleur.annuler(&m);
    }
}

/// Didier : la mauvaise voie. Le micro simulé ne capte que sur la voie 1 ;
/// la voie 2 reste muette : « le micro n'entend pas la zone ».
#[test]
fn la_mauvaise_voie_dit_signal_absent() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "voie");
    let mut d = DemandeNiveau::signal(ZONE, MICRO);
    d.voie_micro = 1;
    let id = b.controleur.tester_niveau(d).unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
    assert_eq!(
        (v.jugement, v.motif),
        (Jugement::TropFaible, "signal_absent"),
        "{v:?}"
    );
    // Une voie qui n'existe pas est refusée avant tout.
    let mut d = DemandeNiveau::signal(ZONE, MICRO);
    d.voie_micro = 2;
    assert!(matches!(
        b.controleur.tester_niveau(d),
        Err(RefusNiveau::Refus(_))
    ));
}

/// Relecture m7 — une pièce bruyante : le signal sort peu du bruit de
/// fond, « faible mais mesurable », et la phrase le dit. La mesure le confirme : au niveau
/// du test, sa salve n'atteint pas le RSB visé, elle doit jouer plus fort.
#[test]
fn une_piece_bruyante_est_jugee_faible_mais_mesurable() {
    let mut sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    sim.noise_floor_dbfs = -36.0;
    let b = banc(&sim, 1.0, None, "bruit");
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO))
        .unwrap();
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
    eprintln!("pièce bruyante : {v:?}");
    assert_eq!(
        (v.jugement, v.motif),
        (Jugement::Faible, "rsb_faible"),
        "{v:?}"
    );
    assert!(v.message.contains("bruit de fond"), "{}", v.message);

    let mut d = Demande::defaut(ZONE, MICRO);
    d.duree_s = 2.0;
    let m = b.controleur.lancer(d).unwrap();
    let t0 = Instant::now();
    while !b.controleur.mesure(&m).unwrap().etat().finale() {
        assert!(t0.elapsed() < Duration::from_secs(90));
        std::thread::sleep(Duration::from_millis(20));
    }
    let salves = b.controleur.mesure(&m).unwrap().avancement().salves;
    let a_la_reference = salves
        .iter()
        .find(|s| s.niveau_joue_dbfs == NIVEAU_REFERENCE_DBFS)
        .expect("la mesure est montée jusqu'au niveau du test");
    assert!(a_la_reference.rsb_db < 50.0, "{salves:?}");
}

/// L'écoute seule : rien n'est joué, le vumètre bouge quand on « parle »
/// (un son déposé devant le micro), et le test s'arrête tout seul à sa
/// durée, l'entrée libérée.
#[test]
fn l_ecoute_seule_montre_le_micro_et_s_arrete_toute_seule() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "ecoute");
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 3.0;
    let id = b.controleur.tester_niveau(d).unwrap();
    // On parle : 0,5 s de son à −20 dBFS devant le micro.
    let t0 = Instant::now();
    while b.controleur.test_niveau(&id).unwrap().etat() != EtatTest::Ecoute {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    let voix: Vec<f64> = (0..24_000)
        .map(|i| 0.1 * (2.0 * std::f64::consts::PI * 300.0 * i as f64 / 48_000.0).sin())
        .collect();
    b.boucle.deposer(&voix, &AtomicBool::new(true));
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    let a = b.controleur.test_niveau(&id).unwrap().avancement();
    assert!(a.verdict.is_none() && a.bruit.is_none());
    let m = a.vumetre.expect("vumètre");
    assert!(m.crete_max_dbfs > -40.0, "le vumètre n'a pas bougé : {m:?}");
    assert!(b.zone.journal.lock().unwrap().is_empty(), "rien n'est joué");
    assert!(b.hote.journal().volumes.is_empty(), "volume non touché");
    // L'entrée est rendue : un nouveau test démarre aussitôt.
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 1.0;
    let id2 = b.controleur.tester_niveau(d).expect("entrée libérée");
    attendre_fin(&b.controleur, &id2, Duration::from_secs(30));
    assert_eq!(demarrages(&b), 2);
}

/// Une seule session par entrée ; la mesure attend ; l'annulation rend
/// l'entrée ; la borne de 30 s tient quoi qu'on demande.
#[test]
fn une_session_par_entree_et_l_annulation_libere() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "unique");
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 600.0;
    let id = b.controleur.tester_niveau(d).unwrap();
    let t = b.controleur.test_niveau(&id).unwrap();
    assert_eq!(t.demande.duree_s, 30.0, "écoute bornée à 30 s");
    assert!(matches!(
        b.controleur.tester_niveau(DemandeNiveau::ecoute(MICRO)),
        Err(RefusNiveau::Occupe(_))
    ));
    assert!(matches!(
        b.controleur
            .tester_niveau(DemandeNiveau::signal(ZONE, MICRO)),
        Err(RefusNiveau::Occupe(_))
    ));
    let e = b
        .controleur
        .lancer(Demande::defaut(ZONE, MICRO))
        .unwrap_err();
    assert!(e.contains("déjà en cours"), "{e}");
    assert!(b.controleur.niveaux.annuler(&id));
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(10)),
        EtatTest::Annulee
    );
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 1.0;
    let id2 = b.controleur.tester_niveau(d).expect("entrée libérée");
    attendre_fin(&b.controleur, &id2, Duration::from_secs(30));
}

/// Fil de Didier (TASCAM US-366 : zone en ASIO, micro en WASAPI sur la même
/// interface) — l'avertissement est là dès le départ, en écoute comme en
/// signal ; si l'entrée cesse de capter quand la zone joue, l'erreur le dit
/// en nommant ASIO, et la zone est rendue.
#[test]
fn une_zone_asio_est_signalee_et_l_entree_coupee_le_dit() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc_asio(&sim, 1.0, None, true, "asio");
    // L'écoute seule, zone nommée : l'avertissement, rien de joué.
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.zone_id = Some(ZONE);
    d.duree_s = 1.0;
    let id = b.controleur.tester_niveau(d).unwrap();
    let a = b.controleur.test_niveau(&id).unwrap().avancement();
    assert!(a.alerte_asio);
    assert!(
        a.avertissements.iter().any(|w| w.contains("ASIO")),
        "{:?}",
        a.avertissements
    );
    assert_eq!(
        attendre_fin(&b.controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee
    );
    assert!(b.zone.journal.lock().unwrap().is_empty());
    // Le signal : l'entrée se tait dès que la zone joue.
    let id = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO))
        .unwrap();
    let fin = attendre_fin(&b.controleur, &id, Duration::from_secs(30));
    let EtatTest::Erreur { message } = fin else {
        panic!("{fin:?}")
    };
    assert!(
        message.contains("ASIO") && message.contains("WASAPI"),
        "{message}"
    );
    let a = b.controleur.test_niveau(&id).unwrap().avancement();
    assert_eq!(a.code, Some("micro_fige"));
    assert!(
        a.bruit.is_some(),
        "le bruit de fond, capté avant, reste lisible"
    );
    assert_eq!(b.hote.journal().volumes, vec![40.0, 50.0]);
    assert_eq!(b.controleur.niveaux.fournisseur.armes(), 0);
}

/// Fil 3094 — une zone sans sortie active est refusée AVANT tout : ni micro
/// ouvert, ni volume forcé, ni signal joué ; le refus nomme l'appareil.
#[test]
fn une_zone_orpheline_est_refusee_avant_tout() {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, Some("local:SmartDX1"), "orpheline");
    let r = b
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO));
    let Err(e @ RefusNiveau::SortieInactive(_)) = r else {
        panic!("{r:?}")
    };
    // #9528 : le refus nomme la zone qui porte l'appareil.
    assert!(e.message().contains("« Salon (DAC) »"), "{}", e.message());
    assert_eq!(demarrages(&b), 0);
    assert!(b.hote.journal().volumes.is_empty());
    assert!(b.zone.journal.lock().unwrap().is_empty());
    // L'écoute seule ne dépend d'aucune zone.
    let mut d = DemandeNiveau::ecoute(MICRO);
    d.duree_s = 1.0;
    assert!(b.controleur.tester_niveau(d).is_ok());
}

/// Les routes : 202 puis l'avancement (vumètre, verdict) ; 409 nommé ;
/// 400 sans zone en mode signal ; 404 inconnu.
#[test]
fn les_routes_du_test_de_niveau() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_calibration_greffon::routes::{EtatRoutes, router};

    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let b = banc(&sim, 1.0, None, "routes");
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let licence = Arc::new(tune_core::license::LicenseManager::new(Arc::new(db)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(licence.set_account_premium(true, None));
    let app = router(EtatRoutes {
        controleur: b.controleur.clone(),
        license: Some(licence),
    });
    let appeler = |methode: &str, chemin: String, corps: serde_json::Value| {
        let app = app.clone();
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
    let (code, e) = appeler(
        "POST",
        "/niveaux".into(),
        serde_json::json!({ "entree": MICRO }),
    );
    assert_eq!(
        (code, e["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("zone_requise"))
    );
    let (code, r) = appeler(
        "POST",
        "/niveaux".into(),
        serde_json::json!({ "mode": "signal", "entree": MICRO, "zone_id": ZONE, "voie_micro": 0 }),
    );
    assert_eq!(code, StatusCode::ACCEPTED, "{r}");
    let id = r["id"].as_str().unwrap().to_string();
    let (code, e) = appeler(
        "POST",
        "/niveaux".into(),
        serde_json::json!({ "mode": "ecoute", "entree": MICRO }),
    );
    assert_eq!(
        (code, e["error"].as_str()),
        (StatusCode::CONFLICT, Some("test_niveau_en_cours"))
    );
    // Suivi par interrogation, comme l'avancement d'une mesure.
    let t0 = Instant::now();
    let fin = loop {
        let (code, a) = appeler("GET", format!("/niveaux/{id}"), serde_json::Value::Null);
        assert_eq!(code, StatusCode::OK);
        if a["etat"] == "terminee" {
            break a;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "{a}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(fin["mode"], "signal");
    assert_eq!(fin["verdict"]["jugement"], "bon", "{fin}");
    assert_eq!(fin["verdict"]["motif"], "bon");
    assert!(fin["verdict"]["signal"]["crete_dbfs"].is_number());
    assert!(fin["verdict"]["plage"]["crete_max_dbfs"].is_number());
    assert!(fin["vumetre"]["rms_dbfs"].is_number());
    assert!(fin["bruit"]["rms_dbfs"].is_number());
    let (code, _) = appeler("GET", "/niveaux/inconnu".into(), serde_json::Value::Null);
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Zone orpheline : la même forme que le refus de la mesure (#9528).
    let b2 = banc(&sim, 1.0, Some("local:SmartDX1"), "routes-orpheline");
    let r = b2
        .controleur
        .tester_niveau(DemandeNiveau::signal(ZONE, MICRO));
    assert!(matches!(r, Err(RefusNiveau::SortieInactive(_))));
}

/// Sur le VRAI orchestrateur (sortie factice) : le signal de test se joue
/// sans toucher la file de la zone, n'arme aucune écoute (historique), et la
/// zone ne l'affiche plus après l'arrêt. Une zone dont l'appareil n'est pas
/// enregistré est dite orpheline.
#[test]
fn le_signal_de_test_ne_passe_ni_par_la_file_ni_par_l_historique() {
    use tokio::sync::Mutex as MutexAsync;
    use tune_calibration_greffon::niveau::{
        DUREE_ANNONCEE_MS, SOURCE_NIVEAU, SignalArme, ZoneOrchestrateur,
    };
    use tune_core::db::backend::DbBackend;
    use tune_core::db::play_queue_repo::PlayQueueRepo;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::http::streamer::AudioStreamer;
    use tune_core::orchestrator::PlaybackOrchestrator;
    use tune_core::outputs::mock::MockOutput;
    use tune_core::outputs::registry::OutputRegistry;
    use tune_core::playback::PlaybackManager;
    use tune_core::streaming::registry::ServiceRegistry;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let orch = Arc::new(PlaybackOrchestrator::new(
        backend.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(MutexAsync::new(ServiceRegistry::new())),
        Arc::new(MutexAsync::new(OutputRegistry::new())),
        Some("127.0.0.1".into()),
    ));
    let zones = ZoneRepo::with_backend(backend.clone());
    let zone_id = zones
        .create("Salon", Some("mock"), Some("mock:salon"))
        .unwrap();
    rt.block_on(async {
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock:salon", "Salon")))
    });
    // La file de l'utilisateur : deux titres d'un service.
    let file = PlayQueueRepo::with_backend(backend.clone());
    let ligne = |id: &str, titre: &str| {
        (
            id.to_string(),
            titre.to_string(),
            "Artiste".to_string(),
            None,
            None,
            200_000,
            Some("qobuz".to_string()),
            None,
            None,
        )
    };
    file.set_streaming_queue(zone_id, &[ligne("q-1", "Titre 1"), ligne("q-2", "Titre 2")])
        .unwrap();
    let lire_file = || {
        file.get_ordered(zone_id)
            .unwrap()
            .into_iter()
            .map(|e| (e.source, e.source_id, e.title))
            .collect::<Vec<_>>()
    };
    let avant = lire_file();
    assert_eq!(avant.len(), 2);

    let fournisseur = Arc::new(FournisseurNiveau::default());
    orch.sources_pcm()
        .inscrire(SOURCE_NIVEAU, fournisseur.clone());
    let zone = ZoneOrchestrateur {
        affichages_avant: Default::default(),
        backend: backend.clone(),
        orchestrator: orch.clone(),
        playback: playback.clone(),
        runtime: rt.handle().clone(),
    };
    // Relecture m3 — la zone, arrêtée, affiche le titre 1 à 42 s.
    let titre_1 = tune_core::playback::NowPlaying {
        title: "Titre 1".into(),
        source: "qobuz".into(),
        source_id: Some("q-1".into()),
        ..Default::default()
    };
    rt.block_on(playback.restore_position(zone_id, 42_000, titre_1));
    assert!(!zone.zone_en_lecture(zone_id));
    let arme = SignalArme::new(vec![0.1; 4_800], Voies::LesDeux, None);
    fournisseur.armer("n1-essai", arme.clone());
    zone.jouer_signal(
        zone_id,
        &ElementDeTest {
            id: "n1-essai".into(),
            format: arme.format,
            duree_ms: DUREE_ANNONCEE_MS,
        },
    )
    .expect("signal joué");
    assert!(
        arme.attendre_ouverture(Duration::from_secs(10)),
        "le flux n'a pas été ouvert"
    );
    let etat = rt.block_on(playback.get_state(zone_id));
    let np = etat.now_playing.clone().expect("la zone joue le signal");
    assert_eq!(np.source, SOURCE_NIVEAU);
    assert!(
        zone.zone_en_lecture(zone_id),
        "la zone joue le signal : un second test serait refusé"
    );
    assert_eq!(lire_file(), avant, "la file a changé pendant le test");
    // Bien au-delà du seuil d'une écoute : rien n'est armé pour l'historique.
    let mut tard = etat.clone();
    tard.position_ms = 15_000;
    assert!(!orch.valider_l_ecoute(&tard), "une écoute a été validée");

    // Relecture m4 — la fin d'un autre test (autre id) ne coupe pas celui-ci.
    zone.arreter_signal(zone_id, "n9-autre");
    assert_eq!(
        rt.block_on(playback.get_state(zone_id))
            .now_playing
            .map(|np| np.source),
        Some(SOURCE_NIVEAU.to_string())
    );
    zone.arreter_signal(zone_id, "n1-essai");
    fournisseur.desarmer("n1-essai");
    // La zone n'affiche plus le signal, mais le titre 1 à sa position.
    let apres = rt.block_on(playback.get_state(zone_id));
    let np = apres.now_playing.expect("le titre d'avant revient");
    assert_eq!(
        (np.source.as_str(), np.title.as_str()),
        ("qobuz", "Titre 1")
    );
    assert_eq!(apres.position_ms, 42_000);
    assert!(!zone.zone_en_lecture(zone_id));
    // Relecture m4 — une musique lancée par un autre client pendant le test
    // n'est pas coupée par la fin du test.
    let autre = tune_core::playback::NowPlaying {
        title: "Musique d'un autre client".into(),
        source: "qobuz".into(),
        source_id: Some("q-9".into()),
        ..Default::default()
    };
    rt.block_on(playback.update_now_playing(zone_id, autre));
    zone.arreter_signal(zone_id, "n1-essai");
    assert_eq!(
        rt.block_on(playback.get_state(zone_id))
            .now_playing
            .map(|np| np.title),
        Some("Musique d'un autre client".to_string())
    );
    assert_eq!(lire_file(), avant, "la file a changé après le test");
    let lignes = backend
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1);
    assert_eq!(lignes, 0, "le signal de test est entré dans l'historique");
}

/// Exploration (non lancée d'office) : pour des bruits de fond croissants,
/// le RSB du test et l'issue de la mesure. Sert à caler `RSB_MIN_DB`.
#[test]
#[ignore]
fn exploration_seuil_rsb() {
    for bruit in [-80.0, -60.0, -50.0, -45.0, -40.0, -35.0, -30.0, -25.0] {
        let mut sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
        sim.noise_floor_dbfs = bruit;
        let b = banc(&sim, 1.0, None, &format!("explo{}", -bruit as i64));
        let id = b
            .controleur
            .tester_niveau(DemandeNiveau::signal(ZONE, MICRO))
            .unwrap();
        attendre_fin(&b.controleur, &id, Duration::from_secs(30));
        let v = b.controleur.test_niveau(&id).unwrap().verdict().unwrap();
        let mut d = Demande::defaut(ZONE, MICRO);
        d.duree_s = 2.0;
        let m = b.controleur.lancer(d).unwrap();
        let t0 = Instant::now();
        let fin = loop {
            let e = b.controleur.mesure(&m).unwrap().etat();
            if e.finale() || t0.elapsed() > Duration::from_secs(90) {
                break e;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let a = b.controleur.mesure(&m).unwrap().avancement();
        let salves: Vec<_> = a
            .salves
            .iter()
            .map(|s| format!("{:.0}:{:.0}", s.niveau_joue_dbfs, s.rsb_db))
            .collect();
        let ri = b
            .controleur
            .mesure(&m)
            .unwrap()
            .resultat()
            .map(|r| r.ri.rsb_db);
        eprintln!(
            "EXPLO bruit {bruit:>5} | test rsb {:>5.1} crete {:>6.1} {:?} | mesure {:?} salves {:?} ri {:?}",
            v.rsb_db, v.signal.crete_dbfs, v.jugement, fin, salves, ri
        );
    }
}

/// rc14 (#9538) — le test de niveau d'un micro à 44,1 ou 96 kHz, face à une
/// zone à 48 kHz : le verdict, le niveau capté et le RSB sont ceux du micro à
/// 48 kHz, et aucun avertissement de fréquence ne sort. Le cadencement du
/// signal sur l'écoute comptée en trames du générateur est vérifié à part
/// (`cadence::tests::l_horloge_d_ecoute_compte_en_trames_du_generateur`) :
/// ce banc, dont le micro attend la zone, ne le distingue pas.
fn tester_a(
    frequence: Option<u32>,
    zone: i64,
    nom: &str,
) -> tune_calibration_greffon::niveau::AvancementNiveau {
    use tune_calibration_greffon::simulation::micro_a_cadence::MicroACadence;
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    // L'anneau de 2 s d'une sortie locale (#9066) : seul le cadencement sur
    // l'écoute empêche le signal d'y prendre de l'avance sur le micro.
    let boucle = Boucle::avec_tampon(
        PieceEnDirect::depuis(&sim, 1.0),
        VITESSE,
        2 * FREQUENCE_MESURE as usize,
    );
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(50.0));
    let peripheriques: Arc<dyn Peripheriques> = match frequence {
        None => Arc::new(MicroSimule {
            nom: MICRO.into(),
            frequence: FREQUENCE_MESURE,
            boucle: boucle.clone(),
            demarrages: Default::default(),
        }),
        Some(frequence) => Arc::new(MicroACadence {
            nom: MICRO.into(),
            frequence,
            derive_ppm: 0.0,
            frequence_zone: FREQUENCE_MESURE,
            boucle: boucle.clone(),
        }),
    };
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-niveau-{nom}"));
    let controleur = Controleur::new(
        peripheriques,
        hote,
        fournisseur,
        dossier.path().to_path_buf(),
    );
    let zone_simulee = Arc::new(ZoneSimulee {
        fournisseur: controleur.niveaux.fournisseur.clone(),
        boucle,
        asio: false,
        en_lecture: AtomicBool::new(false),
        panique: AtomicBool::new(false),
        vivant: Mutex::default(),
        journal: Mutex::default(),
        contournee_au_jeu: Mutex::default(),
    });
    controleur.niveaux.brancher(zone_simulee);
    let id = controleur
        .tester_niveau(DemandeNiveau::signal(zone, MICRO))
        .expect("lancé");
    assert_eq!(
        attendre_fin(&controleur, &id, Duration::from_secs(30)),
        EtatTest::Terminee,
        "{nom}"
    );
    controleur.test_niveau(&id).unwrap().avancement()
}

#[test]
fn le_test_de_niveau_a_44_1_et_96_khz() {
    let reference = tester_a(None, 711, "cadence-48k");
    let vr = reference.verdict.clone().expect("verdict 48 kHz");
    for (frequence, zone) in [(44_100, 712), (96_000, 713)] {
        let a = tester_a(Some(frequence), zone, &format!("cadence-{frequence}"));
        let v = a.verdict.clone().expect("verdict");
        eprintln!("{frequence} Hz : {v:?} (48 kHz : {vr:?})");
        assert_eq!(v.jugement, Jugement::Bon, "{frequence} Hz : {v:?}");
        assert!(
            !a.avertissements.iter().any(|m| m.contains("Hz")),
            "{frequence} Hz : {:?}",
            a.avertissements
        );
        assert!(
            (v.signal.rms_dbfs - vr.signal.rms_dbfs).abs() < 1.0,
            "{frequence} Hz : signal {} dBFS, 48 kHz {} dBFS",
            v.signal.rms_dbfs,
            vr.signal.rms_dbfs
        );
        assert!(
            (v.rsb_db - vr.rsb_db).abs() < 3.0,
            "{frequence} Hz : RSB {} dB, 48 kHz {} dB",
            v.rsb_db,
            vr.rsb_db
        );
    }
}

/// Une fréquence hors de la liste reste signalée, avec la liste.
#[test]
fn le_test_de_niveau_signale_une_frequence_non_prise_en_charge() {
    let a = tester_a(Some(32_000), 714, "cadence-32k");
    assert!(
        a.avertissements
            .iter()
            .any(|m| m.contains("32000 Hz")
                && m.contains("44100, 48000, 88200, 96000, 176400, 192000")),
        "{:?}",
        a.avertissements
    );
}
