//! La clé du signal PCM des FLAC, `tracks.audio_pcm_key` (#5594, lot 1).
//!
//! ## Pourquoi une clé de plus
//!
//! Pour reconnaître une piste d'une instance à l'autre et partager ses mesures
//! (ReplayGain, plage dynamique), il faut une identité du SIGNAL exact :
//! deux pistes qui portent la même clé doivent donner, au bit près, la même
//! sonie et le même pic. Aucune colonne existante ne le dit :
//!
//! - `tracks.audio_hash` (`sample64k-v2`) hache des octets du conteneur : il
//!   change avec les tags ou la pochette ;
//! - `tracks.audio_fingerprint` (`env100ms-v1`) est insensible au niveau
//!   global : un remaster plus fort passerait pour le même contenu.
//!
//! Le MD5 que l'encodeur FLAC écrit dans STREAMINFO est celui des échantillons
//! DÉCODÉS. Il est gratuit : 42 octets d'en-tête, sans rien décoder. La clé
//! est [`crate::audio::flac_vendeur::EnteteFlac::cle_pcm`] :
//! `flac-md5-v1:<md5>:<total_samples>:<sample_rate>:<channels>:<bits>`.
//!
//! ## Ce qui n'en reçoit PAS
//!
//! - un FLAC dont le MD5 est nul (ffmpeg, et tous les enregistrements Qobuz
//!   du .18 mesurés au lot 0) : il ne dit rien du signal ;
//! - une piste de feuille CUE : le MD5 couvre le fichier image entier, pas la
//!   tranche ;
//! - tout ce qui n'est pas du FLAC (sélection SQL, puis marqueur `fLaC`).
//!
//! Une piste sans clé garde `audio_pcm_key` NUL. Jamais de clé inventée.
//!
//! ## La passe : en-tête seulement, en fond, reprenable
//!
//! Le témoin `tracks.audio_pcm_key_seen` garde l'`audio_hash` de l'état du
//! fichier sur lequel l'en-tête a été lu (`''` quand la piste n'en a pas).
//! NUL = jamais lu. Une piste est candidate quand le témoin est NUL ou
//! différent de son `audio_hash` actuel : une piste neuve (scan), une piste
//! d'avant cette version (le rattrapage) et un fichier remplacé depuis (le
//! scan a recalculé son `audio_hash`) passent donc par le MÊME chemin. Le scan
//! lui-même n'est pas touché.
//!
//! La comparaison est TEXTE contre TEXTE, sans `CAST` — un `CAST` casse sous
//! PostgreSQL dès qu'une valeur n'est pas numérique.
//!
//! Chaque piste lue est marquée aussitôt : un arrêt (redémarrage, pause)
//! perd au plus la piste en cours, et la passe suivante reprend exactement
//! les pistes restantes. Une piste dont le fichier ne s'ouvre pas (partage
//! démonté) n'est PAS marquée : elle sera relue plus tard.
//!
//! Aucune écriture dans les fichiers audio, aucun appel réseau.
//!
//! ## Quand elle tourne
//!
//! Au démarrage (après 90 s), à la fin de chaque scan de bibliothèque, puis
//! toutes les [`INTERVALLE`]. Jamais pendant un scan. Elle respecte la pause
//! de « ReplayGain » — c'est la mesure qu'elle prépare — et cède à la
//! lecture ([`super::priorite`]). Elle ne décode rien et ne prend donc pas le
//! créneau d'analyse des passes lourdes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};

/// Entre deux passes, faute de scan.
pub const INTERVALLE: Duration = Duration::from_secs(6 * 3600);

/// Attente au démarrage : laisser le serveur et le scan de démarrage
/// s'installer.
const ATTENTE_AU_DEMARRAGE: Duration = Duration::from_secs(90);

/// Cadence à laquelle la boucle regarde si un scan vient de finir.
const SONDE_DU_SCAN: Duration = Duration::from_secs(60);

/// Pistes lues par lot : une requête de sélection, puis une écriture par piste.
pub const LOT: usize = 500;

/// Ce qu'une passe a fait.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Pistes candidates examinées.
    pub candidates: usize,
    /// Pistes dont l'en-tête a été lu et le témoin posé.
    pub read: usize,
    /// … dont celles qui ont reçu une clé.
    pub keys_written: usize,
    /// … dont celles qui restent sans clé (MD5 nul, pas un FLAC, en-tête
    /// illisible).
    pub without_key: usize,
    /// Pistes dont le fichier ne s'ouvre pas : ni lues, ni marquées.
    pub missing: usize,
    /// La passe s'est arrêtée sur une pause de l'utilisateur.
    pub interrupted: bool,
    /// Durée de la passe.
    pub duration_ms: u64,
}

/// Les pistes à lire, par identifiant croissant, au-delà de `apres`.
///
/// `LOWER(file_path) LIKE '%.flac'` rattrape une ligne dont `format` serait
/// vide ; le marqueur `fLaC` tranche de toute façon à la lecture.
const CANDIDATS_SQL: &str = "SELECT t.id, t.file_path, COALESCE(t.audio_hash, '') \
     FROM tracks t \
     WHERE t.source = 'local' \
       AND t.file_path IS NOT NULL AND t.file_path != '' \
       AND (t.cue_media_path IS NULL OR t.cue_media_path = '') \
       AND (LOWER(t.format) = 'flac' OR LOWER(t.file_path) LIKE '%.flac') \
       AND (t.audio_pcm_key_seen IS NULL \
            OR t.audio_pcm_key_seen != COALESCE(t.audio_hash, '')) \
       AND t.id > ? \
     ORDER BY t.id \
     LIMIT ?";

/// La clé et le témoin, posés ensemble. La garde sur `audio_hash` refuse
/// l'écriture si un scan a changé le fichier depuis la sélection : la piste
/// reste candidate et sera relue.
const ECRITURE_SQL: &str = "UPDATE tracks SET audio_pcm_key = ?, audio_pcm_key_seen = ? \
     WHERE id = ? AND COALESCE(audio_hash, '') = ?";

struct Candidate {
    id: i64,
    chemin: String,
    hash: String,
}

fn lire_un_lot(backend: &Arc<dyn DbBackend>, apres: i64) -> Result<Vec<Candidate>, String> {
    let lot = LOT as i64;
    let lignes = backend.query_many(CANDIDATS_SQL, &[&apres as &dyn ToSqlValue, &lot])?;
    Ok(lignes
        .into_iter()
        .filter_map(|r| {
            Some(Candidate {
                id: r.first().and_then(|v| v.as_i64())?,
                chemin: r.get(1).and_then(|v| v.as_string())?,
                hash: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect())
}

/// Ce que la lecture d'un fichier a appris.
enum Lecture {
    /// Le fichier s'est ouvert : sa clé, ou son absence.
    Lue(Option<String>),
    /// Le fichier ne s'ouvre pas, sous aucune graphie.
    Introuvable,
}

/// Lire la clé d'un fichier : le chemin de la base d'abord, puis la graphie
/// que `resolve_local_path` trouve (base en NFC, disque en NFD sous macOS ou
/// SMB).
fn lire_la_cle(chemin: &str) -> Lecture {
    use crate::audio::flac_vendeur::cle_pcm_du_fichier;
    // #5299 — un fichier rangé dans une image ISO ne s'ouvre pas par son
    // chemin : son en-tête n'est pas lu, et il reste sans clé.
    if crate::audio::iso9660::est_chemin_virtuel(chemin) {
        return Lecture::Lue(None);
    }
    match cle_pcm_du_fichier(std::path::Path::new(chemin)) {
        Ok(cle) => return Lecture::Lue(cle),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Lecture::Introuvable,
        Err(_) => {}
    }
    match crate::library::local_path::resolve_local_path(chemin).found() {
        Some(reel) if reel != chemin => match cle_pcm_du_fichier(std::path::Path::new(&reel)) {
            Ok(cle) => Lecture::Lue(cle),
            Err(_) => Lecture::Introuvable,
        },
        _ => Lecture::Introuvable,
    }
}

/// Une passe complète. Synchrone : disque et base, à lancer hors des fils de
/// l'exécuteur.
pub fn rattraper(backend: &Arc<dyn DbBackend>) -> Bilan {
    use super::{Tache, est_en_pause, priorite};

    let debut = Instant::now();
    let mut bilan = Bilan::default();
    let mut apres = 0i64;
    loop {
        if est_en_pause(Tache::ReplayGain) {
            bilan.interrupted = true;
            break;
        }
        let lot = match lire_un_lot(backend, apres) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "cle_pcm_selection_echouee");
                break;
            }
        };
        let Some(dernier) = lot.last().map(|c| c.id) else {
            break;
        };
        bilan.candidates += lot.len();
        let mut ecritures: Vec<Vec<SqlValue>> = Vec::with_capacity(lot.len());
        for c in &lot {
            match lire_la_cle(&c.chemin) {
                Lecture::Lue(cle) => {
                    if cle.is_some() {
                        bilan.keys_written += 1;
                    } else {
                        bilan.without_key += 1;
                    }
                    ecritures.push(vec![
                        cle.map_or(SqlValue::NullText, SqlValue::Text),
                        SqlValue::Text(c.hash.clone()),
                        SqlValue::Int(c.id),
                        SqlValue::Text(c.hash.clone()),
                    ]);
                }
                Lecture::Introuvable => bilan.missing += 1,
            }
        }
        for r in backend.execute_many(ECRITURE_SQL, &ecritures) {
            match r {
                Ok(_) => bilan.read += 1,
                Err(e) => tracing::warn!(error = %e, "cle_pcm_ecriture_echouee"),
            }
        }
        apres = dernier;
        if lot.len() < LOT {
            break;
        }
        priorite::ceder_a_la_lecture_bloquant(Tache::ReplayGain.id());
    }
    bilan.duration_ms = debut.elapsed().as_millis() as u64;
    bilan
}

fn passe_journalisee(backend: &Arc<dyn DbBackend>) -> Bilan {
    let bilan = rattraper(backend);
    if bilan.candidates > 0 || bilan.interrupted {
        tracing::info!(
            candidates = bilan.candidates,
            lues = bilan.read,
            cles = bilan.keys_written,
            sans_cle = bilan.without_key,
            introuvables = bilan.missing,
            interrompue = bilan.interrupted,
            duree_ms = bilan.duration_ms,
            "cle_pcm_rattrapage"
        );
    }
    bilan
}

/// La boucle de fond. Appelée une fois, au démarrage du serveur.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    use super::{CADENCE_RELECTURE_PAUSE, Tache, est_en_pause};

    tokio::spawn(async move {
        tokio::time::sleep(ATTENTE_AU_DEMARRAGE).await;
        // `None` : la première passe part dès que le scan de démarrage le
        // permet.
        let mut derniere: Option<Instant> = None;
        let mut scan_vu = false;
        // Une passe interrompue par la pause reprend dès la reprise.
        let mut a_reprendre = false;
        loop {
            if est_en_pause(Tache::ReplayGain) {
                tokio::time::sleep(CADENCE_RELECTURE_PAUSE).await;
                continue;
            }
            // Pas pendant un scan : la base est à lui, et il est en train de
            // créer les pistes qu'on lira juste après.
            if crate::scanner::activite::scan_bibliotheque_en_cours() {
                scan_vu = true;
                tokio::time::sleep(SONDE_DU_SCAN).await;
                continue;
            }
            let due = a_reprendre || scan_vu || derniere.is_none_or(|t| t.elapsed() >= INTERVALLE);
            if !due {
                tokio::time::sleep(SONDE_DU_SCAN).await;
                continue;
            }
            scan_vu = false;
            let b = backend.clone();
            let bilan = super::priorite::hors_du_fil_async(Tache::ReplayGain.id(), move || {
                passe_journalisee(&b)
            })
            .await;
            a_reprendre = bilan.is_some_and(|b| b.interrupted);
            derniere = Some(Instant::now());
        }
    });
}

/// La même passe sur PostgreSQL : placeholders, `LIMIT` lié, comparaison
/// texte et NUL typé (`NullText`). Saute sans `TUNE_TEST_PG_URL` ; la base
/// doit porter les scripts numérotés (comme celle de `test-postgres.yml`).
#[cfg(all(test, feature = "postgres"))]
mod pg {
    use super::*;
    use crate::db::backend::PostgresBackend;

    fn cle(db: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
        let r = db
            .query_one(
                "SELECT audio_pcm_key, audio_pcm_key_seen FROM tracks WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .expect("piste");
        (
            r.first().and_then(|v| v.as_string()),
            r.get(1).and_then(|v| v.as_string()),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_5594_le_rattrapage_pose_la_cle() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));
        for sql in crate::db::postgres::ENSURE_COLUMNS {
            let _ = db.execute(sql, &[]);
        }
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
        let vrai = tmp.path().join("vrai.flac");
        std::fs::copy(&fixture, &vrai).unwrap();
        let mut octets = std::fs::read(&fixture).unwrap();
        octets[26..42].fill(0);
        let nul = tmp.path().join("nul.flac");
        std::fs::write(&nul, &octets).unwrap();
        let (vrai, nul) = (
            vrai.to_string_lossy().into_owned(),
            nul.to_string_lossy().into_owned(),
        );
        for (id, chemin, cue, hash) in [
            (1i64, Some(vrai.as_str()), None, Some("h1")),
            (2, Some(nul.as_str()), None, None),
            (3, None, Some(vrai.as_str()), None),
        ] {
            db.execute(
                "INSERT INTO tracks (id, title, file_path, cue_media_path, format, audio_hash, \
                 source) VALUES (?, 'piste', ?, ?, 'flac', ?, 'local')",
                &[&id as &dyn ToSqlValue, &chemin, &cue, &hash],
            )
            .unwrap();
        }

        let b = db.clone();
        let bilan = tokio::task::spawn_blocking(move || rattraper(&b))
            .await
            .unwrap();
        assert_eq!(
            (bilan.candidates, bilan.keys_written, bilan.without_key),
            (2, 1, 1)
        );
        assert_eq!(
            cle(&db, 1),
            (
                Some("flac-md5-v1:e3d5a52400c85f978eebd475ec8a10bf:44100:44100:2:16".into()),
                Some("h1".into())
            )
        );
        assert_eq!(cle(&db, 2), (None, Some(String::new())));
        assert_eq!(cle(&db, 3), (None, None));
        let b = db.clone();
        let bis = tokio::task::spawn_blocking(move || rattraper(&b))
            .await
            .unwrap();
        assert_eq!(bis.candidates, 0, "{bis:?}");
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();
    }
}
