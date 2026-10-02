//! `GET /library/browse/dir` : l'écran Répertoires range ses sous-dossiers
//! dans l'ordre alphabétique naturel (#5582, fil forum 2072).
//!
//! La route triait les noms par OCTETS : toutes les majuscules d'abord, puis
//! les minuscules après « Z ». Sur un partage SMB où vivent côte à côte
//! « Haydn Michael » et « haydn », le second partait en fin de liste, hors de
//! la vue — le testeur l'a cru absent. « Félicien David » passait de même
//! après « Franck » (É > r en octets).
//!
//! La route réemploie désormais `tune_core::upnp_server::comparer_naturel`,
//! l'ordre des dossiers du serveur média (#4956) : sans casse ni accents,
//! nombres par leur valeur. Le chemin est le même pour un dossier local et un
//! partage SMB (`read_dir` sur le chemin résolu, UNC compris sous Windows).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// Une racine musicale dont les sous-dossiers mêlent casse, accents et
/// nombres, créés dans un ordre quelconque.
fn bibliotheque(noms: &[&str]) -> (tempfile::TempDir, axum::Router, String) {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let racine = tmp.path().to_string_lossy().to_string();
    for d in noms {
        std::fs::create_dir_all(tmp.path().join(d)).expect("sous-dossier");
    }
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
        .expect("racines musique");
    let app = tune_server::routes::router(state);
    (tmp, app, racine)
}

/// Encodage minimal des caractères qu'un chemin temporaire peut porter et
/// qu'une chaîne de requête interprète (comme `comptes_sous_dossiers_3857.rs`).
fn encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '&' => "%26".to_string(),
            '+' => "%2B".to_string(),
            '?' => "%3F".to_string(),
            '\\' => "%5C".to_string(),
            autre => autre.to_string(),
        })
        .collect()
}

async fn sous_dossiers(app: &axum::Router, chemin: &str) -> Vec<String> {
    let uri = format!("/api/v1/library/browse/dir?path={}", encode(chemin));
    let resp = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .expect("la route doit répondre");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("corps");
    let corps: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
    corps["directories"]
        .as_array()
        .expect("la route publie ses sous-dossiers")
        .iter()
        .map(|d| d["name"].as_str().expect("nom").to_string())
        .collect()
}

/// Le cas du fil 2072 : « haydn » (minuscule) se range entre « Handel » et
/// « Mozart », pas après « Mozart ».
#[tokio::test]
async fn haydn_en_minuscules_se_range_entre_handel_et_mozart() {
    let (_tmp, app, racine) = bibliotheque(&["Mozart", "haydn", "Handel"]);
    assert_eq!(
        sous_dossiers(&app, &racine).await,
        ["Handel", "haydn", "Mozart"],
        "un dossier en minuscules ne part plus après « Z » (tri par octets)"
    );
}

/// Les autres signatures du tri par octets relevées sur la capture : les
/// minuscules (« d'indy », « dvorak ») et l'accent de tête (« Félicien
/// David » après « Franck »). Les nombres se comparent par leur valeur, comme
/// dans le rayon « Folders » du serveur média.
#[tokio::test]
async fn casse_accents_et_nombres_suivent_l_ordre_du_serveur_media() {
    let (_tmp, app, racine) = bibliotheque(&[
        "dvorak",
        "Franck",
        "Donizetti",
        "Félicien David",
        "d'indy",
        "Chopin",
        "CD10",
        "CD9",
    ]);
    assert_eq!(
        sous_dossiers(&app, &racine).await,
        [
            "CD9",
            "CD10",
            "Chopin",
            "d'indy",
            "Donizetti",
            "dvorak",
            "Félicien David",
            "Franck",
        ],
    );
}
