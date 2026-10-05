//! #5222 : FLAC image + CUE, vrais scans et lots du surveillant, SQLite sur disque.
use super::pochettes_disque_tests_5034::{
    COVER, JAQUETTE, JAQUETTE_2, Passe, lot_du_surveillant, poser_cover, poser_jaquette, racine,
    scan_de_demarrage, scan_manuel,
};
use crate::state::AppState;
use std::path::{Path, PathBuf};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::SourcePochette;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::artwork::content_hash;

struct Banc {
    _racine: tune_core::test_scratch::ScratchDir,
    etat: AppState,
    musique: PathBuf,
    image: PathBuf,
    cue: PathBuf,
}

impl Banc {
    fn nouveau(nom: &str, jaquette: Option<&[u8]>) -> Self {
        let r = racine(nom);
        let musique = r.join("musique");
        let dossier = musique.join("Didier").join("Album CUE");
        std::fs::create_dir_all(&dossier).unwrap();
        let image = dossier.join("image.flac");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac"),
            &image,
        )
        .unwrap();
        poser_jaquette(&image, jaquette);
        poser_cover(&dossier, Some(COVER));
        let cue = dossier.join("album.cue");
        std::fs::write(&cue, "PERFORMER \"Didier\"\nTITLE \"Album CUE\"\nFILE \"image.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Un\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Deux\"\n    INDEX 01 00:01:00\n").unwrap();
        let etat =
            AppState::new(&r.join("tune.db").to_string_lossy(), 0, Default::default()).unwrap();
        SettingsRepo::with_backend(etat.backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&[musique.to_string_lossy()]).unwrap(),
            )
            .unwrap();
        Self {
            _racine: r,
            etat,
            musique,
            image,
            cue,
        }
    }

    /// `cover` : `cover.jpg` a bougé (posé, remplacé ou retiré) dans ce lot.
    fn surveiller(&self, cover: bool) {
        let covers = if cover {
            let chemin = self.image.parent().unwrap().join("cover.jpg");
            let existe = chemin.exists();
            vec![(chemin, existe)]
        } else {
            vec![]
        };
        lot_du_surveillant(
            &self.etat.backend,
            &self.musique,
            &[self.image.clone(), self.cue.clone()],
            &covers,
        );
    }

    fn album(&self) -> i64 {
        let lignes = self
            .etat
            .backend
            .query_many(
                "SELECT album_id, file_path FROM tracks WHERE cue_media_path = ?",
                &[&self.image.to_string_lossy().into_owned()],
            )
            .unwrap();
        assert_eq!(
            lignes.len(),
            2,
            "le banc doit réellement importer les deux tranches CUE"
        );
        assert!(lignes.iter().all(|r| r[1].as_string().is_none()));
        lignes[0][0].as_i64().unwrap()
    }

    fn verifier(&self, octets: &[u8], source: SourcePochette, message: &str) {
        let etat = AlbumRepo::with_backend(self.etat.backend.clone())
            .etat_pochette(self.album())
            .unwrap()
            .unwrap();
        assert_eq!(
            etat.cover_path.as_deref(),
            Some(content_hash(octets).as_str()),
            "#5222 : {message}"
        );
        assert_eq!(etat.source, Some(source));
        if source == SourcePochette::Integree {
            assert_eq!(
                etat.fichier.as_deref(),
                Some(self.image.to_string_lossy().as_ref())
            );
        }
    }

    async fn passer(&self, passe: Passe) {
        self.passer_avec(passe, false).await;
    }

    /// `cover` : le geste a touché `cover.jpg` (le surveillant en reçoit
    /// l'événement).
    async fn passer_avec(&self, passe: Passe, cover: bool) {
        match passe {
            Passe::Rapide => scan_manuel(&self.etat, false, None).await,
            Passe::Repertoires => scan_manuel(&self.etat, false, self.image.parent()).await,
            Passe::Complete => scan_manuel(&self.etat, true, None).await,
            Passe::Demarrage => scan_de_demarrage(&self.etat.backend).await,
            Passe::Surveillant => self.surveiller(cover),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cue_import_surveillant_pose_la_jaquette_5222() {
    let b = Banc::nouveau("cue-import-5222", Some(JAQUETTE));
    // Sans `cover.jpg` : l'image du dossier passe avant la jaquette (#5685),
    // c'est donc seule que la jaquette de l'image CUE doit être lue.
    poser_cover(b.image.parent().unwrap(), None);
    b.surveiller(false);
    b.verifier(
        JAQUETTE,
        SourcePochette::Integree,
        "le nouvel album CUE reste sans jaquette intégrée",
    );
}

async fn reprendre_cover(passe: Passe) {
    let b = Banc::nouveau(&format!("cue-reprise-{passe:?}-5222"), None);
    let dossier = b.image.parent().unwrap().to_path_buf();
    b.surveiller(true);
    b.verifier(
        COVER,
        SourcePochette::Dossier,
        "montage : cover.jpg doit être en place",
    );
    // #5685 — l'image du dossier passe avant la jaquette intégrée.
    poser_jaquette(&b.image, Some(JAQUETTE));
    b.passer(passe).await;
    b.verifier(
        COVER,
        SourcePochette::Dossier,
        &format!("{passe:?} : cover.jpg doit rester devant la jaquette CUE (#5685)"),
    );
    poser_cover(&dossier, None);
    b.passer_avec(passe, true).await;
    b.verifier(
        JAQUETTE,
        SourcePochette::Integree,
        &format!("{passe:?} : le retrait de cover.jpg doit reprendre la jaquette CUE"),
    );
    poser_jaquette(&b.image, Some(JAQUETTE_2));
    b.passer(passe).await;
    b.verifier(
        JAQUETTE_2,
        SourcePochette::Integree,
        "la jaquette CUE retouchée doit suivre le disque",
    );
    poser_cover(&dossier, Some(COVER));
    b.passer_avec(passe, true).await;
    b.verifier(
        COVER,
        SourcePochette::Dossier,
        &format!("{passe:?} : cover.jpg reposé doit repasser devant la jaquette CUE"),
    );
}

macro_rules! reprise {
    ($nom:ident, $passe:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $nom() {
            reprendre_cover(Passe::$passe).await;
        }
    };
}
reprise!(cue_rapide_reprend_cover_5222, Rapide);
reprise!(cue_repertoires_reprend_cover_5222, Repertoires);
reprise!(cue_complet_reprend_cover_5222, Complete);
reprise!(cue_demarrage_reprend_cover_5222, Demarrage);
reprise!(cue_surveillant_reprend_cover_5222, Surveillant);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cue_pochette_televersee_reste_protegee_5222() {
    let b = Banc::nouveau("cue-televersee-5222", Some(JAQUETTE));
    b.surveiller(true);
    AlbumRepo::with_backend(b.etat.backend.clone())
        .force_update_cover_path(b.album(), &content_hash(COVER), SourcePochette::Televersee)
        .unwrap();
    for passe in [
        Passe::Surveillant,
        Passe::Rapide,
        Passe::Complete,
        Passe::Demarrage,
    ] {
        b.passer(passe).await;
        b.verifier(
            COVER,
            SourcePochette::Televersee,
            "une pochette téléversée doit rester protégée",
        );
    }
}
