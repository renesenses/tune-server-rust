//! #2713 — le rattrapage des crêtes vraies d'avant l'annexe 2 de BS.1770.
//!
//! Joue la cascade de fond elle-même (`un_tour_de_cascade`) sur une base
//! complète (schéma et migrations), avec une vraie piste décodable dont la
//! crête vraie est connue : une sinusoïde à fs/4 déphasée de 45°, amplitude
//! 0,9. Ses échantillons ne dépassent pas 0,636 ; l'ancien Catmull-Rom y
//! lisait 0,79 ; le signal continu culmine à 0,9.
//!
//! Ce qui est gardé :
//! - seules les crêtes de Tune d'une autre version sont refaites, et SEULE la
//!   crête change : gain, pic d'échantillon et provenance restent ;
//! - un fichier absent se reporte, un fichier illisible est marqué en échec
//!   pour cette version et garde son ancienne crête ;
//! - la pause du ReplayGain et la lecture arrêtent le rattrapage ;
//! - la crête d'album se refait quand toutes ses pistes sont à jour.
//!
//! Binaire à lui seul (`[[test]]`, `autotests = false`) : la pause, la
//! priorité à la lecture et le verrou d'analyse sont des états de PROCESSUS.

use std::sync::Arc;

use tune_core::audio::replaygain::rattrapage_crete::{
    ANCIEN_TRUE_PEAK_ALGO, TRUE_PEAK_ECHEC_KEY, compter_les_cretes_a_refaire, rattraper_un_lot,
};
use tune_core::audio::replaygain::{
    ALBUM_TRUE_PEAK_ALGO_KEY, TRUE_PEAK_ALGO, TRUE_PEAK_ALGO_KEY, TourDeCascade, passe_d_album,
    un_tour_de_cascade,
};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::{Tache, mettre_en_pause, reprendre};

static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const AMPLITUDE: f64 = 0.9;

/// WAV 16 bits stéréo, 44,1 kHz, 3 s : sinus à fs/4, phase 45°, amplitude
/// [`AMPLITUDE`], fondu d'entrée de 10 ms (un départ brutal sonnerait).
fn wav_over(chemin: &std::path::Path) {
    const SR: u32 = 44_100;
    let frames = (SR * 3) as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let fondu = (i as f64 / 441.0).min(1.0);
        let x = fondu
            * AMPLITUDE
            * (std::f64::consts::FRAC_PI_4 + std::f64::consts::FRAC_PI_2 * i as f64).sin();
        let v = (x * 32_767.0).round() as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    let n = pcm.len() as u32;
    let mut v: Vec<u8> = Vec::with_capacity(n as usize + 44);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + n).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&SR.to_le_bytes());
    v.extend_from_slice(&(SR * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&n.to_le_bytes());
    v.extend_from_slice(&pcm);
    std::fs::write(chemin, v).expect("wav témoin");
}

fn poser(backend: &Arc<dyn DbBackend>, id: i64, cle: &str, valeur: &str) {
    backend
        .execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?) \
             ON CONFLICT (track_id, key) DO UPDATE SET value = excluded.value",
            &[
                &id as &dyn ToSqlValue,
                &cle as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )
        .expect("clé");
}

fn lire(backend: &Arc<dyn DbBackend>, id: i64, cle: &str) -> Option<String> {
    backend
        .query_one(
            "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
            &[&id as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
}

/// Une mesure de Tune complète, crête Catmull-Rom étiquetée comme la
/// migration 121 le fait, plage dynamique présente (le rattrapage DR n'a rien
/// à faire).
fn mesure_ancienne(backend: &Arc<dyn DbBackend>, id: i64) {
    poser(backend, id, "rg_track_gain", "-6.50 dB");
    poser(backend, id, "rg_track_peak", "0.636000");
    poser(backend, id, "rg_track_true_peak", "0.790000");
    poser(backend, id, TRUE_PEAK_ALGO_KEY, ANCIEN_TRUE_PEAK_ALGO);
    poser(backend, id, "rg_track_source", "analysis");
    poser(backend, id, "rg_algo", "bs1770-tp4x-v2");
    poser(backend, id, "dr_track", "3");
}

/// 1 : crête ancienne, fichier lisible (album 1).
/// 2 : crête déjà à jour (album 1).
/// 3 : gain lu dans les tags, jamais mesuré par Tune.
/// 4 : crête ancienne, fichier absent.
/// 5 : crête ancienne, fichier illisible.
fn bibliotheque(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    backend
        .execute(
            "INSERT INTO settings (key, value) VALUES ('replaygain_mode', 'track')",
            &[],
        )
        .expect("réglage");
    backend
        .execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
    // `tracks.file_path` est unique : un fichier par piste, le même signal.
    let lisible = |n: u32| {
        let f = dossier.join(format!("over{n}.wav"));
        wav_over(&f);
        f
    };
    let illisible = dossier.join("illisible.wav");
    std::fs::write(&illisible, b"pas du tout un wav").unwrap();
    let absent = dossier.join("absent.wav");
    let chemins = [
        (1, lisible(1), "1"),
        (2, lisible(2), "1"),
        (3, lisible(3), "NULL"),
        (4, absent, "NULL"),
        (5, illisible, "NULL"),
    ];
    for (id, chemin, album) in chemins {
        backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                     sample_rate, channels) VALUES ({id}, 'P', {album}, 1, ?, 3000, 44100, 2)"
                ),
                &[&chemin.to_string_lossy().to_string() as &dyn ToSqlValue],
            )
            .unwrap();
    }
    for id in [1, 2, 4, 5] {
        mesure_ancienne(&backend, id);
    }
    poser(&backend, 2, "rg_track_true_peak", "0.950000");
    poser(&backend, 2, TRUE_PEAK_ALGO_KEY, TRUE_PEAK_ALGO);
    poser(&backend, 3, "rg_track_gain", "-3.00 dB");
    poser(&backend, 3, "rg_track_peak", "0.700000");
    poser(&backend, 3, "dr_track", "3");
    for id in [1, 2] {
        poser(&backend, id, "rg_album_gain", "-7.00 dB");
        poser(&backend, id, "rg_album_peak", "0.700000");
        poser(&backend, id, "rg_album_true_peak", "0.950000");
        poser(
            &backend,
            id,
            ALBUM_TRUE_PEAK_ALGO_KEY,
            ANCIEN_TRUE_PEAK_ALGO,
        );
        poser(&backend, id, "rg_album_source", "analysis");
    }
    backend
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

#[tokio::test(flavor = "multi_thread")]
async fn la_cascade_refait_la_seule_crete_des_mesures_anciennes() {
    let _g = VERROU.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path());
    assert_eq!(compter_les_cretes_a_refaire(&backend), Some(3), "1, 4 et 5");

    // ReplayGain, empreintes et plage dynamique n'ont rien à faire : le tour
    // tombe sur le dernier rang, le rattrapage des crêtes.
    match un_tour_de_cascade(&backend).await {
        TourDeCascade::Travail(n) => assert_eq!(n, 3, "1, 4 et 5 ont avancé"),
        autre => panic!("le rattrapage des crêtes n'a pas tourné : {autre:?}"),
    }

    // 1 : la crête est celle du signal continu, à 0,1 dB près ; le reste de
    // la mesure n'a pas bougé d'un caractère.
    let crete: f64 = lire(&backend, 1, "rg_track_true_peak")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (db(crete) - db(AMPLITUDE)).abs() <= 0.1,
        "crête refaite {crete}, attendu {AMPLITUDE} à 0,1 dB près (l'ancienne valait 0,79)"
    );
    assert_eq!(
        lire(&backend, 1, TRUE_PEAK_ALGO_KEY).as_deref(),
        Some(TRUE_PEAK_ALGO)
    );
    assert_eq!(
        lire(&backend, 1, "rg_track_gain").as_deref(),
        Some("-6.50 dB")
    );
    assert_eq!(
        lire(&backend, 1, "rg_track_peak").as_deref(),
        Some("0.636000")
    );
    assert_eq!(
        lire(&backend, 1, "rg_algo").as_deref(),
        Some("bs1770-tp4x-v2")
    );
    // 2 : déjà à jour, intacte.
    assert_eq!(
        lire(&backend, 2, "rg_track_true_peak").as_deref(),
        Some("0.950000")
    );
    // 3 : gain des tags, aucune crête inventée.
    assert_eq!(lire(&backend, 3, "rg_track_true_peak"), None);
    assert_eq!(lire(&backend, 3, TRUE_PEAK_ALGO_KEY), None);
    // 4 : reportée, ancienne crête gardée.
    assert!(lire(&backend, 4, "rg_path_unresolved").is_some());
    assert_eq!(
        lire(&backend, 4, "rg_track_true_peak").as_deref(),
        Some("0.790000")
    );
    assert_eq!(
        lire(&backend, 4, TRUE_PEAK_ALGO_KEY).as_deref(),
        Some(ANCIEN_TRUE_PEAK_ALGO)
    );
    // 5 : illisible, marquée en échec pour CETTE version, crête gardée.
    assert_eq!(
        lire(&backend, 5, TRUE_PEAK_ECHEC_KEY).as_deref(),
        Some(TRUE_PEAK_ALGO)
    );
    assert_eq!(
        lire(&backend, 5, "rg_track_true_peak").as_deref(),
        Some("0.790000")
    );
    assert_eq!(compter_les_cretes_a_refaire(&backend), Some(0));
    assert!(
        matches!(un_tour_de_cascade(&backend).await, TourDeCascade::Repos),
        "plus rien à refaire : la cascade revient au repos"
    );

    // L'album 1 : ses deux pistes sont à jour, sa crête se refait — le
    // maximum des crêtes de piste — avec la version courante.
    assert_eq!(passe_d_album(&backend, false).await, 1);
    let attendue = format!("{:.6}", crete.max(0.95));
    for id in [1, 2] {
        assert_eq!(
            lire(&backend, id, "rg_album_true_peak").as_deref(),
            Some(attendue.as_str())
        );
        assert_eq!(
            lire(&backend, id, ALBUM_TRUE_PEAK_ALGO_KEY).as_deref(),
            Some(TRUE_PEAK_ALGO)
        );
        assert_eq!(
            lire(&backend, id, "rg_album_gain").as_deref(),
            Some("-7.00 dB")
        );
    }
    assert_eq!(passe_d_album(&backend, false).await, 0, "rien de plus");
}

/// Une crête d'album ne se refait pas tant qu'une piste de l'album garde une
/// crête ancienne.
#[tokio::test(flavor = "multi_thread")]
async fn la_crete_d_album_attend_toutes_ses_pistes() {
    let _g = VERROU.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path());
    // La piste 1 n'est pas encore refaite.
    assert_eq!(passe_d_album(&backend, false).await, 0);
    assert_eq!(
        lire(&backend, 2, ALBUM_TRUE_PEAK_ALGO_KEY).as_deref(),
        Some(ANCIEN_TRUE_PEAK_ALGO)
    );
}

/// La pause du ReplayGain arrête le rattrapage, y compris s'il est le seul à
/// avoir du travail ; la reprise le relance.
#[tokio::test(flavor = "multi_thread")]
async fn la_pause_du_replaygain_arrete_le_rattrapage() {
    let _g = VERROU.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path());
    mettre_en_pause(&backend, Tache::ReplayGain).unwrap();
    let tour = un_tour_de_cascade(&backend).await;
    let lot = rattraper_un_lot(&backend).await;
    reprendre(&backend, Tache::ReplayGain).unwrap();
    assert!(
        matches!(tour, TourDeCascade::Suspendue(Tache::ReplayGain)),
        "{tour:?}"
    );
    assert_eq!(lot, 0, "en pause, le lot ne lance aucun fichier");
    assert_eq!(
        lire(&backend, 1, "rg_track_true_peak").as_deref(),
        Some("0.790000")
    );
    assert_eq!(compter_les_cretes_a_refaire(&backend), Some(3));
    assert!(matches!(
        un_tour_de_cascade(&backend).await,
        TourDeCascade::Travail(3)
    ));
}

/// Une zone en lecture : le rattrapage ne lance rien (#1310, #4699).
#[tokio::test(flavor = "multi_thread")]
async fn la_lecture_passe_avant_le_rattrapage() {
    let _g = VERROU.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path());
    backend
        .execute(
            "INSERT INTO zones (name, last_play_state) VALUES ('Salon', 'playing')",
            &[],
        )
        .expect("zone");
    assert_eq!(rattraper_un_lot(&backend).await, 0);
    assert_eq!(
        lire(&backend, 1, "rg_track_true_peak").as_deref(),
        Some("0.790000")
    );
    assert_eq!(compter_les_cretes_a_refaire(&backend), Some(3));
}
