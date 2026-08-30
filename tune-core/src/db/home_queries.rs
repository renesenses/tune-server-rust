//! Les requetes SQL de l'ecran d'accueil, ecrites une fois, valides sur les
//! DEUX moteurs.
//!
//! # Pourquoi elles vivent ici et non dans `tune-server/src/routes/home.rs`
//!
//! Parce que c'est le seul endroit ou la CI sait les EXECUTER sur un vrai
//! PostgreSQL. Le job « Test (PostgreSQL) » monte un serveur 16, applique les
//! migrations numerotees et lance `cargo test -p tune-core`. Une requete
//! redigee dans `tune-server` n'est jouee que sur SQLite, et SQLite tolere
//! precisement ce que PostgreSQL refuse — c'est ainsi que « Continuer
//! l'ecoute » est reste vide sur toute installation PostgreSQL sans qu'aucun
//! test ne rougisse (#2860).
//!
//! Le mappage des colonnes vers le JSON reste cote `tune-server` : seule la
//! chaine SQL descend ici, pour qu'un test PG puisse la donner telle quelle a
//! une vraie base.
//!
//! # Les trois pieges PostgreSQL que ces requetes ont payes
//!
//! Mesures le 30/08/2026 sur une base PostgreSQL 16 montee des scripts
//! numerotes plus le rattrapage `ENSURE_COLUMNS`, c'est-a-dire la forme exacte
//! d'un serveur en service :
//!
//! 1. `lh.album_id = a.id` — `listen_history.album_id` etait TEXT et
//!    `albums.id` BIGINT : `operator does not exist: text = bigint`. Corrige
//!    par la migration 047 et par `ENSURE_COLUMNS`, pas par un transtypage
//!    dans la requete.
//! 2. `GROUP BY a.id` en selectionnant `ar.name` — la dependance fonctionnelle
//!    de PostgreSQL ne couvre que les colonnes de la table dont on groupe la
//!    cle primaire, jamais celles d'une jointure :
//!    `column "ar.name" must appear in the GROUP BY clause or be used in an
//!    aggregate function`.
//! 3. `HAVING listened_tracks < ...` — un alias de la liste SELECT n'existe
//!    pas quand le HAVING est evalue : `column "listened_tracks" does not
//!    exist`. L'alias reste legal en ORDER BY et en GROUP BY.
//!
//! Les trois erreurs etaient avalees par le `unwrap_or_default()` des
//! appelants : pas un message, juste une section absente.

use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Le paramètre lié, dans la syntaxe du moteur.
fn ph(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Rapproche une ligne d'historique (`lh`) de son album (`a`).
///
/// Le titre SEUL ne designe pas un album : un « Live » de Police et un
/// « Live » de Pulp portent le meme titre et se retrouvaient comptes pour un
/// seul disque — un album jamais ecoute remontait dans « Continuer l'ecoute »,
/// et le compteur d'avancement additionnait les pistes des deux (#2731,
/// Tades, fil 1600).
///
/// L'identifiant fait foi quand il est ecrit. Il ne l'est PAS toujours :
/// `record_listen` le tire de la piste locale, donc toute ecoute en flux
/// (track_id absent) et toute ligne anterieure a la migration
/// `add_listen_history_source_id_album_id` l'ont a NULL. Joindre sur le seul
/// `album_id` viderait la section pour ces gens-la ; d'ou le repli sur
/// titre ET artiste.
///
/// Le repli suit la regle de `find_album_by_identity`
/// (`tune-core/src/db/favorites_reconcile.rs`), deja partagee par les favoris
/// et par les albums masques (#1391 / PR #2817) — pas une seconde regle de
/// rapprochement inventee ici, la meme, a une reserve pres (la casse, voir
/// plus bas) :
/// * artiste connu — NULL **et** chaine vide valent « inconnu », comme
///   `artist.is_empty()` la-bas : titre + artiste, l'artiste absent de
///   l'album valant chaine vide ;
/// * artiste inconnu : titre seul, et UNIQUEMENT s'il ne designe qu'un
///   album. Un titre partage par deux disques ne rattache plus rien —
///   c'est le cas de Tades par l'autre porte, et la doctrine de
///   `find_album_by_identity` (« AUCUN repli titre seul » quand ca peut
///   designer l'homonyme d'un autre artiste).
///
/// RESERVE MESUREE — la casse reste significative ici, alors que
/// `find_album_by_identity` compare en `LOWER`. Transposer le `LOWER` dans
/// CETTE jointure coute la section : mesure sur 45 000 albums / 5 000 lignes
/// d'historique (dont 20 % sans `album_id`), meme machine, meme jeu de
/// donnees, `fetch_continue_listening` seul :
///
/// ```text
///   titre compare tel quel   :    19 ms
///   LOWER(...) des deux cotes : 83 662 ms
/// ```
///
/// Quatre mille fois plus cher, sur le chemin de l'accueil. La cause est
/// l'index : `idx_albums_title ON albums(title COLLATE NOCASE)` sert la
/// comparaison directe, aucun index ne sert `LOWER(title)`. Le rendre
/// gratuit demanderait un index d'expression AUX QUATRE endroits du schema
/// (CORE_SCHEMA, migration SQLite, PG_FULL_SCHEMA, migration PG) — et
/// PostgreSQL, lui, n'a pas d'equivalent de `COLLATE NOCASE` a portee de
/// main. Hors du perimetre de #2731 : c'est un chantier de schema, pas la
/// confusion de deux albums. Le test
/// `la_casse_separe_encore_une_ecoute_de_son_album` fige la limite pour
/// qu'on ne la redecouvre pas une troisieme fois.
///
/// Le sous-select sur `artists` evite de dependre de l'ordre des jointures —
/// `ar` n'existe pas encore quand cette condition est evaluee, et le
/// GROUP BY de PostgreSQL n'accepte pas qu'on enveloppe `albums` dans une
/// table derivee (la dependance fonctionnelle ne vaut que pour la cle
/// primaire d'une vraie table).
///
/// Cout : les deux sous-selects ne sont atteints QUE par les lignes a
/// `album_id` NULL dont le titre d'album tombe deja juste ; le chemin
/// nominal reste `lh.album_id = a.id`. Mesure ci-dessus.
pub const HISTORIQUE_VERS_ALBUM: &str = "(lh.album_id = a.id \
     OR (lh.album_id IS NULL AND lh.album_title = a.title \
         AND ((COALESCE(lh.artist_name, '') <> '' \
               AND lh.artist_name \
                   = (SELECT ar_hist.name FROM artists ar_hist \
                      WHERE ar_hist.id = a.artist_id)) \
              OR (COALESCE(lh.artist_name, '') = '' \
                  AND NOT EXISTS (SELECT 1 FROM albums a_hom \
                                  WHERE a_hom.title = a.title \
                                    AND a_hom.id <> a.id)))))";

/// Les colonnes d'album de « Continuer l'ecoute » et d'« Ajoutes recemment ».
///
/// Repetees a l'identique dans le `GROUP BY` : PostgreSQL ne deduit `ar.name`
/// d'aucune cle primaire groupee, et se taire ici coute la section entiere.
const COLONNES_ALBUM: &str = "a.id, a.title, ar.name, a.year, a.cover_path, a.genre";

/// Second rang de « Continuer l'ecoute » : les albums DEDUITS de l'historique,
/// pour les lignes anterieures a la migration 84 qui ne disent rien de leur
/// contexte.
///
/// `zone_filter` est injecte tel quel par l'appelant (`AND lh.zone_id = N `,
/// ou vide) : c'est un entier formate, pas une saisie.
///
/// Colonnes rendues, dans l'ordre : `id, title, artist_name, year, cover_path,
/// genre, listened_tracks, track_count, dernier`.
pub fn continue_listening_albums_deduits(engine: Engine, zone_filter: &str) -> String {
    let p1 = ph(engine, 1);
    format!(
        "SELECT {COLONNES_ALBUM}, \
               COUNT(DISTINCT lh.title) as listened_tracks, a.track_count, \
               MAX(lh.listened_at) as dernier \
        FROM listen_history lh \
        JOIN albums a ON {HISTORIQUE_VERS_ALBUM} \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE a.track_count IS NOT NULL AND a.track_count > 0 \
        {zone_filter}\
        GROUP BY {COLONNES_ALBUM}, a.track_count \
        HAVING COUNT(DISTINCT lh.title) < a.track_count \
           AND SUM(CASE WHEN lh.context_type IS NULL THEN 1 ELSE 0 END) > 0 \
        ORDER BY MAX(lh.listened_at) DESC \
        LIMIT {p1}"
    )
}

/// « Ajoutes recemment » : les albums dont une piste a ete ecrite sur le disque
/// depuis `$1`.
///
/// Jumelle exacte du defaut ci-dessus : meme `ar.name`, meme `GROUP BY a.id`,
/// meme section vide sur PostgreSQL (#2860).
///
/// Colonnes rendues, dans l'ordre : `id, title, artist_name, year, cover_path,
/// genre, format, sample_rate, bit_depth, track_count, newest_mtime`.
pub fn recently_added(engine: Engine) -> String {
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    format!(
        "SELECT {COLONNES_ALBUM}, \
               a.format, a.sample_rate, a.bit_depth, a.track_count, \
               MAX(t.file_mtime) as newest_mtime \
        FROM tracks t \
        JOIN albums a ON t.album_id = a.id \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL AND t.file_mtime > {p1} \
        GROUP BY {COLONNES_ALBUM}, a.format, a.sample_rate, a.bit_depth, a.track_count \
        ORDER BY newest_mtime DESC \
        LIMIT {p2}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le defaut de #2860 tel qu'il s'ecrivait : `GROUP BY a.id` tout court.
    ///
    /// Un `GROUP BY` qui ne nomme pas `ar.name` alors que le SELECT la porte
    /// ECHOUE sur PostgreSQL. Le test PG `pg_2860_*` le prouve en base ; ici on
    /// interdit la reecriture du motif, y compris pour le moteur SQLite ou il
    /// passerait sans bruit.
    #[test]
    fn le_group_by_nomme_toutes_les_colonnes_non_agregees() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            for sql in [
                continue_listening_albums_deduits(engine, ""),
                recently_added(engine),
            ] {
                let group_by = sql
                    .split("GROUP BY ")
                    .nth(1)
                    .expect("la requete porte un GROUP BY");
                assert!(
                    group_by.contains("ar.name"),
                    "`ar.name` est selectionnee mais absente du GROUP BY — \
                     PostgreSQL rend « column \"ar.name\" must appear in the \
                     GROUP BY clause » et la section disparait (#2860). SQL :\n{sql}"
                );
            }
        }
    }

    /// Le second defaut : un alias de la liste SELECT dans le HAVING.
    ///
    /// PostgreSQL evalue le HAVING avant de nommer les colonnes de sortie :
    /// `column "listened_tracks" does not exist`.
    #[test]
    fn le_having_ne_cite_aucun_alias_de_la_liste_select() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            let sql = continue_listening_albums_deduits(engine, "");
            let having = sql
                .split("HAVING ")
                .nth(1)
                .and_then(|s| s.split(" ORDER BY").next())
                .expect("la requete porte un HAVING");
            assert!(
                !having.contains("listened_tracks"),
                "l'alias `listened_tracks` est cite dans le HAVING — \
                 PostgreSQL rend « column \"listened_tracks\" does not exist » \
                 (#2860). HAVING :\n{having}"
            );
        }
    }

    /// Le filtre de zone reste injecte, et le parametre lie suit le moteur.
    #[test]
    fn le_dialecte_et_le_filtre_de_zone_sont_respectes() {
        let sqlite = continue_listening_albums_deduits(Engine::Sqlite, "AND lh.zone_id = 3 ");
        assert!(sqlite.contains("AND lh.zone_id = 3 "));
        assert!(
            sqlite.ends_with("LIMIT ?"),
            "SQLite : place tenue positionnelle"
        );

        let pg = continue_listening_albums_deduits(Engine::Postgres, "");
        assert!(!pg.contains("zone_id = "));
        assert!(pg.ends_with("LIMIT $1"));

        assert!(recently_added(Engine::Sqlite).contains("file_mtime > ?"));
        assert!(recently_added(Engine::Postgres).contains("file_mtime > $1"));
    }
}
