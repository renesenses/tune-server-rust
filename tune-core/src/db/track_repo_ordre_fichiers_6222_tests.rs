//! #6222 (fil 2247) — un album d'un seul disque à numéros de piste en double
//! se lit dans l'ordre naturel de ses fichiers ; tout autre album garde
//! « disque, numéro, titre ».
//!
//! Chaque banc passe par les QUATRE chemins qui rangent les pistes d'un
//! album : la fiche ([`TrackRepo::list_by_album`], et
//! [`TrackRepo::list_by_album_filtered`] sous un filtre de qualité), « jouer
//! l'album » (le même `list_by_album` suivi de [`dedup_display_tracks`]) et
//! le serveur média ([`TrackRepo::ids_by_album_ids`]).

use super::*;

/// `(id, titre, disque, numéro, fichier)`.
type Piste = (i64, &'static str, Option<i64>, Option<i64>, &'static str);

fn banc(albums: &[(i64, &[Piste])]) -> (Arc<SqliteDb>, TrackRepo) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let sqlite = Arc::new(db);
    let db: Arc<dyn DbBackend> = sqlite.clone();
    for (album, pistes) in albums {
        db.execute(
            "INSERT INTO albums (id, title) VALUES (?1, 'Album')",
            &[album as &dyn ToSqlValue],
        )
        .unwrap();
        for (id, titre, disque, numero, fichier) in pistes.iter() {
            db.execute(
                "INSERT INTO tracks (id, title, album_id, disc_number, track_number, \
                 file_path, format, sample_rate, bit_depth, source, duration_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'flac', 44100, 16, 'local', 1000)",
                &[
                    id as &dyn ToSqlValue,
                    &titre.to_string(),
                    album,
                    disque,
                    numero,
                    &fichier.to_string(),
                ],
            )
            .unwrap();
        }
    }
    (sqlite, TrackRepo::with_backend(db))
}

fn ids(pistes: &[Track]) -> Vec<i64> {
    pistes.iter().filter_map(|t| t.id).collect()
}

/// Les quatre ordres d'un album : fiche, fiche filtrée, lecture, serveur média.
fn ordres(repo: &TrackRepo, album: i64) -> [Vec<i64>; 4] {
    let fiche = ids(&repo.list_by_album(album).unwrap());
    let filtree = ids(&repo
        .list_by_album_filtered(album, Some("flac"), None)
        .unwrap());
    let jouee = ids(&dedup_display_tracks(repo.list_by_album(album).unwrap()));
    let upnp = repo
        .ids_by_album_ids(&[album])
        .unwrap()
        .into_iter()
        .map(|(piste, _)| piste)
        .collect();
    [fiche, filtree, jouee, upnp]
}

fn assert_ordre(repo: &TrackRepo, album: i64, attendu: &[i64], pourquoi: &str) {
    let [fiche, filtree, jouee, upnp] = ordres(repo, album);
    assert_eq!(fiche, attendu, "fiche album : {pourquoi}");
    assert_eq!(filtree, attendu, "fiche filtrée : {pourquoi}");
    assert_eq!(jouee, attendu, "« jouer l'album » : {pourquoi}");
    assert_eq!(upnp, attendu, "serveur média : {pourquoi}");
}

/// Le cas du testeur : un dossier de fichiers repris de plusieurs albums,
/// chacun avec son numéro d'origine — 1, 1, 1, 2, 2.
#[test]
fn une_compilation_personnelle_a_numeros_en_double_suit_l_ordre_des_fichiers_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (
                1,
                "Ziggy",
                Some(1),
                Some(1),
                "/musique/Compil/01 - Ziggy.wav",
            ),
            (
                2,
                "Angie",
                Some(1),
                Some(2),
                "/musique/Compil/02 - Angie.wav",
            ),
            (
                3,
                "Bohemian",
                None,
                Some(1),
                "/musique/Compil/03 - Bohemian.wav",
            ),
            (
                4,
                "Creep",
                Some(1),
                Some(2),
                "/musique/Compil/04 - Creep.wav",
            ),
            (
                5,
                "Africa",
                Some(0),
                Some(1),
                "/musique/Compil/05 - Africa.wav",
            ),
        ],
    )]);
    assert_ordre(
        &repo,
        1,
        &[1, 2, 3, 4, 5],
        "sans le correctif, disque puis numéro puis titre donne 5, 3, 1, 2, 4 \
         (« 1 Africa, 1 Bohemian, 1 Ziggy, 2 Angie, 2 Creep »)",
    );
}

/// L'ordre naturel de l'explorateur : « 2 » avant « 10 », sans casse.
#[test]
fn l_ordre_des_fichiers_est_naturel_et_sans_casse_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (1, "Dix", Some(1), Some(1), "/musique/Compil/10 - dix.flac"),
            (2, "Deux", Some(1), Some(1), "/musique/Compil/2 - deux.flac"),
            (3, "Un b", Some(1), Some(3), "/musique/Compil/1 - b.flac"),
            (4, "Un A", Some(1), Some(2), "/musique/Compil/1 - A.flac"),
        ],
    )]);
    assert_ordre(
        &repo,
        1,
        &[4, 3, 2, 1],
        "ordre naturel attendu : 1 - A, 1 - b, 2 - deux, 10 - dix",
    );
}

/// Un album normal (numéros uniques) garde son ordre, même quand ses noms de
/// fichiers se rangeraient autrement.
#[test]
fn un_album_sans_numero_en_double_ne_change_pas_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (1, "Trois", Some(1), Some(3), "/musique/Album/a.flac"),
            (2, "Un", Some(1), Some(1), "/musique/Album/c.flac"),
            (3, "Deux", Some(1), Some(2), "/musique/Album/b.flac"),
            (4, "Sans numéro", Some(1), None, "/musique/Album/0.flac"),
        ],
    )]);
    assert_ordre(&repo, 1, &[2, 3, 1, 4], "disque, numéro, titre inchangés");
}

/// Un coffret de deux disques : les numéros se répètent d'un CD à l'autre,
/// c'est attendu (#4471), l'ordre reste disque puis numéro.
#[test]
fn un_coffret_de_deux_disques_ne_change_pas_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (
                1,
                "CD2 piste 1",
                Some(2),
                Some(1),
                "/musique/Coffret/a.flac",
            ),
            (
                2,
                "CD1 piste 2",
                Some(1),
                Some(2),
                "/musique/Coffret/b.flac",
            ),
            (
                3,
                "CD2 piste 2",
                Some(2),
                Some(2),
                "/musique/Coffret/c.flac",
            ),
            (
                4,
                "CD1 piste 1",
                Some(1),
                Some(1),
                "/musique/Coffret/d.flac",
            ),
        ],
    )]);
    assert_ordre(
        &repo,
        1,
        &[4, 2, 1, 3],
        "un coffret garde son ordre par disque",
    );
}

/// Deux copies du même morceau (même numéro, même titre : le rip AIFF et la
/// copie AAC de #1362) ne font pas d'un album normal une compilation : l'ordre
/// ne bouge pas et le repli de l'écran tient.
#[test]
fn deux_copies_du_meme_morceau_ne_declenchent_pas_l_ordre_des_fichiers_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (1, "Deux", Some(1), Some(2), "/musique/Album/a.flac"),
            (2, "Un", Some(1), Some(1), "/musique/Album/z.flac"),
            (3, "un ", Some(1), Some(1), "/musique/Album/b.m4a"),
        ],
    )]);
    let fiche = ids(&repo.list_by_album(1).unwrap());
    assert_eq!(
        fiche,
        [2, 3, 1],
        "numéro d'abord : l'album n'est pas une compilation"
    );
    let affichee = ids(&dedup_display_tracks(repo.list_by_album(1).unwrap()));
    assert_eq!(
        affichee,
        [2, 1],
        "les deux copies de « Un » se replient toujours"
    );
}

/// Dans une compilation, deux versions d'un même morceau reprises de deux
/// albums portent le même numéro et le même titre : leurs fichiers diffèrent,
/// aucune ne disparaît de la fiche ni de la lecture.
#[test]
fn dans_une_compilation_deux_versions_du_meme_morceau_restent_visibles_6222() {
    let (_s, repo) = banc(&[(
        1,
        &[
            (
                1,
                "Yesterday",
                Some(1),
                Some(13),
                "/musique/Compil/01 - Beatles.flac",
            ),
            (
                2,
                "Angie",
                Some(1),
                Some(1),
                "/musique/Compil/02 - Angie.flac",
            ),
            (
                3,
                "Yesterday",
                Some(1),
                Some(13),
                "/musique/Compil/03 - Ray Charles.flac",
            ),
            (
                4,
                "Creep",
                Some(1),
                Some(1),
                "/musique/Compil/04 - Creep.flac",
            ),
        ],
    )]);
    assert_ordre(
        &repo,
        1,
        &[1, 2, 3, 4],
        "sans le correctif, les deux « Yesterday » n° 13 se repliaient en une",
    );
}

/// La règle elle-même, sans base.
#[test]
fn la_regle_de_detection_6222() {
    // Disques NULL/0/1 : un seul disque.
    assert!(album_en_ordre_de_fichiers([(0, 1, "a"), (1, 1, "b")]));
    // Deux disques : jamais.
    assert!(!album_en_ordre_de_fichiers([
        (1, 1, "a"),
        (1, 1, "b"),
        (2, 1, "c")
    ]));
    // Un seul disque, mais numéroté 2 : toujours un seul disque.
    assert!(album_en_ordre_de_fichiers([(2, 1, "a"), (2, 1, "b")]));
    // Pistes sans numéro : pas un doublon.
    assert!(!album_en_ordre_de_fichiers([
        (1, 0, "a"),
        (1, 0, "b"),
        (1, 1, "c")
    ]));
    // Même numéro, même titre (casse et blancs ignorés) : des copies.
    assert!(!album_en_ordre_de_fichiers([
        (1, 1, "Titre"),
        (1, 1, " titre ")
    ]));
    // Même numéro, titres différents après une copie : un doublon.
    assert!(album_en_ordre_de_fichiers([
        (1, 1, "a"),
        (1, 1, "A"),
        (1, 1, "b")
    ]));
    // Liste vide.
    assert!(!album_en_ordre_de_fichiers(std::iter::empty()));
}
