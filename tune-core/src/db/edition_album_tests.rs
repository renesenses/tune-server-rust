//! Témoins de l'édition d'album (mode « Modifier », GO du 25/09/2026).
//!
//! Chaque scénario est écrit UNE fois, sur `Arc<dyn DbBackend>` : SQLite le
//! joue ici, PostgreSQL le rejoue dans `postgres_e2e.rs`
//! (`pg_edition_album_…`). Deux copies divergeraient à la première correction.
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::coffrets_auto;
use crate::db::models::{Album, Artist, Track};
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn sqlite() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Les magasins que `postgres_e2e::reset_schema` ne vide pas.
fn nettoyer(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let _ = SettingsRepo::with_backend(db.clone()).delete(coffrets_auto::CLE_REFUS);
}

fn artiste(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
    let repo = ArtistRepo::with_backend(db.clone());
    if let Some(id) = repo.get_by_name(nom).unwrap().and_then(|a| a.id) {
        return id;
    }
    repo.create(&Artist::new(nom.into())).unwrap()
}

fn album(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64, dossier: &str) -> i64 {
    let mut a = Album::new(titre.into());
    a.artist_id = Some(artiste);
    let repo = AlbumRepo::with_backend(db.clone());
    let id = repo.create(&a).unwrap();
    repo.set_folder_path(id, dossier).unwrap();
    id
}

#[allow(clippy::too_many_arguments)]
fn piste(
    db: &Arc<dyn DbBackend>,
    album: i64,
    artiste: i64,
    n: i32,
    disque: i32,
    nom_disque: Option<&str>,
    balise_album: Option<&str>,
    chemin: &str,
) -> i64 {
    let mut t = Track::new(format!("piste {n}"));
    t.album_id = Some(album);
    t.artist_id = Some(artiste);
    t.track_number = n;
    t.disc_number = disque;
    t.disc_subtitle = nom_disque.map(str::to_string);
    t.album_artist = balise_album.map(str::to_string);
    t.file_path = Some(chemin.to_string());
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

/// Un disque d'un coffret éclaté : un album et `n` pistes dans SON dossier.
fn disque(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64, dossier: &str, n: i32) -> Vec<i64> {
    let id = album(db, titre, artiste, dossier);
    let mut ids = vec![id];
    for k in 1..=n {
        ids.push(piste(
            db,
            id,
            artiste,
            k,
            1,
            None,
            None,
            &format!("{dossier}/{k:02}.flac"),
        ));
    }
    ids
}

fn vue(db: &Arc<dyn DbBackend>, id: i64) -> VueEdition {
    lire_vue(db, id).unwrap().expect("album présent")
}

fn modifier(
    db: &Arc<dyn DbBackend>,
    id: i64,
    corps: serde_json::Value,
) -> Result<(), RefusEdition> {
    let m: Modification = serde_json::from_value(corps).expect("corps conforme au contrat");
    appliquer(db, id, &m)
}

fn code(r: Result<(), RefusEdition>) -> &'static str {
    match r {
        Err(RefusEdition::Invalide { code, .. }) => code,
        autre => panic!("attendu un 422, obtenu {autre:?}"),
    }
}

/// `(id, disque, numéro)` de chaque piste, dans l'ordre de la vue.
fn places(v: &VueEdition) -> Vec<(i64, i32, i32)> {
    v.tracks
        .iter()
        .map(|t| (t.id, t.disc_number, t.track_number))
        .collect()
}

fn disques(v: &VueEdition) -> Vec<(i32, Option<&str>, i64)> {
    v.discs
        .iter()
        .map(|d| (d.number, d.title.as_deref(), d.track_count))
        .collect()
}

fn existe(db: &Arc<dyn DbBackend>, id: i64) -> bool {
    AlbumRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .is_some()
}

/// Le coffret de Keith Jarrett du banc : deux disques, le premier nommé.
pub(crate) struct Koln {
    pub album: i64,
    pub a: [i64; 3],
    pub b: [i64; 2],
}

pub(crate) fn poser_koln(db: &Arc<dyn DbBackend>) -> Koln {
    let kj = artiste(db, "Keith Jarrett");
    let al = album(db, "The Köln Box", kj, "/e/box/cd1");
    let a = [1, 2, 3].map(|n| {
        piste(
            db,
            al,
            kj,
            n,
            1,
            Some("Première partie"),
            None,
            &format!("/e/box/cd1/{n:02}.flac"),
        )
    });
    let b = [1, 2].map(|n| {
        piste(
            db,
            al,
            kj,
            n,
            2,
            None,
            None,
            &format!("/e/box/cd2/{n:02}.flac"),
        )
    });
    Koln { album: al, a, b }
}

/// GET → PUT → GET : champs, ordre des disques, noms, déplacement d'une piste
/// d'un disque à l'autre, titre et artiste de piste ; puis renommer un disque.
pub(crate) fn scenario_aller_retour(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let k = poser_koln(db);
    let [a1, a2, a3] = k.a;
    let [b1, b2] = k.b;

    let v = vue(db, k.album);
    assert_eq!(
        disques(&v),
        vec![(1, Some("Première partie"), 3), (2, None, 2)]
    );
    assert_eq!(
        places(&v),
        vec![(a1, 1, 1), (a2, 1, 2), (a3, 1, 3), (b1, 2, 1), (b2, 2, 2)]
    );
    assert_eq!(v.album.compilation_mode, "auto");
    assert_eq!(v.album.coffret, None);
    assert!(v.album.champs_edites.is_empty());
    assert_eq!(v.discs[0].cover_path, None, "pas de pochette par disque");

    // Le disque 2 passe en tête, nommé ; la piste a3 le rejoint, en dernier ;
    // le disque 1 (sans `title`) garde son nom.
    modifier(
        db,
        k.album,
        json!({
            "title": "Köln, le coffret",
            "album_artist": "Keith Jarrett Trio",
            "year": 1975,
            "label": "ECM",
            "genre": "Jazz",
            "release_type": "album",
            "discs": [
                { "number": 2, "title": "Live", "track_ids": [b2, b1, a3] },
                { "number": 1, "track_ids": [a1, a2] }
            ],
            "tracks": [ { "id": a1, "title": "Intro", "artist_name": "Invité" } ]
        }),
    )
    .unwrap();
    let v = vue(db, k.album);
    assert_eq!(v.album.title, "Köln, le coffret");
    assert_eq!(v.album.album_artist.as_deref(), Some("Keith Jarrett Trio"));
    assert_eq!(v.album.year, Some(1975));
    assert_eq!(v.album.label.as_deref(), Some("ECM"));
    assert_eq!(v.album.genre.as_deref(), Some("Jazz"));
    assert_eq!(v.album.release_type.as_deref(), Some("album"));
    assert_eq!(
        disques(&v),
        vec![(1, Some("Live"), 3), (2, Some("Première partie"), 2)]
    );
    assert_eq!(
        places(&v),
        vec![(b2, 1, 1), (b1, 1, 2), (a3, 1, 3), (a1, 2, 1), (a2, 2, 2)]
    );
    let t1 = v.tracks.iter().find(|t| t.id == a1).unwrap();
    assert_eq!(t1.title, "Intro");
    assert_eq!(t1.artist_name.as_deref(), Some("Invité"));
    assert_eq!(
        v.album.champs_edites,
        vec![
            "album_artist",
            "discs",
            "genre",
            "label",
            "release_type",
            "title",
            "tracks",
            "year"
        ]
    );

    // Renommer un disque, effacer le nom de l'autre (`null`).
    modifier(
        db,
        k.album,
        json!({ "discs": [
            { "number": 1, "title": "Concert", "track_ids": [b2, b1, a3] },
            { "number": 2, "title": null, "track_ids": [a1, a2] }
        ]}),
    )
    .unwrap();
    let v = vue(db, k.album);
    assert_eq!(disques(&v), vec![(1, Some("Concert"), 3), (2, None, 2)]);
    // Le titre de piste renommé plus tôt est toujours tenu.
    let tenues = Tenues::charger(db);
    let t = tenues.get("/e/box/cd1/01.flac").expect("piste tenue");
    assert_eq!(t.titre.as_deref(), Some("Intro"));
    assert_eq!(t.album_id, k.album);
    assert_eq!(t.disposition, Some((2, 1, None)));
}

/// Précision du contrat (web#1599) : un disque VIDÉ n'est pas envoyé — il
/// disparaît, les autres sont renumérotés 1..n. Une piste d'un disque absent
/// qui n'est listée nulle part reste un refus.
pub(crate) fn scenario_disque_vide(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let k = poser_koln(db);
    let [a1, a2, a3] = k.a;
    let [b1, b2] = k.b;

    // Contre-épreuve d'abord : le disque 2 absent, ses pistes nulle part.
    assert_eq!(
        code(modifier(
            db,
            k.album,
            json!({ "discs": [ { "number": 1, "track_ids": [a1, a2, a3] } ] }),
        )),
        "piste_manquante"
    );
    // Le disque 1 vidé vers le disque 2, et absent de la requête.
    modifier(
        db,
        k.album,
        json!({ "discs": [ { "number": 2, "track_ids": [b1, b2, a1, a2, a3] } ] }),
    )
    .unwrap();
    let v = vue(db, k.album);
    assert_eq!(disques(&v), vec![(1, None, 5)]);
    assert_eq!(
        places(&v),
        vec![(b1, 1, 1), (b2, 1, 2), (a1, 1, 3), (a2, 1, 4), (a3, 1, 5)]
    );
    // Envoyé VIDE, il disparaît aussi — sans décaler la numérotation.
    modifier(
        db,
        k.album,
        json!({ "discs": [
            { "number": 7, "track_ids": [] },
            { "number": 1, "title": "Tout", "track_ids": [a1, a2, a3, b1, b2] }
        ]}),
    )
    .unwrap();
    assert_eq!(disques(&vue(db, k.album)), vec![(1, Some("Tout"), 5)]);
}

/// Tout est vérifié AVANT d'écrire : un refus ne laisse rien derrière lui.
pub(crate) fn scenario_refus(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let k = poser_koln(db);
    let [a1, a2, a3] = k.a;
    let [b1, b2] = k.b;
    let ailleurs = disque(db, "Ailleurs", artiste(db, "X"), "/e/ailleurs", 1)[1];

    let cas = [
        (
            json!({ "title": "NON", "discs": [ { "number": 1, "track_ids": [a1, a2, a3, b1] } ] }),
            "piste_manquante",
        ),
        (
            json!({ "title": "NON", "discs": [
                { "number": 1, "track_ids": [a1, a2, a3, b1, b2] },
                { "number": 2, "track_ids": [b2] }
            ] }),
            "piste_en_double",
        ),
        (
            json!({ "title": "NON", "discs": [ { "number": 1, "track_ids": [a1, a2, a3, b1, b2, ailleurs] } ] }),
            "piste_etrangere",
        ),
        (
            json!({ "title": "NON", "tracks": [ { "id": ailleurs, "title": "x" } ] }),
            "piste_etrangere",
        ),
        (
            json!({ "title": "NON", "compilation_mode": "peut-être" }),
            "mode_de_compilation_inconnu",
        ),
        (json!({ "title": "   " }), "titre_vide"),
        (
            json!({ "title": "NON", "tracks": [ { "id": a1, "title": "" } ] }),
            "titre_de_piste_vide",
        ),
        (json!({ "title": "NON", "discs": [] }), "disques_vides"),
    ];
    for (corps, attendu) in cas {
        assert_eq!(
            code(modifier(db, k.album, corps.clone())),
            attendu,
            "{corps}"
        );
    }
    let v = vue(db, k.album);
    assert_eq!(v.album.title, "The Köln Box", "rien d'écrit");
    assert!(v.album.champs_edites.is_empty(), "aucun marqueur posé");
    assert_eq!(
        places(&v),
        vec![(a1, 1, 1), (a2, 1, 2), (a3, 1, 3), (b1, 2, 1), (b2, 2, 2)]
    );
    assert!(Tenues::charger(db).is_empty());
    assert_eq!(
        modifier(db, 987_654, json!({ "title": "x" })),
        Err(RefusEdition::AlbumInconnu(987_654))
    );
}

/// `compilation_mode` : `oui` et `non` tiennent face au scan et au recalcul
/// de #5011 ; `auto` rend la main à LA règle.
pub(crate) fn scenario_compilation(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let repo = AlbumRepo::with_backend(db.clone());
    let a = artiste(db, "David Sanborn");
    let b = artiste(db, "Django Reinhardt");
    let c = artiste(db, "Stéphane Grappelli");
    let va = artiste(db, "Various Artists");

    // Deux albums d'UN seul artiste marqués compilation sous l'ancienne règle.
    let seul = |titre: &str, dossier: &str| {
        let id = album(db, titre, a, dossier);
        repo.reparer_compilation(id, true, None).unwrap();
        piste(
            db,
            id,
            a,
            1,
            1,
            None,
            Some("David Sanborn"),
            &format!("{dossier}/01.flac"),
        );
        piste(
            db,
            id,
            a,
            2,
            1,
            None,
            Some("David Sanborn"),
            &format!("{dossier}/02.flac"),
        );
        id
    };
    let force = seul("Here & Gone", "/c/hg");
    let temoin = seul("Hearsay", "/c/hs");
    // Une vraie compilation : deux artistes sous « Various Artists ».
    let jip = album(db, "Jazz in Paris", va, "/c/jip");
    repo.reparer_compilation(jip, true, None).unwrap();
    piste(
        db,
        jip,
        b,
        1,
        1,
        None,
        Some("Various Artists"),
        "/c/jip/01.flac",
    );
    piste(
        db,
        jip,
        c,
        2,
        1,
        None,
        Some("Various Artists"),
        "/c/jip/02.flac",
    );

    modifier(db, force, json!({ "compilation_mode": "oui" })).unwrap();
    let v = vue(db, force);
    assert_eq!(v.album.compilation_mode, "oui");
    assert!(v.album.compilation_effective);
    assert_eq!(v.album.champs_edites, vec!["compilation_mode"]);
    // Le recalcul de #5011 baisse le TÉMOIN (contre-épreuve), pas l'album forcé.
    repo.recalculer_les_compilations().unwrap();
    assert!(
        !repo.get(temoin).unwrap().unwrap().is_compilation,
        "contre-épreuve"
    );
    assert!(repo.get(force).unwrap().unwrap().is_compilation);

    modifier(db, jip, json!({ "compilation_mode": "non" })).unwrap();
    assert!(!repo.get(jip).unwrap().unwrap().is_compilation);
    // Le scan ne sait que LEVER : il ne relève pas un « non ».
    repo.mark_compilation(jip).unwrap();
    repo.recalculer_les_compilations().unwrap();
    let v = vue(db, jip);
    assert_eq!(v.album.compilation_mode, "non");
    assert!(!v.album.compilation_effective);

    // Retour à `auto` : LA règle reprend la main, dans les deux sens.
    modifier(db, jip, json!({ "compilation_mode": "auto" })).unwrap();
    let v = vue(db, jip);
    assert_eq!(v.album.compilation_mode, "auto");
    assert!(v.album.compilation_effective, "deux artistes : compilation");
    assert!(v.album.champs_edites.is_empty());
    modifier(db, force, json!({ "compilation_mode": "auto" })).unwrap();
    let v = vue(db, force);
    assert_eq!(v.album.compilation_mode, "auto");
    assert!(
        !v.album.compilation_effective,
        "un seul artiste : pas une compilation"
    );
}

/// Ce qu'une analyse relit des fichiers n'écrase plus l'édition : la ligne
/// piste reconstruite depuis les BALISES (disque 1, numéro d'origine, titre
/// « piste n », album résolu par le DOSSIER) reçoit les tenues avant d'être
/// écrite. Puis la passe des coffrets automatiques et le recalcul des
/// compilations passent sans rien défaire.
pub(crate) fn scenario_tenues_face_aux_analyses(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let k = poser_koln(db);
    let [a1, a2, a3] = k.a;
    let [b1, b2] = k.b;
    modifier(
        db,
        k.album,
        json!({
            "compilation_mode": "non",
            "discs": [
                { "number": 2, "title": "Live", "track_ids": [b2, b1, a3] },
                { "number": 1, "track_ids": [a1, a2] }
            ],
            "tracks": [ { "id": a2, "title": "Coda", "artist_name": "Invité" } ]
        }),
    )
    .unwrap();
    let attendu = vue(db, k.album);

    // Le scan relit les cinq fichiers. Le dossier cd2 n'a plus de ligne album
    // à lui : la résolution par dossier lui en crée une.
    let kj = artiste(db, "Keith Jarrett");
    let fantome = album(db, "The Köln Box", kj, "/e/box/cd2");
    let pistes = TrackRepo::with_backend(db.clone());
    let tenues = Tenues::charger(db);
    let mut relues = Vec::new();
    for (id, chemin, disque, n, album_du_dossier) in [
        (a1, "/e/box/cd1/01.flac", 1, 1, k.album),
        (a2, "/e/box/cd1/02.flac", 1, 2, k.album),
        (a3, "/e/box/cd1/03.flac", 1, 3, k.album),
        (b1, "/e/box/cd2/01.flac", 2, 1, fantome),
        (b2, "/e/box/cd2/02.flac", 2, 2, fantome),
    ] {
        let mut t = Track::new(format!("piste {n}"));
        t.id = Some(id);
        t.album_id = Some(album_du_dossier);
        t.artist_id = Some(kj);
        t.disc_number = disque;
        t.track_number = n;
        t.disc_subtitle = None;
        t.file_path = Some(chemin.to_string());
        let brute = t.clone();
        assert!(tenues.appliquer(&mut t), "{chemin} : tenue");
        relues.push((brute, t));
    }
    // Contre-épreuve : les lignes BRUTES auraient défait l'édition.
    assert!(
        relues
            .iter()
            .any(|(brute, tenue)| brute.album_id != tenue.album_id
                || brute.disc_number != tenue.disc_number
                || brute.track_number != tenue.track_number
                || brute.title != tenue.title),
        "la contre-épreuve doit montrer une différence"
    );
    let lignes: Vec<Track> = relues.into_iter().map(|(_, t)| t).collect();
    pistes.update_batch(&lignes).unwrap();
    AlbumRepo::with_backend(db.clone())
        .delete_orphans()
        .unwrap();
    assert!(
        !existe(db, fantome),
        "l'album fantôme est orphelin, donc purgé"
    );
    assert_eq!(vue(db, k.album), attendu, "après un scan qui relit tout");

    // Le recalcul des compilations et la passe des coffrets automatiques.
    let repo = AlbumRepo::with_backend(db.clone());
    repo.mark_compilation(k.album).unwrap();
    repo.recalculer_les_compilations().unwrap();
    coffrets_auto::passe(db).unwrap();
    assert_eq!(vue(db, k.album), attendu, "après les passes");
}

/// La passe des coffrets automatiques épargne un disque dont l'utilisateur a
/// disposé ; le coffret jumeau, jamais touché, est réuni (contre-épreuve).
pub(crate) fn scenario_passe_des_coffrets(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let lg = artiste(db, "Laurent Garnier");
    let maw = artiste(db, "Masters At Work");
    let p = "/m/ELECTRO/Laurent Garnier";
    let ew1 = disque(
        db,
        "Early Works, Disc 1",
        lg,
        &format!("{p}/1999-Early Works, Disc 1"),
        2,
    );
    let ew2 = disque(
        db,
        "Early Works, Disc 2",
        lg,
        &format!("{p}/2001-Early Works, Disc 2"),
        2,
    );
    let q = "/m/HOUSE/Masters At Work";
    let cc1 = disque(
        db,
        "Casino Classics, Disc 1",
        maw,
        &format!("{q}/Casino Classics, Disc 1"),
        2,
    );
    let cc2 = disque(
        db,
        "Casino Classics, Disc 2",
        maw,
        &format!("{q}/Casino Classics, Disc 2"),
        2,
    );

    modifier(
        db,
        ew1[0],
        json!({ "discs": [ { "number": 1, "title": "Face A", "track_ids": [ew1[2], ew1[1]] } ] }),
    )
    .unwrap();
    let r = coffrets_auto::passe(db).unwrap();
    assert_eq!(r.reunis, 1, "{r:?}");
    assert_eq!(r.laisses_manuels, 1, "{r:?}");
    assert!(existe(db, ew2[0]), "le disque 2 n'a pas été absorbé");
    assert!(!existe(db, cc2[0]), "contre-épreuve : le jumeau est réuni");
    assert_eq!(vue(db, cc1[0]).album.coffret.as_deref(), Some("auto"));
    let v = vue(db, ew1[0]);
    assert_eq!(disques(&v), vec![(1, Some("Face A"), 2)]);
    assert_eq!(places(&v), vec![(ew1[2], 1, 1), (ew1[1], 1, 2)]);
}

/// Attacher puis détacher un disque : les identifiants de pistes survivent,
/// le coffret se renumérote, et un coffret AUTOMATIQUE amputé d'un disque ne
/// se reforme pas.
pub(crate) fn scenario_attacher_detacher(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let lg = artiste(db, "Laurent Garnier");
    let p = "/m/ELECTRO/Laurent Garnier";
    let ew1 = disque(
        db,
        "Early Works, Disc 1",
        lg,
        &format!("{p}/1999-Early Works, Disc 1"),
        2,
    );
    let ew2 = disque(
        db,
        "Early Works, Disc 2",
        lg,
        &format!("{p}/2001-Early Works, Disc 2"),
        3,
    );

    assert_eq!(
        attacher(db, ew1[0], ew1[0]),
        Err(invalide(
            "meme_album",
            "un album ne s'attache pas à lui-même"
        ))
    );
    assert_eq!(
        attacher(db, ew1[0], 987_654),
        Err(RefusEdition::AlbumInconnu(987_654))
    );
    attacher(db, ew1[0], ew2[0]).unwrap();
    assert!(!existe(db, ew2[0]));
    let v = vue(db, ew1[0]);
    assert_eq!(v.album.coffret.as_deref(), Some("manuel"));
    assert_eq!(disques(&v), vec![(1, None, 2), (2, None, 3)]);
    assert_eq!(
        v.tracks
            .iter()
            .filter(|t| t.disc_number == 2)
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        ew2[1..].to_vec(),
        "les pistes attachées gardent leurs identifiants"
    );
    assert!(v.album.champs_edites.contains(&"discs".to_string()));

    modifier(
        db,
        ew1[0],
        json!({ "discs": [
            { "number": 1, "track_ids": [ew1[1], ew1[2]] },
            { "number": 2, "title": "Bonus", "track_ids": [ew2[1], ew2[2], ew2[3]] }
        ]}),
    )
    .unwrap();
    assert_eq!(code(detacher(db, ew1[0], 9).map(|_| ())), "disque_inconnu");
    let n = detacher(db, ew1[0], 2).unwrap();
    let d = vue(db, n);
    assert_eq!(d.album.title, "Early Works, Disc 1 — Bonus");
    assert_eq!(
        d.tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
        ew2[1..].to_vec(),
        "les pistes détachées gardent leurs identifiants"
    );
    assert_eq!(disques(&d), vec![(1, Some("Bonus"), 3)]);
    let c = vue(db, ew1[0]);
    assert_eq!(disques(&c), vec![(1, None, 2)]);
    assert_eq!(c.album.coffret, None, "un seul disque : plus un coffret");
    assert_eq!(code(detacher(db, ew1[0], 1).map(|_| ())), "un_seul_disque");
    // Un scan qui relit le disque détaché le range dans SON album.
    let t = Tenues::charger(db);
    assert_eq!(
        t.get(&format!("{p}/2001-Early Works, Disc 2/01.flac"))
            .map(|x| x.album_id),
        Some(n)
    );

    // Un coffret AUTOMATIQUE de trois disques ; le 2 est détaché.
    let maw = artiste(db, "Masters At Work");
    let q = "/m/HOUSE/Masters At Work";
    let cc: Vec<Vec<i64>> = (1..=3)
        .map(|k| {
            disque(
                db,
                &format!("Casino Classics, Disc {k}"),
                maw,
                &format!("{q}/Casino Classics, Disc {k}"),
                2,
            )
        })
        .collect();
    assert_eq!(coffrets_auto::passe(db).unwrap().reunis, 1);
    let coffret = cc[0][0];
    assert_eq!(vue(db, coffret).album.coffret.as_deref(), Some("auto"));
    let n = detacher(db, coffret, 2).unwrap();
    let v = vue(db, coffret);
    assert_eq!(disques(&v), vec![(1, None, 2), (2, None, 2)]);
    assert_eq!(
        v.tracks
            .iter()
            .filter(|t| t.disc_number == 2)
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        cc[2][1..].to_vec(),
        "l'ancien disque 3 devient le 2"
    );
    assert_eq!(v.album.coffret.as_deref(), Some("manuel"));
    assert_eq!(coffrets_auto::refus(db).len(), 1, "le refus est retenu");
    let r = coffrets_auto::passe(db).unwrap();
    assert_eq!(r.reunis, 0, "le coffret ne se reforme pas : {r:?}");
    assert!(existe(db, n));
}

#[test]
fn aller_retour_sur_sqlite() {
    scenario_aller_retour(&sqlite());
}

#[test]
fn disque_vide_sur_sqlite() {
    scenario_disque_vide(&sqlite());
}

#[test]
fn refus_sur_sqlite() {
    scenario_refus(&sqlite());
}

#[test]
fn compilation_sur_sqlite() {
    scenario_compilation(&sqlite());
}

#[test]
fn tenues_face_aux_analyses_sur_sqlite() {
    scenario_tenues_face_aux_analyses(&sqlite());
}

#[test]
fn passe_des_coffrets_sur_sqlite() {
    scenario_passe_des_coffrets(&sqlite());
}

#[test]
fn attacher_detacher_sur_sqlite() {
    scenario_attacher_detacher(&sqlite());
}

/// Le corps du contrat : `null` explicite ≠ champ absent.
#[test]
fn null_explicite_et_champ_absent_se_distinguent() {
    let m: Modification = serde_json::from_value(
        json!({ "label": null, "discs": [ { "track_ids": [1], "title": null } ] }),
    )
    .unwrap();
    assert_eq!(m.label, Some(None));
    assert_eq!(m.genre, None);
    let d = &m.discs.unwrap()[0];
    assert_eq!(d.title, Some(None));
    assert_eq!(d.number, None);
}

/// Tranche 4 — les valeurs que « Écrire dans les fichiers » écrira : celles de
/// la BASE après l'édition, disque par disque, et la règle du drapeau
/// COMPILATION (1 / 0 / retrait / laissé tel quel).
pub(crate) fn scenario_balises_effectives(db: &Arc<dyn DbBackend>) {
    use crate::metadata::tag_writer::DrapeauAEcrire;
    nettoyer(db);
    let k = poser_koln(db);
    // b[1] est une piste de feuille CUE : pas de fichier à elle.
    db.execute(
        &format!(
            "UPDATE tracks SET file_path = NULL, cue_media_path = {}, cue_start_ms = 0 \
             WHERE id = {}",
            marque(db.engine(), 1),
            marque(db.engine(), 2)
        ),
        &[&"/e/box/cd2/image.flac" as &dyn ToSqlValue, &k.b[1]],
    )
    .unwrap();
    modifier(
        db,
        k.album,
        json!({
            "title": "Köln 1975",
            "compilation_mode": "non",
            "tracks": [ { "id": k.a[1], "artist_name": "Gary Peacock" } ]
        }),
    )
    .unwrap();

    assert!(balises_effectives(db, 999_999).unwrap().is_none());
    let p = balises_effectives(db, k.album).unwrap().unwrap();
    assert_eq!(p.len(), 5);
    let de = |id: i64| p.iter().find(|x| x.id == id).expect("piste").clone();
    let a1 = de(k.a[1]);
    assert_eq!(a1.balises.album, "Köln 1975");
    assert_eq!(a1.balises.artiste_album.as_deref(), Some("Keith Jarrett"));
    assert_eq!(a1.balises.artiste.as_deref(), Some("Gary Peacock"));
    assert_eq!(a1.balises.titre, "piste 2");
    assert_eq!((a1.balises.disque, a1.balises.disques), (1, 2));
    assert_eq!((a1.balises.piste, a1.balises.pistes), (2, 3));
    assert_eq!(a1.balises.nom_disque.as_deref(), Some("Première partie"));
    assert_eq!(a1.chemin.as_deref(), Some("/e/box/cd1/02.flac"));
    assert!(!a1.cue);
    assert_eq!(a1.source, "local");
    // « non » et deux artistes : 0, pour que le scan ne le redécouvre pas.
    assert_eq!(a1.balises.compilation, Some(DrapeauAEcrire::Faux));
    let b0 = de(k.b[0]);
    assert_eq!((b0.balises.disque, b0.balises.pistes), (2, 2));
    assert_eq!(b0.balises.nom_disque, None);
    let b1 = de(k.b[1]);
    assert!(b1.cue, "la piste CUE doit être signalée");
    assert_eq!(b1.chemin, None);

    modifier(db, k.album, json!({ "compilation_mode": "oui" })).unwrap();
    let p = balises_effectives(db, k.album).unwrap().unwrap();
    assert!(
        p.iter()
            .all(|x| x.balises.compilation == Some(DrapeauAEcrire::Vrai))
    );

    // Un seul artiste : jamais COMPILATION=1, balise laissée telle quelle.
    modifier(
        db,
        k.album,
        json!({ "tracks": [ { "id": k.a[1], "artist_name": "Keith Jarrett" } ] }),
    )
    .unwrap();
    let p = balises_effectives(db, k.album).unwrap().unwrap();
    assert!(p.iter().all(|x| x.balises.compilation.is_none()));
    modifier(db, k.album, json!({ "compilation_mode": "non" })).unwrap();
    let p = balises_effectives(db, k.album).unwrap().unwrap();
    assert!(
        p.iter()
            .all(|x| x.balises.compilation == Some(DrapeauAEcrire::Retrait))
    );
}

#[test]
fn balises_effectives_sur_sqlite() {
    scenario_balises_effectives(&sqlite());
}
