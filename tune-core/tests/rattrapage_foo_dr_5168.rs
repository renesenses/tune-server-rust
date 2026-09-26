//! #5168 — le rattrapage des rapports `foo_dr.txt`, sans décodage ni rescan.
//!
//! Le DR d'un rapport voisin n'était lu qu'au scan d'un FICHIER : une
//! bibliothèque scannée avant la 0.9.152, ou un rapport posé après coup, ne
//! le voyait jamais. Ces témoins montent une base réelle (schéma et
//! migrations), des pistes DÉJÀ en base sans `dr_track`, le rapport réel du
//! fil 1800 (`tests/fixtures/foo_dr_tades_1800.txt`), et n'appellent QUE la
//! passe `taches_de_fond::rapports_dr::rattraper` — aucun scan.
//!
//! `[[test]]` à lui seul dans `tune-core/Cargo.toml` (`autotests = false`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::rapports_dr::{Bilan, CLE_TEMOIN, rattraper};

const RAPPORT_TADES: &[u8] = include_bytes!("fixtures/foo_dr_tades_1800.txt");

/// Un rapport stéréo minimal, au format du DR Meter, pour les dossiers où
/// l'on veut RÉÉCRIRE le rapport entre deux passes.
fn rapport_court(lignes: &[(u8, u32, &str)]) -> String {
    let mut s = String::from(
        "foobar2000 v2.25.10 / DR Meter v1.0.8\r\n\r\n\
         DR         Peak           RMS       Duration Track\r\n\
         --------------------------------------------------------------------------------\r\n",
    );
    for (dr, n, titre) in lignes {
        s.push_str(&format!(
            "DR{dr}      -0.10 dB     -12.00 dB      4:00 {n:02}-{titre}\r\n"
        ));
    }
    s.push_str(
        "--------------------------------------------------------------------------------\r\n\r\n\
         Number of tracks:  2\r\nOfficial DR value: DR10\r\n",
    );
    s
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    Arc::new(db)
}

/// Une piste déjà en base, SANS DR, et son fichier (quelques octets : rien ne
/// le décode).
fn piste(
    b: &Arc<dyn DbBackend>,
    id: i64,
    dossier: &Path,
    nom: &str,
    numero: i64,
    titre: &str,
    canaux: i64,
) -> PathBuf {
    let fichier = dossier.join(nom);
    std::fs::write(&fichier, b"pas de l'audio").expect("fichier");
    let chemin = fichier.to_string_lossy().to_string();
    b.execute(
        "INSERT INTO tracks (id, title, file_path, track_number, disc_number, channels, format) \
         VALUES (?, ?, ?, ?, 1, ?, 'flac')",
        &[&id as &dyn ToSqlValue, &titre, &chemin, &numero, &canaux],
    )
    .expect("insertion de piste");
    fichier
}

fn meta(b: &Arc<dyn DbBackend>, id: i64, cle: &str) -> Option<String> {
    b.query_one(
        "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
        &[&id as &dyn ToSqlValue, &cle],
    )
    .ok()
    .flatten()
    .and_then(|r| r.first().and_then(|v| v.as_string()))
}

fn poser(b: &Arc<dyn DbBackend>, id: i64, cle: &str, valeur: &str) {
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
        &[&id as &dyn ToSqlValue, &cle, &valeur],
    )
    .expect("métadonnée");
}

fn passe(b: &Arc<dyn DbBackend>) -> Bilan {
    rattraper(b, &HashSet::new())
}

/// Dater un fichier ou un dossier : les témoins de « rien n'a bougé » et de
/// « le rapport a changé » ne doivent pas dépendre de la finesse de
/// l'horloge du système de fichiers.
fn dater(chemin: &Path, decalage_s: u64) {
    let quand = SystemTime::now() + Duration::from_secs(decalage_s);
    std::fs::File::open(chemin)
        .expect("ouverture pour dater")
        .set_modified(quand)
        .expect("dater");
}

/// Le cas de Thierry : trois pistes du Mahler en base, sans DR, rapport de
/// Tades à côté. Aucun scan : la passe seule les pourvoit, avec la
/// provenance « sidecar » et l'appariement du scan (couche stéréo / couche
/// multicanal).
#[test]
fn une_piste_deja_en_base_sans_dr_recoit_le_dr_du_rapport_sans_rescan() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("rattrapage-foo-dr-5168-thierry");
    std::fs::write(dossier.join("foo_dr.txt"), RAPPORT_TADES).expect("rapport");
    piste(
        &b,
        1,
        &dossier,
        "11 - Mahler.flac",
        11,
        "Mahler Sym No 2: 5th Mov Etwas bewegter",
        2,
    );
    piste(
        &b,
        2,
        &dossier,
        "01 - Mahler.flac",
        1,
        "Mahler Sym No 2: 1st Mov Allegro maestoso",
        6,
    );
    piste(
        &b,
        3,
        &dossier,
        "02 - Mahler.flac",
        2,
        "Mahler Sym No 2: 2nd Mov Andante moderato",
        2,
    );

    let bilan = passe(&b);

    for (id, attendu) in [(1, "10"), (2, "7"), (3, "12")] {
        assert_eq!(
            meta(&b, id, "dr_track").as_deref(),
            Some(attendu),
            "🔴 #5168 — la piste {id}, déjà en base et sans DR, devait recevoir \
             le DR du `foo_dr.txt` de son dossier SANS rescan. Bilan : {bilan:?}"
        );
        assert_eq!(
            meta(&b, id, "dr_source").as_deref(),
            Some("sidecar"),
            "la provenance s'écrit avec la valeur"
        );
    }
    assert_eq!(bilan.tracks_written, 3, "{bilan:?}");
    assert_eq!(bilan.reports_found, 1, "{bilan:?}");
}

/// La précédence ne change pas : un DR lu dans les TAGS ou MESURÉ par Tune
/// n'est jamais remplacé par celui du rapport. Seul le vide se comble — un
/// tag présent mais vide n'est pas une valeur (`peut_ecrire_le_dr`).
#[test]
fn un_tag_ou_une_mesure_existants_ne_sont_jamais_ecrases() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("rattrapage-foo-dr-5168-precedence");
    std::fs::write(dossier.join("foo_dr.txt"), RAPPORT_TADES).expect("rapport");
    piste(
        &b,
        1,
        &dossier,
        "11 - a.flac",
        11,
        "Mahler Sym No 2: 5th Mov Etwas bewegter",
        2,
    );
    piste(
        &b,
        2,
        &dossier,
        "02 - b.flac",
        2,
        "Mahler Sym No 2: 2nd Mov Andante moderato",
        2,
    );
    piste(
        &b,
        3,
        &dossier,
        "03 - c.flac",
        3,
        "Mahler Sym No 2: 3rd Mov In ruhig fliessender Bewegung",
        2,
    );
    poser(&b, 1, "dr_track", "14");
    poser(&b, 1, "dr_source", "tag");
    poser(&b, 2, "dr_track", "13");
    poser(&b, 2, "dr_source", "analysis");
    poser(&b, 3, "dr_track", "  ");

    let bilan = passe(&b);

    assert_eq!(
        (
            meta(&b, 1, "dr_track").as_deref(),
            meta(&b, 1, "dr_source").as_deref()
        ),
        (Some("14"), Some("tag")),
        "🔴 #5168 — le TAG du fichier fait foi : le rapport ne l'écrase jamais. {bilan:?}"
    );
    assert_eq!(
        (
            meta(&b, 2, "dr_track").as_deref(),
            meta(&b, 2, "dr_source").as_deref()
        ),
        (Some("13"), Some("analysis")),
        "🔴 #5168 — une mesure déjà faite par Tune n'est pas écrasée. {bilan:?}"
    );
    assert_eq!(
        meta(&b, 3, "dr_track").as_deref(),
        Some("12"),
        "un tag VIDE n'est pas une valeur : le rapport le comble. {bilan:?}"
    );
    assert_eq!(bilan.tracks_written, 1, "{bilan:?}");
}

/// Incrémental : un dossier déjà vu dont rien n'a bougé coûte UN `stat`, et
/// n'est pas relu. Deux dossiers : l'un avec un rapport qui laisse une piste
/// sans appariement, l'autre sans rapport du tout.
#[test]
fn un_dossier_inchange_n_est_pas_relu() {
    let b = base();
    let racine = tune_core::test_scratch::scratch_dir("rattrapage-foo-dr-5168-inchange");
    let avec = racine.join("Avec rapport");
    let sans = racine.join("Sans rapport");
    std::fs::create_dir_all(&avec).unwrap();
    std::fs::create_dir_all(&sans).unwrap();
    std::fs::write(avec.join("foo_dr.txt"), rapport_court(&[(11, 1, "Un")])).unwrap();
    piste(&b, 1, &avec, "01 - Un.flac", 1, "Un", 2);
    piste(&b, 2, &avec, "05 - Cinq.flac", 5, "Cinq", 2);
    piste(&b, 3, &sans, "01 - x.flac", 1, "x", 2);
    piste(&b, 4, &sans, "02 - y.flac", 2, "y", 2);

    let premiere = passe(&b);
    assert_eq!(
        premiere.folders_read, 2,
        "premier passage : tout se lit. {premiere:?}"
    );
    assert_eq!(premiere.tracks_written, 1, "{premiere:?}");
    assert!(
        meta(&b, 2, CLE_TEMOIN).is_some(),
        "le dossier est mémorisé : {premiere:?}"
    );

    let seconde = passe(&b);
    assert_eq!(
        seconde.folders_read, 0,
        "🔴 #5168 — rien n'a bougé : aucun dossier ne devait être RELU. Sur \
         500 000 pistes, relire chaque dossier à chaque passe n'est pas un \
         rattrapage bon marché. {seconde:?}"
    );
    assert_eq!(seconde.folders_unchanged, 2, "{seconde:?}");
    assert_eq!(seconde.stats, 2, "un seul `stat` par dossier : {seconde:?}");
}

/// Un rapport MODIFIÉ est relu, et un rapport POSÉ après coup dans un dossier
/// qui n'en avait pas l'est aussi ; une piste AJOUTÉE à un dossier dont le
/// rapport n'a pas bougé fait relire ce dossier.
#[test]
fn un_rapport_modifie_ou_pose_apres_coup_est_relu() {
    let b = base();
    let racine = tune_core::test_scratch::scratch_dir("rattrapage-foo-dr-5168-modifie");
    let modifie = racine.join("Modifie");
    let pose = racine.join("Pose");
    let ajout = racine.join("Ajout");
    for d in [&modifie, &pose, &ajout] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(modifie.join("foo_dr.txt"), rapport_court(&[(11, 1, "Un")])).unwrap();
    std::fs::write(
        ajout.join("foo_dr.txt"),
        rapport_court(&[(8, 1, "Un"), (9, 2, "Deux")]),
    )
    .unwrap();
    piste(&b, 1, &modifie, "01 - Un.flac", 1, "Un", 2);
    piste(&b, 2, &modifie, "02 - Deux.flac", 2, "Deux", 2);
    piste(&b, 3, &pose, "01 - Un.flac", 1, "Un", 2);
    piste(&b, 4, &ajout, "03 - Trois.flac", 3, "Trois", 2);

    let premiere = passe(&b);
    assert_eq!(
        meta(&b, 1, "dr_track").as_deref(),
        Some("11"),
        "{premiere:?}"
    );
    assert_eq!(meta(&b, 2, "dr_track"), None, "{premiere:?}");
    assert_eq!(meta(&b, 3, "dr_track"), None, "{premiere:?}");
    assert_eq!(meta(&b, 4, "dr_track"), None, "{premiere:?}");

    // Le rapport du premier dossier est RÉÉCRIT (la piste 2 y entre) ; un
    // rapport est POSÉ dans le deuxième ; une piste 2 est AJOUTÉE au troisième.
    std::fs::write(
        modifie.join("foo_dr.txt"),
        rapport_court(&[(11, 1, "Un"), (13, 2, "Deux")]),
    )
    .unwrap();
    dater(&modifie.join("foo_dr.txt"), 30);
    std::fs::write(pose.join("foo_dr.txt"), rapport_court(&[(7, 1, "Un")])).unwrap();
    dater(&pose, 30);
    piste(&b, 5, &ajout, "02 - Deux.flac", 2, "Deux", 2);

    let seconde = passe(&b);
    assert_eq!(
        meta(&b, 2, "dr_track").as_deref(),
        Some("13"),
        "🔴 #5168 — le `foo_dr.txt` a CHANGÉ : son dossier devait être relu. {seconde:?}"
    );
    assert_eq!(
        meta(&b, 3, "dr_track").as_deref(),
        Some("7"),
        "🔴 #5168 — un `foo_dr.txt` POSÉ après coup devait être lu. {seconde:?}"
    );
    assert_eq!(
        meta(&b, 5, "dr_track").as_deref(),
        Some("9"),
        "🔴 #5168 — une piste AJOUTÉE à un dossier au rapport inchangé devait \
         le faire relire. {seconde:?}"
    );
    assert_eq!(seconde.folders_read, 3, "{seconde:?}");
}

/// Un dossier signalé par le surveillant de fichiers est relu sans consulter
/// la mémoire, même si la date n'a pas bougé (horloge grossière d'un partage).
#[test]
fn un_dossier_signale_par_le_surveillant_est_relu_sans_consulter_la_memoire() {
    let b = base();
    let dossier = tune_core::test_scratch::scratch_dir("rattrapage-foo-dr-5168-signale");
    let rapport = dossier.join("foo_dr.txt");
    std::fs::write(&rapport, rapport_court(&[(11, 1, "Un")])).unwrap();
    piste(&b, 1, &dossier, "02 - Deux.flac", 2, "Deux", 2);
    passe(&b);
    assert_eq!(meta(&b, 1, "dr_track"), None);

    // Réécrit, puis remis à la MÊME date : la mémoire seule ne le verrait pas.
    let avant = std::fs::metadata(&rapport).unwrap().modified().unwrap();
    std::fs::write(&rapport, rapport_court(&[(11, 1, "Un"), (12, 2, "Deux")])).unwrap();
    std::fs::File::open(&rapport)
        .unwrap()
        .set_modified(avant)
        .unwrap();
    assert_eq!(passe(&b).folders_read, 0, "même date : la mémoire l'écarte");

    let forces: HashSet<PathBuf> = [dossier.path().to_path_buf()].into_iter().collect();
    let bilan = rattraper(&b, &forces);
    assert_eq!(
        meta(&b, 1, "dr_track").as_deref(),
        Some("12"),
        "🔴 #5168 — le surveillant a signalé ce dossier : il devait être relu. {bilan:?}"
    );
}

/// Monte `dossiers` × 12 pistes (un dossier sur dix porte le rapport de
/// Tades), passe deux fois, rend les deux bilans et imprime les chiffres.
fn mesurer(etiquette: &str, dossiers: usize) -> (Bilan, Bilan) {
    let b = base();
    let racine = tune_core::test_scratch::scratch_dir(etiquette);
    let mut id = 0i64;
    for d in 0..dossiers {
        let dossier = racine.join(format!("Album {d:06}"));
        std::fs::create_dir_all(&dossier).unwrap();
        if d % 10 == 0 {
            std::fs::write(dossier.join("foo_dr.txt"), RAPPORT_TADES).unwrap();
        }
        for n in 1..=12 {
            id += 1;
            let chemin = dossier.join(format!("{n:02} - Piste.flac"));
            let chemin = chemin.to_string_lossy().to_string();
            b.execute(
                "INSERT INTO tracks (id, title, file_path, track_number, disc_number, channels) \
                 VALUES (?, 'Piste', ?, ?, 1, 2)",
                &[&id as &dyn ToSqlValue, &chemin, &(n as i64)],
            )
            .unwrap();
        }
    }
    let premiere = passe(&b);
    let seconde = passe(&b);
    println!("{etiquette} premier passage : {premiere:?}");
    println!("{etiquette} second passage  : {seconde:?}");
    (premiere, seconde)
}

/// La mesure du coût, sur un jeu de 2 000 dossiers × 12 pistes (24 000
/// pistes) : le premier passage lit tout, le second ne coûte qu'un `stat` par
/// dossier. Les bornes de temps ne sont pas gardées (machine partagée) : c'est
/// le NOMBRE de `stat` et de lectures qui l'est.
#[test]
fn cout_mesure_sur_un_jeu_de_24000_pistes() {
    let (premiere, seconde) = mesurer("rattrapage-foo-dr-5168-cout", 2000);
    assert_eq!(premiere.folders, 2000);
    assert_eq!(premiere.folders_read, 2000);
    assert_eq!(premiere.reports_found, 200);
    assert_eq!(seconde.folders_read, 0, "{seconde:?}");
    assert_eq!(
        seconde.stats, seconde.folders,
        "un `stat` par dossier : {seconde:?}"
    );
}

/// La même mesure à l'échelle de Thierry : 42 000 dossiers × 12 = 504 000
/// pistes. Ignorée par défaut (plusieurs minutes de montage) ; se lance avec
/// `cargo test -p tune-core --test rattrapage_foo_dr_5168 -- --ignored --nocapture`.
#[test]
#[ignore]
fn cout_mesure_sur_un_jeu_de_504000_pistes() {
    let (_, seconde) = mesurer("rattrapage-foo-dr-5168-cout-504k", 42_000);
    assert_eq!(seconde.folders_read, 0, "{seconde:?}");
    assert_eq!(seconde.stats, seconde.folders, "{seconde:?}");
}
