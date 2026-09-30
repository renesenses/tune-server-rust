//! #5469 — une passe d'enrichissement coupée par un arrêt du serveur repart.
//!
//! Tades, fil 2042 : il suspend les métadonnées, le serveur a redémarré entre
//! temps, et « Reprendre » ne relance rien. La passe vivait dans un
//! `tokio::spawn` que l'arrêt avait détruit, et le bouton ne faisait que lever
//! un drapeau.
//!
//! **L'arrêt est simulé pour de vrai** : chaque « processus » a son propre
//! runtime tokio et son propre `AppState`, sur le MÊME fichier SQLite. Détruire
//! le runtime détruit la passe en plein vol, sans qu'elle atteigne sa fin,
//! comme un arrêt du serveur. Le « redémarrage » rouvre la base avec un
//! registre de tâches vide, puis relit les pauses (`hydrater`), dans l'ordre
//! de `background::spawn_background_tasks`.
//!
//! **Hermétique** : aucune requête MusicBrainz ne part. Une passe qui a des
//! candidats n'est lancée que sous pause, et elle se gare avant sa première
//! requête. Une passe qu'on laisse courir n'a aucun candidat.
//!
//! Les pauses sont un état GLOBAL du processus (`taches_de_fond::PAUSES`). Les
//! essais de ce fichier sont donc sérialisés par [`UN_ESSAI_A_LA_FOIS`], et ce
//! fichier est une cible à part : aucun autre essai ne touche à ses pauses.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::taches_de_fond::{self, Tache};
use tune_server::reprise_des_passes::{self, Origine, Passe, RELANCES_MAX, Relance};
use tune_server::state::AppState;

static UN_ESSAI_A_LA_FOIS: Mutex<()> = Mutex::new(());

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Un « processus » : un runtime, un `AppState` sur la base partagée. À la
/// sortie, le runtime est détruit, et avec lui toute passe encore en vol.
fn processus<T>(base: &Path, corps: impl AsyncFnOnce(AppState) -> T) -> T {
    let rt = runtime();
    let chemin = base.to_str().expect("chemin UTF-8").to_string();
    let sortie = rt.block_on(async move {
        let state = AppState::new(&chemin, 0, Default::default()).expect("état");
        // Le démarrage : le miroir des pauses part à zéro, puis relit la base.
        taches_de_fond::oublier_pour_les_essais();
        taches_de_fond::hydrater(&state.backend);
        corps(state).await
    });
    // ARRÊT. `shutdown_timeout` détruit les tâches sans attendre leur fin.
    rt.shutdown_timeout(Duration::from_secs(2));
    sortie
}

fn base_neuve(nom: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("tune-5469-{nom}-"))
        .tempdir()
        .expect("dossier temporaire");
    let base = dir.path().join("tune.db");
    (dir, base)
}

/// Une piste locale sans aucune métadonnée MusicBrainz : un candidat de
/// `POST /library/enrich-all`.
fn un_candidat(state: &AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Bill Evans'); \
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Waltz for Debby', 1); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
               VALUES (10, 'My Foolish Heart', 1, 1, '/m-n5469/a/01.flac', 'local');",
        )
        .expect("insertion du candidat");
}

fn plus_aucun_candidat(state: &AppState) {
    state
        .backend
        .execute_batch("DELETE FROM tracks;")
        .expect("retrait des pistes");
}

async fn post(state: &AppState, chemin: &str) -> (StatusCode, Value) {
    let reponse = tune_server::routes::router(state.clone())
        .oneshot(Request::post(chemin).body(Body::empty()).expect("requête"))
        .await
        .expect("réponse");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn compteur_du_quota(state: &AppState) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get("enrichment_daily_count")
        .expect("lecture du quota")
}

/// Attendre que la passe ait quitté le registre, c'est-à-dire qu'elle soit
/// allée au bout de sa boucle.
async fn attendre_la_fin(state: &AppState, passe: Passe) {
    for _ in 0..200 {
        if !reprise_des_passes::vivante(state, passe) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("la passe {} n'a pas fini en 5 s", passe.id());
}

/// Le premier processus : le bouton lance la passe des métadonnées, suspendue,
/// puis le serveur s'arrête pendant qu'elle est garée.
fn lancer_puis_arreter(base: &Path) {
    processus(base, async |state| {
        un_candidat(&state);
        taches_de_fond::mettre_en_pause(&state.backend, Tache::Enrichissement).expect("pause");
        let (statut, corps) = post(&state, "/api/v1/library/enrich-all").await;
        assert_eq!(statut, StatusCode::ACCEPTED, "{corps}");
        assert!(reprise_des_passes::vivante(&state, Passe::Metadonnees));
        let repere = reprise_des_passes::lire(&state.backend, Passe::Metadonnees)
            .expect("le bouton pose le repère de reprise");
        assert_eq!(repere.relances, 0, "un geste remet le compteur à zéro");
        assert_eq!(repere.portee, None, "passe complète : aucune portée");
        // Laisser la passe atteindre sa frontière et s'y garer.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            reprise_des_passes::vivante(&state, Passe::Metadonnees),
            "sous pause, la passe se gare : elle ne doit pas avoir fini"
        );
    });
}

/// 🔴 LE DÉFAUT : après l'arrêt, rien ne relançait la passe. Le démarrage la
/// relance, sur la même portée, et elle se gare sur la pause restaurée.
#[test]
fn arret_simule_puis_redemarrage_la_passe_de_metadonnees_repart() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, base) = base_neuve("demarrage");

    lancer_puis_arreter(&base);

    processus(&base, async |state| {
        assert!(
            taches_de_fond::est_en_pause(Tache::Enrichissement),
            "la pause survit au redémarrage (#4574)"
        );
        assert!(
            !reprise_des_passes::vivante(&state, Passe::Metadonnees),
            "l'arrêt a tué la passe : le registre du nouveau processus est vide"
        );
        let relancees = reprise_des_passes::relancer_les_passes_interrompues(&state).await;
        assert_eq!(
            relancees,
            vec!["enrich_all"],
            "la passe coupée par l'arrêt doit repartir au démarrage"
        );
        assert!(reprise_des_passes::vivante(&state, Passe::Metadonnees));
        assert_eq!(
            reprise_des_passes::lire(&state.backend, Passe::Metadonnees)
                .expect("repère")
                .relances,
            1,
            "une relance automatique est comptée"
        );
        // Elle se gare sur la pause restaurée au lieu de travailler.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(reprise_des_passes::vivante(&state, Passe::Metadonnees));
    });
}

/// La reprise n'est pas un geste : elle passe même quota du jour épuisé, et ne
/// l'entame pas.
#[test]
fn la_reprise_de_demarrage_ignore_le_quota_gratuit() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, base) = base_neuve("quota");

    lancer_puis_arreter(&base);

    processus(&base, async |state| {
        // Le geste du premier processus a posé la date du jour et compté 1 ;
        // on épuise le quota.
        assert_eq!(compteur_du_quota(&state).as_deref(), Some("1"));
        SettingsRepo::with_backend(state.backend.clone())
            .set("enrichment_daily_count", "999")
            .expect("quota épuisé");
        // Témoin : le bouton, lui, est bien refusé.
        let (statut, corps) = post(&state, "/api/v1/library/enrich-all").await;
        assert_eq!(statut, StatusCode::TOO_MANY_REQUESTS, "{corps}");

        let issue =
            reprise_des_passes::relancer_si_absente(&state, Passe::Metadonnees, Origine::Demarrage)
                .await;
        assert_eq!(issue, Relance::Relancee, "quota épuisé : la reprise passe");
        assert_eq!(
            compteur_du_quota(&state).as_deref(),
            Some("999"),
            "la reprise ne consomme pas le quota"
        );
    });
}

/// 🔴 LE SECOND DÉFAUT : « Reprendre » sur une passe que l'arrêt a tuée ne
/// faisait que lever le drapeau. Il en relance une, et elle va au bout.
#[test]
fn reprendre_sans_passe_vivante_en_relance_une() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, base) = base_neuve("reprendre");

    lancer_puis_arreter(&base);

    processus(&base, async |state| {
        // Pas de reprise de démarrage ici : c'est le clic qui doit relancer.
        assert!(!reprise_des_passes::vivante(&state, Passe::Metadonnees));
        // Sans candidat, la passe relancée finit tout de suite, sans réseau.
        plus_aucun_candidat(&state);

        let (statut, corps) =
            post(&state, "/api/v1/system/background-tasks/enrichment/resume").await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(
            corps["relaunched"],
            serde_json::json!(["enrich_all"]),
            "« Reprendre » sans passe vivante doit en relancer une : {corps}"
        );
        assert!(!taches_de_fond::est_en_pause(Tache::Enrichissement));

        attendre_la_fin(&state, Passe::Metadonnees).await;
        let statut_passe: Value = serde_json::from_str(
            &SettingsRepo::with_backend(state.backend.clone())
                .get("enrich_all_status")
                .expect("lecture")
                .expect("statut de la passe"),
        )
        .expect("JSON");
        assert_eq!(statut_passe["status"], "done", "{statut_passe}");
        assert_eq!(
            reprise_des_passes::lire(&state.backend, Passe::Metadonnees),
            None,
            "à sa fin normale, la passe efface son repère"
        );

        // Un second clic ne relance rien : il n'y a plus rien à reprendre.
        let (_, corps) = post(&state, "/api/v1/system/background-tasks/enrichment/resume").await;
        assert_eq!(corps["relaunched"], serde_json::json!([]), "{corps}");
    });
}

/// Témoin inverse : une passe allée au bout ne repart pas au démarrage.
#[test]
fn une_passe_finie_normalement_ne_repart_pas() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, base) = base_neuve("finie");

    processus(&base, async |state| {
        taches_de_fond::reprendre(&state.backend, Tache::Enrichissement).expect("pas de pause");
        let (statut, corps) = post(&state, "/api/v1/library/enrich-all").await;
        assert_eq!(statut, StatusCode::ACCEPTED, "{corps}");
        attendre_la_fin(&state, Passe::Metadonnees).await;
        assert_eq!(
            reprise_des_passes::lire(&state.backend, Passe::Metadonnees),
            None
        );
    });

    processus(&base, async |state| {
        let relancees = reprise_des_passes::relancer_les_passes_interrompues(&state).await;
        assert!(
            relancees.is_empty(),
            "rien n'était interrompu : {relancees:?}"
        );
    });
}

/// Une passe qui meurt à chaque démarrage n'est pas relancée sans fin :
/// au-delà de `RELANCES_MAX`, le démarrage renonce. « Reprendre » peut encore
/// la relancer, et remet le compteur à zéro.
#[test]
fn le_plafond_de_relances_arrete_la_boucle_mais_pas_le_bouton() {
    let _un = UN_ESSAI_A_LA_FOIS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, base) = base_neuve("plafond");

    lancer_puis_arreter(&base);

    for n in 1..=RELANCES_MAX {
        processus(&base, async |state| {
            let issue = reprise_des_passes::relancer_si_absente(
                &state,
                Passe::Metadonnees,
                Origine::Demarrage,
            )
            .await;
            assert_eq!(issue, Relance::Relancee, "relance n° {n}");
        });
    }

    processus(&base, async |state| {
        let issue =
            reprise_des_passes::relancer_si_absente(&state, Passe::Metadonnees, Origine::Demarrage)
                .await;
        assert_eq!(
            issue,
            Relance::Abandonnee {
                relances: RELANCES_MAX
            }
        );
        assert!(!reprise_des_passes::vivante(&state, Passe::Metadonnees));

        let issue =
            reprise_des_passes::relancer_si_absente(&state, Passe::Metadonnees, Origine::Reprendre)
                .await;
        assert_eq!(issue, Relance::Relancee, "le bouton n'est pas plafonné");
        assert_eq!(
            reprise_des_passes::lire(&state.backend, Passe::Metadonnees)
                .expect("repère")
                .relances,
            0
        );
    });
}
