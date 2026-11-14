//! L'ANNÉE d'une piste, à la LECTURE : la sienne, à défaut celle de son album.
//!
//! Go de Bertrand du 13/11 (lot rc12). Mesuré sur le .18 : 13 603 pistes
//! locales sur 47 029 sans année (`tracks.year` NULL ou 0), dont 432 dont
//! l'album en porte une ; côté UPnP, 49 121 sur 49 409, dont 6 739
//! récupérables. Ces pistes sortaient de tout filtre par année, de l'onglet
//! Années et des règles `year` des playlists intelligentes alors que leur
//! album dit l'année.
//!
//! 🔴 REPLI DE LECTURE SEULEMENT : rien n'est écrit, ni dans `tracks`, ni
//! dans les balises. L'éditeur de balises, `write_tags` et la fiche d'une
//! piste continuent de lire `tracks.year` tel quel — sinon une réécriture
//! des balises graverait l'année de l'album dans le fichier.
//!
//! Une année à 0 vaut « pas d'année », des deux côtés : le résultat est NULL
//! quand ni la piste ni l'album n'en ont. Même règle que la répartition par
//! décennie de `GET /library/stats` (`repartitions_bibliotheque`), qui
//! l'appliquait déjà.
//!
//! `original_year` n'est PAS concerné : c'est une donnée d'album
//! (`albums.original_year`), sans équivalent de piste.
//!
//! Le SQL est le même sur SQLite et PostgreSQL (`COALESCE`, `NULLIF`,
//! sous-requête scalaire corrélée sur la clé primaire).

/// L'année d'une piste (alias `t`) quand la requête JOINT déjà son album sous
/// l'alias `al` (`LEFT JOIN albums al ON al.id = t.album_id`).
pub const ANNEE_DE_LA_PISTE_AVEC_AL: &str = "COALESCE(NULLIF(t.year, 0), NULLIF(al.year, 0))";

/// L'année d'une piste (alias `t`) SANS jointure sur `albums` : l'année de
/// l'album est lue par une sous-requête corrélée sur sa clé primaire. Pour les
/// conditions qui tournent sur un `FROM tracks t` nu (facettes, filtres).
pub const ANNEE_DE_LA_PISTE: &str = "COALESCE(NULLIF(t.year, 0), \
     (SELECT NULLIF(al_an.year, 0) FROM albums al_an WHERE al_an.id = t.album_id))";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::sqlite::SqliteDb;

    #[test]
    fn la_piste_d_abord_puis_son_album() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db.execute_batch(
            "INSERT INTO albums (id, title, year) VALUES (1, 'A', 1959), (2, 'B', 0), (3, 'C', NULL); \
             INSERT INTO tracks (id, album_id, title, file_path, year) VALUES \
               (1, 1, 'a', '/1', 1960), (2, 1, 'b', '/2', NULL), (3, 1, 'c', '/3', 0), \
               (4, 2, 'd', '/4', NULL), (5, 3, 'e', '/5', 0), (6, NULL, 'f', '/6', NULL);",
        )
        .unwrap();
        for expr in [ANNEE_DE_LA_PISTE, ANNEE_DE_LA_PISTE_AVEC_AL] {
            let sql = format!(
                "SELECT t.id, {expr} FROM tracks t LEFT JOIN albums al ON al.id = t.album_id ORDER BY t.id"
            );
            let lu: Vec<(i64, Option<i64>)> = db
                .query_many(&sql, &[])
                .unwrap()
                .iter()
                .map(|l| (l[0].as_i64().unwrap(), l[1].as_i64()))
                .collect();
            assert_eq!(
                lu,
                vec![
                    (1, Some(1960)),
                    (2, Some(1959)),
                    (3, Some(1959)),
                    (4, None),
                    (5, None),
                    (6, None)
                ],
                "{expr}"
            );
        }
    }
}
