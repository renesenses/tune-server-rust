//! #5034 (Didier, fil 1904, Windows 11) — la pochette d'album face au DISQUE,
//! jouée sur de VRAIS FLAC et par les VRAIES passes :
//!
//! - « Analyse rapide » et « Répertoires » : `spawn_library_scan` (non forcé,
//!   entier ou ciblé sur le dossier de l'album) ;
//! - « Analyse complète » : `spawn_library_scan` forcé ;
//! - le scan de démarrage : `spawn_auto_scan` ;
//! - le surveillant : `traiter_le_lot_du_surveillant`, nourri des événements
//!   que `notify` fabrique sous Windows, traduits par le gestionnaire de
//!   production (`rejouer_evenements_notify`) et passés par l'attente
//!   d'écriture stable (`settle_partition`).
//!
//! Chaque cas part d'un album de deux pistes indexé par un premier scan, fait
//! le geste de Didier sur le disque (Mp3tag sur les DEUX pistes, ou
//! l'explorateur sur `cover.jpg`), rejoue la passe, et lit `albums.cover_path`.
//! Le tableau reprend celui de la sonde de l'enquête
//! (`~/DEV/banc5034/sonde_pochettes_5034_5035.rs`).
use super::{ReglagesDuSurveillant, settle_partition, traiter_le_lot_du_surveillant};
use crate::state::AppState;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::{OggPictureStorage, VorbisComments};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::SourcePochette;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::artwork::{artwork_hash, content_hash};
use tune_core::scanner::watcher::notify::event::{CreateKind, ModifyKind, RemoveKind};
use tune_core::scanner::watcher::notify::{Event, EventKind};
use tune_core::scanner::watcher::{ChangeType, FileChange};

pub(super) const JAQUETTE: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DU-FLAC-5034";
pub(super) const JAQUETTE_2: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DU-FLAC-CHANGEE-5034";
pub(super) const COVER: &[u8] = b"\xFF\xD8\xFF\xE0COVER-JPG-DU-DOSSIER-5034";
pub(super) const COVER_2: &[u8] = b"\xFF\xD8\xFF\xE0COVER-JPG-CHANGE-DANS-L-EXPLORATEUR-5034";
const TELEVERSEE: &[u8] = b"\xFF\xD8\xFF\xE0TELEVERSEE-A-LA-MAIN-5034";
const FOURNISSEUR: &[u8] = b"\xFF\xD8\xFF\xE0COVER-ART-ARCHIVE-5034";

fn gabarit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac")
}

fn hier() -> SystemTime {
    SystemTime::now() - Duration::from_secs(86_400)
}

fn dater(fichier: &Path, quand: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(fichier)
        .and_then(|f| f.set_modified(quand))
        .expect("date de modification");
}

/// Pose (ou retire, `None`) la jaquette intégrée, comme Mp3tag.
pub(super) fn poser_jaquette(piste: &Path, jaquette: Option<&[u8]>) {
    let mut f = std::fs::File::open(piste).unwrap();
    let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
    drop(f);
    while !flac.pictures().is_empty() {
        flac.remove_picture(0);
    }
    if let Some(octets) = jaquette {
        let pic = Picture::unchecked(octets.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Jpeg)
            .build();
        flac.insert_picture(pic, Some(PictureInformation::default()))
            .unwrap();
    }
    flac.save_to_path(piste, WriteOptions::default()).unwrap();
}

/// Pose, remplace ou retire `cover.jpg`, comme l'explorateur.
pub(super) fn poser_cover(dossier: &Path, cover: Option<&[u8]>) {
    let chemin = dossier.join("cover.jpg");
    match cover {
        Some(o) => std::fs::write(&chemin, o).unwrap(),
        None => {
            let _ = std::fs::remove_file(&chemin);
        }
    }
}

/// Un album de deux pistes sous une racine de musique, daté d'hier.
///
/// Racine sous le dossier courant et NON sous le dossier temporaire du
/// système : `is_tune_temp_file` écarte tout ce qui vit sous ce dernier.
pub(super) fn album_sur_disque(
    epreuve: &str,
    jaquette: Option<&[u8]>,
    cover: Option<&[u8]>,
) -> (tune_core::test_scratch::ScratchDir, PathBuf, Vec<PathBuf>) {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("pochettes-5034-{epreuve}"),
    );
    let dossier = racine.join("Didier").join("Album");
    std::fs::create_dir_all(&dossier).unwrap();
    let mut pistes = Vec::new();
    for (i, titre) in ["Un", "Deux"].iter().enumerate() {
        let piste = dossier.join(format!("0{} - {titre}.flac", i + 1));
        std::fs::copy(gabarit(), &piste).unwrap();
        let mut f = std::fs::File::open(&piste).unwrap();
        let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        drop(f);
        let mut vc = VorbisComments::default();
        let n = (i + 1).to_string();
        for (k, v) in [
            ("TITLE", *titre),
            ("ARTIST", "Didier"),
            ("ALBUMARTIST", "Didier"),
            ("ALBUM", "Album de Didier"),
            ("TRACKNUMBER", n.as_str()),
        ] {
            vc.insert(k.to_string(), v.to_string());
        }
        flac.set_vorbis_comments(vc);
        flac.save_to_path(&piste, WriteOptions::default()).unwrap();
        poser_jaquette(&piste, jaquette);
        dater(&piste, hier());
        pistes.push(piste);
    }
    poser_cover(&dossier, cover);
    if cover.is_some() {
        dater(&dossier.join("cover.jpg"), hier());
    }
    (racine, dossier, pistes)
}

pub(super) fn etat_sur(racine: &Path) -> AppState {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("AppState en mémoire");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    etat
}

/// Le scan MANUEL (`force` = « Analyse complète », `cible` = « Répertoires »),
/// jusqu'à sa fin annoncée. Le droit de scanner est global au processus : un
/// autre essai peut le tenir, on attend qu'il revienne.
pub(super) async fn scan_manuel(etat: &AppState, force: bool, cible: Option<&Path>) {
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    let cible = cible.map(|c| c.to_string_lossy().into_owned());
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), force, cible.clone()).await
    {
        assert!(
            debut.elapsed() < Duration::from_secs(300),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan manuel n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => return,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus d'événements fermé : {e}"),
        }
    }
}

/// Le scan de DÉMARRAGE. Il se retire en silence quand un autre scan tient le
/// droit : `scan_started_at`, qu'il ne pose qu'une fois le droit acquis, dit
/// s'il a vraiment tourné.
pub(super) async fn scan_de_demarrage(db: &Arc<dyn DbBackend>) {
    let reglages = SettingsRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let fini = crate::auto_scan::spawn_auto_scan(
            db.clone(),
            Arc::new(tune_core::event_bus::EventBus::new()),
        );
        while !fini.load(Ordering::Acquire) {
            assert!(
                debut.elapsed() < Duration::from_secs(300),
                "scan de démarrage sans fin"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            return;
        }
        assert!(
            debut.elapsed() < Duration::from_secs(300),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Un lot du surveillant : les événements Windows de `notify` 7.0.0
/// (`src/windows.rs`) pour ce qui a bougé, traduits par le gestionnaire de
/// production, passés par l'attente d'écriture stable, puis traités.
pub(super) fn lot_du_surveillant(
    db: &Arc<dyn DbBackend>,
    racine: &Path,
    pistes_touchees: &[PathBuf],
    cover: Option<(&Path, bool)>,
) {
    let mut evenements: Vec<Event> = pistes_touchees
        .iter()
        .map(|p| Event::new(EventKind::Modify(ModifyKind::Any)).add_path(p.clone()))
        .collect();
    if let Some((chemin, existe)) = cover {
        // Windows : `FILE_ACTION_REMOVED` → `Remove(Any)` ; une écriture en
        // place → `Modify(Any)` ; une création → `Create(Any)`.
        let genre = if !existe {
            EventKind::Remove(RemoveKind::Any)
        } else if chemin.exists() {
            EventKind::Modify(ModifyKind::Any)
        } else {
            EventKind::Create(CreateKind::Any)
        };
        evenements.push(Event::new(genre).add_path(chemin.to_path_buf()));
    }
    let changes = tune_core::scanner::watcher::rejouer_evenements_notify(evenements);
    let (prets, en_attente) = settle_partition(changes, &[]);
    assert!(
        en_attente.is_empty(),
        "montage : rien ne s'écrit encore ({en_attente:?})"
    );
    let racines = [racine.to_string_lossy().into_owned()];
    let mut attente = Vec::new();
    traiter_le_lot_du_surveillant(
        db,
        prets,
        &ReglagesDuSurveillant {
            exclusions: &[],
            racines: &racines,
            quality_split: true,
        },
        &mut attente,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Passe {
    Rapide,
    Repertoires,
    Complete,
    Demarrage,
    Surveillant,
}

/// Ce qu'on fait à la pochette déjà en place avant le geste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Avant {
    /// Telle que le premier scan l'a posée.
    DuScan,
    /// Téléversée à la main (`POST /albums/{id}/artwork`).
    Televersee,
    /// Récupérée chez un fournisseur (Cover Art Archive).
    Fournisseur,
    /// Ligne d'avant la migration 111 : source inconnue.
    SourceInconnue,
    /// Ligne d'avant #1444 : adresse dérivée du CHEMIN de la première piste,
    /// source inconnue.
    AdresseHeritee,
}

pub(super) struct Cas {
    pub nom: &'static str,
    pub avant: (Option<&'static [u8]>, Option<&'static [u8]>),
    pub pochette: Avant,
    pub apres: (Option<&'static [u8]>, Option<&'static [u8]>),
    /// `None` : l'album ne doit plus avoir de pochette.
    pub attendu: Option<&'static [u8]>,
}

fn nommer(h: Option<&str>) -> String {
    let noms: [(&[u8], &str); 6] = [
        (JAQUETTE, "jaquette du FLAC"),
        (JAQUETTE_2, "jaquette du FLAC changée"),
        (COVER, "cover.jpg"),
        (COVER_2, "cover.jpg changé"),
        (TELEVERSEE, "pochette téléversée"),
        (FOURNISSEUR, "pochette du fournisseur"),
    ];
    match h {
        None => "aucune".into(),
        Some(h) => noms
            .iter()
            .find(|(o, _)| content_hash(o) == h)
            .map(|(_, n)| (*n).to_string())
            .unwrap_or_else(|| format!("autre ({h})")),
    }
}

fn album_de(db: &Arc<dyn DbBackend>, piste: &Path) -> i64 {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&piste.to_string_lossy())
        .unwrap()
        .expect("piste indexée")
        .album_id
        .expect("album")
}

/// Joue UN cas par UNE passe ; rend ce que la base porte à la fin, nommé.
pub(super) async fn jouer(cas: &Cas, passe: Passe) -> (String, String) {
    let epreuve = format!(
        "{:?}-{}",
        passe,
        cas.nom
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
    );
    let (racine, dossier, pistes) = album_sur_disque(&epreuve, cas.avant.0, cas.avant.1);
    let etat = etat_sur(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat, false, None).await;
    let aid = album_de(&db, &pistes[0]);
    let repo = AlbumRepo::with_backend(db.clone());
    match cas.pochette {
        Avant::DuScan => {}
        Avant::Televersee => repo
            .force_update_cover_path(aid, &content_hash(TELEVERSEE), SourcePochette::Televersee)
            .unwrap(),
        Avant::Fournisseur => repo
            .force_update_cover_path(aid, &content_hash(FOURNISSEUR), SourcePochette::Fournisseur)
            .unwrap(),
        Avant::SourceInconnue => {
            db.execute(
                "UPDATE albums SET cover_source = NULL, cover_source_path = NULL, \
                 cover_source_stamp = NULL WHERE id = ?",
                &[&aid],
            )
            .unwrap();
        }
        Avant::AdresseHeritee => {
            let heritee = artwork_hash(&pistes[0].to_string_lossy());
            db.execute(
                "UPDATE albums SET cover_path = ?, cover_source = NULL, \
                 cover_source_path = NULL, cover_source_stamp = NULL WHERE id = ?",
                &[&heritee as &dyn tune_core::db::backend::ToSqlValue, &aid],
            )
            .unwrap();
        }
    }
    let avant = nommer(repo.get(aid).unwrap().unwrap().cover_path.as_deref());

    // Le geste, sur le disque.
    let mut touchees = Vec::new();
    if cas.apres.0 != cas.avant.0 {
        for p in &pistes {
            poser_jaquette(p, cas.apres.0);
            touchees.push(p.clone());
        }
    }
    let cover_touche = cas.apres.1 != cas.avant.1;
    if cover_touche {
        poser_cover(&dossier, cas.apres.1);
    }

    match passe {
        Passe::Rapide => scan_manuel(&etat, false, None).await,
        Passe::Repertoires => scan_manuel(&etat, false, Some(&dossier)).await,
        Passe::Complete => scan_manuel(&etat, true, None).await,
        Passe::Demarrage => scan_de_demarrage(&db).await,
        Passe::Surveillant => {
            let cover = dossier.join("cover.jpg");
            lot_du_surveillant(
                &db,
                &racine,
                &touchees,
                cover_touche.then_some((cover.as_path(), cas.apres.1.is_some())),
            );
        }
    }
    let aid = album_de(&db, &pistes[0]);
    let apres = nommer(
        AlbumRepo::with_backend(db.clone())
            .get(aid)
            .unwrap()
            .unwrap()
            .cover_path
            .as_deref(),
    );
    (avant, apres)
}

/// Joue le tableau par une passe, et rend TOUS les écarts d'un coup : un
/// rouge dit chaque cas faux, pas seulement le premier.
pub(super) async fn jouer_le_tableau(cas: &[Cas], passe: Passe) {
    let mut ecarts = Vec::new();
    for c in cas {
        let (avant, apres) = jouer(c, passe).await;
        let attendu = nommer(c.attendu.map(content_hash).as_deref());
        if apres != attendu {
            ecarts.push(format!(
                "{passe:?} / {} : avant « {avant} », après « {apres} », attendu « {attendu} »",
                c.nom
            ));
        }
    }
    assert!(ecarts.is_empty(), "#5034 —\n{}", ecarts.join("\n"));
}

/// Les gestes de RETRAIT (points 6 et 8 de Didier) et leurs contre-témoins.
pub(super) const RETRAITS: &[Cas] = &[
    Cas {
        nom: "6 jaquette retiree sans cover.jpg",
        avant: (Some(JAQUETTE), None),
        pochette: Avant::DuScan,
        apres: (None, None),
        attendu: None,
    },
    Cas {
        nom: "6b jaquette retiree, cover.jpg present",
        avant: (Some(JAQUETTE), Some(COVER)),
        pochette: Avant::DuScan,
        apres: (None, Some(COVER)),
        attendu: Some(COVER),
    },
    Cas {
        nom: "8 cover.jpg retire sans jaquette",
        avant: (None, Some(COVER)),
        pochette: Avant::DuScan,
        apres: (None, None),
        attendu: None,
    },
    Cas {
        nom: "8b cover.jpg retire, jaquette presente",
        avant: (Some(JAQUETTE), Some(COVER)),
        pochette: Avant::DuScan,
        apres: (Some(JAQUETTE), None),
        attendu: Some(JAQUETTE),
    },
    Cas {
        nom: "televersee puis jaquette retiree",
        avant: (Some(JAQUETTE), None),
        pochette: Avant::Televersee,
        apres: (None, None),
        attendu: Some(TELEVERSEE),
    },
    Cas {
        nom: "televersee puis cover.jpg retire",
        avant: (None, Some(COVER)),
        pochette: Avant::Televersee,
        apres: (None, None),
        attendu: Some(TELEVERSEE),
    },
    Cas {
        nom: "fournisseur puis cover.jpg retire",
        avant: (None, Some(COVER)),
        pochette: Avant::Fournisseur,
        apres: (None, None),
        attendu: Some(FOURNISSEUR),
    },
    Cas {
        nom: "source inconnue (avant migration) puis cover.jpg retire",
        avant: (None, Some(COVER)),
        pochette: Avant::SourceInconnue,
        apres: (None, None),
        attendu: Some(COVER),
    },
    Cas {
        nom: "source inconnue (avant migration) puis jaquette retiree",
        avant: (Some(JAQUETTE), None),
        pochette: Avant::SourceInconnue,
        apres: (None, None),
        attendu: Some(JAQUETTE),
    },
];

/// Le retrait PROUVÉ d'une ligne d'avant la migration : son adresse est celle
/// que l'ancien schéma dérivait du chemin de la piste — un téléversement ne la
/// fabrique jamais. La jaquette retirée, elle part.
///
/// Ne vaut que pour les passes qui RELISENT la piste : le surveillant (piste
/// modifiée), les scans (piste plus récente que la ligne) — toutes.
const RETRAIT_PROUVE: &[Cas] = &[Cas {
    nom: "adresse heritee puis jaquette retiree",
    avant: (Some(JAQUETTE), None),
    pochette: Avant::AdresseHeritee,
    apres: (None, None),
    attendu: None,
}];

#[tokio::test]
async fn retraits_par_l_analyse_rapide_5034() {
    jouer_le_tableau(RETRAITS, Passe::Rapide).await;
    jouer_le_tableau(RETRAIT_PROUVE, Passe::Rapide).await;
}

#[tokio::test]
async fn retraits_par_repertoires_5034() {
    jouer_le_tableau(RETRAITS, Passe::Repertoires).await;
    jouer_le_tableau(RETRAIT_PROUVE, Passe::Repertoires).await;
}

#[tokio::test]
async fn retraits_par_l_analyse_complete_5034() {
    jouer_le_tableau(RETRAITS, Passe::Complete).await;
    jouer_le_tableau(RETRAIT_PROUVE, Passe::Complete).await;
}

#[tokio::test]
async fn retraits_par_le_scan_de_demarrage_5034() {
    jouer_le_tableau(RETRAITS, Passe::Demarrage).await;
    jouer_le_tableau(RETRAIT_PROUVE, Passe::Demarrage).await;
}

#[tokio::test]
async fn retraits_par_le_surveillant_5034() {
    jouer_le_tableau(RETRAITS, Passe::Surveillant).await;
    jouer_le_tableau(RETRAIT_PROUVE, Passe::Surveillant).await;
}

/// Le surveillant relaie désormais les images de pochette — et SEULEMENT
/// elles parmi les fichiers non audio. Une suppression de `cover.jpg` sous
/// Windows (`Remove(Any)`) ne passe plus pour un dossier disparu, et elle
/// franchit l'attente d'écriture stable, qui jetait tout chemin absent.
#[test]
fn le_surveillant_relaie_les_images_de_pochette_5034() {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "pochettes-5034-relais",
    );
    let dossier = racine.join("Album");
    std::fs::create_dir_all(&dossier).unwrap();
    let absente = dossier.join("Cover.JPG");
    let changes = tune_core::scanner::watcher::rejouer_evenements_notify(vec![
        Event::new(EventKind::Remove(RemoveKind::Any)).add_path(absente.clone()),
        Event::new(EventKind::Remove(RemoveKind::Any)).add_path(dossier.join("notes.txt")),
    ]);
    assert_eq!(
        changes
            .iter()
            .map(|c| (c.change_type.clone(), c.path.clone()))
            .filter(|(_, p)| p.ends_with("Cover.JPG"))
            .collect::<Vec<_>>(),
        vec![(
            ChangeType::ImageDePochette,
            absente.to_string_lossy().into_owned()
        )],
        "la suppression de Cover.JPG doit être relayée comme image de pochette : {changes:?}"
    );
    let (prets, _) = settle_partition(
        vec![FileChange {
            change_type: ChangeType::ImageDePochette,
            path: absente.to_string_lossy().into_owned(),
        }],
        &[],
    );
    assert_eq!(prets.len(), 1, "une image supprimée n'a rien à attendre");
}
