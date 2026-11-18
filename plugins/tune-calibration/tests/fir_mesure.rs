//! C3 (lot TESS T10b) : le FIR mesuré par les routes du greffon.
//!
//! La session à 5 positions est IMPORTÉE (document v2) à partir de réponses
//! de pièces simulées : aucune capture, aucun son. Ce qui est éprouvé ici :
//! le droit (D1 : la correction MESURÉE est Premium), le rapport, la RI
//! `.wav` relue par la convolution de `tune-core`, et les refus.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_calibration::MeasurementSpec;
use tune_calibration::analysis::{self, LogResponse};
use tune_calibration::deconv::ImpulseResponse;
use tune_calibration::positions::ConfigPositions;
use tune_calibration::sim::{POSITIONS_SIMULEES, RoomSim};
use tune_calibration_greffon::fournisseur::FournisseurBalayage;
use tune_calibration_greffon::hote::TraitementsDeZone;
use tune_calibration_greffon::mesure::{Controleur, FREQUENCE_MESURE};
use tune_calibration_greffon::routes::{EtatRoutes, router};
use tune_calibration_greffon::simulation::{Boucle, HoteSimule, MicroSimule, PieceEnDirect};
use tune_core::license::LicenseManager;
use tune_entree_audio::peripheriques::Peripheriques;

const FS: u32 = FREQUENCE_MESURE;

fn controleur(nom: &str) -> (Arc<Controleur>, tune_core::test_scratch::ScratchDir) {
    let sim = RoomSim::dry_living_room(FS);
    let boucle = Boucle::new(PieceEnDirect::depuis(&sim, 1.0), 8.0);
    let fournisseur = FournisseurBalayage::new();
    let hote = HoteSimule::new(fournisseur.clone(), boucle.clone(), Some(63.0));
    let micro: Arc<dyn Peripheriques> = Arc::new(MicroSimule {
        nom: "UMIK-1 simulé".into(),
        frequence: FS,
        boucle,
        demarrages: Default::default(),
    });
    let dossier = tune_core::test_scratch::scratch_dir(&format!("tune-calibration-fir-{nom}"));
    let c = Controleur::new(micro, hote, fournisseur, dossier.path().to_path_buf());
    (c, dossier)
}

/// Réponse « vraie » (RI synthétique, même analyse que la mesure).
fn reponse(room: &RoomSim) -> LogResponse {
    let spec = MeasurementSpec::room_default(FS);
    let ir = ImpulseResponse {
        sample_rate: FS,
        samples: room.impulse_response(),
        peak_index: 0,
        delay_samples: 0,
    };
    analysis::measured_response(&ir, spec.window_post_ms, spec.smoothing_n, &spec.grid())
}

/// Document de session v2 : 5 positions du salon sec simulé.
fn document_session() -> serde_json::Value {
    let base = RoomSim::dry_living_room(FS);
    let positions: Vec<serde_json::Value> = POSITIONS_SIMULEES[..5]
        .iter()
        .enumerate()
        .map(|(k, u)| {
            let r = reponse(&base.a_la_position(*u));
            serde_json::json!({
                "index": k,
                "etiquette": format!("place {k}"),
                "mesure_id": null,
                "entree": "UMIK-1 simulé",
                "resultat": {
                    "frequence": FS,
                    "niveau_joue_dbfs": -20.0,
                    "salves": [],
                    "ri": { "retard_ms": 12.0, "crete_dbfs": -10.0, "rsb_db": 60.0, "trames": 48000, "crete_a_la_trame": 4800 },
                    "reponse": { "lissage": "1/12", "freqs_hz": r.freqs, "db": r.magnitude_db },
                    "rt60": [],
                    "etalonnage": null,
                    "traitements_zone": serde_json::to_value(TraitementsDeZone::default()).unwrap(),
                    "avertissements": [],
                    "duree_analyse_ms": 0
                }
            })
        })
        .collect();
    serde_json::json!({
        "version": 2,
        "id": "",
        "zone_id": 7,
        "positions_prevues": 5,
        "centrale": 0,
        "config": serde_json::to_value(ConfigPositions::default()).unwrap(),
        "positions": positions,
    })
}

struct Banc {
    controleur: Arc<Controleur>,
    rt: tokio::runtime::Runtime,
    licence: Arc<LicenseManager>,
    _dossier: tune_core::test_scratch::ScratchDir,
}

impl Banc {
    fn new(nom: &str) -> Self {
        let (controleur, dossier) = controleur(nom);
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let licence = Arc::new(LicenseManager::new(Arc::new(db)));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        Self {
            controleur,
            rt,
            licence,
            _dossier: dossier,
        }
    }

    /// Appel brut : statut, type de contenu, octets.
    fn appel(
        &self,
        licence: Option<Arc<LicenseManager>>,
        methode: &str,
        chemin: &str,
        corps: serde_json::Value,
    ) -> (StatusCode, String, Vec<u8>) {
        let app = router(EtatRoutes {
            controleur: self.controleur.clone(),
            license: licence,
        });
        let req = Request::builder()
            .method(methode)
            .uri(chemin)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap();
        self.rt.block_on(async move {
            let r = app.oneshot(req).await.unwrap();
            let code = r.status();
            let ct = r
                .headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default();
            let o = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (code, ct, o.to_vec())
        })
    }

    fn json(
        &self,
        methode: &str,
        chemin: &str,
        corps: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let (c, _, o) = self.appel(Some(self.licence.clone()), methode, chemin, corps);
        (c, serde_json::from_slice(&o).unwrap_or_default())
    }
}

/// D1 (18/11/2026) : la correction MESURÉE, dont le FIR, est Premium. Sans
/// licence fournie, puis avec un compte gratuit, chaque route FIR rend le
/// refus nommé `premium_required` (402) ; avec Premium elles servent.
#[test]
fn le_fir_mesure_est_premium() {
    let b = Banc::new("premium");
    let routes = [
        "/sessions/inconnue/fir",
        "/sessions/inconnue/fir.wav",
        "/mesures/inconnue/fir",
        "/mesures/inconnue/fir.wav",
    ];
    for (cas, l) in [
        ("sans licence", None),
        ("compte gratuit", Some(b.licence.clone())),
    ] {
        for chemin in routes {
            let (code, _, o) = b.appel(l.clone(), "POST", chemin, serde_json::json!({}));
            let v: serde_json::Value = serde_json::from_slice(&o).unwrap_or_default();
            assert_eq!(code, StatusCode::PAYMENT_REQUIRED, "{cas} : {chemin} → {v}");
            assert_eq!(v["error"], "premium_required", "{cas} : {chemin}");
            assert_eq!(v["code"], "room_correction", "{cas} : {chemin}");
        }
    }
    b.rt.block_on(b.licence.set_account_premium(true, None));
    for chemin in routes {
        let (code, v) = b.json("POST", chemin, serde_json::json!({}));
        assert_eq!(code, StatusCode::NOT_FOUND, "Premium : {chemin} → {v}");
    }
}

/// Bout en bout : session importée de 5 positions → rapport → RI `.wav`
/// relue par la convolution de `tune-core` à l'identique (au f32 près), et
/// dont le module (calculé par `convolver::fir_frequency_response`) suit la
/// correction annoncée.
#[test]
fn la_session_donne_un_fir_que_la_convolution_relit() {
    let b = Banc::new("bout-en-bout");
    b.rt.block_on(b.licence.set_account_premium(true, None));
    let (code, v) = b.json("POST", "/sessions/import", document_session());
    assert_eq!(code, StatusCode::CREATED, "{v}");
    let sid = v["id"].as_str().unwrap().to_string();

    let (code, r) = b.json(
        "POST",
        &format!("/sessions/{sid}/fir"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["positions"], 5);
    assert_eq!(r["frequence"], FS);
    assert_eq!(r["coefficients"], 32_768);
    assert!(
        r["latence_ms"].as_f64().unwrap() < 50.0,
        "{}",
        r["latence_ms"]
    );
    let av: Vec<String> = serde_json::from_value(r["avertissements"].clone()).unwrap();
    assert!(
        av.iter().all(|a| !a.contains("mesure en un point")),
        "{av:?}"
    );
    let freqs: Vec<f64> = serde_json::from_value(r["freqs_hz"].clone()).unwrap();
    let corr: Vec<f64> = serde_json::from_value(r["correction_db"].clone()).unwrap();
    // Le mode à 42 Hz (+9 dB) est coupé ; aucune remontée au-delà de la borne.
    let a42 = analysis::interp_log(&freqs, &corr, 42.0);
    assert!(a42 < -4.0, "42 Hz : {a42:+.2} dB");
    assert!(corr.iter().all(|c| *c <= 4.0 + 1e-9));

    let (code, ct, wav) = b.appel(
        Some(b.licence.clone()),
        "POST",
        &format!("/sessions/{sid}/fir.wav"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::OK);
    assert_eq!(ct, "audio/wav");
    let dossier = tune_core::test_scratch::scratch_dir("tune-calibration-fir-wav");
    let chemin = dossier.path().join("correction.wav");
    std::fs::write(&chemin, &wav).unwrap();
    let (taps, fs) =
        tune_core::audio::convolver::Convolver::read_ir_taps(chemin.to_str().unwrap()).unwrap();
    assert_eq!(fs, FS);
    assert_eq!(taps.len(), 1, "RI mono");
    assert_eq!(taps[0].len(), 32_768);

    // Le module relu par la convolution suit la correction annoncée (au
    // niveau près : la normalisation retire un gain constant).
    let points = [42.0, 68.0, 115.0, 1000.0, 5000.0];
    let lu = tune_core::audio::convolver::fir_frequency_response(&taps[0], fs, &points);
    let attendu: Vec<f64> = points
        .iter()
        .map(|f| analysis::interp_log(&freqs, &corr, *f))
        .collect();
    let decalage = lu[3].magnitude_db - attendu[3];
    for (p, a) in lu.iter().zip(&attendu) {
        println!(
            "T10b convolution relue : {:.0} Hz, FIR {:+.2} dB, annoncé {:+.2} dB",
            p.freq_hz,
            p.magnitude_db - decalage,
            a
        );
        assert!(
            (p.magnitude_db - decalage - a).abs() < 0.6,
            "{} Hz : relu {:+.2} dB, annoncé {:+.2} dB",
            p.freq_hz,
            p.magnitude_db - decalage,
            a
        );
    }

    // Format 64 bits : relu aussi, mêmes coefficients au f32 près.
    let (code, _, wav64) = b.appel(
        Some(b.licence.clone()),
        "POST",
        &format!("/sessions/{sid}/fir.wav"),
        serde_json::json!({ "format": "float64" }),
    );
    assert_eq!(code, StatusCode::OK);
    std::fs::write(&chemin, &wav64).unwrap();
    let (taps64, _) =
        tune_core::audio::convolver::Convolver::read_ir_taps(chemin.to_str().unwrap()).unwrap();
    assert_eq!(taps64, taps);
}

/// Refus : réglage hors bornes dures, fréquence non prise, session sans
/// moyenne. Et la mesure en un point (session v1 importée) est acceptée
/// avec l'avertissement.
#[test]
fn les_refus_et_la_mesure_en_un_point() {
    let b = Banc::new("refus");
    b.rt.block_on(b.licence.set_account_premium(true, None));
    let (_, v) = b.json("POST", "/sessions/import", document_session());
    let sid = v["id"].as_str().unwrap().to_string();
    for corps in [
        serde_json::json!({ "remontee_max_db": 10.0 }),
        serde_json::json!({ "coupe_max_db": 40.0 }),
        serde_json::json!({ "frequence": 22050 }),
        serde_json::json!({ "f_min_hz": 5.0 }),
    ] {
        let (code, r) = b.json("POST", &format!("/sessions/{sid}/fir"), corps.clone());
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{corps} → {r}");
        assert_eq!(r["error"], "fir_refuse");
    }
    // 96 kHz : le FIR est conçu à la fréquence demandée.
    let (code, r) = b.json(
        "POST",
        &format!("/sessions/{sid}/fir"),
        serde_json::json!({ "frequence": 96000, "cible": { "type": "harman" } }),
    );
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["frequence"], 96_000);
    assert_eq!(r["coefficients"], 65_536);

    // Une seule position (document v1 : le résultat d'une mesure).
    let doc = document_session();
    let v1 = doc["positions"][0]["resultat"].clone();
    let (code, v) = b.json("POST", "/sessions/import", v1);
    assert_eq!(code, StatusCode::CREATED, "{v}");
    let id1 = v["id"].as_str().unwrap().to_string();
    let (code, r) = b.json(
        "POST",
        &format!("/sessions/{id1}/fir"),
        serde_json::json!({}),
    );
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["positions"], 1);
    let av: Vec<String> = serde_json::from_value(r["avertissements"].clone()).unwrap();
    assert!(
        av.iter().any(|a| a.contains("mesure en un point")),
        "{av:?}"
    );
}
