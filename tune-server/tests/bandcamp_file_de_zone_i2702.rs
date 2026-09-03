//! Bandcamp est un SERVICE du registre, pas seulement un greffon (#2702, #2778).
//!
//! # Ce qui n'allait pas
//!
//! Bandcamp montait des routes sous `/api/v1/ext/bandcamp/…` et n'existait
//! nulle part dans `AppState::services`. Or les deux SEULES routes qui savent
//! construire une file complète — `POST /zones/{id}/play` avec
//! `streaming_album_id` ou `streaming_playlist_id` — commencent par
//! `registry.get(source)`. Pour `source = "bandcamp"` elles répondaient donc
//! `400 unknown service: bandcamp`, et il ne restait au client que le chemin
//! « piste distante seule », qui termine par `update_queue_info(zone, 0, 1)` :
//! une file d'EXACTEMENT une piste.
//!
//! C'est le défaut de Sevy Tabroc — « les morceaux Bandcamp ne s'enchaînent
//! pas » (#2702). Il n'y avait jamais de piste suivante : le poller trouvait
//! une file de longueur 1 et s'arrêtait.
//!
//! Et côté FabienM (#2778), l'état de liaison de « Ma collection » n'était
//! lisible par AUCUNE route : le greffon écrit `bandcamp_username` et
//! `bandcamp_fan_id` sans jamais les rendre.
//!
//! # Ce que ce fichier cloue
//!
//! 1. Bandcamp EST dans le registre — c'est la condition que les routes de
//!    file interrogent, et le seul geste qui les débloque.
//! 2. Une demande d'album Bandcamp ne rend plus `unknown service` : elle
//!    atteint l'adaptateur, qui NOMME son échec.
//! 3. Une playlist Bandcamp — qui n'existe pas chez Bandcamp — se refuse en le
//!    disant, au lieu de se confondre avec un service inconnu.
//! 4. Le TÉMOIN : les cinq services déjà inscrits sont toujours là, et
//!    répondent comme avant.
//! 5. L'état de liaison se lit par une route (#2778).
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.
//!
//! # Pourquoi ce fichier porte `#![cfg(feature = "bandcamp")]`
//!
//! Le service n'existe QUE sous cette fonctionnalité, et ce n'est pas un
//! choix de ce lot : `bandcamp = ["dep:tune-bandcamp"]` dans
//! `tune-server/Cargo.toml`, et `tune-bandcamp` y est déclarée
//! `optional = true`. Sans la fonctionnalité, la caisse du greffon n'est pas
//! compilée du tout : `BandcampService` n'existe pas, et l'inscription de
//! `state.rs` — elle-même sous `#[cfg(feature = "bandcamp")]` — disparaît.
//! Exiger ici un registre à six services reviendrait alors à exiger ce que le
//! binaire ne contient pas.
//!
//! C'est exactement ce qui a mis le run **33702848850** en rouge : le job
//! `Test` lançait `--no-default-features --features oaat,cloud-relay`, la
//! fonctionnalité était absente, et les cinq essais rougissaient sur un
//! registre à cinq services alors que rien n'était cassé. Le job
//! `Test (PostgreSQL)` (`--features postgres,oaat`) tombait pour la même
//! raison.
//!
//! ⚠️ Un `cfg` qui rend un test invisible est un faux vert de plus s'il reste
//! seul. Il ne reste pas seul : le job `Test` de `ci.yml` NOMME désormais
//! `bandcamp` dans son `--features`, donc ce fichier s'exécute sur chaque PR
//! Rust, et le garde `le_job_test_de_la_ci_active_bandcamp`
//! (`workflows_bornes.rs`) refuse qu'on l'en retire. Sans cette ligne de CI,
//! les cinq essais ne tourneraient que dans `test-shipped-features`, différé
//! jusqu'à `full` — donc jamais sur une PR vers `batch/*`, celle-ci comprise.
//!
//! Même idiome que `karaoke_plugin.rs`, module voisin du même agrégateur.
#![cfg(feature = "bandcamp")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn poster_texte(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn obtenir(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// 🔴 #2702 — le geste qui débloque tout : Bandcamp est dans `state.services`.
///
/// Sabotage : retirer le `services.register(BandcampService…)` de `state.rs`
/// fait tomber ce test, et avec lui les deux suivants.
#[tokio::test]
async fn bandcamp_est_inscrit_au_registre_des_services() {
    let etat = etat();
    let registre = etat.services.lock().await;
    assert!(
        registre.get("bandcamp").is_some(),
        "sans entrée « bandcamp » dans le registre, les routes de file \
         répondent 400 unknown service et la file reste à une piste (#2702) — \
         services inscrits : {:?}",
        registre.list()
    );
}

/// LE TÉMOIN : les cinq services déjà inscrits ne bougent pas.
#[tokio::test]
async fn les_services_deja_inscrits_ne_changent_pas() {
    let etat = etat();
    let registre = etat.services.lock().await;
    for nom in ["tidal", "qobuz", "spotify", "deezer", "youtube"] {
        assert!(
            registre.get(nom).is_some(),
            "{nom} doit rester inscrit — inscrits : {:?}",
            registre.list()
        );
    }
    assert_eq!(
        registre.list().len(),
        6,
        "cinq services d'origine plus Bandcamp, et rien d'autre : {:?}",
        registre.list()
    );
}

/// 🔴 #2702 — la route de file ATTEINT Bandcamp au lieu de le déclarer inconnu.
///
/// L'adresse d'album est volontairement invalide : l'épreuve ne doit dépendre
/// d'aucun accès réseau. Ce qui est mesuré est que le refus vient de
/// l'adaptateur Bandcamp — qui parle d'adresse — et non du registre, qui
/// parlait de service inconnu.
#[tokio::test]
async fn un_album_bandcamp_n_est_plus_un_service_inconnu() {
    let app = tune_server::routes::router(etat());
    let (status, corps) = poster_texte(
        &app,
        "/api/v1/zones/1/play",
        json!({ "source": "bandcamp", "streaming_album_id": "pas-une-adresse" }),
    )
    .await;
    assert!(
        !corps.contains("unknown service"),
        "la route de file ne doit plus ignorer Bandcamp (#2702) — {status} : {corps}"
    );
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "l'échec vient désormais de l'adaptateur : {corps}"
    );
    assert!(
        corps.contains("bandcamp.com"),
        "l'échec doit NOMMER ce qu'il attendait : {corps}"
    );
}

/// Bandcamp n'a pas de playlists. Le refus le DIT, au lieu de se confondre
/// avec un service absent du registre.
#[tokio::test]
async fn une_playlist_bandcamp_se_refuse_en_le_nommant() {
    let app = tune_server::routes::router(etat());
    let (status, corps) = poster_texte(
        &app,
        "/api/v1/zones/1/play",
        json!({ "source": "bandcamp", "streaming_playlist_id": "peu-importe" }),
    )
    .await;
    assert!(!corps.contains("unknown service"), "{status} : {corps}");
    assert!(
        corps.contains("playlists"),
        "le refus doit nommer ce qui manque : {status} — {corps}"
    );
}

/// 🔴 #2778 — l'état de liaison Bandcamp se LIT.
///
/// Aucune route ne le rendait : le greffon écrivait `bandcamp_username` et
/// `bandcamp_fan_id` et ne les relisait que pour `GET /collection`, qui répond
/// « aucun compte lié » sans distinguer « jamais lié » de « écriture perdue ».
/// L'inscription au registre donne `GET /streaming/bandcamp/status`.
#[tokio::test]
async fn l_etat_de_liaison_bandcamp_se_lit_par_une_route() {
    let etat = etat();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone());
    let app = tune_server::routes::router(etat);

    let (status, corps) = obtenir(&app, "/api/v1/streaming/bandcamp/status").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "la route d'état doit exister pour Bandcamp (#2778) : {corps}"
    );
    assert_eq!(corps["authenticated"], json!(false));

    reglages.set("bandcamp_username", "fabienm").unwrap();
    reglages.set("bandcamp_fan_id", "897100").unwrap();

    let (status, corps) = obtenir(&app, "/api/v1/streaming/bandcamp/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["authenticated"],
        json!(true),
        "un compte mémorisé doit se voir : {corps}"
    );
    assert_eq!(
        corps["username"],
        json!("fabienm"),
        "le pseudo lié doit être rendu — c'est l'« identifiant perdu » de \
         FabienM qui devient visible : {corps}"
    );
}
