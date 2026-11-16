//! web#3090 (fil 2849) — `DELETE /playlists/{id}` laisse une ligne INFO
//! `playlist_supprimee` (identifiant, nom, nombre de lignes, profil), et
//! rien quand rien n'est supprimé.

use super::supprimer_et_journaliser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

/// Lance `f` sous un abonné qui écrit dans un tampon, et rend ce tampon.
fn journal_de<R>(f: impl FnOnce() -> R) -> (R, String) {
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(journal.clone())
        .finish();
    let r = tracing::subscriber::with_default(abonne, f);
    let texte = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
    (r, texte)
}

#[test]
fn la_suppression_laisse_une_ligne_info_avec_id_nom_lignes_et_profil() {
    let db = base();
    let pistes = TrackRepo::with_backend(db.clone());
    let ids: Vec<i64> = (0..2)
        .map(|i| {
            let mut t = Track::new(format!("T{i}"));
            t.file_path = Some(format!("/musique/t{i}.flac"));
            pistes.create(&t).unwrap()
        })
        .collect();
    let repo = PlaylistRepo::with_backend(db);
    let id = repo.create("Dimanche", None, 1).unwrap();
    repo.add_tracks(id, &ids, None).unwrap();

    let (r, texte) = journal_de(|| supprimer_et_journaliser(&repo, id, 1));

    assert_eq!(r, Ok(true));
    assert!(
        repo.get(id).unwrap().is_none(),
        "la playlist doit être supprimée"
    );
    let ligne = texte
        .lines()
        .find(|l| l.contains("playlist_supprimee"))
        .unwrap_or_else(|| panic!("aucune ligne playlist_supprimee dans : {texte}"));
    assert!(ligne.contains("INFO"), "{ligne}");
    assert!(ligne.contains(&format!("playlist_id={id}")), "{ligne}");
    assert!(ligne.contains("profile_id=1"), "{ligne}");
    assert!(ligne.contains("nom=Dimanche"), "{ligne}");
    assert!(ligne.contains("lignes=2"), "{ligne}");
}

#[test]
fn rien_au_journal_quand_rien_n_est_supprime() {
    let db = base();
    let repo = PlaylistRepo::with_backend(db);
    let id = repo.create("Dimanche", None, 1).unwrap();

    // Un autre profil ne supprime rien (404) : pas de ligne.
    let (r, texte) = journal_de(|| supprimer_et_journaliser(&repo, id, 2));
    assert_eq!(r, Ok(false));
    assert!(repo.get(id).unwrap().is_some());
    assert!(!texte.contains("playlist_supprimee"), "{texte}");

    // Une playlist inexistante non plus.
    let (r, texte) = journal_de(|| supprimer_et_journaliser(&repo, id + 1000, 1));
    assert_eq!(r, Ok(false));
    assert!(!texte.contains("playlist_supprimee"), "{texte}");
}
