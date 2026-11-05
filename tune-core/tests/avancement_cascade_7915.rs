//! #7915 — l'avancement des empreintes est publié quand le ReplayGain est
//! coupé, ce qui est le réglage par défaut.
//!
//! Sur un NAS lent, la cascade a décodé toute une nuit après le scan (les
//! empreintes, puis la plage dynamique) sans qu'aucune route ne dise ce qui
//! tournait : seule la passe ReplayGain alimentait un avancement, et elle est
//! coupée par défaut. Ce témoin fait tourner la VRAIE cascade sur une base
//! sans `replaygain_mode` (donc coupé) et vérifie que la campagne des
//! empreintes s'ouvre avec son total, avance piste par piste, puis se ferme
//! quand tout est traité.
//!
//! Binaire à part (`[[test]]` dans `Cargo.toml`) : l'avancement est un état de
//! processus, un voisin qui ferait tourner la cascade le verrait bouger.

use std::sync::Arc;

use tune_core::audio::replaygain::{TourDeCascade, un_tour_de_cascade};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::Tache;
use tune_core::taches_de_fond::avancement;

const PISTES: i64 = 6;

/// Des pistes FLAC dont le fichier répond et reste indécodable : chacune
/// traverse la passe des empreintes (marque « pas d'empreinte possible »)
/// sans qu'aucun décodeur ne travaille longtemps.
fn bibliotheque(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(
        "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
         CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                                updated_at TEXT NOT NULL DEFAULT '');
         CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                              duration_ms INTEGER, sample_rate INTEGER, channels INTEGER,
                              audio_fingerprint TEXT, format TEXT);
         CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                      value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
    )
    .expect("schéma");
    for i in 1..=PISTES {
        let fichier = dossier.join(format!("{i}.flac"));
        std::fs::write(&fichier, b"pas du flac").expect("fichier temoin");
        let chemin = fichier.to_string_lossy().to_string();
        db.execute(
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels, format) \
             VALUES (?, NULL, ?, 300000, 44100, 2, 'flac')",
            &[&i, &chemin],
        )
        .expect("insertion de piste");
    }
    Arc::new(db)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_empreintes_publient_leur_avancement_replaygain_coupe_7915() {
    avancement::reinitialiser_pour_les_essais();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque(tmp.path());

    let premier = un_tour_de_cascade(&backend).await;
    assert!(
        matches!(premier, TourDeCascade::Travail(n) if n > 0),
        "la cascade doit travailler sur les empreintes, ReplayGain coupé : {premier:?}"
    );
    let a = avancement::releve(Tache::Empreintes).unwrap_or_else(|| {
        panic!(
            "🔴 AVANCEMENT NON PUBLIÉ (#7915) : les empreintes décodent la \
             bibliothèque, ReplayGain coupé, et aucune campagne n'est ouverte — \
             l'écran ne peut dire ni ce qui tourne, ni combien il en reste."
        )
    });
    assert!(a.active, "{a:?}");
    assert_eq!(
        a.total, PISTES,
        "le total compte toutes les candidates : {a:?}"
    );
    assert!(
        a.processed > 0 && a.processed <= PISTES,
        "les pistes traitées sont comptées : {a:?}"
    );
    assert!(
        a.eta_seconds.is_some(),
        "une estimation dès la première piste : {a:?}"
    );

    // Jusqu'au repos : la campagne se ferme, le compte reste.
    for _ in 0..10 {
        if matches!(un_tour_de_cascade(&backend).await, TourDeCascade::Repos) {
            break;
        }
    }
    let fin = avancement::releve(Tache::Empreintes).expect("campagne vue");
    assert!(!fin.active, "la campagne doit se fermer au repos : {fin:?}");
    assert_eq!(fin.processed, PISTES, "{fin:?}");
    assert_eq!(fin.remaining, 0, "{fin:?}");
}
