//! #5519 — le gain ReplayGain APPROCHÉ depuis un rapport `foo_dr.txt`,
//! plafonné par la crête, quand la piste n'a aucun ReplayGain.
//!
//! Base réelle (schéma et migrations), rapport RÉEL de Tades (fil 2050,
//! foobar2000 1.3.13 / DR Meter 1.1.1, en russe :
//! `tests/fixtures/foo_dr_tades_2050_ru.txt`). La piste 1 y porte
//! `DR13  -4.93 дБ  -24.80 дБ` — l'exemple même du testeur : « si la pointe
//! est à −4,93 dB on peut sans risque appliquer un Replay gain de 4,9 dB ».
//!
//! Les témoins n'emploient que l'API publique d'avant le correctif et des
//! chaînes (`"foo_dr_mesure"`, `"foo_dr_approx"`) : avant #5519 ils
//! compilent et ROUGISSENT, ils ne tombent pas faute de symbole.
//!
//! `[[test]]` à lui seul dans `tune-core/Cargo.toml` (`autotests = false`).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tune_core::audio::replaygain::{
    PeakKind, ReplayGainMode, ReplayGainSettings, gain_factor_with_peak, stored_gain_source,
    stored_gain_with_peak,
};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::rapports_dr::rattraper;

const RAPPORT_TADES_RU: &[u8] = include_bytes!("fixtures/foo_dr_tades_2050_ru.txt");

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    Arc::new(db)
}

fn piste(b: &Arc<dyn DbBackend>, id: i64, dossier: &Path, nom: &str, numero: i64) {
    let fichier = dossier.join(nom);
    std::fs::write(&fichier, b"pas de l'audio").expect("fichier");
    let chemin = fichier.to_string_lossy().to_string();
    b.execute(
        "INSERT INTO tracks (id, title, file_path, track_number, disc_number, channels, format) \
         VALUES (?, 'x', ?, ?, 1, 2, 'dsf')",
        &[&id as &dyn ToSqlValue, &chemin, &numero],
    )
    .expect("insertion de piste");
}

fn poser(b: &Arc<dyn DbBackend>, id: i64, cle: &str, valeur: &str) {
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
        &[&id as &dyn ToSqlValue, &cle, &valeur],
    )
    .expect("métadonnée");
}

fn meta(b: &Arc<dyn DbBackend>, id: i64, cle: &str) -> Option<String> {
    b.query_one(
        "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
        &[&id as &dyn ToSqlValue, &cle],
    )
    .ok()
    .flatten()
    .and_then(|r| r.first().and_then(|v| v.as_string()))
}

/// La valeur que le scan range pour la piste 1 du rapport de Tades.
const MESURE_PISTE_1: &str = "v1;-4.93;-24.8";

/// Facteur appliqué et crête qui en résulte, anti-écrêtage COUPÉ : le
/// plafond doit tenir par le gain approché lui-même.
fn crete_apres_gain_db(b: &Arc<dyn DbBackend>, id: i64, plafond_db: f64) -> (f64, f64) {
    let (gain, granularite, kind) = stored_gain_with_peak(b, id, ReplayGainMode::Track).expect(
        "#5519 — une piste sans ReplayGain, dont le rapport foo_dr.txt donne la crête \
             et le RMS, doit recevoir un gain approché",
    );
    assert_eq!(granularite, ReplayGainMode::Track);
    assert_eq!(
        kind,
        PeakKind::SamplePeak,
        "la crête du rapport est une crête d'échantillon"
    );
    let settings = ReplayGainSettings {
        mode: ReplayGainMode::Track,
        preamp_db: 0.0,
        prevent_clipping: false,
        true_peak_ceiling_db: plafond_db,
    };
    let (facteur, _) = gain_factor_with_peak(gain, settings, kind);
    let applique_db = 20.0 * facteur.log10();
    (applique_db, -4.93 + applique_db)
}

/// Le cas de Tades : RMS −24,80 dB, soit ≈ +9,8 dB demandés par la sonie
/// estimée — la crête à −4,93 dBFS borne le gain à +4,93 dB (plafond 0 dBTP).
#[test]
fn le_gain_approche_est_borne_par_la_crete_du_rapport_5519() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("gain-approche-5519-borne");
    piste(&b, 1, &dossier, "01 - Trauermarsch.dsf", 1);
    poser(&b, 1, "foo_dr_mesure", MESURE_PISTE_1);

    let (applique_db, crete_db) = crete_apres_gain_db(&b, 1, 0.0);
    assert!(
        (applique_db - 4.93).abs() < 1e-6,
        "gain approché attendu +4,93 dB (borné par la crête), obtenu {applique_db:+.3} dB"
    );
    assert!(
        crete_db <= 1e-9,
        "crête après gain {crete_db:+.3} dBFS > 0 : écrêtage"
    );

    let source = stored_gain_source(&b, 1, ReplayGainMode::Track).expect("une provenance");
    assert_eq!(source.as_str(), "foo_dr_approx");
    assert_eq!(source.label_fr(), "approché (foo_dr)");
}

/// Le plafond dBTP réglé (−1) est respecté par le gain approché lui-même.
#[test]
fn le_plafond_dbtp_regle_est_respecte_5519() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("gain-approche-5519-plafond");
    piste(&b, 1, &dossier, "01 - Trauermarsch.dsf", 1);
    poser(&b, 1, "foo_dr_mesure", MESURE_PISTE_1);
    SettingsRepo::with_backend(b.clone())
        .set("replaygain_true_peak_ceiling_db", "-1")
        .expect("réglage");

    let (applique_db, crete_db) = crete_apres_gain_db(&b, 1, -1.0);
    assert!(
        (applique_db - 3.93).abs() < 1e-6,
        "gain attendu +3,93 dB, obtenu {applique_db:+.3} dB"
    );
    assert!(
        crete_db <= -1.0 + 1e-9,
        "crête après gain {crete_db:+.3} dBFS > −1 dBTP"
    );
}

/// Un ReplayGain réel (tag ou analyse) prime toujours sur l'approximation.
#[test]
fn un_replaygain_reel_prime_sur_l_approximation_5519() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("gain-approche-5519-reel");
    piste(&b, 1, &dossier, "01 - Trauermarsch.dsf", 1);
    poser(&b, 1, "foo_dr_mesure", MESURE_PISTE_1);
    poser(&b, 1, "rg_track_gain", "-2.50 dB");
    poser(&b, 1, "rg_track_source", "analysis");

    let (gain, _, _) = stored_gain_with_peak(&b, 1, ReplayGainMode::Track).expect("gain");
    assert!(
        (gain.gain_db - (-2.5)).abs() < 1e-9,
        "gain {}",
        gain.gain_db
    );
    assert_eq!(
        stored_gain_source(&b, 1, ReplayGainMode::Track).map(|s| s.as_str()),
        Some("analysis")
    );
}

/// Une bibliothèque déjà scannée : le DR de la piste vient DÉJÀ du rapport
/// (`dr_source = sidecar`, avant #5519), sans ReplayGain. La passe de
/// rattrapage relit le dossier, sans scan ni décodage, et lui donne sa crête
/// et son RMS — donc un gain approché.
#[test]
fn le_rattrapage_donne_ses_niveaux_a_une_piste_deja_pourvue_de_son_dr_5519() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("gain-approche-5519-rattrapage");
    std::fs::write(dossier.join("foo_dr.txt"), RAPPORT_TADES_RU).expect("rapport");
    piste(&b, 1, &dossier, "01 - Trauermarsch.dsf", 1);
    poser(&b, 1, "dr_track", "13");
    poser(&b, 1, "dr_source", "sidecar");
    piste(&b, 2, &dossier, "04 - Adagietto.dsf", 4);
    poser(&b, 2, "dr_track", "13");
    poser(&b, 2, "dr_source", "sidecar");

    let bilan = rattraper(&b, &HashSet::new());
    assert_eq!(
        meta(&b, 1, "foo_dr_mesure").as_deref(),
        Some(MESURE_PISTE_1),
        "#5519 — la passe doit garder crête et RMS du rapport. Bilan : {bilan:?}"
    );
    assert_eq!(
        meta(&b, 2, "foo_dr_mesure").as_deref(),
        Some("v1;-13.46;-29.99")
    );
    // Le DR n'est pas touché, et aucun ReplayGain n'est écrit.
    assert_eq!(meta(&b, 1, "dr_track").as_deref(), Some("13"));
    assert_eq!(meta(&b, 1, "rg_track_gain"), None);
    assert!(stored_gain_with_peak(&b, 1, ReplayGainMode::Track).is_some());

    // Une seconde passe ne revient pas sur elles.
    let second = rattraper(&b, &HashSet::new());
    assert_eq!(second.folders, 0, "plus rien à relire : {second:?}");
}
