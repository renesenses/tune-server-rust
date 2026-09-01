//! #2793 — les routes de configuration ne publient plus les secrets, et
//! l'export/import de configuration exige le rôle administrateur.
//!
//! Trois choses tenaient ce défaut :
//!
//! 1. `GET /system/config` recopie la table `settings` telle quelle. Le
//!    caviardage se faisait sur une liste de deux clés nommées à la main plus
//!    trois sous-champs Qobuz, et cette liste avait pris du retard sur ce que
//!    la table contient : la graine Ed25519 d'un appairage AirPlay et les clés
//!    `tunedev_` de l'API développeur sortaient en clair.
//! 2. `GET /system/config/export` avait sa PROPRE liste, de trois clés, avec le
//!    même retard — et `?include_secrets=true` rendait tout, à n'importe qui.
//! 3. `POST /system/config/import` écrivait chaque clé reçue sans vérifier le
//!    rôle : un compte standard postait `auth_enabled=false` et éteignait
//!    l'authentification du serveur.
//!
//! Les valeurs employées ici sont FAUSSES et le restent : rien de ce que ce
//! fichier écrit ne ressemble à un vrai secret.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "test-jwt-secret";

/// Valeurs factices posées en base. Chacune est une chaîne improbable : si
/// l'une d'elles apparaît dans une réponse, c'est que le caviardage l'a ratée.
const FAUX_JWT: &str = "FAUX-jwt-a1b2c3";
const FAUX_DISCOGS: &str = "FAUX-discogs-d4e5f6";
const FAUX_TUNEDEV: &str = "tunedev_FAUX0011223344556677889900aabb";
const FAUX_GRAINE: &str = "FAUX-graine-ed25519-99887766";
const FAUX_VAULT: &str = "FAUX-vault-0f0f0f";
const FAUX_LASTFM: &str = "FAUX-lastfm-session-7788";

/// Valeur légitime, servie avant comme après : le témoin anti-régression.
const THEME_ATTENDU: &str = "nuit-profonde";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

fn tok(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET_JWT).unwrap()
}

/// Pose en base un échantillon de tout ce que `settings` peut porter :
/// des secrets de plusieurs formes, et un réglage parfaitement anodin.
fn semer_les_reglages(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("jwt_secret", FAUX_JWT).unwrap();
    s.set("discogs_token", FAUX_DISCOGS).unwrap();
    s.set("lastfm_session_key", FAUX_LASTFM).unwrap();
    s.set("credentials_vault", FAUX_VAULT).unwrap();
    // Un tableau JSON, comme l'écrit `developer_api.rs`.
    s.set(
        "developer_api_keys",
        &format!(r#"[{{"id":"1","name":"essai","key":"{FAUX_TUNEDEV}"}}]"#),
    )
    .unwrap();
    // Un objet dont le NOM de clé est anodin : seule la descente récursive
    // attrape la graine.
    s.set(
        "airplay2_pairing:airplay2:salon",
        &format!(
            r#"{{"our_ed25519_seed_hex":"{FAUX_GRAINE}","accessory_ltpk_hex":"aabb","accessory_id":"salon"}}"#
        ),
    )
    .unwrap();
    // Un bloc de credentials de service, tel que le connecteur Qobuz l'écrit.
    s.set(
        "auth_tokens_qobuz",
        r#"{"stored_password":"FAUX-mdp","user_auth_token":"FAUX-jeton","app_secret":"FAUX-app"}"#,
    )
    .unwrap();
    // Le témoin : un réglage sans rien de sensible, qui doit rester servi.
    s.set("theme", THEME_ATTENDU).unwrap();
}

async fn get_body(state: &AppState, path: &str, bearer: Option<&str>) -> (StatusCode, String) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::get(path);
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_json(
    state: &AppState,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Tous les faux secrets semés, pour l'assertion « aucun ne sort ».
const FAUX_SECRETS: &[&str] = &[
    FAUX_JWT,
    FAUX_DISCOGS,
    FAUX_TUNEDEV,
    FAUX_GRAINE,
    FAUX_VAULT,
    FAUX_LASTFM,
];

fn aucun_secret_dans(corps: &str, ou: &str) {
    for faux in FAUX_SECRETS {
        assert!(
            !corps.contains(faux),
            "{ou} laisse sortir un secret ({} caracteres de reponse)",
            corps.len()
        );
    }
}

// ── /system/config ──────────────────────────────────────────────────

/// Le cœur du défaut : la route publiait la table entière.
#[tokio::test]
async fn get_config_ne_laisse_sortir_aucun_secret() {
    let state = new_state();
    semer_les_reglages(&state);
    let (st, corps) = get_body(&state, "/api/v1/system/config", None).await;
    assert_eq!(st, StatusCode::OK);
    aucun_secret_dans(&corps, "/system/config");
}

/// Le témoin anti-régression. Sans lui, « aucun secret ne sort » serait aussi
/// vrai d'une route qui ne rend plus rien du tout.
#[tokio::test]
async fn get_config_sert_toujours_les_reglages_legitimes() {
    let state = new_state();
    semer_les_reglages(&state);
    let (st, corps) = get_body(&state, "/api/v1/system/config", None).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(
        v["theme"].as_str(),
        Some(THEME_ATTENDU),
        "un reglage anodin doit rester servi"
    );
    // Le booléen dérivé du jeton Discogs : c'est LUI que l'interface lit
    // (SettingsView.svelte:2948 et :4799), pas le jeton. Il se calcule sur la
    // valeur en clair, donc avant le caviardage — l'ordre des deux compte.
    assert_eq!(
        v["discogs_token_set"].as_bool(),
        Some(true),
        "le badge « Discogs configure » se calcule AVANT le caviardage"
    );
    // Quelques réglages que les clients lisent réellement sur cette route.
    for cle in [
        "server_name",
        "db_engine",
        "supported_audio_backends",
        "api_port",
        "stream_port",
        "discovery_enabled",
    ] {
        assert!(!v[cle].is_null(), "{cle} doit rester servi");
    }
}

/// La graine AirPlay vit sous un nom de clé anodin. Sans descente dans
/// l'objet, elle sortait — et sa voisine, la clé PUBLIQUE de l'accessoire,
/// n'est pas un secret et doit rester lisible.
#[tokio::test]
async fn get_config_masque_la_graine_airplay_sans_effacer_le_reste_du_bloc() {
    let state = new_state();
    semer_les_reglages(&state);
    let (_, corps) = get_body(&state, "/api/v1/system/config", None).await;
    let v: Value = serde_json::from_str(&corps).unwrap();
    let bloc = &v["airplay2_pairing:airplay2:salon"];
    assert_eq!(
        bloc["our_ed25519_seed_hex"].as_str(),
        Some(tune_core::secrets::MASQUE)
    );
    assert_eq!(bloc["accessory_ltpk_hex"].as_str(), Some("aabb"));
    assert_eq!(bloc["accessory_id"].as_str(), Some("salon"));
}

// ── /system/config/export ───────────────────────────────────────────

#[tokio::test]
async fn export_sans_secrets_ne_laisse_rien_passer() {
    let state = new_state();
    semer_les_reglages(&state);
    let (st, corps) = get_body(&state, "/api/v1/system/config/export", None).await;
    assert_eq!(st, StatusCode::OK);
    aucun_secret_dans(&corps, "/system/config/export");
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(
        v["theme"].as_str(),
        Some(THEME_ATTENDU),
        "une sauvegarde vide ne sert a rien : le reglage anodin reste"
    );
}

/// `include_secrets=true` reste le mode « migration vers un serveur neuf » —
/// mais il n'est plus offert qu'à l'administrateur.
#[tokio::test]
async fn export_avec_secrets_reste_complet_pour_l_administrateur() {
    let state = new_state();
    semer_les_reglages(&state);
    let (st, corps) = get_body(
        &state,
        "/api/v1/system/config/export?include_secrets=true",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(
        v["jwt_secret"].as_str(),
        Some(FAUX_JWT),
        "la sauvegarde complete doit rester complete"
    );
}

#[tokio::test]
async fn export_refuse_un_compte_standard_quand_l_auth_est_active() {
    let state = new_state();
    semer_les_reglages(&state);
    enable_auth(&state);
    let (st, _) = get_body(
        &state,
        "/api/v1/system/config/export",
        Some(&tok("user", 2)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Et surtout : le mode complet, qui rendait TOUT en clair.
    let (st, corps) = get_body(
        &state,
        "/api/v1/system/config/export?include_secrets=true",
        Some(&tok("user", 2)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    aucun_secret_dans(&corps, "export refuse a un compte standard");
}

#[tokio::test]
async fn export_reste_ouvert_a_l_administrateur_et_sans_authentification() {
    let state = new_state();
    semer_les_reglages(&state);

    // Auth desactivee : le cas courant, mono-utilisateur. Rien ne change.
    let (st, _) = get_body(&state, "/api/v1/system/config/export", None).await;
    assert_eq!(st, StatusCode::OK);

    enable_auth(&state);
    let (st, _) = get_body(
        &state,
        "/api/v1/system/config/export",
        Some(&tok("admin", 1)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

// ── /system/config/import ───────────────────────────────────────────

/// L'attaque nommée dans #2793 : éteindre l'authentification depuis un compte
/// standard. Le réglage doit être intact après la tentative.
#[tokio::test]
async fn import_ne_permet_pas_a_un_compte_standard_de_couper_l_authentification() {
    let state = new_state();
    enable_auth(&state);
    let (st, _) = post_json(
        &state,
        "/api/v1/system/config/import",
        Some(&tok("user", 2)),
        r#"{"auth_enabled":"false"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let s = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        s.get("auth_enabled").unwrap().as_deref(),
        Some("true"),
        "l'authentification est restee active"
    );
}

#[tokio::test]
async fn import_reste_ouvert_a_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let (st, _) = post_json(
        &state,
        "/api/v1/system/config/import",
        Some(&tok("admin", 1)),
        r#"{"theme":"clair"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let s = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(s.get("theme").unwrap().as_deref(), Some("clair"));
}

/// Un corps dont UNE entrée est refusée ne doit rien laisser derrière lui.
/// Avant, la validation vivait dans la boucle d'écriture : les clés déjà
/// parcourues étaient appliquées, et l'erreur n'arrivait qu'après.
#[tokio::test]
async fn un_import_refuse_ne_laisse_aucune_ecriture_partielle() {
    let state = new_state();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("theme", THEME_ATTENDU).unwrap();

    // 32 clés valides, puis une clé vide — refusée. `serde_json::Map` est
    // ordonné (par insertion ou par nom selon la feature `preserve_order`) :
    // dans les deux cas la clé vide n'est pas la première, donc si la
    // validation ne précédait pas l'écriture, des `aaa_*` seraient déjà en
    // base au moment du refus.
    let mut corps = String::from("{");
    for i in 0..32 {
        corps.push_str(&format!(r#""aaa_reglage_{i:02}":"valeur","#));
    }
    corps.push_str(r#""theme":"ecrase","":"cle vide"}"#);

    let (st, _) = post_json(&state, "/api/v1/system/config/import", None, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    assert_eq!(
        s.get("theme").unwrap().as_deref(),
        Some(THEME_ATTENDU),
        "un import refuse n'ecrase rien"
    );
    for i in 0..32 {
        assert!(
            s.get(&format!("aaa_reglage_{i:02}")).unwrap().is_none(),
            "aaa_reglage_{i:02} a ete ecrit malgre le refus"
        );
    }
}

/// Le pendant du retrait à l'export : une sauvegarde caviardée se restaure
/// sans détruire les secrets de la machine. C'est ce que promet le
/// commentaire historique d'`export_config`, et ce qu'un export MASQUÉ
/// aurait cassé — la restauration aurait écrit `********` par-dessus le vrai
/// secret de signature.
#[tokio::test]
async fn une_sauvegarde_caviardee_se_restaure_sans_detruire_les_secrets() {
    let state = new_state();
    semer_les_reglages(&state);
    let s = SettingsRepo::with_backend(state.backend.clone());

    let (st, sauvegarde) = get_body(&state, "/api/v1/system/config/export", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !sauvegarde.contains(tune_core::secrets::MASQUE),
        "une sauvegarde ne porte jamais le marqueur de masquage"
    );

    let (st, _) = post_json(&state, "/api/v1/system/config/import", None, &sauvegarde).await;
    assert_eq!(st, StatusCode::OK);

    assert_eq!(
        s.get("jwt_secret").unwrap().as_deref(),
        Some(FAUX_JWT),
        "le secret de signature a survecu a la restauration"
    );
    assert_eq!(s.get("theme").unwrap().as_deref(), Some(THEME_ATTENDU));
}
