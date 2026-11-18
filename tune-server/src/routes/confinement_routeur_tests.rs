//! Sécurité — contournements rejoués par le VRAI routeur axum
//! (`crate::routes::router`) : décodage de `Query`, `Path` et `Json`
//! compris, authentification éteinte (le défaut), donc `RequireAdmin` laisse
//! passer tout le monde. C'est le cas où le périmètre des dossiers de musique
//! doit tenir seul.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::banc;
use crate::state::AppState;

async fn appel(
    state: &AppState,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, String) {
    let mut requete = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            requete = requete.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = crate::routes::router(state.clone()).layer(axum::extract::connect_info::MockConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 50000))))
        .oneshot(requete.body(body).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

fn racines(state: &AppState) -> Vec<String> {
    crate::routes::system::get_music_dirs_list(&state.backend)
}

/// Des chemins qu'aucun client ne doit pouvoir déclarer dossier de musique.
fn trop_larges(b: &super::Banc) -> Vec<String> {
    vec![
        "/".into(),
        "//".into(),
        "/etc".into(),
        "//etc".into(),
        "/./etc".into(),
        "/private".into(),
        "/proc/self".into(),
        "/root".into(),
        r"C:\".into(),
        r"C:\Windows".into(),
        b.remontee(""),
        format!("{}/%2e%2e/dehors", b.musique.display()),
    ]
}

// ─── B1 : toute écriture des dossiers de musique ───────────────────────────

#[tokio::test]
async fn b1_l_ajout_d_un_dossier_trop_large_est_refuse_par_le_routeur() {
    let b = banc();
    let avant = racines(&b.state);
    for chemin in trop_larges(&b) {
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/system/music-dirs/add",
            Some(json!({ "path": chemin })),
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::BAD_REQUEST,
            "{chemin:?} ajouté : {corps}"
        );
        assert_eq!(racines(&b.state), avant, "{chemin:?} a modifié music_dirs");
    }
}

#[tokio::test]
async fn b1_le_patch_de_configuration_ne_declare_pas_un_dossier_trop_large() {
    let b = banc();
    let avant = racines(&b.state);
    for chemin in trop_larges(&b) {
        for valeur in [json!([avant[0], chemin]), json!(format!("[{:?}]", chemin))] {
            let (statut, corps) = appel(
                &b.state,
                "PATCH",
                "/api/v1/system/config",
                Some(json!({ "music_dirs": valeur, "theme": "dark" })),
            )
            .await;
            assert_eq!(statut, StatusCode::BAD_REQUEST, "{valeur} : {corps}");
            assert_eq!(racines(&b.state), avant, "{valeur} a modifié music_dirs");
        }
    }
    // Une valeur illisible n'est pas écrite telle quelle.
    let (statut, _) = appel(
        &b.state,
        "PATCH",
        "/api/v1/system/config",
        Some(json!({ "music_dirs": 5 })),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(racines(&b.state), avant);
}

/// Contre-épreuve : une racine DÉJÀ déclarée n'est pas revérifiée (une
/// installation existante ne casse pas), et une vraie bibliothèque s'ajoute.
#[tokio::test]
async fn b1_le_patch_garde_l_existant_et_accepte_une_vraie_bibliotheque() {
    let b = banc();
    let musique = racines(&b.state)[0].clone();
    tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone())
        .set("music_dirs", &json!([musique, "/"]).to_string())
        .unwrap();
    let (statut, corps) = appel(
        &b.state,
        "PATCH",
        "/api/v1/system/config",
        Some(json!({ "music_dirs": [musique, "/"] })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let (statut, corps) = appel(
        &b.state,
        "PATCH",
        "/api/v1/system/config",
        Some(json!({ "music_dirs": [musique] })),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "un retrait reste possible : {corps}"
    );
    assert_eq!(racines(&b.state), vec![musique]);
}

#[tokio::test]
async fn b1_l_onboarding_ne_declare_pas_un_dossier_trop_large() {
    let b = banc();
    let avant = racines(&b.state);
    for chemin in trop_larges(&b) {
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/onboarding/step/music-dirs",
            Some(json!({ "dirs": [chemin] })),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{chemin:?} : {corps}");
        assert_eq!(racines(&b.state), avant, "{chemin:?} a modifié music_dirs");
    }
}

#[tokio::test]
async fn b1_l_import_de_configuration_ecarte_un_dossier_trop_large() {
    let b = banc();
    let avant = racines(&b.state);
    for chemin in ["/", "/etc", "/private"] {
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/system/config/import",
            Some(json!({ "music_dirs": [chemin], "theme": "dark" })),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert!(
            corps.contains("music_dirs"),
            "l'écart doit être dit : {corps}"
        );
        assert_eq!(
            racines(&b.state),
            avant,
            "{chemin:?} importé dans music_dirs"
        );
    }
}

// ─── B3 : browse/dir n'est pas un oracle d'existence ───────────────────────

#[tokio::test]
async fn b3_browse_dir_rend_la_meme_reponse_que_le_chemin_existe_ou_non() {
    let b = banc();
    let lire = |chemin: String| {
        let state = b.state.clone();
        async move {
            let uri = format!(
                "/api/v1/library/browse/dir?path={}",
                urlencoding::encode(&chemin)
            );
            appel(&state, "GET", &uri, None).await
        }
    };
    let dehors = b.dehors.to_string_lossy().into_owned();
    let cas = [
        dehors.clone(),                   // existe, hors bibliothèque
        format!("{dehors}/secret.flac"),  // fichier existant
        format!("{dehors}/n-existe-pas"), // n'existe pas
        "/etc".to_string(),               // existe
        "/n-existe-pas-2".to_string(),    // n'existe pas
        b.remontee(""),                   // `..`
        b.dedans("/n-existe-pas"),        // dans la bibliothèque, absent
        "relatif/x".to_string(),
        "  ".to_string(),
    ];
    let reference = lire(cas[0].clone()).await;
    assert_eq!(reference.0, StatusCode::BAD_REQUEST, "{}", reference.1);
    for chemin in cas {
        let r = lire(chemin.clone()).await;
        assert_eq!(r, reference, "{chemin:?} se distingue des autres refus");
    }
    // `%2F` doublement encodé : décodé une fois par `Query`, il reste du texte.
    let uri = format!(
        "/api/v1/library/browse/dir?path={}%252F..%252Fdehors",
        urlencoding::encode(&b.musique.to_string_lossy())
    );
    let r = appel(&b.state, "GET", &uri, None).await;
    assert_eq!(r, reference, "forme doublement encodée");
    // Contre-épreuve : la bibliothèque elle-même se lit.
    let (statut, corps) = lire(b.dedans("")).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(corps.contains("Album"), "{corps}");
}

// ─── B2 : aucun fichier local par une adresse de flux client ───────────────

#[tokio::test]
async fn b2_un_episode_de_podcast_n_est_jamais_un_fichier_local() {
    let b = banc();
    let secret = b.dehors.join("secret.flac");
    for adresse in [
        secret.to_string_lossy().into_owned(),
        format!("file://{}", secret.display()),
        format!("  {}", secret.display()),
    ] {
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/podcasts/play/1",
            Some(json!({ "audio_url": adresse, "title": "x" })),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{adresse:?} : {corps}");
        assert!(!corps.contains("secret"), "{corps}");
    }
}

#[tokio::test]
async fn b2_hqplayer_ne_recoit_pas_un_chemin_local() {
    let b = banc();
    for uri in [
        "/etc/hosts".to_string(),
        format!("file://{}", b.dehors.join("secret.flac").display()),
    ] {
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/hqplayer/play",
            Some(json!({ "uri": uri })),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{uri:?} : {corps}");
    }
}

/// B1 — la restauration d'une sauvegarde de configuration (Premium) écarte
/// elle aussi une racine trop large, et le dit.
#[tokio::test]
async fn b1_la_restauration_d_une_sauvegarde_ecarte_un_dossier_trop_large() {
    let b = banc();
    b.state.license.set_account_premium(true, None).await;
    let avant = racines(&b.state);
    for chemin in ["/", "/etc", "/Volumes"] {
        let instantane = json!({
            "version": "1",
            "created_at": "2026-11-14T00:00:00Z",
            "zones": [],
            "settings": [["music_dirs", json!([chemin]).to_string()], ["theme", "dark"]],
            "playlists": [],
            "favorites": [],
            "radio_stations": [],
            "alarms": [],
            "eq_presets": [],
            "room_profiles": [],
        });
        let (statut, corps) = appel(
            &b.state,
            "POST",
            "/api/v1/system/config-backup/import",
            Some(instantane),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{chemin:?} : {corps}");
        assert!(
            corps.contains("music_dirs"),
            "l'écart doit être dit : {corps}"
        );
        assert_eq!(
            racines(&b.state),
            avant,
            "{chemin:?} restauré dans music_dirs"
        );
    }
}

// ─── #8676 « Remplacer le chemin » : même périmètre que l'ajout ────────────

#[tokio::test]
async fn b1_remplacer_le_chemin_ne_mene_pas_hors_du_perimetre() {
    let b = banc();
    let avant = racines(&b.state);
    let ancien = avant[0].clone();
    for nouveau in [
        "/",
        "/etc",
        "/Volumes",
        "/private",
        "/var/lib/x",
        r"C:\Windows",
    ] {
        for (route, confirm) in [
            ("/api/v1/system/music-dirs/move/preview", None),
            ("/api/v1/system/music-dirs/move", Some(0)),
        ] {
            let (statut, corps) = appel(
                &b.state,
                "POST",
                route,
                Some(json!({
                    "old_prefix": ancien,
                    "new_prefix": nouveau,
                    "confirm_tracks": confirm,
                })),
            )
            .await;
            assert_eq!(
                statut,
                StatusCode::BAD_REQUEST,
                "{route} vers {nouveau:?} : {corps}"
            );
            assert_eq!(racines(&b.state), avant, "{route} vers {nouveau:?}");
        }
    }
    // Contre-épreuve : un déplacement vers une vraie bibliothèque reste
    // possible (l'aperçu répond, sans refus de périmètre).
    let ailleurs = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "confinement-deplacer-ok",
    );
    let (statut, corps) = appel(
        &b.state,
        "POST",
        "/api/v1/system/music-dirs/move/preview",
        Some(json!({
            "old_prefix": ancien,
            "new_prefix": ailleurs.path().to_string_lossy(),
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}
