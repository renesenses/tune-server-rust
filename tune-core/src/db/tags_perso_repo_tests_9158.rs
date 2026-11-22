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

// ---------------------------------------------------------------------------
// Relecture indépendante (19/11) : la pose SOUS le `BEGIN IMMEDIATE` d'un lot
// de scan, sur une base FICHIER (WAL : le pool de lecture ne voit pas la
// transaction de l'écrivain ; `:memory:` partage une seule connexion et
// masquerait le défaut).
// ---------------------------------------------------------------------------

fn base_fichier(dossier: &Path) -> Arc<dyn DbBackend> {
    let chemin = dossier.join("tune-9158.db");
    let db = crate::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn entree(id: i64, tags: &[&str]) -> (i64, HashMap<String, String>) {
    let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
    (
        id,
        HashMap::from([(
            crate::metadata::tags_perso::CLE_LUE.to_string(),
            crate::metadata::tags_perso::encoder(&tags),
        )]),
    )
}

/// Scénario A : premier scan. Les pistes sont insérées DANS la transaction
/// du lot, puis `ecrire_lot` : l'album reçoit le tag que portent ses deux
/// pistes. Scénario B : rescan, une piste perd le tag : l'album le perd.
#[test]
fn sous_begin_immediate_l_album_suit_ses_pistes_base_fichier_9158() {
    let dossier = tempfile::tempdir().unwrap();
    let b = base_fichier(dossier.path());

    // A — tout dans la transaction du lot, comme le scan.
    b.execute_batch("BEGIN IMMEDIATE").unwrap();
    poser(&b, 700);
    let (t1, t2, alb) = (701, 702, 700);
    let bilan = ecrire_lot(&b, &[entree(t1, &["Nuit"]), entree(t2, &["Nuit"])]);
    b.execute_batch("COMMIT").unwrap();
    assert_eq!(noms(&b, "track", t1), vec!["Nuit".to_string()]);
    assert_eq!(noms(&b, "track", t2), vec!["Nuit".to_string()]);
    assert_eq!(
        noms(&b, "album", alb),
        vec!["Nuit".to_string()],
        "A : l'album doit recevoir le tag de ses deux pistes ({bilan:?})"
    );

    // B — rescan du fichier 2 modifié : Nuit retiré de la piste 2.
    b.execute_batch("BEGIN IMMEDIATE").unwrap();
    let bilan = ecrire_lot(&b, &[entree(t2, &[])]);
    b.execute_batch("COMMIT").unwrap();
    assert_eq!(noms(&b, "track", t1), vec!["Nuit".to_string()]);
    assert_eq!(noms(&b, "track", t2), Vec::<String>::new());
    assert_eq!(
        noms(&b, "album", alb),
        Vec::<String>::new(),
        "B : l'album ne doit plus porter un tag qu'une piste a perdu ({bilan:?})"
    );

    // Idempotence sous transaction : le même lot n'écrit plus rien.
    b.execute_batch("BEGIN IMMEDIATE").unwrap();
    let bilan = ecrire_lot(&b, &[entree(t1, &["Nuit"]), entree(t2, &[])]);
    b.execute_batch("COMMIT").unwrap();
    assert_eq!(bilan, Bilan::default());
}

/// Un tag créé dans la transaction d'un lot (nom neuf) est retrouvé, casse
/// et forme Unicode ignorées, par le lot suivant de la même transaction.
#[test]
fn sous_begin_immediate_un_tag_neuf_est_reutilise_nfc_9158() {
    let dossier = tempfile::tempdir().unwrap();
    let b = base_fichier(dossier.path());
    b.execute_batch("BEGIN IMMEDIATE").unwrap();
    poser(&b, 800);
    ecrire_lot(&b, &[entree(801, &["Café"])]);
    // « Cafe » + accent combinant (NFD), autre casse : le même tag.
    ecrire_lot(&b, &[entree(802, &["CAFE\u{301}"])]);
    b.execute_batch("COMMIT").unwrap();
    let n: i64 = b.query_many("SELECT COUNT(*) FROM tags", &[]).unwrap()[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(n, 1, "un seul tag : {:?}", noms(&b, "track", 802));
    assert_eq!(noms(&b, "album", 800), vec!["Café".to_string()]);
}

/// Un tag repris PAR FILTRE devient manuel : le scan ne le retire plus.
#[test]
fn un_tag_repris_par_filtre_n_est_plus_retire_par_le_scan_9158() {
    use crate::db::track_repo::{CibleDuFiltre, TrackFilter, TrackRepo};
    let b = base_sqlite();
    poser(&b, 900);
    ecrire_lot(&b, &[entree(901, &["Fête"])]);
    let fete = TagRepo::with_backend(b.clone())
        .get_by_name("Fête")
        .unwrap()
        .and_then(|t| t.id)
        .unwrap();
    let filtre = TrackFilter {
        collection_track_ids: Some(vec![901]),
        ..Default::default()
    };
    TrackRepo::with_backend(b.clone())
        .etiqueter_par_filtre(fete, &filtre, CibleDuFiltre::Pistes, false)
        .unwrap()
        .unwrap();
    // Le fichier n'a plus la balise : la pose, devenue manuelle, reste.
    ecrire_lot(&b, &[entree(901, &[])]);
    assert_eq!(noms(&b, "track", 901), vec!["Fête".to_string()]);
}

/// L'aperçu ne propose que des clés que le réglage accepte.
#[test]
fn l_apercu_ne_propose_que_des_cles_acceptees_9158() {
    let b = |cle: &str| BaliseBrute {
        bloc: "vorbis",
        cle: cle.into(),
        valeur: "x".into(),
        tronquee: false,
    };
    let fichier = BalisesDuFichier {
        format: Some("FLAC".into()),
        balises: vec![b("ÉPOQUE"), b("MOOD/2"), b("HUMEUR (ROB)"), b("OCCASION")],
        liste_tronquee: false,
    };
    let a = apercu(
        &["/a".to_string()],
        &TagsPerso::default(),
        |_| Some(fichier.clone()),
        || true,
    );
    let cles: Vec<&str> = a.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(cles, vec!["OCCASION"], "{cles:?}");
    for k in &a.keys {
        serde_json::from_value::<TagsPerso>(serde_json::json!({ "mappings": [{ "key": k.key }] }))
            .unwrap()
            .valider()
            .unwrap_or_else(|e| panic!("{} refusée : {e}", k.key));
    }
}

/// La fusion d'albums redirige les marques « scan » avec leurs poses ; une
/// pose manuelle de la cible ne devient pas « du scan ».
#[test]
fn la_fusion_d_albums_redirige_les_marques_du_scan_9158() {
    let b = base_sqlite();
    poser(&b, 1000);
    let (cible, doublon) = (1000, 1001);
    let repo = TagRepo::with_backend(b.clone());
    let x = repo.create("X", None).unwrap();
    let y = repo.create("Y", None).unwrap();
    // Le doublon tient X et Y du scan ; la cible porte Y à la main.
    synchroniser(&b, "album", doublon, &[x, y]).unwrap();
    repo.tag_item(y, "album", cible).unwrap();
    crate::db::album_repo::AlbumRepo::with_backend(b.clone())
        .absorber(cible, doublon)
        .unwrap();
    assert_eq!(
        noms(&b, "album", cible),
        vec!["X".to_string(), "Y".to_string()]
    );
    let marques = |item: i64| -> Vec<i64> {
        let mut v: Vec<i64> = b
            .query_many(
                "SELECT tag_id FROM item_tags_scan WHERE item_type = 'album' AND item_id = ?",
                &[&item as &dyn ToSqlValue],
            )
            .unwrap()
            .into_iter()
            .filter_map(|r| r[0].as_i64())
            .collect();
        v.sort();
        v
    };
    assert_eq!(marques(cible), vec![x], "X suit sa pose, Y reste manuel");
    assert!(marques(doublon).is_empty());
    // Le scan rend la cible conforme à « rien » : X part, Y (manuel) reste.
    synchroniser(&b, "album", cible, &[]).unwrap();
    assert_eq!(noms(&b, "album", cible), vec!["Y".to_string()]);
}

// ---------------------------------------------------------------------------
// Suite de la relecture (19/11 soir).
// ---------------------------------------------------------------------------

/// Un nom de tag impossible à créer saute SA piste ; le lot continue.
#[test]
fn un_nom_non_resolu_saute_sa_piste_sans_arreter_le_lot_9158() {
    let b = base_sqlite();
    poser(&b, 1100);
    ecrire_lot(&b, &[entree(1101, &["Avant"])]);
    b.execute_batch(
        "CREATE TRIGGER interdit_9158 BEFORE INSERT ON tags WHEN NEW.name = 'Interdit' \
         BEGIN SELECT RAISE(ABORT, 'interdit'); END;",
    )
    .unwrap();
    let bilan = ecrire_lot(&b, &[entree(1101, &["Interdit"]), entree(1102, &["Ok"])]);
    assert_eq!(noms(&b, "track", 1102), vec!["Ok".to_string()], "{bilan:?}");
    assert_eq!(
        noms(&b, "track", 1101),
        vec!["Avant".to_string()],
        "piste sautée : rien retiré"
    );
}

/// Fusion : le tag que le doublon porte À LA MAIN et la cible du scan
/// devient manuel sur la cible.
#[test]
fn la_fusion_garde_la_pose_manuelle_du_doublon_9158() {
    let b = base_sqlite();
    poser(&b, 1200);
    let (cible, doublon) = (1200, 1201);
    let repo = TagRepo::with_backend(b.clone());
    let z = repo.create("Z", None).unwrap();
    synchroniser(&b, "album", cible, &[z]).unwrap();
    repo.tag_item(z, "album", doublon).unwrap();
    crate::db::album_repo::AlbumRepo::with_backend(b.clone())
        .absorber(cible, doublon)
        .unwrap();
    synchroniser(&b, "album", cible, &[]).unwrap();
    assert_eq!(
        noms(&b, "album", cible),
        vec!["Z".to_string()],
        "la pose manuelle du doublon ne part pas au scan"
    );
}

/// Renommer un tag posé par le scan le rend manuel : le scan suivant ne le
/// retire pas (il repose l'ancien nom, lu dans le fichier).
#[test]
fn renommer_un_tag_du_scan_le_rend_manuel_9158() {
    let b = base_sqlite();
    poser(&b, 1300);
    ecrire_lot(&b, &[entree(1301, &["Nuit"])]);
    let repo = TagRepo::with_backend(b.clone());
    let id = repo
        .get_by_name("Nuit")
        .unwrap()
        .and_then(|t| t.id)
        .unwrap();
    // Changer seulement la casse ne compte pas comme un renommage.
    repo.update(id, Some("NUIT"), None).unwrap();
    ecrire_lot(&b, &[entree(1301, &[])]);
    assert_eq!(
        noms(&b, "track", 1301),
        Vec::<String>::new(),
        "casse : reste du scan"
    );
    ecrire_lot(&b, &[entree(1301, &["Nuit"])]);
    repo.update(id, Some("Soir"), None).unwrap();
    ecrire_lot(&b, &[entree(1301, &["Nuit"])]);
    assert_eq!(
        noms(&b, "track", 1301),
        vec!["Nuit".to_string(), "Soir".to_string()]
    );
    ecrire_lot(&b, &[entree(1301, &[])]);
    assert_eq!(
        noms(&b, "track", 1301),
        vec!["Soir".to_string()],
        "Soir est manuel"
    );
}
