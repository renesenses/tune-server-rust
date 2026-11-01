//! #7619 (L3) — témoins de la recherche `/search` (et `/library/search`) en
//! DÉBUT DE MOT, décision de Bertrand du 01/11/2026 : « la recherche trouve
//! par début de mot (préfixe), pas par trigrammes ».
//!
//! Témoins de RÉSULTAT : chaque mot saisi est le début d'un mot — titre,
//! artiste, genre, compositeur de la piste ; titre, artiste, genre de
//! l'album, ou titre de l'une de ses pistes ; nom de l'artiste ; mot d'un
//! label —, sans accents ni casse. Le milieu d'un mot ne trouve plus rien :
//! « eat » ne trouve plus « The Beatles ».
//!
//! Témoin de PLAN : ni la recherche de pistes ni celle d'albums ne parcourt la
//! table des pistes ; tout passe par l'index plein texte, la piste par sa clé.

use std::sync::Arc;

use super::album_repo::AlbumRepo;
use super::artist_repo::ArtistRepo;
use super::backend::DbBackend;
use super::engine::{Engine, SqliteDialect};
use super::sqlite::SqliteDb;
use super::track_repo::{TrackRepo, sql};

/// Le jeu des témoins, une instruction par entrée : les deux moteurs le
/// rejouent tel quel (identifiants explicites, aucun dialecte).
const DONNEES: [&str; 4] = [
    "INSERT INTO artists (id, name) VALUES (1, 'The Beatles'), (2, 'Beyoncé'), (3, 'ÉBÈNE QUARTET'), (4, 'Ólafur Arnalds'), (5, 'Miles Davis'), (6, 'abeat collective')",
    "INSERT INTO albums (id, title, artist_id, year, label) VALUES (1, 'Abbey Road', 1, 1969, 'Parlophone'), (2, 'Lemonade', 2, 2016, 'Columbia'), (3, 'Fiction', 3, 2010, 'Erato'), (4, 'Re:member', 4, 2018, 'Mercury KX'), (5, 'Kind of Blue', 5, 1959, 'Sony Columbia'), (6, 'Caché', 6, 1975, 'Nicolas Records'), (7, 'Album masqué', 1, 1975, NULL)",
    "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 7)",
    "INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path) VALUES (1, 'Come Together', 1, 1, 'Rock', 'Lennon', '/m/1.flac'), (2, 'Something', 1, 1, 'rock', NULL, '/m/2.flac'), (3, 'Formation', 2, 2, 'R&B', 'Beyoncé Knowles', '/m/3.flac'), (4, 'Libertango', 3, 3, 'Jazz', 'Piazzolla', '/m/4.flac'), (5, 'Saman', 4, 4, 'Électronique', 'Ólafur', '/m/5.flac'), (6, 'So What', 5, 5, 'JAZZ', 'Davis', '/m/6.flac'), (7, 'Beat It', 6, 6, NULL, 'Dvořák', '/m/7.flac'), (8, 'Orpheline', NULL, NULL, 'electro', NULL, '/m/8.flac'), (9, 'Cachée', 7, 1, 'Rock', 'Lennon', '/m/9.flac'), (10, 'Sans rien', NULL, NULL, NULL, NULL, '/m/10.flac')",
];

/// Pose [`DONNEES`] dans `db`.
pub(crate) fn peupler(db: &dyn DbBackend) {
    for instruction in DONNEES {
        db.execute(instruction, &[]).unwrap();
    }
}

fn base() -> (Arc<SqliteDb>, TrackRepo) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    peupler(&db);
    let db = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    (db, repo)
}

fn pistes(repo: &TrackRepo, q: &str) -> Vec<i64> {
    repo.search_page(q, 100, 0)
        .unwrap()
        .iter()
        .map(|t| t.id.unwrap())
        .collect()
}

/// Les pistes : chaque mot saisi est le début d'un mot de leur titre, de leur
/// artiste, de leur genre ou de leur compositeur — jamais le milieu d'un mot.
pub(crate) fn scenario_pistes_7619(db: Arc<dyn DbBackend>) {
    let repo = TrackRepo::with_backend(db.clone());
    let ids = |q: &str| pistes(&repo, q);
    // Début de mot : trouvé, accents et casse ignorés.
    assert!(ids("beat").contains(&1), "« beat » : The Beatles");
    assert!(ids("beat").contains(&7), "« beat » : Beat It");
    assert!(ids("BEYONCE").contains(&3), "accents de l'artiste");
    assert!(ids("ebene").contains(&4), "accents de l'artiste");
    assert!(ids("electro").contains(&5), "accents du genre");
    assert!(ids("electro").contains(&8), "genre");
    assert!(ids("élect").contains(&5), "accents dans la saisie");
    assert!(ids("dvorak").contains(&7), "accents du compositeur");
    assert!(ids("piazz").contains(&4), "compositeur");
    assert!(ids("knowl").contains(&3), "second mot du compositeur");
    assert!(ids("1975").contains(&7), "année de l'album");
    assert!(!ids("1975").contains(&9), "album masqué exclu");
    // Chaque mot est un début de mot, pas seulement le dernier.
    assert_eq!(ids("mil dav"), vec![6], "« mil dav » : Miles Davis");
    assert_eq!(ids("so wh"), vec![6]);
    // Le milieu d'un mot ne trouve plus rien (décision du 01/11/2026).
    for q in [
        "eat", "eatles", "orak", "ectroniq", "eyonce", "iles", "ogether",
    ] {
        assert!(
            ids(q).is_empty(),
            "« {q} » : milieu de mot, trouvé {:?}",
            ids(q)
        );
    }
    // Rien d'utilisable dans la saisie : rien, sans erreur.
    assert!(ids("!!!").is_empty());
    assert_eq!(repo.search_count("!!!", 10).unwrap(), 0);
    assert_eq!(repo.search_page_et_total("!!!", 10, 0, 10).unwrap().1, 0);
}

/// La page lue avec son total ([`TrackRepo::search_page_et_total`]) vaut la
/// page et le compte lus séparément, plafond et page au-delà de la fin compris.
#[test]
fn la_page_et_son_total_valent_la_page_et_le_compte() {
    let (_db, repo) = base();
    let mut non_vides = 0;
    for q in [
        "beat",
        "rock",
        "jazz",
        "e",
        "electro",
        "1975",
        "\"come together\"",
        "zzqx",
        "mil dav",
    ] {
        let tous = pistes(&repo, q);
        let (p, total) = repo.search_page_et_total(q, 1000, 0, 5000).unwrap();
        assert_eq!(
            p.iter().map(|t| t.id.unwrap()).collect::<Vec<_>>(),
            tous,
            "« {q} »"
        );
        assert_eq!(total, tous.len() as i64, "« {q} » : total");
        assert_eq!(
            repo.search_count(q, 5000).unwrap(),
            total,
            "« {q} » : compte"
        );
        for offset in [0_i64, 2, 50] {
            let (p, t) = repo.search_page_et_total(q, 2, offset, 3).unwrap();
            let attendu: Vec<i64> = tous.iter().skip(offset as usize).take(2).copied().collect();
            assert_eq!(p.iter().map(|t| t.id.unwrap()).collect::<Vec<_>>(), attendu);
            assert_eq!(t, total.min(3), "« {q} » offset {offset} : total borné");
        }
        if !tous.is_empty() {
            non_vides += 1;
        }
    }
    assert!(non_vides >= 7, "{non_vides} requêtes non vides seulement");
}

/// Les albums, les artistes et les labels : même règle.
pub(crate) fn scenario_albums_artistes_labels_7619(db: Arc<dyn DbBackend>) {
    let albums = AlbumRepo::with_backend(db.clone());
    let artistes = ArtistRepo::with_backend(db.clone());
    let al = |q: &str| -> Vec<i64> {
        let v: Vec<i64> = albums
            .search_page(q, 100, 0)
            .unwrap()
            .iter()
            .map(|a| a.id.unwrap())
            .collect();
        assert_eq!(
            albums.search_count(q, 1000).unwrap(),
            v.len() as i64,
            "« {q} »"
        );
        v
    };
    let ar = |q: &str| -> Vec<i64> {
        let v: Vec<i64> = artistes
            .search_page(q, 100, 0)
            .unwrap()
            .iter()
            .map(|a| a.id.unwrap())
            .collect();
        assert_eq!(
            artistes.search_count(q, 1000).unwrap(),
            v.len() as i64,
            "« {q} »"
        );
        v
    };
    let labels = |q: &str| -> Vec<String> {
        albums
            .search_labels(q, 100)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    };
    // Albums : titre, artiste, titre de l'une de ses pistes.
    assert_eq!(al("lemon"), vec![2]);
    assert_eq!(al("kin blu"), vec![5]);
    assert_eq!(al("cache"), vec![6], "accents du titre, album masqué exclu");
    assert!(al("beyon").contains(&2), "artiste de l'album");
    assert_eq!(al("togeth"), vec![1], "titre d'une piste");
    assert_eq!(al("libert"), vec![3], "titre d'une piste");
    for q in ["emonade", "lue", "ogether", "ibertango", "eyonce"] {
        assert!(al(q).is_empty(), "albums « {q} » : milieu de mot");
    }
    // Artistes.
    assert_eq!(ar("beat"), vec![1]);
    assert_eq!(ar("ebene"), vec![3]);
    assert_eq!(ar("quart"), vec![3]);
    assert_eq!(ar("olaf"), vec![4]);
    for q in ["eatles", "bene", "uartet", "beat collective x"] {
        assert!(ar(q).is_empty(), "artistes « {q} » : milieu de mot");
    }
    assert!(ar("!!!").is_empty());
    // Labels : début du nom ou d'un mot après une espace.
    assert_eq!(labels("col"), vec!["Columbia", "Sony Columbia"]);
    assert_eq!(labels("KX"), vec!["Mercury KX"]);
    assert_eq!(labels("olumbia"), Vec::<String>::new());
    assert_eq!(
        labels("cola"),
        Vec::<String>::new(),
        "Nicolas : milieu de mot"
    );
    assert_eq!(labels("   "), Vec::<String>::new());
}

/// Une ligne de plan qui PARCOURT la table des pistes (et non son index plein
/// texte, `SCAN tracks_fts VIRTUAL TABLE`, qui est la voie voulue).
fn parcourt_les_pistes(ligne: &str) -> bool {
    let l = ligne.trim();
    ["t", "tracks", "dico_t", "ttp"]
        .iter()
        .any(|alias| l == format!("SCAN {alias}") || l.starts_with(&format!("SCAN {alias} ")))
}

/// Le plan : ni la recherche de pistes ni celle d'albums ne parcourt la table
/// des pistes. Les pistes passent par l'index plein texte, puis par leur clé.
#[test]
fn la_recherche_ne_parcourt_plus_la_table_des_pistes() {
    let (db, _repo) = base();
    // Une bibliothèque à l'échelle : 300 artistes et 300 albums de 10 pistes.
    let mut lot = String::from("BEGIN;");
    for a in 100..400 {
        lot.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\
             INSERT INTO albums (id, title, artist_id, year) VALUES ({a}, 'Album {a}', {a}, {});",
            1950 + a % 70
        ));
    }
    for i in 100..3100 {
        let al = 100 + (i - 100) / 10;
        lot.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path) \
             VALUES ({i}, 'Titre {i}', {al}, {al}, 'Genre {}', 'Compositeur {}', '/m/{i}.flac');",
            i % 40,
            i % 300
        ));
    }
    lot.push_str("COMMIT; ANALYZE;");
    db.execute_batch(&lot).unwrap();
    let plan_de = |requete: &str, params: &[&dyn super::backend::ToSqlValue]| -> String {
        db.query_many(&format!("EXPLAIN QUERY PLAN {requete}"), params)
            .unwrap()
            .iter()
            .map(|r| r.get(3).and_then(|v| v.as_string()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fts = crate::db::engine::format_fts_query_piste_ou_chemin(Engine::Sqlite, "beat");
    let fts_id = crate::db::engine::format_fts_query_piste(Engine::Sqlite, "beat");
    let plan = plan_de(
        &sql::search_avec_total(&SqliteDialect),
        &[&fts, &"beat", &fts_id, &"beat", &20_i64, &0_i64],
    );
    assert!(
        plan.contains("SEARCH t USING INTEGER PRIMARY KEY"),
        "la piste n'est pas lue par sa clé :\n{plan}"
    );
    assert!(
        !plan.lines().any(parcourt_les_pistes),
        "la recherche de pistes parcourt la table des pistes :\n{plan}"
    );
    assert!(plan.contains("tracks_fts"), "{plan}");
    let fts_al = crate::db::engine::format_fts_query(Engine::Sqlite, "beat");
    let fts_titre = crate::db::engine::format_fts_query_titre_de_piste(Engine::Sqlite, "beat");
    let plan = plan_de(
        &crate::db::album_repo::sql::search(&SqliteDialect),
        &[
            &fts_al, &"beat", &fts_titre, &"beat%", &"%beat%", &"%beat%", &20_i64, &0_i64,
        ],
    );
    assert!(
        plan.contains("tracks_fts"),
        "le titre de piste passe par l'index :\n{plan}"
    );
    assert!(
        !plan.lines().any(parcourt_les_pistes),
        "la recherche d'albums parcourt la table des pistes :\n{plan}"
    );
}

/// BANC sur une COPIE de base (`TUNE_BANC_DB`) — `#[ignore]`, à la main, en
/// `--release`. La copie est migrée : ne jamais le pointer sur une base de
/// production.
///
/// ```text
/// TUNE_BANC_DB=/tmp/copie.db cargo test --release -p tune-core --lib \
///   banc_recherche_7619_sur_copie -- --ignored --nocapture
/// ```
///
/// Pour chaque requête : pistes (page de 50 et total), albums (page et
/// compte), artistes (page et compte) ; médiane de 5 après une chauffe.
#[test]
#[ignore = "banc manuel sur une copie de base, --release"]
fn banc_recherche_7619_sur_copie() {
    use std::time::Instant;
    let Ok(chemin) = std::env::var("TUNE_BANC_DB") else {
        eprintln!("TUNE_BANC_DB absent : banc sauté");
        return;
    };
    let db = SqliteDb::open(&chemin).expect("copie de base");
    // `TUNE_BANC_INIT=1` : base NEUVE (synthétique), schéma posé puis arrêt.
    if std::env::var("TUNE_BANC_INIT").is_ok() {
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        return;
    }
    crate::db::migrations::run_migrations(&db).unwrap();
    let db = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    let albums = AlbumRepo::with_backend(db.clone());
    let artistes = ArtistRepo::with_backend(db.clone());
    let requetes = std::env::var("TUNE_BANC_REQUETES")
        .unwrap_or_else(|_| "beat,bach,miles davis,jazz,1975,beyonce,mozart,é,zzqx".into());
    let mediane = |f: &mut dyn FnMut()| {
        f();
        let mut d: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        d[2]
    };
    for q in requetes.split(',') {
        let mut n_pistes = 0;
        let t_pistes = mediane(&mut || {
            n_pistes = repo.search_page_et_total(q, 50, 0, 5000).unwrap().1;
        });
        let mut n_albums = 0;
        let t_albums = mediane(&mut || {
            albums.search_page(q, 50, 0).unwrap();
            n_albums = albums.search_count(q, 5000).unwrap();
        });
        let mut n_artistes = 0;
        let t_artistes = mediane(&mut || {
            artistes.search_page(q, 50, 0).unwrap();
            n_artistes = artistes.search_count(q, 5000).unwrap();
        });
        // Le texte libre d'Oxygen (`/library/tracks?q=…&limit=200`).
        let mut n_oxygen = 0;
        let t_oxygen = mediane(&mut || {
            let f = crate::db::facet_filter::TrackFilter {
                q: Some(q.into()),
                ..Default::default()
            };
            n_oxygen = repo.list_filtered(&f, 200, 0).unwrap().1;
        });
        eprintln!("BANC oxygen    {q:<12} {t_oxygen:>6.1} ms ({n_oxygen})");
        eprintln!(
            "BANC recherche {q:<12} pistes={t_pistes:>6.1} ms ({n_pistes})  \
             albums={t_albums:>6.1} ms ({n_albums})  artistes={t_artistes:>6.1} ms ({n_artistes})"
        );
    }
}

/// Le texte libre d'Oxygen (`/library/tracks?q=`) : début de mot aussi
/// (décision du 01/11/2026), sur chaque champ — titre, artiste, album, label,
/// chemin, compositeur, chef… —, la ponctuation séparant les mots.
pub(crate) fn scenario_texte_libre_7619(db: Arc<dyn DbBackend>) {
    use crate::db::facet_filter::{Placeholders, condition_texte_libre};
    for instruction in [
        "UPDATE tracks SET label = 'Parlophone' WHERE id IN (1, 2)",
        "UPDATE tracks SET label = 'ÉCM Records' WHERE id = 4",
        "UPDATE tracks SET label = 'columbia' WHERE id = 6",
    ] {
        db.execute(instruction, &[]).unwrap();
    }
    let ids = |q: &str| -> Vec<i64> {
        let mut ph = Placeholders::resuming_at(db.engine(), 1);
        let (cond, valeurs) = condition_texte_libre(&mut ph, q);
        let refs: Vec<&dyn super::backend::ToSqlValue> = valeurs
            .iter()
            .map(|v| v as &dyn super::backend::ToSqlValue)
            .collect();
        db.query_many(
            &format!("SELECT t.id FROM tracks t WHERE {cond} ORDER BY t.id"),
            &refs,
        )
        .unwrap()
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect()
    };
    assert_eq!(ids("parlo"), vec![1, 2], "label");
    assert_eq!(ids("ecm"), vec![4], "accents du label");
    assert_eq!(ids("RECORDS"), vec![4], "second mot du label");
    assert_eq!(ids("COLUMB"), vec![6]);
    assert_eq!(ids("dvorak"), vec![7], "accents du compositeur");
    assert_eq!(ids("come tog"), vec![1], "chaque mot, début de mot");
    assert_eq!(ids("member"), vec![5], "« Re:member » : « : » sépare");
    assert!(ids("beat").contains(&7) && ids("beat").contains(&1));
    for q in ["arlo", "ecords", "olumb", "orak", "eat", "ogether", "50%"] {
        assert!(
            ids(q).is_empty(),
            "« {q} » : milieu de mot, trouvé {:?}",
            ids(q)
        );
    }
}

#[test]
fn les_pistes_se_trouvent_par_debut_de_mot() {
    let (db, _repo) = base();
    scenario_pistes_7619(db);
}

#[test]
fn albums_artistes_et_labels_se_trouvent_par_debut_de_mot() {
    let (db, _repo) = base();
    scenario_albums_artistes_labels_7619(db);
}

#[test]
fn le_texte_libre_trouve_par_debut_de_mot() {
    let (db, _repo) = base();
    scenario_texte_libre_7619(db);
}
