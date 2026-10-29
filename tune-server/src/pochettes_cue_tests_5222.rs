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
    #[allow(clippy::await_holding_lock)]
    async fn passer_avec(&self, passe: Passe, cover: bool) {
        // Ces aides #5034 supposent que l'appelant tient le verrou des scans.
        // Sans lui, les passes CUE prennent le droit global pendant que #6729
        // vérifie que son scan ciblé a fini. Garder le verrou jusqu'au retour
        // de la passe, qui attend aussi la restitution du droit de scanner.
        let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
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

/// L'interférence de #6729, sans ordonnanceur favorable ni délai : un autre
/// témoin tient le verrou des scans, puis on sonde UNE fois la vraie passe
/// CUE. Elle doit attendre le verrou avant de démarrer son scan global.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn les_passes_cue_attendent_le_verrou_des_scans_6729() {
    use std::task::Poll;
    for passe in [
        Passe::Rapide,
        Passe::Repertoires,
        Passe::Complete,
        Passe::Demarrage,
    ] {
        let seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
        crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
        let b = Banc::nouveau(&format!("cue-isolement-{passe:?}-6729"), Some(JAQUETTE));
        // Même si le scan est très rapide, il ne peut pas finir entre le
        // sondage de la future et le constat du droit : sa transaction attend
        // cette porte. On force un entrelacement, jamais une fenêtre de temps.
        let ecriture = crate::sqlite_write_gate::user_queue().await;
        let mut travail = std::pin::pin!(b.passer(passe));
        let en_attente = std::future::poll_fn(|cx| {
            Poll::Ready(std::future::Future::poll(travail.as_mut(), cx).is_pending())
        })
        .await;
        let droit_pris = crate::routes::system::scan::droit_de_scanner_tenu();
        drop(ecriture);
        drop(seul);
        // Toujours terminer la tâche lancée, même quand la contre-épreuve
        // constate l'interférence : aucun scan ne fuit vers le test suivant.
        if en_attente {
            travail.await;
        }
        assert!(
            en_attente,
            "#6729 : la passe {passe:?} doit attendre le verrou du témoin voisin"
        );
        assert!(
            !droit_pris,
            "#6729 : la passe CUE {passe:?} a pris le droit de scanner sous le verrou du témoin voisin"
        );
    }
}
