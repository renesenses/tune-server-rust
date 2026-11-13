//! #8325 — la relecture UNIQUE des balises classiques des fichiers déjà
//! importés : œuvre (WORK, `TXXX:WORK`, `©wrk`, TIT1 quand GRP1 est là),
//! mouvement (MOVEMENTNAME, MVNM, `©mvn`) et ensemble (ENSEMBLE, ORCHESTRA,
//! BAND quand il n'est pas un artiste).
//!
//! Le scan ordinaire écarte les fichiers inchangés : sans cette passe, une
//! bibliothèque importée avant #8325 n'aurait ses œuvres qu'après un scan
//! complet forcé, que Bertrand ne veut pas demander à l'utilisateur.
//!
//! - **Une seule fois** : le réglage [`CLE_FAITE`] la clôt pour de bon.
//! - **Reprenable** : le curseur [`CLE_CURSEUR`] (dernier identifiant de piste
//!   traité) est écrit après chaque lot ; un redémarrage reprend au lot
//!   suivant, une pause aussi.
//! - **Idempotente** : elle n'écrit que `work`, `movement`, `ensemble` et
//!   `conductor`, par upsert, et seulement quand le fichier les porte. Une
//!   œuvre posée par l'assistant IA sur un fichier qui n'en dit rien reste en
//!   place.
//! - **Relancée par le réglage « Balises du classique »** ([`reinitialiser`],
//!   appelé au PUT, donc persistant : un redémarrage la reprend) : la passe
//!   repart de zéro selon le nouveau réglage. Elle n'efface une valeur que si
//!   elle est ÉGALE à ce qu'un ANCIEN réglage lisait dans le fichier et que le
//!   nouveau ne la lit plus ; une valeur posée par l'assistant IA, la
//!   communauté ou l'utilisateur, que l'ancien réglage ne produisait pas,
//!   reste. Elle réécrit aussi les crédits `balises` de la piste.
//!
//! Le pilote (pause, lecture, registre des tâches de fond) est côté serveur :
//! `tune-server/src/routes/library/relecture_classique.rs`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::metadata::balises_classique::BalisesClassique;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;

/// Le réglage qui dit la passe terminée.
pub const CLE_FAITE: &str = "relecture_classique_8325_faite";
/// Le dernier identifiant de piste traité.
pub const CLE_CURSEUR: &str = "relecture_classique_8325_curseur";
/// Présente pendant une relecture demandée par une MODIFICATION du réglage
/// « Balises du classique » : les réglages d'AVANT (JSON, liste), ceux qui
/// ont produit les valeurs en base. Voir [`reinitialiser`].
pub const CLE_RECONFIGURATION: &str = "relecture_classique_reconfiguration";
/// Les clés de `track_metadata` que la passe écrit.
pub const CLES: [&str; 4] = ["work", "movement", "ensemble", "conductor"];

/// Sérialise un lot et une remise à zéro : sans lui, un lot en cours
/// écrirait son curseur APRÈS la remise à zéro du PUT, et sauterait des
/// pistes.
static VERROU: Mutex<()> = Mutex::new(());

fn verrou() -> std::sync::MutexGuard<'static, ()> {
    VERROU.lock().unwrap_or_else(|e| e.into_inner())
}

/// Relance la passe pour TOUTE la bibliothèque après une modification du
/// réglage « Balises du classique ». Appelée au PUT : le curseur repart de
/// zéro, la passe n'est plus close, et `ancien` (le réglage qui a produit les
/// valeurs en base) est enregistré — tout est en base, un redémarrage
/// reprend la relance. Plusieurs modifications de suite cumulent leurs
/// anciens réglages.
pub fn reinitialiser(
    backend: &Arc<dyn DbBackend>,
    ancien: &BalisesClassique,
) -> Result<(), String> {
    let _v = verrou();
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut anciens = anciens_reglages(backend).unwrap_or_default();
    if !anciens.contains(ancien) {
        anciens.push(ancien.clone());
    }
    let json = serde_json::to_string(&anciens).map_err(|e| e.to_string())?;
    settings.set(CLE_RECONFIGURATION, &json)?;
    settings.delete(CLE_CURSEUR)?;
    settings.delete(CLE_FAITE)?;
    Ok(())
}

/// Les anciens réglages d'une relance en cours, `None` hors relance.
fn anciens_reglages(backend: &Arc<dyn DbBackend>) -> Option<Vec<BalisesClassique>> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_RECONFIGURATION)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

/// Ce qu'un lot a fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lot {
    /// Pistes dont le fichier a été lu.
    pub lues: u64,
    /// Pistes dont au moins une clé a été écrite.
    pub ecrites: u64,
    /// Plus rien à lire : la passe est close ([`CLE_FAITE`] posé).
    pub fini: bool,
}

/// La passe est-elle déjà faite ?
pub fn est_faite(backend: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_FAITE)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// L'identifiant de piste comparable en NOMBRE : sur un miroir PostgreSQL
/// copié depuis SQLite, `tracks.id` est du texte (« 10 » < « 9 »).
fn id_num(backend: &Arc<dyn DbBackend>) -> &'static str {
    match backend.engine() {
        crate::db::engine::Engine::Sqlite => "id",
        crate::db::engine::Engine::Postgres => "CAST(id AS BIGINT)",
    }
}

fn curseur(backend: &Arc<dyn DbBackend>) -> i64 {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_CURSEUR)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Le nombre de pistes locales qui restent à lire (pour l'avancement).
pub fn restantes(backend: &Arc<dyn DbBackend>) -> u64 {
    let apres = curseur(backend);
    let sql = format!(
        "SELECT COUNT(*) FROM tracks WHERE {} > ? \
         AND file_path IS NOT NULL AND file_path != ''",
        id_num(backend)
    );
    backend
        .query_many(&sql, &[&apres as &dyn ToSqlValue])
        .ok()
        .and_then(|r| r.first()?.first()?.as_i64())
        .unwrap_or(0)
        .max(0) as u64
}

/// Les métadonnées étendues d'un fichier selon le réglage `reglage`.
/// `None` : fichier introuvable ou illisible (partage absent…) — la piste
/// est sautée, rien n'est effacé. Une seule lecture des balises.
pub fn lire(chemin: &Path, reglage: &BalisesClassique) -> Option<HashMap<String, String>> {
    let ouvrable = match crate::audio::iso9660::ouvrir_si_virtuel(chemin) {
        Some(lecteur) => lecteur.is_ok(),
        None => crate::audio::iso9660::ouvrir_fichier(chemin).is_ok(),
    };
    ouvrable.then(|| crate::metadata::read_extended_metadata_selon(chemin, reglage))
}

/// Lit et écrit le lot suivant de `taille` pistes locales selon le réglage
/// `actuel`, avance le curseur, et clôt la passe quand il n'y a plus rien.
///
/// `lire_fichier` est [`lire`] en production ; les témoins le remplacent.
/// Pendant une relance, un fichier n'est relu selon un ancien réglage que
/// pour une clé que le nouveau ne donne plus et que la base porte.
pub fn traiter_lot_suivant(
    backend: &Arc<dyn DbBackend>,
    taille: i64,
    actuel: &BalisesClassique,
    lire_fichier: impl Fn(&Path, &BalisesClassique) -> Option<HashMap<String, String>>,
) -> Result<Lot, String> {
    let _v = verrou();
    let settings = SettingsRepo::with_backend(backend.clone());
    if est_faite(backend) {
        return Ok(Lot {
            fini: true,
            ..Lot::default()
        });
    }
    let anciens = anciens_reglages(backend);
    let apres = curseur(backend);
    let n = id_num(backend);
    let rangs = backend.query_many(
        &format!(
            "SELECT {n}, file_path FROM tracks WHERE {n} > ? \
             AND file_path IS NOT NULL AND file_path != '' ORDER BY {n} LIMIT ?"
        ),
        &[&apres as &dyn ToSqlValue, &taille as &dyn ToSqlValue],
    )?;
    if rangs.is_empty() {
        settings.set(CLE_FAITE, "1")?;
        settings.delete(CLE_RECONFIGURATION)?;
        return Ok(Lot {
            fini: true,
            ..Lot::default()
        });
    }
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    let mut lot = Lot::default();
    let mut dernier = apres;
    for rang in rangs {
        let mut it = rang.into_iter();
        let (Some(id), Some(chemin)) = (
            it.next().and_then(|v| v.as_i64()),
            it.next().and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        dernier = dernier.max(id);
        lot.lues += 1;
        let chemin = Path::new(&chemin);
        let Some(lu) = lire_fichier(chemin, actuel) else {
            continue;
        };
        let mut ecrite = false;
        // Les lectures selon les anciens réglages, faites au besoin seulement.
        let mut lus_avant: Vec<Option<Option<HashMap<String, String>>>> =
            vec![None; anciens.as_ref().map_or(0, Vec::len)];
        for cle in CLES {
            if let Some(valeur) = lu.get(cle).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                meta.set(id, cle, valeur)?;
                ecrite = true;
                continue;
            }
            let Some(anciens) = &anciens else {
                continue;
            };
            let Some(deja) = meta.get_all(id)?.get(cle).cloned() else {
                continue;
            };
            // Effacée seulement si un ancien réglage la LISAIT dans ce
            // fichier, telle quelle.
            let lue_avant = anciens.iter().enumerate().any(|(i, ancien)| {
                let lu_avant = lus_avant[i].get_or_insert_with(|| lire_fichier(chemin, ancien));
                lu_avant
                    .as_ref()
                    .and_then(|m| m.get(cle))
                    .is_some_and(|v| v.trim() == deja.trim())
            });
            if lue_avant {
                meta.delete(id, cle)?;
                ecrite = true;
            }
        }
        if anciens.is_some() {
            // Les crédits `balises` (chef, ensemble) suivent le réglage, comme
            // au scan d'un fichier modifié.
            let lignes = crate::metadata::credits_balises::lignes_des_balises(&lu);
            crate::metadata::credits_balises::ecrire_credits_des_balises(backend, id, &lignes)?;
        }
        if ecrite {
            lot.ecrites += 1;
        }
    }
    settings.set(CLE_CURSEUR, &dernier.to_string())?;
    Ok(lot)
}

#[cfg(test)]
#[path = "relecture_classique_8325_tests.rs"]
mod tests;
