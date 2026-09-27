//! #5169 — la place de la plage dynamique dans la cascade de fond.
//!
//! Chez Thierry (537 910 pistes), la plage dynamique décode en DERNIER :
//! après le ReplayGain et les empreintes, en alternance avec le CLAP. Le
//! réglage `taches_de_fond::ordre` la fait passer avant les empreintes (et le
//! CLAP), ou avant tout. Par défaut, l'ordre ne change pas.
//!
//! Chaque témoin monte UNE bibliothèque où les trois rangs ont du travail, et
//! regarde QUEL rang a écrit après un seul tour de cascade — ce qui est écrit
//! en base, pas ce que la fonction rend.
//!
//! Binaire à lui seul (`[[test]]` dans `tune-core/Cargo.toml`, `autotests =
//! false`) : le réglage, les pauses et le verrou d'analyse sont des états de
//! PROCESSUS ; les témoins se sérialisent entre eux.

use std::sync::Arc;

use tune_core::audio::replaygain::{TourDeCascade, progression, un_tour_de_cascade};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::ordre::{
    PrioriteDr, fixer_priorite_dr, le_clap_cede_a_la_plage_dynamique, priorite_dr,
};
use tune_core::taches_de_fond::{Tache, hydrater, mettre_en_pause, oublier_pour_les_essais};

static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SCHEMA: &str = "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
     CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                            updated_at TEXT NOT NULL DEFAULT '');
     CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                          duration_ms INTEGER, sample_rate INTEGER, channels INTEGER,
                          audio_fingerprint TEXT, format TEXT);
     CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                  value TEXT NOT NULL, PRIMARY KEY (track_id, key));";

/// Trois populations, deux pistes chacune, fichiers présents et indécodables
/// (la piste traverse la boucle sans qu'aucun décodeur ne travaille) :
///
/// * 1-2 : **ReplayGain** à faire (aucune clé) — `format = 'dsf'`, hors du
///   rang des empreintes ;
/// * 11-12 : ReplayGain fait, **plage dynamique** à faire, DSD donc sans
///   empreinte possible — seul le rang « plage dynamique » les prend ;
/// * 21-22 : ReplayGain fait, **empreinte** à faire, en FLAC — candidates des
///   empreintes ET de la plage dynamique : c'est l'ordre qui décide qui
///   passe le premier.
fn bibliotheque(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(SCHEMA).expect("schéma");
    db.execute(
        "INSERT INTO settings (key, value) VALUES ('replaygain_mode', 'track')",
        &[],
    )
    .expect("réglage");
    for (id, format, rg) in [
        (1i64, "dsf", false),
        (2, "dsf", false),
        (11, "dsf", true),
        (12, "dsf", true),
        (21, "flac", true),
        (22, "flac", true),
    ] {
        let fichier = dossier.join(format!("{id}.{format}"));
        std::fs::write(&fichier, b"pas de l'audio").expect("fichier temoin");
        let chemin = fichier.to_string_lossy().to_string();
        db.execute(
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels, format) \
             VALUES (?, NULL, ?, 300000, 44100, 2, ?)",
            &[&id, &chemin, &format],
        )
        .expect("piste");
        if rg {
            db.execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_analyzed', '1')",
                &[&id],
            )
            .expect("rg_analyzed");
        }
    }
    Arc::new(db)
}

fn compte(backend: &Arc<dyn DbBackend>, sql: &str) -> i64 {
    backend
        .query_one(sql, &[])
        .ok()
        .flatten()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// Pistes touchées par chaque rang, lues en base.
#[derive(Debug, PartialEq, Eq)]
struct Touchees {
    replaygain: i64,
    plage_dynamique: i64,
    empreintes: i64,
}

fn touchees(b: &Arc<dyn DbBackend>) -> Touchees {
    Touchees {
        replaygain: compte(
            b,
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'rg_analyzed' AND track_id < 10",
        ),
        plage_dynamique: compte(
            b,
            "SELECT COUNT(DISTINCT track_id) FROM track_metadata \
             WHERE key IN ('dr_track', 'dr_indisponible')",
        ),
        empreintes: compte(
            b,
            // Le rang ReplayGain pose lui aussi un témoin d'empreinte sur les
            // pistes qu'il décode (1-2) : seul le rang des empreintes touche
            // 21-22.
            "SELECT COUNT(*) FROM tracks WHERE id >= 21 AND audio_fingerprint IS NOT NULL",
        ),
    }
}

async fn preparer(
    priorite: PrioriteDr,
) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    oublier_pour_les_essais();
    tune_core::taches_de_fond::ordre::oublier_pour_les_essais();
    progression::reinitialiser_pour_les_essais();
    let dossier = tune_core::test_scratch::scratch_dir("ordre-des-passes-5169");
    let backend = bibliotheque(dossier.path());
    fixer_priorite_dr(&backend, priorite).expect("réglage d'ordre");
    (dossier, backend)
}

#[tokio::test]
async fn par_defaut_le_replaygain_passe_d_abord_et_la_plage_dynamique_attend() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::Derniere).await;
    let tour = un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert!(matches!(tour, TourDeCascade::Travail(_)), "{tour:?}");
    assert_eq!(
        t,
        Touchees {
            replaygain: 2,
            plage_dynamique: 0,
            empreintes: 0
        },
        "🔴 #5169 — par DÉFAUT l'ordre ne doit pas changer : le ReplayGain \
         d'abord, la plage dynamique en dernier"
    );
}

#[tokio::test]
async fn en_premier_la_plage_dynamique_passe_avant_le_replaygain() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::Premiere).await;
    let tour = un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert!(matches!(tour, TourDeCascade::Travail(_)), "{tour:?}");
    assert!(
        t.plage_dynamique > 0 && t.replaygain == 0 && t.empreintes == 0,
        "🔴 #5169 — réglée « en premier », la plage dynamique devait passer \
         AVANT le ReplayGain : {t:?}"
    );
    assert!(
        le_clap_cede_a_la_plage_dynamique(),
        "🔴 #5169 — la plage dynamique a du travail et passe avant le CLAP : \
         le CLAP devait lui céder son tour"
    );
}

#[tokio::test]
async fn avant_les_empreintes_la_plage_dynamique_passe_juste_apres_le_replaygain() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::AvantEmpreintes).await;
    // Premier tour : le ReplayGain garde sa place en tête.
    un_tour_de_cascade(&b).await;
    assert_eq!(touchees(&b).replaygain, 2, "{:?}", touchees(&b));
    assert_eq!(touchees(&b).plage_dynamique, 0);
    // Les pistes 1-2 viennent d'être analysées : elles sont désormais au
    // ReplayGain, et DSD, donc hors des empreintes. Tour suivant : la plage
    // dynamique AVANT les empreintes.
    un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert!(
        t.plage_dynamique > 0 && t.empreintes == 0,
        "🔴 #5169 — réglée « avant les empreintes », la plage dynamique devait \
         passer avant elles : {t:?}"
    );
}

/// Contre-épreuve de la précédente : dans l'ordre par défaut, le même
/// deuxième tour va aux EMPREINTES. Sans elle, une bibliothèque où les
/// empreintes n'auraient rien à faire rendrait le témoin précédent vert pour
/// une mauvaise raison.
#[tokio::test]
async fn contre_epreuve_par_defaut_le_second_tour_va_aux_empreintes() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::Derniere).await;
    un_tour_de_cascade(&b).await;
    un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert!(
        t.empreintes > 0 && t.plage_dynamique == 0,
        "le montage est en défaut : par défaut, le second tour va aux empreintes : {t:?}"
    );
    assert!(
        !le_clap_cede_a_la_plage_dynamique(),
        "par défaut, le CLAP ne cède jamais"
    );
}

/// La pause de l'utilisateur : suspendre la SEULE plage dynamique, placée en
/// tête, laisse le ReplayGain travailler — et le CLAP aussi.
#[tokio::test]
async fn une_plage_dynamique_avancee_et_suspendue_ne_bloque_pas_le_replaygain() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::Premiere).await;
    mettre_en_pause(&b, Tache::PlageDynamique).expect("pause");
    let tour = un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert_eq!(
        t,
        Touchees {
            replaygain: 2,
            plage_dynamique: 0,
            empreintes: 0
        },
        "🔴 #5169 — la plage dynamique est SUSPENDUE : elle ne décode pas, et \
         elle ne prend pas le ReplayGain en otage ({tour:?})"
    );
    assert!(
        !le_clap_cede_a_la_plage_dynamique(),
        "le CLAP ne cède pas à une passe suspendue"
    );
}

/// La priorité à la lecture : une zone joue, rien ne décode, quel que soit
/// l'ordre.
#[tokio::test]
async fn une_zone_qui_joue_arrete_la_plage_dynamique_meme_en_premier() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::Premiere).await;
    b.execute(
        "INSERT INTO zones (id, name, last_play_state) VALUES (1, 'Salon', 'playing')",
        &[],
    )
    .expect("zone");
    un_tour_de_cascade(&b).await;
    let t = touchees(&b);
    assert_eq!(
        t.plage_dynamique, 0,
        "🔴 #5169 — une zone joue : la plage dynamique, même en tête, doit céder \
         à la lecture ({t:?})"
    );
}

/// Le réglage survit au redémarrage : il est relu par `hydrater`, comme les
/// pauses.
#[tokio::test]
async fn le_reglage_survit_au_redemarrage() {
    let _s = VERROU.lock().await;
    let (_d, b) = preparer(PrioriteDr::AvantEmpreintes).await;
    tune_core::taches_de_fond::ordre::oublier_pour_les_essais();
    assert_eq!(priorite_dr(), PrioriteDr::Derniere, "miroir vidé");
    hydrater(&b);
    assert_eq!(
        priorite_dr(),
        PrioriteDr::AvantEmpreintes,
        "🔴 #5169 — le réglage d'ordre doit être relu au démarrage"
    );
}
