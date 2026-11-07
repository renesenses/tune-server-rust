//! #6285, deuxième étape — le verdict publié : la règle Rust de l'album
//! ([`publier_l_album`]) et son écriture SQL ([`predicat_album`],
//! [`predicat_piste`]) rendent les MÊMES albums et les mêmes pistes, « douteux »
//! masqué comme montré.

use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;

/// | album | pistes mesurées | verdict de l'album |
/// |---|---|---|
/// | 1 | vérifiée, vérifiée | vérifié |
/// | 2 | vérifiée, conforme | conforme |
/// | 3 | vérifiée, douteuse (bits de remplissage) | douteux |
/// | 4 | aucune (une piste jamais mesurée) | non analysé |
/// | 5 | non concluante | non analysé |
/// | 6 | vérifiée, non concluante, une piste jamais mesurée | vérifié |
/// | 7 | conforme | conforme |
pub(crate) fn bibliotheque() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    for album in 1..=7i64 {
        db.execute(
            "INSERT INTO albums (id, title) VALUES (?1, 'a')",
            &[&album as &dyn ToSqlValue],
        )
        .unwrap();
    }
    // (piste, album, mesure : verdict et raison)
    type Piste<'a> = (i64, i64, Option<(&'a str, Option<&'a str>)>);
    let pistes: &[Piste] = &[
        (11, 1, Some(("verifie", None))),
        (12, 1, Some(("verifie", None))),
        (21, 2, Some(("verifie", None))),
        (22, 2, Some(("conforme", None))),
        (31, 3, Some(("verifie", None))),
        (32, 3, Some(("douteux", Some("bits_de_remplissage")))),
        (41, 4, None),
        (51, 5, Some(("non_concluant", None))),
        (61, 6, Some(("verifie", None))),
        (62, 6, Some(("non_concluant", None))),
        (63, 6, None),
        (71, 7, Some(("conforme", None))),
    ];
    for (piste, album, mesure) in pistes {
        let chemin = format!("/m/{piste}.flac");
        db.execute(
            "INSERT INTO tracks (id, album_id, title, file_path, format, source, duration_ms) \
             VALUES (?1, ?2, 't', ?3, 'flac', 'local', 60000)",
            &[piste as &dyn ToSqlValue, album, &chemin],
        )
        .unwrap();
        if let Some((verdict, raison)) = mesure {
            let verdict = verdict.to_string();
            let raison = raison.map(str::to_string);
            db.execute(
                "INSERT INTO hires_verifications (track_id, verdict, raison) VALUES (?1, ?2, ?3)",
                &[piste as &dyn ToSqlValue, &verdict, &raison],
            )
            .unwrap();
        }
    }
    db
}

const TOUS: [VerdictPublie; 4] = [
    VerdictPublie::Verifie,
    VerdictPublie::Conforme,
    VerdictPublie::Douteux,
    VerdictPublie::NonAnalyse,
];

fn albums_du_predicat(db: &Arc<dyn DbBackend>, v: VerdictPublie, visible: bool) -> Vec<i64> {
    let sql = format!(
        "SELECT a.id FROM albums a WHERE {} ORDER BY a.id",
        predicat_album("a.id", v, visible)
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .filter_map(|r| r[0].as_i64())
        .collect()
}

fn pistes_du_predicat(db: &Arc<dyn DbBackend>, v: VerdictPublie, visible: bool) -> Vec<i64> {
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE {} ORDER BY t.id",
        predicat_piste("t.id", v, visible)
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .filter_map(|r| r[0].as_i64())
        .collect()
}

/// Ce que publie la règle Rust pour chaque album de la bibliothèque.
fn verdicts_publies(db: &Arc<dyn DbBackend>, visible: bool) -> Vec<(i64, &'static str)> {
    let ids: Vec<i64> = (1..=7).collect();
    let par_album = pistes_mesurees_par_album(db, &ids).unwrap();
    ids.iter()
        .map(|id| {
            let pistes = par_album.get(id).cloned().unwrap_or_default();
            (*id, publier_l_album(&pistes, visible).0)
        })
        .collect()
}

#[test]
fn le_verdict_publie_d_un_album_suit_ses_pistes_6285() {
    let db = bibliotheque();
    assert_eq!(
        verdicts_publies(&db, true),
        vec![
            (1, "verifie"),
            (2, "conforme"),
            (3, "douteux"),
            (4, "non_analyse"),
            (5, "non_analyse"),
            (6, "verifie"),
            (7, "conforme"),
        ]
    );
    let par_album = pistes_mesurees_par_album(&db, &[3]).unwrap();
    assert_eq!(
        publier_l_album(&par_album[&3], true),
        ("douteux", Some("bits_de_remplissage".to_string()))
    );
}

/// 🔴 Masqué par défaut : sans réglage, l'album douteux se publie « non
/// analysé », sans raison — rien ne le distingue d'un album pas mesuré.
#[test]
fn douteux_est_masque_par_defaut_6285() {
    let db = bibliotheque();
    assert!(
        !douteux_visible(&db),
        "« douteux » doit être masqué par défaut"
    );
    let visible = douteux_visible(&db);
    let publies = verdicts_publies(&db, visible);
    assert_eq!(
        publies[2],
        (3, "non_analyse"),
        "l'album douteux ne doit pas apparaître tant que le réglage n'est pas posé"
    );
    assert!(publies.iter().all(|(_, v)| *v != "douteux"));
    let par_album = pistes_mesurees_par_album(&db, &[3]).unwrap();
    assert_eq!(publier_l_album(&par_album[&3], visible).1, None);
    assert!(albums_du_predicat(&db, VerdictPublie::Douteux, visible).is_empty());
    assert!(pistes_du_predicat(&db, VerdictPublie::Douteux, visible).is_empty());

    montrer_douteux(&db, true).unwrap();
    assert!(douteux_visible(&db));
    assert_eq!(
        albums_du_predicat(&db, VerdictPublie::Douteux, true),
        vec![3]
    );
    montrer_douteux(&db, false).unwrap();
    assert!(!douteux_visible(&db));
}

/// 🔴 Le filtre rend EXACTEMENT les albums qui portent le badge : pour chaque
/// verdict, et dans les deux réglages, le prédicat SQL et la règle Rust
/// désignent les mêmes albums.
#[test]
fn le_filtre_rend_les_albums_du_badge_6285() {
    let db = bibliotheque();
    for visible in [false, true] {
        let publies = verdicts_publies(&db, visible);
        for v in TOUS {
            let attendus: Vec<i64> = publies
                .iter()
                .filter(|(_, p)| *p == v.code())
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(
                albums_du_predicat(&db, v, visible),
                attendus,
                "filtre {} (douteux visible : {visible})",
                v.code()
            );
        }
    }
    assert_eq!(
        albums_du_predicat(&db, VerdictPublie::Verifie, false),
        vec![1, 6]
    );
    assert_eq!(
        albums_du_predicat(&db, VerdictPublie::NonAnalyse, false),
        vec![3, 4, 5]
    );
}

#[test]
fn le_verdict_d_une_piste_6285() {
    let db = bibliotheque();
    let v = |verdict, visible| pistes_du_predicat(&db, verdict, visible);
    assert_eq!(v(VerdictPublie::Verifie, false), vec![11, 12, 21, 31, 61]);
    assert_eq!(v(VerdictPublie::Conforme, false), vec![22, 71]);
    assert_eq!(v(VerdictPublie::Douteux, true), vec![32]);
    assert_eq!(v(VerdictPublie::NonAnalyse, true), vec![41, 51, 62, 63]);
    assert_eq!(
        v(VerdictPublie::NonAnalyse, false),
        vec![32, 41, 51, 62, 63],
        "masquée, la piste douteuse est non analysée"
    );
}

#[test]
fn les_mots_du_filtre_6285() {
    for v in TOUS {
        assert_eq!(VerdictPublie::lire(v.code()), Some(v));
    }
    assert_eq!(
        VerdictPublie::lire(" Verifie "),
        Some(VerdictPublie::Verifie)
    );
    assert_eq!(VerdictPublie::lire("non_concluant"), None);
    assert_eq!(VerdictPublie::lire("hires"), None);
}

/// Le dépôt des albums applique le filtre à ses listages, et le total de la
/// page est celui de l'ensemble FILTRÉ.
#[test]
fn la_liste_des_albums_se_filtre_sur_le_verdict_6285() {
    use crate::db::album_repo::AlbumRepo;
    let db = bibliotheque();
    let lister = |mot: &str, visible: bool| {
        let repo = AlbumRepo::with_backend(db.clone())
            .avec_filtre_hires(Some(FiltreHires::depuis_le_mot(mot, visible)));
        let (albums, total) = repo
            .list_filtered_seeded_avec_total(
                50, 0, "added_at", "asc", None, None, None, false, None, None,
            )
            .unwrap();
        let mut ids: Vec<i64> = albums.iter().filter_map(|a| a.id).collect();
        ids.sort();
        (ids, total)
    };
    assert_eq!(lister("verifie", false), (vec![1, 6], Some(2)));
    assert_eq!(lister("conforme", false), (vec![2, 7], Some(2)));
    assert_eq!(lister("douteux", false), (vec![], None));
    assert_eq!(lister("douteux", true), (vec![3], Some(1)));
    assert_eq!(lister("non_analyse", false), (vec![3, 4, 5], Some(3)));
    assert_eq!(lister("non_analyse", true), (vec![4, 5], Some(2)));
    // Un mot inconnu ne rend rien, jamais toute la bibliothèque.
    assert_eq!(lister("hires", true), (vec![], None));
    let tout = AlbumRepo::with_backend(db.clone())
        .list_filtered(50, 0, "title", "asc", None, None, None, false, None)
        .unwrap();
    assert_eq!(tout.len(), 7, "sans filtre, toute la bibliothèque");
}
