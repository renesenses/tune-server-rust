//! #9158 (Rob) — les balises personnelles des fichiers deviennent des tags
//! Tune : la pose dans `tags` / `item_tags`, avec une MARQUE D'ORIGINE.
//!
//! Le réglage et la lecture des balises vivent dans
//! [`crate::metadata::tags_perso`]. Ici : ce que la base en fait.
//!
//! # La marque d'origine : `item_tags_scan`
//!
//! Une ligne `(tag_id, item_type, item_id)` de `item_tags_scan` dit que la
//! pose de même clé dans `item_tags` vient du SCAN. Elle seule autorise le
//! scan à RETIRER une pose. Conséquences :
//!
//! - un tag posé à la main n'a jamais de marque : le scan ne le retire
//!   jamais, et ne le « reprend » pas non plus quand le fichier porte la
//!   même valeur (la pose existe déjà, aucune marque n'est ajoutée) ;
//! - poser à la main un tag que le scan avait posé EFFACE la marque
//!   (`TagRepo::tag_item` / `batch_tag`) : la pose devient manuelle, et ne
//!   part plus quand la balise quitte le fichier ;
//! - retirer à la main une pose du scan efface aussi la marque ; le prochain
//!   scan du fichier la reposera tant que la balise y est (le fichier fait
//!   foi pour ce qui vient du fichier).
//!
//! Une marque sans pose (pose retirée par un autre chemin) est ignorée et
//! nettoyée au passage.
//!
//! # Piste, et album quand toutes ses pistes le portent
//!
//! Chaque tag lu est posé sur la **piste** (`item_type = 'track'`). Il est
//! posé EN PLUS sur l'**album** quand TOUTES les pistes de l'album le portent
//! (quelle que soit l'origine de chaque pose de piste) et qu'au moins une le
//! tient du scan. L'album le perd, s'il le tenait du scan, dès qu'une piste ne
//! le porte plus. Ainsi le filtre par tag des pistes (Oxygen) voit chaque
//! piste, et la vue des albums d'un tag montre les albums homogènes.
//!
//! # Idempotence
//!
//! [`synchroniser`] rend l'objet conforme à la liste voulue : un second
//! passage avec la même liste n'écrit rien. Les inserts sont tous
//! `ON CONFLICT DO NOTHING` : aucun doublon possible.
//!
//! # La table
//!
//! Neuve et additive, SANS migration numérotée (même régime que
//! `streaming_hidden_items`) : rattrapage de `run_migrations` et
//! `CORE_SCHEMA` (SQLite), `ENSURE_TABLES` (PostgreSQL), `PG_FULL_SCHEMA`
//! et `MIGRATION_TABLES` (bascule SQLite → PostgreSQL).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::tag_repo::TagRepo;
use crate::metadata::balises_brutes::BalisesDuFichier;
use crate::metadata::tags_perso::{self, TagsPerso};

/// La table des marques d'origine, SQLite.
pub const DDL_SQLITE: &str = "CREATE TABLE IF NOT EXISTS item_tags_scan (\
    tag_id INTEGER NOT NULL,\
    item_type TEXT NOT NULL,\
    item_id INTEGER NOT NULL,\
    PRIMARY KEY (tag_id, item_type, item_id)\
);\
CREATE INDEX IF NOT EXISTS idx_item_tags_scan_item ON item_tags_scan(item_type, item_id);";

/// La table des marques d'origine, PostgreSQL.
pub const DDL_PG: &str = "CREATE TABLE IF NOT EXISTS item_tags_scan (\
    tag_id BIGINT NOT NULL,\
    item_type TEXT NOT NULL,\
    item_id BIGINT NOT NULL,\
    PRIMARY KEY (tag_id, item_type, item_id)\
);\
CREATE INDEX IF NOT EXISTS idx_item_tags_scan_item ON item_tags_scan(item_type, item_id);";

/// Crée la table si elle manque (témoins, bases montées à la main).
pub fn assurer_table(backend: &Arc<dyn DbBackend>) -> Result<(), String> {
    match backend.engine() {
        Engine::Sqlite => backend.execute_batch(DDL_SQLITE),
        Engine::Postgres => backend.execute_batch(DDL_PG),
    }
}

/// L'expression « maintenant » (ISO-8601 UTC) du moteur.
fn maintenant(backend: &Arc<dyn DbBackend>) -> &'static str {
    match backend.engine() {
        Engine::Sqlite => SqliteDialect.now_iso8601(),
        Engine::Postgres => PostgresDialect.now_iso8601(),
    }
}

/// SQL : l'effacement de la marque d'une pose (`TagRepo` l'appelle à chaque
/// pose ou retrait MANUELS). Les `?` sont traduits pour PostgreSQL par le
/// moteur.
pub const SQL_EFFACER_MARQUE: &str =
    "DELETE FROM item_tags_scan WHERE tag_id = ? AND item_type = ? AND item_id = ?";

fn ids(rows: Vec<Vec<super::backend::SqlValue>>) -> Vec<i64> {
    rows.into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// Ce qu'une synchronisation a changé.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Bilan {
    pub ajoutes: u64,
    pub retires: u64,
}

impl Bilan {
    fn ajouter(&mut self, autre: Bilan) {
        self.ajoutes += autre.ajoutes;
        self.retires += autre.retires;
    }
}

/// Rend l'objet `(item_type, item_id)` conforme à la liste `voulus` des tags
/// QUE LE SCAN doit y voir. Ne touche jamais une pose sans marque.
pub fn synchroniser(
    backend: &Arc<dyn DbBackend>,
    item_type: &str,
    item_id: i64,
    voulus: &[i64],
) -> Result<Bilan, String> {
    let p2: [&dyn ToSqlValue; 2] = [&item_type, &item_id];
    let presents: HashSet<i64> = ids(backend.query_many(
        "SELECT tag_id FROM item_tags WHERE item_type = ? AND item_id = ?",
        &p2,
    )?)
    .into_iter()
    .collect();
    let marques: Vec<i64> = ids(backend.query_many(
        "SELECT tag_id FROM item_tags_scan WHERE item_type = ? AND item_id = ?",
        &p2,
    )?);
    let effacer_marque = SQL_EFFACER_MARQUE;
    let retirer = "DELETE FROM item_tags WHERE tag_id = ? AND item_type = ? AND item_id = ?";
    let poser = format!(
        "INSERT INTO item_tags (tag_id, item_type, item_id, created_at) VALUES (?, ?, ?, {}) \
         ON CONFLICT (tag_id, item_type, item_id) DO NOTHING",
        maintenant(backend)
    );
    let marquer = "INSERT INTO item_tags_scan (tag_id, item_type, item_id) VALUES (?, ?, ?) \
         ON CONFLICT (tag_id, item_type, item_id) DO NOTHING";
    let voulus_set: HashSet<i64> = voulus.iter().copied().collect();
    let mut bilan = Bilan::default();
    for m in &marques {
        let p3: [&dyn ToSqlValue; 3] = [m, &item_type, &item_id];
        if !presents.contains(m) {
            // Marque orpheline : la pose est partie par un autre chemin.
            backend.execute(effacer_marque, &p3)?;
        } else if !voulus_set.contains(m) {
            backend.execute(retirer, &p3)?;
            backend.execute(effacer_marque, &p3)?;
            bilan.retires += 1;
        }
    }
    let mut faits: HashSet<i64> = HashSet::new();
    for v in voulus {
        if presents.contains(v) || !faits.insert(*v) {
            // Déjà posé : par le scan (marqué), ou à la main (jamais marqué).
            continue;
        }
        let p3: [&dyn ToSqlValue; 3] = [v, &item_type, &item_id];
        if backend.execute(&poser, &p3)? > 0 {
            backend.execute(marquer, &p3)?;
            bilan.ajoutes += 1;
        }
    }
    Ok(bilan)
}

/// Les identifiants des tags nommés, créés s'ils manquent. La casse est
/// ignorée : « calme » existant sert pour « Calme » lu dans un fichier.
pub struct Resolveur {
    repo: TagRepo,
    par_nom: HashMap<String, i64>,
}

impl Resolveur {
    pub fn new(backend: &Arc<dyn DbBackend>) -> Result<Self, String> {
        let repo = TagRepo::with_backend(backend.clone());
        let mut par_nom = HashMap::new();
        for t in repo.list()? {
            if let Some(id) = t.id {
                par_nom.entry(t.name.to_lowercase()).or_insert(id);
            }
        }
        Ok(Self { repo, par_nom })
    }

    pub fn id(&mut self, nom: &str) -> Result<i64, String> {
        let cle = nom.to_lowercase();
        if let Some(id) = self.par_nom.get(&cle) {
            return Ok(*id);
        }
        let id = match self.repo.create(nom, None) {
            Ok(id) if id > 0 => id,
            // Créé entre-temps par un autre chemin (nom UNIQUE) : relu.
            _ => self
                .repo
                .get_by_name(nom)?
                .and_then(|t| t.id)
                .ok_or_else(|| format!("tag « {nom} » ni créé ni trouvé"))?,
        };
        self.par_nom.insert(cle, id);
        Ok(id)
    }

    pub fn ids(&mut self, noms: &[String]) -> Result<Vec<i64>, String> {
        noms.iter().map(|n| self.id(n)).collect()
    }
}

fn liste_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Les albums des pistes `track_ids`.
pub fn albums_des_pistes(
    backend: &Arc<dyn DbBackend>,
    track_ids: &[i64],
) -> Result<Vec<i64>, String> {
    let mut albums: Vec<i64> = Vec::new();
    for tranche in track_ids.chunks(500) {
        let sql = format!(
            "SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL AND id IN ({})",
            liste_ids(tranche)
        );
        for a in ids(backend.query_many(&sql, &[])?) {
            if a > 0 && !albums.contains(&a) {
                albums.push(a);
            }
        }
    }
    Ok(albums)
}

/// Les tags que le scan doit voir sur l'album : ceux que portent TOUTES ses
/// pistes, dont au moins une pose vient du scan.
pub fn tags_voulus_pour_l_album(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
) -> Result<Vec<i64>, String> {
    let p1: [&dyn ToSqlValue; 1] = [&album_id];
    let pistes = ids(backend.query_many("SELECT id FROM tracks WHERE album_id = ?", &p1)?);
    if pistes.is_empty() {
        return Ok(Vec::new());
    }
    let poses = backend.query_many(
        "SELECT it.tag_id, it.item_id FROM item_tags it JOIN tracks t ON t.id = it.item_id \
         WHERE it.item_type = 'track' AND t.album_id = ?",
        &p1,
    )?;
    let mut porteurs: HashMap<i64, HashSet<i64>> = HashMap::new();
    for r in poses {
        if let (Some(tag), Some(piste)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_i64()),
        ) {
            porteurs.entry(tag).or_default().insert(piste);
        }
    }
    let mut marques = ids(backend.query_many(
        "SELECT DISTINCT s.tag_id FROM item_tags_scan s JOIN tracks t ON t.id = s.item_id \
         WHERE s.item_type = 'track' AND t.album_id = ?",
        &p1,
    )?);
    marques.sort_unstable();
    Ok(marques
        .into_iter()
        .filter(|tag| {
            porteurs
                .get(tag)
                .is_some_and(|p| pistes.iter().all(|piste| p.contains(piste)))
        })
        .collect())
}

/// Recalcule les tags du scan des albums `album_ids`.
pub fn synchroniser_albums(
    backend: &Arc<dyn DbBackend>,
    album_ids: &[i64],
) -> Result<Bilan, String> {
    let mut bilan = Bilan::default();
    for &a in album_ids {
        let voulus = tags_voulus_pour_l_album(backend, a)?;
        bilan.ajouter(synchroniser(backend, "album", a, &voulus)?);
    }
    Ok(bilan)
}

/// Les tags des pistes d'un lot du scan : chaque entrée qui porte la clé
/// réservée [`tags_perso::CLE_LUE`] (posée par la lecture étendue quand une
/// correspondance est réglée) est rendue conforme, puis ses albums. Une
/// entrée sans la clé (aucun réglage, ou fichier illisible) n'est pas
/// touchée. Les échecs sont journalisés, jamais remontés : le scan continue.
pub fn ecrire_lot(backend: &Arc<dyn DbBackend>, lot: &[(i64, HashMap<String, String>)]) -> Bilan {
    match ecrire_lot_ou_erreur(backend, lot) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "tags_perso_ecriture_echec");
            Bilan::default()
        }
    }
}

fn ecrire_lot_ou_erreur(
    backend: &Arc<dyn DbBackend>,
    lot: &[(i64, HashMap<String, String>)],
) -> Result<Bilan, String> {
    let a_traiter: Vec<(i64, Vec<String>)> = lot
        .iter()
        .filter_map(|(id, meta)| {
            meta.get(tags_perso::CLE_LUE)
                .map(|v| (*id, tags_perso::decoder(v)))
        })
        .collect();
    if a_traiter.is_empty() {
        return Ok(Bilan::default());
    }
    let mut resolveur = Resolveur::new(backend)?;
    let mut bilan = Bilan::default();
    for (id, noms) in &a_traiter {
        let voulus = resolveur.ids(noms)?;
        bilan.ajouter(synchroniser(backend, "track", *id, &voulus)?);
    }
    let pistes: Vec<i64> = a_traiter.iter().map(|(id, _)| *id).collect();
    let albums = albums_des_pistes(backend, &pistes)?;
    bilan.ajouter(synchroniser_albums(backend, &albums)?);
    Ok(bilan)
}

/// Retire TOUTES les poses du scan (réglage vidé), sans lire un fichier.
/// Les poses manuelles restent.
pub fn retirer_tout(backend: &Arc<dyn DbBackend>) -> Result<u64, String> {
    let n = backend.execute(
        "DELETE FROM item_tags WHERE EXISTS (SELECT 1 FROM item_tags_scan s \
         WHERE s.tag_id = item_tags.tag_id AND s.item_type = item_tags.item_type \
         AND s.item_id = item_tags.item_id)",
        &[],
    )?;
    backend.execute("DELETE FROM item_tags_scan", &[])?;
    Ok(n as u64)
}

// ---------------------------------------------------------------------------
// « Appliquer à la bibliothèque existante » : la passe de fond.
// ---------------------------------------------------------------------------

/// L'identifiant de piste comparable en NOMBRE (voir `relecture_classique`).
fn id_num(backend: &Arc<dyn DbBackend>) -> &'static str {
    match backend.engine() {
        Engine::Sqlite => "id",
        Engine::Postgres => "CAST(id AS BIGINT)",
    }
}

/// Les pistes locales après `apres` (pour l'avancement).
pub fn restantes(backend: &Arc<dyn DbBackend>, apres: i64) -> u64 {
    let sql = format!(
        "SELECT COUNT(*) FROM tracks WHERE {} > ? AND file_path IS NOT NULL AND file_path != ''",
        id_num(backend)
    );
    backend
        .query_many(&sql, &[&apres as &dyn ToSqlValue])
        .ok()
        .and_then(|r| r.first()?.first()?.as_i64())
        .unwrap_or(0)
        .max(0) as u64
}

/// Ce qu'un lot de la passe a fait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LotApplique {
    /// Le dernier identifiant de piste vu (le curseur suivant).
    pub dernier: i64,
    pub lues: u64,
    /// Fichiers illisibles : sautés, rien retiré.
    pub illisibles: u64,
    pub bilan: Bilan,
    /// Plus aucune piste après le curseur.
    pub fini: bool,
}

/// Le lot suivant de la passe : `taille` pistes locales après `apres`, lues
/// par `lire` (paires `(clé, valeur)` du fichier ; `None` = illisible),
/// rendues conformes au réglage `reglage`.
pub fn traiter_lot(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
    taille: i64,
    reglage: &TagsPerso,
    lire: impl Fn(&Path) -> Option<Vec<(String, String)>>,
) -> Result<LotApplique, String> {
    let n = id_num(backend);
    let rangs = backend.query_many(
        &format!(
            "SELECT {n}, file_path FROM tracks WHERE {n} > ? \
             AND file_path IS NOT NULL AND file_path != '' ORDER BY {n} LIMIT ?"
        ),
        &[&apres as &dyn ToSqlValue, &taille as &dyn ToSqlValue],
    )?;
    let mut lot = LotApplique {
        dernier: apres,
        fini: rangs.is_empty(),
        ..Default::default()
    };
    let mut entrees: Vec<(i64, HashMap<String, String>)> = Vec::new();
    for r in rangs {
        let (Some(id), Some(chemin)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        lot.dernier = lot.dernier.max(id);
        lot.lues += 1;
        let Some(paires) = lire(Path::new(&chemin)) else {
            lot.illisibles += 1;
            continue;
        };
        let tags = reglage.tags_des_paires(&paires);
        entrees.push((
            id,
            HashMap::from([(tags_perso::CLE_LUE.to_string(), tags_perso::encoder(&tags))]),
        ));
    }
    lot.bilan = ecrire_lot_ou_erreur(backend, &entrees)?;
    Ok(lot)
}

/// La lecture de production de la passe : toutes les balises du fichier.
pub fn lire_paires(chemin: &Path) -> Option<Vec<(String, String)>> {
    crate::metadata::balises_brutes::lire_balises_du_fichier(chemin)
        .ok()
        .map(|b| b.paires())
}

// ---------------------------------------------------------------------------
// L'aperçu des clés rencontrées.
// ---------------------------------------------------------------------------

/// Une clé vue dans l'échantillon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleVue {
    /// Le nom comparé (celui à saisir dans une correspondance).
    pub key: String,
    /// Les graphies d'origine rencontrées (`TXXX:Mood`, `MOOD`…).
    pub forms: Vec<String>,
    /// Le nombre de fichiers de l'échantillon qui la portent.
    pub files: u64,
    /// Quelques valeurs (au plus 3, 60 caractères chacune).
    pub examples: Vec<String>,
    /// Clé déjà lue par Tune ou standard.
    pub known: bool,
    /// Clé déjà dans une correspondance.
    pub configured: bool,
}

/// L'aperçu entier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Apercu {
    pub sampled_files: u64,
    pub unreadable_files: u64,
    /// L'échantillon s'est arrêté au délai.
    pub timed_out: bool,
    pub keys: Vec<CleVue>,
}

/// Au plus tant de clés rendues par l'aperçu.
pub const MAX_CLES_APERCU: usize = 300;

/// Un échantillon de `taille` chemins de pistes locales, au hasard.
pub fn echantillon(backend: &Arc<dyn DbBackend>, taille: i64) -> Result<Vec<String>, String> {
    let rangs = backend.query_many(
        "SELECT file_path FROM tracks WHERE file_path IS NOT NULL AND file_path != '' \
         ORDER BY RANDOM() LIMIT ?",
        &[&taille as &dyn ToSqlValue],
    )?;
    Ok(rangs
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect())
}

/// Compte les clés utilisables de `chemins` (voir
/// [`tags_perso::nom_utilisable`]), lues par `lire`, jusqu'à `tant_que()`
/// faux. Les clés libres d'abord, puis par nombre de fichiers.
pub fn apercu(
    chemins: &[String],
    reglage: &TagsPerso,
    lire: impl Fn(&Path) -> Option<BalisesDuFichier>,
    tant_que: impl Fn() -> bool,
) -> Apercu {
    let configurees: HashSet<String> = reglage.cles().into_iter().collect();
    let mut par_nom: HashMap<String, CleVue> = HashMap::new();
    let mut a = Apercu::default();
    for chemin in chemins {
        if !tant_que() {
            a.timed_out = true;
            break;
        }
        let Some(b) = lire(Path::new(chemin)) else {
            a.unreadable_files += 1;
            continue;
        };
        a.sampled_files += 1;
        let mut vues_ici: HashSet<String> = HashSet::new();
        for balise in &b.balises {
            let Some(nom) = tags_perso::nom_utilisable(balise.bloc, &balise.cle) else {
                continue;
            };
            let e = par_nom.entry(nom.clone()).or_insert_with(|| CleVue {
                key: nom.clone(),
                forms: Vec::new(),
                files: 0,
                examples: Vec::new(),
                known: tags_perso::est_cle_connue(&nom),
                configured: configurees.contains(&nom),
            });
            if !e.forms.contains(&balise.cle) && e.forms.len() < 4 {
                e.forms.push(balise.cle.clone());
            }
            let ex: String = balise.valeur.chars().take(60).collect();
            if e.examples.len() < 3 && !e.examples.contains(&ex) {
                e.examples.push(ex);
            }
            if vues_ici.insert(nom) {
                e.files += 1;
            }
        }
    }
    let mut cles: Vec<CleVue> = par_nom.into_values().collect();
    cles.sort_by(|x, y| {
        x.known
            .cmp(&y.known)
            .then(y.files.cmp(&x.files))
            .then(x.key.cmp(&y.key))
    });
    cles.truncate(MAX_CLES_APERCU);
    a.keys = cles;
    a
}

#[cfg(test)]
#[path = "tags_perso_repo_tests_9158.rs"]
mod tests;
