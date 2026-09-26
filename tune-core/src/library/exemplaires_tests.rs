//! Témoins de #4907 — la règle de choix, le repli, la promotion et la
//! migration qui garde les identifiants.

use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;

fn ex(track_id: i64, chemin: &str, format: &str, sr: i32, bd: i32, copie: bool) -> Exemplaire {
    Exemplaire {
        track_id,
        chemin: chemin.to_string(),
        format: Some(format.to_string()),
        sample_rate: Some(sr),
        bit_depth: Some(bd),
        file_size: Some(1),
        copie,
    }
}

fn v(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

fn toujours(chemin: &str) -> Option<String> {
    Some(chemin.to_string())
}

// ─── Règle pure ──────────────────────────────────────────────────────────

#[test]
fn sans_reglage_l_ordre_est_celui_des_dossiers_de_musique() {
    let dossiers = v(&["/nas", "/local", "/sauvegarde"]);
    assert_eq!(ordre_effectif(&dossiers, &[]), dossiers);
    // Un réglage partiel classe ce qu'il nomme, puis le reste garde sa place ;
    // une entrée qui n'est plus un dossier de musique est ignorée.
    assert_eq!(
        ordre_effectif(&dossiers, &v(&["/sauvegarde", "/disparu"])),
        v(&["/sauvegarde", "/nas", "/local"])
    );
}

#[test]
fn la_racine_d_un_chemin_est_la_plus_longue_qui_le_contient() {
    let racines = v(&["/musique", "/musique/hires", "/nas"]);
    assert_eq!(
        racine_de("/musique/hires/A/01.flac", &racines).as_deref(),
        Some("/musique/hires")
    );
    assert_eq!(
        racine_de("/musique/A/01.flac", &racines).as_deref(),
        Some("/musique")
    );
    // `/musiquex` n'est pas sous `/musique`.
    assert_eq!(racine_de("/musiquex/A/01.flac", &racines), None);
}

#[test]
fn le_dossier_miroir_est_le_meme_chemin_relatif_sous_les_autres_racines() {
    let racines = v(&["/nas/Musique", "/mnt/sauvegarde"]);
    assert_eq!(
        dossiers_miroirs("/nas/Musique/Miles Davis/Kind of Blue", &racines),
        v(&["/mnt/sauvegarde/Miles Davis/Kind of Blue"])
    );
    assert!(dossiers_miroirs("/ailleurs/Miles Davis/Kind of Blue", &racines).is_empty());
    assert!(dossiers_miroirs("/nas/Musique", &racines).is_empty());
}

/// ⭐ Témoin 3a — une qualité supérieure gagne, quel que soit le répertoire.
#[test]
fn la_meilleure_qualite_gagne_d_abord() {
    let racines = v(&["/nas", "/local"]);
    let o = ordonner(
        vec![
            ex(1, "/nas/A/01.flac", "flac", 44_100, 16, false),
            ex(2, "/local/A/01.flac", "flac", 96_000, 24, false),
        ],
        &racines,
        None,
        1,
    );
    assert_eq!(
        o[0].chemin, "/local/A/01.flac",
        "le 96/24 doit passer devant : {o:?}"
    );
    // Un MP3 ne passe jamais devant un FLAC de même fréquence.
    let o = ordonner(
        vec![
            ex(1, "/nas/A/01.mp3", "mp3", 44_100, 16, false),
            ex(2, "/local/A/01.flac", "flac", 44_100, 16, false),
        ],
        &racines,
        None,
        1,
    );
    assert_eq!(o[0].chemin, "/local/A/01.flac");
}

/// ⭐ Témoin 3b — à qualité égale, l'ordre des répertoires décide.
#[test]
fn a_qualite_egale_l_ordre_des_repertoires_decide() {
    let exs = vec![
        ex(1, "/nas/A/01.flac", "flac", 44_100, 16, false),
        ex(1, "/local/A/01.flac", "flac", 44_100, 16, true),
    ];
    let o = ordonner(exs.clone(), &v(&["/local", "/nas"]), None, 1);
    assert_eq!(o[0].chemin, "/local/A/01.flac", "{o:?}");
    let o = ordonner(exs, &v(&["/nas", "/local"]), None, 1);
    assert_eq!(o[0].chemin, "/nas/A/01.flac", "{o:?}");
}

/// ⭐ Témoin 3c — la préférence de l'album prime sur la qualité ET l'ordre.
#[test]
fn la_preference_de_l_album_prime_sur_la_qualite_et_l_ordre() {
    let racines = v(&["/nas", "/local"]);
    let exs = vec![
        ex(1, "/nas/A/01.flac", "flac", 96_000, 24, false),
        ex(2, "/local/A/01.mp3", "mp3", 44_100, 16, false),
    ];
    let o = ordonner(exs.clone(), &racines, Some("/local"), 1);
    assert_eq!(o[0].chemin, "/local/A/01.mp3", "{o:?}");
    // Sans préférence, la règle par défaut revient.
    let o = ordonner(exs, &racines, None, 1);
    assert_eq!(o[0].chemin, "/nas/A/01.flac");
}

/// ⭐ Témoin 4 (règle) — l'exemplaire préféré injoignable cède au suivant.
#[test]
fn un_exemplaire_injoignable_cede_au_suivant() {
    let o = vec![
        ex(1, "/nas/A/01.flac", "flac", 44_100, 16, false),
        ex(1, "/local/A/01.flac", "flac", 44_100, 16, true),
    ];
    let choix = choisir(&o, |c| (!c.starts_with("/nas")).then(|| c.to_string())).unwrap();
    assert_eq!(choix.exemplaire.chemin, "/local/A/01.flac");
    assert_eq!(choix.ecartes, v(&["/nas/A/01.flac"]));
    let choix = choisir(&o, toujours).unwrap();
    assert!(choix.ecartes.is_empty());
    assert!(choisir(&o, |_| None).is_none());
}

// ─── Base réelle ─────────────────────────────────────────────────────────

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn poser_music_dirs(db: &dyn DbBackend, dirs: &[&str]) {
    let json = serde_json::to_string(dirs).unwrap();
    let params: [&dyn ToSqlValue; 1] = [&json];
    db.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES ('music_dirs', ?, '') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        &params,
    )
    .unwrap();
}

fn piste(db: &dyn DbBackend, id: i64, album: i64, chemin: &str, format: &str, sr: i32, bd: i32) {
    let params: [&dyn ToSqlValue; 6] = [&id, &album, &chemin, &format, &sr, &bd];
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
         file_path, format, sample_rate, bit_depth, duration_ms) \
         VALUES (?, 'So What', ?, 1, 1, 1, ?, ?, ?, ?, 1000)",
        &params,
    )
    .unwrap();
}

fn socle(db: &dyn DbBackend) {
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1);",
    )
    .unwrap();
}

fn copie(db: &dyn DbBackend, track_id: i64, chemin: &str) {
    let n = NouvelExemplaire {
        chemin_proprietaire: String::new(),
        chemin: chemin.to_string(),
        format: Some("flac".into()),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        file_size: Some(4),
        file_mtime: Some(1.0),
        audio_hash: None,
    };
    let proprietaire: String = db
        .query_one(
            &format!("SELECT file_path FROM tracks WHERE id = {track_id}"),
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_string()
        .unwrap();
    assert_eq!(
        rattacher(
            db,
            &[NouvelExemplaire {
                chemin_proprietaire: proprietaire,
                ..n
            }]
        ),
        1
    );
}

fn get(db: &Arc<dyn DbBackend>, id: i64) -> Track {
    crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
}

fn ecrire(chemin: &std::path::Path) {
    std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
    std::fs::write(chemin, b"fLaC").unwrap();
}

/// ⭐ Témoin 4 (base) — préférence posée sur une racine qui ne répond plus :
/// la lecture part depuis l'autre exemplaire, et le repli est noté.
#[test]
fn la_preference_injoignable_se_replie_sur_l_exemplaire_suivant() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("nas");
    let b = dir.path().join("local");
    let fa = a.join("Kind of Blue/01.flac");
    let fb = b.join("Kind of Blue/01.flac");
    ecrire(&fa);
    ecrire(&fb);
    let (a, b) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    );
    let db = base();
    socle(&*db);
    poser_music_dirs(&*db, &[&a, &b]);
    piste(&*db, 7, 1, &fa.to_string_lossy(), "flac", 44_100, 16);
    copie(&*db, 7, &fb.to_string_lossy());

    // Règle par défaut : qualité égale, ordre des répertoires ⇒ le NAS.
    let mut t = get(&db, 7);
    appliquer_a_la_lecture(&*db, &mut t).expect("deux exemplaires");
    assert_eq!(t.file_path.as_deref(), Some(&*fa.to_string_lossy()));

    // Préférence de l'album : le disque local.
    poser_racine_preferee(&*db, 1, &b).unwrap();
    let mut t = get(&db, 7);
    appliquer_a_la_lecture(&*db, &mut t).unwrap();
    assert_eq!(t.file_path.as_deref(), Some(&*fb.to_string_lossy()));
    assert!(!exemplaire_lu(7).unwrap().repli);

    // Le disque local ne répond plus : repli sur le NAS.
    std::fs::remove_dir_all(&b).unwrap();
    let mut t = get(&db, 7);
    let choix = appliquer_a_la_lecture(&*db, &mut t).expect("le NAS répond encore");
    assert_eq!(t.file_path.as_deref(), Some(&*fa.to_string_lossy()));
    assert_eq!(choix.ecartes, vec![fb.to_string_lossy().into_owned()]);
    let lu = exemplaire_lu(7).unwrap();
    assert!(lu.repli, "le repli doit être dit : {lu:?}");
    assert_eq!(lu.racine.as_deref(), Some(a.as_str()));

    // Retirer la préférence rend la règle par défaut.
    assert!(retirer_racine_preferee(&*db, 1).unwrap());
    assert_eq!(racine_preferee(&*db, 1), None);
}

/// Une sœur d'une autre qualité (même album, disque, numéro, titre) est un
/// exemplaire : la meilleure qualité est lue, même depuis la ligne moindre
/// qu'une playlist aurait retenue.
#[test]
fn une_soeur_de_meilleure_qualite_est_lue_depuis_la_ligne_moindre() {
    let dir = tempfile::tempdir().unwrap();
    let fa = dir.path().join("nas/Kind of Blue/01.mp3");
    let fb = dir.path().join("local/Kind of Blue/01.flac");
    ecrire(&fa);
    ecrire(&fb);
    let db = base();
    socle(&*db);
    poser_music_dirs(
        &*db,
        &[
            &dir.path().join("nas").to_string_lossy(),
            &dir.path().join("local").to_string_lossy(),
        ],
    );
    piste(&*db, 1, 1, &fa.to_string_lossy(), "mp3", 44_100, 16);
    piste(&*db, 2, 1, &fb.to_string_lossy(), "flac", 96_000, 24);
    let mut t = get(&db, 1);
    appliquer_a_la_lecture(&*db, &mut t).unwrap();
    assert_eq!(t.id, Some(1), "la PISTE demandée reste la même");
    assert_eq!(t.file_path.as_deref(), Some(&*fb.to_string_lossy()));
    assert_eq!(t.format.as_deref(), Some("flac"));
    assert_eq!(t.sample_rate, Some(96_000));
}

/// Une piste sans exemplaire n'est pas touchée : rien ne change pour une
/// bibliothèque ordinaire.
#[test]
fn une_piste_a_un_seul_fichier_n_est_pas_touchee() {
    let db = base();
    socle(&*db);
    piste(&*db, 1, 1, "/nulle/part/01.flac", "flac", 44_100, 16);
    let mut t = get(&db, 1);
    let avant = serde_json::to_value(&t).unwrap();
    assert!(appliquer_a_la_lecture(&*db, &mut t).is_none());
    assert_eq!(serde_json::to_value(&t).unwrap(), avant);
}

/// ⭐ Témoin 6 — le fichier propre disparu : une copie prend sa place, la
/// piste GARDE son identifiant ; une copie disparue ne retire qu'elle-même.
#[test]
fn un_fichier_disparu_retire_son_exemplaire_pas_la_piste() {
    let dir = tempfile::tempdir().unwrap();
    let fa = dir.path().join("nas/Kind of Blue/01.flac");
    let fb = dir.path().join("local/Kind of Blue/01.flac");
    let fc = dir.path().join("sauvegarde/Kind of Blue/01.flac");
    ecrire(&fb);
    ecrire(&fc);
    let db = base();
    socle(&*db);
    piste(&*db, 9, 1, &fa.to_string_lossy(), "flac", 44_100, 16);
    copie(&*db, 9, &fb.to_string_lossy());
    copie(&*db, 9, &fc.to_string_lossy());

    // La copie `fc` disparaît : elle seule part.
    std::fs::remove_file(&fc).unwrap();
    assert_eq!(
        retirer_le_fichier(&*db, &fc.to_string_lossy()),
        RetraitDuFichier::ExemplaireRetire
    );
    // Le fichier propre (`fa`, jamais écrit) a disparu : `fb` le remplace.
    assert_eq!(
        retirer_le_fichier(&*db, &fa.to_string_lossy()),
        RetraitDuFichier::Promu(fb.to_string_lossy().into_owned())
    );
    let t = get(&db, 9);
    assert_eq!(t.file_path.as_deref(), Some(&*fb.to_string_lossy()));
    assert!(carte_des_exemplaires(&*db).unwrap().is_empty());
    // Plus aucun exemplaire : le retrait ordinaire reprend la main.
    std::fs::remove_file(&fb).unwrap();
    assert_eq!(
        retirer_le_fichier(&*db, &fb.to_string_lossy()),
        RetraitDuFichier::Aucun
    );
}

/// Une piste retirée emporte ses copies (clé étrangère `ON DELETE CASCADE`).
#[test]
fn une_piste_retiree_emporte_ses_copies() {
    let db = base();
    socle(&*db);
    piste(&*db, 3, 1, "/a/01.flac", "flac", 44_100, 16);
    copie(&*db, 3, "/b/01.flac");
    db.execute("DELETE FROM tracks WHERE id = 3", &[]).unwrap();
    assert!(carte_des_exemplaires(&*db).unwrap().is_empty());
}

/// Les exemplaires d'un album : un par racine, joignable, préféré.
#[test]
fn les_exemplaires_d_un_album_se_rangent_par_racine() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("nas");
    let b = dir.path().join("local");
    std::fs::create_dir_all(&a).unwrap();
    let (sa, sb) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    );
    let db = base();
    socle(&*db);
    poser_music_dirs(&*db, &[&sa, &sb]);
    piste(&*db, 1, 1, &format!("{sa}/K/01.flac"), "flac", 96_000, 24);
    copie(&*db, 1, &format!("{sb}/K/01.flac"));
    poser_racine_preferee(&*db, 1, &sb).unwrap();
    let e = exemplaires_de_l_album(&*db, 1).unwrap();
    assert_eq!(e.len(), 2, "{e:?}");
    assert_eq!(e[0].racine.as_deref(), Some(sa.as_str()));
    assert!(e[0].joignable && !e[0].prefere);
    assert_eq!(e[0].sample_rate, Some(96_000));
    assert_eq!(e[1].racine.as_deref(), Some(sb.as_str()));
    assert!(!e[1].joignable, "le dossier local n'existe pas");
    assert!(e[1].prefere);
}

/// ⭐ Témoin 5 — la migration 110 sur une bibliothèque EXISTANTE garde tous
/// les identifiants de pistes : une playlist, un favori et l'historique
/// pointent toujours sur la même piste.
#[test]
fn la_migration_110_garde_les_identifiants_de_pistes() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    // Revenir à une base d'AVANT la 110, peuplée. Les versions suivantes
    // (111, 112…) sont oubliées aussi : le lanceur ne joue que
    // `version > MAX`, une base d'avant la 110 ne les a pas davantage.
    db.execute_batch(
        "DROP TABLE track_copies; DROP TABLE album_preferred_roots;
         DELETE FROM _migrations WHERE version >= 110;
         INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, format)
           VALUES (41, 'So What', 1, 1, '/a/01.flac', 'flac'),
                  (57, 'Freddie Freeloader', 1, 1, '/a/02.flac', 'flac'),
                  (58, 'So What', 1, 1, '/b/01.mp3', 'mp3');
         INSERT INTO playlists (id, name) VALUES (1, 'Jazz');
         INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, 57, 0), (1, 58, 1);
         INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', 41);
         INSERT INTO listen_history (track_id, title) VALUES (58, 'So What');",
    )
    .unwrap();
    let lire = |sql: &str| -> Vec<String> {
        let conn = db.connection().lock().unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| {
            Ok(format!(
                "{}|{}",
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    };
    let pistes = "SELECT id, file_path FROM tracks ORDER BY id";
    let refs = "SELECT t.id, 'pl' FROM playlist_tracks p JOIN tracks t ON t.id = p.track_id \
                UNION ALL SELECT t.id, 'fav' FROM favorites f JOIN tracks t ON t.id = f.item_id \
                UNION ALL SELECT t.id, 'hist' FROM listen_history h JOIN tracks t ON t.id = h.track_id \
                ORDER BY 1, 2";
    let (avant_pistes, avant_refs) = (lire(pistes), lire(refs));
    assert_eq!(avant_refs.len(), 4);

    run_migrations(&db).unwrap();

    assert_eq!(
        lire(pistes),
        avant_pistes,
        "la migration a touché aux pistes"
    );
    assert_eq!(
        lire(refs),
        avant_refs,
        "une référence ne vise plus la même piste"
    );
    let conn = db.connection().lock().unwrap();
    let appliquee: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _migrations WHERE version = 110",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(appliquee, 1);
    let copies: i64 = conn
        .query_row("SELECT COUNT(*) FROM track_copies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        copies, 0,
        "la table naît vide : c'est le scan qui la remplit"
    );
}
