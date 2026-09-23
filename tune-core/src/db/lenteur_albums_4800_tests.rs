//! #4800, tranche 1 — les DEUX causes serveur de la lenteur de la grille
//! d'albums mesurées sur le .18, et leurs preuves.
//!
//! * Cause 1 : le prédicat de doublon #4146 (`LOWER(loc.title) =
//!   LOWER(a.title)`) rendait `idx_albums_title` inutilisable — 9 427 × 9 427
//!   comparaisons, deux fois par page. Preuve : sur un banc de milliers
//!   d'albums locaux et distants homonymes, la liste et le total rendus par le
//!   dépôt sont IDENTIQUES à ceux de l'ANCIENNE clause, recopiée ici au
//!   caractère près. Le banc `#[ignore]` mesure les deux sur une base réelle.
//! * Cause 2 : le pool de lecture attribuait sa connexion par compteur
//!   tournant, occupée ou non. Preuve : trois lectures dont une lente, les
//!   deux autres ne l'attendent plus.
//!
//! Les trois premiers témoins ne dépendent que d'API STABLES du dépôt
//! (`count_visible`, `list_filtered_seeded`, `query_one`) : ils compilent
//! AVANT le correctif — c'est ce qui permet la contre-épreuve (rouge avant,
//! vert après) et la mesure avant/après sur la même base. Les suivants
//! tiennent au correctif lui-même : le plan d'exécution, le total compté
//! dans la même requête que la page.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::album_repo::AlbumRepo;
use super::backend::{DbBackend, ToSqlValue};
use super::engine::Engine;
use super::facet_filter::{
    copie_de_moindre_qualite_exclue, hidden_albums_excluded, hidden_tracks_excluded,
};
use super::sqlite::SqliteDb;
use super::track_repo::TrackRepo;

/// L'ANCIEN corps du rapprochement #4146 (`facet_filter::condition_de_doublon`
/// avant #4800), recopié au caractère près : c'est la RÉFÉRENCE de ce que la
/// grille masquait. Toute réécriture doit rendre exactement le même ensemble.
fn ancienne_condition_de_doublon(alias: &str) -> String {
    let nom_distant = format!(
        "COALESCE((SELECT ar_dist.name FROM artists ar_dist WHERE ar_dist.id = {alias}.artist_id), '')"
    );
    let nom_local =
        "COALESCE((SELECT ar_loc.name FROM artists ar_loc WHERE ar_loc.id = loc.artist_id), '')";
    format!(
        "COALESCE(NULLIF({alias}.source, ''), 'local') <> 'local' \
         AND COALESCE(NULLIF(loc.source, ''), 'local') = 'local' \
         AND LOWER(loc.title) = LOWER({alias}.title) \
         AND (LOWER({nom_local}) = LOWER({nom_distant}) \
              OR ({nom_distant} = '' \
                  AND (SELECT COUNT(*) FROM albums amb \
                       WHERE LOWER(amb.title) = LOWER({alias}.title) \
                       AND COALESCE(NULLIF(amb.source, ''), 'local') = 'local') = 1))"
    )
}

/// L'ancien `album_distant_double_exclu("a")`.
fn ancienne_exclusion_albums() -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM albums loc WHERE {})",
        ancienne_condition_de_doublon("a")
    )
}

/// L'ancien `pistes_album_distant_double_exclu()`.
fn ancienne_exclusion_pistes() -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM albums dist WHERE dist.id = t.album_id \
         AND EXISTS (SELECT 1 FROM albums loc WHERE {}))",
        ancienne_condition_de_doublon("dist")
    )
}

fn ids(db: &dyn DbBackend, sql: &str, params: &[&dyn ToSqlValue]) -> Vec<i64> {
    db.query_many(sql, params)
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

fn un_entier(db: &dyn DbBackend, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
}

/// Le banc : des MILLIERS d'albums locaux et distants, dans toutes les
/// situations que la règle #4146 distingue.
///
/// * 1 000 locaux, 100 artistes, titres `Album n` ;
/// * 700 distants qui doublent un local — même artiste, titre en
///   MAJUSCULES pour que la casse compte (masqués) ;
/// * 200 distants homonymes d'un AUTRE artiste (visibles) ;
/// * 150 distants SANS artiste sur un titre local non ambigu (masqués) ;
/// * 50 paires locales de même titre `Live n` chez deux artistes, et un
///   distant sans artiste sur chacune (ambigu : visibles) ;
/// * 300 distants sans aucune contrepartie (visibles) ;
/// * 40 albums masqués à la main (`hidden_items`), pris parmi les locaux ;
/// * une piste sur un album sur deux, pour que le tri par date d'ajout et le
///   compteur de pistes aient matière.
///
/// 2 500 albums : l'ANCIENNE clause y coûte déjà des secondes (elle est en
/// n²), c'est le prix de la comparaison — pas plus, pour que la CI reste
/// courte.
fn banc_d_albums_homonymes() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let mut sql = String::from("BEGIN;\n");
    for a in 0..100 {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({}, 'Artiste {a}');\n",
            a + 1
        ));
    }
    sql.push_str("INSERT INTO artists (id, name) VALUES (9001, 'Autre Artiste'), (9002, 'Second Artiste');\n");
    let mut id = 1_000;
    let mut album = |sql: &mut String, titre: &str, artiste: Option<i64>, source: &str| -> i64 {
        id += 1;
        let artiste = artiste.map_or("NULL".to_string(), |a| a.to_string());
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({id}, '{titre}', {artiste}, '{source}');\n"
        ));
        id
    };
    for i in 0..1_000 {
        album(&mut sql, &format!("Album {i}"), Some(i % 100 + 1), "local");
    }
    for i in 0..700 {
        album(&mut sql, &format!("ALBUM {i}"), Some(i % 100 + 1), "upnp");
    }
    for i in 0..200 {
        album(&mut sql, &format!("Album {i}"), Some(9001), "upnp");
    }
    for i in 800..950 {
        album(&mut sql, &format!("album {i}"), None, "upnp");
    }
    for j in 0..50 {
        album(&mut sql, &format!("Live {j}"), Some(9001), "local");
        album(&mut sql, &format!("Live {j}"), Some(9002), "local");
        album(&mut sql, &format!("live {j}"), None, "upnp");
    }
    for i in 0..300 {
        album(&mut sql, &format!("Distant {i}"), Some(i % 100 + 1), "upnp");
    }
    let dernier = id;
    for (n, id) in (1_001..=dernier).enumerate() {
        if n % 2 == 0 {
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, album_id, file_path, source) VALUES ({}, 'Piste', {id}, '/banc/{id}.flac', 'local');\n",
                100_000 + n
            ));
        }
    }
    // Les masqués à la main sont pris parmi les 1 000 locaux — tous visibles
    // par ailleurs — pour que leur retrait se compte simplement.
    for id in (1_001..=2_000).step_by(20).take(40) {
        sql.push_str(&format!(
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {id});\n"
        ));
    }
    sql.push_str("COMMIT;");
    db.execute_batch(&sql).unwrap();
    db
}

/// Preuve (a) : même exclusion des doublons, même ordre, même total qu'avec
/// l'ancienne clause — sur toute la liste et sur une page du milieu.
#[test]
fn la_liste_et_le_total_sont_ceux_de_l_ancienne_clause() {
    let db = banc_d_albums_homonymes();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    let repo = AlbumRepo::new(db.clone());

    let total_avant = un_entier(
        &db,
        &format!(
            "SELECT COUNT(*) FROM albums a WHERE {} AND {}",
            hidden_albums_excluded(),
            ancienne_exclusion_albums()
        ),
    );
    let total_apres = repo.count_visible().unwrap();
    // Le banc a de la matière : des masqués ET des visibles de chaque sorte.
    let tous = un_entier(&db, "SELECT COUNT(*) FROM albums");
    assert_eq!(tous, 2_500, "taille du banc");
    assert_eq!(
        total_avant,
        1_000 + 100 + 200 + 50 + 300 - 40,
        "l'ancienne clause masque 700 + 150 doublons et 40 masqués à la main ; \
         si ce nombre bouge, c'est le BANC qui a changé, pas la règle"
    );
    assert_eq!(total_apres, total_avant, "count_visible ≠ ancienne clause");

    let liste_avant = |limit: i64, offset: i64| {
        ids(
            backend.as_ref(),
            &format!(
                "SELECT a.id FROM albums a WHERE {} AND {} \
                 ORDER BY LOWER(a.title) ASC, a.id ASC LIMIT ? OFFSET ?",
                hidden_albums_excluded(),
                ancienne_exclusion_albums()
            ),
            &[&limit, &offset],
        )
    };
    let liste_apres = |limit: i64, offset: i64| -> Vec<i64> {
        repo.list_filtered_seeded(
            limit, offset, "title", "asc", None, None, None, false, None, None,
        )
        .unwrap()
        .iter()
        .filter_map(|a| a.id)
        .collect()
    };
    let entiere = liste_apres(100_000, 0);
    assert_eq!(
        entiere.len() as i64,
        total_apres,
        "la liste entière a la taille du total"
    );
    assert_eq!(
        entiere,
        liste_avant(100_000, 0),
        "liste entière ≠ ancienne clause"
    );
    assert_eq!(
        liste_apres(100, 800),
        liste_avant(100, 800),
        "page du milieu ≠ ancienne clause"
    );
    assert_eq!(
        liste_apres(100, 1_550),
        liste_avant(100, 1_550),
        "dernière page, incomplète ≠ ancienne clause"
    );

    // Les pistes suivent leur album : même règle, même résultat.
    let pistes_avant = un_entier(
        &db,
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {} AND {} AND {}",
            hidden_tracks_excluded(),
            ancienne_exclusion_pistes(),
            copie_de_moindre_qualite_exclue()
        ),
    );
    let pistes_apres = TrackRepo::new(db.clone()).count_visible().unwrap();
    assert!(
        pistes_avant > 500 && pistes_avant < 1_250,
        "matière : {pistes_avant}"
    );
    assert_eq!(
        pistes_apres, pistes_avant,
        "pistes visibles ≠ ancienne clause"
    );
}

/// Le plan d'exécution SQLite, une ligne par étape.
fn plan(db: &dyn DbBackend, sql: &str) -> String {
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
        .unwrap()
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| {
                    v.as_string()
                        .unwrap_or_else(|| v.as_i64().map_or(String::new(), |n| n.to_string()))
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// La cause 1, lue dans le PLAN : l'ancienne clause parcourait `albums loc`
/// en entier pour chaque ligne ; la nouvelle la cherche par
/// `idx_albums_title`. Ce n'est pas une garde de texte sur le SQL — c'est
/// le moteur qui dit ce qu'il fera.
#[test]
fn la_sous_requete_de_doublon_cherche_par_l_index_des_titres() {
    let db = banc_d_albums_homonymes();
    let ancien = plan(
        &db,
        &format!(
            "SELECT COUNT(*) FROM albums a WHERE {} AND {}",
            hidden_albums_excluded(),
            ancienne_exclusion_albums()
        ),
    );
    let nouveau = plan(&db, &super::album_repo::sql::count_visible(Engine::Sqlite));
    assert!(
        ancien.contains("SCAN loc"),
        "l'ancienne clause doit parcourir `loc` (sinon le banc ne reproduit pas le .18) :\n{ancien}"
    );
    assert!(
        nouveau.contains("SEARCH loc USING") && nouveau.contains("idx_albums_title"),
        "la nouvelle clause doit chercher `loc` par idx_albums_title :\n{nouveau}"
    );
    assert!(
        !nouveau.contains("SCAN loc"),
        "plus aucun parcours complet de `loc` :\n{nouveau}"
    );
    // Le texte PostgreSQL, lui, garde `LOWER` : pas de `COLLATE NOCASE` là-bas.
    let pg = super::album_repo::sql::count_visible(Engine::Postgres);
    assert!(pg.contains("LOWER(loc.title) = LOWER(a.title)") && !pg.contains("NOCASE"));
}

/// #4800 — le total compté dans la MÊME requête que la page vaut
/// `count_visible`, page pleine ou incomplète, tri aléatoire compris ; la
/// page elle-même ne change pas ; au-delà de la fin, pas de total.
#[test]
fn le_total_de_la_page_est_celui_du_compteur() {
    let db = banc_d_albums_homonymes();
    let repo = AlbumRepo::new(db.clone());
    let attendu = repo.count_visible().unwrap();
    let ids_de = |albums: &[super::models::Album]| -> Vec<i64> {
        albums.iter().filter_map(|a| a.id).collect()
    };
    for (limit, offset, sort, seed) in [
        (100, 0, "added_at", None),
        (100, 800, "title", None),
        (100, 1_550, "title", None),
        (2_000, 0, "added_at", None),
        (50, 0, "random", Some(42)),
    ] {
        let (page, total) = repo
            .list_filtered_seeded_avec_total(
                limit, offset, sort, "asc", None, None, None, false, None, seed,
            )
            .unwrap();
        assert_eq!(
            total,
            Some(attendu),
            "limit {limit} offset {offset} sort {sort}"
        );
        let sans_total = repo
            .list_filtered_seeded(
                limit, offset, sort, "asc", None, None, None, false, None, seed,
            )
            .unwrap();
        assert_eq!(
            ids_de(&page),
            ids_de(&sans_total),
            "la page ne bouge pas avec le total"
        );
    }
    let (page, total) = repo
        .list_filtered_seeded_avec_total(
            100, 10_000, "title", "asc", None, None, None, false, None, None,
        )
        .unwrap();
    assert!(page.is_empty());
    assert_eq!(total, None, "une page vide ne porte pas de total");
}

/// Preuve (c) : trois lectures dont une LENTE — les deux autres ne doivent
/// plus attendre derrière elle.
///
/// Avant #4800, la connexion était attribuée par compteur tournant : une
/// lecture sur trois tombait sur la connexion occupée et attendait la fin de
/// la lente. Ici deux fils enchaînent trente lectures d'une milliseconde
/// pendant qu'un troisième en tient une de plusieurs secondes : chacun doit
/// avoir fini bien avant elle. Base sur FICHIER, car en mémoire les trois
/// lecteurs sont une seule connexion.
#[test]
fn une_lecture_lente_ne_retient_plus_les_autres() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("pool.db");
    let db = Arc::new(SqliteDb::open(chemin.to_str().unwrap()).unwrap());
    db.init_schema().unwrap();

    // ~1 à 3 s selon la machine : assez pour que « attendre derrière » se voie.
    const LENTE: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 6000000) \
                         SELECT COUNT(*) FROM c";
    let depart = Arc::new(std::sync::Barrier::new(3));
    let lente = {
        let (db, depart) = (db.clone(), depart.clone());
        std::thread::spawn(move || {
            depart.wait();
            let debut = Instant::now();
            db.query_one(LENTE, &[]).unwrap();
            debut.elapsed()
        })
    };
    let rapides: Vec<_> = (0..2)
        .map(|_| {
            let (db, depart) = (db.clone(), depart.clone());
            std::thread::spawn(move || {
                depart.wait();
                // Laisser la lente prendre sa connexion d'abord.
                std::thread::sleep(Duration::from_millis(100));
                let debut = Instant::now();
                let mut pire = Duration::ZERO;
                for _ in 0..30 {
                    let une = Instant::now();
                    db.query_one("SELECT 1", &[]).unwrap();
                    pire = pire.max(une.elapsed());
                }
                (debut.elapsed(), pire)
            })
        })
        .collect();
    let duree_lente = lente.join().unwrap();
    assert!(
        duree_lente > Duration::from_millis(300),
        "la lecture lente ne l'est pas ({duree_lente:?}) : le test ne prouverait rien"
    );
    for (n, rapide) in rapides.into_iter().enumerate() {
        let (total, pire) = rapide.join().unwrap();
        assert!(
            total < duree_lente / 2,
            "lecteur rapide {n} : {total:?} pour trente lectures (pire : {pire:?}) alors que la lente a duré {duree_lente:?} — il a attendu derrière elle"
        );
    }
}

/// Preuve (b) — le banc sur une base RÉELLE (`TUNE_BANC_DB`, copie de
/// `tune_v2.db` du .18 : 9 427 albums dont 5 489 distants). Ignoré par
/// défaut ; chiffres dans la PR #4800.
///
/// ```text
/// TUNE_BANC_DB=/chemin/tune_v2.db cargo test -p tune-core --lib \
///     banc_reel_4800 -- --ignored --nocapture
/// ```
#[test]
#[ignore = "mesure sur une base réelle, chemin dans TUNE_BANC_DB"]
fn banc_reel_4800() {
    let Ok(chemin) = std::env::var("TUNE_BANC_DB") else {
        eprintln!("TUNE_BANC_DB absent : rien à mesurer");
        return;
    };
    let db = SqliteDb::open(&chemin).unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let repo = AlbumRepo::new(db.clone());
    let par_source = db
        .query_many(
            "SELECT COALESCE(NULLIF(source, ''), 'local'), COUNT(*) FROM albums GROUP BY 1",
            &[],
        )
        .unwrap();
    eprintln!("albums par source : {par_source:?}");

    let chrono = |nom: &str, f: &dyn Fn() -> i64| {
        for _ in 0..3 {
            let debut = Instant::now();
            let n = f();
            eprintln!(
                "{nom:<44} {:>9.1} ms  (= {n})",
                debut.elapsed().as_secs_f64() * 1e3
            );
        }
    };

    let ancien_count = format!(
        "SELECT COUNT(*) FROM albums a WHERE {} AND {}",
        hidden_albums_excluded(),
        ancienne_exclusion_albums()
    );
    eprintln!(
        "--- plan, ANCIENNE clause (COUNT) :\n  {}",
        plan(&ancien_count)
    );
    chrono("COUNT ancienne clause", &|| un_entier(&db, &ancien_count));
    chrono("repo.count_visible()", &|| repo.count_visible().unwrap());
    chrono("repo.list_filtered_seeded(100, 0, added_at)", &|| {
        repo.list_filtered_seeded(
            100, 0, "added_at", "asc", None, None, None, false, None, None,
        )
        .unwrap()
        .len() as i64
    });
    chrono("repo.list_filtered_seeded(2000, 0, added_at)", &|| {
        repo.list_filtered_seeded(
            2000, 0, "added_at", "asc", None, None, None, false, None, None,
        )
        .unwrap()
        .len() as i64
    });
    chrono("repo.list_filtered_seeded(100, 0, title)", &|| {
        repo.list_filtered_seeded(100, 0, "title", "asc", None, None, None, false, None, None)
            .unwrap()
            .len() as i64
    });
}
