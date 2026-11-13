//! Témoins de la route de la forme d'onde (fil du forum 2793) : contrat JSON
//! d'un sinus à −6 dBFS, deux canaux, cache au second appel, plage du zoom,
//! piste de service et piste inconnue, section de la fiche.

use super::*;

const SR: u32 = 44_100;

/// Un WAV PCM 16 bits : un sinus de 1 kHz par canal, à la crête donnée en
/// dBFS (au-dessus de 0 : écrêté).
fn ecrire_wav(chemin: &CheminFs, cretes_db: &[f64], secondes: f64) {
    let canaux = cretes_db.len() as u16;
    let n = (secondes * f64::from(SR)) as usize;
    let donnees = (n * 2 * canaux as usize) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&canaux.to_le_bytes());
    o.extend_from_slice(&SR.to_le_bytes());
    o.extend_from_slice(&(SR * 2 * u32::from(canaux)).to_le_bytes());
    o.extend_from_slice(&(2 * canaux).to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..n {
        let s = (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(SR)).sin();
        for db in cretes_db {
            let a = 10f64.powf(db / 20.0);
            o.extend_from_slice(&(((a * s).clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    std::fs::write(chemin, o).unwrap();
}

struct Banc {
    state: AppState,
    dossier: tempfile::TempDir,
}

impl Banc {
    fn new() -> Self {
        Self {
            state: AppState::new(":memory:", 0, Default::default()).unwrap(),
            dossier: tempfile::tempdir().unwrap(),
        }
    }

    fn piste(&self, id: i64, cretes_db: &[f64]) -> PathBuf {
        let chemin = self.dossier.path().join(format!("piste_{id}.wav"));
        ecrire_wav(&chemin, cretes_db, 3.0);
        self.state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, duration_ms, source) VALUES (?, 'sinus', ?, 3000, 'local')",
                &[&id, &chemin.to_string_lossy().into_owned()],
            )
            .unwrap();
        chemin
    }

    fn cache(&self) -> PathBuf {
        self.dossier.path().join("cache")
    }

    async fn servir(&self, id: i64, p: Parametres) -> (StatusCode, Option<String>, Value) {
        let r = servir(&self.state, id, p, &self.cache(), PLAFOND_DEFAUT_OCTETS).await;
        let statut = r.status();
        let cache = r
            .headers()
            .get("x-tune-cache")
            .map(|v| v.to_str().unwrap().to_string());
        let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            cache,
            serde_json::from_slice(&corps).unwrap_or(Value::Null),
        )
    }
}

fn colonnes(n: usize) -> Parametres {
    Parametres {
        colonnes: Some(n),
        ..Default::default()
    }
}

fn proche(v: &Value, attendu: f64, tolerance: f64) -> bool {
    v.as_f64().is_some_and(|x| (x - attendu).abs() <= tolerance)
}

#[tokio::test]
async fn sinus_stereo_contrat_json_et_cache() {
    let banc = Banc::new();
    banc.piste(1, &[-6.0, -12.0]);

    let (statut, cache, f) = banc.servir(1, colonnes(200)).await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    assert_eq!(cache.as_deref(), Some("miss"));
    assert_eq!(f["version"], 1);
    assert_eq!(f["track_id"], 1);
    assert_eq!(f["colonnes"], 200);
    assert_eq!(f["canaux"], 2);
    assert_eq!(f["sample_rate"], SR);
    assert!(proche(&f["duree_s"], 3.0, 0.01), "{}", f["duree_s"]);
    assert_eq!(f["seuil_ecretage_dbfs"], SEUIL_ECRETAGE_DBFS);
    assert!(f["calcule_le"].as_str().is_some_and(|d| d.ends_with('Z')));
    assert!(f["duree_calcul_ms"].as_u64().is_some());

    let g = &f["voies"][0];
    let d = &f["voies"][1];
    assert_eq!(f["voies"].as_array().unwrap().len(), 2);
    assert!(proche(&g["crete_dbfs"], -6.0, 0.1), "{}", g["crete_dbfs"]);
    assert!(proche(&g["rms_dbfs"], -9.0, 0.1), "{}", g["rms_dbfs"]);
    assert!(proche(&g["facteur_crete_db"], 3.0, 0.1));
    assert_eq!(g["ecretes"], 0);
    assert!(proche(&d["crete_dbfs"], -12.0, 0.1), "{}", d["crete_dbfs"]);
    for voie in [g, d] {
        for cle in ["min", "max", "rms"] {
            assert_eq!(voie[cle].as_array().unwrap().len(), 200, "{cle}");
        }
    }
    assert!(proche(&g["max"][100], 0.501, 0.01), "{}", g["max"][100]);
    assert!(proche(&g["min"][100], -0.501, 0.01));
    assert!(proche(&f["global"]["crete_dbfs"], -6.0, 0.1));
    assert_eq!(f["global"]["ecretes"], 0);

    // Second appel : depuis le cache, à l'identique.
    let (statut, cache, f2) = banc.servir(1, colonnes(200)).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("hit"));
    assert_eq!(f2["voies"], f["voies"]);
}

#[tokio::test]
async fn signal_ecrete_decompte_positif() {
    let banc = Banc::new();
    banc.piste(2, &[6.0]);
    let (statut, _, f) = banc.servir(2, colonnes(100)).await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    assert!(
        f["global"]["ecretes"].as_u64().unwrap() > 0,
        "{}",
        f["global"]
    );
    assert_eq!(f["canaux"], 1);
    assert!(proche(&f["global"]["crete_dbfs"], 0.0, 0.01));
}

#[tokio::test]
async fn plage_du_zoom_sans_cache() {
    let banc = Banc::new();
    banc.piste(3, &[-6.0]);
    let p = Parametres {
        colonnes: Some(100),
        debut_s: Some(1.0),
        fin_s: Some(2.0),
    };
    let (statut, cache, f) = banc.servir(3, p).await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    assert_eq!(cache.as_deref(), Some("plage"));
    assert_eq!(f["debut_s"], 1.0);
    assert!(proche(&f["duree_s"], 1.0, 0.01), "{}", f["duree_s"]);
    assert_eq!(f["colonnes"], 100);
    assert!(!banc.cache().exists(), "une plage n'entre pas au cache");

    let p = Parametres {
        colonnes: None,
        debut_s: Some(2.0),
        fin_s: Some(1.0),
    };
    let (statut, _, _) = banc.servir(3, p).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn piste_de_service_ou_inconnue() {
    let banc = Banc::new();
    banc.state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, duration_ms, source, source_id) VALUES (4, 'service', 180000, 'qobuz', 'q-4')",
            &[],
        )
        .unwrap();
    let (statut, _, _) = banc.servir(4, colonnes(100)).await;
    assert_eq!(
        statut,
        StatusCode::UNPROCESSABLE_ENTITY,
        "même règle que le spectrogramme"
    );
    let (statut, _, _) = banc.servir(99, colonnes(100)).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn section_de_la_fiche_absente_puis_a_jour() {
    let banc = Banc::new();
    banc.piste(5, &[-6.0]);
    let track = TrackRepo::with_backend(banc.state.backend.clone())
        .get(5)
        .unwrap()
        .unwrap();
    let s = section(&track, &banc.cache()).unwrap();
    assert_eq!(s["etat"], "absent");
    assert_eq!(
        s["url"],
        "/api/v1/library/tracks/5/forme-onde?colonnes=1000"
    );
    let (statut, _, _) = banc.servir(5, Parametres::default()).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(section(&track, &banc.cache()).unwrap()["etat"], "a_jour");

    let mut service = track.clone();
    service.file_path = None;
    assert!(section(&service, &banc.cache()).is_none());
}

#[tokio::test]
async fn route_branchee_au_routeur() {
    let banc = Banc::new();
    let app = crate::routes::router(banc.state.clone());
    use tower::ServiceExt;
    let r = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/library/tracks/42/forme-onde")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &corps[..],
        b"track not found",
        "la route existe : 404 de la piste"
    );
}
