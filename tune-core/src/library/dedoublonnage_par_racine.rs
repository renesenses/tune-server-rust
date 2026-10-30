//! #6917 (fil 2397, Tades) — ne dédoublonner que les copies d'un MÊME
//! support. Arbitrage de Bertrand du 27/10/2026 : une option, éteinte par
//! défaut.
//!
//! ## Le besoin
//!
//! Un disque dur et un NAS présentent le même fichier, octet pour octet. Le
//! NAS est une sauvegarde, ou une bibliothèque complémentaire. Aujourd'hui,
//! Tune n'en garde qu'UNE entrée :
//!
//! * le scan fait de la seconde copie un EXEMPLAIRE de la première
//!   (`track_copies`, #4907), sans ligne `tracks` de plus ;
//! * « Nettoyage du serveur » (`POST /system/cleanup`,
//!   [`crate::db::track_repo::TrackRepo::deduplicate`]) retire de la
//!   bibliothèque la ligne d'une copie identique entrée après la première.
//!
//! Tades veut le dédoublonnage sur un même support, pas d'un support à
//! l'autre.
//!
//! ## La règle
//!
//! « Un même support » est une même RACINE de musique déclarée
//! (`music_dirs`), la plus longue qui contient le fichier
//! ([`crate::library::exemplaires::racine_de`]). Option allumée, deux copies
//! identiques ne se replient l'une sur l'autre que si elles vivent sous la
//! même racine ; sous deux racines, elles restent deux entrées. Un fichier
//! hors de toute racine déclarée n'est la copie de personne : on ne sait pas
//! sur quel support il vit, il reste une entrée.
//!
//! Option éteinte (défaut, valeur absente ou illisible), rien ne change.
//!
//! Dans les deux cas, aucun FICHIER n'est jamais effacé : seules des lignes
//! de bibliothèque sont en jeu.

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::library::exemplaires::{dossiers_de_musique, racine_de};

/// Réglage : `true` ⇒ le dédoublonnage se limite aux copies d'une même racine.
pub const CLE: &str = "library_dedup_same_root_only";

/// Valeur quand la clé est absente : **éteint**, le comportement d'avant.
pub const DEFAUT: bool = false;

/// Lit une valeur stockée. Seules les formes « vraies » usuelles allument
/// l'option : une valeur absente ou illisible garde le comportement d'avant.
pub fn depuis_valeur(valeur: Option<&str>) -> bool {
    match valeur {
        None => DEFAUT,
        Some(v) => matches!(
            v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
    }
}

/// L'option est-elle allumée ? Une base illisible répond le défaut (éteint).
pub fn active(db: &dyn DbBackend) -> bool {
    let sql = match db.engine() {
        Engine::Sqlite => format!(
            "SELECT value FROM settings WHERE key = {}",
            SqliteDialect.placeholder(1)
        ),
        Engine::Postgres => format!(
            "SELECT value FROM settings WHERE key = {}",
            PostgresDialect.placeholder(1)
        ),
    };
    let params: [&dyn ToSqlValue; 1] = [&CLE];
    match db.query_one_strong(&sql, &params) {
        Ok(ligne) => depuis_valeur(
            ligne
                .and_then(|r| r.first().and_then(|v| v.as_string()))
                .as_deref(),
        ),
        Err(e) => {
            tracing::warn!(cle = CLE, error = %e, "dedoublonnage_par_racine_lecture_echouee");
            DEFAUT
        }
    }
}

/// Les racines qui bornent le dédoublonnage : `None` quand l'option est
/// éteinte (aucune borne, le comportement d'avant).
pub fn racines_si_active(db: &dyn DbBackend) -> Option<Vec<String>> {
    active(db).then(|| dossiers_de_musique(db))
}

/// Deux chemins vivent-ils sous la même racine déclarée ? Faux dès que l'un
/// des deux est hors de toute racine.
pub fn meme_racine(a: &str, b: &str, racines: &[String]) -> bool {
    match (racine_de(a, racines), racine_de(b, racines)) {
        (Some(ra), Some(rb)) => ra == rb,
        _ => false,
    }
}

/// Peut-on replier `chemin` sur `candidat` ? Toujours sans borne ; avec
/// borne, seulement sous la même racine.
pub fn repliable(chemin: &str, candidat: &str, racines: Option<&[String]>) -> bool {
    racines.is_none_or(|r| meme_racine(chemin, candidat, r))
}

/// Les candidats sur lesquels `chemin` peut se replier.
pub fn candidats_repliables(
    chemin: &str,
    candidats: Vec<String>,
    racines: Option<&[String]>,
) -> Vec<String> {
    if racines.is_none() {
        return candidats;
    }
    candidats
        .into_iter()
        .filter(|c| repliable(chemin, c, racines))
        .collect()
}

#[cfg(test)]
#[path = "dedoublonnage_par_racine_tests.rs"]
mod tests;
