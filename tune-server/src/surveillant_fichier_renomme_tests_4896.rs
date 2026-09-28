//! #4896 (Didier, fil 1904, point 2 « non établi ») — un fichier audio
//! renommé ISOLÉMENT pendant que Tune tourne.
//!
//! Les moteurs natifs signalent un renommage par `Modify(Name)` sur l'ancien
//! ET le nouveau nom. Le gestionnaire les traduisait tous deux en `Modified` ;
//! l'ancien nom n'existant plus, l'attente d'écriture stable le jetait, et sa
//! ligne restait en base sous un chemin mort, à côté d'une ligne NEUVE pour le
//! nouveau nom : une piste en double, dont une illisible, et l'identifiant
//! (favoris, écoutes, étiquettes) perdu. Même trou pour un fichier mis à la
//! corbeille sous macOS (`Name(Any)`).
//!
//! Chaîne de production : événements `notify` bruts → gestionnaire du
//! surveillant → `settle_partition` → `traiter_le_lot_du_surveillant`.
//!
//! ⚠️ Base de FICHIER : sur `:memory:`, le pool de lecture clone la connexion
//! d'écriture et voit ce qu'une base réelle ne verrait pas.
use super::surveillant_retouche_tests_4896::coffret_indexe;
use super::{ReglagesDuSurveillant, settle_partition, traiter_le_lot_du_surveillant};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::notify::Event;
use tune_core::scanner::watcher::notify::event::{
    CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode,
};
use tune_core::scanner::watcher::rejouer_evenements_notify;

fn base_fichier(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("renomme-4896-base-{epreuve}"));
    let chemin = dossier.join("tune-epreuve.db");
    let db =
        tune_core::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    (dossier, Arc::new(db))
}

fn ev(kind: EventKind, chemin: &Path) -> Event {
    Event::new(kind).add_path(chemin.to_path_buf())
}

fn nom(mode: RenameMode) -> EventKind {
    EventKind::Modify(ModifyKind::Name(mode))
}

fn chaine(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Les événements, puis deux tours à vide, comme la boucle de
/// `spawn_file_watcher`.
fn surveiller(db: &Arc<dyn DbBackend>, racine: &Path, evenements: Vec<Event>) {
    let racines = vec![chaine(racine)];
    let mut attente = Vec::new();
    let mut a_suivre = rejouer_evenements_notify(evenements);
    for _ in 0..3 {
        let (changes, mut en_ecriture) = settle_partition(a_suivre, &[]);
        let a_relire = traiter_le_lot_du_surveillant(
            db,
            changes,
            &ReglagesDuSurveillant {
                exclusions: &[],
                racines: &racines,
                quality_split: true,
            },
            &mut attente,
        );
        en_ecriture.extend(a_relire);
        a_suivre = en_ecriture;
    }
}

/// (identifiant de piste, identifiant d'album) du fichier, en base.
fn ligne(db: &Arc<dyn DbBackend>, piste: &Path) -> Option<(i64, i64)> {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&chaine(piste))
        .unwrap()
        .map(|t| (t.id.unwrap(), t.album_id.unwrap()))
}

fn nombre_de_pistes(db: &Arc<dyn DbBackend>) -> i64 {
    TrackRepo::with_backend(db.clone()).count().unwrap()
}

/// Le témoin, joué pour chaque moteur : « 01 - Speak To Me.flac » renommé
/// dans son dossier.
#[test]
fn un_fichier_renomme_dans_son_dossier_garde_sa_ligne_sur_chaque_moteur_4896() {
    type Sequence = fn(&Path, &Path) -> Vec<Event>;
    let cas: [(&str, Sequence); 4] = [
        ("Windows (RENAMED_OLD_NAME, RENAMED_NEW_NAME)", |a, n| {
            vec![ev(nom(RenameMode::From), a), ev(nom(RenameMode::To), n)]
        }),
        ("macOS FSEvents (deux ItemRenamed sans lien)", |a, n| {
            vec![ev(nom(RenameMode::Any), a), ev(nom(RenameMode::Any), n)]
        }),
        ("Linux inotify (MOVED_FROM, MOVED_TO, paire)", |a, n| {
            vec![
                ev(nom(RenameMode::From), a),
                ev(nom(RenameMode::To), n),
                Event::new(nom(RenameMode::Both))
                    .add_path(a.to_path_buf())
                    .add_path(n.to_path_buf()),
            ]
        }),
        (
            "PollWatcher (partage réseau) : disparaît, apparaît",
            |a, n| {
                vec![
                    ev(EventKind::Remove(RemoveKind::Any), a),
                    ev(EventKind::Create(CreateKind::Any), n),
                ]
            },
        ),
    ];
    for (i, (moteur, sequence)) in cas.into_iter().enumerate() {
        let (_base, db) = base_fichier(&format!("fichier-{i}"));
        let (racine, pistes) = coffret_indexe(&db, &format!("fichier-renomme-{i}"));
        let ancien: PathBuf = pistes[0].clone();
        let avant = ligne(&db, &ancien);
        assert!(avant.is_some(), "montage : piste indexée");
        assert_eq!(nombre_de_pistes(&db), 2, "montage : deux pistes");
        let nouveau = ancien.with_file_name("01 - Speak to Me (remaster).flac");
        std::fs::rename(&ancien, &nouveau).unwrap();

        surveiller(&db, &racine, sequence(&ancien, &nouveau));

        assert_eq!(
            ligne(&db, &nouveau),
            avant,
            "{moteur} : la piste garde sa ligne (identifiant et album) sous son nouveau nom"
        );
        assert_eq!(
            ligne(&db, &ancien),
            None,
            "{moteur} : plus rien sous l'ancien nom"
        );
        assert_eq!(
            nombre_de_pistes(&db),
            2,
            "{moteur} : aucune piste en double"
        );
    }
}

/// macOS : mettre UN fichier à la corbeille est un renommage vers `~/.Trash`,
/// hors de la racine — le surveillant ne reçoit que `Name(Any)` sur l'ancien
/// nom. La piste part ; sa voisine reste.
#[test]
fn un_fichier_mis_a_la_corbeille_sous_macos_quitte_la_bibliotheque_4896() {
    let (_base, db) = base_fichier("corbeille");
    let (racine, pistes) = coffret_indexe(&db, "fichier-corbeille");
    let corbeille = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "surveillant-4896-corbeille-hors-racine",
    );
    let ancien = pistes[0].clone();
    std::fs::rename(&ancien, corbeille.join("01 - Speak To Me.flac")).unwrap();

    surveiller(&db, &racine, vec![ev(nom(RenameMode::Any), &ancien)]);

    assert_eq!(
        ligne(&db, &ancien),
        None,
        "la piste mise à la corbeille part"
    );
    assert!(ligne(&db, &pistes[1]).is_some(), "sa voisine reste");
    assert_eq!(nombre_de_pistes(&db), 1);
}

/// Un fichier déplacé dans un AUTRE dossier n'est pas apparié (il change
/// d'album) : l'ancien nom part, le nouveau entre, sans doublon ni ligne morte.
#[test]
fn un_fichier_deplace_dans_un_autre_dossier_ne_laisse_pas_de_ligne_morte_4896() {
    let (_base, db) = base_fichier("autre-dossier");
    let (racine, pistes) = coffret_indexe(&db, "fichier-autre-dossier");
    let ancien = pistes[0].clone();
    let ailleurs = racine.join("Divers");
    std::fs::create_dir_all(&ailleurs).unwrap();
    let nouveau = ailleurs.join("01 - Speak To Me.flac");
    std::fs::rename(&ancien, &nouveau).unwrap();

    surveiller(
        &db,
        &racine,
        vec![
            ev(nom(RenameMode::From), &ancien),
            ev(nom(RenameMode::To), &nouveau),
        ],
    );

    assert_eq!(
        ligne(&db, &ancien),
        None,
        "plus de ligne morte sous l'ancien nom"
    );
    assert!(
        ligne(&db, &nouveau).is_some(),
        "le nouveau chemin est indexé"
    );
    assert_eq!(nombre_de_pistes(&db), 2, "aucune piste en double");
}
