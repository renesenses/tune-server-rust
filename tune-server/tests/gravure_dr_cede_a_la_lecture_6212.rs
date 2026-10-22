//! #6212 / #6634 — la gravure du Dynamic Range dans les fichiers cède à la
//! lecture, et n'écrit jamais dans le fichier qu'une zone joue.
//!
//! Fil 2347 (Tades, rc6, 20/10) : 1 625 puis 1 650 pistes gravées sur 1 886
//! entre 11 h 46 et 11 h 47, pendant la lecture. La passe
//! (`POST /library/dr/gravure`) n'appelait ni `ceder_a_la_lecture` ni
//! `lecture_en_cours`, et ne regardait pas ce qui jouait.
//!
//! Ce que ces témoins tiennent, par la route montée :
//!
//! 1. **Pendant une lecture**, la passe cède comme les passes freinées de
//!    #4681 (elle est nommée au relevé `throttled`), grave les AUTRES fichiers,
//!    laisse intact octet pour octet celui de la piste jouée, et ne finit
//!    pas tant qu'il est tenu ;
//! 2. **la lecture arrêtée**, elle reprend le fichier reporté et le grave ;
//! 3. **sans lecture**, rien ne change : aucune pause, aucune passe au relevé,
//!    tout est gravé d'un trait.
//!
//! ## Pourquoi une cible `[[test]]` à elle seule
//!
//! Le témoin de lecture et la politique des freins sont des états de
//! PROCESSUS : un voisin qui mettrait une zone en lecture dans le même binaire
//! rendrait ces témoins intermittents (et ferait marquer des pauses de 5 s aux
//! passes des autres). Ils se sérialisent entre eux pour la même raison.
//! `autotests = false` : sans l'entrée du manifeste, ce fichier ne serait
//! jamais compilé.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::Value;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::playback::NowPlaying;
use tune_core::taches_de_fond::priorite::{
    self, ID_GRAVER_DR, noter_etat_de_lecture, oublier_la_lecture_pour_les_essais,
};
use tune_server::state::AppState;

static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ZONE: i64 = 6212;

struct Banc {
    state: AppState,
    port: u16,
    _dossier: tune_core::test_scratch::ScratchDir,
    /// `(track_id, chemin)` des deux pistes à graver.
    pistes: [(i64, String); 2],
}

impl Banc {
    async fn monter() -> Self {
        let dossier = tune_core::test_scratch::scratch_dir("gravure-dr-6212");
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let state = AppState::new(":memory:", port, Default::default()).unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(tune_core::metadata::ecriture_fichiers::CLE, "true")
            .unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tune-core/tests/fixtures/test.flac");
        let meta = TrackMetadataRepo::with_backend(state.backend.clone());
        let mut pistes = Vec::new();
        for (id, nom, dr) in [(1i64, "jouee.flac", "12"), (2, "libre.flac", "9")] {
            let chemin = dossier.path().join(nom);
            std::fs::copy(&fixture, &chemin).unwrap();
            let chemin = chemin.to_string_lossy().into_owned();
            state
                .backend
                .execute(
                    "INSERT INTO tracks (id, title, file_path) VALUES (?1, ?2, ?3)",
                    &[&id, &nom, &chemin],
                )
                .unwrap();
            meta.set(id, "dr_track", dr).unwrap();
            meta.set(id, "dr_source", "analysis").unwrap();
            pistes.push((id, chemin));
        }
        let app = tune_server::routes::router(state.clone());
        tokio::spawn(async move {
            axum::serve(
                ecoute,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self {
            state,
            port,
            _dossier: dossier,
            pistes: pistes.try_into().unwrap(),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1/library/dr/gravure", self.port)
    }

    async fn lancer(&self) {
        let r = reqwest::Client::new()
            .post(self.url())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    }

    async fn statut(&self) -> Value {
        reqwest::get(self.url())
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Attend que `condition` tienne sur le statut, ou échoue en le montrant.
    async fn attendre(&self, quoi: &str, condition: impl Fn(&Value) -> bool) -> Value {
        let mut dernier = Value::Null;
        for _ in 0..400 {
            dernier = self.statut().await;
            if condition(&dernier) {
                return dernier;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{quoi} : jamais atteint, dernier statut {dernier}");
    }

    fn source(&self, track_id: i64) -> Option<String> {
        TrackMetadataRepo::with_backend(self.state.backend.clone())
            .get_all(track_id)
            .unwrap()
            .get("dr_source")
            .cloned()
    }

    /// Une zone joue la piste `track_id` : l'état de lecture du gestionnaire
    /// (ce que l'API rend) et le témoin de lecture des passes de fond
    /// (ce qu'écrit `ZoneRepo::save_play_state`).
    async fn jouer(&self, track_id: i64) {
        self.state
            .playback
            .play(
                ZONE,
                NowPlaying {
                    track_id: Some(track_id),
                    title: "piste jouée".into(),
                    source: "local".into(),
                    ..Default::default()
                },
            )
            .await;
        noter_etat_de_lecture(ZONE, "playing");
    }

    async fn arreter(&self) {
        self.state.playback.stop(ZONE).await;
        noter_etat_de_lecture(ZONE, "stopped");
    }
}

fn a_neuf(pause_ms: u64) {
    oublier_la_lecture_pour_les_essais();
    priorite::politique::regler(priorite::politique::Politique {
        pause_between_items_ms: pause_ms,
        ..priorite::politique::DEFAUT
    });
}

/// 🔴 Le cas du fil 2347 : une zone joue la piste 1 pendant que la gravure
/// tourne. La passe cède (relevé), grave la piste 2, et ne touche PAS au
/// fichier de la piste 1 tant qu'il joue. Lecture arrêtée, elle le reprend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pendant_une_lecture_la_gravure_cede_et_epargne_le_fichier_joue_6212() {
    let _g = VERROU.lock().await;
    a_neuf(50);
    let banc = Banc::monter().await;
    let [(jouee, chemin_joue), (libre, _)] = banc.pistes.clone();
    let octets_avant = std::fs::read(&chemin_joue).unwrap();

    banc.jouer(jouee).await;
    banc.lancer().await;
    // Le statut ne publie ses compteurs que tous les 25 fichiers : ce qui
    // est gravé se lit sur la provenance (`dans_les_fichiers`).
    banc.attendre("un premier fichier gravé", |s| s["dans_les_fichiers"] == 1)
        .await;
    assert!(
        priorite::releve().throttled.contains(&ID_GRAVER_DR),
        "une zone joue : la gravure doit céder comme les passes freinées \
         (#4681) et le dire au relevé, qui nomme {:?}",
        priorite::releve().throttled
    );

    // Plusieurs tours de relecture : le fichier joué reste intact, la passe
    // ne se déclare pas finie.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let statut = banc.statut().await;
    assert!(
        std::fs::read(&chemin_joue).unwrap() == octets_avant,
        "#6212 — la gravure a réécrit le fichier EN LECTURE (statut {statut})"
    );
    assert_eq!(banc.source(jouee).as_deref(), Some("analysis"));
    assert_eq!(banc.source(libre).as_deref(), Some("tag"));
    assert_eq!(statut["status"], "running", "{statut}");

    banc.arreter().await;
    let statut = banc
        .attendre("la passe finie après l'arrêt", |s| s["status"] == "done")
        .await;
    assert_eq!(statut["written"], 2, "{statut}");
    assert_eq!(statut["errors"], 0, "{statut}");
    assert_eq!(banc.source(jouee).as_deref(), Some("tag"));
    assert!(
        std::fs::read(&chemin_joue).unwrap() != octets_avant,
        "lecture arrêtée : le fichier reporté doit être gravé"
    );
    let relu = tune_core::metadata::read_extended_metadata(std::path::Path::new(&chemin_joue));
    assert_eq!(relu.get("dr_track").map(String::as_str), Some("12"));
    a_neuf(priorite::politique::DEFAUT.pause_between_items_ms);
}

/// Sans lecture, rien ne change : pas de pause (la pause par défaut de 5 s
/// par fichier se verrait), personne au relevé, tout est gravé.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_lecture_la_gravure_ne_change_pas_6212() {
    let _g = VERROU.lock().await;
    a_neuf(priorite::politique::DEFAUT.pause_between_items_ms);
    let banc = Banc::monter().await;

    let debut = Instant::now();
    banc.lancer().await;
    let statut = banc
        .attendre("la passe finie", |s| s["status"] == "done")
        .await;
    assert!(
        debut.elapsed() < Duration::from_secs(4),
        "sans lecture, la passe ne doit marquer aucune pause : {:?}",
        debut.elapsed()
    );
    assert_eq!(statut["written"], 2, "{statut}");
    assert!(
        priorite::releve().throttled.is_empty(),
        "sans lecture, aucune passe ne cède : {:?}",
        priorite::releve().throttled
    );
    for (id, _) in &banc.pistes {
        assert_eq!(banc.source(*id).as_deref(), Some("tag"));
    }
}
