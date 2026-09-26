//! #5108 (suite de #5073) — le SCAN COMPLET ne confrontait pas la base aux
//! feuilles CUE relues.
//!
//! - Feuille `.cue` supprimée : l'image était réimportée entière, et ses
//!   anciennes tranches restaient à côté. L'album existait deux fois.
//! - Feuille retouchée (INDEX déplacé) : la nouvelle tranche était écrite,
//!   l'ancienne restait.
//!
//! Ces épreuves exécutent les VRAIS scans (`spawn_library_scan` et
//! `spawn_auto_scan`) sur une base SQLite de FICHIER, et de vrais FLAC posés
//! sous une racine de musique.
use super::scan_realigne_tests_4896::{scan_de_demarrage, scan_manuel};
use super::surveillant_retouche_tests_4896::flac_8_canaux;
use crate::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

/// 3 trames CD = 40 ms ; 6 trames = 80 ms.
const PISTE_2_MS: i64 = 40;
const PISTE_2_DEPLACEE_MS: i64 = 80;

struct Bibliotheque {
    _base: tune_core::test_scratch::ScratchDir,
    _racine: tune_core::test_scratch::ScratchDir,
    etat: AppState,
    db: Arc<dyn DbBackend>,
    image: PathBuf,
    cue: PathBuf,
    /// Un album ordinaire, sans feuille, à côté.
    ordinaire: PathBuf,
}

fn chaine(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn feuille(titre_2: &str, debut_2: &str) -> String {
    format!(
        "PERFORMER \"Alain Bashung\"\r\n\
         TITLE \"Osez Joséphine\"\r\n\
         FILE \"Osez Joséphine.flac\" WAVE\r\n\
         \x20 TRACK 01 AUDIO\r\n\
         \x20   TITLE \"J'écume\"\r\n\
         \x20   INDEX 01 00:00:00\r\n\
         \x20 TRACK 02 AUDIO\r\n\
         \x20   TITLE \"{titre_2}\"\r\n\
         \x20   INDEX 01 {debut_2}\r\n"
    )
}

/// Une base de FICHIER, un album « image + feuille » et un album ordinaire.
/// Racine sous le dossier courant : `is_tune_temp_file` écarte tout ce qui
/// vit sous le dossier temporaire du système.
fn bibliotheque(epreuve: &str) -> Bibliotheque {
    let base = tune_core::test_scratch::scratch_dir(&format!("scan-cue-5108-base-{epreuve}"));
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("scan-cue-5108-{epreuve}"),
    );
    let etat = AppState::new(
        &chaine(&base.join("tune-epreuve.db")),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db = etat.backend.clone();
    SettingsRepo::with_backend(db.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();

    let dossier = racine.join("Alain Bashung").join("1991 - Osez Joséphine");
    std::fs::create_dir_all(&dossier).unwrap();
    let image = dossier.join("Osez Joséphine.flac");
    let cue = dossier.join("Osez Joséphine.cue");
    std::fs::write(&image, flac_8_canaux()).unwrap();
    std::fs::write(&cue, feuille("Volutes (remix)", "00:00:03")).unwrap();

    let autre = racine.join("Pink Floyd").join("Meddle");
    std::fs::create_dir_all(&autre).unwrap();
    let ordinaire = autre.join("01 - One of These Days.flac");
    std::fs::write(&ordinaire, flac_8_canaux()).unwrap();

    Bibliotheque {
        _base: base,
        _racine: racine,
        etat,
        db,
        image,
        cue,
        ordinaire,
    }
}

/// `(id, titre, file_path, cue_media_path, cue_start_ms)`, triées par chemin
/// puis début.
type Ligne = (i64, String, Option<String>, Option<String>, Option<i64>);

fn pistes(db: &Arc<dyn DbBackend>) -> Vec<Ligne> {
    db.query_many(
        "SELECT id, title, file_path, cue_media_path, cue_start_ms FROM tracks \
         ORDER BY COALESCE(cue_media_path, file_path), cue_start_ms",
        &[],
    )
    .unwrap()
    .iter()
    .map(|r| {
        (
            r[0].as_i64().unwrap(),
            r[1].as_string().unwrap_or_default(),
            r[2].as_string(),
            r[3].as_string(),
            r[4].as_i64(),
        )
    })
    .collect()
}

/// Les pistes de l'image : `(file_path, cue_media_path, cue_start_ms)`.
fn pistes_de_l_image(b: &Bibliotheque) -> Vec<(Option<String>, Option<String>, Option<i64>)> {
    let image = chaine(&b.image);
    pistes(&b.db)
        .into_iter()
        .filter(|p| p.2.as_deref() == Some(&image) || p.3.as_deref() == Some(&image))
        .map(|p| (p.2, p.3, p.4))
        .collect()
}

/// La ligne de l'album ordinaire, telle quelle.
fn ligne_ordinaire(b: &Bibliotheque) -> Ligne {
    let chemin = chaine(&b.ordinaire);
    pistes(&b.db)
        .into_iter()
        .find(|p| p.2.as_deref() == Some(&chemin))
        .expect("l'album ordinaire est indexé")
}

fn decoupee(
    b: &Bibliotheque,
    debuts: &[i64],
) -> Vec<(Option<String>, Option<String>, Option<i64>)> {
    debuts
        .iter()
        .map(|d| (None, Some(chaine(&b.image)), Some(*d)))
        .collect()
}

/// Premier scan : l'image est découpée en deux tranches.
async fn premier_scan(b: &Bibliotheque) -> Ligne {
    scan_manuel(&b.etat).await;
    assert_eq!(
        pistes_de_l_image(b),
        decoupee(b, &[0, PISTE_2_MS]),
        "montage : la feuille découpe l'image"
    );
    ligne_ordinaire(b)
}

/// LE TÉMOIN — feuille supprimée, scan manuel : l'image redevient UNE piste
/// entière, et ses tranches s'en vont. Plus de doublon.
#[tokio::test]
async fn la_feuille_supprimee_ne_laisse_pas_de_doublon_au_scan_5108() {
    let b = bibliotheque("supprimee");
    let ordinaire = premier_scan(&b).await;

    std::fs::remove_file(&b.cue).unwrap();
    scan_manuel(&b.etat).await;

    assert_eq!(
        pistes_de_l_image(&b),
        vec![(Some(chaine(&b.image)), None, None)],
        "feuille supprimée : une seule piste, l'image entière, sans ses anciennes tranches"
    );
    assert_eq!(
        ligne_ordinaire(&b),
        ordinaire,
        "l'album sans feuille n'a pas bougé"
    );
}

/// Le même défaut au scan de DÉMARRAGE (`auto_scan`).
#[tokio::test]
async fn la_feuille_supprimee_ne_laisse_pas_de_doublon_au_scan_de_demarrage_5108() {
    let b = bibliotheque("supprimee-demarrage");
    scan_de_demarrage(&b.db).await;
    assert_eq!(
        pistes_de_l_image(&b),
        decoupee(&b, &[0, PISTE_2_MS]),
        "montage : la feuille découpe l'image"
    );
    let ordinaire = ligne_ordinaire(&b);

    std::fs::remove_file(&b.cue).unwrap();
    scan_de_demarrage(&b.db).await;

    assert_eq!(
        pistes_de_l_image(&b),
        vec![(Some(chaine(&b.image)), None, None)],
        "feuille supprimée : une seule piste, l'image entière, sans ses anciennes tranches"
    );
    assert_eq!(
        ligne_ordinaire(&b),
        ordinaire,
        "l'album sans feuille n'a pas bougé"
    );
}

/// LE TÉMOIN — INDEX 01 de la piste 2 déplacé de 40 à 80 ms : la tranche à
/// 40 ms s'en va, celle à 80 ms la remplace.
#[tokio::test]
async fn l_index_deplace_met_les_tranches_a_jour_au_scan_5108() {
    let b = bibliotheque("index");
    let ordinaire = premier_scan(&b).await;

    std::fs::write(&b.cue, feuille("Volutes (remix)", "00:00:06")).unwrap();
    scan_manuel(&b.etat).await;

    assert_eq!(
        pistes_de_l_image(&b),
        decoupee(&b, &[0, PISTE_2_DEPLACEE_MS]),
        "INDEX déplacé : l'ancienne tranche est retirée"
    );
    assert_eq!(
        ligne_ordinaire(&b),
        ordinaire,
        "l'album sans feuille n'a pas bougé"
    );
}

/// Un dossier SANS feuille, scanné deux fois, garde exactement sa ligne.
#[tokio::test]
async fn un_dossier_sans_feuille_est_inchange_5108() {
    let b = bibliotheque("sans-feuille");
    std::fs::remove_file(&b.cue).unwrap();
    std::fs::remove_file(&b.image).unwrap();
    scan_manuel(&b.etat).await;
    let avant = pistes(&b.db);
    assert_eq!(
        avant.len(),
        1,
        "montage : l'album ordinaire seul : {avant:?}"
    );
    scan_manuel(&b.etat).await;
    assert_eq!(pistes(&b.db), avant, "aucune ligne retirée ni ajoutée");
}

/// ⚠️ Feuille présente mais ILLISIBLE (droits, écriture en cours) : aucune
/// tranche n'est retirée sur ce doute.
#[cfg(unix)]
#[tokio::test]
async fn une_feuille_illisible_ne_retire_rien_5108() {
    use std::os::unix::fs::PermissionsExt;
    let b = bibliotheque("illisible");
    premier_scan(&b).await;

    std::fs::write(&b.cue, feuille("Volutes (remix)", "00:00:06")).unwrap();
    std::fs::set_permissions(&b.cue, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::read(&b.cue).is_err(),
        "montage : la feuille doit être illisible (épreuve lancée en root ?)"
    );
    scan_manuel(&b.etat).await;
    std::fs::set_permissions(&b.cue, std::fs::Permissions::from_mode(0o644)).unwrap();

    let tranches: Vec<_> = pistes_de_l_image(&b)
        .into_iter()
        .filter(|p| p.1.is_some())
        .collect();
    assert_eq!(
        tranches,
        decoupee(&b, &[0, PISTE_2_MS]),
        "feuille illisible : les tranches en base restent toutes"
    );
}
