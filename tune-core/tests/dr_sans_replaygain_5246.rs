//! #5246 — la plage dynamique et les empreintes se calculent MÊME ReplayGain
//! coupé.
//!
//! Le défaut (Levente Toth, 0.9.166, fil 1997) : ReplayGain sur « Off », la
//! carte « Dynamic range » restait sur IDLE avec 1 468 pistes en attente, et
//! promettait la plage dynamique « quand le ReplayGain et les empreintes
//! n'ont plus rien à faire ». Rien ne démarrait jamais. Trois verrous, tous
//! tenus par le réglage ReplayGain :
//!
//! 1. la boucle de fond ne lançait la cascade que ReplayGain armé ;
//! 2. le rang ReplayGain, tenté en premier, lisait le réglage et rendait 0 —
//!    et les deux rattrapages relisaient le même réglage avant chaque fichier ;
//! 3. les prédicats des deux rattrapages exigeaient le témoin `rg_analyzed`,
//!    que seule la passe ReplayGain pose : ReplayGain coupé, aucune piste
//!    neuve n'était jamais candidate.
//!
//! Décision de Bertrand du 27/09/2026 : découpler. Seuls le calcul et
//! l'application du ReplayGain restent coupés.
//!
//! Ce témoin joue la cascade elle-même (`un_tour_de_cascade`), sur une vraie
//! piste décodable, dans les TROIS états « coupé » (mode Off, mode absent,
//! coche décochée). Et sa contre-épreuve de périmètre : ReplayGain ARMÉ, le
//! témoin reste exigé — la passe nominale décode la piste et pose DR et
//! empreinte elle-même, un rattrapage qui la prendrait aussi la décoderait
//! deux fois.
//!
//! Binaire à lui seul (`[[test]]`, `autotests = false`) : la pause, l'ordre
//! des passes et le verrou d'analyse sont des états de PROCESSUS.

use std::sync::Arc;

use tune_core::audio::replaygain::{
    TourDeCascade, compter_les_candidats_a_empreinter, compter_les_candidats_dr, un_tour_de_cascade,
};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SCHEMA: &str = "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
     CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                            updated_at TEXT NOT NULL DEFAULT '');
     CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                          duration_ms INTEGER, sample_rate INTEGER, channels INTEGER,
                          audio_fingerprint TEXT, format TEXT);
     CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                  value TEXT NOT NULL, PRIMARY KEY (track_id, key));";

/// Un WAV 16 bits stéréo de 9 s dont la plage dynamique vaut 10 dB : trois
/// blocs de 3 s, chacun ouvert par 13 200 échantillons de sinus pleine échelle
/// puis du silence. Même signal que `wav_de_plage_connue` (tests unitaires de
/// `replaygain.rs`), recopié parce qu'un test d'intégration ne voit pas les
/// aides `cfg(test)` de la caisse.
fn wav_de_plage_connue(chemin: &std::path::Path) {
    const SR: u32 = 44_100;
    const BLOCS: u32 = 3;
    const CYCLES: u32 = 132;
    let frames = (SR * 3 * BLOCS) as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let dans_le_bloc = i % (SR * 3) as usize;
        let v: i16 = if dans_le_bloc < (CYCLES * 100) as usize {
            let phase = (dans_le_bloc % 100) as f64 / 100.0;
            ((phase * std::f64::consts::TAU).sin() * 32_767.0).round() as i16
        } else {
            0
        };
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

/// Une piste neuve, jamais vue par la passe ReplayGain : ni `rg_analyzed`, ni
/// gain lu dans les tags. C'est l'état de toute la bibliothèque de Levente.
fn bibliotheque(dossier: &std::path::Path, reglages: &[(&str, &str)]) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(SCHEMA).expect("schéma");
    for (cle, valeur) in reglages {
        db.execute(
            "INSERT INTO settings (key, value) VALUES (?, ?)",
            &[cle, valeur],
        )
        .expect("réglage");
    }
    let fichier = dossier.join("plage.wav");
    wav_de_plage_connue(&fichier);
    let chemin = fichier.to_string_lossy().to_string();
    db.execute(
        "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels, format) \
         VALUES (42, NULL, ?, 9000, 44100, 2, 'wav')",
        &[&chemin],
    )
    .expect("piste");
    Arc::new(db)
}

fn cle(backend: &Arc<dyn DbBackend>, cle: &str) -> Option<String> {
    backend
        .query_one(
            "SELECT value FROM track_metadata WHERE track_id = 42 AND key = ?",
            &[&cle as &dyn tune_core::db::backend::ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
}

fn empreinte(backend: &Arc<dyn DbBackend>) -> Option<String> {
    backend
        .query_one("SELECT audio_fingerprint FROM tracks WHERE id = 42", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
}

/// Faire tourner la cascade jusqu'au repos, comme la boucle de fond.
async fn jusqu_au_repos(backend: &Arc<dyn DbBackend>) {
    for _ in 0..8 {
        match un_tour_de_cascade(backend).await {
            TourDeCascade::Travail(_) => continue,
            TourDeCascade::Repos => return,
            TourDeCascade::Suspendue(t) => panic!("aucune pause posée, suspendue sur {t:?}"),
        }
    }
    panic!("la cascade ne revient pas au repos");
}

/// Les trois états « ReplayGain coupé » : aucun ne doit plus priver la
/// bibliothèque de plage dynamique ni d'empreinte.
#[tokio::test]
async fn replaygain_coupe_la_cascade_mesure_quand_meme_la_plage_et_l_empreinte() {
    let _g = VERROU.lock().await;
    let cas: [(&str, &[(&str, &str)]); 3] = [
        ("replaygain_mode = off", &[("replaygain_mode", "off")]),
        ("replaygain_mode absent", &[]),
        (
            "coche « Analyse ReplayGain » décochée",
            &[
                ("replaygain_mode", "track"),
                ("replaygain_analysis_enabled", "false"),
            ],
        ),
    ];
    for (nom, reglages) in cas {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = bibliotheque(tmp.path(), reglages);

        assert_eq!(
            compter_les_candidats_dr(&backend),
            1,
            "[{nom}] la piste neuve doit être candidate à la plage dynamique — \
             le prédicat exige encore le témoin de la passe ReplayGain (#5246)"
        );
        assert_eq!(
            compter_les_candidats_a_empreinter(&backend),
            Some(1),
            "[{nom}] la piste neuve doit être candidate à l'empreinte (#5246)"
        );

        jusqu_au_repos(&backend).await;

        assert_eq!(
            cle(&backend, "dr_track").as_deref(),
            Some("10"),
            "[{nom}] ReplayGain coupé, la cascade n'a pas mesuré la plage \
             dynamique (#5246) — la plage du signal construit vaut 10 dB"
        );
        assert_eq!(cle(&backend, "dr_source").as_deref(), Some("analysis"));
        assert!(
            empreinte(&backend).is_some(),
            "[{nom}] ReplayGain coupé, la cascade n'a pas posé l'empreinte (#5246)"
        );
        // Le ReplayGain, lui, reste coupé : ni gain, ni témoin de sa passe.
        assert_eq!(
            cle(&backend, "rg_track_gain"),
            None,
            "[{nom}] ReplayGain coupé : aucun gain ne doit être calculé"
        );
        assert_eq!(
            cle(&backend, "rg_analyzed"),
            None,
            "[{nom}] le témoin ReplayGain ne doit pas être posé : armer le \
             ReplayGain plus tard doit encore mesurer la piste"
        );
        // Et la cascade ne boucle pas : plus rien de candidat.
        assert_eq!(compter_les_candidats_dr(&backend), 0, "[{nom}]");
        assert_eq!(
            compter_les_candidats_a_empreinter(&backend),
            Some(0),
            "[{nom}]"
        );
    }
}

/// Contre-épreuve de périmètre : ReplayGain ARMÉ, rien ne change. La piste
/// neuve n'est PAS candidate aux rattrapages — c'est la passe nominale qui la
/// décode, et elle pose DR et empreinte au passage (un seul décodage).
#[tokio::test]
async fn replaygain_arme_les_rattrapages_exigent_toujours_le_temoin() {
    let _g = VERROU.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path(), &[("replaygain_mode", "track")]);
    assert_eq!(
        compter_les_candidats_dr(&backend),
        0,
        "ReplayGain armé : le rattrapage DR ne doit pas doubler la passe nominale"
    );
    assert_eq!(compter_les_candidats_a_empreinter(&backend), Some(0));

    jusqu_au_repos(&backend).await;
    // La passe nominale a tout fait, en une fois.
    assert!(cle(&backend, "rg_analyzed").is_some());
    assert!(cle(&backend, "rg_track_gain").is_some());
    assert_eq!(cle(&backend, "dr_track").as_deref(), Some("10"));
    assert!(empreinte(&backend).is_some());
}

/// Le rang ReplayGain coupé est SAUTÉ, y compris quand il est aussi mis en
/// pause : une pause posée sur une passe éteinte ne doit pas arrêter la
/// descente vers les empreintes et la plage dynamique. Sans le saut, la
/// cascade rendrait `Suspendue(ReplayGain)` à chaque tour et rien ne
/// tournerait — le défaut de #5246 par un autre chemin.
#[tokio::test]
async fn une_pause_du_replaygain_coupe_ne_bloque_pas_la_plage_dynamique() {
    use tune_core::taches_de_fond::{Tache, mettre_en_pause, oublier_pour_les_essais};
    let _g = VERROU.lock().await;
    oublier_pour_les_essais();
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = bibliotheque(tmp.path(), &[("replaygain_mode", "off")]);
    mettre_en_pause(&backend, Tache::ReplayGain).expect("pause");

    let tour = un_tour_de_cascade(&backend).await;
    oublier_pour_les_essais();
    assert!(
        matches!(tour, TourDeCascade::Travail(_)),
        "ReplayGain coupé ET en pause : la cascade doit sauter son rang et \
         travailler sur les empreintes et la plage dynamique, pas s'arrêter \
         dessus (#5246) — rendu : {tour:?}"
    );
}
