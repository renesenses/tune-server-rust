//! Portée d'un enrichissement limité à un répertoire (#1660).
//!
//! « Enrichir ma collection Jazz, 6 000 albums mal étiquetés, sans toucher au
//! reste » (jfpaquet). La portée se calcule UNE fois — l'ensemble des albums et
//! des artistes dont au moins une piste vit sous le répertoire demandé — puis
//! s'applique à la **sélection des candidats** de chaque passe du pipeline
//! d'enrichissement existant. Rien d'autre ne change : mêmes passes, mêmes
//! sources, mêmes écritures. Il n'existe pas de « deuxième pipeline ».
//!
//! Le filtrage se fait en Rust, jamais par un `LIKE` SQL : sur PostgreSQL,
//! l'antislash des chemins Windows est un caractère d'échappement et le motif
//! dégénère en silence (vécu : « Dossier vide — 0 pistes » sur les 4 racines
//! de JF Paquet).

use std::collections::HashSet;
use std::sync::Arc;

use crate::db::backend::DbBackend;

/// `path` est-il le dossier `dossier` lui-même, ou en dessous ?
///
/// Même contrat que `sous_le_dossier` du scan ciblé (tune-server), dont c'est
/// désormais l'unique implémentation : les DEUX séparateurs sont acceptés —
/// `music_dirs` et les chemins de la base portent des antislashs sous
/// Windows — et un préfixe de NOM ne suffit pas : `/music/Jazz2` n'est pas
/// sous `/music/Jazz` (#2016, trois occurrences du même défaut).
pub fn sous_le_dossier(path: &str, dossier: &str) -> bool {
    let d = dossier.trim_end_matches(['/', '\\']);
    if path == d {
        return true;
    }
    path.strip_prefix(d)
        .is_some_and(|reste| reste.starts_with('/') || reste.starts_with('\\'))
}

/// Les identifiants qui vivent sous un répertoire de la bibliothèque.
///
/// Se construit par [`EnrichScope::from_directory`], se consomme par les
/// variantes `*_scoped` des passes d'enrichissement, qui en intersectent leurs
/// listes de candidats. `None` partout ailleurs = comportement historique.
#[derive(Debug, Clone, Default)]
pub struct EnrichScope {
    /// Répertoire demandé, sans séparateur final.
    pub dir: String,
    /// Albums ayant au moins une piste locale sous `dir`.
    pub album_ids: HashSet<i64>,
    /// Artistes de ces pistes ET artistes de ces albums (compilations :
    /// l'artiste d'album peut différer de celui des pistes).
    pub artist_ids: HashSet<i64>,
    /// Nombre de pistes locales sous `dir` — pour la réponse HTTP et les logs.
    pub track_count: usize,
}

impl EnrichScope {
    /// Calcule la portée depuis la table `tracks` (pistes locales seulement,
    /// comme les passes d'enrichissement elles-mêmes).
    pub fn from_directory(db: &Arc<dyn DbBackend>, dir: &str) -> Self {
        let mut scope = EnrichScope {
            dir: dir.trim_end_matches(['/', '\\']).to_string(),
            ..Default::default()
        };
        let rows = db
            .query_many(
                "SELECT t.file_path, t.album_id, t.artist_id, a.artist_id \
                 FROM tracks t LEFT JOIN albums a ON a.id = t.album_id \
                 WHERE t.source = 'local' AND t.file_path IS NOT NULL",
                &[],
            )
            .unwrap_or_default();
        for cols in rows {
            let Some(path) = cols.first().and_then(|v| v.as_string()) else {
                continue;
            };
            if !sous_le_dossier(&path, &scope.dir) {
                continue;
            }
            scope.track_count += 1;
            if let Some(id) = cols.get(1).and_then(|v| v.as_i64()) {
                scope.album_ids.insert(id);
            }
            if let Some(id) = cols.get(2).and_then(|v| v.as_i64()) {
                scope.artist_ids.insert(id);
            }
            if let Some(id) = cols.get(3).and_then(|v| v.as_i64()) {
                scope.artist_ids.insert(id);
            }
        }
        scope
    }

    /// La piste à ce chemin est-elle dans la portée ?
    pub fn contient_chemin(&self, path: &str) -> bool {
        sous_le_dossier(path, &self.dir)
    }

    /// Cet album a-t-il au moins une piste dans la portée ?
    pub fn contient_album(&self, id: i64) -> bool {
        self.album_ids.contains(&id)
    }

    /// Cet artiste a-t-il au moins une piste (ou un album) dans la portée ?
    pub fn contient_artiste(&self, id: i64) -> bool {
        self.artist_ids.contains(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    #[test]
    fn sous_le_dossier_contrat() {
        // Le dossier lui-même, et dessous.
        assert!(sous_le_dossier("/music/Jazz", "/music/Jazz"));
        assert!(sous_le_dossier("/music/Jazz/a.flac", "/music/Jazz"));
        assert!(sous_le_dossier("/music/Jazz/Sub/b.flac", "/music/Jazz/"));
        // Un préfixe de NOM n'est pas un sous-dossier.
        assert!(!sous_le_dossier("/music/Jazz2/a.flac", "/music/Jazz"));
        // Hors périmètre.
        assert!(!sous_le_dossier("/autre/a.flac", "/music/Jazz"));
        // Windows : antislashs des deux côtés.
        assert!(sous_le_dossier(
            r"G:\Jazz - Vocal\x\01.flac",
            r"G:\Jazz - Vocal"
        ));
        assert!(!sous_le_dossier(
            r"G:\Jazz - Vocal 2\01.flac",
            r"G:\Jazz - Vocal"
        ));
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        backend
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'); \
                 INSERT INTO artists (id, name) VALUES (2, 'Kraftwerk'); \
                 INSERT INTO artists (id, name) VALUES (3, 'Various Artists'); \
                 INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1); \
                 INSERT INTO albums (id, title, artist_id) VALUES (2, 'Autobahn', 2); \
                 INSERT INTO albums (id, title, artist_id) VALUES (3, 'Jazz Comp', 3); \
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                   VALUES (10, 'So What', 1, 1, '/music/Jazz/Kind of Blue/01.flac', 'local'); \
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                   VALUES (11, 'Autobahn', 2, 2, '/music/Electro/Autobahn/01.flac', 'local'); \
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                   VALUES (12, 'Blue Comp', 3, 1, '/music/Jazz/Comp/01.flac', 'local'); \
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                   VALUES (13, 'Stream', 1, 1, 'qobuz:123', 'qobuz');",
            )
            .unwrap();
        backend
    }

    /// Le cœur du #1660 : la portée retient ce qui vit sous le répertoire, et
    /// RIEN d'autre. L'album témoin hors répertoire (Autobahn) et son artiste
    /// (Kraftwerk) n'y figurent pas — c'est cette intersection qui garantit
    /// que les passes scoped ne les toucheront pas.
    #[test]
    fn from_directory_retient_le_sous_arbre_et_exclut_le_reste() {
        let db = base();
        let scope = EnrichScope::from_directory(&db, "/music/Jazz");

        assert_eq!(scope.track_count, 2, "deux pistes locales sous /music/Jazz");
        assert!(scope.contient_album(1));
        assert!(scope.contient_album(3));
        assert!(!scope.contient_album(2), "album hors répertoire exclu");
        assert!(scope.contient_artiste(1));
        assert!(
            scope.contient_artiste(3),
            "l'artiste d'ALBUM d'une compilation est dans la portée"
        );
        assert!(!scope.contient_artiste(2), "artiste hors répertoire exclu");
        assert!(scope.contient_chemin("/music/Jazz/Comp/01.flac"));
        assert!(!scope.contient_chemin("/music/Electro/Autobahn/01.flac"));
    }

    /// Une piste de streaming sous aucun chemin ne compte jamais.
    #[test]
    fn from_directory_ignore_les_sources_non_locales() {
        let db = base();
        let scope = EnrichScope::from_directory(&db, "/music");
        assert_eq!(scope.track_count, 3, "la piste qobuz ne compte pas");
    }

    /// Répertoire valide mais vide : portée vide, pas d'erreur — les passes
    /// n'auront simplement aucun candidat.
    #[test]
    fn from_directory_repertoire_sans_piste_rend_une_portee_vide() {
        let db = base();
        let scope = EnrichScope::from_directory(&db, "/music/Classique");
        assert_eq!(scope.track_count, 0);
        assert!(scope.album_ids.is_empty());
        assert!(scope.artist_ids.is_empty());
    }
}
