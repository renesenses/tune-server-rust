//! Dossiers de collections (#4853, Gros Bidon, fil 1907).
//!
//! Un dossier range des sous-dossiers et des collections des DEUX sortes —
//! simples (réglage JSON `collections`) et intelligentes (`smart_collections`).
//! Décision de Bertrand du 24/09/2026 : un ARBRE (une collection dans un seul
//! dossier), profondeur maximale [`MAX_DEPTH`].
//!
//! # `(kind, collection_id)`, jamais un entier nu
//!
//! Les deux sortes de collections ont des espaces d'identifiants qui SE
//! RECOUVRENT : l'id 1 est à la fois « favorites » et « 💎 Audiophile » sur le
//! .18. Une ligne de rangement porte donc toujours la PAIRE, et `kind` est
//! vérifié contre la liste fermée [`KINDS`] à chaque écriture.
//!
//! # Ce que ce dépôt vérifie, et ce qu'il ne vérifie pas
//!
//! Il n'y a aucune clef étrangère (le schéma de bascule PostgreSQL n'en porte
//! pas, et les collections simples ne sont pas une table). Ce dépôt est donc le
//! SEUL chemin d'écriture, et c'est ici que sont refusés le cycle, la
//! profondeur, le nom vide et le dossier inexistant. L'existence de la
//! COLLECTION rangée, elle, est vérifiée par la route : le réglage
//! `collections` vit côté serveur HTTP.
//!
//! # Ordre
//!
//! Dans un parent, les sous-dossiers ont leur ordre et les collections le
//! leur (`position`, 0..n). Toute écriture renumérote la fratrie touchée, de
//! sorte qu'aucun trou ni doublon de position ne survive.

use std::sync::Arc;

use serde::Serialize;

use super::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};
use super::sqlite::SqliteDb;

/// Profondeur maximale de l'arbre de DOSSIERS : un dossier de la racine est
/// au niveau 1, son sous-dossier au niveau 2, le sous-sous-dossier au niveau 3.
/// Un dossier de niveau 3 range des collections, pas de sous-dossier.
pub const MAX_DEPTH: usize = 3;

/// Les deux sortes de collections qu'un dossier peut ranger.
pub const KINDS: [&str; 2] = ["collection", "smart"];

pub fn kind_valide(kind: &str) -> bool {
    KINDS.contains(&kind)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CollectionFolder {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderItem {
    pub kind: String,
    pub collection_id: i64,
    /// `None` = rangée à la racine, à sa position.
    pub folder_id: Option<i64>,
    pub position: i64,
}

/// Refus typés : la route les traduit en 400 / 404 / 409.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderError {
    /// Requête mal formée (nom vide, `kind` inconnu) — 400.
    Invalid(String),
    /// Dossier inexistant — 404.
    NotFound(String),
    /// Refus de l'arbre tel qu'il est (cycle, profondeur) — 409.
    Conflict(String),
    /// Erreur de base — 500.
    Db(String),
}

impl std::fmt::Display for FolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FolderError::Invalid(m)
            | FolderError::NotFound(m)
            | FolderError::Conflict(m)
            | FolderError::Db(m) => f.write_str(m),
        }
    }
}

const SELECT_FOLDERS: &str =
    "SELECT id, name, parent_id, position FROM collection_folders ORDER BY position, id";
const SELECT_ITEMS: &str = "SELECT kind, collection_id, folder_id, position \
     FROM collection_folder_items ORDER BY position, kind, collection_id";

fn row_to_folder(r: &[SqlValue]) -> CollectionFolder {
    CollectionFolder {
        id: r.first().and_then(|v| v.as_i64()).unwrap_or(0),
        name: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        parent_id: r.get(2).and_then(|v| v.as_i64()),
        position: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

fn row_to_item(r: &[SqlValue]) -> FolderItem {
    FolderItem {
        kind: r.first().and_then(|v| v.as_string()).unwrap_or_default(),
        collection_id: r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        folder_id: r.get(2).and_then(|v| v.as_i64()),
        position: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

/// Niveau d'un dossier : 1 à la racine. Borné par le nombre de dossiers, pour
/// qu'un cycle déjà en base (écrit à la main) ne fasse pas boucler.
pub fn depth_of(folders: &[CollectionFolder], id: i64) -> usize {
    let mut depth = 0;
    let mut current = Some(id);
    while let Some(cid) = current {
        depth += 1;
        if depth > folders.len() {
            break;
        }
        current = folders
            .iter()
            .find(|f| f.id == cid)
            .and_then(|f| f.parent_id);
    }
    depth
}

/// Hauteur du sous-arbre enraciné en `id` : 1 pour un dossier sans
/// sous-dossier.
pub fn height_of(folders: &[CollectionFolder], id: i64) -> usize {
    fn rec(folders: &[CollectionFolder], id: i64, garde: usize) -> usize {
        if garde == 0 {
            return 1;
        }
        1 + folders
            .iter()
            .filter(|f| f.parent_id == Some(id))
            .map(|f| rec(folders, f.id, garde - 1))
            .max()
            .unwrap_or(0)
    }
    rec(folders, id, folders.len())
}

/// Vrai si `candidate` est `ancestor` lui-même ou l'un de ses descendants.
pub fn is_self_or_descendant(folders: &[CollectionFolder], ancestor: i64, candidate: i64) -> bool {
    let mut current = Some(candidate);
    let mut garde = folders.len() + 1;
    while let Some(cid) = current {
        if cid == ancestor {
            return true;
        }
        if garde == 0 {
            return false;
        }
        garde -= 1;
        current = folders
            .iter()
            .find(|f| f.id == cid)
            .and_then(|f| f.parent_id);
    }
    false
}

fn nom_valide(name: &str) -> Result<String, FolderError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(FolderError::Invalid(
            "le nom du dossier ne peut pas être vide".into(),
        ));
    }
    Ok(n.to_string())
}

fn introuvable(id: i64) -> FolderError {
    FolderError::NotFound(format!("dossier {id} introuvable"))
}

fn trop_profond() -> FolderError {
    FolderError::Conflict(format!(
        "profondeur maximale atteinte : {MAX_DEPTH} niveaux de dossiers"
    ))
}

/// Insère `id` dans la liste ordonnée `ordre` à `position` (bornée), ou à la
/// fin si `position` est absente.
fn inserer(ordre: &mut Vec<i64>, id: i64, position: Option<i64>) {
    ordre.retain(|&x| x != id);
    let at = match position {
        Some(p) if p >= 0 => (p as usize).min(ordre.len()),
        _ => ordre.len(),
    };
    ordre.insert(at, id);
}

fn lire_dossiers(tx: &dyn DbTxHandle) -> Result<Vec<CollectionFolder>, String> {
    Ok(tx
        .query_many(SELECT_FOLDERS, &[])?
        .iter()
        .map(|r| row_to_folder(r))
        .collect())
}

fn lire_rangements(tx: &dyn DbTxHandle) -> Result<Vec<FolderItem>, String> {
    Ok(tx
        .query_many(SELECT_ITEMS, &[])?
        .iter()
        .map(|r| row_to_item(r))
        .collect())
}

/// Renumérote les sous-dossiers de `parent` dans l'ordre `ordre`.
fn ecrire_fratrie_dossiers(
    tx: &dyn DbTxHandle,
    parent: Option<i64>,
    ordre: &[i64],
) -> Result<(), String> {
    for (pos, id) in ordre.iter().enumerate() {
        let pos = pos as i64;
        tx.execute(
            "UPDATE collection_folders SET parent_id = ?, position = ? WHERE id = ?",
            &[&parent as &dyn ToSqlValue, &pos, id],
        )?;
    }
    Ok(())
}

/// Renumérote les collections rangées dans `folder` dans l'ordre `ordre`.
fn ecrire_fratrie_rangements(
    tx: &dyn DbTxHandle,
    folder: Option<i64>,
    ordre: &[(String, i64)],
) -> Result<(), String> {
    for (pos, (kind, cid)) in ordre.iter().enumerate() {
        let pos = pos as i64;
        tx.execute(
            "UPDATE collection_folder_items SET folder_id = ?, position = ? \
             WHERE kind = ? AND collection_id = ?",
            &[&folder as &dyn ToSqlValue, &pos, kind, cid],
        )?;
    }
    Ok(())
}

fn fratrie_dossiers(folders: &[CollectionFolder], parent: Option<i64>, sauf: i64) -> Vec<i64> {
    folders
        .iter()
        .filter(|f| f.parent_id == parent && f.id != sauf)
        .map(|f| f.id)
        .collect()
}

pub struct CollectionFolderRepo {
    db: Arc<dyn DbBackend>,
}

impl CollectionFolderRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn list_folders(&self) -> Result<Vec<CollectionFolder>, String> {
        Ok(self
            .db
            .query_many(SELECT_FOLDERS, &[])?
            .iter()
            .map(|r| row_to_folder(r))
            .collect())
    }

    pub fn list_items(&self) -> Result<Vec<FolderItem>, String> {
        Ok(self
            .db
            .query_many(SELECT_ITEMS, &[])?
            .iter()
            .map(|r| row_to_item(r))
            .collect())
    }

    pub fn get_folder(&self, id: i64) -> Result<Option<CollectionFolder>, String> {
        Ok(self
            .db
            .query_one(
                "SELECT id, name, parent_id, position FROM collection_folders WHERE id = ?",
                &[&id],
            )?
            .as_ref()
            .map(|r| row_to_folder(r)))
    }

    /// Lance `f` dans une transaction ; un refus typé annule la transaction
    /// et remonte tel quel, une erreur de base remonte en [`FolderError::Db`].
    fn transaction<T>(
        &self,
        mut f: impl FnMut(&dyn DbTxHandle) -> Result<Result<T, FolderError>, String>,
    ) -> Result<T, FolderError> {
        let mut sortie: Option<Result<T, FolderError>> = None;
        let res = self.db.write_tx(&mut |tx| match f(tx)? {
            Ok(v) => {
                sortie = Some(Ok(v));
                Ok(())
            }
            Err(refus) => {
                sortie = Some(Err(refus));
                Err("refus: transaction annulée".into())
            }
        });
        match (sortie, res) {
            (Some(Err(refus)), _) => Err(refus),
            (_, Err(e)) => Err(FolderError::Db(e)),
            (Some(Ok(v)), Ok(())) => Ok(v),
            (None, Ok(())) => Err(FolderError::Db("transaction sans résultat".into())),
        }
    }

    /// Crée un dossier à la fin de la fratrie de `parent_id`.
    pub fn create_folder(
        &self,
        name: &str,
        parent_id: Option<i64>,
    ) -> Result<CollectionFolder, FolderError> {
        let name = nom_valide(name)?;
        self.transaction(|tx| {
            let folders = lire_dossiers(tx)?;
            if let Some(pid) = parent_id {
                if !folders.iter().any(|f| f.id == pid) {
                    return Ok(Err(introuvable(pid)));
                }
                if depth_of(&folders, pid) + 1 > MAX_DEPTH {
                    return Ok(Err(trop_profond()));
                }
            }
            let id = folders.iter().map(|f| f.id).max().unwrap_or(0) + 1;
            let position = folders.iter().filter(|f| f.parent_id == parent_id).count() as i64;
            tx.execute(
                "INSERT INTO collection_folders (id, name, parent_id, position) VALUES (?, ?, ?, ?)",
                &[&id as &dyn ToSqlValue, &name, &parent_id, &position],
            )?;
            Ok(Ok(CollectionFolder {
                id,
                name: name.clone(),
                parent_id,
                position,
            }))
        })
    }

    pub fn rename_folder(&self, id: i64, name: &str) -> Result<CollectionFolder, FolderError> {
        let name = nom_valide(name)?;
        let n = self
            .db
            .execute(
                "UPDATE collection_folders SET name = ? WHERE id = ?",
                &[&name as &dyn ToSqlValue, &id],
            )
            .map_err(FolderError::Db)?;
        if n == 0 {
            return Err(introuvable(id));
        }
        self.get_folder(id)
            .map_err(FolderError::Db)?
            .ok_or_else(|| introuvable(id))
    }

    /// Déplace un dossier sous `parent_id` (`None` = racine), à `position`
    /// dans sa nouvelle fratrie (à la fin si absente). Refuse le cycle et
    /// tout déplacement qui porterait un dossier du sous-arbre au-delà de
    /// [`MAX_DEPTH`].
    pub fn move_folder(
        &self,
        id: i64,
        parent_id: Option<i64>,
        position: Option<i64>,
    ) -> Result<CollectionFolder, FolderError> {
        self.transaction(|tx| {
            let folders = lire_dossiers(tx)?;
            let Some(courant) = folders.iter().find(|f| f.id == id).cloned() else {
                return Ok(Err(introuvable(id)));
            };
            let profondeur_parent = match parent_id {
                None => 0,
                Some(pid) => {
                    if !folders.iter().any(|f| f.id == pid) {
                        return Ok(Err(introuvable(pid)));
                    }
                    if is_self_or_descendant(&folders, id, pid) {
                        return Ok(Err(FolderError::Conflict(format!(
                            "un dossier ne peut pas être rangé dans lui-même ni dans l'un de ses \
                             sous-dossiers (dossier {id} sous {pid})"
                        ))));
                    }
                    depth_of(&folders, pid)
                }
            };
            if profondeur_parent + height_of(&folders, id) > MAX_DEPTH {
                return Ok(Err(trop_profond()));
            }
            // L'ancienne fratrie se resserre, la nouvelle reçoit le dossier.
            if courant.parent_id != parent_id {
                let ancienne = fratrie_dossiers(&folders, courant.parent_id, id);
                ecrire_fratrie_dossiers(tx, courant.parent_id, &ancienne)?;
            }
            let mut nouvelle = fratrie_dossiers(&folders, parent_id, id);
            inserer(&mut nouvelle, id, position);
            ecrire_fratrie_dossiers(tx, parent_id, &nouvelle)?;
            let pos = nouvelle.iter().position(|&x| x == id).unwrap_or(0) as i64;
            Ok(Ok(CollectionFolder {
                parent_id,
                position: pos,
                ..courant
            }))
        })
    }

    /// Supprime un dossier. AUCUNE collection n'est supprimée : les
    /// sous-dossiers et les collections rangées remontent au parent, à la
    /// suite de ce qui s'y trouvait déjà, dans leur ordre.
    ///
    /// Remonter d'un niveau ne peut pas dépasser [`MAX_DEPTH`].
    pub fn delete_folder(&self, id: i64) -> Result<(), FolderError> {
        self.transaction(|tx| {
            let folders = lire_dossiers(tx)?;
            let Some(courant) = folders.iter().find(|f| f.id == id).cloned() else {
                return Ok(Err(introuvable(id)));
            };
            let parent = courant.parent_id;

            // Sous-dossiers : la fratrie du parent, sans le dossier supprimé,
            // puis ses enfants à sa suite.
            let mut ordre = fratrie_dossiers(&folders, parent, id);
            ordre.extend(
                folders
                    .iter()
                    .filter(|f| f.parent_id == Some(id))
                    .map(|f| f.id),
            );
            ecrire_fratrie_dossiers(tx, parent, &ordre)?;

            // Collections : celles du parent, puis celles du dossier supprimé.
            let items = lire_rangements(tx)?;
            let mut ordre_items: Vec<(String, i64)> = items
                .iter()
                .filter(|i| i.folder_id == parent)
                .map(|i| (i.kind.clone(), i.collection_id))
                .collect();
            ordre_items.extend(
                items
                    .iter()
                    .filter(|i| i.folder_id == Some(id))
                    .map(|i| (i.kind.clone(), i.collection_id)),
            );
            ecrire_fratrie_rangements(tx, parent, &ordre_items)?;

            tx.execute("DELETE FROM collection_folders WHERE id = ?", &[&id])?;
            Ok(Ok(()))
        })
    }

    /// Range la collection `(kind, collection_id)` dans `folder_id` (`None` =
    /// racine), à `position` parmi les collections de ce dossier (à la fin si
    /// absente). Une collection déjà rangée ailleurs est DÉPLACÉE : elle
    /// n'est jamais dans deux dossiers.
    ///
    /// ⚠️ L'existence de la collection n'est PAS vérifiée ici — voir l'en-tête
    /// du module.
    pub fn place_item(
        &self,
        kind: &str,
        collection_id: i64,
        folder_id: Option<i64>,
        position: Option<i64>,
    ) -> Result<FolderItem, FolderError> {
        if !kind_valide(kind) {
            return Err(FolderError::Invalid(format!(
                "sorte de collection inconnue : « {kind} » — sortes admises : {}",
                KINDS.join(", ")
            )));
        }
        let kind = kind.to_string();
        self.transaction(|tx| {
            if let Some(fid) = folder_id {
                let existe = tx
                    .query_one("SELECT id FROM collection_folders WHERE id = ?", &[&fid])?
                    .is_some();
                if !existe {
                    return Ok(Err(introuvable(fid)));
                }
            }
            let items = lire_rangements(tx)?;
            let ancien = items
                .iter()
                .find(|i| i.kind == kind && i.collection_id == collection_id)
                .cloned();
            let cle = (kind.clone(), collection_id);
            let fratrie = |dossier: Option<i64>| -> Vec<(String, i64)> {
                items
                    .iter()
                    .filter(|i| i.folder_id == dossier)
                    .map(|i| (i.kind.clone(), i.collection_id))
                    .filter(|c| *c != cle)
                    .collect()
            };
            match &ancien {
                None => {
                    tx.execute(
                        "INSERT INTO collection_folder_items (kind, collection_id, folder_id, position) \
                         VALUES (?, ?, ?, 0)",
                        &[&kind as &dyn ToSqlValue, &collection_id, &folder_id],
                    )?;
                }
                Some(a) if a.folder_id != folder_id => {
                    let ancienne = fratrie(a.folder_id);
                    ecrire_fratrie_rangements(tx, a.folder_id, &ancienne)?;
                }
                Some(_) => {}
            }
            let mut nouvelle = fratrie(folder_id);
            let at = match position {
                Some(p) if p >= 0 => (p as usize).min(nouvelle.len()),
                _ => nouvelle.len(),
            };
            nouvelle.insert(at, cle.clone());
            ecrire_fratrie_rangements(tx, folder_id, &nouvelle)?;
            Ok(Ok(FolderItem {
                kind: kind.clone(),
                collection_id,
                folder_id,
                position: at as i64,
            }))
        })
    }

    /// Retire une collection de l'arbre : elle retourne à la racine, après
    /// les collections rangées. Rend `true` si une ligne a été retirée.
    ///
    /// Sans vérification de `kind` : un retrait doit pouvoir atteindre une
    /// ligne écrite avant un changement de la liste admise.
    pub fn remove_item(&self, kind: &str, collection_id: i64) -> Result<bool, String> {
        let n = self.db.execute(
            "DELETE FROM collection_folder_items WHERE kind = ? AND collection_id = ?",
            &[&kind as &dyn ToSqlValue, &collection_id],
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> CollectionFolderRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        CollectionFolderRepo::new(db)
    }

    #[test]
    fn creer_renommer_lister() {
        let r = repo();
        let rock = r.create_folder("Rock", None).unwrap();
        let jazz = r.create_folder("  Jazz ", None).unwrap();
        assert_eq!(jazz.name, "Jazz", "le nom est rogné");
        assert_eq!((rock.position, jazz.position), (0, 1));
        let r2 = r.rename_folder(rock.id, "Rock & Blues").unwrap();
        assert_eq!(r2.name, "Rock & Blues");
        let noms: Vec<String> = r
            .list_folders()
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(noms, ["Rock & Blues", "Jazz"]);
    }

    #[test]
    fn nom_vide_refuse() {
        let r = repo();
        assert!(matches!(
            r.create_folder("   ", None),
            Err(FolderError::Invalid(_))
        ));
        let f = r.create_folder("A", None).unwrap();
        assert!(matches!(
            r.rename_folder(f.id, ""),
            Err(FolderError::Invalid(_))
        ));
    }

    #[test]
    fn quatrieme_niveau_refuse() {
        let r = repo();
        let n1 = r.create_folder("1", None).unwrap();
        let n2 = r.create_folder("2", Some(n1.id)).unwrap();
        let n3 = r.create_folder("3", Some(n2.id)).unwrap();
        assert_eq!(
            r.create_folder("4", Some(n3.id)),
            Err(trop_profond()),
            "un 4e niveau de dossiers doit être refusé"
        );
        // Déplacer un sous-arbre de hauteur 2 sous un dossier de niveau 2
        // donnerait 4 niveaux : refusé aussi.
        let a = r.create_folder("a", None).unwrap();
        let _b = r.create_folder("b", Some(a.id)).unwrap();
        assert_eq!(r.move_folder(a.id, Some(n2.id), None), Err(trop_profond()));
        // Sous un dossier de niveau 1 : 3 niveaux, admis.
        assert!(r.move_folder(a.id, Some(n1.id), None).is_ok());
    }

    #[test]
    fn cycle_refuse() {
        let r = repo();
        let a = r.create_folder("a", None).unwrap();
        let b = r.create_folder("b", Some(a.id)).unwrap();
        assert!(matches!(
            r.move_folder(a.id, Some(b.id), None),
            Err(FolderError::Conflict(_))
        ));
        assert!(matches!(
            r.move_folder(a.id, Some(a.id), None),
            Err(FolderError::Conflict(_))
        ));
        assert_eq!(
            r.get_folder(a.id).unwrap().unwrap().parent_id,
            None,
            "rien n'a bougé"
        );
    }

    #[test]
    fn supprimer_fait_remonter_le_contenu() {
        let r = repo();
        let musique = r.create_folder("Musique", None).unwrap();
        let rock = r.create_folder("Rock", Some(musique.id)).unwrap();
        let _deja = r.create_folder("Déjà là", Some(musique.id)).unwrap();
        let punk = r.create_folder("Punk", Some(rock.id)).unwrap();
        r.place_item("collection", 1, Some(rock.id), None).unwrap();
        r.place_item("smart", 1, Some(rock.id), None).unwrap();
        r.place_item("collection", 2, Some(musique.id), None)
            .unwrap();

        r.delete_folder(rock.id).unwrap();

        let dossiers = r.list_folders().unwrap();
        assert!(dossiers.iter().all(|f| f.id != rock.id));
        let punk2 = dossiers.iter().find(|f| f.id == punk.id).unwrap();
        assert_eq!(punk2.parent_id, Some(musique.id), "le sous-dossier remonte");
        let items = r.list_items().unwrap();
        assert_eq!(items.len(), 3, "aucune collection n'est perdue");
        assert!(items.iter().all(|i| i.folder_id == Some(musique.id)));
        let ordre: Vec<(String, i64, i64)> = items
            .iter()
            .map(|i| (i.kind.clone(), i.collection_id, i.position))
            .collect();
        assert!(ordre.contains(&("collection".into(), 2, 0)), "{ordre:?}");
    }

    #[test]
    fn meme_id_deux_sortes_deux_dossiers() {
        let r = repo();
        let a = r.create_folder("A", None).unwrap();
        let b = r.create_folder("B", None).unwrap();
        r.place_item("collection", 1, Some(a.id), None).unwrap();
        r.place_item("smart", 1, Some(b.id), None).unwrap();
        let items = r.list_items().unwrap();
        let col = items.iter().find(|i| i.kind == "collection").unwrap();
        let smart = items.iter().find(|i| i.kind == "smart").unwrap();
        assert_eq!((col.folder_id, smart.folder_id), (Some(a.id), Some(b.id)));
    }

    #[test]
    fn une_collection_dans_un_seul_dossier_et_reordonner() {
        let r = repo();
        let a = r.create_folder("A", None).unwrap();
        let b = r.create_folder("B", None).unwrap();
        r.place_item("collection", 5, Some(a.id), None).unwrap();
        r.place_item("collection", 6, Some(a.id), None).unwrap();
        r.place_item("collection", 5, Some(b.id), None).unwrap();
        let items = r.list_items().unwrap();
        assert_eq!(items.iter().filter(|i| i.collection_id == 5).count(), 1);
        let six = items.iter().find(|i| i.collection_id == 6).unwrap();
        assert_eq!(six.position, 0, "l'ancienne fratrie se resserre");
        r.place_item("collection", 7, Some(b.id), Some(0)).unwrap();
        let mut dans_b: Vec<(i64, i64)> = r
            .list_items()
            .unwrap()
            .into_iter()
            .filter(|i| i.folder_id == Some(b.id))
            .map(|i| (i.position, i.collection_id))
            .collect();
        dans_b.sort();
        assert_eq!(dans_b, [(0, 7), (1, 5)]);
    }

    #[test]
    fn sorte_inconnue_et_dossier_inexistant() {
        let r = repo();
        assert!(matches!(
            r.place_item("smart_collection", 1, None, None),
            Err(FolderError::Invalid(_))
        ));
        assert!(matches!(
            r.place_item("smart", 1, Some(99), None),
            Err(FolderError::NotFound(_))
        ));
        assert!(matches!(r.delete_folder(99), Err(FolderError::NotFound(_))));
        assert!(matches!(
            r.move_folder(99, None, None),
            Err(FolderError::NotFound(_))
        ));
    }

    /// Le même dépôt sur un VRAI PostgreSQL, par les DEUX naissances d'une
    /// base : installation native (la 071 sur une base nue) et bascule depuis
    /// SQLite (`PG_FULL_SCHEMA` tout-TEXT, puis la 071 qui convertit). SQLite
    /// tolère ce que PostgreSQL refuse : `text = bigint` n'existe pas.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_4853_dossiers_sur_les_deux_naissances() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const MIGRATION: &str =
            include_str!("../../migrations/postgres/071_collection_folders.sql");
        const SCHEMA_VERSION: &str = "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY, name TEXT, applied_at TIMESTAMPTZ DEFAULT now())";

        async fn base(url: &str, schema: &'static str) -> sqlx::PgPool {
            let maintenance = sqlx::PgPool::connect(url).await.unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
            )))
            .execute(&maintenance)
            .await
            .unwrap();
            maintenance.close().await;
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .after_connect(move |c, _| {
                    Box::pin(async move {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
                            .execute(c)
                            .await
                            .map(|_| ())
                    })
                })
                .connect(url)
                .await
                .unwrap()
        }

        async fn types(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT table_name::text, column_name::text, data_type::text \
                 FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name IN ('collection_folders', 'collection_folder_items') \
                   AND column_name IN ('id', 'parent_id', 'position', 'collection_id', 'folder_id') \
                 ORDER BY 1, 2",
            )
            .fetch_all(pool)
            .await
            .unwrap()
        }

        fn scenario(r: &CollectionFolderRepo) {
            let n1 = r.create_folder("Rock", None).unwrap();
            let n2 = r.create_folder("Hard", Some(n1.id)).unwrap();
            let n3 = r.create_folder("Glam", Some(n2.id)).unwrap();
            assert_eq!(r.create_folder("4", Some(n3.id)), Err(trop_profond()));
            assert!(matches!(
                r.move_folder(n1.id, Some(n3.id), None),
                Err(FolderError::Conflict(_))
            ));
            r.place_item("collection", 1, Some(n3.id), None).unwrap();
            r.place_item("smart", 1, Some(n1.id), None).unwrap();
            r.delete_folder(n3.id).unwrap();
            let items = r.list_items().unwrap();
            let simple = items.iter().find(|i| i.kind == "collection").unwrap();
            assert_eq!(simple.folder_id, Some(n2.id), "le contenu remonte");
            let smart = items.iter().find(|i| i.kind == "smart").unwrap();
            assert_eq!(smart.folder_id, Some(n1.id));
            assert!(r.remove_item("smart", 1).unwrap());
        }

        // 1. Installation native.
        let pool = base(&url, "rayons_4853_natif").await;
        sqlx::raw_sql(SCHEMA_VERSION).execute(&pool).await.unwrap();
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        assert!(
            types(&pool).await.iter().all(|(_, _, t)| t == "bigint"),
            "{:?}",
            types(&pool).await
        );
        scenario(&CollectionFolderRepo::with_backend(Arc::new(
            crate::db::backend::PostgresBackend::new(pool.clone()),
        )));
        pool.close().await;

        // 2. Bascule SQLite -> PostgreSQL : tout-TEXT, une ligne copiée, puis 071.
        let pool = base(&url, "rayons_4853_bascule").await;
        sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(SCHEMA_VERSION).execute(&pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collection_folders (id, name, parent_id, position) VALUES ('7', 'Copié', NULL, '0')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            types(&pool).await.iter().all(|(_, _, t)| t == "text"),
            "prémisse : tout-TEXT"
        );
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        // Rejouée : strict no-op.
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        assert!(
            types(&pool).await.iter().all(|(_, _, t)| t == "bigint"),
            "{:?}",
            types(&pool).await
        );
        let r = CollectionFolderRepo::with_backend(Arc::new(
            crate::db::backend::PostgresBackend::new(pool.clone()),
        ));
        assert_eq!(
            r.get_folder(7).unwrap().map(|f| f.name),
            Some("Copié".into())
        );
        assert_eq!(r.create_folder("Suivant", None).unwrap().id, 8);
        pool.close().await;
    }
}
