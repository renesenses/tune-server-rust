//! #9158 (Rob) — témoins de la pose des tags du scan : passe « Appliquer »,
//! retrait complet, aperçu des clés, et la même chose sous PostgreSQL
//! (`pg_9158_…`, sauté sans `TUNE_TEST_PG_URL`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::tag_repo::TagRepo;
use crate::metadata::balises_brutes::{BaliseBrute, BalisesDuFichier};
use crate::metadata::tags_perso::TagsPerso;

fn reglage(json: serde_json::Value) -> TagsPerso {
    serde_json::from_value::<TagsPerso>(json)
        .unwrap()
        .valider()
        .unwrap()
}

fn base_sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Trois pistes : 1 et 2 sur l'album `base`, 3 sur l'album `base + 1`.
fn poser(b: &Arc<dyn DbBackend>, base: i64) {
    let a = base;
    let sql = [
        format!("INSERT INTO artists (id, name) VALUES ({a}, 'Rob {a}')"),
        format!("INSERT INTO albums (id, title, artist_id) VALUES ({a}, 'A', {a})"),
        format!(
            "INSERT INTO albums (id, title, artist_id) VALUES ({}, 'B', {a})",
            a + 1
        ),
        format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels, file_path) \
             VALUES ({}, 'Un', {a}, {a}, 1000, 1, 1, 2, '/m9158/{a}/1.flac')",
            a + 1
        ),
        format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels, file_path) \
             VALUES ({}, 'Deux', {a}, {a}, 1000, 1, 2, 2, '/m9158/{a}/2.flac')",
            a + 2
        ),
        format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels, file_path) \
             VALUES ({}, 'Trois', {}, {a}, 1000, 1, 1, 2, '/m9158/{a}/3.flac')",
            a + 3,
            a + 1
        ),
    ];
    for s in sql {
        b.execute(&s, &[]).unwrap();
    }
}

fn noms(b: &Arc<dyn DbBackend>, item_type: &str, item_id: i64) -> Vec<String> {
    let mut v: Vec<String> = TagRepo::with_backend(b.clone())
        .tags_for_item(item_type, item_id)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    v.sort();
    v
}

/// La passe « Appliquer », lot après lot, avec une lecture simulée.
fn appliquer(
    b: &Arc<dyn DbBackend>,
    r: &TagsPerso,
    fichiers: &HashMap<String, Vec<(String, String)>>,
    apres: i64,
) -> Bilan {
    let mut curseur = apres;
    let mut bilan = Bilan::default();
    loop {
        let lot = traiter_lot(b, curseur, 2, r, |c: &Path| {
            fichiers.get(c.to_str().unwrap()).cloned()
        })
        .unwrap();
        if lot.fini {
            return bilan;
        }
        curseur = lot.dernier;
        bilan.ajouter(lot.bilan);
    }
}

fn pp(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(k, x)| (k.to_string(), x.to_string()))
        .collect()
}

/// Le scénario commun aux deux moteurs.
fn scenario(b: &Arc<dyn DbBackend>, base: i64) {
    poser(b, base);
    let (t1, t2, t3, alb) = (base + 1, base + 2, base + 3, base);
    let manuel = TagRepo::with_backend(b.clone())
        .create(&format!("Manuel {base}"), None)
        .unwrap();
    TagRepo::with_backend(b.clone())
        .tag_item(manuel, "track", t1)
        .unwrap();
    let humeur = |v: &str| format!("{v} {base}");
    let mut fichiers: HashMap<String, Vec<(String, String)>> = HashMap::from([
        (
            format!("/m9158/{base}/1.flac"),
            pp(&[("MOOD", &format!("{};{}", humeur("Calme"), humeur("Nuit")))]),
        ),
        (
            format!("/m9158/{base}/2.flac"),
            pp(&[("TXXX:MOOD", &humeur("Nuit"))]),
        ),
        // Piste 3 : illisible (absente de la table).
    ]);
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "MOOD" }] }));
    let apres = base;
    let b1 = appliquer(b, &r, &fichiers, apres);
    assert_eq!(
        noms(b, "track", t1),
        vec![humeur("Calme"), format!("Manuel {base}"), humeur("Nuit")]
    );
    assert_eq!(noms(b, "track", t2), vec![humeur("Nuit")]);
    assert_eq!(noms(b, "track", t3), Vec::<String>::new());
    assert_eq!(
        noms(b, "album", alb),
        vec![humeur("Nuit")],
        "piste ET album"
    );
    assert!(b1.ajoutes >= 4, "{b1:?}");

    // Idempotence : la même passe n'écrit plus rien.
    assert_eq!(appliquer(b, &r, &fichiers, apres), Bilan::default());

    // Retrait : Nuit quitte la piste 2 → part de la piste 2 et de l'album.
    fichiers.insert(format!("/m9158/{base}/2.flac"), pp(&[("TITLE", "x")]));
    let b3 = appliquer(b, &r, &fichiers, apres);
    assert_eq!(b3.retires, 2, "{b3:?}");
    assert_eq!(noms(b, "track", t2), Vec::<String>::new());
    assert_eq!(noms(b, "album", alb), Vec::<String>::new());

    // Réglage vidé : tout ce que le scan a posé part, le manuel reste.
    let n = retirer_tout(b).unwrap();
    assert_eq!(n, 2, "Calme et Nuit de la piste 1");
    assert_eq!(noms(b, "track", t1), vec![format!("Manuel {base}")]);
    let marques: i64 = b
        .query_many(
            "SELECT COUNT(*) FROM item_tags_scan WHERE item_type = 'track' AND item_id = ?",
            &[&t1 as &dyn ToSqlValue],
        )
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(marques, 0);
}

#[test]
fn la_passe_appliquer_pose_retire_et_reste_idempotente_9158() {
    scenario(&base_sqlite(), 100);
}

#[test]
fn supprimer_un_tag_efface_ses_marques_9158() {
    let b = base_sqlite();
    poser(&b, 200);
    let mut r = Resolveur::new(&b).unwrap();
    let id = r.id("Pluie").unwrap();
    assert_eq!(r.id("pluie").unwrap(), id, "la casse est ignorée");
    synchroniser(&b, "track", 201, &[id]).unwrap();
    TagRepo::with_backend(b.clone()).delete(id).unwrap();
    let n = b
        .query_many("SELECT COUNT(*) FROM item_tags_scan", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn l_apercu_compte_les_cles_libres_par_fichier_9158() {
    let b = |bloc: &'static str, cle: &str, v: &str| BaliseBrute {
        bloc,
        cle: cle.into(),
        valeur: v.into(),
        tronquee: false,
    };
    let fichiers: HashMap<String, BalisesDuFichier> = HashMap::from([
        (
            "/a".to_string(),
            BalisesDuFichier {
                format: Some("MPEG".into()),
                balises: vec![
                    b("id3v2", "TIT2", "Titre"),
                    b("id3v2", "TXXX:Occasion", "Dîner"),
                    b("id3v2", "TXXX:MusicBrainz Album Id", "x"),
                ],
                liste_tronquee: false,
            },
        ),
        (
            "/b".to_string(),
            BalisesDuFichier {
                format: Some("FLAC".into()),
                balises: vec![
                    b("vorbis", "OCCASION", "Fête"),
                    b("vorbis", "OCCASION", "Nuit"),
                    b("vorbis", "TITLE", "t"),
                ],
                liste_tronquee: false,
            },
        ),
    ]);
    let chemins = vec!["/a".to_string(), "/b".to_string(), "/absent".to_string()];
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "TITLE" }] }));
    let a = apercu(
        &chemins,
        &r,
        |c| fichiers.get(c.to_str().unwrap()).cloned(),
        || true,
    );
    assert_eq!(a.sampled_files, 2);
    assert_eq!(a.unreadable_files, 1);
    let occ = &a.keys[0];
    assert_eq!(
        occ.key, "OCCASION",
        "la clé libre vient en tête : {:?}",
        a.keys
    );
    assert_eq!(
        occ.files, 2,
        "deux trames dans un fichier comptent une fois"
    );
    assert!(!occ.known);
    assert_eq!(
        occ.forms,
        vec!["TXXX:Occasion".to_string(), "OCCASION".to_string()]
    );
    let titre = a.keys.iter().find(|k| k.key == "TITLE").unwrap();
    assert!(titre.known && titre.configured);
    assert!(
        a.keys.iter().all(|k| k.key != "TIT2"),
        "trame standard exclue"
    );
    // Le délai arrête l'échantillon.
    let coupe = apercu(&chemins, &r, |_| None, || false);
    assert!(coupe.timed_out);
    assert_eq!(coupe.sampled_files, 0);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_9158_tags_perso() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let base: i64 = 915_800;
    let nettoyer = |db: &Arc<dyn DbBackend>| {
        for s in [
            format!(
                "DELETE FROM item_tags_scan WHERE item_id BETWEEN {base} AND {}",
                base + 9
            ),
            format!(
                "DELETE FROM item_tags WHERE item_id BETWEEN {base} AND {}",
                base + 9
            ),
            format!("DELETE FROM tags WHERE name LIKE '% {base}'"),
            format!(
                "DELETE FROM tracks WHERE id BETWEEN {base} AND {}",
                base + 9
            ),
            format!(
                "DELETE FROM albums WHERE id BETWEEN {base} AND {}",
                base + 9
            ),
            format!("DELETE FROM artists WHERE id = {base}"),
        ] {
            let _ = db.execute(&s, &[]);
        }
    };
    nettoyer(&db);
    scenario(&db, base);
    nettoyer(&db);
}
