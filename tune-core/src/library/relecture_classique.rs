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
//! - **Relancée par le réglage « Balises du classique »** ([`reinitialiser`]) :
//!   la passe repart de zéro, selon le nouveau réglage ; elle efface alors
//!   une valeur que le fichier porte sous une clé que le réglage ne lit plus,
//!   et réécrit les crédits `balises` de la piste.
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
/// Présente pendant une relecture demandée par une MODIFICATION du réglage
/// « Balises du classique » : les clés brutes (sans préfixe) sous
/// lesquelles une valeur déjà en base a pu être lue (JSON). Voir
/// [`reinitialiser`].
pub const CLE_RECONFIGURATION: &str = "relecture_classique_reconfiguration";
/// Les clés de `track_metadata` que la passe écrit.
pub const CLES: [&str; 4] = ["work", "movement", "ensemble", "conductor"];

/// Ce qu'une lecture de fichier rend à la passe.
#[derive(Debug, Clone, Default)]
pub struct Lecture {
    /// Les métadonnées étendues, lues selon le réglage en vigueur.
    pub meta: std::collections::HashMap<String, String>,
    /// Toutes les balises brutes du fichier `(clé, valeur)`.
    pub brutes: Vec<(String, String)>,
}

impl Lecture {
    /// Témoins : une lecture qui ne porte que `paires` (et aucune brute).
    pub fn de(paires: &[(&str, &str)]) -> Self {
        Self {
            meta: paires
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            brutes: Vec::new(),
        }
    }
}

/// Relance la passe pour TOUTE la bibliothèque après une modification du
/// réglage « Balises du classique » : le curseur repart de zéro, et la
/// passe peut cette fois EFFACER une valeur que le fichier porte sous l'une
/// des `candidates` mais que le nouveau réglage ne lit plus (BAND ignoré,
/// balise retirée d'une liste). Une valeur posée d'ailleurs (assistant IA,
/// communauté) n'est jamais effacée : le fichier ne la porte sous aucune
/// clé candidate.
pub fn reinitialiser(backend: &Arc<dyn DbBackend>, candidates: &[String]) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut toutes: Vec<String> = settings
        .get(CLE_RECONFIGURATION)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    for c in candidates {
        if !toutes.contains(c) {
            toutes.push(c.clone());
        }
    }
    let json = serde_json::to_string(&toutes).map_err(|e| e.to_string())?;
    settings.set(CLE_RECONFIGURATION, &json)?;
    settings.delete(CLE_CURSEUR)?;
    settings.delete(CLE_FAITE)?;
    Ok(())
}

fn candidates(backend: &Arc<dyn DbBackend>) -> Option<Vec<String>> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_RECONFIGURATION)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

/// Chaque entrée (« ; ») de `valeur` est-elle portée par le fichier sous
/// l'une des clés `candidates` ?
fn portee_par_le_fichier(valeur: &str, brutes: &[(String, String)], candidates: &[String]) -> bool {
    let norm = |v: &str| v.trim().to_lowercase();
    let du_fichier: Vec<String> = brutes
        .iter()
        .filter(|(k, _)| candidates.contains(&crate::metadata::balises_brutes::nom_de_cle(k)))
        .flat_map(|(_, v)| v.split(';').map(norm).collect::<Vec<_>>())
        .collect();
    let entrees: Vec<String> = valeur
        .split(';')
        .map(norm)
        .filter(|e| !e.is_empty())
        .collect();
    !entrees.is_empty() && entrees.iter().all(|e| du_fichier.contains(e))
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

/// Les clés classiques d'un fichier, telles que le scan les lit selon le
/// réglage en vigueur, et ses balises brutes. `None` : fichier illisible
/// (partage absent…) — la piste est sautée, rien n'est effacé.
pub fn lire(chemin: &Path) -> Option<Lecture> {
    let brutes = crate::metadata::balises_brutes::lire_balises_du_fichier(chemin)
        .ok()?
        .paires();
    let meta = crate::metadata::read_extended_metadata(chemin);
    Some(Lecture { meta, brutes })
}

/// Lit et écrit le lot suivant de `taille` pistes locales, avance le
/// curseur, et clôt la passe quand il n'y a plus rien.
///
/// `lire_fichier` est [`lire`] en production ; les témoins le remplacent.
pub fn traiter_lot_suivant(
    backend: &Arc<dyn DbBackend>,
    taille: i64,
    lire_fichier: impl Fn(&Path) -> Option<Lecture>,
) -> Result<Lot, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    if est_faite(backend) {
        return Ok(Lot {
            fini: true,
            ..Lot::default()
        });
    }
    let reconfiguration = candidates(backend);
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
        let Some(lecture) = lire_fichier(Path::new(&chemin)) else {
            continue;
        };
        let mut ecrite = false;
        for cle in CLES {
            match lecture
                .meta
                .get(cle)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
            {
                Some(valeur) => {
                    meta.set(id, cle, valeur)?;
                    ecrite = true;
                }
                None => {
                    // Seulement après une modification du réglage, et
                    // seulement une valeur que le fichier porte sous une clé
                    // candidate : celle que l'ancien réglage avait lue.
                    let Some(candidates) = &reconfiguration else {
                        continue;
                    };
                    let deja = meta.get_all(id)?.get(cle).cloned();
                    if let Some(deja) = deja
                        && portee_par_le_fichier(&deja, &lecture.brutes, candidates)
                    {
                        meta.delete(id, cle)?;
                        ecrite = true;
                    }
                }
            }
        }
        if reconfiguration.is_some() {
            // Les crédits `balises` (chef, ensemble) suivent le réglage, comme
            // au scan d'un fichier modifié.
            let lignes = crate::metadata::credits_balises::lignes_des_balises(&lecture.meta);
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
