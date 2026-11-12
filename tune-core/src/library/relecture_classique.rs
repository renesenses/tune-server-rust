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
//! - **Idempotente** : elle n'écrit que `work`, `movement` et `ensemble`, par
//!   upsert, et seulement quand le fichier les porte. Une œuvre posée par
//!   l'assistant IA sur un fichier qui n'en dit rien reste en place.
//!
//! Le pilote (pause, lecture, registre des tâches de fond) est côté serveur :
//! `tune-server/src/routes/library/relecture_classique.rs`.

use std::path::Path;
use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;

/// Le réglage qui dit la passe terminée.
pub const CLE_FAITE: &str = "relecture_classique_8325_faite";
/// Le dernier identifiant de piste traité.
pub const CLE_CURSEUR: &str = "relecture_classique_8325_curseur";
/// Les clés de `track_metadata` que la passe écrit.
pub const CLES: [&str; 3] = ["work", "movement", "ensemble"];

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

/// Les clés classiques d'un fichier, telles que le scan les lit.
pub fn lire(chemin: &Path) -> Vec<(&'static str, String)> {
    let meta = crate::metadata::read_extended_metadata(chemin);
    CLES.iter()
        .filter_map(|k| {
            meta.get(*k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(|v| (*k, v.to_string()))
        })
        .collect()
}

/// Lit et écrit le lot suivant de `taille` pistes locales, avance le
/// curseur, et clôt la passe quand il n'y a plus rien.
///
/// `lire_fichier` est [`lire`] en production ; les témoins le remplacent.
pub fn traiter_lot_suivant(
    backend: &Arc<dyn DbBackend>,
    taille: i64,
    lire_fichier: impl Fn(&Path) -> Vec<(&'static str, String)>,
) -> Result<Lot, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    if est_faite(backend) {
        return Ok(Lot {
            fini: true,
            ..Lot::default()
        });
    }
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
        let valeurs = lire_fichier(Path::new(&chemin));
        if valeurs.is_empty() {
            continue;
        }
        for (cle, valeur) in &valeurs {
            meta.set(id, cle, valeur)?;
        }
        lot.ecrites += 1;
    }
    settings.set(CLE_CURSEUR, &dernier.to_string())?;
    Ok(lot)
}

#[cfg(test)]
#[path = "relecture_classique_8325_tests.rs"]
mod tests;
