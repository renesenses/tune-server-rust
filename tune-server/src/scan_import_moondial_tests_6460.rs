//! #6460 (Lulu, fil 2311) — « MoonDial » de Pat Metheny, un album d'un seul
//! artiste, porte le badge compilation après la mise à jour rc4 → rc6.
//!
//! Ces témoins jouent le trajet de PRODUCTION sur une base neuve : le vrai
//! `begin_batch` / `import` d'un scan complet, les pistes écrites en base,
//! puis la passe `recalculer_les_compilations` que le serveur joue au
//! démarrage et après chaque scan. Aucune feuille CUE : la passe CUE, et donc
//! les correctifs de #6459 (10c3fe723, fbaa5387a), n'y interviennent pas.
//!
//! - les variantes aux balises propres gardent leur artiste : c'est ce que
//!   vérifie le témoin actif ;
//! - trois variantes plausibles, mais pas confirmées chez le testeur, passent
//!   en compilation sous « Various Artists » et y RESTENT après le recalcul.
//!   Ce sont les règles du 25/09/2026 appliquées telles qu'elles sont écrites,
//!   pas un défaut de code ; les changer est un arbitrage. Les témoins
//!   ignorés décrivent le comportement attendu si l'arbitrage les retient.
use super::*;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

const DOSSIER: &str = "Pat Metheny - MoonDial";

fn fichier(
    chemin: &str,
    album_artist: Option<&str>,
    artiste: &str,
    album: &str,
    piste: u32,
    compilation: Option<bool>,
) -> ScannedFile {
    ScannedFile {
        path: chemin.to_string(),
        metadata: Some(TrackMetadata {
            title: Some(format!("{album} {piste}")),
            artist: Some(artiste.to_string()),
            album: Some(album.to_string()),
            album_artist: album_artist.map(str::to_string),
            track_number: Some(piste),
            compilation,
            ..Default::default()
        }),
        unsupported: None,
        audio_hash: Some(format!("hash-{chemin}")),
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

/// Les quatre pistes de « MoonDial », chacune décrite par `piste(n)` :
/// `(artiste d'album, artiste de la piste, balise compilation)`.
fn moondial(
    racine: &std::path::Path,
    piste: impl Fn(u32) -> (Option<&'static str>, &'static str, Option<bool>),
) -> Vec<ScannedFile> {
    let d = racine.join(DOSSIER);
    std::fs::create_dir_all(&d).unwrap();
    (1..=4)
        .map(|n| {
            let (aa, a, comp) = piste(n);
            let chemin = d.join(format!("0{n}.flac"));
            fichier(&chemin.to_string_lossy(), aa, a, "MoonDial", n, comp)
        })
        .collect()
}

/// Un fichier d'un autre artiste rangé dans le dossier de « MoonDial ».
fn intrus(racine: &std::path::Path, album: &str, piste: u32) -> ScannedFile {
    let chemin = racine.join(DOSSIER).join("intrus.flac");
    fichier(
        &chemin.to_string_lossy(),
        Some("Lyle Mays"),
        "Lyle Mays",
        album,
        piste,
        None,
    )
}

/// Scan complet, pistes écrites, puis recalcul de démarrage. Rend chaque
/// album en base, `(titre, artiste d'album, compilation)`, trié.
fn scanner_puis_recalculer(
    fichiers: &[ScannedFile],
    cache: &std::path::Path,
    quality_split: bool,
) -> Vec<(String, String, bool)> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let albums = AlbumRepo::with_backend(backend.clone());
    let artistes = ArtistRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let mut imp = TrackImporter::new(
        backend.clone(),
        quality_split,
        cache.to_path_buf(),
        PorteeDuScan::TOUT,
    );
    imp.begin_batch(fichiers);
    let lignes: Vec<Track> = fichiers
        .iter()
        .map(|f| imp.import(f).expect("import").0)
        .collect();
    assert_eq!(pistes.create_batch(&lignes).unwrap(), fichiers.len());
    albums.recalculer_les_compilations().unwrap();
    let mut rendu: Vec<(String, String, bool)> = albums
        .list(1000, 0)
        .unwrap()
        .into_iter()
        .map(|a| {
            let nom = a
                .artist_id
                .and_then(|id| artistes.get(id).unwrap())
                .map(|x| x.name)
                .unwrap_or_default();
            (a.title, nom, a.is_compilation)
        })
        .collect();
    rendu.sort();
    rendu
}

fn les_deux_reglages(
    fichiers: impl Fn(&std::path::Path) -> Vec<ScannedFile>,
) -> Vec<(bool, Vec<(String, String, bool)>)> {
    [true, false]
        .into_iter()
        .map(|quality_split| {
            let tmp = tempfile::tempdir().unwrap();
            let f = fichiers(tmp.path());
            (
                quality_split,
                scanner_puis_recalculer(&f, &tmp.path().join("cache"), quality_split),
            )
        })
        .collect()
}

fn attendu(albums: &[(&str, &str)]) -> Vec<(String, String, bool)> {
    albums
        .iter()
        .map(|(t, a)| (t.to_string(), a.to_string(), false))
        .collect()
}

/// Les balises propres d'un album d'un seul artiste : il garde son artiste et
/// n'est pas une compilation, balise `COMPILATION=1` ou invité compris.
#[test]
fn moondial_aux_balises_propres_garde_son_artiste_6460() {
    let variantes: [(
        &str,
        fn(u32) -> (Option<&'static str>, &'static str, Option<bool>),
    ); 4] = [
        ("artiste d'album et de piste", |_| {
            (Some("Pat Metheny"), "Pat Metheny", None)
        }),
        ("balise COMPILATION=1", |_| {
            (Some("Pat Metheny"), "Pat Metheny", Some(true))
        }),
        ("sans artiste d'album", |_| (None, "Pat Metheny", None)),
        ("un invité et COMPILATION=1", |n| {
            let a = if n == 2 {
                "Pat Metheny & Linda May Han Oh"
            } else {
                "Pat Metheny"
            };
            (Some("Pat Metheny"), a, Some(true))
        }),
    ];
    for (nom, piste) in variantes {
        for (quality_split, rendu) in les_deux_reglages(|r| moondial(r, piste)) {
            assert_eq!(
                rendu,
                attendu(&[("MoonDial", "Pat Metheny")]),
                "{nom}, quality_split={quality_split} : MoonDial doit rester à Pat Metheny, \
                 sans badge compilation"
            );
        }
    }
}

/// Variante 1 — un fichier d'un AUTRE album et d'un autre artiste dans le
/// dossier de « MoonDial ». Aujourd'hui : un seul album nommé d'après le
/// dossier, sous « Various Artists », compilation (verdict du dossier,
/// `decide_compilation_folders`), que le recalcul garde puisque l'album réunit
/// bien deux artistes.
#[test]
#[ignore = "#6460 : en attente d'arbitrage sur la règle du dossier"]
fn moondial_et_un_intrus_d_un_autre_album_6460() {
    for (quality_split, rendu) in les_deux_reglages(|r| {
        let mut f = moondial(r, |_| (Some("Pat Metheny"), "Pat Metheny", None));
        f.push(intrus(r, "Lyle Mays", 2));
        f
    }) {
        assert_eq!(
            rendu,
            attendu(&[("Lyle Mays", "Lyle Mays"), ("MoonDial", "Pat Metheny")]),
            "quality_split={quality_split}"
        );
    }
}

/// Variante 2 — le même intrus, balisé du MÊME titre d'album « MoonDial ».
/// Aujourd'hui : « MoonDial » sous « Various Artists », compilation (deux
/// artistes principaux, deux artistes d'album).
#[test]
#[ignore = "#6460 : en attente d'arbitrage sur la règle du dossier"]
fn moondial_et_un_intrus_du_meme_titre_6460() {
    for (quality_split, rendu) in les_deux_reglages(|r| {
        let mut f = moondial(r, |_| (Some("Pat Metheny"), "Pat Metheny", None));
        f.push(intrus(r, "MoonDial", 5));
        f
    }) {
        assert!(
            rendu.contains(&("MoonDial".to_string(), "Pat Metheny".to_string(), false)),
            "quality_split={quality_split} : {rendu:?}"
        );
    }
}

/// Variante 3 — pas d'artiste d'album, et le nom écrit tantôt « Pat
/// Metheny », tantôt « Metheny, Pat ». Aujourd'hui : la virgule coupe le
/// crédit en « Metheny » et « Pat », aucun nom ne court sur toutes les
/// pistes, et l'album passe sous « Various Artists », compilation.
#[test]
#[ignore = "#6460 : en attente d'arbitrage sur les noms « Nom, Prénom »"]
fn moondial_au_nom_ecrit_de_deux_facons_6460() {
    for (quality_split, rendu) in les_deux_reglages(|r| {
        moondial(r, |n| {
            let a = if n % 2 == 0 {
                "Metheny, Pat"
            } else {
                "Pat Metheny"
            };
            (None, a, None)
        })
    }) {
        assert_eq!(
            rendu,
            attendu(&[("MoonDial", "Pat Metheny")]),
            "quality_split={quality_split}"
        );
    }
}
