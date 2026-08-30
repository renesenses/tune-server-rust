use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;
use crate::favorites_sort::{self, CleDeTri, TriFavoris};

/// Engine-agnostic SQL builders for profile_repo.
pub mod sql {
    use super::SqlDialect;

    const PROFILE_COLS: &str = "id, username, display_name, avatar_path, is_admin, created_at";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn list_all() -> &'static str {
        "SELECT id, username, display_name, avatar_path, is_admin, created_at FROM profiles ORDER BY id"
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO profiles (username, display_name, avatar_path) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE profiles SET username = COALESCE({}, username), display_name = COALESCE({}, display_name), avatar_path = COALESCE({}, avatar_path) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM profiles WHERE id = {}", d.placeholder(1))
    }

    /// INSERT OR IGNORE form. Uses the portable ON CONFLICT DO NOTHING
    /// (SQLite 3.24+, PG 9.5+) so the same SQL runs on both engines.
    pub fn add_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES ({}, {}, {}) ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn remove_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM favorites WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn count_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM favorites WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn list_favorites_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_favorites_by_type<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at FROM favorites WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Mêmes lignes que ci-dessus, plus l'**instantané d'identité**
    /// (`item_name`, `item_artist`) que le tri par titre ou par artiste doit
    /// lire (#2001).
    ///
    /// Ces deux colonnes existent déjà — migration SQLite v66 / PostgreSQL 017,
    /// posée pour réparer les « cœurs éteints ». Le tri s'en sert en lecture et
    /// n'y touche pas : aucune colonne n'est ajoutée, aucune migration n'est
    /// nécessaire. Elles ne sont pas rendues au client : `row_to_favorite`
    /// ignore les colonnes 5 et 6, et la réponse JSON garde sa forme.
    pub fn list_favorites_all_pour_tri<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at, item_name, item_artist, position FROM favorites WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_favorites_by_type_pour_tri<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, profile_id, item_type, item_id, created_at, item_name, item_artist, position FROM favorites WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Efface le rang manuel de TOUT un onglet avant d'en reposer un
    /// (#2001, piste 2). Voir [`super::ProfileRepo::reorder_favorites`] pour ce
    /// que cette remise à zéro garantit.
    pub fn raz_ordre_manuel<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE favorites SET position = NULL WHERE profile_id = {} AND item_type = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn poser_rang_manuel<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE favorites SET position = {} WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Option<i64>,
    #[serde(alias = "username")]
    pub name: String,
    #[serde(alias = "display_name")]
    pub display_name: Option<String>,
    #[serde(alias = "avatar_path", rename = "avatar_color")]
    pub avatar_path: Option<String>,
    pub is_admin: bool,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub id: Option<i64>,
    pub profile_id: i64,
    pub item_type: String,
    pub item_id: i64,
    pub created_at: Option<String>,
}

pub struct ProfileRepo {
    db: Arc<dyn DbBackend>,
}

impl ProfileRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    pub fn get(&self, id: i64) -> Result<Option<Profile>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_profile))
    }

    pub fn list(&self) -> Result<Vec<Profile>, String> {
        let rows = self.db.query_many(sql::list_all(), &[])?;
        Ok(rows.iter().map(row_to_profile).collect())
    }

    pub fn create(
        &self,
        username: &str,
        display_name: Option<&str>,
        avatar_path: Option<&str>,
    ) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 3] = [&username, &display_name, &avatar_path];
        Ok(self.db.execute_returning_id(&sql, &params)?)
    }

    pub fn update(
        &self,
        id: i64,
        display_name: Option<&str>,
        avatar_path: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::update, sql::update);
        // Pass display_name twice: once for username, once for display_name column
        let params: [&dyn ToSqlValue; 4] = [&display_name, &display_name, &avatar_path, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        if id == 1 {
            return Err("cannot delete default profile".into());
        }
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn add_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::add_favorite, sql::add_favorite);
        // Bind ids as strings: the Postgres mirror stores these columns as TEXT,
        // so binding i64 made `text = bigint` / int-into-text errors → 500.
        let (pid, iid) = (profile_id.to_string(), item_id.to_string());
        let params: [&dyn ToSqlValue; 3] = [&pid, &item_type, &iid];
        self.db.execute(&sql, &params)?;
        // Fige l'identité (titre/artiste/chemin) au moment de l'ajout, pour
        // re-rattacher le favori si un rescan renouvelle les rowids (racines
        // music déplacées, library clear — bug .18). Non-fatal : le favori
        // reste acquis même si l'instantané échoue.
        super::favorites_reconcile::FavoritesReconciler::with_backend(self.db.clone())
            .snapshot_item(item_type, item_id);
        Ok(())
    }

    pub fn remove_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::remove_favorite, sql::remove_favorite);
        let (pid, iid) = (profile_id.to_string(), item_id.to_string());
        let params: [&dyn ToSqlValue; 3] = [&pid, &item_type, &iid];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn is_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        item_id: i64,
    ) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_favorite, sql::count_favorite);
        let (pid, iid) = (profile_id.to_string(), item_id.to_string());
        let params: [&dyn ToSqlValue; 3] = [&pid, &item_type, &iid];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one("SELECT COUNT(*) FROM profiles", &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn list_favorites(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
    ) -> Result<Vec<Favorite>, String> {
        let pid = profile_id.to_string();
        let rows = if let Some(t) = item_type {
            let sql = self.dialect_sql(sql::list_favorites_by_type, sql::list_favorites_by_type);
            let params: [&dyn ToSqlValue; 2] = [&pid, &t];
            self.db.query_many(&sql, &params)?
        } else {
            let sql = self.dialect_sql(sql::list_favorites_all, sql::list_favorites_all);
            let params: [&dyn ToSqlValue; 1] = [&pid];
            self.db.query_many(&sql, &params)?
        };
        Ok(rows.iter().map(row_to_favorite).collect())
    }

    /// Les mêmes favoris, rangés selon `tri` (#2001).
    ///
    /// Le tri se fait en Rust, sur les lignes déjà lues : les règles du client
    /// web — accents rangés avec leur lettre, champ absent en fin de liste dans
    /// les deux sens, « Volume 2 » avant « Volume 10 » — n'ont pas d'écriture
    /// portable entre SQLite et PostgreSQL. Le `ORDER BY created_at DESC` reste
    /// dans la requête et sert de départage, le tri étant stable.
    pub fn list_favorites_sorted(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
        tri: TriFavoris,
    ) -> Result<Vec<Favorite>, String> {
        let pid = profile_id.to_string();
        let mut rows = if let Some(t) = item_type {
            let sql = self.dialect_sql(
                sql::list_favorites_by_type_pour_tri,
                sql::list_favorites_by_type_pour_tri,
            );
            let params: [&dyn ToSqlValue; 2] = [&pid, &t];
            self.db.query_many(&sql, &params)?
        } else {
            let sql = self.dialect_sql(
                sql::list_favorites_all_pour_tri,
                sql::list_favorites_all_pour_tri,
            );
            let params: [&dyn ToSqlValue; 1] = [&pid];
            self.db.query_many(&sql, &params)?
        };
        match tri.cle {
            CleDeTri::Ajout => favorites_sort::appliquer_ajout(&mut rows, tri.sens),
            CleDeTri::Titre => favorites_sort::trier_par(&mut rows, tri.sens, |r| {
                r.get(5).and_then(|v| v.as_string())
            }),
            CleDeTri::Artiste => favorites_sort::trier_par(&mut rows, tri.sens, |r| {
                r.get(6).and_then(|v| v.as_string())
            }),
            CleDeTri::Manuel => favorites_sort::trier_par_rang(&mut rows, tri.sens, |r| {
                r.get(7).and_then(|v| v.as_i64())
            }),
            // La table `favorites` ne mémorise pas d'album — l'instantané
            // d'identité n'en garde pas. La clé reste acceptée (elle a un sens
            // pour les favoris de service) mais ne range rien ici : l'ordre
            // d'ajout est conservé plutôt que renvoyer une erreur au client.
            CleDeTri::Album => {}
        }
        Ok(rows.iter().map(row_to_favorite).collect())
    }

    /// Pose l'ordre manuel d'UN onglet (#2001, piste 2) — le geste que Tades
    /// avait tenté à la souris. Rend le nombre de favoris effectivement rangés.
    ///
    /// `item_ids` est la liste **complète et ordonnée** de l'onglet
    /// `(profile_id, item_type)`. Ce que cela garantit, et qu'il faut savoir
    /// avant d'appeler :
    ///
    /// 1. **Un réordonnancement porte sur l'onglet ENTIER.** La transaction
    ///    remet d'abord tout l'onglet à « sans rang », puis numérote `1..n` les
    ///    ids reçus. Un favori de cet onglet absent de la liste retourne donc
    ///    **en fin** d'ordre manuel — jamais à un rang qui entrerait en
    ///    collision avec un autre. Les autres onglets du profil, et les autres
    ///    profils, ne sont pas touchés : la clé du rang est
    ///    `(profile_id, item_type)`.
    /// 2. **Retiré puis rajouté = en fin de liste.** Le rang vit dans la ligne
    ///    `favorites` ; `remove_favorite` la supprime, le rang avec elle, et
    ///    `add_favorite` en crée une neuve à `position` NULL. Un favori qui
    ///    revient ne reprend donc PAS son ancienne place — il faut le
    ///    redéplacer. C'est délibéré : rien ne permet de distinguer « je l'ai
    ///    retiré par erreur » de « je le remets pour le mettre ailleurs », et
    ///    ressusciter un rang produirait des trous invisibles.
    /// 3. **Deux clients qui réordonnent en même temps : le dernier gagne, en
    ///    entier.** Tout passe dans un seul `write_tx`, donc jamais un
    ///    entrelacement des deux ordres — l'état final est l'un des deux ordres
    ///    envoyés, complet. Il n'y a **pas** de détection de conflit : un client
    ///    qui réordonne à partir d'une liste périmée écrase l'autre sans le
    ///    savoir. C'est le compromis assumé pour une liste de favoris ; une
    ///    garde optimiste demanderait un jeton de version côté client.
    ///
    /// Un id inconnu du profil ne fait pas d'erreur : il ne met simplement à
    /// jour aucune ligne, et n'est pas compté. Un id répété n'est honoré qu'à
    /// sa première occurrence, pour que les rangs restent une suite sans trou.
    pub fn reorder_favorites(
        &self,
        profile_id: i64,
        item_type: &str,
        item_ids: &[i64],
    ) -> Result<usize, String> {
        let mut vus = std::collections::HashSet::new();
        let uniques: Vec<i64> = item_ids
            .iter()
            .copied()
            .filter(|id| vus.insert(*id))
            .collect();

        let pid = profile_id.to_string();
        let raz = self.dialect_sql(sql::raz_ordre_manuel, sql::raz_ordre_manuel);
        let pose = self.dialect_sql(sql::poser_rang_manuel, sql::poser_rang_manuel);

        let mut ranges = 0usize;
        self.db.write_tx(&mut |tx| {
            ranges = 0;
            let params: [&dyn ToSqlValue; 2] = [&pid, &item_type];
            tx.execute(&raz, &params)?;
            for (rang, item_id) in uniques.iter().enumerate() {
                // Rangs et identifiants liés en TEXTE : le miroir PostgreSQL
                // porte ces colonnes en TEXT, et y lier un i64 rend
                // « column is of type text but expression is of type bigint »
                // — le 500 que `add_favorite` documente déjà juste au-dessus.
                let (rang, iid) = ((rang as i64 + 1).to_string(), item_id.to_string());
                let params: [&dyn ToSqlValue; 4] = [&rang, &pid, &item_type, &iid];
                ranges += tx.execute(&pose, &params)?;
            }
            Ok(())
        })?;
        Ok(ranges)
    }
}

fn row_to_profile(cols: &Vec<SqlValue>) -> Profile {
    Profile {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        display_name: cols.get(2).and_then(|v| v.as_string()),
        avatar_path: cols.get(3).and_then(|v| v.as_string()),
        is_admin: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        created_at: cols.get(5).and_then(|v| v.as_string()),
    }
}

fn row_to_favorite(cols: &Vec<SqlValue>) -> Favorite {
    Favorite {
        id: cols.first().and_then(|v| v.as_i64()),
        profile_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(1),
        item_type: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        item_id: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        created_at: cols.get(4).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    /// Quatre favoris de piste, avec leur instantané d'identité et des dates
    /// d'ajout croissantes (#2001). Le quatrième n'a pas de titre : c'est le
    /// cas qui doit finir la liste dans les DEUX sens.
    fn repo_avec_favoris_dates() -> ProfileRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let repo = ProfileRepo::new(db);
        for (item_id, nom, artiste, date) in [
            (
                1_i64,
                Some("Volume 10"),
                Some("Éric Zimmer"),
                "2026-01-01T00:00:00Z",
            ),
            (
                2,
                Some("volume 2"),
                Some("aaron Zed"),
                "2026-02-01T00:00:00Z",
            ),
            (3, Some("Zorro"), Some("Erik Satie"), "2026-03-01T00:00:00Z"),
            (4, None, None, "2026-04-01T00:00:00Z"),
        ] {
            repo.add_favorite(1, "track", item_id).unwrap();
            let params: [&dyn ToSqlValue; 4] = [&nom, &artiste, &date, &item_id];
            repo.db
                .execute(
                    "UPDATE favorites SET item_name = ?, item_artist = ?, created_at = ? \
                     WHERE item_type = 'track' AND item_id = ?",
                    &params,
                )
                .unwrap();
        }
        repo
    }

    fn ids(favoris: Vec<Favorite>) -> Vec<i64> {
        favoris.iter().map(|f| f.item_id).collect()
    }

    fn range(repo: &ProfileRepo, sort: &str, order: &str) -> Vec<i64> {
        let tri = TriFavoris::depuis(Some(sort), Some(order)).unwrap();
        ids(repo.list_favorites_sorted(1, None, tri).unwrap())
    }

    /// Rétro-compatibilité : sans paramètre, rien ne bouge.
    #[test]
    fn sans_tri_l_ordre_reste_celui_d_avant() {
        let repo = repo_avec_favoris_dates();
        assert_eq!(
            ids(repo.list_favorites(1, None).unwrap()),
            vec![4, 3, 2, 1],
            "created_at DESC : le plus recemment ajoute d'abord"
        );
    }

    #[test]
    fn le_tri_par_titre_range_les_nombres_et_met_le_vide_a_la_fin() {
        let repo = repo_avec_favoris_dates();
        // « volume 2 » avant « Volume 10 » (tri naturel, casse ignoree), puis
        // « Zorro », et le favori sans titre ferme la marche.
        assert_eq!(range(&repo, "title", "asc"), vec![2, 1, 3, 4]);
        // Sens inverse : l'ordre des trois titres se retourne, le favori sans
        // titre reste DERNIER — c'est la regle 2.
        assert_eq!(range(&repo, "title", "desc"), vec![3, 1, 2, 4]);
    }

    #[test]
    fn le_tri_par_artiste_range_les_accents_avec_leur_lettre() {
        let repo = repo_avec_favoris_dates();
        // « Éric Zimmer » se range entre « aaron Zed » et « Erik Satie », pas
        // apres tout le monde comme le ferait un tri par code de caractere.
        assert_eq!(range(&repo, "artist", "asc"), vec![2, 1, 3, 4]);
    }

    #[test]
    fn le_tri_par_ajout_croissant_rend_l_ordre_d_enregistrement() {
        let repo = repo_avec_favoris_dates();
        // Le geste de Tades : reecouter ses favoris dans l'ordre ou il les a
        // enregistres.
        assert_eq!(range(&repo, "added", "asc"), vec![1, 2, 3, 4]);
        assert_eq!(range(&repo, "added", "desc"), vec![4, 3, 2, 1]);
    }

    #[test]
    fn le_tri_ne_desactive_pas_le_filtre_par_type() {
        let repo = repo_avec_favoris_dates();
        repo.add_favorite(1, "album", 77).unwrap();
        let tri = TriFavoris::depuis(Some("title"), Some("asc")).unwrap();
        assert_eq!(
            ids(repo.list_favorites_sorted(1, Some("album"), tri).unwrap()),
            vec![77]
        );
        assert_eq!(
            ids(repo.list_favorites_sorted(1, Some("track"), tri).unwrap()),
            vec![2, 1, 3, 4]
        );
    }

    /// La table `favorites` ne garde pas d'album : la cle est acceptee mais ne
    /// range rien, plutot que de faire une erreur au client.
    #[test]
    fn le_tri_par_album_est_sans_effet_sur_les_favoris_locaux() {
        let repo = repo_avec_favoris_dates();
        assert_eq!(range(&repo, "album", "asc"), vec![4, 3, 2, 1]);
    }

    // --- Ordre manuel (#2001, piste 2) ---------------------------------
    //
    // Le geste de Tades : deplacer un favori. Le tri par champ (piste 1) ne le
    // rend pas — il range d'apres une donnee, pas d'apres une intention.

    #[test]
    fn l_ordre_manuel_se_relit_dans_l_ordre_pose() {
        let repo = repo_avec_favoris_dates();
        // L'ordre d'ajout est 1,2,3,4 ; la liste par defaut rend 4,3,2,1.
        // On demande explicitement 3,1,4,2 — un ordre qu'AUCUN champ ne produit.
        assert_eq!(
            repo.reorder_favorites(1, "track", &[3, 1, 4, 2]).unwrap(),
            4
        );
        assert_eq!(range(&repo, "manual", "asc"), vec![3, 1, 4, 2]);
        assert_eq!(range(&repo, "manual", "desc"), vec![2, 4, 1, 3]);
    }

    /// Le temoin anti-regression : poser un ordre manuel ne doit RIEN changer
    /// a ce que rendent les chemins existants. La colonne `position` n'est lue
    /// que par `sort=manual`.
    #[test]
    fn poser_un_ordre_manuel_ne_change_pas_les_listes_sans_tri() {
        let repo = repo_avec_favoris_dates();
        let avant = ids(repo.list_favorites(1, None).unwrap());
        let avant_titre = range(&repo, "title", "asc");
        repo.reorder_favorites(1, "track", &[3, 1, 4, 2]).unwrap();
        assert_eq!(
            ids(repo.list_favorites(1, None).unwrap()),
            avant,
            "sans tri, l'ordre servi doit rester celui d'avant"
        );
        assert_eq!(range(&repo, "title", "asc"), avant_titre);
        assert_eq!(range(&repo, "added", "asc"), vec![1, 2, 3, 4]);
    }

    /// Garantie 2 : le rang vit dans la LIGNE. La retirer l'emporte.
    #[test]
    fn un_favori_retire_puis_rajoute_revient_en_fin_d_ordre_manuel() {
        let repo = repo_avec_favoris_dates();
        repo.reorder_favorites(1, "track", &[3, 1, 4, 2]).unwrap();
        assert_eq!(range(&repo, "manual", "asc"), vec![3, 1, 4, 2]);

        // Le favori 3 etait PREMIER. On le retire, on le remet.
        repo.remove_favorite(1, "track", 3).unwrap();
        repo.add_favorite(1, "track", 3).unwrap();

        // Il ne reprend pas sa place : il est sans rang, donc en fin de liste.
        assert_eq!(range(&repo, "manual", "asc"), vec![1, 4, 2, 3]);
        // Et il y reste en descendant — regle 2, un sans-rang ne remonte pas.
        assert_eq!(range(&repo, "manual", "desc"), vec![2, 4, 1, 3]);
    }

    /// Garantie 1 : le rang est par (profil, item_type). Ranger les albums ne
    /// doit pas defaire l'ordre des pistes — c'est exactement le motif
    /// « un chemin corrige, les autres nus » applique aux onglets.
    #[test]
    fn reordonner_un_onglet_ne_touche_pas_les_autres() {
        let repo = repo_avec_favoris_dates();
        for album in [10_i64, 20, 30] {
            repo.add_favorite(1, "album", album).unwrap();
        }
        repo.reorder_favorites(1, "track", &[3, 1, 4, 2]).unwrap();
        repo.reorder_favorites(1, "album", &[30, 10, 20]).unwrap();

        let tri = TriFavoris::depuis(Some("manual"), Some("asc")).unwrap();
        assert_eq!(
            ids(repo.list_favorites_sorted(1, Some("track"), tri).unwrap()),
            vec![3, 1, 4, 2],
            "l'ordre des pistes a survecu au rangement des albums"
        );
        assert_eq!(
            ids(repo.list_favorites_sorted(1, Some("album"), tri).unwrap()),
            vec![30, 10, 20]
        );
    }

    /// Garantie 1, seconde moitie : un reordonnancement porte sur l'onglet
    /// ENTIER. Un favori absent de la liste envoyee — ajoute par un autre
    /// client entre-temps, par exemple — repart en fin, jamais a un rang qui
    /// entrerait en collision.
    #[test]
    fn un_favori_absent_de_la_liste_envoyee_repart_en_fin() {
        let repo = repo_avec_favoris_dates();
        assert_eq!(repo.reorder_favorites(1, "track", &[4, 2]).unwrap(), 2);
        let ordre = range(&repo, "manual", "asc");
        assert_eq!(&ordre[..2], &[4, 2], "les deux ranges viennent en tete");
        // Les deux autres n'ont pas de rang : ils ferment la marche, departages
        // par `created_at DESC` comme partout ailleurs.
        assert_eq!(&ordre[2..], &[3, 1]);
    }

    /// Garantie 3 : deux reordonnancements successifs ne s'entrelacent pas.
    /// Le second ecrase entierement le premier — pas de rang orphelin qui
    /// survivrait du premier ordre.
    #[test]
    fn le_dernier_reordonnancement_gagne_en_entier() {
        let repo = repo_avec_favoris_dates();
        repo.reorder_favorites(1, "track", &[1, 2, 3, 4]).unwrap();
        repo.reorder_favorites(1, "track", &[4, 3]).unwrap();
        let ordre = range(&repo, "manual", "asc");
        assert_eq!(&ordre[..2], &[4, 3]);
        // 1 et 2 avaient les rangs 1 et 2 au premier tour. S'ils les avaient
        // gardes, ils seraient revenus EN TETE, devant 4 et 3.
        assert_eq!(
            &ordre[2..],
            &[2, 1],
            "les rangs du premier tour sont effaces"
        );
    }

    #[test]
    fn un_id_inconnu_ou_repete_ne_fait_pas_d_erreur() {
        let repo = repo_avec_favoris_dates();
        // 999 n'est pas en favori : il n'est pas compte, et ne decale rien.
        assert_eq!(repo.reorder_favorites(1, "track", &[2, 999, 1]).unwrap(), 2);
        assert_eq!(&range(&repo, "manual", "asc")[..2], &[2, 1]);

        // Un id repete n'est honore qu'a sa premiere occurrence : les rangs
        // restent une suite sans trou.
        assert_eq!(repo.reorder_favorites(1, "track", &[3, 3, 1]).unwrap(), 2);
        assert_eq!(&range(&repo, "manual", "asc")[..2], &[3, 1]);
    }

    #[test]
    fn l_ordre_manuel_d_un_profil_ne_franchit_pas_les_profils() {
        let repo = repo_avec_favoris_dates();
        let bob = repo.create("bob", None, None).unwrap();
        repo.add_favorite(bob, "track", 1).unwrap();
        repo.add_favorite(bob, "track", 2).unwrap();

        repo.reorder_favorites(1, "track", &[3, 1, 4, 2]).unwrap();
        repo.reorder_favorites(bob, "track", &[2, 1]).unwrap();

        assert_eq!(range(&repo, "manual", "asc"), vec![3, 1, 4, 2]);
        let tri = TriFavoris::depuis(Some("manual"), Some("asc")).unwrap();
        assert_eq!(
            ids(repo.list_favorites_sorted(bob, None, tri).unwrap()),
            vec![2, 1]
        );
    }

    #[test]
    fn sql_du_rang_manuel_emet_les_placeholders_du_moteur() {
        // Un placeholder mal forme fait echouer la requete sur UN seul moteur —
        // et l'ordre manuel serait muet sur PostgreSQL uniquement.
        assert_eq!(
            sql::poser_rang_manuel(&SqliteDialect),
            "UPDATE favorites SET position = ? WHERE profile_id = ? AND item_type = ? AND item_id = ?"
        );
        assert_eq!(
            sql::poser_rang_manuel(&PostgresDialect),
            "UPDATE favorites SET position = $1 WHERE profile_id = $2 AND item_type = $3 AND item_id = $4"
        );
        assert!(sql::raz_ordre_manuel(&PostgresDialect).ends_with("item_type = $2"));
        // La colonne du rang doit etre LUE par les requetes de tri, sinon le
        // tri manuel range sur une colonne toujours absente.
        assert!(sql::list_favorites_all_pour_tri(&SqliteDialect).contains("item_artist, position"));
        assert!(
            sql::list_favorites_by_type_pour_tri(&PostgresDialect)
                .contains("item_artist, position")
        );
    }

    #[test]
    fn profiles_and_favorites() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);

        let profiles = repo.list().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "default");

        let id = repo.create("bertrand", Some("Bertrand"), None).unwrap();
        assert!(id > 1);

        repo.add_favorite(1, "track", 42).unwrap();
        repo.add_favorite(1, "album", 10).unwrap();
        assert!(repo.is_favorite(1, "track", 42).unwrap());
        assert!(!repo.is_favorite(1, "track", 99).unwrap());

        let favs = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(favs.len(), 1);

        repo.remove_favorite(1, "track", 42).unwrap();
        assert!(!repo.is_favorite(1, "track", 42).unwrap());

        assert!(repo.delete(1).is_err());
        repo.delete(id).unwrap();
    }

    #[test]
    fn profile_update() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let id = repo.create("alice", Some("Alice"), None).unwrap();
        repo.update(id, Some("Alice Updated"), Some("/avatars/alice.png"))
            .unwrap();

        let p = repo.get(id).unwrap().unwrap();
        assert_eq!(p.display_name.as_deref(), Some("Alice Updated"));
        assert_eq!(p.avatar_path.as_deref(), Some("/avatars/alice.png"));
    }

    #[test]
    fn profile_get_default() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let default = repo.get(1).unwrap().unwrap();
        assert_eq!(default.name, "default");
        assert!(default.is_admin);
    }

    #[test]
    fn profile_favorites_multiple_types() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);

        repo.add_favorite(1, "track", 1).unwrap();
        repo.add_favorite(1, "track", 2).unwrap();
        repo.add_favorite(1, "album", 10).unwrap();
        repo.add_favorite(1, "artist", 5).unwrap();

        let all = repo.list_favorites(1, None).unwrap();
        assert_eq!(all.len(), 4);

        let tracks = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(tracks.len(), 2);

        let albums = repo.list_favorites(1, Some("album")).unwrap();
        assert_eq!(albums.len(), 1);
    }

    #[test]
    fn profile_duplicate_favorite_ignored() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        repo.add_favorite(1, "track", 42).unwrap();
        repo.add_favorite(1, "track", 42).unwrap();

        let favs = repo.list_favorites(1, Some("track")).unwrap();
        assert_eq!(favs.len(), 1);
    }

    #[test]
    fn profile_get_nonexistent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn profile_multiple_users_separate_favorites() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        let user2 = repo.create("bob", Some("Bob"), None).unwrap();

        repo.add_favorite(1, "track", 100).unwrap();
        repo.add_favorite(user2, "track", 200).unwrap();

        assert!(repo.is_favorite(1, "track", 100).unwrap());
        assert!(!repo.is_favorite(1, "track", 200).unwrap());
        assert!(!repo.is_favorite(user2, "track", 100).unwrap());
        assert!(repo.is_favorite(user2, "track", 200).unwrap());
    }

    #[test]
    fn profile_list_all() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = ProfileRepo::new(db);
        repo.create("alice", None, None).unwrap();
        repo.create("bob", None, None).unwrap();

        let all = repo.list().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn sql_builders_emit_dialect_specific_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;

        assert!(sql::get_by_id(&s).ends_with("WHERE id = ?"));
        assert!(sql::get_by_id(&p).ends_with("WHERE id = $1"));

        let pg_add = sql::add_favorite(&p);
        assert!(pg_add.contains("VALUES ($1, $2, $3)"));
        assert!(pg_add.ends_with("ON CONFLICT (profile_id, item_type, item_id) DO NOTHING"));

        let sqlite_add = sql::add_favorite(&s);
        assert!(sqlite_add.contains("VALUES (?, ?, ?)"));
        assert!(sqlite_add.ends_with("ON CONFLICT (profile_id, item_type, item_id) DO NOTHING"));
    }

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ProfileRepo::with_backend(backend);
        let id = repo.create("xx", None, None).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().name, "xx");
    }
}
