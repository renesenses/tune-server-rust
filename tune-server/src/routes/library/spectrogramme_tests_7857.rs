//! Témoins de la route du spectrogramme (#7857) : PNG servi, cache au second
//! appel, plafond du cache, une seule génération par piste.

use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

/// Écrit un WAV PCM 16 bits mono : un sinus de `freq` Hz.
fn ecrire_wav(chemin: &CheminFs, freq: f64, secondes: f64) {
    let sr: u32 = 44_100;
    let n = (secondes * sr as f64) as usize;
    let donnees = (n * 2) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&sr.to_le_bytes());
    o.extend_from_slice(&(sr * 2).to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..n {
        let s = 0.5 * (std::f64::consts::TAU * freq * i as f64 / sr as f64).sin();
        o.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
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

    /// Une piste locale `id` : un sinus de 3 s.
    fn piste(&self, id: i64, freq: f64) -> PathBuf {
        let chemin = self.dossier.path().join(format!("piste_{id}.wav"));
        ecrire_wav(&chemin, freq, 3.0);
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

    async fn servir(&self, id: i64, plafond: u64) -> (StatusCode, Option<String>, Vec<u8>) {
        let taille = Taille {
            largeur: Some(400),
            hauteur: Some(200),
        };
        decomposer(servir(&self.state, id, taille, &self.cache(), plafond).await).await
    }
}

async fn decomposer(r: Response) -> (StatusCode, Option<String>, Vec<u8>) {
    let statut = r.status();
    let cache = r
        .headers()
        .get("x-tune-cache")
        .map(|v| v.to_str().unwrap().to_string());
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (statut, cache, corps)
}

fn pngs_du_cache(dossier: &CheminFs) -> Vec<(PathBuf, u64)> {
    let mut v: Vec<_> = std::fs::read_dir(dossier)
        .map(|e| {
            e.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
                .map(|e| (e.path(), e.metadata().unwrap().len()))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[tokio::test]
async fn la_route_rend_un_png_puis_le_sert_depuis_le_cache_7857() {
    let banc = Banc::new();
    banc.piste(1, 1_000.0);
    let app = crate::routes::router(banc.state.clone());
    let appel = || async {
        let r = app
            .clone()
            .oneshot(
                Request::get("/api/v1/library/tracks/1/spectrogramme.png?largeur=300&hauteur=150")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png",
            "la route doit rendre une image PNG"
        );
        decomposer(r).await
    };
    let (statut, cache, premier) = appel().await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("miss"));
    let image = image::load_from_memory(&premier).unwrap();
    assert_eq!((image.width(), image.height()), (300, 150));

    let (statut, cache, second) = appel().await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        cache.as_deref(),
        Some("hit"),
        "le second appel doit être servi depuis le cache, sans nouvelle génération"
    );
    assert_eq!(premier, second);
}

#[tokio::test]
async fn un_fichier_remplace_ne_sert_pas_l_image_perimee_7857() {
    let banc = Banc::new();
    let chemin = banc.piste(1, 1_000.0);
    let (_, cache, avant) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(cache.as_deref(), Some("miss"));
    // Même piste, autre contenu (autre taille de fichier).
    ecrire_wav(&chemin, 8_000.0, 4.0);
    let (statut, cache, apres) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("miss"));
    assert_ne!(avant, apres);
}

#[tokio::test]
async fn le_plafond_du_cache_est_respecte_7857() {
    let banc = Banc::new();
    for id in 1..=4 {
        banc.piste(id, 500.0 * id as f64);
    }
    let (_, _, une) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    // De quoi garder deux images, pas trois.
    let plafond = une.len() as u64 * 5 / 2;
    for id in 2..=4 {
        let (statut, cache, _) = banc.servir(id, plafond).await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(cache.as_deref(), Some("miss"));
        let total: u64 = pngs_du_cache(&banc.cache()).iter().map(|(_, l)| l).sum();
        assert!(
            total <= plafond,
            "cache de {total} octets au-delà du plafond de {plafond} après la piste {id}"
        );
    }
    let restants = pngs_du_cache(&banc.cache());
    assert!(
        restants
            .iter()
            .any(|(p, _)| p.file_name().unwrap().to_string_lossy().starts_with("4-")),
        "l'image qui vient d'être générée doit rester : {restants:?}"
    );
    assert!(
        !restants
            .iter()
            .any(|(p, _)| p.file_name().unwrap().to_string_lossy().starts_with("1-")),
        "la plus ancienne doit partir d'abord : {restants:?}"
    );
}

#[tokio::test]
async fn deux_appels_simultanes_ne_generent_qu_une_fois_7857() {
    let banc = Banc::new();
    banc.piste(1, 2_000.0);
    let (a, b) = tokio::join!(
        banc.servir(1, PLAFOND_DEFAUT_OCTETS),
        banc.servir(1, PLAFOND_DEFAUT_OCTETS)
    );
    let mut caches = vec![a.1.unwrap(), b.1.unwrap()];
    caches.sort();
    assert_eq!(
        caches,
        vec!["hit".to_string(), "miss".to_string()],
        "une seule génération par piste : le second appel attend puis lit le cache"
    );
    assert_eq!(a.2, b.2);
}

#[tokio::test]
async fn piste_inconnue_ou_sans_fichier_7857() {
    let banc = Banc::new();
    let (statut, _, _) = banc.servir(42, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
    banc.state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, file_path, source) VALUES (7, 'absente', '/nulle/part.flac', 'local')",
            &[],
        )
        .unwrap();
    let (statut, _, _) = banc.servir(7, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
}
