//! #6332 — compléter le débit des pistes déjà en base, au rescan ordinaire.
//!
//! Fil forum 2269 : « Serait-il possible d'ajouter le débit binaire aux
//! informations affichées à la lecture en cours ? ». Le débit moyen est lu au
//! scan (`TrackMetadata::bitrate_kbps`) et stocké depuis la migration 130
//! (`tracks.bitrate_kbps`). Mais un scan incrémental ne relit JAMAIS un fichier
//! inchangé : sans ce module, une bibliothèque existante n'aurait de débit que
//! pour les fichiers modifiés, ou après un « Scan complet » qui relit tout
//! (20 284 fichiers sur 20 348 relus au premier démarrage, ticket 201 — c'est
//! précisément ce que Bertrand a écarté le 30/09).
//!
//! À la fin de chaque scan, on complète donc les lignes encore SANS débit :
//!
//! 1. **DSD** (`dsf`, `dff`) : débit constant, 1 bit par échantillon et par
//!    canal — calculé en SQL depuis `sample_rate` et `channels`, sans ouvrir
//!    un fichier ;
//! 2. **le reste** : une lecture des SEULES propriétés audio (lofty, sans
//!    balises ni pochette), par lots de [`LOT_PAR_SCAN`] et dans un budget de
//!    temps : une grosse bibliothèque se complète sur quelques scans plutôt
//!    que d'allonger le premier de plusieurs minutes.
//!
//! Un fichier dont le débit ne se lit pas reçoit [`DEBIT_INCONNU`] (0) : il
//! n'est pas relu à chaque scan. Le prochain scan qui le relit en entier
//! (fichier modifié, « Scan complet ») récrit la vraie valeur. Un fichier qui
//! ne RÉPOND pas est marqué de même avant que la passe s'arrête : sans quoi
//! elle bloquerait au même endroit à chaque scan, et les pistes suivantes ne
//! seraient jamais complétées.
//!
//! La passe tourne dans son propre fil ([`lancer_en_fond`]), APRÈS que le scan
//! a rendu le droit de scanner : un nouveau scan n'est jamais refusé à cause
//! d'elle, et elle lui cède la place (elle s'interrompt dès qu'un scan tient
//! le droit). Une seule à la fois. Le réglage [`CLE_REGLAGE`] à `false` la
//! coupe.
//!
//! Rien n'est supprimé, aucune autre colonne n'est touchée.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Pistes au plus relues par scan.
pub const LOT_PAR_SCAN: usize = 5_000;

/// Temps au plus consacré aux lectures par scan.
pub const BUDGET_PAR_SCAN: Duration = Duration::from_secs(180);

/// Au-delà, un fichier qui ne rend pas ses propriétés arrête la passe : le
/// stockage ne répond plus, inutile d'en attendre 5 000 autres.
const DELAI_PAR_FICHIER: Duration = Duration::from_secs(10);

/// « Lu, débit inconnu » : la ligne n'est plus candidate au complément.
pub const DEBIT_INCONNU: i64 = 0;

/// Réglage qui coupe la passe quand il vaut `false` (absent : active).
pub const CLE_REGLAGE: &str = "scan_bitrate_backfill";

/// Attente au plus que le scan qui vient de finir rende le droit de scanner.
const ATTENTE_DU_DROIT: Duration = Duration::from_secs(600);

/// Une passe à la fois.
static EN_COURS: AtomicBool = AtomicBool::new(false);

/// Ce que la passe a fait, pour le journal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RapportDebits {
    /// Lignes DSD complétées par le calcul.
    pub dsd_calcules: usize,
    /// Fichiers relus dont le débit a été écrit.
    pub lus: usize,
    /// Fichiers relus sans débit lisible (marqués [`DEBIT_INCONNU`]).
    pub inconnus: usize,
    /// La passe s'est arrêtée avant la fin (lot, budget ou stockage muet) :
    /// le scan suivant reprend.
    pub interrompue: bool,
}

fn ph(db: &dyn DbBackend, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// Le débit moyen d'un fichier, en kbit/s, par ses seules propriétés audio.
pub fn lire_debit_kbps(path: &Path) -> Option<u32> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::AudioFile;
    use lofty::probe::Probe;

    let tagged = Probe::open(&*crate::library::artwork::extended_path(path))
        .and_then(|p| {
            p.options(
                ParseOptions::new()
                    .parsing_mode(ParsingMode::Relaxed)
                    .max_junk_bytes(1024 * 1024)
                    .read_tags(false)
                    .read_cover_art(false),
            )
            .guess_file_type()?
            .read()
        })
        .ok()?;
    tagged.properties().audio_bitrate().filter(|b| *b > 0)
}

/// [`lire_debit_kbps`] borné à `delai` : `Err(())` si le fichier ne répond pas.
fn lire_borne(path: &Path, delai: Duration) -> Result<Option<u32>, ()> {
    let (rendre, reponse) = mpsc::channel();
    let chemin = path.to_path_buf();
    // Un fil par fichier : un montage réseau muet ne s'interrompt pas, on
    // cesse seulement d'attendre (même principe que le `stat` du scan).
    let lance = std::thread::Builder::new()
        .name("debit-piste".into())
        .spawn(move || {
            let _ = rendre.send(lire_debit_kbps(&chemin));
        });
    if lance.is_err() {
        return Ok(lire_debit_kbps(path));
    }
    reponse.recv_timeout(delai).map_err(|_| ())
}

/// Complète en SQL le débit des pistes DSD encore sans débit.
fn completer_le_dsd(db: &dyn DbBackend) -> usize {
    let sql = "UPDATE tracks SET bitrate_kbps = (sample_rate * channels) / 1000 \
               WHERE bitrate_kbps IS NULL AND LOWER(format) IN ('dsf', 'dff') \
               AND sample_rate > 0 AND channels > 0";
    match db.execute(sql, &[]) {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "debit_dsd_non_complete");
            0
        }
    }
}

/// Les pistes locales encore sans débit, au plus `limite`.
fn candidates(db: &dyn DbBackend, limite: usize) -> Vec<(i64, String)> {
    let sql = format!(
        "SELECT id, file_path FROM tracks \
         WHERE bitrate_kbps IS NULL AND file_path IS NOT NULL AND file_path <> '' \
         AND cue_media_path IS NULL AND (source IS NULL OR source = 'local') \
         ORDER BY id LIMIT {}",
        ph(db, 1)
    );
    let limite = limite as i64;
    let params: [&dyn ToSqlValue; 1] = [&limite];
    match db.query_many(&sql, &params) {
        Ok(lignes) => lignes
            .iter()
            .filter_map(|l| Some((l.first()?.as_i64()?, l.get(1)?.as_string()?)))
            .collect(),
        Err(e) => {
            warn!(error = %e, "debit_candidates_illisibles");
            Vec::new()
        }
    }
}

fn ecrire(db: &dyn DbBackend, id: i64, debit: i64) -> bool {
    let sql = format!(
        "UPDATE tracks SET bitrate_kbps = {} WHERE id = {} AND bitrate_kbps IS NULL",
        ph(db, 1),
        ph(db, 2)
    );
    let params: [&dyn ToSqlValue; 2] = [&debit, &id];
    db.execute(&sql, &params).is_ok()
}

/// La passe de fin de scan : DSD calculé, puis au plus `limite` fichiers relus
/// dans `budget`. `lire` est la lecture d'un fichier (injectée pour les tests ;
/// la route passe [`completer_les_debits`]).
pub fn completer_avec(
    db: &dyn DbBackend,
    limite: usize,
    budget: Duration,
    continuer: impl Fn() -> bool,
    lire: impl Fn(&Path) -> Result<Option<u32>, ()>,
) -> RapportDebits {
    let mut rapport = RapportDebits {
        dsd_calcules: completer_le_dsd(db),
        ..Default::default()
    };
    let lot = candidates(db, limite);
    let debut = Instant::now();
    for (i, (id, chemin)) in lot.iter().enumerate() {
        if debut.elapsed() >= budget || !continuer() {
            rapport.interrompue = true;
            break;
        }
        match lire(Path::new(chemin)) {
            Ok(Some(kbps)) => {
                if ecrire(db, *id, i64::from(kbps)) {
                    rapport.lus += 1;
                }
            }
            Ok(None) => {
                if ecrire(db, *id, DEBIT_INCONNU) {
                    rapport.inconnus += 1;
                }
            }
            Err(()) => {
                // Marqué inconnu AVANT l'arrêt : le scan suivant reprend après
                // lui au lieu de buter au même endroit.
                if ecrire(db, *id, DEBIT_INCONNU) {
                    rapport.inconnus += 1;
                }
                warn!(chemin = %chemin, deja = i, "debit_lecture_sans_reponse — passe arrêtée");
                rapport.interrompue = true;
                break;
            }
        }
    }
    if lot.len() >= limite {
        rapport.interrompue = true;
    }
    rapport
}

/// Le réglage [`CLE_REGLAGE`] laisse-t-il tourner la passe ?
pub fn active(db: &Arc<dyn DbBackend>) -> bool {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten()
        .is_none_or(|v| v.trim() != "false")
}

/// La passe de fin de scan, avec la vraie lecture bornée. `occupe` dit si un
/// scan tient le droit de scanner : la passe s'arrête dès qu'il est vrai.
pub fn completer_les_debits(db: &dyn DbBackend, occupe: impl Fn() -> bool) -> RapportDebits {
    let rapport = completer_avec(
        db,
        LOT_PAR_SCAN,
        BUDGET_PAR_SCAN,
        || !occupe(),
        |p| lire_borne(p, DELAI_PAR_FICHIER),
    );
    if rapport != RapportDebits::default() {
        info!(
            dsd_calcules = rapport.dsd_calcules,
            lus = rapport.lus,
            inconnus = rapport.inconnus,
            interrompue = rapport.interrompue,
            "debit_des_pistes_complete"
        );
    }
    rapport
}

/// Lance la passe dans son propre fil, sans tenir le droit de scanner : elle
/// attend d'abord que le scan qui vient de finir le rende (`occupe` faux),
/// puis cède la place au premier scan suivant. Rend `false` sans rien lancer
/// si le réglage la coupe ou si une passe tourne déjà.
pub fn lancer_en_fond(db: Arc<dyn DbBackend>, occupe: fn() -> bool) -> bool {
    if !active(&db) {
        return false;
    }
    if EN_COURS.swap(true, Ordering::SeqCst) {
        return false;
    }
    let lance = std::thread::Builder::new()
        .name("debit-des-pistes".into())
        .spawn(move || {
            let debut = Instant::now();
            while occupe() && debut.elapsed() < ATTENTE_DU_DROIT {
                std::thread::sleep(Duration::from_millis(500));
            }
            if !occupe() {
                completer_les_debits(db.as_ref(), occupe);
            }
            EN_COURS.store(false, Ordering::SeqCst);
        });
    if lance.is_err() {
        EN_COURS.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

#[cfg(test)]
#[path = "debit_des_pistes_tests.rs"]
mod tests;
