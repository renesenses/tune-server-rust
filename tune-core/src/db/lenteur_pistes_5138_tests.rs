//! #5138 — la liste des PISTES et son compteur : 7 à 9,7 s CHACUN sur les
//! 34 091 pistes de JeromeQ (0.9.165, Linux, SQLite), deux requêtes par page,
//! dix-huit pages pour charger la vue Oxygen.
//!
//! * La cause, lue dans le plan : le `NOT EXISTS` corrélé de
//!   [`copie_de_moindre_qualite_exclue`] cherchait `mieux` par
//!   `idx_tracks_album_id` seul — il relisait, pour CHAQUE piste, toutes les
//!   pistes de son album dans la table. L'index de la clé de copie
//!   (`idx_tracks_cle_de_copie`, passe rejouée au démarrage) le fait
//!   chercher par la clé entière.
//! * La page et son total sortent d'une seule évaluation du `WHERE`
//!   ([`TrackRepo::list_visible_avec_total`]).
//!
//! Les témoins comparent au SQL de la 0.9.165, recopié ici au caractère
//! près, joué sur la même base PRIVÉE de l'index : c'est l'état d'avant.

use std::time::{Duration, Instant};

use super::backend::DbBackend;
use super::engine::Engine;
use super::facet_filter::{
    copie_de_moindre_qualite_exclue, hidden_tracks_excluded, pistes_album_distant_double_exclu,
};
use super::sqlite::SqliteDb;
use super::track_repo::TrackRepo;

/// Un banc au profil d'une bibliothèque réelle, à l'échelle `locaux` :
///
/// * `locaux` albums locaux de 19 pistes FLAC 16 bits, `locaux / 3,2`
///   artistes (530 pour 1 700) ;
/// * 30 albums distants (`upnp`) qui DOUBLENT un local — titre en
///   majuscules, même artiste : leurs pistes sortent — et 22 sans
///   contrepartie : elles restent ;
/// * un album sur 40 porte en plus une copie MP3 de chaque piste (sort) ;
/// * un sur 97, une seconde copie FLAC de même qualité de sa piste 1 (la
///   plus RÉCENTE sort : départage par l'identifiant) ;
/// * un sur 113, une copie 24 bits de sa piste 2 ajoutée APRÈS (c'est
///   l'ORIGINAL 16 bits qui sort) ;
/// * 60 pistes sans album (restent) ; 8 albums masqués (leurs pistes sortent).
///
/// `locaux = 1 700` donne 1 752 albums, 530 artistes et ≈ 34 000 pistes :
/// le profil de JeromeQ.
pub(super) fn remplir_banc(db: &SqliteDb, locaux: i64) -> BancDePistes {
    assert!(locaux >= 100, "le banc suppose au moins 100 albums locaux");
    let artistes = (locaux * 10 / 32).max(10);
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=artistes {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let artiste_de = |al: i64| al % artistes + 1;
    for al in 1..=locaux {
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, 'Album {al}', {}, 'local');\n",
            artiste_de(al)
        ));
    }
    let mut distants_doubles = Vec::new();
    for k in 1..=52 {
        let al = locaux + k;
        let (titre, artiste) = if k <= 30 {
            distants_doubles.push(al);
            (format!("ALBUM {k}"), artiste_de(k))
        } else {
            (format!("Distant {al}"), artiste_de(al))
        };
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, '{titre}', {artiste}, 'upnp');\n"
        ));
    }
    let mut id = 0_i64;
    let mut piste = |sql: &mut String,
                     album: Option<i64>,
                     artiste: i64,
                     n: i64,
                     format: &str,
                     bits: i64|
     -> i64 {
        id += 1;
        let source = match album {
            Some(al) if al > locaux => "upnp",
            _ => "local",
        };
        let album = album.map_or("NULL".to_string(), |a| a.to_string());
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, album_artist) \
             VALUES ({id}, 'Piste {n}', {album}, {artiste}, 1, {n}, '/banc/{id}.{format}', \
             '{format}', 44100, {bits}, '{source}', '');\n"
        ));
        id
    };
    let mut ecartees = Vec::new();
    for al in 1..=locaux + 52 {
        for n in 1..=19 {
            piste(&mut sql, Some(al), artiste_de(al), n, "flac", 16);
        }
    }
    for al in (1..=locaux).step_by(40) {
        for n in 1..=19 {
            ecartees.push(piste(&mut sql, Some(al), artiste_de(al), n, "mp3", 16));
        }
    }
    for al in (3..=locaux).step_by(97) {
        ecartees.push(piste(&mut sql, Some(al), artiste_de(al), 1, "flac", 16));
    }
    let mut meilleures = Vec::new();
    for al in (7..=locaux).step_by(113) {
        // L'original 16 bits est la piste 2 de l'album : id (al-1)×19 + 2.
        ecartees.push((al - 1) * 19 + 2);
        meilleures.push(piste(&mut sql, Some(al), artiste_de(al), 2, "flac", 24));
    }
    let mut sans_album = Vec::new();
    for n in 1..=60 {
        sans_album.push(piste(&mut sql, None, n % artistes + 1, n, "flac", 16));
    }
    // Un album de 1 000 pistes SANS numéro (des WAV sans étiquettes rangés
    // par dossier) : le cas où la recherche de copie par album seul devient
    // quadratique — 1 000 pistes relues pour chacune des 1 000.
    let gros = locaux + 53;
    sql.push_str(&format!(
        "INSERT INTO albums (id, title, artist_id, source) VALUES ({gros}, 'Sans titre', 1, 'local');\n"
    ));
    for n in 1..=1_000 {
        let id = piste(&mut sql, Some(gros), 1, 0, "wav", 16);
        sql.push_str(&format!(
            "UPDATE tracks SET title = 'Morceau {n}' WHERE id = {id};\n"
        ));
    }
    let masques: Vec<i64> = (5..=locaux)
        .step_by((locaux / 8) as usize)
        .take(8)
        .collect();
    for al in &masques {
        sql.push_str(&format!(
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {al});\n"
        ));
    }
    sql.push_str("COMMIT;");
    db.execute_batch(&sql).unwrap();
    BancDePistes {
        masques,
        distants_doubles,
        ecartees,
        meilleures,
        sans_album,
    }
}

/// Ce que le banc a posé, pour que les témoins disent POURQUOI une ligne
/// doit sortir ou rester.
pub(super) struct BancDePistes {
    masques: Vec<i64>,
    distants_doubles: Vec<i64>,
    ecartees: Vec<i64>,
    meilleures: Vec<i64>,
    sans_album: Vec<i64>,
}

fn banc_fichier(dossier: &tempfile::TempDir, locaux: i64) -> (SqliteDb, BancDePistes) {
    let chemin = dossier.path().join("banc-5138.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let banc = remplir_banc(&db, locaux);
    (db, banc)
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
        .join("\n  ")
}

fn un_entier(db: &dyn DbBackend, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
}

/// La liste de la 0.9.165 (`TrackRepo::list_visible` avant #5138), au
/// caractère près, identifiants seuls. Sans `LIMIT` : la liste ENTIÈRE.
///
/// `suffixe` s'ajoute à l'`ORDER BY` : la 0.9.165 ne départageait pas les
/// ex-æquo (le gros album sans numéros n'a que ça), leur ordre n'y était
/// pas défini — d'une page à l'autre, une piste pouvait sauter ou revenir.
/// `", t.id"` rend l'ancien ordre, ex-æquo départagés ; `""` le SQL nu.
fn liste_d_avant(db: &dyn DbBackend, suffixe: &str) -> Vec<i64> {
    let sql = format!(
        "SELECT t.id{} WHERE {} AND {} AND {} ORDER BY LOWER(ar.name), LOWER(al.title), CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER){suffixe}",
        super::track_repo::sql::track_from(),
        hidden_tracks_excluded(),
        pistes_album_distant_double_exclu(Engine::Sqlite),
        copie_de_moindre_qualite_exclue(),
    );
    db.query_many(&sql, &[])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

const INDEX: &str = "idx_tracks_cle_de_copie";

/// La cause, lue dans le PLAN : sans l'index de la clé de copie, `mieux`
/// n'est cherché que par l'album ; avec, par la clé entière. Le compteur
/// comme la liste. Rouge sur la 0.9.165 : l'index n'existe pas.
#[test]
fn la_copie_est_cherchee_par_sa_cle_entiere() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, _) = banc_fichier(&dossier, 100);
    let compteur = super::track_repo::sql::count_visible(Engine::Sqlite);
    let liste = format!(
        "SELECT t.id{} WHERE {}",
        super::track_repo::sql::track_from(),
        copie_de_moindre_qualite_exclue()
    );
    for (nom, sql) in [("compteur", compteur), ("liste", liste)] {
        let p = plan(&db, &sql);
        assert!(
            p.contains(&format!("SEARCH mieux USING INDEX {INDEX} (album_id=? AND <expr>=? AND <expr>=? AND <expr>=?)")),
            "{nom} : la copie doit être cherchée par la clé entière ({INDEX}) — sinon chaque \
             piste relit tout son album, n × pistes par album (#5138) :\n  {p}"
        );
    }
}

/// L'index est posé par la passe REJOUÉE au démarrage : une base déjà
/// montée par la 0.9.165 le reçoit au simple redémarrage, sans que le
/// numéro de migration bouge.
#[test]
fn l_index_est_pose_au_redemarrage_d_une_base_existante() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, _) = banc_fichier(&dossier, 100);
    let version = || un_entier(&db, "SELECT MAX(version) FROM _migrations");
    let avant = version();
    db.execute_batch(&format!("DROP INDEX IF EXISTS {INDEX};"))
        .unwrap();
    let present = || {
        un_entier(
            &db,
            &format!(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = '{INDEX}'"
            ),
        )
    };
    assert_eq!(present(), 0);
    super::migrations::run_migrations(&db).unwrap();
    assert_eq!(present(), 1, "la passe rejouée doit reposer {INDEX}");
    assert_eq!(version(), avant, "aucun numéro de migration consommé");
}

/// Même ensemble, même ordre, même total qu'en 0.9.165 — sur la base PRIVÉE
/// de l'index (l'état d'avant) contre la base qui l'a, page après page comme
/// la vue Oxygen les demande. Et chaque exclusion pour sa raison.
#[test]
fn les_pages_et_le_total_sont_ceux_d_avant() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, banc) = banc_fichier(&dossier, 170);
    let repo = TrackRepo::new(db.clone());

    db.execute_batch(&format!("DROP INDEX IF EXISTS {INDEX};"))
        .unwrap();
    let reference = liste_d_avant(&db, ", t.id");
    let mut ensemble_d_avant = liste_d_avant(&db, "");
    ensemble_d_avant.sort_unstable();
    let total_d_avant = repo.count_visible().unwrap();
    super::migrations::run_migrations(&db).unwrap();

    assert_eq!(reference.len() as i64, total_d_avant, "le banc lui-même");
    let mut pages = Vec::new();
    let mut offset = 0;
    loop {
        let (page, total) = repo.list_visible_avec_total(500, offset).unwrap();
        if page.is_empty() {
            assert_eq!(total, None, "une page vide ne porte pas de total");
            break;
        }
        assert_eq!(
            total,
            Some(total_d_avant),
            "total de la page à l'offset {offset}"
        );
        let n = page.len();
        pages.extend(page.into_iter().filter_map(|t| t.id));
        if n < 500 {
            break;
        }
        offset += 500;
    }
    assert_eq!(
        pages, reference,
        "pages mises bout à bout ≠ liste de la 0.9.165"
    );
    let mut ensemble = pages.clone();
    ensemble.sort_unstable();
    assert_eq!(
        ensemble, ensemble_d_avant,
        "ensemble rendu ≠ SQL nu de la 0.9.165"
    );
    assert_eq!(repo.count_visible().unwrap(), total_d_avant);
    // `list_visible` elle-même ne change pas de lignes.
    let entiere: Vec<i64> = repo
        .list_visible(100_000, 0)
        .unwrap()
        .into_iter()
        .filter_map(|t| t.id)
        .collect();
    assert_eq!(entiere, reference);

    // Chaque exclusion, pour sa raison.
    let rendues: std::collections::HashSet<i64> = pages.iter().copied().collect();
    let albums_de = |ids: &[i64]| -> Vec<i64> {
        repo.list_by_ids(ids)
            .unwrap()
            .into_iter()
            .filter_map(|t| t.album_id)
            .collect()
    };
    let albums_rendus = albums_de(&pages);
    for al in banc.masques.iter().chain(&banc.distants_doubles) {
        assert!(
            !albums_rendus.contains(al),
            "album {al} masqué ou doublé : ses pistes sortent"
        );
    }
    for id in &banc.ecartees {
        assert!(
            !rendues.contains(id),
            "piste {id} : copie de moindre qualité, elle sort"
        );
    }
    for id in banc.meilleures.iter().chain(&banc.sans_album) {
        assert!(rendues.contains(id), "piste {id} : doit rester");
    }
}

/// La MESURE, sur le profil de JeromeQ (≈ 35 000 pistes, base fichier, dont
/// l'album de 1 000 pistes sans numéro) : compteur, page de 2 000 en tête et
/// en queue. Le seuil est celui de la trace `slow_query` du serveur (500 ms,
/// #4800) : plus aucune de ces lectures ne doit y paraître. Rouge sans
/// l'index de la clé de copie, vert avec. Ignoré par défaut : une durée n'est
/// pas un verdict de CI, et la charge de la machine la fait varier.
///
/// ```text
/// cargo test -p tune-core --lib temoin_de_duree_5138 -- --ignored --nocapture
/// ```
#[test]
#[ignore = "mesure : lancer à la main"]
fn temoin_de_duree_5138() {
    const SEUIL: Duration = Duration::from_millis(500);
    let dossier = tempfile::tempdir().unwrap();
    let (db, _) = banc_fichier(&dossier, 1_700);
    eprintln!(
        "banc : {} pistes, {} albums, {} artistes",
        un_entier(&db, "SELECT COUNT(*) FROM tracks"),
        un_entier(&db, "SELECT COUNT(*) FROM albums"),
        un_entier(&db, "SELECT COUNT(*) FROM artists"),
    );
    eprintln!(
        "--- plan du compteur :\n  {}",
        plan(&db, &super::track_repo::sql::count_visible(Engine::Sqlite))
    );
    let repo = TrackRepo::new(db.clone());
    let mut pire = Duration::ZERO;
    let mut mesure = |nom: &str, f: &dyn Fn() -> i64| {
        // La première lecture chauffe le cache de pages ; on garde le pire
        // des trois suivantes.
        f();
        let mut ici = Duration::ZERO;
        let mut n = 0;
        for _ in 0..3 {
            let debut = Instant::now();
            n = f();
            ici = ici.max(debut.elapsed());
        }
        eprintln!("{nom:<46} {:>8.1} ms  (= {n})", ici.as_secs_f64() * 1e3);
        pire = pire.max(ici);
    };
    mesure("(b) count_visible", &|| repo.count_visible().unwrap());
    mesure("(a) page 2000 à l'offset 0, total compris", &|| {
        repo.list_visible_avec_total(2_000, 0)
            .unwrap()
            .1
            .unwrap_or(-1)
    });
    mesure("(a) page 2000 à l'offset 32000, total compris", &|| {
        repo.list_visible_avec_total(2_000, 32_000)
            .unwrap()
            .1
            .unwrap_or(-1)
    });
    assert!(
        pire < SEUIL,
        "pire durée {pire:?} ≥ {SEUIL:?} sur ≈ 35 000 pistes : la lecture paraîtrait en `slow_query` (#5138)"
    );
}
