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

/// Les deux témoins lisent l'avancement, un état de PROCESSUS : ils ne
/// tournent jamais en même temps.
static UN_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Des pistes FLAC dont le fichier répond et reste indécodable : chacune
/// traverse la passe des empreintes (marque « pas d'empreinte possible »)
/// sans qu'aucun décodeur ne travaille longtemps.
fn bibliotheque(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    bibliotheque_de(dossier, PISTES)
}

fn bibliotheque_de(dossier: &std::path::Path, pistes: i64) -> Arc<dyn DbBackend> {
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
    for i in 1..=pistes {
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
    let _seul = UN_A_LA_FOIS.lock().await;
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

/// Les pistes de la base simulée volumineuse.
const PISTES_D_UNE_GROSSE_BASE: i64 = 2_000;

/// #7915, consigne du 05/11 — chercher une passe qui recommence sans fin
/// après le scan, sur une base volumineuse simulée.
///
/// La vraie cascade tourne, ReplayGain coupé, jusqu'au repos sur 2 000 pistes
/// (80 lots de 25 par passe). Si une passe reprenait des pistes déjà traitées
/// (marque non posée, sélection qui repart du début sans rien retirer), le
/// total des pistes rendues par les tours dépasserait les candidates, la
/// campagne compterait plus que son total, ou le repos ne viendrait jamais.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_grosse_base_va_au_repos_sans_reprendre_une_piste_7915() {
    let _seul = UN_A_LA_FOIS.lock().await;
    avancement::reinitialiser_pour_les_essais();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque_de(tmp.path(), PISTES_D_UNE_GROSSE_BASE);

    // Deux passes (empreintes, puis plage dynamique), 80 lots chacune : une
    // marge large, mais finie. Au-delà, la cascade ne s'arrête pas.
    let tours_permis = 4 * (PISTES_D_UNE_GROSSE_BASE as usize / 25) + 20;
    let mut rendues = 0usize;
    let mut tours = 0usize;
    loop {
        match un_tour_de_cascade(&backend).await {
            TourDeCascade::Travail(n) => rendues += n,
            TourDeCascade::Repos => break,
            autre => panic!("aucune pause n'est posée : {autre:?}"),
        }
        tours += 1;
        assert!(
            tours <= tours_permis,
            "🔴 LA CASCADE NE S'ARRÊTE PAS (#7915) : {tours} tours, {rendues} pistes \
             rendues pour {PISTES_D_UNE_GROSSE_BASE} pistes — une passe reprend sans fin \
             les mêmes pistes."
        );
    }

    let empreintes = avancement::releve(Tache::Empreintes).expect("campagne des empreintes");
    let dr = avancement::releve(Tache::PlageDynamique).expect("campagne de la plage dynamique");
    for (nom, c) in [("empreintes", &empreintes), ("plage dynamique", &dr)] {
        assert!(!c.active, "{nom} : la campagne se ferme au repos : {c:?}");
        assert_eq!(
            c.total, PISTES_D_UNE_GROSSE_BASE,
            "{nom} : le total compte toutes les pistes, une fois : {c:?}"
        );
        assert_eq!(
            c.processed, PISTES_D_UNE_GROSSE_BASE,
            "🔴 PISTE REPRISE (#7915) — {nom} : chaque piste est traitée une fois \
             et une seule : {c:?}"
        );
    }
    assert_eq!(
        rendues,
        2 * PISTES_D_UNE_GROSSE_BASE as usize,
        "🔴 PISTE REPRISE (#7915) : les tours ont rendu {rendues} pistes, une par \
         piste et par passe en attendu"
    );

    // Au repos, la cascade y reste : rien ne repart au tour suivant.
    assert_eq!(un_tour_de_cascade(&backend).await, TourDeCascade::Repos);
}
