//! Fil 3094 (Xavier, rc13.90, WASAPI + UMIK-1).
//!
//! 1. Réussie, échouée ou annulée, une mesure LIBÈRE la file de sa zone : le
//!    balayage n'y reste pas (`HoteZone::liberer`, après l'arrêt). Le retrait
//!    lui-même et la file rendue sont prouvés sur une vraie base dans
//!    `tune_core::db::file_technique_3094`.
//! 2. Une zone sans sortie active (deux zones sur le même DAC, celle-ci
//!    orpheline) est refusée AVANT tout : ni volume forcé, ni balayage, ni
//!    micro ouvert, et le refus nomme la zone qui porte l'appareil.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tune_calibration::sim::RoomSim;
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::hote::{
    ElementBalayage, HoteZone, SortieInactive, TraitementsDeZone, zone_porteuse,
};
use tune_calibration_greffon::mesure::{Controleur, Demande, Etat, FREQUENCE_MESURE};
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_core::outputs::identite_de_sortie::ZoneLocale;
use tune_core::source_pcm::FournisseurPcm;
use tune_entree_audio::peripheriques::Peripheriques;

const MICRO: &str = "UMIK-1 simulé";
const VITESSE: f64 = 8.0;

/// L'hôte simulé, plus ce que ce fil ajoute : `liberer` noté, et une sortie
/// inactive au besoin.
struct Hote {
    simule: Arc<HoteSimule>,
    inactive: Option<SortieInactive>,
    liberees: Mutex<Vec<(i64, String)>>,
    /// Le fournisseur était-il encore armé pour cette mesure au moment de
    /// `liberer` ? (L'orchestrateur ne doit pas l'avoir jugée périmée trop tôt.)
    armee_a_la_liberation: Mutex<Vec<bool>>,
    fournisseur: Arc<FournisseurBalayage>,
}

impl HoteZone for Hote {
    fn volume(&self, zone_id: i64) -> Option<f64> {
        self.simule.volume(zone_id)
    }
    fn regler_volume(&self, zone_id: i64, volume_pct: f64) -> Result<(), String> {
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
    fn liberer(&self, zone_id: i64, mesure_id: &str) {
        self.armee_a_la_liberation
            .lock()
            .unwrap()
            .push(self.fournisseur.encore_jouable(mesure_id));
        self.liberees
            .lock()
            .unwrap()
            .push((zone_id, mesure_id.to_string()));
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
    hote: Arc<Hote>,
    micro: Arc<MicroSimule>,
    _dossier: tune_core::test_scratch::ScratchDir,
}

fn banc(gain_micro: f64, inactive: Option<SortieInactive>, nom: &str) -> Banc {
    let sim = RoomSim::dry_living_room(FREQUENCE_MESURE);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, gain_micro), VITESSE);
    let fournisseur = FournisseurBalayage::new();
    let simule = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(50.0));
    let hote = Arc::new(Hote {
        simule,
        inactive,
        liberees: Mutex::default(),
        armee_a_la_liberation: Mutex::default(),
        fournisseur: fournisseur.clone(),
    });
    let micro = Arc::new(MicroSimule {
        nom: MICRO.into(),
        frequence: FREQUENCE_MESURE,
        boucle,
        demarrages: Default::default(),
    });
    let peripheriques: Arc<dyn Peripheriques> = micro.clone();
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-3094-{nom}"));
    let controleur = Controleur::new(
        peripheriques,
        hote.clone(),
        fournisseur,
        dossier.path().to_path_buf(),
    );
    Banc {
        controleur,
        hote,
        micro,
        _dossier: dossier,
    }
}

fn demande(zone: i64, duree_s: f64) -> Demande {
    let mut d = Demande::defaut(zone, MICRO);
    d.duree_s = duree_s;
    d
}

fn attendre(c: &Controleur, id: &str, pred: impl Fn(&Etat) -> bool) -> Etat {
    let t0 = Instant::now();
    loop {
        let e = c.mesure(id).expect("mesure connue").etat();
        if pred(&e) || e.finale() {
            return e;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(90),
            "état non atteint : {e:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn liberee_une_fois(b: &Banc, zone: i64, id: &str) {
    assert_eq!(
        *b.hote.liberees.lock().unwrap(),
        vec![(zone, id.to_string())],
        "le balayage quitte la file une fois, à la fin de la mesure"
    );
    assert_eq!(
        *b.hote.armee_a_la_liberation.lock().unwrap(),
        vec![true],
        "libérée par la mesure elle-même, avant le désarmement"
    );
    // Après la conclusion, plus rien n'est jouable sous cet identifiant : une
    // ligne oubliée serait écartée par l'orchestrateur.
    assert!(!b.hote.fournisseur.encore_jouable(id));
}

#[test]
fn une_mesure_reussie_libere_la_file_3094() {
    let b = banc(1.0, None, "reussie");
    let id = b.controleur.lancer(demande(6, 2.0)).expect("lancée");
    let etat = attendre(&b.controleur, &id, |_| false);
    assert_eq!(etat, Etat::Terminee);
    liberee_une_fois(&b, 6, &id);
}

#[test]
fn une_mesure_echouee_libere_la_file_3094() {
    // Micro ×200 : écrêtage dès la première salve, refus.
    let b = banc(200.0, None, "echouee");
    let id = b.controleur.lancer(demande(6, 2.0)).expect("lancée");
    let etat = attendre(&b.controleur, &id, |_| false);
    assert!(matches!(etat, Etat::Refusee { .. }), "{etat:?}");
    liberee_une_fois(&b, 6, &id);
}

#[test]
fn une_mesure_annulee_libere_la_file_3094() {
    let b = banc(1.0, None, "annulee");
    let id = b.controleur.lancer(demande(6, 8.0)).expect("lancée");
    let e = attendre(&b.controleur, &id, |e| matches!(e, Etat::Balayage { .. }));
    assert!(matches!(e, Etat::Balayage { .. }), "{e:?}");
    assert!(b.controleur.annuler(&id));
    assert_eq!(attendre(&b.controleur, &id, |_| false), Etat::Annulee);
    liberee_une_fois(&b, 6, &id);
}

#[test]
fn une_zone_orpheline_est_refusee_avant_tout_en_nommant_la_porteuse_3094() {
    let inactive = SortieInactive {
        device_id: "local:SmartDX1".into(),
        porteuse: Some((6, "Zone 7".into())),
    };
    let b = banc(1.0, Some(inactive), "orpheline");
    let e = b.controleur.lancer(demande(7, 2.0)).unwrap_err();
    assert!(e.starts_with("Cette zone n'a pas de sortie active"), "{e}");
    assert!(
        e.contains("une autre zone utilise déjà cet appareil"),
        "{e}"
    );
    assert!(
        e.contains("« SmartDX1 »") && e.contains("« Zone 7 » (n° 6)"),
        "{e}"
    );
    assert!(!e.contains("not registered"), "{e}");
    let j = b.hote.simule.journal();
    assert!(j.volumes.is_empty(), "aucun volume forcé");
    assert!(j.joues.is_empty(), "aucun balayage");
    assert_eq!(
        *b.micro.demarrages.lock().unwrap(),
        0,
        "micro jamais ouvert"
    );
    assert!(b.controleur.mesures().is_empty());
    assert!(b.hote.liberees.lock().unwrap().is_empty());
}

#[test]
fn sans_porteuse_connue_le_refus_dit_que_l_appareil_n_est_pas_detecte_3094() {
    let s = SortieInactive {
        device_id: "local:SmartDX1".into(),
        porteuse: None,
    };
    let m = s.message();
    assert!(m.starts_with("Cette zone n'a pas de sortie active"), "{m}");
    assert!(m.contains("« SmartDX1 » n'est pas détecté"), "{m}");
}

fn zone(id: i64, device: &str, endpoint: Option<&str>, masquee: bool) -> ZoneLocale {
    ZoneLocale {
        id,
        output_device_id: device.into(),
        output_endpoint_id: endpoint.map(Into::into),
        masquee,
    }
}

#[test]
fn la_porteuse_est_la_zone_active_au_meme_endpoint_3094() {
    const GUID: &str = "wasapi:{0.0.0.00000000}.{smartdx1}";
    // Le parc de Xavier : la zone 7 garde l'ancien nom, la zone 6 porte le
    // DAC sous son nom d'aujourd'hui ; une zone 9 masquée au même endpoint.
    let zones = [
        zone(6, "local:Haut-parleurs (SmartDX1)", Some(GUID), false),
        zone(7, "local:SmartDX1", Some(GUID), false),
        zone(9, "local:SmartDX1 (2)", Some(GUID), true),
        zone(3, "local:Realtek", Some("wasapi:{autre}"), false),
    ];
    let enregistree = |d: &str| d == "local:Haut-parleurs (SmartDX1)" || d == "local:Realtek";
    assert_eq!(zone_porteuse(&zones, 7, enregistree), Some(6));
    // Endpoint inconnu : aucune devinette sur le nom.
    let sans = [
        zone(6, "local:SmartDX1 (2)", None, false),
        zone(7, "local:SmartDX1", None, false),
    ];
    assert_eq!(zone_porteuse(&sans, 7, |_| true), None);
    // La seule zone au même endpoint n'est pas active : pas de porteuse.
    assert_eq!(zone_porteuse(&zones, 7, |d| d == "local:Realtek"), None);
}

/// Par la route : 409 au motif stable `zone_sans_sortie_active`, l'appareil
/// et la zone porteuse en paramètres (le client compose la phrase dans sa
/// langue), la phrase française en `message`.
#[test]
fn la_route_rend_un_refus_structure_pour_une_zone_orpheline_3094() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_calibration_greffon::routes::{EtatRoutes, router};
    use tune_core::license::LicenseManager;

    let inactive = SortieInactive {
        device_id: "local:SmartDX1".into(),
        porteuse: Some((6, "Zone 7".into())),
    };
    let b = banc(1.0, Some(inactive), "route-orpheline");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let licence = Arc::new(LicenseManager::new(Arc::new(db)));
    rt.block_on(licence.set_account_premium(true, None));
    let app = router(EtatRoutes {
        controleur: b.controleur.clone(),
        license: Some(licence),
    });
    let req = Request::builder()
        .method("POST")
        .uri("/zones/7/mesures")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"entree":"{MICRO}","duree_s":2.0}}"#
        )))
        .unwrap();
    let (code, v) = rt.block_on(async move {
        let r = app.oneshot(req).await.unwrap();
        let code = r.status();
        let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            code,
            serde_json::from_slice::<serde_json::Value>(&octets).unwrap_or_default(),
        )
    });
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "zone_sans_sortie_active", "{v}");
    assert_eq!(v["params"]["device"], "SmartDX1", "{v}");
    assert_eq!(v["params"]["holder_zone_id"], 6, "{v}");
    assert_eq!(v["params"]["holder_zone_name"], "Zone 7", "{v}");
    assert!(
        v["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Cette zone n'a pas de sortie active")),
        "{v}"
    );
    assert!(b.hote.simule.journal().volumes.is_empty());
    assert!(b.controleur.mesures().is_empty());
}
