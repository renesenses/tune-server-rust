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
use crate::db::track_repo::sql::chemin_ouvrable;

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
    ///
    /// 🔴 Le chemin passe par [`chemin_ouvrable`], pas par `t.file_path`.
    ///
    /// Une piste découpée par une feuille CUE porte `file_path = NULL` **par
    /// construction** (`scanner::cue_bibliotheque`, `t.file_path = None`) : son
    /// support est `cue_media_path`. Avec `t.file_path IS NOT NULL`, un album
    /// entièrement issu d'une image CUE n'entrait dans AUCUNE portée — ni son
    /// `album_id`, ni ses `artist_ids`. « Enrichir ma collection Jazz » sautait
    /// donc en silence tous les repiquages de vinyle et tous les concerts du
    /// dossier, et `track_count` — le chiffre que la réponse HTTP annonce —
    /// les comptait pour zéro.
    ///
    /// Ce qu'on peut élargir ici SANS rien borner : la portée n'est qu'un
    /// ensemble d'identifiants. Ses consommateurs sont des `retain` sur des
    /// listes d'albums et d'artistes (`library::artwork`, `metadata::bio_batch`,
    /// `metadata::matcher`) et deux `contient_chemin` (`routes/library/enrich`,
    /// `routes/system/enrich`). Aucun ne décode de signal, aucun n'écrit dans
    /// un fichier. Les quinze tranches d'une image insèrent quinze fois le même
    /// `album_id` dans un `HashSet` — idempotent — et comptent bien quinze
    /// pistes, ce qu'elles sont.
    pub fn from_directory(db: &Arc<dyn DbBackend>, dir: &str) -> Self {
        let mut scope = EnrichScope {
            dir: dir.trim_end_matches(['/', '\\']).to_string(),
            ..Default::default()
        };
        let rows = db
            .query_many(
                concat!(
                    "SELECT ",
                    chemin_ouvrable!(),
                    ", t.album_id, t.artist_id, a.artist_id \
                     FROM tracks t LEFT JOIN albums a ON a.id = t.album_id \
                     WHERE t.source = 'local' AND ",
                    chemin_ouvrable!(),
                    " IS NOT NULL"
                ),
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

    /// La bibliothèque de [`base`], plus UN album de feuille CUE : trois
    /// tranches d'une même image, sous `/music/Jazz`.
    ///
    /// 🔴 `file_path` est NUL sur les trois — c'est ÇA le témoin. Une fixture
    /// qui le renseignerait décrirait le cas qui marchait déjà et resterait
    /// verte contre n'importe quel code. Le support commun est
    /// `/music/Jazz/Live 1975/image.flac`, et les tranches ne se distinguent
    /// que par `cue_start_ms` : c'est l'identité que porte l'index unique
    /// partiel `idx_tracks_cue_identity`.
    fn base_avec_album_cue() -> Arc<dyn DbBackend> {
        let backend = base();
        backend
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (4, 'Keith Jarrett'); \
                 INSERT INTO albums (id, title, artist_id) VALUES (4, 'Live 1975', 4); \
                 INSERT INTO tracks \
                   (id, title, album_id, artist_id, file_path, source, cue_media_path, cue_start_ms) \
                   VALUES (20, 'Part I', 4, 4, NULL, 'local', '/music/Jazz/Live 1975/image.flac', 0); \
                 INSERT INTO tracks \
                   (id, title, album_id, artist_id, file_path, source, cue_media_path, cue_start_ms) \
                   VALUES (21, 'Part II', 4, 4, NULL, 'local', '/music/Jazz/Live 1975/image.flac', 1620000); \
                 INSERT INTO tracks \
                   (id, title, album_id, artist_id, file_path, source, cue_media_path, cue_start_ms) \
                   VALUES (22, 'Part III', 4, 4, NULL, 'local', '/music/Jazz/Live 1975/image.flac', 2940000);",
            )
            .unwrap();
        backend
    }

    /// 🔴 UN ALBUM DE FEUILLE CUE ENTRE DANS LA PORTÉE DE SON RÉPERTOIRE.
    ///
    /// Avec `WHERE t.file_path IS NOT NULL`, aucune des trois tranches n'était
    /// lue : `album_ids` ne contenait pas l'album 4, `artist_ids` pas
    /// l'artiste 4, et `track_count` annonçait deux pistes là où il y en a
    /// cinq. « Enrichir /music/Jazz » sautait donc l'album entier — pochette,
    /// biographie, appariement — sans que rien ne le signale.
    #[test]
    fn from_directory_voit_les_pistes_de_feuille_cue() {
        let db = base_avec_album_cue();
        let scope = EnrichScope::from_directory(&db, "/music/Jazz");

        assert_eq!(
            scope.track_count, 5,
            "deux pistes ordinaires + les TROIS tranches de l'image CUE"
        );
        assert!(
            scope.contient_album(4),
            "l'album de feuille CUE doit être dans la portée de son répertoire"
        );
        assert!(
            scope.contient_artiste(4),
            "et son artiste avec lui, sinon la passe de biographies le saute"
        );
        // Le témoin anti-régression : les pistes ordinaires n'ont pas bougé.
        assert!(scope.contient_album(1));
        assert!(!scope.contient_album(2), "album hors répertoire exclu");
    }

    /// Le pendant : une image CUE HORS du répertoire demandé reste dehors.
    ///
    /// Sans cette moitié, un `COALESCE` qui rendrait n'importe quoi de non nul
    /// — ou une garde qui laisserait tout passer — resterait verte au témoin
    /// ci-dessus.
    #[test]
    fn from_directory_exclut_une_image_cue_hors_du_repertoire() {
        let db = base_avec_album_cue();
        let scope = EnrichScope::from_directory(&db, "/music/Electro");
        assert!(
            !scope.contient_album(4),
            "l'album CUE vit sous /music/Jazz, pas sous /music/Electro"
        );
        assert_eq!(scope.track_count, 1, "la seule piste d'Electro");
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
