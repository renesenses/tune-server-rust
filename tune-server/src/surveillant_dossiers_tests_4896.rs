//! #4896 (Didier, fil 1904) — un dossier d'album renommé, déplacé, mis à la
//! corbeille ou supprimé pendant que Tune tourne.
//!
//! Le surveillant écartait tout événement dont le chemin n'était pas un
//! fichier audio ; or les trois moteurs natifs de `notify` 7.0.0 ne signalent
//! que le DOSSIER (Windows `ReadDirectoryChangesW`, macOS FSEvents, Linux
//! inotify). L'album restait sous son chemin mort jusqu'au scan suivant, qui
//! le supprimait puis le réimportait comme un album neuf : identifiants,
//! favoris, écoutes et étiquettes perdus.
//!
//! Ces épreuves jouent la chaîne de PRODUCTION de bout en bout, sur de vrais
//! FLAC indexés par le scan (`coffret_indexe`) puis déplacés sur le disque :
//! événements `notify` bruts → gestionnaire du surveillant
//! (`rejouer_evenements_notify`) → `settle_partition` →
//! [`traiter_le_lot_du_surveillant`], trois tours de la boucle de
//! `spawn_file_watcher`. Chaque moteur livre la séquence que son code source
//! fabrique (voir `evenement_de_dossier`, `tune-core/src/scanner/watcher.rs`).
use super::surveillant_retouche_tests_4896::{baliser, base, coffret_indexe, flac_8_canaux};
use super::{ReglagesDuSurveillant, settle_partition, traiter_le_lot_du_surveillant};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::notify::Event;
use tune_core::scanner::watcher::notify::event::{
    CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode,
};
use tune_core::scanner::watcher::{FileChange, rejouer_evenements_notify};

fn ev(kind: EventKind, chemin: &Path) -> Event {
    Event::new(kind).add_path(chemin.to_path_buf())
}

fn nom(mode: RenameMode) -> EventKind {
    EventKind::Modify(ModifyKind::Name(mode))
}

fn chaine(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// UN tour de la boucle de `spawn_file_watcher`, à partir de ce qui attendait
/// (`a_suivre`) : attente d'écriture stable, puis le lot. Rend ce qui attend
/// le tour suivant.
fn un_tour(
    db: &Arc<dyn DbBackend>,
    racines: &[String],
    a_suivre: Vec<FileChange>,
    attente: &mut Vec<String>,
) -> Vec<FileChange> {
    let (changes, mut en_ecriture) = settle_partition(a_suivre, &[]);
    let a_relire = traiter_le_lot_du_surveillant(
        db,
        changes,
        &ReglagesDuSurveillant {
            exclusions: &[],
            racines,
            quality_split: true,
        },
        attente,
    );
    en_ecriture.extend(a_relire);
    en_ecriture
}

/// Les événements, puis deux tours sans événement : un dossier disparu attend
/// un lot, et le contenu d'un dossier apparu attend son écriture stable.
fn surveiller(db: &Arc<dyn DbBackend>, racine: &Path, evenements: Vec<Event>) {
    let racines = vec![chaine(racine)];
    let mut attente = Vec::new();
    let mut a_suivre = rejouer_evenements_notify(evenements);
    for _ in 0..3 {
        a_suivre = un_tour(db, &racines, a_suivre, &mut attente);
    }
    assert!(attente.is_empty(), "rien ne reste en attente : {attente:?}");
}

/// (identifiant de piste, identifiant d'album) de chaque fichier, en base.
fn lignes(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> Vec<Option<(i64, i64)>> {
    let track_repo = TrackRepo::with_backend(db.clone());
    pistes
        .iter()
        .map(|p| {
            track_repo
                .get_by_path(&chaine(p))
                .unwrap()
                .map(|t| (t.id.unwrap(), t.album_id.unwrap()))
        })
        .collect()
}

fn nombre_de_pistes(db: &Arc<dyn DbBackend>) -> i64 {
    TrackRepo::with_backend(db.clone()).count().unwrap()
}

/// Les pistes du dossier renommé : mêmes noms, sous le nouveau dossier.
fn sous(nouveau: &Path, pistes: &[PathBuf]) -> Vec<PathBuf> {
    pistes
        .iter()
        .map(|p| nouveau.join(p.file_name().unwrap()))
        .collect()
}

/// Le même album sous un autre nom : les MÊMES lignes (identifiants de piste
/// et d'album) aux nouveaux chemins, plus rien aux anciens, pas une ligne de
/// plus, et l'album reconnu à son nouveau dossier.
fn assert_meme_album_deplace(
    db: &Arc<dyn DbBackend>,
    moteur: &str,
    avant: &[Option<(i64, i64)>],
    anciennes: &[PathBuf],
    nouvelles: &[PathBuf],
) {
    assert_eq!(
        lignes(db, nouvelles),
        avant,
        "{moteur} : les pistes gardent leur ligne (favoris, écoutes, étiquettes) \
         sous le nouveau nom du dossier"
    );
    assert!(
        lignes(db, anciennes).iter().all(Option::is_none),
        "{moteur} : plus rien sous l'ancien nom"
    );
    assert_eq!(
        nombre_de_pistes(db),
        avant.len() as i64,
        "{moteur} : aucune piste en double"
    );
    let aid = avant[0].unwrap().1;
    let dossier = AlbumRepo::with_backend(db.clone())
        .folder_path_of(aid)
        .unwrap()
        .expect("l'album garde un dossier");
    assert_eq!(
        Some(dossier),
        tune_core::scanner::album_folder::album_folder(&chaine(&nouvelles[0])),
        "{moteur} : l'album se reconnaît à son NOUVEAU dossier, sans quoi la prochaine \
         relecture d'une piste en ouvrirait un second"
    );
}

/// Le témoin, joué pour chaque moteur : le dossier « Multichannel 7.1 » de
/// Didier renommé sur place, ou déplacé sous un autre parent.
#[test]
fn un_dossier_d_album_renomme_garde_ses_pistes_sur_chaque_moteur_4896() {
    type Sequence = fn(&Path, &Path, &[PathBuf], &[PathBuf]) -> Vec<Event>;
    let cas: [(&str, bool, Sequence); 6] = [
        ("Windows, renommage sur place", false, |a, n, _, _| {
            vec![ev(nom(RenameMode::From), a), ev(nom(RenameMode::To), n)]
        }),
        (
            "Windows, déplacement vers un autre parent (REMOVED puis ADDED)",
            true,
            |a, n, _, _| {
                vec![
                    ev(EventKind::Remove(RemoveKind::Any), a),
                    ev(EventKind::Create(CreateKind::Any), n),
                ]
            },
        ),
        (
            "macOS FSEvents (deux ItemRenamed sans lien)",
            false,
            |a, n, _, _| vec![ev(nom(RenameMode::Any), a), ev(nom(RenameMode::Any), n)],
        ),
        (
            "macOS FSEvents, ordre inverse dans le lot",
            true,
            |a, n, _, _| vec![ev(nom(RenameMode::Any), n), ev(nom(RenameMode::Any), a)],
        ),
        (
            "Linux inotify (MOVED_FROM, MOVED_TO, paire, MOVE_SELF)",
            false,
            |a, n, _, _| {
                vec![
                    ev(nom(RenameMode::From), a),
                    ev(nom(RenameMode::To), n),
                    Event::new(nom(RenameMode::Both))
                        .add_path(a.to_path_buf())
                        .add_path(n.to_path_buf()),
                    ev(nom(RenameMode::From), a),
                ]
            },
        ),
        (
            "PollWatcher (partage réseau) : chaque fichier disparaît et apparaît",
            true,
            |a, n, anciennes, nouvelles| {
                let mut v: Vec<Event> = anciennes
                    .iter()
                    .map(|p| ev(EventKind::Remove(RemoveKind::Any), p))
                    .collect();
                v.push(ev(EventKind::Remove(RemoveKind::Any), a));
                v.push(ev(EventKind::Create(CreateKind::Any), n));
                v.extend(
                    nouvelles
                        .iter()
                        .map(|p| ev(EventKind::Create(CreateKind::Any), p)),
                );
                v
            },
        ),
    ];
    for (i, (moteur, autre_parent, sequence)) in cas.into_iter().enumerate() {
        let db = base();
        let (racine, pistes) = coffret_indexe(&db, &format!("dossier-renomme-{i}"));
        let avant = lignes(&db, &pistes);
        assert!(
            avant.iter().all(Option::is_some),
            "montage : coffret indexé"
        );
        let ancien = pistes[0].parent().unwrap().to_path_buf();
        let nouveau = if autre_parent {
            let archives = racine.join("Archives");
            std::fs::create_dir_all(&archives).unwrap();
            archives.join("Multichannel 7.1")
        } else {
            ancien.with_file_name("Multichannel 7.1 (2023)")
        };
        std::fs::rename(&ancien, &nouveau).unwrap();
        let nouvelles = sous(&nouveau, &pistes);
        surveiller(
            &db,
            &racine,
            sequence(&ancien, &nouveau, &pistes, &nouvelles),
        );
        assert_meme_album_deplace(&db, moteur, &avant, &pistes, &nouvelles);
    }
}

/// Les deux moitiés d'un renommage dans DEUX lots (l'ancien nom en fin de
/// lot, le nouveau au début du suivant) : le disparu a attendu, il est
/// reconnu. Sans l'attente, ses pistes seraient parties au premier lot.
#[test]
fn un_renommage_coupe_entre_deux_lots_garde_ses_pistes_4896() {
    let db = base();
    let (racine, pistes) = coffret_indexe(&db, "dossier-deux-lots");
    let avant = lignes(&db, &pistes);
    let ancien = pistes[0].parent().unwrap().to_path_buf();
    let nouveau = ancien.with_file_name("Multichannel 7.1 (2023)");
    std::fs::rename(&ancien, &nouveau).unwrap();
    let racines = vec![chaine(&racine)];
    let mut attente = Vec::new();
    let reste = un_tour(
        &db,
        &racines,
        rejouer_evenements_notify(vec![ev(nom(RenameMode::From), &ancien)]),
        &mut attente,
    );
    assert_eq!(attente, vec![chaine(&ancien)], "le disparu attend un lot");
    assert_eq!(
        lignes(&db, &pistes),
        avant,
        "rien n'est retiré pendant l'attente"
    );
    let mut a_suivre = reste;
    a_suivre.extend(rejouer_evenements_notify(vec![ev(
        nom(RenameMode::To),
        &nouveau,
    )]));
    for _ in 0..2 {
        a_suivre = un_tour(&db, &racines, a_suivre, &mut attente);
    }
    assert_meme_album_deplace(
        &db,
        "lots coupés",
        &avant,
        &pistes,
        &sous(&nouveau, &pistes),
    );
}

/// Mis à la corbeille (ou sorti de la racine) : un seul événement, sur le
/// dossier. Ses pistes partent après UN lot d'attente, et seulement elles —
/// « Multichannel 7.1 bis », qui partage le préfixe du nom, reste.
#[test]
fn un_dossier_mis_a_la_corbeille_retire_ses_seules_pistes_4896() {
    let db = base();
    let (racine, pistes) = coffret_indexe(&db, "dossier-corbeille");
    let ancien = pistes[0].parent().unwrap().to_path_buf();
    // Le voisin au nom préfixé, indexé par le surveillant lui-même.
    let voisin = ancien.with_file_name("Multichannel 7.1 bis");
    std::fs::create_dir_all(&voisin).unwrap();
    let voisines = sous(&voisin, &pistes);
    for (depuis, vers) in pistes.iter().zip(&voisines) {
        std::fs::copy(depuis, vers).unwrap();
    }
    surveiller(
        &db,
        &racine,
        vec![ev(EventKind::Create(CreateKind::Any), &voisin)],
    );
    let voisines_avant = lignes(&db, &voisines);
    assert!(
        voisines_avant.iter().all(Option::is_some),
        "montage : le voisin est indexé (et c'est déjà un dossier APPARU qui l'a fait)"
    );

    std::fs::remove_dir_all(&ancien).unwrap();
    let racines = vec![chaine(&racine)];
    let mut attente = Vec::new();
    let reste = un_tour(
        &db,
        &racines,
        rejouer_evenements_notify(vec![ev(EventKind::Remove(RemoveKind::Any), &ancien)]),
        &mut attente,
    );
    assert!(
        lignes(&db, &pistes).iter().all(Option::is_some),
        "un lot d'attente : son nouveau nom pourrait encore arriver"
    );
    un_tour(&db, &racines, reste, &mut attente);
    assert!(
        lignes(&db, &pistes).iter().all(Option::is_none),
        "#4896 — le dossier mis à la corbeille ne laisse pas ses pistes en bibliothèque"
    );
    assert_eq!(
        lignes(&db, &voisines),
        voisines_avant,
        "portée : « Multichannel 7.1 bis » n'est pas sous « Multichannel 7.1 »"
    );
}

/// Contre-épreuve de l'appariement : un dossier disparu et un dossier apparu
/// dont les fichiers portent les MÊMES noms mais pas le même contenu ne sont
/// pas un renommage. L'ancien part, le nouveau entre comme un album neuf.
#[test]
fn deux_albums_aux_memes_noms_de_fichiers_ne_s_apparient_pas_4896() {
    let db = base();
    let (racine, pistes) = coffret_indexe(&db, "dossier-faux-jumeau");
    let avant = lignes(&db, &pistes);
    let ancien = pistes[0].parent().unwrap().to_path_buf();
    std::fs::remove_dir_all(&ancien).unwrap();
    let autre = racine.join("Pink Floyd").join("Animals");
    std::fs::create_dir_all(&autre).unwrap();
    let autres = sous(&autre, &pistes);
    for (i, p) in autres.iter().enumerate() {
        std::fs::write(p, flac_8_canaux()).unwrap();
        let n = (i + 1).to_string();
        baliser(
            p,
            &[
                ("TITLE", "Pigs On The Wing, un titre bien plus long"),
                ("ARTIST", "Pink Floyd"),
                ("ALBUM", "Animals"),
                ("TRACKNUMBER", &n),
            ],
            std::time::SystemTime::now(),
        );
    }
    surveiller(
        &db,
        &racine,
        vec![
            ev(EventKind::Remove(RemoveKind::Any), &ancien),
            ev(EventKind::Create(CreateKind::Any), &autre),
        ],
    );
    assert!(
        lignes(&db, &pistes).iter().all(Option::is_none),
        "l'ancien est parti"
    );
    let apres = lignes(&db, &autres);
    assert!(apres.iter().all(Option::is_some), "le nouveau est indexé");
    for (a, b) in avant.iter().zip(&apres) {
        assert_ne!(
            a.unwrap().0,
            b.unwrap().0,
            "un autre album n'hérite pas des lignes (favoris, écoutes) du disparu"
        );
    }
}

/// Un dossier entré dans la racine depuis ailleurs (Windows `ADDED`, macOS
/// `ItemRenamed`, Linux `MOVED_TO`) : aucun événement pour ses fichiers, et
/// pourtant il est indexé.
#[test]
fn un_dossier_entre_dans_la_racine_est_indexe_4896() {
    for (i, (moteur, kind)) in [
        ("Windows", EventKind::Create(CreateKind::Any)),
        ("macOS", nom(RenameMode::Any)),
        ("Linux", nom(RenameMode::To)),
    ]
    .into_iter()
    .enumerate()
    {
        let db = base();
        let (racine, pistes) = coffret_indexe(&db, &format!("dossier-entre-{i}"));
        let dehors = tune_core::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            &format!("surveillant-4896-dehors-{i}"),
        );
        let depose = dehors.join("Wish You Were Here");
        std::fs::create_dir_all(&depose).unwrap();
        let neuves = sous(&depose, &pistes);
        for (j, p) in neuves.iter().enumerate() {
            std::fs::write(p, flac_8_canaux()).unwrap();
            let n = (j + 1).to_string();
            baliser(
                p,
                &[
                    ("TITLE", "Shine On You Crazy Diamond"),
                    ("ARTIST", "Pink Floyd"),
                    ("ALBUM", "Wish You Were Here"),
                    ("TRACKNUMBER", &n),
                ],
                std::time::SystemTime::now(),
            );
        }
        let entre = racine.join("Pink Floyd").join("Wish You Were Here");
        std::fs::rename(&depose, &entre).unwrap();
        surveiller(&db, &racine, vec![ev(kind, &entre)]);
        assert!(
            lignes(&db, &sous(&entre, &pistes))
                .iter()
                .all(Option::is_some),
            "{moteur} : le dossier entré est indexé sans attendre le scan"
        );
        assert_eq!(
            nombre_de_pistes(&db),
            4,
            "{moteur} : l'ancien album est intact"
        );
    }
}

/// La séquence Windows d'une retouche Mp3tag (fichier temporaire du même
/// dossier) produit un « disparu » sur le temporaire : sans piste sous ce
/// chemin, il ne touche à rien.
#[test]
fn un_chemin_disparu_sans_piste_ne_touche_a_rien_4896() {
    let db = base();
    let (racine, pistes) = coffret_indexe(&db, "dossier-temporaire");
    let avant = lignes(&db, &pistes);
    let tmp = pistes[0].with_extension("tmp");
    let racines = vec![chaine(&racine)];
    let mut attente = Vec::new();
    let reste = un_tour(
        &db,
        &racines,
        rejouer_evenements_notify(vec![ev(nom(RenameMode::From), &tmp)]),
        &mut attente,
    );
    assert!(
        attente.is_empty(),
        "rien à attendre : aucune piste sous {tmp:?}"
    );
    un_tour(&db, &racines, reste, &mut attente);
    assert_eq!(lignes(&db, &pistes), avant);
}

/// Arbitrage #1943 : une racine illisible (partage tombé) ou un dossier hors
/// de toute racine configurée ne perdent aucune piste.
#[cfg(unix)]
#[test]
fn un_dossier_disparu_sous_une_racine_illisible_ou_hors_perimetre_garde_ses_pistes_4896() {
    use std::os::unix::fs::PermissionsExt;
    let db = base();
    let (racine, pistes) = coffret_indexe(&db, "dossier-illisible");
    let avant = lignes(&db, &pistes);
    let ancien = pistes[0].parent().unwrap().to_path_buf();
    std::fs::remove_dir_all(&ancien).unwrap();
    let evenement =
        || rejouer_evenements_notify(vec![ev(EventKind::Remove(RemoveKind::Any), &ancien)]);

    // Hors périmètre : la seule racine configurée est ailleurs.
    let ailleurs = vec![chaine(&racine.join("Autre racine"))];
    let mut attente = Vec::new();
    let reste = un_tour(&db, &ailleurs, evenement(), &mut attente);
    un_tour(&db, &ailleurs, reste, &mut attente);
    assert_eq!(
        lignes(&db, &pistes),
        avant,
        "hors périmètre : rien ne part (#1943)"
    );

    // Racine illisible : le montage est tombé.
    let racines = vec![chaine(&racine)];
    std::fs::set_permissions(&*racine, std::fs::Permissions::from_mode(0o000)).unwrap();
    let lisible_quand_meme = std::fs::read_dir(&*racine).is_ok();
    let reste = un_tour(&db, &racines, evenement(), &mut attente);
    un_tour(&db, &racines, reste, &mut attente);
    std::fs::set_permissions(&*racine, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !lisible_quand_meme {
        assert_eq!(
            lignes(&db, &pistes),
            avant,
            "racine illisible : rien ne part (#1943)"
        );
    }
}
