//! #8521 (suite de #7307) — « écrire les métadonnées dans les fichiers » et
//! « chercher les paroles en ligne » reviennent décochés après un
//! redémarrage, pas à chaque fois (Tades, ~560 000 pistes, SQLite, rc11).
//!
//! Trois témoins, sur une base de FICHIER (sur `:memory:`, le pool de lecture
//! est la connexion d'écriture elle-même, et rien ne « redémarre ») :
//!
//! 1. Le chemin ordinaire tient : un consentement posé survit à cinq
//!    redémarrages simulés (fermeture, réouverture, schéma et migrations
//!    rejoués comme au démarrage). Ni migration ni amorçage ne le remet à
//!    faux.
//! 2. Chaque écriture d'un consentement laisse une ligne qui nomme sa valeur
//!    et son AUTEUR (le lieu du code : route, tâche, import).
//! 3. Le mécanisme qui perd une écriture au redémarrage : une transaction
//!    laissée ouverte par un autre fil sur la connexion d'écriture
//!    (`transaction_du_lot.rs`, déclarée orpheline). L'écriture y entre, la
//!    connexion d'écriture la relit — l'écran affiche la case cochée — et le
//!    redémarrage l'efface. Le témoin le reproduit, et prouve que la ligne
//!    `reglage_consenti_non_valide` le nomme désormais.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::{CONSENTEMENTS_JOURNALISES, SettingsRepo};
use crate::db::sqlite::SqliteDb;

const ECRITURE: &str = crate::metadata::ecriture_fichiers::CLE;
const PAROLES: &str = crate::library::lyrics_pass::SETTING_LRCLIB_ENABLED;

/// Ouvre la base comme le démarrage : fichier, schéma, migrations.
fn demarrer(chemin: &std::path::Path) -> Arc<SqliteDb> {
    let db = SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    db.init_schema().expect("schéma");
    crate::db::migrations::run_migrations(&db).expect("migrations");
    Arc::new(db)
}

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capturer(f: impl FnOnce()) -> String {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(abonne, f);
    String::from_utf8(journal.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn les_deux_consentements_suivis_sont_ceux_de_l_ecran() {
    assert_eq!(CONSENTEMENTS_JOURNALISES, [ECRITURE, PAROLES]);
}

#[test]
fn un_consentement_pose_survit_a_cinq_redemarrages() {
    let dossier = crate::test_scratch::scratch_dir("consentements-8521-redemarrages");
    let chemin = dossier.join("tune.db");
    {
        let db: Arc<dyn DbBackend> = demarrer(&chemin);
        let reglages = SettingsRepo::with_backend(db);
        reglages.set(ECRITURE, "true").unwrap();
        reglages.set(PAROLES, "true").unwrap();
    }
    for n in 1..=5 {
        let db: Arc<dyn DbBackend> = demarrer(&chemin);
        let reglages = SettingsRepo::with_backend(db);
        for cle in [ECRITURE, PAROLES] {
            assert_eq!(
                reglages.get(cle).unwrap().as_deref(),
                Some("true"),
                "{cle} n'est plus coché après le redémarrage n° {n}"
            );
        }
    }
}

#[test]
fn chaque_ecriture_d_un_consentement_nomme_sa_valeur_et_son_auteur() {
    let dossier = crate::test_scratch::scratch_dir("consentements-8521-journal");
    let db: Arc<dyn DbBackend> = demarrer(&dossier.join("tune.db"));
    let reglages = SettingsRepo::with_backend(db);
    let texte = capturer(|| {
        reglages.set(ECRITURE, "true").unwrap();
        reglages.set(PAROLES, "false").unwrap();
        reglages.delete(PAROLES).unwrap();
        // Un réglage ordinaire n'est pas concerné.
        reglages.set("theme", "dark").unwrap();
    });
    let lignes: Vec<&str> = texte
        .lines()
        .filter(|l| l.contains("reglage_consenti_ecrit"))
        .collect();
    assert_eq!(
        lignes.len(),
        3,
        "trois écritures de consentement attendues :\n{texte}"
    );
    assert!(
        lignes[0].contains(ECRITURE) && lignes[0].contains("true"),
        "{texte}"
    );
    assert!(
        lignes[1].contains(PAROLES) && lignes[1].contains("false"),
        "{texte}"
    );
    assert!(
        lignes[2].contains(PAROLES) && lignes[2].contains("<supprimée>"),
        "{texte}"
    );
    for l in &lignes {
        assert!(
            l.contains("consentements_redemarrage_8521_tests.rs"),
            "l'auteur de l'écriture (lieu du code) n'est pas nommé : {l}"
        );
    }
    assert!(
        !texte.contains("theme"),
        "un réglage ordinaire est journalisé :\n{texte}"
    );
    assert!(!texte.contains("reglage_consenti_non_valide"), "{texte}");
}

#[test]
fn une_ecriture_tombee_dans_une_transaction_etrangere_est_signalee_et_perdue_au_redemarrage() {
    let dossier = crate::test_scratch::scratch_dir("consentements-8521-orpheline");
    let chemin = dossier.join("tune.db");
    {
        let sqlite = demarrer(&chemin);
        // Une attente courte : l'écrivain cesse vite d'attendre la
        // transaction et y entre, comme en production après 30 s.
        sqlite
            .connection()
            .fixer_attente_max_pour_essai(Duration::from_millis(200));
        let db: Arc<dyn DbBackend> = sqlite.clone();
        // Un autre fil ouvre une transaction et ne la ferme pas.
        let db_du_fil = db.clone();
        std::thread::spawn(move || {
            db_du_fil
                .execute_batch("BEGIN IMMEDIATE")
                .expect("BEGIN laissé ouvert");
        })
        .join()
        .expect("fil de la transaction");

        let reglages = SettingsRepo::with_backend(db.clone());
        let texte = capturer(|| {
            reglages.set(ECRITURE, "true").unwrap();
        });
        // Relue par la connexion d'écriture, la case paraît cochée : c'est
        // ce que `GET /system/config` publie, et ce que l'écran montre.
        assert_eq!(reglages.get(ECRITURE).unwrap().as_deref(), Some("true"));
        assert!(
            texte.contains("reglage_consenti_non_valide"),
            "l'écriture non validée n'est pas signalée :\n{texte}"
        );
        // Arrêt du serveur sans COMMIT de la transaction étrangère.
    }
    let db: Arc<dyn DbBackend> = demarrer(&chemin);
    assert_eq!(
        SettingsRepo::with_backend(db).get(ECRITURE).unwrap(),
        None,
        "l'écriture tombée dans la transaction étrangère a survécu au \
         redémarrage : le mécanisme décrit n'est plus celui-là"
    );
}
