//! Le tag « compilation » fait foi, la forme des dossiers sert de repli (C1/C2).
//!
//! ## Ce que ce fichier verrouille
//!
//! Arbitrage de Bertrand du 14/09/2026, chantier « gestion du tag compilation » :
//!
//! - **C1** — le tag fait foi, la forme des dossiers n'intervient qu'en repli.
//!   Le tag est une intention explicite ; la forme n'est qu'une déduction.
//! - **C2** — l'artiste d'un album de compilation est l'artiste d'album
//!   **tagué** s'il existe, et « Various Artists » seulement à défaut.
//!
//! ## Pourquoi un binaire de test à lui seul, et un seul test dedans
//!
//! Leçon déjà payée sept fois par ce dépôt (`panne_sql_journalisee.rs`,
//! `journal_descriptif_illisible.rs`) : `tracing` met en cache, **pour tout le
//! processus**, la décision « ce point d'appel intéresse-t-il quelqu'un ? ». Un
//! abonné posé au milieu d'un binaire qui lance des tests en parallèle se voit
//! priver d'évènements de façon imprévisible, et la capture revient vide sans
//! prévenir — signature : journal vide, `left: 0`.
//!
//! Ici l'abonné est **global**, installé avant toute autre chose, et ce binaire
//! ne contient **qu'un seul test** : rien ne tourne en parallèle, rien d'autre
//! n'enregistre d'abonné, la capture ne dépend d'aucun ordre. Vérifié à
//! `--test-threads=4`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier ne serait JAMAIS
//! compilé sans sa cible `[[test]]` dans `tune-server/Cargo.toml`. Voir
//! `tests_orphelins.rs`, qui refuse tout fichier non enregistré.

use std::sync::{Arc, Mutex};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;
use tune_server::scan_import::{PorteeDuScan, TrackImporter};

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qui dira
/// POURQUOI un album a été jugé compilation.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Une piste à poser sur le disque, telle que le scan la verra.
struct Piste {
    dossier: &'static str,
    fichier: &'static str,
    titre: &'static str,
    artiste: &'static str,
    album: &'static str,
    album_artiste: Option<&'static str>,
    /// Le TAG, tel que le fichier le porte : absent, faux, ou vrai.
    tag: Option<bool>,
    numero: u32,
}

/// Ce qu'un album vaut à l'arrivée, lu dans la BASE et non dans une variable.
#[derive(Debug, PartialEq, Eq)]
struct Verdict {
    compilation: bool,
    artiste: String,
    titre: String,
}

/// Joue un cas de bout en bout : vrais fichiers sur le disque, vrai
/// `TrackImporter`, vrai `begin_batch`, base SQLite neuve. Rend un verdict par
/// album créé, trié par titre.
fn jouer(racine: &std::path::Path, pistes: &[Piste]) -> Vec<Verdict> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let album_repo = AlbumRepo::with_backend(backend.clone());
    let artist_repo = ArtistRepo::with_backend(backend.clone());

    let mut lot = Vec::new();
    for p in pistes {
        let d = racine.join(p.dossier);
        std::fs::create_dir_all(&d).unwrap();
        let chemin = d.join(p.fichier).to_string_lossy().into_owned();
        std::fs::write(&chemin, b"pas-du-vrai-audio").unwrap();
        lot.push(ScannedFile {
            path: chemin,
            metadata: Some(TrackMetadata {
                title: Some(p.titre.to_string()),
                artist: Some(p.artiste.to_string()),
                album: Some(p.album.to_string()),
                album_artist: p.album_artiste.map(str::to_string),
                track_number: Some(p.numero),
                compilation: p.tag,
                ..Default::default()
            }),
            unsupported: None,
            audio_hash: Some(format!("hash-{}-{}", p.dossier, p.fichier)),
            file_size: 4096,
            mtime: 1_700_000_000,
        });
    }

    let mut imp = TrackImporter::new(
        backend.clone(),
        true,
        racine.join("cache"),
        PorteeDuScan::TOUT,
    );
    // Le vrai scan appelle `begin_batch` AVANT d'importer : c'est lui qui
    // construit les deux décisions. Sans lui, aucune des règles mesurées ici
    // n'entre en jeu — les tests du module s'en passent et ne mesurent donc
    // que le repli.
    imp.begin_batch(&lot);
    let mut ids = std::collections::BTreeSet::new();
    for sf in &lot {
        let (piste, _) = imp.import(sf).expect("import");
        if let Some(aid) = piste.album_id {
            ids.insert(aid);
        }
    }

    let mut verdicts: Vec<Verdict> = ids
        .into_iter()
        .map(|aid| {
            let a = album_repo.get(aid).unwrap().unwrap();
            let artiste = a
                .artist_id
                .and_then(|id| artist_repo.get(id).ok().flatten())
                .map(|ar| ar.name)
                .unwrap_or_else(|| "<sans artiste>".into());
            Verdict {
                compilation: a.is_compilation,
                artiste,
                titre: a.title,
            }
        })
        .collect();
    verdicts.sort_by(|a, b| (&a.titre, &a.artiste).cmp(&(&b.titre, &b.artiste)));
    verdicts
}

#[test]
fn le_tag_decide_et_le_journal_dit_pourquoi() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let tmp = tempfile::tempdir().unwrap();

    // ───────────────────────────────────────────────────────────────────────
    // Cas A — #1656 (jfpaquet). Un dossier « VA-xxx », le TAG posé sur les
    // trois pistes, AUCUN artiste d'album. Le tag dit « compilation » et rien
    // ne le contredit : le repli n'a pas à intervenir.
    // C2 : aucun artiste d'album tagué ⇒ « Various Artists ».
    // ───────────────────────────────────────────────────────────────────────
    let a = jouer(
        &tmp.path().join("A"),
        &[
            Piste {
                dossier: "Musique/VA-Les Plus Belles Chansons",
                fichier: "01.flac",
                titre: "Göttingen",
                artiste: "Barbara",
                album: "Les Plus Belles Chansons",
                album_artiste: None,
                tag: Some(true),
                numero: 1,
            },
            Piste {
                dossier: "Musique/VA-Les Plus Belles Chansons",
                fichier: "02.flac",
                titre: "Amsterdam",
                artiste: "Jacques Brel",
                album: "Les Plus Belles Chansons",
                album_artiste: None,
                tag: Some(true),
                numero: 2,
            },
            Piste {
                dossier: "Musique/VA-Les Plus Belles Chansons",
                fichier: "03.flac",
                titre: "Les Copains d'abord",
                artiste: "Georges Brassens",
                album: "Les Plus Belles Chansons",
                album_artiste: None,
                tag: Some(true),
                numero: 3,
            },
        ],
    );
    eprintln!("CAS A (#1656) — obtenu : {a:?}");
    assert_eq!(a.len(), 1, "les trois pistes tiennent dans UN album");
    assert!(a[0].compilation, "#1656 — le tag dit compilation");
    assert_eq!(
        a[0].artiste, "Various Artists",
        "#1656 — aucun artiste d'album tagué ⇒ Various Artists (C2)"
    );
    assert_ne!(
        a[0].artiste, a[0].titre,
        "#1656 — le titre de l'album ne doit JAMAIS devenir l'artiste"
    );

    // ───────────────────────────────────────────────────────────────────────
    // Cas B — #3855 (Pierre M), coffret RCA Reiner. Un dossier de disque, six
    // fichiers, un seul titre d'album, et l'artiste d'album écrit de DEUX
    // façons : « Fritz Reiner » et « Chicago Symphony Orchestra, Fritz
    // Reiner ». La forme crie « compilation » (deux artistes distincts), et
    // les fichiers portent un tag qui dit explicitement le CONTRAIRE.
    //
    // C1 : le tag présent et FAUX l'emporte sur la forme. Ce n'est pas une
    // compilation, et l'artiste n'est pas écrasé par « Various Artists ».
    // ───────────────────────────────────────────────────────────────────────
    const COFFRET: &str = "Reiner/The Complete RCA Album Collection/CD02 Strauss";
    const TITRE_COFFRET: &str = "The Complete RCA Album Collection";
    let deux_graphies = |tag: Option<bool>| {
        vec![
            Piste {
                dossier: COFFRET,
                fichier: "01.flac",
                titre: "Ein Heldenleben I",
                artiste: "Fritz Reiner",
                album: TITRE_COFFRET,
                album_artiste: Some("Fritz Reiner"),
                tag,
                numero: 1,
            },
            Piste {
                dossier: COFFRET,
                fichier: "02.flac",
                titre: "Ein Heldenleben II",
                artiste: "Fritz Reiner",
                album: TITRE_COFFRET,
                album_artiste: Some("Fritz Reiner"),
                tag,
                numero: 2,
            },
            Piste {
                dossier: COFFRET,
                fichier: "03.flac",
                titre: "Ein Heldenleben III",
                artiste: "Fritz Reiner",
                album: TITRE_COFFRET,
                album_artiste: Some("Chicago Symphony Orchestra, Fritz Reiner"),
                tag,
                numero: 3,
            },
        ]
    };

    let b = jouer(&tmp.path().join("B"), &deux_graphies(Some(false)));
    eprintln!("CAS B (#3855, tag=0) — obtenu : {b:?}");
    assert_eq!(b.len(), 1, "#3855 — un dossier de disque, UN album");
    assert!(
        !b[0].compilation,
        "#3855 — C1 : le tag dit « pas une compilation », la forme des dossiers \
         ne peut pas le contredire"
    );
    assert_ne!(
        b[0].artiste, "Various Artists",
        "#3855 — C2 : un coffret tagué ne part pas sous « Various Artists »"
    );

    // ───────────────────────────────────────────────────────────────────────
    // Contre-épreuve de C1 : le MÊME coffret sans aucun tag. Le tag est
    // absent, donc le repli s'applique — et la forme, elle, tranche
    // « compilation ». C2 garde malgré tout l'artiste d'album tagué.
    // ───────────────────────────────────────────────────────────────────────
    let c = jouer(&tmp.path().join("C"), &deux_graphies(None));
    eprintln!("CAS C (#3855, tag absent) — obtenu : {c:?}");
    assert_eq!(c.len(), 1, "sans tag non plus, le dossier reste UN album");
    assert!(
        c[0].compilation,
        "sans tag, la forme des dossiers reprend la main — c'est le REPLI de C1"
    );

    // ───────────────────────────────────────────────────────────────────────
    // Le journal dit POURQUOI. Aujourd'hui, rien ne le dit : c'était déjà au
    // plan du chantier (phase 1, « Journaliser chaque décision »).
    // ───────────────────────────────────────────────────────────────────────
    let journal = capture.texte();
    eprintln!("---- JOURNAL ----\n{journal}\n---- FIN ----");
    assert!(
        journal.contains("compilation_decidee"),
        "le scan doit journaliser sa décision ; journal obtenu :\n{journal}"
    );
    assert!(
        journal.contains("motif=\"tag\"") || journal.contains("motif=tag"),
        "le cas A est décidé PAR LE TAG, et le journal doit le nommer ;\n{journal}"
    );
    assert!(
        journal.contains("motif=\"forme_des_dossiers\"")
            || journal.contains("motif=forme_des_dossiers"),
        "le cas C est décidé par la FORME, et le journal doit le nommer ;\n{journal}"
    );
}
