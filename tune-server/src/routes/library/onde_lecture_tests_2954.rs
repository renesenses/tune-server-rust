//! Témoins de l'enveloppe de la barre de lecture (fil du forum 2954) :
//! résolution demandée et bornée, ancien contrat sans paramètre, cache par
//! résolution (le cache en base des 200 points n'est pas touché), tranche CUE.

use super::*;
use axum::body::to_bytes;

const HZ: usize = 22_050;

/// Un WAV mono 16 bits de `secondes` s : silence pendant `silence_s`, puis
/// un signal plein (±20 000).
fn ecrire_wav(chemin: &CheminFs, secondes: usize, silence_s: usize) {
    let mut donnees = Vec::new();
    for i in 0..HZ * secondes {
        let v: i16 = if i < HZ * silence_s {
            0
        } else if i % 2 == 0 {
            20_000
        } else {
            -20_000
        };
        donnees.extend_from_slice(&v.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&(HZ as u32).to_le_bytes());
    w.extend_from_slice(&((HZ * 2) as u32).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
    w.extend_from_slice(&donnees);
    std::fs::write(chemin, w).unwrap();
}

struct Banc {
    state: AppState,
    dossier: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Banc {
    fn new() -> Self {
        Self {
            state: AppState::new(":memory:", 0, Default::default()).unwrap(),
            dossier: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        }
    }

    /// Une piste ordinaire de 4 s, dont la première moitié est muette.
    fn piste(&self, id: i64) -> PathBuf {
        let chemin = self.dossier.path().join(format!("piste_{id}.wav"));
        ecrire_wav(&chemin, 4, 2);
        self.state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, duration_ms, source) VALUES (?, 'onde', ?, 4000, 'local')",
                &[&id, &chemin.to_string_lossy().into_owned()],
            )
            .unwrap();
        chemin
    }

    /// Une piste CUE : la tranche `[debut_ms, fin_ms]` d'une image de 4 s
    /// (silence sur [0, 2 s], son ensuite).
    fn piste_cue(&self, id: i64, debut_ms: i64, fin_ms: Option<i64>) {
        let image = self.dossier.path().join("image.wav");
        if !image.exists() {
            ecrire_wav(&image, 4, 2);
        }
        self.state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, file_path, cue_media_path, cue_start_ms, cue_end_ms, duration_ms, source) VALUES (?, 'cue', NULL, ?, ?, ?, 2000, 'local')",
                &[
                    &id,
                    &image.to_string_lossy().into_owned(),
                    &debut_ms,
                    &fin_ms,
                ],
            )
            .unwrap();
    }

    async fn demander(&self, id: i64, points: Option<usize>) -> (StatusCode, Value) {
        let r = servir(
            &self.state,
            id,
            Parametres { points },
            self.cache.path(),
            PLAFOND_DEFAUT_OCTETS,
        )
        .await;
        let statut = r.status();
        let corps = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        (
            statut,
            serde_json::from_slice(&corps).unwrap_or(Value::Null),
        )
    }

    fn en_base(&self, id: i64) -> Option<String> {
        TrackRepo::with_backend(self.state.backend.clone())
            .get_waveform(id)
            .unwrap()
    }

    fn fichiers_du_cache(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.cache.path())
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}

fn onde(v: &Value) -> Vec<f64> {
    v["waveform"]
        .as_array()
        .unwrap_or_else(|| panic!("pas d'onde : {v}"))
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect()
}

#[tokio::test]
async fn sans_parametre_l_ancien_contrat_de_200_points_en_base() {
    let banc = Banc::new();
    banc.piste(1);
    let (statut, v) = banc.demander(1, None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(onde(&v).len(), 200, "un ancien client reçoit 200 points");
    assert_eq!(v["points"], 200);
    assert_eq!(v["track_id"], 1);
    let en_base: Vec<f32> = serde_json::from_str(&banc.en_base(1).expect("mis en base")).unwrap();
    assert_eq!(en_base.len(), 200);
    assert!(
        banc.fichiers_du_cache().is_empty(),
        "les 200 points ne vont pas au cache disque"
    );
}

#[tokio::test]
async fn la_resolution_demandee_est_respectee_et_mise_en_cache_par_resolution() {
    let banc = Banc::new();
    banc.piste(2);
    // Un ancien cache de 200 points en base, déjà là.
    let ancien = serde_json::to_string(&vec![0.5f32; 200]).unwrap();
    TrackRepo::with_backend(banc.state.backend.clone())
        .set_waveform(2, &ancien)
        .unwrap();

    let (_, v) = banc.demander(2, Some(1000)).await;
    let fine = onde(&v);
    assert_eq!(fine.len(), 1000, "1 000 points demandés, 1 000 rendus");
    assert_eq!(v["points"], 1000);
    // La première moitié est muette, la seconde pleine : la vraie piste, pas
    // l'ancien cache.
    assert!(fine[..490].iter().all(|x| *x < 0.01));
    assert!(fine[510..].iter().all(|x| *x > 0.9));

    // L'ancien cache est intact et resservi tel quel aux anciens clients.
    assert_eq!(banc.en_base(2).as_deref(), Some(ancien.as_str()));
    let (_, v) = banc.demander(2, None).await;
    assert_eq!(onde(&v), vec![0.5; 200]);

    // Une entrée par résolution dans le cache disque.
    let (_, v) = banc.demander(2, Some(500)).await;
    assert_eq!(onde(&v).len(), 500);
    let fichiers = banc.fichiers_du_cache();
    assert_eq!(fichiers.len(), 2, "{fichiers:?}");
    assert!(
        fichiers.iter().any(|f| f.ends_with("-1000.json")),
        "{fichiers:?}"
    );
    assert!(
        fichiers.iter().any(|f| f.ends_with("-500.json")),
        "{fichiers:?}"
    );
}

#[tokio::test]
async fn le_second_appel_sert_le_cache_de_sa_resolution() {
    let banc = Banc::new();
    banc.piste(3);
    banc.demander(3, Some(1000)).await;
    let nom = banc
        .fichiers_du_cache()
        .into_iter()
        .find(|f| f.ends_with("-1000.json"))
        .expect("en cache");
    // Le cache est réécrit : s'il est lu, c'est lui qui revient.
    let marque = vec![0.25f32; 1000];
    std::fs::write(
        banc.cache.path().join(&nom),
        serde_json::to_vec(&marque).unwrap(),
    )
    .unwrap();
    let (_, v) = banc.demander(3, Some(1000)).await;
    assert_eq!(
        onde(&v),
        vec![0.25; 1000],
        "le cache de 1 000 points est servi"
    );
}

#[tokio::test]
async fn un_fichier_remplace_ne_sert_pas_une_onde_perimee() {
    let banc = Banc::new();
    let chemin = banc.piste(4);
    banc.demander(4, Some(1000)).await;
    // Remplacé par une piste de 6 s (taille différente) : autre empreinte.
    ecrire_wav(&chemin, 6, 0);
    let (_, v) = banc.demander(4, Some(1000)).await;
    assert!(
        onde(&v).iter().all(|x| *x > 0.9),
        "la nouvelle piste est pleine"
    );
    assert_eq!(banc.fichiers_du_cache().len(), 2);
}

#[tokio::test]
async fn la_resolution_est_bornee() {
    let banc = Banc::new();
    banc.piste(5);
    let (_, v) = banc.demander(5, Some(3)).await;
    assert_eq!(onde(&v).len(), 100, "au moins 100 points");
    let (_, v) = banc.demander(5, Some(1_000_000)).await;
    assert_eq!(onde(&v).len(), 2000, "au plus 2 000 points");
}

#[tokio::test]
async fn une_piste_cue_rend_sa_tranche() {
    let banc = Banc::new();
    // [1 s, 3 s] : moitié silence, moitié son.
    banc.piste_cue(10, 1_000, Some(3_000));
    // [2 s, fin] : que du son (dernière piste de la feuille).
    banc.piste_cue(11, 2_000, None);

    let (_, v) = banc.demander(10, Some(1000)).await;
    let t = onde(&v);
    assert_eq!(t.len(), 1000);
    assert!(t[..450].iter().all(|x| *x < 0.01), "début de tranche muet");
    assert!(t[550..].iter().all(|x| *x > 0.9), "fin de tranche pleine");

    let (_, v) = banc.demander(11, Some(1000)).await;
    assert!(
        onde(&v).iter().all(|x| *x > 0.9),
        "dernière piste : que du son"
    );

    // L'ancien contrat aussi, pour une piste CUE (avant : « no file path »).
    let (_, v) = banc.demander(10, None).await;
    assert_eq!(onde(&v).len(), 200);
    assert!(banc.en_base(10).is_some());

    let fichiers = banc.fichiers_du_cache();
    assert!(
        fichiers
            .iter()
            .any(|f| f.contains("-cue1000-3000-1000.json")),
        "{fichiers:?}"
    );
    assert!(
        fichiers
            .iter()
            .any(|f| f.contains("-cue2000-fin-1000.json")),
        "{fichiers:?}"
    );
}

#[tokio::test]
async fn sans_fichier_ou_inconnue() {
    let banc = Banc::new();
    banc.state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, file_path, duration_ms, source, source_id) VALUES (20, 'q', NULL, 1000, 'qobuz', '123')",
            &[],
        )
        .unwrap();
    let (statut, v) = banc.demander(20, Some(1000)).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["waveform"], Value::Null);
    assert_eq!(v["error"], "no file path");

    let r = servir(
        &banc.state,
        999,
        Parametres { points: Some(1000) },
        banc.cache.path(),
        PLAFOND_DEFAUT_OCTETS,
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
