//! T2 (#5325) : le jeton de liaison du serveur au compte n'entre JAMAIS au
//! journal — ni à la délivrance, ni sur `sync`, ni sur un refus.
//!
//! Un binaire à lui seul, un seul test, un abonné GLOBAL posé avant tout le
//! reste, niveau TRACE, toutes cibles (même méthode que `journal.rs`).

mod commun;

use std::io::Write;
use std::sync::{Arc, Mutex};

use tune_core::cloud::library_sync;
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn le_jeton_de_liaison_n_entre_jamais_au_journal() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .init();

    let faux = demarrer().await;
    faux.etat.lock().unwrap().sync_exige_liaison = true;
    let backend = base(&faux.base, Some(JETON));
    let s = SettingsRepo::with_backend(backend.clone());
    s.set("server_id", SERVEUR_DU_COMPTE).unwrap();
    backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
             INSERT INTO albums (id, title, artist_id) VALUES (10, 'Kind of Blue', 20);\
             INSERT INTO tracks (id, title, album_id, artist_id) VALUES (1, 'So What', 10, 20);",
        )
        .unwrap();
    library_sync::populate_changelog_after_scan(&backend);
    let api = format!("{}/api/v1/cloud-library", faux.base);
    let client = reqwest::Client::new();

    // Liaison à la demande, puis synchro qui présente le jeton.
    library_sync::push_changes_vers(&backend, &client, &api, SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let premier = s.get(library_sync::CLE_JETON_DE_LIAISON).unwrap().unwrap();

    // Jeton ancien : refusé, relié, réessayé.
    s.set(
        library_sync::CLE_JETON_DE_LIAISON,
        "jeton-ancien-SECRET-5325",
    )
    .unwrap();
    library_sync::record_change(&backend, "track", 1, "upsert");
    library_sync::push_changes_vers(&backend, &client, &api, SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let second = s.get(library_sync::CLE_JETON_DE_LIAISON).unwrap().unwrap();

    // Refus persistant, puis liaison refusée.
    faux.etat.lock().unwrap().jeton_toujours_refuse = true;
    library_sync::record_change(&backend, "track", 1, "upsert");
    library_sync::push_changes_vers(&backend, &client, &api, SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    faux.etat.lock().unwrap().liaison_refusee = Some(404);
    library_sync::lier_le_serveur(&backend, &client, &api, SERVEUR_DU_COMPTE, JETON)
        .await
        .ok();

    let journal = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // Contre-épreuve de la capture elle-même : un journal vide passerait tout.
    for attendu in [
        "cloud_server_linked",
        "cloud_server_link_refused",
        "cloud_library_sync_batch_failed",
    ] {
        assert!(
            journal.contains(attendu),
            "évènement {attendu} absent de la capture :\n{journal}"
        );
    }
    for secret in [
        premier.as_str(),
        second.as_str(),
        PREFIXE_JETON_DE_LIAISON,
        "jeton-ancien-SECRET-5325",
        "SECRET-5325",
        JETON,
        "SECRET-5018",
    ] {
        assert!(
            !journal.contains(secret),
            "le journal contient {secret:?} :\n{journal}"
        );
    }
}
