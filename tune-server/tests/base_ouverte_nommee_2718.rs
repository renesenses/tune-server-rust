//! Le journal dit QUELLE base a été ouverte (#2718).
//!
//! ## Le défaut mesuré
//!
//! **Belkadi Yacine**, fil forum 1597, 28/08/2026, Tune 0.9.119 **Linux**, un
//! signalement d'une ligne et aucune pièce jointe :
//!
//! > « disparition des serveurs et de la bibliothèque »
//!
//! Le mécanisme n'est **pas** établi, et ce fichier ne prétend pas l'établir.
//! Il ferme un angle mort mesuré dans le code, qui empêche de trancher :
//!
//! * `tune-server/src/config.rs` relocalise la base sous `%LOCALAPPDATA%` sur
//!   **Windows** et sous `~/Library/Application Support/Tune` sur **macOS**
//!   (#3185). Il n'y a **aucun bloc Linux** : `db_path` y reste `"tune.db"`,
//!   résolu contre le **répertoire courant du processus**.
//! * `SqliteDb::open` pose `SQLITE_OPEN_CREATE` : un fichier absent est créé
//!   **vide, en silence**.
//!
//! Le même binaire lancé autrement — un terminal depuis un autre dossier, un
//! `sudo`, un autre compte que l'utilisateur `tune` du service — ouvre donc une
//! **autre** base, vide, et l'écran affiche « plus rien ». Aucune ligne n'a été
//! effacée ; la bibliothèque est dans l'autre fichier.
//!
//! Et le journal ne permettait pas de le voir : `sqlite_opened` annonçait la
//! valeur **brute** de `db_path`, soit `path="tune.db"` — qui ne désigne aucun
//! fichier. La question « quelle base a-t-il ouverte ? » était sans réponse,
//! même avec le journal complet sous les yeux.
//!
//! ⚠️ Ce fichier **ne corrige pas** la relocalisation Linux. La déplacer est un
//! geste de migration qui toucherait la base de tous les testeurs à la mise à
//! jour, et rien dans le signalement de Yacine ne le justifie. Ce qui est
//! corrigé ici est la **mesurabilité** : la prochaine fois, la question se
//! tranche en lisant une ligne.
//!
//! ## Pourquoi un binaire à lui seul, un seul test dedans
//!
//! Deux raisons cumulées, chacune suffisante :
//!
//! 1. `tracing` met en cache, pour tout le processus, la décision « ce point
//!    d'appel intéresse-t-il quelqu'un ? » : un abonné global posé au milieu
//!    d'un binaire qui lance des tests en parallèle rend des captures vides
//!    sans prévenir (`panne_sql_journalisee`, `journal_descriptif_illisible`).
//! 2. Ce test **change le répertoire courant**, qui est global au processus.
//!    Un voisin qui ouvrirait un fichier au même instant le chercherait
//!    ailleurs.
//!
//! ⚠️ `tune-server` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-server/Cargo.toml`, ce fichier ne serait JAMAIS compilé.

use std::sync::{Arc, Mutex};

/// Recueille la sortie `tracing`.
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

#[test]
fn ouvrir_une_base_par_un_chemin_relatif_journalise_le_fichier_reellement_ouvert() {
    let capture = JournalCapture::default();
    // INFO : le niveau ORDINAIRE du service. `sqlite_opened` est posé en
    // `info!`, et c'est ce niveau-là qui doit porter la réponse — une trace
    // visible seulement en `debug` ne serait dans aucun rapport de testeur.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let abri = tempfile::tempdir().expect("répertoire temporaire");
    let ailleurs = abri.path().canonicalize().expect("chemin du répertoire");

    // Le geste exact du défaut : on se place dans un AUTRE dossier, puis on
    // ouvre « tune.db » — le défaut par défaut de la configuration Linux.
    // `set_current_dir` est global au processus ; ce binaire n'a qu'un test,
    // et c'est la raison n° 2 de sa cible dédiée.
    std::env::set_current_dir(&ailleurs).expect("se placer dans le répertoire temporaire");

    let base = tune_core::db::sqlite::SqliteDb::open("tune.db")
        .expect("SQLITE_OPEN_CREATE crée la base si elle n'existe pas — c'est le défaut mesuré");
    drop(base);

    let journal = capture.texte();
    let lignes: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("sqlite_opened"))
        .collect();
    assert_eq!(
        lignes.len(),
        1,
        "ouvrir la base doit laisser UNE ligne `sqlite_opened`.\njournal complet :\n{journal}"
    );
    let ligne = lignes[0];

    // Le cœur du témoin : le chemin ABSOLU, celui qui désigne un fichier.
    let attendu = ailleurs.join("tune.db");
    let attendu = attendu.display().to_string();
    assert!(
        ligne.contains(&format!("chemin_absolu=\"{attendu}\"")),
        "le journal doit nommer le fichier RÉELLEMENT ouvert. Sans lui, « tune.db » \
         ne désigne rien : sur Linux la base se résout contre le répertoire courant, \
         et deux lancements depuis deux dossiers ouvrent deux bases — dont l'une est \
         créée vide en silence. C'est la première hypothèse à écarter quand un \
         testeur écrit « la bibliothèque a disparu ».\n\
         attendu dans la ligne : chemin_absolu=\"{attendu}\"\n\
         ligne reçue : {ligne}"
    );

    // …et la valeur CONFIGURÉE reste à côté : c'est en comparant les deux qu'on
    // voit la résolution, donc qu'on comprend POURQUOI ce fichier-là.
    assert!(
        ligne.contains("path=\"tune.db\""),
        "le chemin tel qu'il a été configuré doit rester dans la ligne — sans lui, \
         on lit un fichier absolu sans savoir qu'il vient d'un chemin relatif :\n{ligne}"
    );

    // Et le fichier est bien là où le journal le dit : une trace qui désignerait
    // un chemin sans rapport serait pire que pas de trace du tout.
    assert!(
        std::path::Path::new(&attendu).exists(),
        "le fichier nommé par le journal doit exister : {attendu}"
    );
}
