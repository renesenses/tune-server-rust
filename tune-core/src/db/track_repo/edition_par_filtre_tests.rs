//! #7539 — modifier un champ sur tout le résultat filtré d'Oxygen : plan,
//! transaction, plafond, cohérence `genre`/`genres`, pistes écartées.
//!
//! Base SQLite de FICHIER (#5043), dans un dossier `test_scratch`.
use super::*;
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::facet_filter::TrackFilter;
use crate::db::sqlite::SqliteDb;
use crate::test_scratch::{ScratchDir, scratch_dir};

struct Banc {
    db: Arc<dyn DbBackend>,
    _dir: ScratchDir,
}

fn exec(db: &Arc<dyn DbBackend>, sql: &str) {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

fn un_texte(db: &Arc<dyn DbBackend>, sql: &str) -> Option<String> {
    db.query_one(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .and_then(|r| r.first().and_then(|v| v.as_string()))
}

fn un_entier(db: &Arc<dyn DbBackend>, sql: &str) -> Option<i64> {
    db.query_one(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
}

/// Deux albums locaux et un distant.
///
/// - album 1 « Ella » : pistes 1 et 2, genre Swing7539 toutes deux ;
/// - album 2 « Miles » : piste 3 (Swing7539), piste 4 (Bop7539, HORS filtre) ;
/// - album 3 « Tidal » (source tidal) : piste 5 (Swing7539, distante) ;
/// - piste 6 : album 2, Swing7539, feuille CUE (base oui, fichier non).
///
/// Le filtre `genre=Swing7539` désigne 1, 2, 3, 5 et 6.
fn banc() -> Banc {
    let dir = scratch_dir("edition-par-filtre-7539");
    let sqlite = SqliteDb::open(dir.join("tune.db").to_str().unwrap()).unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    exec(
        &db,
        "INSERT INTO artists (id, name) VALUES (1, 'Ella Fitzgerald'), (2, 'Miles Davis')",
    );
    exec(
        &db,
        "INSERT INTO albums (id, title, artist_id, genre, genres, year, label, source) VALUES \
         (1, 'Ella', 1, 'Swing7539', '[\"Swing7539\"]', 1956, 'Verve', 'local'), \
         (2, 'Miles', 2, 'Swing7539', '[\"Swing7539\"]', 1959, 'Columbia', 'local'), \
         (3, 'Tidal', 2, 'Swing7539', '[\"Swing7539\"]', 1960, 'Columbia', 'tidal')",
    );
    for (id, album, genre, source, chemin, cue) in [
        (1, 1, "Swing7539", "local", "'/m/7539/1.flac'", "NULL"),
        (2, 1, "Swing7539", "local", "'/m/7539/2.flac'", "NULL"),
        (3, 2, "Swing7539", "local", "'/m/7539/3.flac'", "NULL"),
        (4, 2, "Bop7539", "local", "'/m/7539/4.flac'", "NULL"),
        (5, 3, "Swing7539", "tidal", "NULL", "NULL"),
        (6, 2, "Swing7539", "local", "NULL", "'/m/7539/image.flac'"),
    ] {
        exec(
            &db,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, cue_media_path, \
                 cue_start_ms, duration_ms, format, genre, genres, year, source, sample_rate) \
                 VALUES ({id}, 'Piste {id}', {album}, 1, {chemin}, {cue}, 0, 1000, 'flac', \
                 '{genre}', '[\"{genre}\"]', 1956, '{source}', 44100)"
            ),
        );
    }
    Banc { db, _dir: dir }
}

fn filtre(genre: &str) -> TrackFilter {
    TrackFilter {
        genres: vec![genre.to_string()],
        ..Default::default()
    }
}

fn texte(s: &str) -> ValeurDeMasse {
    ValeurDeMasse::Texte(s.to_string())
}

#[test]
fn la_valeur_est_validee() {
    assert_eq!(
        valider_valeur(ChampDeMasse::Annee, "1959"),
        Ok(ValeurDeMasse::Annee(1959))
    );
    for faux in ["59", "19590", "19a9", "0999", " "] {
        assert!(
            valider_valeur(ChampDeMasse::Annee, faux).is_err(),
            "année {faux:?} acceptée"
        );
    }
    assert_eq!(
        valider_valeur(ChampDeMasse::Genre, "  "),
        Err(RefusEdition::ValeurVide)
    );
    assert!(matches!(
        valider_valeur(ChampDeMasse::Label, &"x".repeat(LONGUEUR_MAX + 1)),
        Err(RefusEdition::ValeurTropLongue { .. })
    ));
}

#[test]
fn un_filtre_vide_est_refuse_et_rien_n_est_ecrit() {
    let b = banc();
    let repo = TrackRepo::with_backend(b.db.clone());
    let r = repo
        .appliquer_edition_par_filtre(
            &TrackFilter::default(),
            CibleDuFiltre::Pistes,
            ChampDeMasse::Label,
            texte("Blue Note"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.unwrap_err(), RefusEdition::FiltreVide);
    assert_eq!(
        un_entier(
            &b.db,
            "SELECT COUNT(*) FROM tracks WHERE label = 'Blue Note'"
        ),
        Some(0)
    );
}

/// Cible pistes, genre : `genre` = premier, `genres` = tableau ; l'album 1
/// (toutes ses pistes dans le filtre) change, l'album 2 (piste 4 hors
/// filtre, autre genre) NON ; la piste distante est écartée et comptée.
#[test]
fn genre_en_cible_pistes_reste_coherent() {
    let b = banc();
    let repo = TrackRepo::with_backend(b.db.clone());
    let f = filtre("Swing7539");
    let plan = repo
        .planifier_edition_par_filtre(
            &f,
            CibleDuFiltre::Pistes,
            ChampDeMasse::Genre,
            texte("Jazz; Vocal"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(plan.pistes_changees, vec![1, 2, 3, 6], "pistes changées");
    assert_eq!(plan.albums_changes, vec![1], "albums changés");
    assert_eq!(plan.pistes_distantes, 1, "la piste distante est comptée");
    assert_eq!(plan.fichiers.len(), 3, "trois fichiers propres");
    assert_eq!(plan.fichiers_cue, 1, "la piste CUE ne réécrit pas l'image");
    assert_eq!(plan.exemples_pistes[0].before.as_deref(), Some("Swing7539"));
    assert_eq!(plan.exemples_pistes[0].after, "Jazz; Vocal");
    // Le plan n'a rien écrit.
    assert_eq!(
        un_texte(&b.db, "SELECT genre FROM tracks WHERE id = 1").as_deref(),
        Some("Swing7539")
    );

    let bilan = repo
        .appliquer_edition_par_filtre(
            &f,
            CibleDuFiltre::Pistes,
            ChampDeMasse::Genre,
            texte("Jazz; Vocal"),
            None,
            Some((4, 1)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(bilan.pistes_changees.len(), 4);
    for id in [1, 2, 3, 6] {
        assert_eq!(
            un_texte(&b.db, &format!("SELECT genre FROM tracks WHERE id = {id}")).as_deref(),
            Some("Jazz"),
            "tracks.genre de {id} : le PREMIER genre"
        );
        assert_eq!(
            un_texte(&b.db, &format!("SELECT genres FROM tracks WHERE id = {id}")).as_deref(),
            Some(r#"["Jazz","Vocal"]"#),
            "tracks.genres de {id} : tous les genres"
        );
    }
    assert_eq!(
        un_texte(&b.db, "SELECT genre FROM albums WHERE id = 1").as_deref(),
        Some("Jazz; Vocal")
    );
    assert_eq!(
        un_texte(&b.db, "SELECT genres FROM albums WHERE id = 1").as_deref(),
        Some(r#"["Jazz","Vocal"]"#)
    );
    assert_eq!(
        un_texte(&b.db, "SELECT genre FROM albums WHERE id = 2").as_deref(),
        Some("Swing7539"),
        "album 2 : sa piste 4 n'a pas le nouveau genre"
    );
    assert_eq!(
        un_texte(&b.db, "SELECT genre FROM tracks WHERE id = 4").as_deref(),
        Some("Bop7539"),
        "piste hors filtre intacte"
    );
    assert_eq!(
        un_texte(&b.db, "SELECT genre FROM tracks WHERE id = 5").as_deref(),
        Some("Swing7539"),
        "piste distante intacte"
    );
    // Tenues et marqueurs : l'analyse suivante ne défait rien.
    let tenue = crate::db::champs_tenus::de_la_piste(&b.db, 1).expect("tenue de la piste 1");
    assert_eq!(tenue.genre.as_deref(), Some("Jazz"));
    assert_eq!(tenue.genres.as_deref(), Some(r#"["Jazz","Vocal"]"#));
    assert_eq!(tenue.chemin.as_deref(), Some("/m/7539/1.flac"));
    let marque = un_texte(
        &b.db,
        "SELECT value FROM album_metadata WHERE album_id = 1 AND key = 'edition_manuelle'",
    );
    assert_eq!(marque.as_deref(), Some(r#"["genre"]"#));
    let journal = un_texte(
        &b.db,
        "SELECT value FROM settings WHERE key = 'edit_by_filter_journal'",
    )
    .expect("journal");
    assert!(journal.contains("\"tracks\":4"), "journal : {journal}");

    // Relancer : plus rien à changer.
    let plan = repo
        .planifier_edition_par_filtre(
            &filtre("Jazz"),
            CibleDuFiltre::Pistes,
            ChampDeMasse::Genre,
            texte("Jazz; Vocal"),
        )
        .unwrap()
        .unwrap();
    assert!(plan.pistes_changees.is_empty(), "{plan:?}");
    assert_eq!(plan.pistes_deja, 4);
}

/// Cible albums : toutes les pistes des albums du résultat, piste 4 comprise.
#[test]
fn annee_en_cible_albums_touche_toutes_leurs_pistes() {
    let b = banc();
    let repo = TrackRepo::with_backend(b.db.clone());
    let bilan = repo
        .appliquer_edition_par_filtre(
            &filtre("Swing7539"),
            CibleDuFiltre::Albums,
            ChampDeMasse::Annee,
            ValeurDeMasse::Annee(1961),
            None,
            None,
        )
        .unwrap()
        .unwrap();
    assert_eq!(bilan.pistes_changees, vec![1, 2, 3, 4, 6]);
    assert_eq!(bilan.albums_changes, vec![1, 2]);
    assert_eq!(bilan.albums_distants, 1);
    assert_eq!(
        un_entier(&b.db, "SELECT year FROM tracks WHERE id = 4"),
        Some(1961)
    );
    assert_eq!(
        un_entier(&b.db, "SELECT year FROM albums WHERE id = 2"),
        Some(1961)
    );
    assert_eq!(
        un_entier(&b.db, "SELECT year FROM albums WHERE id = 3"),
        Some(1960)
    );
    assert_eq!(
        un_entier(&b.db, "SELECT year FROM tracks WHERE id = 5"),
        Some(1956)
    );
}

/// L'artiste d'album se pose sur les albums, même demandé en cible pistes.
#[test]
fn artiste_d_album_porte_sur_les_albums() {
    let b = banc();
    exec(
        &b.db,
        "INSERT INTO artists (id, name) VALUES (3, 'Miles Davis Quintet')",
    );
    let repo = TrackRepo::with_backend(b.db.clone());
    let bilan = repo
        .appliquer_edition_par_filtre(
            &filtre("Bop7539"),
            CibleDuFiltre::Pistes,
            ChampDeMasse::ArtisteAlbum,
            texte("Miles Davis Quintet"),
            Some(3),
            None,
        )
        .unwrap()
        .unwrap();
    // Le filtre désigne la piste 4 → son album 2 → les pistes 3, 4, 6.
    assert_eq!(bilan.pistes_changees, vec![3, 4, 6]);
    assert_eq!(bilan.albums_changes, vec![2]);
    assert_eq!(
        un_entier(&b.db, "SELECT artist_id FROM albums WHERE id = 2"),
        Some(3)
    );
    assert_eq!(
        un_entier(&b.db, "SELECT artist_id FROM albums WHERE id = 1"),
        Some(1)
    );
    assert_eq!(
        un_texte(&b.db, "SELECT album_artist FROM tracks WHERE id = 3").as_deref(),
        Some("Miles Davis Quintet")
    );
}

/// 🔴 La transaction : un échec à l'écriture des ALBUMS (déclencheur qui
/// refuse) laisse les PISTES telles qu'elles étaient — tout passe, ou rien.
#[test]
fn un_echec_en_cours_de_route_ne_laisse_rien_d_ecrit() {
    let b = banc();
    exec(
        &b.db,
        "CREATE TRIGGER refus_7539 BEFORE UPDATE OF label ON albums \
         BEGIN SELECT RAISE(ABORT, 'refus 7539'); END",
    );
    let repo = TrackRepo::with_backend(b.db.clone());
    let r = repo.appliquer_edition_par_filtre(
        &filtre("Swing7539"),
        CibleDuFiltre::Albums,
        ChampDeMasse::Label,
        texte("Blue Note"),
        None,
        None,
    );
    assert!(r.is_err(), "le déclencheur devait faire échouer : {r:?}");
    assert_eq!(
        un_entier(
            &b.db,
            "SELECT COUNT(*) FROM tracks WHERE label = 'Blue Note'"
        ),
        Some(0),
        "des pistes ont gardé le label malgré l'échec : la modification n'est pas atomique"
    );
    assert_eq!(
        un_entier(
            &b.db,
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'champs_tenus'"
        ),
        Some(0),
        "des tenues ont survécu à l'échec"
    );
}

/// L'aperçu confirmé ne correspond plus : rien n'est écrit.
#[test]
fn un_apercu_perime_est_refuse() {
    let b = banc();
    let repo = TrackRepo::with_backend(b.db.clone());
    let r = repo
        .appliquer_edition_par_filtre(
            &filtre("Swing7539"),
            CibleDuFiltre::Pistes,
            ChampDeMasse::Compositeur,
            texte("Duke Ellington"),
            None,
            Some((3, 0)),
        )
        .unwrap();
    assert_eq!(
        r.unwrap_err(),
        RefusEdition::ApercuPerime {
            pistes: 4,
            albums: 0
        }
    );
    assert_eq!(
        un_entier(
            &b.db,
            "SELECT COUNT(*) FROM tracks WHERE composer = 'Duke Ellington'"
        ),
        Some(0)
    );
}

/// 🔴 Le plafond : au-delà, refus AVANT toute écriture, et l'aperçu le dit.
#[test]
fn au_dela_du_plafond_rien_n_est_ecrit() {
    let b = banc();
    exec(
        &b.db,
        &format!(
            "INSERT INTO tracks (title, file_path, duration_ms, format, genre, sample_rate) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {}) \
             SELECT 'Masse', '/m/m7539/' || CAST(i AS TEXT) || '.flac', 1000, 'flac', \
             'Masse7539', 44100 FROM n",
            PLAFOND_EDITION_PAR_FILTRE + 1
        ),
    );
    let repo = TrackRepo::with_backend(b.db.clone());
    let f = filtre("Masse7539");
    let attendu = RefusEdition::TropDePistes {
        nombre: PLAFOND_EDITION_PAR_FILTRE + 1,
        plafond: PLAFOND_EDITION_PAR_FILTRE,
    };
    assert_eq!(
        repo.planifier_edition_par_filtre(
            &f,
            CibleDuFiltre::Pistes,
            ChampDeMasse::Label,
            texte("X")
        )
        .unwrap()
        .unwrap_err(),
        attendu
    );
    assert_eq!(
        repo.appliquer_edition_par_filtre(
            &f,
            CibleDuFiltre::Pistes,
            ChampDeMasse::Label,
            texte("X"),
            None,
            None
        )
        .unwrap()
        .unwrap_err(),
        attendu,
        "le plafond doit refuser l'écriture"
    );
    assert_eq!(
        un_entier(&b.db, "SELECT COUNT(*) FROM tracks WHERE label = 'X'"),
        Some(0),
        "le plafond a laissé passer des lignes"
    );
}
