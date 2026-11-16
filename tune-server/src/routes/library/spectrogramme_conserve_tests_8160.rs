//! #8160 — le spectrogramme CONSERVÉ : chaque génération laisse sa trace datée
//! dans `spectrogrammes`, l'image précédente part du cache quand le fichier
//! change, une image évincée emporte sa ligne, `POST …/spectrogramme`
//! recalcule sans lire le cache.

use super::*;
use tune_core::db::spectrogrammes as traces;

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

    fn piste(&self, id: i64, freq: f64) -> PathBuf {
        let chemin = self.dossier.path().join(format!("piste_{id}.wav"));
        ecrire_wav(&chemin, freq, 2.0);
        self.state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, duration_ms, source) VALUES (?, 'sinus', ?, 2000, 'local')",
                &[&id, &chemin.to_string_lossy().into_owned()],
            )
            .unwrap();
        chemin
    }

    fn cache(&self) -> PathBuf {
        self.dossier.path().join("cache")
    }

    fn taille() -> Taille {
        Taille {
            largeur: Some(400),
            hauteur: Some(200),
        }
    }

    async fn servir(&self, id: i64, plafond: u64) -> (StatusCode, Option<String>) {
        let r = servir(&self.state, id, Self::taille(), &self.cache(), plafond).await;
        let cache = r
            .headers()
            .get("x-tune-cache")
            .map(|v| v.to_str().unwrap().to_string());
        (r.status(), cache)
    }

    fn trace(&self, id: i64) -> Option<traces::Spectrogramme> {
        traces::lire(self.state.backend.as_ref(), id).unwrap()
    }
}

fn pngs(dossier: &CheminFs) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dossier)
        .map(|e| {
            e.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[tokio::test]
async fn une_generation_laisse_sa_trace_datee_8160() {
    let banc = Banc::new();
    let chemin = banc.piste(1, 1_000.0);
    assert_eq!(banc.trace(1), None);
    let (statut, cache) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!((statut, cache.as_deref()), (StatusCode::OK, Some("miss")));
    let t = banc
        .trace(1)
        .expect("une image générée doit laisser sa trace dans `spectrogrammes`");
    assert_eq!(pngs(&banc.cache()), vec![t.fichier.clone()]);
    assert_eq!((t.largeur, t.hauteur), (400, 200));
    let meta = std::fs::metadata(&chemin).unwrap();
    assert_eq!(t.signature, traces::signature(&meta));
    assert_eq!(t.taille_fichier, Some(meta.len() as i64));
    assert_eq!(
        t.octets as u64,
        std::fs::metadata(banc.cache().join(&t.fichier))
            .unwrap()
            .len()
    );
    assert!(
        t.calcule_le.ends_with('Z') && t.calcule_le.len() == 20,
        "{}",
        t.calcule_le
    );
    assert!(t.duree_calcul_ms.is_some());

    // Servie depuis le cache : la trace ne bouge pas.
    let (_, cache) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(cache.as_deref(), Some("hit"));
    assert_eq!(banc.trace(1), Some(t));
}

#[tokio::test]
async fn le_fichier_change_l_ancienne_image_part_8160() {
    let banc = Banc::new();
    let chemin = banc.piste(1, 1_000.0);
    banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    let avant = banc.trace(1).unwrap();
    let meta = std::fs::metadata(&chemin).unwrap();
    assert_eq!(
        section(banc.state.backend.as_ref(), 1, &meta, &banc.cache()).unwrap()["etat"],
        "a_jour"
    );

    ecrire_wav(&chemin, 6_000.0, 3.0);
    let meta = std::fs::metadata(&chemin).unwrap();
    assert_eq!(
        section(banc.state.backend.as_ref(), 1, &meta, &banc.cache()).unwrap()["etat"],
        "perime",
        "un fichier réécrit rend l'image conservée périmée"
    );
    let (_, cache) = banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    assert_eq!(cache.as_deref(), Some("miss"));
    let apres = banc.trace(1).unwrap();
    assert_ne!(apres.fichier, avant.fichier);
    assert_eq!(
        pngs(&banc.cache()),
        vec![apres.fichier.clone()],
        "l'image du fichier d'avant doit quitter le cache"
    );
    assert_eq!(
        section(banc.state.backend.as_ref(), 1, &meta, &banc.cache()).unwrap()["etat"],
        "a_jour"
    );
}

#[tokio::test]
async fn une_image_evincee_emporte_sa_trace_8160() {
    let banc = Banc::new();
    for id in 1..=3 {
        banc.piste(id, 700.0 * id as f64);
    }
    banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    let une = banc.trace(1).unwrap().octets as u64;
    // De quoi garder une image et demie : chaque nouvelle évince la précédente.
    let plafond = une * 3 / 2;
    banc.servir(2, plafond).await;
    banc.servir(3, plafond).await;
    assert_eq!(
        banc.trace(1),
        None,
        "image 1 évincée : sa ligne doit partir"
    );
    assert_eq!(
        banc.trace(2),
        None,
        "image 2 évincée : sa ligne doit partir"
    );
    assert!(banc.trace(3).is_some());
    let restantes = pngs(&banc.cache());
    assert_eq!(restantes, vec![banc.trace(3).unwrap().fichier]);
}

#[tokio::test]
async fn une_image_effacee_du_cache_se_dit_absente_8160() {
    let banc = Banc::new();
    let chemin = banc.piste(1, 1_000.0);
    banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    let t = banc.trace(1).unwrap();
    std::fs::remove_file(banc.cache().join(&t.fichier)).unwrap();
    let meta = std::fs::metadata(&chemin).unwrap();
    let s = section(banc.state.backend.as_ref(), 1, &meta, &banc.cache()).unwrap();
    assert_eq!(s["etat"], "absent");
    assert!(s.get("calcule_le").is_none());
    assert_eq!(
        banc.trace(1),
        None,
        "la ligne d'une image disparue s'efface"
    );
}

#[tokio::test]
async fn recalculer_ne_lit_pas_le_cache_8160() {
    let banc = Banc::new();
    banc.piste(1, 1_000.0);
    banc.servir(1, PLAFOND_DEFAUT_OCTETS).await;
    let avant = banc.trace(1).unwrap();
    // La trace d'avant porte une date ancienne : le recalcul doit la changer.
    banc.state
        .backend
        .execute(
            "UPDATE spectrogrammes SET calcule_le = '2000-01-01T00:00:00Z' WHERE track_id = 1",
            &[],
        )
        .unwrap();
    let r = recalculer_dans(
        &banc.state,
        1,
        Banc::taille(),
        &banc.cache(),
        PLAFOND_DEFAUT_OCTETS,
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&corps).unwrap();
    assert_eq!(v["etat"], "a_jour");
    assert_ne!(v["calcule_le"], "2000-01-01T00:00:00Z", "{v}");
    let apres = banc.trace(1).unwrap();
    assert_eq!(apres.fichier, avant.fichier);
    assert_ne!(apres.calcule_le, "2000-01-01T00:00:00Z");
    assert_eq!(pngs(&banc.cache()).len(), 1);
}

#[tokio::test]
async fn recalculer_une_piste_sans_fichier_8160() {
    let banc = Banc::new();
    let r = recalculer_dans(
        &banc.state,
        9,
        Banc::taille(),
        &banc.cache(),
        PLAFOND_DEFAUT_OCTETS,
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    banc.state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, source, source_id) VALUES (8, 'service', 'qobuz', 'q8')",
            &[],
        )
        .unwrap();
    let r = recalculer_dans(
        &banc.state,
        8,
        Banc::taille(),
        &banc.cache(),
        PLAFOND_DEFAUT_OCTETS,
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
