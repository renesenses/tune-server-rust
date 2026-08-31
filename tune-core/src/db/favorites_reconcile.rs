//! Réconciliation des favoris locaux après renouvellement des ids.
//!
//! Les favoris par profil (`favorites`) référencent des rowids de
//! `albums`/`tracks`/`artists`. Ces ids ne survivent PAS à tous les scans :
//! une racine music déplacée/remplacée fait passer chaque fichier par le
//! prune post-scan (ancien chemin absent → piste supprimée, album orphelin
//! nettoyé) puis par une ré-insertion sous un nouvel id ; un « library
//! clear » + rescan réattribue tout ; la fusion de doublons d'albums
//! supprime l'id perdant. Résultat (bug .18, v0.9.50) : cœurs éteints
//! partout et filtre « Favoris » vide — les favoris pointent des ids morts.
//!
//! Ce module rend les favoris durables en deux temps :
//! 1. **Instantané d'identité** : à l'ajout d'un favori (et en rattrapage à
//!    chaque réconciliation), on fige `item_name`/`item_artist`/`item_path`
//!    (migration v66 SQLite, 017 PG) — l'identité stable de l'item, qui
//!    survit au renouvellement d'id.
//! 2. **Réconciliation** (démarrage + post-scan) : chaque favori dont l'item
//!    n'existe plus est re-rattaché à l'item vivant retrouvé par identité
//!    (album : titre+artiste ; piste : chemin puis titre+artiste ; artiste :
//!    nom). Sans instantané (bibliothèques déjà cassées comme .18), on tente
//!    l'historique d'écoute (`listen_history.album_id` n'a pas de FK et
//!    garde titre/artiste après suppression de l'album). Un favori vraiment
//!    introuvable n'est supprimé qu'après un scan COMPLET et sain
//!    (`delete_unresolved`) — jamais au démarrage ni sur un scan partiel.
//!
//! SQL volontairement agnostique SQLite/Postgres : placeholders `?`
//! (traduits en `$n` par le backend PG), ids liés en i64 (colonnes INTEGER
//! côté SQLite, BIGINT côté PG depuis la migration 012), aucune comparaison
//! sur `file_mtime` (TEXT vs DOUBLE selon le millésime).

use std::sync::Arc;

use tracing::{info, warn};

use super::backend::{DbBackend, ToSqlValue};

/// Types d'items locaux que `favorites` peut référencer par rowid. Tout autre
/// `item_type` (streaming…) est ignoré — et surtout jamais supprimé.
///
/// `playlist` a rejoint la liste avec #2442 (FabienM, fil 1557) : une playlist
/// locale porte un `INTEGER PRIMARY KEY`, elle entre donc dans `favorites`
/// sans aucune migration. Tant qu'elle restait hors de cette liste le favori
/// s'écrivait bien, mais AUCUN instantané d'identité n'était figé : le cœur
/// s'éteignait dès que l'id changeait (import M3U rejoué, playlist recréée,
/// bascule SQLite→PostgreSQL). C'est exactement le défaut .18 des albums, sur
/// un autre type.
const LOCAL_ITEM_TYPES: [&str; 4] = ["track", "album", "artist", "playlist"];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileStats {
    /// Favoris locaux examinés.
    pub scanned: u64,
    /// Instantanés d'identité remplis sur des favoris encore vivants.
    pub snapshots_backfilled: u64,
    /// Favoris orphelins re-rattachés à l'item vivant retrouvé par identité.
    pub relinked: u64,
    /// Orphelins supprimés car leur cible retrouvée était déjà en favori.
    pub deduplicated: u64,
    /// Orphelins définitivement introuvables supprimés (scan complet sain).
    pub deleted: u64,
    /// Orphelins introuvables conservés (démarrage / scan partiel).
    pub unresolved: u64,
}

impl ReconcileStats {
    pub fn changed(&self) -> u64 {
        self.snapshots_backfilled + self.relinked + self.deduplicated + self.deleted
    }
}

/// Identité stable d'un item favori : nom/titre, artiste, chemin fichier
/// (pistes uniquement). Chaînes vides = inconnu.
#[derive(Debug, Default, Clone)]
struct Identity {
    name: String,
    artist: String,
    path: String,
}

/// Identité vivante (titre, artiste) d'un album, ou `None` s'il n'existe plus.
///
/// Écrite une seule fois pour les trois tables de marqueurs sans clé
/// étrangère — favoris, albums masqués (`hidden_repo`), paires déclarées
/// distinctes (`album_distinct_repo`) : c'est le pendant en lecture de
/// [`find_album_by_identity`], et le recopier a déjà produit des divergences
/// (#2848).
pub(crate) fn album_live_identity(
    db: &dyn DbBackend,
    album_id: i64,
) -> Result<Option<(String, String)>, String> {
    let params: [&dyn ToSqlValue; 1] = [&album_id];
    Ok(db
        .query_one(
            "SELECT a.title, COALESCE(ar.name, '') FROM albums a \
             LEFT JOIN artists ar ON ar.id = a.artist_id WHERE a.id = ?",
            &params,
        )?
        .map(|cols| {
            (
                cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            )
        }))
}

/// Retrouve l'album VIVANT correspondant à une identité (titre, artiste).
///
/// Règles de re-rattachement identiques pour les favoris et pour les albums
/// masqués (`hidden_repo`) — c'est exactement le genre de logique qu'on
/// recopie une fois de trop :
/// * artiste connu : titre + artiste, l'album le plus peuplé gagne ; AUCUN
///   repli titre seul (un homonyme d'un autre artiste serait re-rattaché —
///   constaté sur .18 : « ok » de daoud disparu vs « OK » de Talvin Singh) ;
/// * artiste inconnu : titre seul, UNIQUEMENT si non ambigu.
pub(crate) fn find_album_by_identity(
    db: &dyn DbBackend,
    name: &str,
    artist: &str,
) -> Result<Option<i64>, String> {
    if name.is_empty() {
        return Ok(None);
    }
    if !artist.is_empty() {
        let params: [&dyn ToSqlValue; 2] = [&name, &artist];
        let rows = db.query_many(
            "SELECT a.id FROM albums a LEFT JOIN artists ar ON ar.id = a.artist_id \
             WHERE LOWER(a.title) = LOWER(?) AND LOWER(COALESCE(ar.name, '')) = LOWER(?) \
             ORDER BY COALESCE(a.track_count, 0) DESC, a.id ASC",
            &params,
        )?;
        return Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64()));
    }
    let params: [&dyn ToSqlValue; 1] = [&name];
    let rows = db.query_many(
        "SELECT id FROM albums WHERE LOWER(title) = LOWER(?)",
        &params,
    )?;
    if rows.len() == 1 {
        return Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64()));
    }
    Ok(None)
}

pub struct FavoritesReconciler {
    db: Arc<dyn DbBackend>,
}

impl FavoritesReconciler {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    /// Fige l'identité d'un item sur tous les favoris qui le référencent et
    /// n'ont pas encore d'instantané. Appelé à l'ajout d'un favori ;
    /// non-fatal (l'ajout reste acquis même si l'instantané échoue).
    pub fn snapshot_item(&self, item_type: &str, item_id: i64) {
        if !LOCAL_ITEM_TYPES.contains(&item_type) {
            return;
        }
        let ident = match self.lookup_live(item_type, item_id) {
            Ok(Some(ident)) => ident,
            Ok(None) => return,
            Err(e) => {
                warn!(item_type, item_id, error = %e, "favorite_snapshot_lookup_failed");
                return;
            }
        };
        let params: [&dyn ToSqlValue; 5] = [
            &ident.name,
            &ident.artist,
            &ident.path,
            &item_type,
            &item_id,
        ];
        if let Err(e) = self.db.execute(
            "UPDATE favorites SET item_name = ?, item_artist = ?, item_path = ? \
             WHERE item_type = ? AND item_id = ? \
             AND (item_name IS NULL OR item_name = '')",
            &params,
        ) {
            warn!(item_type, item_id, error = %e, "favorite_snapshot_failed");
        }
    }

    /// Passe complète : backfill des instantanés manquants + re-rattachement
    /// des favoris orphelins. `delete_unresolved` ne doit être vrai qu'après
    /// un scan COMPLET et sain (pas ciblé, pas annulé, aucune racine
    /// manquante/illisible) — c'est la seule situation où « introuvable »
    /// veut dire « n'existe vraiment plus ».
    pub fn run(&self, delete_unresolved: bool) -> Result<ReconcileStats, String> {
        let rows = self.db.query_many(
            "SELECT id, profile_id, item_type, item_id, item_name, item_artist, item_path \
             FROM favorites",
            &[],
        )?;

        let mut stats = ReconcileStats::default();
        for row in &rows {
            let Some(fav_id) = row.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let profile_id = row.get(1).and_then(|v| v.as_i64()).unwrap_or(1);
            let item_type = row.get(2).and_then(|v| v.as_string()).unwrap_or_default();
            if !LOCAL_ITEM_TYPES.contains(&item_type.as_str()) {
                continue;
            }
            let Some(item_id) = row.get(3).and_then(|v| v.as_i64()) else {
                continue;
            };
            stats.scanned += 1;

            let snapshot = Identity {
                name: row.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                artist: row.get(5).and_then(|v| v.as_string()).unwrap_or_default(),
                path: row.get(6).and_then(|v| v.as_string()).unwrap_or_default(),
            };

            // Item encore vivant → au plus un backfill d'instantané.
            if let Some(live) = self.lookup_live(&item_type, item_id)? {
                if snapshot.name.is_empty() {
                    let params: [&dyn ToSqlValue; 4] =
                        [&live.name, &live.artist, &live.path, &fav_id];
                    self.db.execute(
                        "UPDATE favorites SET item_name = ?, item_artist = ?, item_path = ? \
                         WHERE id = ?",
                        &params,
                    )?;
                    stats.snapshots_backfilled += 1;
                }
                continue;
            }

            // Orphelin : identité = instantané, sinon historique d'écoute
            // (bibliothèques cassées AVANT l'instantané, comme .18).
            let ident = if snapshot.name.is_empty() && snapshot.path.is_empty() {
                self.history_identity(&item_type, item_id)?
            } else {
                Some(snapshot)
            };

            let target = match ident.as_ref() {
                Some(ident) => self.find_live_id(&item_type, ident)?,
                None => None,
            };

            match target {
                Some(new_id) if new_id != item_id => {
                    if self.is_favorite(profile_id, &item_type, new_id)? {
                        // La cible vivante est déjà en favori : l'orphelin est
                        // un doublon, on le retire.
                        let params: [&dyn ToSqlValue; 1] = [&fav_id];
                        self.db
                            .execute("DELETE FROM favorites WHERE id = ?", &params)?;
                        stats.deduplicated += 1;
                    } else {
                        // Ré-instantané depuis l'item vivant : le chemin (et
                        // parfois la casse du titre) a pu changer.
                        let live = self
                            .lookup_live(&item_type, new_id)?
                            .unwrap_or_else(|| ident.clone().unwrap_or_default());
                        let params: [&dyn ToSqlValue; 5] =
                            [&new_id, &live.name, &live.artist, &live.path, &fav_id];
                        self.db.execute(
                            "UPDATE favorites SET item_id = ?, item_name = ?, \
                             item_artist = ?, item_path = ? WHERE id = ?",
                            &params,
                        )?;
                        info!(
                            profile_id,
                            item_type,
                            old_id = item_id,
                            new_id,
                            name = %live.name,
                            "favorite_relinked"
                        );
                        stats.relinked += 1;
                    }
                }
                _ => {
                    if delete_unresolved {
                        let params: [&dyn ToSqlValue; 1] = [&fav_id];
                        self.db
                            .execute("DELETE FROM favorites WHERE id = ?", &params)?;
                        warn!(
                            profile_id,
                            item_type, item_id, "favorite_orphan_deleted_unresolvable"
                        );
                        stats.deleted += 1;
                    } else {
                        stats.unresolved += 1;
                    }
                }
            }
        }
        Ok(stats)
    }

    /// Identité de l'item vivant référencé par (item_type, item_id), ou None
    /// s'il n'existe plus.
    fn lookup_live(&self, item_type: &str, item_id: i64) -> Result<Option<Identity>, String> {
        let sql = match item_type {
            "album" => {
                "SELECT a.title, COALESCE(ar.name, ''), '' FROM albums a \
                 LEFT JOIN artists ar ON ar.id = a.artist_id WHERE a.id = ?"
            }
            "track" => {
                "SELECT t.title, COALESCE(ar.name, ''), COALESCE(t.file_path, '') \
                 FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id WHERE t.id = ?"
            }
            "artist" => "SELECT name, '', '' FROM artists WHERE id = ?",
            // Une playlist n'a ni artiste ni chemin : son nom EST son identité.
            "playlist" => "SELECT name, '', '' FROM playlists WHERE id = ?",
            _ => return Ok(None),
        };
        let params: [&dyn ToSqlValue; 1] = [&item_id];
        Ok(self.db.query_one(sql, &params)?.map(|cols| Identity {
            name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
            artist: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            path: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        }))
    }

    /// Identité de secours via l'historique d'écoute : `listen_history` garde
    /// titre/artiste inline. `album_id` n'a pas de FK (colonne ajoutée par
    /// ALTER) et survit à la suppression de l'album ; `track_id` est mis à
    /// NULL par sa FK sous SQLite mais survit sur les bases PG migrées (sans
    /// contraintes FK), donc on tente les deux.
    fn history_identity(&self, item_type: &str, item_id: i64) -> Result<Option<Identity>, String> {
        let sql = match item_type {
            "album" => {
                "SELECT album_title, COALESCE(artist_name, '') FROM listen_history \
                 WHERE album_id = ? AND album_title IS NOT NULL AND album_title != '' \
                 ORDER BY id DESC LIMIT 1"
            }
            "track" => {
                "SELECT title, COALESCE(artist_name, '') FROM listen_history \
                 WHERE track_id = ? AND title IS NOT NULL AND title != '' \
                 ORDER BY id DESC LIMIT 1"
            }
            _ => return Ok(None),
        };
        let params: [&dyn ToSqlValue; 1] = [&item_id];
        Ok(self.db.query_one(sql, &params)?.map(|cols| Identity {
            name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
            artist: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            path: String::new(),
        }))
    }

    /// Retrouve l'item vivant correspondant à une identité.
    fn find_live_id(&self, item_type: &str, ident: &Identity) -> Result<Option<i64>, String> {
        match item_type {
            "album" => self.find_album(ident),
            "track" => self.find_track(ident),
            "artist" => self.find_artist(ident),
            "playlist" => self.find_playlist(ident),
            _ => Ok(None),
        }
    }

    fn first_id(rows: &[Vec<super::backend::SqlValue>]) -> Option<i64> {
        rows.first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64())
    }

    fn find_album(&self, ident: &Identity) -> Result<Option<i64>, String> {
        find_album_by_identity(self.db.as_ref(), &ident.name, &ident.artist)
    }

    fn find_track(&self, ident: &Identity) -> Result<Option<i64>, String> {
        // 1. Chemin fichier : identité la plus forte (library clear + rescan
        //    recrée la même piste au même chemin sous un nouvel id).
        if !ident.path.is_empty() {
            let params: [&dyn ToSqlValue; 1] = [&ident.path];
            if let Some(row) = self
                .db
                .query_one("SELECT id FROM tracks WHERE file_path = ?", &params)?
            {
                if let Some(id) = row.first().and_then(|v| v.as_i64()) {
                    return Ok(Some(id));
                }
            }
        }
        if ident.name.is_empty() {
            return Ok(None);
        }
        // 2. Titre + artiste (fichier déplacé) : premier id, déterministe.
        if !ident.artist.is_empty() {
            let params: [&dyn ToSqlValue; 2] = [&ident.name, &ident.artist];
            let rows = self.db.query_many(
                "SELECT t.id FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
                 WHERE LOWER(t.title) = LOWER(?) AND LOWER(COALESCE(ar.name, '')) = LOWER(?) \
                 ORDER BY t.id ASC",
                &params,
            )?;
            if let Some(id) = Self::first_id(&rows) {
                return Ok(Some(id));
            }
            // Artiste connu sans correspondance : pas de repli titre seul
            // (risque d'homonyme d'un autre artiste), même logique que les
            // albums.
            return Ok(None);
        }
        // 3. Titre seul (artiste inconnu), uniquement si non ambigu.
        let params: [&dyn ToSqlValue; 1] = [&ident.name];
        let rows = self.db.query_many(
            "SELECT id FROM tracks WHERE LOWER(title) = LOWER(?)",
            &params,
        )?;
        if rows.len() == 1 {
            return Ok(Self::first_id(&rows));
        }
        Ok(None)
    }

    fn find_artist(&self, ident: &Identity) -> Result<Option<i64>, String> {
        if ident.name.is_empty() {
            return Ok(None);
        }
        let params: [&dyn ToSqlValue; 1] = [&ident.name];
        let rows = self.db.query_many(
            "SELECT id FROM artists WHERE LOWER(name) = LOWER(?) ORDER BY id ASC",
            &params,
        )?;
        Ok(Self::first_id(&rows))
    }

    /// Une playlist se retrouve par son nom, et UNIQUEMENT s'il est
    /// univoque. Deux playlists homonymes ne donnent aucun gagnant : le
    /// re-rattachement au hasard rendrait le favori à la mauvaise liste, ce
    /// qui est pire qu'un cœur éteint — même règle que le repli « titre seul »
    /// des albums.
    fn find_playlist(&self, ident: &Identity) -> Result<Option<i64>, String> {
        if ident.name.is_empty() {
            return Ok(None);
        }
        let params: [&dyn ToSqlValue; 1] = [&ident.name];
        let rows = self.db.query_many(
            "SELECT id FROM playlists WHERE LOWER(name) = LOWER(?)",
            &params,
        )?;
        if rows.len() == 1 {
            return Ok(Self::first_id(&rows));
        }
        Ok(None)
    }

    fn is_favorite(&self, profile_id: i64, item_type: &str, item_id: i64) -> Result<bool, String> {
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &item_type, &item_id];
        match self.db.query_one(
            "SELECT COUNT(*) FROM favorites \
             WHERE profile_id = ? AND item_type = ? AND item_id = ?",
            &params,
        )? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::profile_repo::ProfileRepo;
    use crate::db::sqlite::SqliteDb;

    fn test_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn insert_artist(db: &Arc<dyn DbBackend>, name: &str) -> i64 {
        let params: [&dyn ToSqlValue; 1] = [&name];
        db.execute("INSERT INTO artists (name) VALUES (?)", &params)
            .unwrap();
        db.last_insert_rowid()
    }

    fn insert_album(db: &Arc<dyn DbBackend>, title: &str, artist_id: i64, tracks: i64) -> i64 {
        let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &tracks];
        db.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, ?)",
            &params,
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn insert_track(db: &Arc<dyn DbBackend>, title: &str, artist_id: i64, path: &str) -> i64 {
        let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &path];
        db.execute(
            "INSERT INTO tracks (title, artist_id, file_path) VALUES (?, ?, ?)",
            &params,
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn insert_playlist(db: &Arc<dyn DbBackend>, name: &str) -> i64 {
        let params: [&dyn ToSqlValue; 1] = [&name];
        db.execute("INSERT INTO playlists (name) VALUES (?)", &params)
            .unwrap();
        db.last_insert_rowid()
    }

    fn fav_item_id(db: &Arc<dyn DbBackend>, item_type: &str) -> Option<i64> {
        let params: [&dyn ToSqlValue; 1] = [&item_type];
        db.query_one("SELECT item_id FROM favorites WHERE item_type = ?", &params)
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
    }

    #[test]
    fn add_favorite_snapshots_identity() {
        let db = test_db();
        let artist = insert_artist(&db, "Miles Davis");
        let album = insert_album(&db, "Kind of Blue", artist, 5);

        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "album", album)
            .unwrap();

        let row = db
            .query_one("SELECT item_name, item_artist FROM favorites", &[])
            .unwrap()
            .unwrap();
        assert_eq!(row[0].as_string().as_deref(), Some("Kind of Blue"));
        assert_eq!(row[1].as_string().as_deref(), Some("Miles Davis"));
    }

    #[test]
    fn relink_album_after_id_renewal() {
        // Scénario .18 : rescan avec nouvelles racines → l'album favori est
        // supprimé (prune + orphan cleanup) puis recréé sous un nouvel id.
        let db = test_db();
        let artist = insert_artist(&db, "Miles Davis");
        let old_album = insert_album(&db, "Kind of Blue", artist, 5);
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "album", old_album)
            .unwrap();

        let params: [&dyn ToSqlValue; 1] = [&old_album];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();
        let new_album = insert_album(&db, "Kind of Blue", artist, 5);
        assert_ne!(old_album, new_album);

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 1);
        assert_eq!(stats.deleted, 0);
        assert_eq!(fav_item_id(&db, "album"), Some(new_album));
    }

    #[test]
    fn relink_track_by_file_path() {
        // library clear + rescan : même chemin, nouvel id.
        let db = test_db();
        let artist = insert_artist(&db, "Nina Simone");
        let old_track = insert_track(&db, "Sinnerman", artist, "/music/nina/sinnerman.flac");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "track", old_track)
            .unwrap();

        let params: [&dyn ToSqlValue; 1] = [&old_track];
        db.execute("DELETE FROM tracks WHERE id = ?", &params)
            .unwrap();
        let new_track = insert_track(&db, "Sinnerman", artist, "/music/nina/sinnerman.flac");

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 1);
        assert_eq!(fav_item_id(&db, "track"), Some(new_track));
    }

    #[test]
    fn relink_track_moved_file_by_title_artist() {
        // Fichier déplacé : le chemin ne matche plus, titre+artiste si.
        let db = test_db();
        let artist = insert_artist(&db, "Nina Simone");
        let old_track = insert_track(&db, "Sinnerman", artist, "/old/sinnerman.flac");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "track", old_track)
            .unwrap();

        let params: [&dyn ToSqlValue; 1] = [&old_track];
        db.execute("DELETE FROM tracks WHERE id = ?", &params)
            .unwrap();
        let new_track = insert_track(&db, "Sinnerman", artist, "/new/nina/sinnerman.flac");

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 1);
        assert_eq!(fav_item_id(&db, "track"), Some(new_track));
    }

    #[test]
    fn relink_album_via_listen_history_without_snapshot() {
        // Bibliothèque déjà cassée AVANT l'instantané (cas .18) : le favori
        // n'a aucun snapshot, mais l'album a été écouté — listen_history garde
        // album_id + titre/artiste inline.
        let db = test_db();
        let artist = insert_artist(&db, "Miles Davis");
        let living = insert_album(&db, "Kind of Blue", artist, 5);

        // Favori orphelin sans instantané, id mort.
        let dead_id = 2108i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
            &params,
        )
        .unwrap();
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO listen_history (track_id, title, artist_name, album_title, album_id) \
             VALUES (NULL, 'So What', 'Miles Davis', 'Kind of Blue', ?)",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 1);
        assert_eq!(fav_item_id(&db, "album"), Some(living));
    }

    #[test]
    fn ambiguous_title_without_artist_is_not_relinked() {
        // Deux albums homonymes d'artistes différents : sans artiste connu,
        // on ne re-rattache pas au hasard.
        let db = test_db();
        let a1 = insert_artist(&db, "Artist A");
        let a2 = insert_artist(&db, "Artist B");
        insert_album(&db, "Greatest Hits", a1, 10);
        insert_album(&db, "Greatest Hits", a2, 12);

        let dead_id = 999i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id, item_name) \
             VALUES (1, 'album', ?, 'Greatest Hits')",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 0);
        assert_eq!(stats.unresolved, 1);
        // Toujours là, non supprimé.
        assert_eq!(fav_item_id(&db, "album"), Some(dead_id));
    }

    #[test]
    fn known_artist_never_relinks_to_homonym_of_other_artist() {
        // Cas réel .18 : « ok » de daoud disparu, « OK » de Talvin Singh
        // vivant. L'artiste du favori est connu et ne matche pas → on ne
        // re-rattache PAS le titre homonyme, même s'il est unique.
        let db = test_db();
        let other = insert_artist(&db, "Talvin Singh");
        insert_album(&db, "OK", other, 8);

        let dead_id = 2108i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id, item_name, item_artist) \
             VALUES (1, 'album', ?, 'ok', 'daoud')",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 0);
        assert_eq!(stats.unresolved, 1);
        assert_eq!(fav_item_id(&db, "album"), Some(dead_id));
    }

    #[test]
    fn snapshot_artist_disambiguates_homonym_albums() {
        let db = test_db();
        let a1 = insert_artist(&db, "Artist A");
        let a2 = insert_artist(&db, "Artist B");
        insert_album(&db, "Greatest Hits", a1, 10);
        let wanted = insert_album(&db, "Greatest Hits", a2, 12);

        let dead_id = 999i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id, item_name, item_artist) \
             VALUES (1, 'album', ?, 'Greatest Hits', 'Artist B')",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.relinked, 1);
        assert_eq!(fav_item_id(&db, "album"), Some(wanted));
    }

    #[test]
    fn unresolved_orphan_kept_at_startup_deleted_after_full_scan() {
        let db = test_db();
        let dead_id = 33604i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', ?)",
            &params,
        )
        .unwrap();

        // Démarrage / scan partiel : conservé.
        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.unresolved, 1);
        assert_eq!(stats.deleted, 0);
        assert_eq!(fav_item_id(&db, "track"), Some(dead_id));

        // Scan complet sain : vraiment introuvable → supprimé.
        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(true)
            .unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(fav_item_id(&db, "track"), None);
    }

    #[test]
    fn relink_dedups_when_target_already_favorited() {
        // L'utilisateur a re-cliqué le cœur sur le nouvel id avant la
        // réconciliation : l'orphelin devient un doublon → retiré, sans
        // violer UNIQUE(profile_id, item_type, item_id).
        let db = test_db();
        let artist = insert_artist(&db, "Miles Davis");
        let living = insert_album(&db, "Kind of Blue", artist, 5);
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "album", living)
            .unwrap();

        let dead_id = 2108i64;
        let params: [&dyn ToSqlValue; 1] = [&dead_id];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id, item_name, item_artist) \
             VALUES (1, 'album', ?, 'Kind of Blue', 'Miles Davis')",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.deduplicated, 1);
        assert_eq!(fav_item_id(&db, "album"), Some(living));
        let count = db
            .query_one("SELECT COUNT(*) FROM favorites", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn streaming_favorites_and_unknown_types_untouched() {
        // `playlist` servait ici d'exemple de type inconnu ; c'est desormais un
        // type LOCAL (#2442). Le contrat teste — un type que la reconciliation
        // ne connait pas n'est ni compte ni supprime — reste le meme, il se
        // verifie juste sur un type reellement etranger.
        let db = test_db();
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'radio', 12345)",
            &[],
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(true)
            .unwrap();
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.deleted, 0);
        assert_eq!(fav_item_id(&db, "radio"), Some(12345));
    }

    // --- Favori de playlist locale (#2442, FabienM fil 1557) ---------------
    //
    // Une playlist locale porte un `INTEGER PRIMARY KEY` : elle entre dans
    // `favorites` sans migration. Mais tant que `playlist` n'est pas un type
    // LOCAL, la reconciliation l'ignore : aucun instantane d'identite n'est
    // fige a l'ajout, et un favori orphelin n'est jamais re-rattache.

    #[test]
    fn un_favori_de_playlist_recoit_son_instantane_d_identite() {
        let db = test_db();
        let pl = insert_playlist(&db, "Dimanche matin");

        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "playlist", pl)
            .unwrap();

        let row = db
            .query_one(
                "SELECT item_name FROM favorites WHERE item_type = 'playlist'",
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            row[0].as_string().as_deref(),
            Some("Dimanche matin"),
            "le nom de la playlist doit etre fige a l'ajout, comme pour un album"
        );
    }

    #[test]
    fn un_favori_de_playlist_survit_a_une_reconciliation_complete() {
        // Le piege de LOCAL_ITEM_TYPES : en ouvrant le type on ouvre AUSSI la
        // suppression des orphelins. Une playlist bien vivante ne doit jamais
        // etre balayee par la passe post-scan la plus agressive.
        let db = test_db();
        let pl = insert_playlist(&db, "Dimanche matin");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "playlist", pl)
            .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(true)
            .unwrap();

        assert_eq!(stats.scanned, 1, "le favori de playlist doit etre examine");
        assert_eq!(stats.deleted, 0);
        assert_eq!(fav_item_id(&db, "playlist"), Some(pl));
    }

    #[test]
    fn un_favori_de_playlist_est_reattache_par_son_nom() {
        // Import M3U rejoue, playlist recreee, bascule SQLite -> PostgreSQL :
        // l'id change, le nom reste. Sans re-rattachement le coeur s'eteint.
        let db = test_db();
        let ancienne = insert_playlist(&db, "Dimanche matin");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "playlist", ancienne)
            .unwrap();

        let params: [&dyn ToSqlValue; 1] = [&ancienne];
        db.execute("DELETE FROM playlists WHERE id = ?", &params)
            .unwrap();
        let nouvelle = insert_playlist(&db, "Dimanche matin");
        assert_ne!(ancienne, nouvelle);

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();

        assert_eq!(stats.relinked, 1);
        assert_eq!(fav_item_id(&db, "playlist"), Some(nouvelle));
    }

    #[test]
    fn un_favori_de_playlist_introuvable_est_purge_apres_un_scan_complet() {
        let db = test_db();
        let pl = insert_playlist(&db, "Dimanche matin");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "playlist", pl)
            .unwrap();
        let params: [&dyn ToSqlValue; 1] = [&pl];
        db.execute("DELETE FROM playlists WHERE id = ?", &params)
            .unwrap();

        // Demarrage / scan partiel : on garde, on ne devine pas.
        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.unresolved, 1);
        assert_eq!(fav_item_id(&db, "playlist"), Some(pl));

        // Scan complet et sain : la playlist n'existe vraiment plus.
        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(true)
            .unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(fav_item_id(&db, "playlist"), None);
    }

    #[test]
    fn deux_playlists_homonymes_ne_sont_pas_reattachees_au_hasard() {
        let db = test_db();
        let ancienne = insert_playlist(&db, "Dimanche matin");
        ProfileRepo::with_backend(db.clone())
            .add_favorite(1, "playlist", ancienne)
            .unwrap();
        let params: [&dyn ToSqlValue; 1] = [&ancienne];
        db.execute("DELETE FROM playlists WHERE id = ?", &params)
            .unwrap();
        insert_playlist(&db, "Dimanche matin");
        insert_playlist(&db, "Dimanche matin");

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(
            stats.relinked, 0,
            "ambigu : on ne re-rattache pas au hasard"
        );
        assert_eq!(stats.unresolved, 1);
    }

    #[test]
    fn backfill_snapshot_on_living_favorites() {
        // Favori d'avant la migration v66 (colonnes NULL) dont l'item vit
        // toujours : la passe remplit l'instantané pour survivre au prochain
        // renouvellement d'ids.
        let db = test_db();
        let artist = insert_artist(&db, "Miles Davis");
        let album = insert_album(&db, "Kind of Blue", artist, 5);
        let params: [&dyn ToSqlValue; 1] = [&album];
        db.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
            &params,
        )
        .unwrap();

        let stats = FavoritesReconciler::with_backend(db.clone())
            .run(false)
            .unwrap();
        assert_eq!(stats.snapshots_backfilled, 1);

        let row = db
            .query_one("SELECT item_name, item_artist FROM favorites", &[])
            .unwrap()
            .unwrap();
        assert_eq!(row[0].as_string().as_deref(), Some("Kind of Blue"));
        assert_eq!(row[1].as_string().as_deref(), Some("Miles Davis"));
    }
}
