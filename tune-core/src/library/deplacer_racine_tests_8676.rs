//! #8676 — « Remplacer le chemin ». Témoins sur SQLite ET PostgreSQL
//! (`pg_8676_…`, saute sans `TUNE_TEST_PG_URL`, étape dédiée dans
//! `test-postgres.yml`).
//!
//! Le témoin : deux racines voisines, `/srv/Musique8676` et
//! `/srv/Musique86762` (préfixe de NOM, pas de composant). Seule la première
//! doit bouger, vers `/data/Bibli8676`, avec tous ses chemins recensés, sans
//! qu'un identifiant change ; puis l'opération inverse doit rendre la base
//! d'origine à l'octet près.
use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, SqlValue};

const A: &str = "/srv/Musique8676";
const VOISINE: &str = "/srv/Musique86762";
const B: &str = "/data/Bibli8676";

fn p(a: &str, n: &str) -> Prefixes {
    Prefixes::nouveaux(a, n).expect("préfixes valides")
}

// ─── La règle de préfixe, en Rust ─────────────────────────────────────────

#[test]
fn le_prefixe_se_compare_sur_une_frontiere_de_composant_8676() {
    let pr = p("/mnt/Music", "/media/nas/Musique");
    assert_eq!(
        remplacer("/mnt/Music/Jazz/a.flac", &pr).as_deref(),
        Some("/media/nas/Musique/Jazz/a.flac")
    );
    assert_eq!(
        remplacer("/mnt/Music", &pr).as_deref(),
        Some("/media/nas/Musique")
    );
    assert_eq!(
        remplacer("/mnt/Music2/a.flac", &pr),
        None,
        "#8676 : /mnt/Music ne couvre pas /mnt/Music2"
    );
    assert_eq!(remplacer("/mnt/Musi", &pr), None);
    assert_eq!(remplacer("/autre/mnt/Music/a.flac", &pr), None);
}

#[test]
fn les_separateurs_finaux_sont_ignores_8676() {
    let pr = p("/mnt/Music/", "/media/Musique//");
    assert_eq!(pr.ancien, "/mnt/Music");
    assert_eq!(pr.nouveau, "/media/Musique");
    assert_eq!(
        remplacer("/mnt/Music/a.flac", &pr).as_deref(),
        Some("/media/Musique/a.flac")
    );
}

#[test]
fn les_chemins_windows_sont_compares_separateurs_normalises_8676() {
    let pr = p("D:\\Musique\\", "E:\\Bibli");
    assert_eq!(
        remplacer("D:\\Musique\\Jazz\\a.flac", &pr).as_deref(),
        Some("E:\\Bibli\\Jazz\\a.flac")
    );
    // Une base Windows porte l'un ou l'autre séparateur.
    assert_eq!(
        remplacer("D:/Musique/Jazz/a.flac", &pr).as_deref(),
        Some("E:\\Bibli/Jazz/a.flac")
    );
    assert_eq!(remplacer("D:\\Musique2\\a.flac", &pr), None);
    // Unix → Windows : la fin du chemin prend les séparateurs du nouveau.
    let pr = p("/mnt/Music", "D:\\Musique");
    assert_eq!(
        remplacer("/mnt/Music/Jazz/a.flac", &pr).as_deref(),
        Some("D:\\Musique\\Jazz\\a.flac")
    );
    // Et retour : l'inverse rend le chemin d'origine.
    let inv = pr.inverse();
    assert_eq!(
        remplacer("D:\\Musique\\Jazz\\a.flac", &inv).as_deref(),
        Some("/mnt/Music/Jazz/a.flac")
    );
}

#[test]
fn les_prefixes_invalides_sont_refuses_8676() {
    for (a, n) in [
        ("/", "/x"),
        ("", "/x"),
        ("relatif/x", "/y"),
        ("/x", "/y/../z"),
        ("/x", "  "),
    ] {
        assert_eq!(
            Prefixes::nouveaux(a, n),
            Err(Refus::InvalidPrefix),
            "{a:?} → {n:?}"
        );
    }
    assert_eq!(Prefixes::nouveaux("/x/", "/x"), Err(Refus::SamePrefix));
    assert!(Prefixes::nouveaux("\\\\nas\\musique", "/mnt/nas").is_ok());
}

// ─── Le témoin en base ────────────────────────────────────────────────────

fn ex(db: &dyn DbBackend, sql: &str) {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Pose le témoin. `base` décale les identifiants (base PG partagée).
fn poser(db: &dyn DbBackend, base: i64) {
    ex(
        db,
        &format!(
            "INSERT INTO artists (id, name, image_path) VALUES ({base}, 'Artiste 8676', '{A}/Artiste/artist.jpg')"
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO albums (id, title, artist_id, folder_path, cover_path, cover_source_path) \
         VALUES ({base}, 'Album 8676', {base}, '{A}/Artiste/Album', '{A}/Artiste/Album/cover.jpg', '{A}/Artiste/Album/folder.jpg')"
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO albums (id, title, artist_id, folder_path) VALUES ({}, 'Voisin 8676', {base}, '{VOISINE}/Album')",
            base + 1
        ),
    );
    for i in 0..3 {
        ex(
            db,
            &format!(
                "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
             VALUES ({}, 'Titre {i}', {base}, {base}, {}, '{A}/Artiste/Album/0{i}.flac')",
                base + i,
                i + 1
            ),
        );
    }
    // Une piste de feuille CUE : `file_path` nul, son support.
    ex(
        db,
        &format!(
            "INSERT INTO tracks (id, title, artist_id, album_id, track_number, cue_media_path) \
         VALUES ({}, 'Cue', {base}, {base}, 4, '{A}/Artiste/Album/image.flac')",
            base + 3
        ),
    );
    // La racine voisine : ne doit pas bouger.
    ex(
        db,
        &format!(
            "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
         VALUES ({}, 'Voisine', {base}, {}, 1, '{VOISINE}/Album/01.flac')",
            base + 4,
            base + 1
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO track_copies (track_id, file_path) VALUES ({base}, '{A}/Copie/00.flac')"
        ),
    );
    ex(
        db,
        &format!("INSERT INTO album_preferred_roots (album_id, root) VALUES ({base}, '{A}')"),
    );
    ex(
        db,
        &format!(
            "INSERT INTO favorites (item_type, item_id, item_path) VALUES ('track', {}, '{A}/Artiste/Album/01.flac')",
            base + 1
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO playlists (id, name, description) VALUES ({base}, 'Dossier', 'Dossier : {A}/Compil')"
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES ({base}, {}, 0)",
            base + 2
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO file_first_seen (file_path, first_seen_at) VALUES ('{A}/Artiste/Album/00.flac', 1)"
        ),
    );
    ex(
        db,
        &format!(
            "INSERT INTO bpm_estimes (file_path, bpm, source, mesure_le) VALUES ('{A}/Artiste/Album/00.flac', 120, 'test', '2026-11-13')"
        ),
    );
    let reglages = [
        ("music_dirs", serde_json::json!([A, VOISINE]).to_string()),
        (
            "background_analysis_excluded_roots",
            serde_json::json!([format!("{A}/Artiste")]).to_string(),
        ),
        (
            "scan_exclude_paths",
            serde_json::json!(["/incoming/", format!("{A}/Brouillons")]).to_string(),
        ),
        ("cd_rip_output_dir", format!("{A}/CD")),
    ];
    for (k, v) in reglages {
        db.execute(
            &format!(
                "INSERT INTO settings (key, value) VALUES ({}, {}) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                marque(db.engine(), 1),
                marque(db.engine(), 2)
            ),
            &[&k as &dyn ToSqlValue, &v as &dyn ToSqlValue],
        )
        .unwrap();
    }
}

fn nettoyer(db: &dyn DbBackend, base: i64) {
    for sql in [
        format!("DELETE FROM playlist_tracks WHERE playlist_id = {base}"),
        format!("DELETE FROM playlists WHERE id = {base}"),
        format!(
            "DELETE FROM favorites WHERE item_type = 'track' AND item_id BETWEEN {base} AND {}",
            base + 9
        ),
        format!("DELETE FROM album_preferred_roots WHERE album_id = {base}"),
        format!(
            "DELETE FROM track_copies WHERE track_id BETWEEN {base} AND {}",
            base + 9
        ),
        format!(
            "DELETE FROM tracks WHERE id BETWEEN {base} AND {}",
            base + 9
        ),
        format!(
            "DELETE FROM albums WHERE id BETWEEN {base} AND {}",
            base + 1
        ),
        format!("DELETE FROM artists WHERE id = {base}"),
        "DELETE FROM file_first_seen WHERE file_path LIKE '%8676%'".to_string(),
        "DELETE FROM bpm_estimes WHERE file_path LIKE '%8676%'".to_string(),
        format!(
            "DELETE FROM settings WHERE key IN ('music_dirs', 'background_analysis_excluded_roots', \
             'scan_exclude_paths', 'cd_rip_output_dir', '{CLE_JOURNAL}')"
        ),
    ] {
        ex(db, &sql);
    }
}

/// Toutes les valeurs que l'opération peut toucher, plus les identifiants.
fn instantane(db: &dyn DbBackend, base: i64) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let lire = |sql: &str| -> Vec<Vec<SqlValue>> {
        db.query_many(sql, &[])
            .unwrap_or_else(|e| panic!("{sql} : {e}"))
    };
    let texte = |v: &SqlValue| {
        v.as_string()
            .or_else(|| v.as_i64().map(|i| i.to_string()))
            .unwrap_or_else(|| format!("{v:?}"))
    };
    for r in lire(&format!(
        "SELECT id, file_path, cue_media_path, album_id FROM tracks WHERE id BETWEEN {base} AND {} ORDER BY id",
        base + 9
    )) {
        m.insert(
            format!("track {}", texte(&r[0])),
            format!("{} | {} | {}", texte(&r[1]), texte(&r[2]), texte(&r[3])),
        );
    }
    for r in lire(&format!(
        "SELECT id, folder_path, cover_path, cover_source_path FROM albums WHERE id BETWEEN {base} AND {} ORDER BY id",
        base + 1
    )) {
        m.insert(
            format!("album {}", texte(&r[0])),
            format!("{} | {} | {}", texte(&r[1]), texte(&r[2]), texte(&r[3])),
        );
    }
    for (cle, sql) in [
        ("artist", format!("SELECT image_path FROM artists WHERE id = {base}")),
        ("copy", format!("SELECT track_id, file_path FROM track_copies WHERE track_id = {base}")),
        ("root", format!("SELECT root FROM album_preferred_roots WHERE album_id = {base}")),
        ("fav", format!("SELECT item_id, item_path FROM favorites WHERE item_type = 'track' AND item_id = {}", base + 1)),
        ("playlist", format!("SELECT description FROM playlists WHERE id = {base}")),
        ("playlist_track", format!("SELECT track_id FROM playlist_tracks WHERE playlist_id = {base}")),
        ("first_seen", "SELECT file_path FROM file_first_seen WHERE file_path LIKE '%8676%' ORDER BY file_path".to_string()),
        ("bpm", "SELECT file_path FROM bpm_estimes WHERE file_path LIKE '%8676%' ORDER BY file_path".to_string()),
        ("settings", "SELECT key, value FROM settings WHERE key IN ('music_dirs', 'background_analysis_excluded_roots', 'scan_exclude_paths', 'cd_rip_output_dir') ORDER BY key".to_string()),
    ] {
        let lignes: Vec<String> = lire(&sql).iter().map(|r| r.iter().map(texte).collect::<Vec<_>>().join(" | ")).collect();
        m.insert(cle.to_string(), lignes.join(" ; "));
    }
    m
}

/// Le disque simulé : les fichiers « présents » au nouveau chemin.
fn disque(present: &'static [&'static str]) -> impl Fn(&str) -> bool {
    move |c: &str| present.iter().any(|f| c.ends_with(f))
}

fn verifier(db: &Arc<dyn DbBackend>, base: i64) {
    let pr = p(A, B);
    let avant = instantane(db.as_ref(), base);

    // Aperçu : quatre pistes sous A (trois fichiers + un support CUE), quatre
    // fichiers distincts, dont deux présents au nouveau chemin.
    let ap = apercu(
        db.as_ref(),
        &pr,
        ECHANTILLON_MAX,
        &disque(&["/00.flac", "/image.flac"]),
    )
    .unwrap();
    assert_eq!(ap.tracks, 4, "{ap:#?}");
    assert_eq!(ap.checked, 4, "{ap:#?}");
    assert_eq!((ap.found, ap.missing), (2, 2), "{ap:#?}");
    assert!(
        ap.missing_examples
            .iter()
            .all(|e| e.new.starts_with(B) && e.old.starts_with(A))
    );
    assert_eq!(ap.refusal, None, "{ap:#?}");
    assert!(ap.can_apply);
    let lignes = |t: &str, c: &str| {
        ap.columns
            .iter()
            .find(|l| l.table == t && l.column == c)
            .map(|l| l.rows)
    };
    assert_eq!(lignes("tracks", "file_path"), Some(3));
    assert_eq!(lignes("tracks", "cue_media_path"), Some(1));
    assert_eq!(
        lignes("albums", "folder_path"),
        Some(1),
        "#8676 : l'album voisin n'est pas sous A"
    );
    assert_eq!(lignes("playlists", "description"), Some(1));
    let cles: Vec<_> = ap.settings.iter().map(|s| s.key).collect();
    assert_eq!(
        cles,
        vec![
            "music_dirs",
            "background_analysis_excluded_roots",
            "scan_exclude_paths",
            "cd_rip_output_dir"
        ]
    );
    assert_eq!(
        instantane(db.as_ref(), base),
        avant,
        "l'aperçu n'écrit rien"
    );

    // Refus : aucun fichier retrouvé.
    let refus = apercu(db.as_ref(), &pr, ECHANTILLON_MAX, &|_| false).unwrap();
    assert_eq!(refus.refusal, Some(Refus::NoneFound));
    // Refus : aucune piste sous l'ancien préfixe.
    let refus = apercu(
        db.as_ref(),
        &p("/srv/Rien8676", B),
        ECHANTILLON_MAX,
        &|_| true,
    )
    .unwrap();
    assert_eq!(refus.refusal, Some(Refus::NoTracks));

    // Application.
    let bilan = appliquer(db, &pr).unwrap();
    assert_eq!(bilan.tracks_updated, 3);
    let apres = instantane(db.as_ref(), base);
    for (cle, valeur) in &apres {
        let attendu = avant[cle]
            .replace(&format!("{A}/"), &format!("{B}/"))
            .replace(&format!("\"{A}\""), &format!("\"{B}\""));
        let attendu = if cle == "root" {
            avant[cle].replace(A, B)
        } else {
            attendu
        };
        assert_eq!(valeur, &attendu, "#8676 : {cle} mal réécrit");
    }
    assert!(
        apres.values().all(|v| !v.contains(&format!("{A}/"))),
        "{apres:#?}"
    );
    assert!(
        apres[&format!("track {}", base + 4)].starts_with(&format!("{VOISINE}/")),
        "#8676 : la racine voisine a bougé"
    );
    assert!(apres["settings"].contains(VOISINE));
    assert!(apres["settings"].contains("/incoming/"));

    // Le journal.
    let journal = lire_reglage(db.as_ref(), CLE_JOURNAL)
        .unwrap()
        .expect("journal écrit");
    assert!(journal.contains(A) && journal.contains(B), "{journal}");

    // L'inverse rend la base d'origine.
    appliquer(db, &pr.inverse()).unwrap();
    assert_eq!(
        instantane(db.as_ref(), base),
        avant,
        "#8676 : l'opération inverse doit tout rendre"
    );

    // Collision : une piste existe déjà sous B → aperçu refusé.
    ex(
        db.as_ref(),
        &format!(
            "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
         VALUES ({}, 'Doublon', {base}, {base}, 9, '{B}/Artiste/Album/00.flac')",
            base + 9
        ),
    );
    let refus = apercu(db.as_ref(), &pr, ECHANTILLON_MAX, &|_| true).unwrap();
    assert_eq!(refus.refusal, Some(Refus::Conflicts));
    assert_eq!(refus.conflicts, 1);
    assert_eq!(
        refus.conflict_examples,
        vec![format!("{A}/Artiste/Album/00.flac")]
    );
    // Et l'application forcée échoue d'un bloc, sans rien changer.
    let avant_echec = instantane(db.as_ref(), base);
    assert!(appliquer(db, &pr).is_err());
    assert_eq!(
        instantane(db.as_ref(), base),
        avant_echec,
        "#8676 : la transaction doit tout annuler"
    );
}

#[test]
fn sqlite_deplacer_la_racine_8676() {
    let base = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    crate::db::migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    poser(db.as_ref(), 1);
    verifier(&db, 1);
}

/// Le texte SQL et la règle Rust disent la même chose, sur un corpus.
#[test]
fn sqlite_le_sql_et_le_rust_reecrivent_pareil_8676() {
    let base = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    ex(db.as_ref(), "CREATE TABLE t (v TEXT)");
    let corpus = [
        "/mnt/Music",
        "/mnt/Music/a.flac",
        "/mnt/Music2/a.flac",
        "/mnt/Musi",
        "/mnt/Music/l'été/b.flac",
        "D:\\Musique\\a.flac",
        "D:/Musique/b.flac",
        "D:\\Musique2\\c.flac",
        "D:\\Musique",
        "/mnt/Music\\x",
    ];
    for c in corpus {
        db.execute("INSERT INTO t (v) VALUES (?)", &[&c as &dyn ToSqlValue])
            .unwrap();
    }
    for pr in [
        p("/mnt/Music", "/media/l'été"),
        p("D:\\Musique", "E:\\Bibli"),
        p("/mnt/Music", "D:\\Musique"),
        p("D:/Musique", "/srv/m"),
    ] {
        let (cond, nouvelle) = sql_reecriture("t.v", "", &pr);
        let sql = format!("SELECT v, CASE WHEN {cond} THEN {nouvelle} ELSE NULL END FROM t");
        for r in db.query_many(&sql, &[]).unwrap() {
            let v = r[0].as_string().unwrap();
            assert_eq!(r[1].as_string(), remplacer(&v, &pr), "{pr:?} sur {v}");
        }
    }
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8676_deplacer_la_racine() {
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
    let base = 867_600;
    nettoyer(db.as_ref(), base);
    poser(db.as_ref(), base);
    verifier(&db, base);
    nettoyer(db.as_ref(), base);
}
