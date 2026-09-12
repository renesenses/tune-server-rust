use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;
use crate::favorites_sort::{self, CleDeTri, TriFavoris};

/// A favorited streaming item (Tidal/Qobuz/…). Unlike local `favorites` (keyed
/// on an INTEGER `item_id`), streaming items use string `service_id`s, so they
/// live in their own `streaming_favorites` table. Display metadata is stored so
/// the favorites list needs no per-item hydration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingFavorite {
    pub id: i64,
    pub profile_id: i64,
    pub item_type: String,
    pub service: String,
    pub service_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub created_at: Option<String>,
}

/// Engine-agnostic SQL builders.
pub mod sql {
    use super::SqlDialect;

    pub fn add<D: SqlDialect>(d: &D) -> String {
        // created_at is filled with the engine's own "now" SQL expression
        // (SQLite strftime / PG to_char) rather than a bound param or a column
        // DEFAULT: the PG table created via ensure_schema (ENSURE_TABLES) has no
        // DEFAULT on created_at, so without this the value was NULL and
        // `ORDER BY created_at DESC` in list() was non-deterministic on PG.
        format!(
            "INSERT INTO streaming_favorites \
             (profile_id, item_type, service, service_id, title, artist, album, cover_url, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, service, service_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.now_iso8601(),
        )
    }

    pub fn remove<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_favorites \
             WHERE profile_id = {} AND item_type = {} AND service = {} AND service_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    pub fn count_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM streaming_favorites \
             WHERE profile_id = {} AND item_type = {} AND service = {} AND service_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    const SELECT_COLS: &str = "SELECT id, profile_id, item_type, service, service_id, title, artist, album, cover_url, created_at \
         FROM streaming_favorites";

    pub fn list_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS} WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_by_type<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS} WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Mêmes lignes, plus le **rang manuel** en colonne 10 (#2001, piste 2).
    ///
    /// Requête séparée, et non `position` ajouté à `SELECT_COLS` : la colonne
    /// n'est lue que par le tri manuel et n'entre JAMAIS dans
    /// `StreamingFavorite`, donc la forme du JSON rendu au client ne bouge pas.
    const SELECT_COLS_POUR_RANG: &str = "SELECT id, profile_id, item_type, service, service_id, title, artist, album, cover_url, created_at, position \
         FROM streaming_favorites";

    pub fn list_all_pour_rang<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS_POUR_RANG} WHERE profile_id = {} ORDER BY created_at DESC",
            d.placeholder(1)
        )
    }

    pub fn list_by_type_pour_rang<D: SqlDialect>(d: &D) -> String {
        format!(
            "{SELECT_COLS_POUR_RANG} WHERE profile_id = {} AND item_type = {} ORDER BY created_at DESC",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Efface le rang manuel de tout un onglet avant d'en reposer un.
    pub fn raz_ordre_manuel<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE streaming_favorites SET position = NULL WHERE profile_id = {} AND item_type = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn poser_rang_manuel<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE streaming_favorites SET position = {} WHERE profile_id = {} AND item_type = {} AND service = {} AND service_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5)
        )
    }
}

pub struct StreamingFavoritesRepo {
    db: Arc<dyn DbBackend>,
}

impl StreamingFavoritesRepo {
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

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        cover_url: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::add, sql::add);
        // profile_id is bound as TEXT: PG's column is TEXT (so `= $1` with a
        // bigint fails "operator does not exist: text = bigint"), and SQLite's
        // INTEGER affinity coerces the string back. Mirrors ProfileRepo.
        let pid = profile_id.to_string();
        let params: [&dyn ToSqlValue; 8] = [
            &pid,
            &item_type,
            &service,
            &service_id,
            &title,
            &artist,
            &album,
            &cover_url,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn remove(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::remove, sql::remove);
        let pid = profile_id.to_string();
        let params: [&dyn ToSqlValue; 4] = [&pid, &item_type, &service, &service_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn is_favorite(
        &self,
        profile_id: i64,
        item_type: &str,
        service: &str,
        service_id: &str,
    ) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_one, sql::count_one);
        let pid = profile_id.to_string();
        let params: [&dyn ToSqlValue; 4] = [&pid, &item_type, &service, &service_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n > 0)
    }

    pub fn list(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
    ) -> Result<Vec<StreamingFavorite>, String> {
        let pid = profile_id.to_string();
        let rows = if let Some(t) = item_type {
            let sql = self.dialect_sql(sql::list_by_type, sql::list_by_type);
            let params: [&dyn ToSqlValue; 2] = [&pid, &t];
            self.db.query_many(&sql, &params)?
        } else {
            let sql = self.dialect_sql(sql::list_all, sql::list_all);
            let params: [&dyn ToSqlValue; 1] = [&pid];
            self.db.query_many(&sql, &params)?
        };
        Ok(rows.iter().map(row_to_streaming_favorite).collect())
    }

    /// Les mêmes favoris, rangés selon `tri` (#2001).
    ///
    /// Cette table mémorise déjà `title`, `artist` et `album` — c'est tout
    /// l'intérêt de l'instantané posé à l'ajout — donc rien à joindre : on
    /// range en Rust ce que `list` vient de lire, avec les règles du client web
    /// (accents, champ absent en fin, tri naturel des nombres).
    pub fn list_sorted(
        &self,
        profile_id: i64,
        item_type: Option<&str>,
        tri: TriFavoris,
    ) -> Result<Vec<StreamingFavorite>, String> {
        // Le rang manuel n'est pas dans `StreamingFavorite` (il ne part pas au
        // client) : il faut donc relire les lignes avec leur colonne 10 et
        // ranger AVANT de construire les structures.
        if tri.cle == CleDeTri::Manuel {
            let pid = profile_id.to_string();
            let mut rows = if let Some(t) = item_type {
                let sql =
                    self.dialect_sql(sql::list_by_type_pour_rang, sql::list_by_type_pour_rang);
                let params: [&dyn ToSqlValue; 2] = [&pid, &t];
                self.db.query_many(&sql, &params)?
            } else {
                let sql = self.dialect_sql(sql::list_all_pour_rang, sql::list_all_pour_rang);
                let params: [&dyn ToSqlValue; 1] = [&pid];
                self.db.query_many(&sql, &params)?
            };
            favorites_sort::trier_par_rang(&mut rows, tri.sens, |r| {
                r.get(10).and_then(|v| v.as_i64())
            });
            return Ok(rows.iter().map(row_to_streaming_favorite).collect());
        }

        let mut items = self.list(profile_id, item_type)?;
        match tri.cle {
            CleDeTri::Ajout => favorites_sort::appliquer_ajout(&mut items, tri.sens),
            CleDeTri::Titre => favorites_sort::trier_par(&mut items, tri.sens, |f| f.title.clone()),
            CleDeTri::Artiste => {
                favorites_sort::trier_par(&mut items, tri.sens, |f| f.artist.clone())
            }
            CleDeTri::Album => favorites_sort::trier_par(&mut items, tri.sens, |f| f.album.clone()),
            // Traité au-dessus par le retour anticipé : ce bras est
            // inatteignable. Laissé SANS EFFET plutôt qu'en `unreachable!()` —
            // si un remaniement futur retirait le retour anticipé, une route de
            // lecture rendrait l'ordre d'ajout, pas un 500.
            CleDeTri::Manuel => {}
        }
        Ok(items)
    }

    /// Pose l'ordre manuel d'un onglet de favoris de service (#2001, piste 2).
    ///
    /// Jumelle de [`super::profile_repo::ProfileRepo::reorder_favorites`], dont
    /// elle reprend les trois garanties — onglet entier, retiré-puis-rajouté en
    /// fin de liste, dernier écrivain gagnant en entier — la seule différence
    /// étant la clé de l'élément : ici `(service, service_id)` et non un
    /// `item_id` entier.
    ///
    /// Rend le nombre de favoris effectivement rangés. Une référence inconnue
    /// du profil ne fait pas d'erreur et n'est pas comptée.
    pub fn reorder(
        &self,
        profile_id: i64,
        item_type: &str,
        items: &[(String, String)],
    ) -> Result<usize, String> {
        let mut vus = std::collections::HashSet::new();
        let uniques: Vec<&(String, String)> = items
            .iter()
            .filter(|cle| vus.insert((*cle).clone()))
            .collect();

        let pid = profile_id.to_string();
        let raz = self.dialect_sql(sql::raz_ordre_manuel, sql::raz_ordre_manuel);
        let pose = self.dialect_sql(sql::poser_rang_manuel, sql::poser_rang_manuel);

        let mut ranges = 0usize;
        self.db.write_tx(&mut |tx| {
            ranges = 0;
            let params: [&dyn ToSqlValue; 2] = [&pid, &item_type];
            tx.execute(&raz, &params)?;
            for (rang, (service, service_id)) in uniques.iter().enumerate() {
                // Rang lié en TEXTE : la colonne est TEXT sur le miroir
                // PostgreSQL, comme `profile_id` juste à côté.
                let rang = (rang as i64 + 1).to_string();
                let params: [&dyn ToSqlValue; 5] = [&rang, &pid, &item_type, service, service_id];
                ranges += tx.execute(&pose, &params)?;
            }
            Ok(())
        })?;
        Ok(ranges)
    }
}

fn row_to_streaming_favorite(cols: &Vec<SqlValue>) -> StreamingFavorite {
    StreamingFavorite {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        profile_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(1),
        item_type: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        service: cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
        service_id: cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
        title: cols.get(5).and_then(|v| v.as_string()),
        artist: cols.get(6).and_then(|v| v.as_string()),
        album: cols.get(7).and_then(|v| v.as_string()),
        cover_url: cols.get(8).and_then(|v| v.as_string()),
        created_at: cols.get(9).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_repo() -> StreamingFavoritesRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        StreamingFavoritesRepo::new(db)
    }

    #[test]
    fn add_list_remove() {
        let repo = fresh_repo();
        repo.add(
            1,
            "track",
            "tidal",
            "t1",
            Some("Song"),
            Some("Artist"),
            Some("Album"),
            None,
        )
        .unwrap();
        repo.add(
            1,
            "album",
            "qobuz",
            "q9",
            Some("Rec"),
            Some("Band"),
            None,
            None,
        )
        .unwrap();

        let all = repo.list(1, None).unwrap();
        assert_eq!(all.len(), 2);
        let tracks = repo.list(1, Some("track")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].service, "tidal");
        assert_eq!(tracks[0].service_id, "t1");
        assert_eq!(tracks[0].title.as_deref(), Some("Song"));

        assert!(repo.is_favorite(1, "track", "tidal", "t1").unwrap());
        assert!(!repo.is_favorite(1, "track", "tidal", "nope").unwrap());

        repo.remove(1, "track", "tidal", "t1").unwrap();
        assert!(!repo.is_favorite(1, "track", "tidal", "t1").unwrap());
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
    }

    /// Quatre favoris de service, ajoutés du plus ancien au plus récent, dont
    /// un sans titre ni album (#2001).
    fn repo_a_trier() -> StreamingFavoritesRepo {
        let repo = fresh_repo();
        for (id, titre, artiste, album, date) in [
            (
                "s1",
                Some("Volume 10"),
                Some("Éric Zimmer"),
                Some("Anthologie"),
                "2026-01-01T00:00:00Z",
            ),
            (
                "s2",
                Some("volume 2"),
                Some("aaron Zed"),
                Some("bis"),
                "2026-02-01T00:00:00Z",
            ),
            (
                "s3",
                Some("Zorro"),
                Some("Erik Satie"),
                Some("Coda"),
                "2026-03-01T00:00:00Z",
            ),
            ("s4", None, None, None, "2026-04-01T00:00:00Z"),
        ] {
            repo.add(1, "track", "qobuz", id, titre, artiste, album, None)
                .unwrap();
            let params: [&dyn ToSqlValue; 2] = [&date, &id];
            repo.db
                .execute(
                    "UPDATE streaming_favorites SET created_at = ? WHERE service_id = ?",
                    &params,
                )
                .unwrap();
        }
        repo
    }

    fn ids(items: Vec<StreamingFavorite>) -> Vec<String> {
        items.into_iter().map(|f| f.service_id).collect()
    }

    fn range(repo: &StreamingFavoritesRepo, sort: &str, order: &str) -> Vec<String> {
        let tri = TriFavoris::depuis(Some(sort), Some(order)).unwrap();
        ids(repo.list_sorted(1, None, tri).unwrap())
    }

    #[test]
    fn sans_tri_les_favoris_de_service_gardent_l_ordre_d_ajout() {
        let repo = repo_a_trier();
        assert_eq!(ids(repo.list(1, None).unwrap()), ["s4", "s3", "s2", "s1"]);
    }

    #[test]
    fn les_favoris_de_service_se_trient_sur_leur_instantane() {
        let repo = repo_a_trier();
        // Titre : tri naturel, casse ignoree, sans-titre en dernier.
        assert_eq!(range(&repo, "title", "asc"), ["s2", "s1", "s3", "s4"]);
        assert_eq!(range(&repo, "title", "desc"), ["s3", "s1", "s2", "s4"]);
        // Artiste : « Éric » entre « aaron » et « Erik ».
        assert_eq!(range(&repo, "artist", "asc"), ["s2", "s1", "s3", "s4"]);
        // Album : la colonne existe ici, contrairement aux favoris locaux.
        assert_eq!(range(&repo, "album", "asc"), ["s1", "s2", "s3", "s4"]);
        // Ajout croissant = l'ordre dans lequel ils ont ete enregistres.
        assert_eq!(range(&repo, "added", "asc"), ["s1", "s2", "s3", "s4"]);
    }

    // --- Ordre manuel (#2001, piste 2) ---------------------------------

    fn qobuz(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter()
            .map(|i| ("qobuz".to_string(), (*i).to_string()))
            .collect()
    }

    #[test]
    fn l_ordre_manuel_se_relit_sur_les_favoris_de_service() {
        let repo = repo_a_trier();
        // Un ordre qu'aucun champ ne produit : ni titre, ni artiste, ni album,
        // ni date d'ajout ne rendent s3, s1, s4, s2.
        assert_eq!(
            repo.reorder(1, "track", &qobuz(&["s3", "s1", "s4", "s2"]))
                .unwrap(),
            4
        );
        assert_eq!(range(&repo, "manual", "asc"), ["s3", "s1", "s4", "s2"]);
        assert_eq!(range(&repo, "manual", "desc"), ["s2", "s4", "s1", "s3"]);
    }

    /// Temoin anti-regression : poser un rang ne change RIEN aux chemins
    /// existants, et n'ajoute aucun champ a la reponse JSON.
    #[test]
    fn poser_un_ordre_manuel_ne_change_ni_l_ordre_par_defaut_ni_la_forme() {
        let repo = repo_a_trier();
        let avant = ids(repo.list(1, None).unwrap());
        let forme_avant = serde_json::to_value(&repo.list(1, None).unwrap()[0]).unwrap();
        repo.reorder(1, "track", &qobuz(&["s3", "s1", "s4", "s2"]))
            .unwrap();
        assert_eq!(ids(repo.list(1, None).unwrap()), avant);
        assert_eq!(range(&repo, "added", "asc"), ["s1", "s2", "s3", "s4"]);
        assert_eq!(range(&repo, "title", "asc"), ["s2", "s1", "s3", "s4"]);
        let forme_apres = serde_json::to_value(&repo.list(1, None).unwrap()[0]).unwrap();
        assert_eq!(
            forme_avant.as_object().unwrap().keys().collect::<Vec<_>>(),
            forme_apres.as_object().unwrap().keys().collect::<Vec<_>>(),
            "le rang ne doit pas fuir dans la reponse"
        );
    }

    #[test]
    fn un_favori_de_service_retire_puis_rajoute_revient_en_fin() {
        let repo = repo_a_trier();
        repo.reorder(1, "track", &qobuz(&["s3", "s1", "s4", "s2"]))
            .unwrap();
        repo.remove(1, "track", "qobuz", "s3").unwrap();
        repo.add(1, "track", "qobuz", "s3", Some("Zorro"), None, None, None)
            .unwrap();
        // s3 etait PREMIER : sa ligne est partie, son rang avec elle.
        let ordre = range(&repo, "manual", "asc");
        assert_eq!(&ordre[..3], &["s1", "s4", "s2"]);
        assert_eq!(ordre[3], "s3");
    }

    #[test]
    fn reordonner_un_onglet_de_service_ne_touche_pas_les_autres() {
        let repo = repo_a_trier();
        for a in ["a1", "a2"] {
            repo.add(1, "album", "qobuz", a, None, None, None, None)
                .unwrap();
        }
        repo.reorder(1, "track", &qobuz(&["s3", "s1", "s4", "s2"]))
            .unwrap();
        repo.reorder(1, "album", &qobuz(&["a2", "a1"])).unwrap();

        let tri = TriFavoris::depuis(Some("manual"), Some("asc")).unwrap();
        assert_eq!(
            ids(repo.list_sorted(1, Some("track"), tri).unwrap()),
            ["s3", "s1", "s4", "s2"]
        );
        assert_eq!(
            ids(repo.list_sorted(1, Some("album"), tri).unwrap()),
            ["a2", "a1"]
        );
    }

    #[test]
    fn une_reference_inconnue_est_ignoree_sans_erreur() {
        let repo = repo_a_trier();
        assert_eq!(
            repo.reorder(1, "track", &qobuz(&["s2", "inconnu", "s1"]))
                .unwrap(),
            2
        );
        assert_eq!(&range(&repo, "manual", "asc")[..2], &["s2", "s1"]);
        // Meme service_id chez un AUTRE service : ce n'est pas le meme favori.
        assert_eq!(
            repo.reorder(1, "track", &[("tidal".into(), "s1".into())])
                .unwrap(),
            0
        );
    }

    #[test]
    fn sql_du_rang_manuel_emet_les_placeholders_du_moteur() {
        assert!(sql::poser_rang_manuel(&SqliteDialect).ends_with(
            "WHERE profile_id = ? AND item_type = ? AND service = ? AND service_id = ?"
        ));
        assert!(sql::poser_rang_manuel(&PostgresDialect).ends_with(
            "WHERE profile_id = $2 AND item_type = $3 AND service = $4 AND service_id = $5"
        ));
        // Le rang doit etre LU, et seulement par la requete dediee : la
        // requete ordinaire ne le nomme pas, donc la forme du JSON ne bouge pas.
        assert!(sql::list_by_type_pour_rang(&SqliteDialect).contains("created_at, position"));
        assert!(!sql::list_by_type(&SqliteDialect).contains("position"));
    }

    #[test]
    fn add_is_idempotent_and_profile_scoped() {
        let repo = fresh_repo();
        repo.add(1, "track", "tidal", "t1", None, None, None, None)
            .unwrap();
        repo.add(1, "track", "tidal", "t1", None, None, None, None)
            .unwrap();
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
        // Different profile is isolated.
        repo.add(2, "track", "tidal", "t1", None, None, None, None)
            .unwrap();
        assert_eq!(repo.list(1, None).unwrap().len(), 1);
        assert_eq!(repo.list(2, None).unwrap().len(), 1);
    }
}
