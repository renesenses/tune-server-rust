//! #8426 — `tokens_saved service=youtube` se lisait à CHAQUE sauvegarde, même
//! sans compte YouTube : `save_tokens` rendait `{"api_key_configured": …}`.
//! Le journal faisait croire à une connexion qui n'avait jamais eu lieu.
use std::sync::Mutex;

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::streaming::youtube::YouTubeService;

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
impl Journal {
    fn lignes_tokens_saved(&self) -> usize {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|l| l.contains("tokens_saved") && l.contains("service=youtube"))
            .count()
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    (journal, garde)
}

/// Sans compte : ni ligne écrite, ni `tokens_saved`. Le témoin positif (un
/// compte connecté journalise bien) prouve que la capture fonctionne.
#[tokio::test]
async fn sans_jeton_youtube_ne_journalise_pas_tokens_saved_8426() {
    let (journal, _garde) = capturer();
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());

    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(YouTubeService::new()));
    registre.save_all_tokens(&db).await;
    assert_eq!(journal.lignes_tokens_saved(), 0, "tokens_saved sans jeton");
    assert_eq!(reglages.get("auth_tokens_youtube").unwrap(), None);

    // Témoin : un compte connecté est bien enregistré et annoncé.
    let mut connecte = ServiceRegistry::new();
    connecte.register(Box::new(
        YouTubeService::new().avec_jeton_pour_essai("jeton-8426"),
    ));
    connecte.save_all_tokens(&db).await;
    assert_eq!(
        journal.lignes_tokens_saved(),
        1,
        "le témoin doit journaliser"
    );
}

/// Une ligne inchangée n'est pas réannoncée ; la déconnexion écrase la ligne
/// de la session close (un redémarrage ne doit pas reconnecter), une fois.
#[tokio::test]
async fn deconnexion_ecrase_la_ligne_et_ne_se_repete_pas_8426() {
    let (journal, _garde) = capturer();
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(
        YouTubeService::new().avec_jeton_pour_essai("jeton-8426"),
    ));

    registre.save_all_tokens(&db).await;
    registre.save_all_tokens(&db).await;
    assert_eq!(
        journal.lignes_tokens_saved(),
        1,
        "ligne inchangée réannoncée"
    );
    assert!(
        reglages
            .get("auth_tokens_youtube")
            .unwrap()
            .unwrap()
            .contains("jeton-8426")
    );

    registre
        .get("youtube")
        .unwrap()
        .write()
        .await
        .logout()
        .await
        .unwrap();
    registre.save_all_tokens(&db).await;
    registre.save_all_tokens(&db).await;
    assert_eq!(journal.lignes_tokens_saved(), 2);
    let apres = reglages.get("auth_tokens_youtube").unwrap().unwrap();
    assert!(!apres.contains("jeton-8426"), "jeton survivant : {apres}");
}
