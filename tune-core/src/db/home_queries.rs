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

/// La jointure qui donne la date de PREMIERE VUE d'un fichier par le scan.
///
/// `file_first_seen` (#473) est une table a cote, jamais purgee par
/// `delete_all` : un rescan complet reecrit `tracks` mais ne touche pas a ces
/// horodatages. Elle est ecrite `INSERT OR IGNORE` a la premiere insertion
/// d'un chemin, donc elle porte bien « ajoute a la bibliotheque le … » et non
/// « fichier ecrit sur le disque le … ».
///
/// Elle exige l'alias `t` pour `tracks`.
pub const JOINTURE_PREMIERE_VUE: &str =
    "LEFT JOIN file_first_seen ffs ON ffs.file_path = t.file_path";

/// La duree d'une piste en millisecondes, lisible quel que soit le type de la
/// colonne. Voir [`recently_added_totaux`] pour la derive de type qui l'exige.
pub const DUREE_MS: &str = "CAST(NULLIF(CAST(t.duration_ms AS TEXT), '') AS DOUBLE PRECISION)";

/// La date d'ajout d'une piste : sa premiere vue si le scan l'a enregistree,
/// sinon SEULEMENT le `mtime` du fichier.
///
/// Le repli n'est pas un detail : `file_first_seen` n'est peuplee que depuis
/// #473, donc une bibliotheque scannee avant porte encore des pistes sans
/// ligne. Pour celles-la l'expression rend exactement l'ancienne valeur, et la
/// vue se comporte comme avant. Pour les autres elle dit la verite : un
/// `rsync -a`, une restauration de sauvegarde ou une recopie deplacent le
/// `mtime`, jamais la premiere vue.
///
/// Transtypage : `file_first_seen.first_seen_at` est DOUBLE, mais le type de
/// `tracks.file_mtime` sur PostgreSQL depend du millesime de l'installation
/// (TEXT sur certaines, DOUBLE PRECISION sur .15). Meme forme que
/// `AlbumRepo::ADDED_AT_JOIN`, et pour la meme raison : un `COALESCE(double,
/// text)` est une erreur dure sur les installations TEXT, et
/// `NULLIF(double, '')` en est une a l'analyse sur les installations DOUBLE.
/// Passer par TEXT puis retranstyper est valide dans les deux cas, et sur
/// SQLite dont les affinites sont souples.
pub const DATE_D_AJOUT: &str = "COALESCE(ffs.first_seen_at, \
     CAST(NULLIF(CAST(t.file_mtime AS TEXT), '') AS DOUBLE PRECISION))";

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

/// Les cinq natures que `contexte_de_lecture` (tune-server/src/routes/
/// playback.rs) sait ecrire, telles que FabienM les a enumerees (#2441).
pub const CONTEXTES_AFFICHES: [&str; 5] = ["album", "playlist", "artist", "label", "track"];

/// Premier rang de « Continuer l'ecoute » : la derniere ecoute de CHAQUE
/// contexte distinct — l'objet que l'auditeur a demande, et ou il en etait.
///
/// # Pourquoi cette requete est descendue ici (#2441)
///
/// Elle etait redigee dans `tune-server/src/routes/home.rs`. C'est le crate
/// que le job « Test (PostgreSQL) » ne compile PAS : il lance
/// `cargo test -p tune-core`. La requete qui EST le correctif de #2441
/// n'etait donc jouee que sur SQLite — exactement la situation qui avait
/// laisse « Continuer l'ecoute » vide sur tout serveur PostgreSQL pendant des
/// mois (#2860), sans qu'aucun test ne rougisse, l'erreur etant avalee par le
/// `unwrap_or_default()` de l'appelant.
///
/// Mesure du 01/09/2026 sur PostgreSQL 15, schema monte des scripts numerotes
/// plus `ENSURE_COLUMNS` : la requete s'execute et rend ses colonnes. Le
/// defaut de #2860 ne se rejouait pas ici — mais rien ne le prouvait, et
/// `pg_2441_*` le prouve desormais a chaque promotion.
///
/// # L'ordre est TOTAL, a dessein
///
/// `ORDER BY lh.listened_at DESC` seul ne departage pas deux contextes ecoutes
/// dans la MEME seconde — or `listened_at` est au format seconde, et deux
/// gestes rapproches y tombent. Avec un `LIMIT`, un ordre partiel laisse le
/// moteur choisir QUI entre dans la section et qui disparait, sans rien dire.
///
/// RESERVE — sur le jeu mesure (trois contextes a la meme seconde, `LIMIT 2`)
/// les deux moteurs rendaient DEJA le meme couple : la divergence est latente,
/// pas observee. Le departage est donc une GARDE, pas la reparation d'un
/// defaut constate. Il est gratuit : `EXPLAIN QUERY PLAN` sur SQLite montre le
/// meme `USE TEMP B-TREE FOR ORDER BY` avant et apres, la jointure continuant
/// de passer par `idx_listen_history_listened_at`.
///
/// `zone_filter` est injecte tel quel par l'appelant (`AND lh.zone_id = N `,
/// ou vide) : c'est un entier formate, pas une saisie.
///
/// Colonnes rendues, dans l'ordre : `context_type, context_id, listened_at,
/// context_position, title, artist_name, album_title, cover_url, album_id,
/// source`.
pub fn continue_listening_contextes(engine: Engine, zone_filter: &str) -> String {
    let p1 = ph(engine, 1);
    let natures = CONTEXTES_AFFICHES
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // La ligne la PLUS RECENTE de chaque contexte : c'est elle qui porte le
    // rang atteint, et les champs d'affichage de repli. La jointure sur le
    // MAX plutot qu'une fonction de fenetrage — les deux moteurs la
    // comprennent, `ROW_NUMBER() OVER` n'existe pas sur toutes les versions de
    // SQLite embarquees.
    format!(
        "SELECT lh.context_type, lh.context_id, lh.listened_at, \
                lh.context_position, lh.title, lh.artist_name, lh.album_title, \
                lh.cover_url, lh.album_id, lh.source \
         FROM listen_history lh \
         JOIN (SELECT context_type, context_id, MAX(listened_at) as dernier \
               FROM listen_history lh \
               WHERE lh.context_type IN ({natures}) \
                 AND lh.context_id IS NOT NULL \
                 {zone_filter}\
               GROUP BY context_type, context_id) d \
           ON d.context_type = lh.context_type \
          AND d.context_id = lh.context_id \
          AND d.dernier = lh.listened_at \
         WHERE lh.context_type IN ({natures}) \
         {zone_filter}\
         ORDER BY lh.listened_at DESC, lh.context_type ASC, lh.context_id ASC \
         LIMIT {p1}"
    )
}

/// Les albums LOCAUX designes par des contextes `album`, avec leur avancement.
///
/// C'est CETTE requete qui produit les deux nombres dont sort la barre de
/// progression (`listened_tracks` et `track_count`). Descendue ici avec sa
/// jumelle ci-dessus, et pour la meme raison : redigee dans `tune-server`,
/// elle n'etait jamais executee sur PostgreSQL, alors qu'elle porte
/// exactement les deux pieges qui avaient vide la section (#2860) — `ar.name`
/// selectionnee depuis une AUTRE table, et un `GROUP BY` qui doit donc etre
/// exhaustif.
///
/// `ids` est interpole tel quel : ce sont des `i64` reformates par l'appelant,
/// pas une saisie.
///
/// Colonnes rendues, dans l'ordre : `id, title, artist_name, year, cover_path,
/// genre, listened_tracks, track_count`.
pub fn continue_listening_albums_du_contexte(ids: &[i64]) -> String {
    let liste = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {COLONNES_ALBUM}, \
                COUNT(DISTINCT lh.title) as listened_tracks, a.track_count \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         LEFT JOIN listen_history lh ON {HISTORIQUE_VERS_ALBUM} \
         WHERE a.id IN ({liste}) \
         GROUP BY {COLONNES_ALBUM}, a.track_count"
    )
}

/// L'avancement d'une entree de « Continuer l'ecoute », en pourcentage entier.
///
/// # Le defaut repare (#2441, arbitrage de Bertrand du 01/09/2026)
///
/// « Corriger au passage le champ de progression que le client lit et que le
/// serveur n'emet nulle part. » `HomeView.svelte` dessine la barre sous la
/// vignette derriere `{#if item.progress_percent != null}` — et
/// `progress_percent` n'apparaissait NULLE PART dans ce depot (compte : zero
/// occurrence). La barre etait donc morte sur toute installation, quel que
/// soit le moteur : la condition n'a jamais pu etre vraie.
///
/// # Pourquoi le calcul est en Rust et NON en SQL
///
/// `100 * ecoutees / total` en SQL diverge entre les deux moteurs des que
/// `total` vaut 0 : SQLite rend NULL, PostgreSQL LEVE `division by zero` — et
/// l'erreur, avalee par le `unwrap_or_default()` de l'appelant, ferait
/// disparaitre la section entiere au lieu d'une barre. C'est la forme exacte
/// du defaut #2860. Le garde-fou `total > 0` ci-dessous est donc la raison
/// d'etre de cette fonction, pas un detail.
///
/// Renvoie `None` — donc `null`, donc pas de barre — quand l'avancement n'a
/// pas de sens : nature sans notion de completude (playlist, artiste, label,
/// titre isole), album de streaming dont on ignore le nombre de pistes, ou
/// `track_count` a zero. Mieux vaut aucune barre qu'une barre a 0 % qui
/// ferait croire a une ecoute jamais commencee.
///
/// Borne a 100 : `listened_tracks` compte les titres DISTINCTS de l'historique
/// rattaches a l'album, et un disque dont une piste a ete renommee depuis le
/// scan peut en compter plus que `track_count`. Une barre a 130 % deborderait
/// sa gouttiere.
pub fn progression_pourcent(ecoutees: Option<i64>, total: Option<i64>) -> Option<i64> {
    let (ecoutees, total) = (ecoutees?, total?);
    if total <= 0 || ecoutees < 0 {
        return None;
    }
    Some((ecoutees.saturating_mul(100) / total).min(100))
}

/// « Ajoutes recemment » : les albums dont une piste est entree dans la
/// bibliotheque depuis `$1`, au plus `$2`.
///
/// `$1` est la BORNE BASSE de la fenetre, en secondes epoch. Elle etait
/// calculee a 7 jours en dur par l'appelant et n'etait donc reglable par
/// personne (#3039) ; c'est desormais l'appelant qui la choisit, et lui seul
/// qui plafonne. La requete, elle, ne connait qu'un instant.
///
/// Jumelle exacte du defaut ci-dessus : meme `ar.name`, meme `GROUP BY a.id`,
/// meme section vide sur PostgreSQL (#2860).
///
/// Colonnes rendues, dans l'ordre : `id, title, artist_name, year, cover_path,
/// genre, format, sample_rate, bit_depth, track_count, added_at`.
pub fn recently_added(engine: Engine) -> String {
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    format!(
        "SELECT {COLONNES_ALBUM}, \
               a.format, a.sample_rate, a.bit_depth, a.track_count, \
               MAX({DATE_D_AJOUT}) as added_at \
        FROM tracks t \
        JOIN albums a ON t.album_id = a.id \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        {JOINTURE_PREMIERE_VUE} \
        WHERE {DATE_D_AJOUT} > {p1} \
        GROUP BY {COLONNES_ALBUM}, a.format, a.sample_rate, a.bit_depth, a.track_count \
        ORDER BY added_at DESC \
        LIMIT {p2}"
    )
}

/// Le decompte de la meme fenetre : combien d'albums, combien de pistes,
/// combien de temps.
///
/// C'est le sous-titre que le testeur montre — « 7 albums • 71 pistes •
/// 5 h 55 min » (#3039). Il se calcule ici et non en comptant les elements
/// rendus par [`recently_added`], que le `LIMIT` tronque : compter la page
/// affichee annoncerait « 20 albums » sur une fenetre qui en porte 300.
///
/// DEUX transtypages, et aucun n'est decoratif :
///
/// * `t.duration_ms` n'a pas le meme type partout. `010_numeric_column_types`
///   le porte en BIGINT, mais l'outil de reprise SQLite → PostgreSQL
///   (`pg_migrate`) le pose en TEXT, exactement la derive qui a coute la .15
///   sur `file_mtime` (#550). Or `SUM(text)` n'existe pas sur PostgreSQL :
///   `function sum(text) does not exist`, avalee par le
///   `ou_defaut_journalise()` de l'appelant, et le sous-titre annoncerait
///   « 0 min » sans un mot. Le detour par TEXT puis DOUBLE est valide quel que
///   soit le type de depart, sur les deux moteurs.
/// * `SUM` rend NUMERIC sur PostgreSQL et non BIGINT ; sans le `CAST` final la
///   valeur reviendrait en chaine d'un cote et en entier de l'autre.
///
/// `$1` : la meme borne basse. Colonnes rendues : `albums, tracks,
/// duration_ms`.
pub fn recently_added_totaux(engine: Engine) -> String {
    let p1 = ph(engine, 1);
    format!(
        "SELECT COUNT(DISTINCT a.id) AS albums, \
                COUNT(*) AS tracks, \
                CAST(COALESCE(SUM({DUREE_MS}), 0) AS BIGINT) AS duration_ms \
         FROM tracks t \
         JOIN albums a ON t.album_id = a.id \
         {JOINTURE_PREMIERE_VUE} \
         WHERE {DATE_D_AJOUT} > {p1}"
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
    /// L'avancement est calcule en Rust, JAMAIS en SQL — et ce test dit
    /// pourquoi. `100 * ecoutees / total` avec `total` a zero rend NULL sur
    /// SQLite et LEVE `division by zero` sur PostgreSQL ; l'erreur, avalee par
    /// le `unwrap_or_default()` de l'appelant, emporterait la section entiere
    /// au lieu d'une seule barre. C'est la forme exacte du defaut #2860.
    #[test]
    fn un_album_sans_piste_ne_divise_pas_par_zero() {
        assert_eq!(progression_pourcent(Some(3), Some(0)), None);
        assert_eq!(progression_pourcent(Some(0), Some(0)), None);
        assert_eq!(progression_pourcent(Some(3), None), None);
        assert_eq!(progression_pourcent(None, Some(5)), None);
    }

    /// Les nombres que les deux tests de moteur exigent, et les deux bornes.
    #[test]
    fn l_avancement_est_borne_et_entier() {
        assert_eq!(progression_pourcent(Some(1), Some(5)), Some(20));
        assert_eq!(progression_pourcent(Some(2), Some(5)), Some(40));
        assert_eq!(progression_pourcent(Some(3), Some(5)), Some(60));
        assert_eq!(progression_pourcent(Some(0), Some(5)), Some(0));
        // Un titre renomme depuis le scan peut faire compter plus de pistes
        // ecoutees que l'album n'en porte : la barre ne doit pas deborder.
        assert_eq!(progression_pourcent(Some(9), Some(5)), Some(100));
        assert_eq!(progression_pourcent(Some(5), Some(5)), Some(100));
    }

    /// L'ordre de la section doit etre TOTAL : `listened_at` seul ne departage
    /// pas deux gestes de la meme seconde, et le `LIMIT` laisse alors le
    /// moteur choisir qui disparait.
    #[test]
    fn l_ordre_des_contextes_est_total() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            let sql = continue_listening_contextes(engine, "");
            let order_by = sql
                .split("ORDER BY ")
                .nth(1)
                .expect("la requete porte un ORDER BY");
            assert!(
                order_by.contains("lh.context_type") && order_by.contains("lh.context_id"),
                "`ORDER BY lh.listened_at DESC` seul n'est pas un ordre total : \
                 deux contextes de la meme seconde se classent au hasard et le \
                 LIMIT en supprime un en silence. SQL :\n{sql}"
            );
        }
    }

    #[test]
    fn le_group_by_nomme_toutes_les_colonnes_non_agregees() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            for sql in [
                continue_listening_albums_deduits(engine, ""),
                recently_added(engine),
                continue_listening_albums_du_contexte(&[1, 2]),
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
    }

    /// #3039 — la fenetre est une PLACE TENUE, sur les DEUX moteurs.
    ///
    /// Le defaut d'origine n'etait pas une mauvaise valeur : c'etait l'absence
    /// de valeur reglable. `chrono_epoch_seven_days_ago()` ne prenait aucun
    /// argument, et la requete recevait donc toujours le meme instant. Le
    /// garde exige ici que la borne basse arrive LIEE — `?` sur SQLite, `$1`
    /// sur PostgreSQL — pour qu'un litteral reintroduit d'un cote ou de
    /// l'autre rougisse.
    #[test]
    fn la_fenetre_d_ajouts_recents_est_liee_sur_les_deux_moteurs() {
        for (engine, place) in [(Engine::Sqlite, "?"), (Engine::Postgres, "$1")] {
            for sql in [recently_added(engine), recently_added_totaux(engine)] {
                let filtre = sql
                    .split("WHERE ")
                    .nth(1)
                    .expect("la requete porte un WHERE");
                assert!(
                    filtre.starts_with(&format!("{DATE_D_AJOUT} > {place}")),
                    "la borne basse de la fenetre n'est pas liee sur {engine:?} : \
                     une fenetre ecrite en dur n'est reglable par personne (#3039). \
                     WHERE :\n{filtre}"
                );
            }
        }
    }

    /// #3039 — la vue ordonne une DATE D'AJOUT, pas un `mtime` nu.
    ///
    /// Le `mtime` d'un fichier n'est pas sa date d'entree dans la
    /// bibliotheque : un `rsync -a`, une restauration de sauvegarde ou une
    /// recopie le deplacent, et l'album ressort « ajoute aujourd'hui » alors
    /// qu'il est la depuis dix ans. `file_first_seen` porte la vraie date ;
    /// le `mtime` n'est plus qu'un repli pour les pistes scannees avant #473.
    #[test]
    fn les_ajouts_recents_preferent_la_premiere_vue_au_mtime() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            for sql in [recently_added(engine), recently_added_totaux(engine)] {
                assert!(
                    sql.contains(JOINTURE_PREMIERE_VUE),
                    "la jointure sur `file_first_seen` a disparu : la vue \
                     retomberait sur le seul `mtime` (#3039). SQL :\n{sql}"
                );
                assert!(
                    sql.contains("ffs.first_seen_at"),
                    "`first_seen_at` n'est pas lue — la jointure ne sert a rien. \
                     SQL :\n{sql}"
                );
                assert!(
                    !sql.contains("t.file_mtime >"),
                    "le filtre porte encore sur le `mtime` nu. SQL :\n{sql}"
                );
            }
        }
    }

    /// Le decompte porte les TROIS nombres du sous-titre, et il compte la
    /// FENETRE, pas la page : aucun `LIMIT` ne doit s'y glisser.
    #[test]
    fn le_decompte_des_ajouts_recents_ne_se_limite_pas() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            let sql = recently_added_totaux(engine);
            assert!(sql.contains("COUNT(DISTINCT a.id)"), "albums : {sql}");
            assert!(sql.contains("COUNT(*)"), "pistes : {sql}");
            assert!(sql.contains(&format!("SUM({DUREE_MS})")), "duree : {sql}");
            assert!(
                !sql.contains("SUM(t.duration_ms)"),
                "`SUM(text)` n'existe pas sur PostgreSQL, et `pg_migrate` pose \
                 encore `duration_ms` en TEXT : la duree reviendrait a zero \
                 sans un mot. SQL :\n{sql}"
            );
            assert!(
                !sql.contains("LIMIT"),
                "un LIMIT dans le decompte annoncerait la page au lieu de la \
                 fenetre — « 20 albums » sur une fenetre qui en porte 300. \
                 SQL :\n{sql}"
            );
        }
    }
}
