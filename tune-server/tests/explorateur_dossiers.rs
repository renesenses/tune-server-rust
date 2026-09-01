//! L'explorateur de dossiers vu de la ROUTE (#1275).
//!
//! `explorateur.rs` teste les prédicats ; ce fichier-ci teste ce que la route
//! rend réellement, parce qu'une garde peut être juste et n'être appelée nulle
//! part. Chaque cas est un REFUS : ils tombent si l'on retire la garde
//! correspondante de `browse_dirs`, et c'est là toute leur valeur.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn active_l_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
}

fn jeton(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET).unwrap()
}

async fn explorer(
    state: &AppState,
    requete: &str,
    porteur: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::get(requete);
    if let Some(b) = porteur {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let reponse = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let statut = reponse.status();
    let corps = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&corps).unwrap_or(serde_json::Value::Null);
    (statut, json)
}

// -------------------------------------------------------------------------
// Le PÉRIMÈTRE. Ces refus valent même sans authentification : sur une
// installation par défaut `auth_enabled` est absent, le serveur est ouvert
// « LAN de confiance », et c'est précisément là que la route était nue.
// -------------------------------------------------------------------------

/// Un arbre système est refusé, jeton ou pas.
#[tokio::test]
async fn un_arbre_systeme_est_refuse_meme_sans_authentification() {
    let state = new_state();
    for chemin in ["/etc", "/proc", "/root", "/var/lib"] {
        let (statut, corps) = explorer(
            &state,
            &format!("/api/v1/system/browse-dirs?path={chemin}"),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "laissé passer : {chemin}");
        assert_eq!(corps["dirs"].as_array().map(Vec::len), Some(0));
    }
}

/// Un `..` est refusé, dans les deux écritures. Sans cette garde, un chemin
/// peut se faufiler sous n'importe quel préfixe autorisé.
#[tokio::test]
async fn une_remontee_vers_le_parent_est_refusee() {
    let state = new_state();
    for chemin in [
        "/mnt/musique/../../etc",
        "/tmp/..",
        "D:%5CMusique%5C..%5C..%5CWindows",
    ] {
        let (statut, corps) = explorer(
            &state,
            &format!("/api/v1/system/browse-dirs?path={chemin}"),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "laissé passer : {chemin}");
        assert_eq!(corps["error"], "path must not contain '..'");
    }
}

/// Un chemin relatif n'a rien à faire ici : il serait résolu contre le
/// répertoire courant du PROCESSUS SERVEUR, que l'appelant ne connaît pas.
#[tokio::test]
async fn un_chemin_relatif_est_refuse() {
    let state = new_state();
    let (statut, corps) =
        explorer(&state, "/api/v1/system/browse-dirs?path=Musique/Jazz", None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
    assert_eq!(corps["error"], "path must be absolute");
}

/// La contre-épreuve du périmètre : la racine reste explorable — sinon le
/// sélecteur ne sert plus à rien — mais les arbres système n'y figurent plus.
/// Ce test tombe aussi si quelqu'un resserre le périmètre au point de rendre
/// la racine vide.
#[cfg(unix)]
#[tokio::test]
async fn la_racine_reste_explorable_mais_sans_les_arbres_systeme() {
    let state = new_state();
    let (statut, corps) = explorer(&state, "/api/v1/system/browse-dirs?path=/", None).await;
    assert_eq!(statut, StatusCode::OK);
    let noms: Vec<String> = corps["dirs"]
        .as_array()
        .expect("la racine doit rendre une liste")
        .iter()
        .filter_map(|d| d["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !noms.is_empty(),
        "la racine ne rend plus rien : le périmètre a mangé le sélecteur"
    );
    for systeme in ["etc", "proc", "sys", "dev", "root", "var", "usr", "bin"] {
        assert!(
            !noms.iter().any(|n| n == systeme),
            "« {systeme} » est encore listé : {noms:?}"
        );
    }
}

// -------------------------------------------------------------------------
// Le RÔLE. La route de lecture s'aligne enfin sur la route d'écriture
// qu'elle alimente (`POST /system/music-dirs`, déjà `RequireAdmin`).
// -------------------------------------------------------------------------

/// Le cœur du correctif de rôle : un jeton VALIDE mais non administrateur ne
/// peut plus énumérer le disque. `/auth/register` est public et ne crée que
/// des non-administrateurs — c'était donc un compte à la demande.
#[tokio::test]
async fn un_jeton_non_administrateur_ne_peut_plus_explorer() {
    let state = new_state();
    active_l_auth(&state);
    let (statut, _) = explorer(
        &state,
        "/api/v1/system/browse-dirs?path=/tmp",
        Some(&jeton("user", 2)),
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
}

/// Contre-épreuve du rôle : l'administrateur, lui, passe la garde de rôle —
/// sans quoi le test précédent serait vert parce que la route est cassée.
#[cfg(unix)]
#[tokio::test]
async fn l_administrateur_explore_toujours() {
    let state = new_state();
    active_l_auth(&state);
    let (statut, _) = explorer(
        &state,
        "/api/v1/system/browse-dirs?path=/",
        Some(&jeton("admin", 1)),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
}

/// Sans jeton, l'auth activée, c'est 401 — pas 403 : la distinction dit à
/// l'interface s'il faut demander une connexion ou expliquer un droit manquant.
#[tokio::test]
async fn sans_jeton_l_auth_activee_c_est_401() {
    let state = new_state();
    active_l_auth(&state);
    let (statut, _) = explorer(&state, "/api/v1/system/browse-dirs?path=/tmp", None).await;
    assert_eq!(statut, StatusCode::UNAUTHORIZED);
}

/// Le lien symbolique : le chemin demandé est irréprochable, sa cible est un
/// arbre système. Sans la forme canonique, la route l'ouvrirait.
#[cfg(unix)]
#[tokio::test]
async fn un_lien_vers_un_arbre_systeme_est_refuse_par_la_route() {
    let state = new_state();
    // `/tmp` et non `std::env::temp_dir()` : sous macOS ce dernier vit sous
    // `/private/var`, donc déjà hors périmètre — le refus attendu tomberait
    // pour la mauvaise raison et ne prouverait plus rien du lien.
    let base = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-explorateur-route-i1275");
    let lien = base.join("raccourci");
    std::os::unix::fs::symlink("/etc", &lien).expect("lien de test");

    // Sur macOS le dossier temporaire vit sous `/private/var`, donc DÉJÀ hors
    // périmètre : le refus qui suit ne prouverait alors rien. On vérifie
    // d'abord que le dossier porteur, lui, est bien explorable — faute de
    // quoi cet hôte ne peut pas porter la démonstration.
    let (statut_base, _) = explorer(
        &state,
        &format!("/api/v1/system/browse-dirs?path={}", base.display()),
        None,
    )
    .await;
    if statut_base != StatusCode::OK {
        return;
    }

    let (statut, corps) = explorer(
        &state,
        &format!("/api/v1/system/browse-dirs?path={}", lien.display()),
        None,
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "le lien a ouvert /etc : {corps}"
    );
}
