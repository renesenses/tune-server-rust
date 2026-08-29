//! Contrat de `GET /library/albums/{id}/bio` sur la LANGUE (#1849, Dimitri).
//!
//! ## Ce que ces tests gardent
//!
//! #2126 a pose ce garde-fou sur la route **artiste** : la bio stockee n'est
//! resservie que si sa langue convient a celle qu'on demande. La route
//! **album** — que #1849 cite pourtant dans sa portee — ne l'avait pas. Elle
//! rendait la bio stockee sans regarder ni `albums.bio_lang` ni le `lang` de la
//! requete, et le `let lang = q.lang…` du dessous n'etait donc jamais atteint
//! pour un album qui possedait deja une bio.
//!
//! ## Hermetique : aucun appel reseau
//!
//! Le proxy communautaire (`mozaiklabs.fr`) n'est jamais joint ici. Chaque cas
//! rend une reponse **avant** la moindre requete : soit la bio stockee convient
//! et la route sort tout de suite, soit elle ne convient pas et l'entree de
//! cache — semee par le test, indexee par langue — repond a sa place.
//!
//! C'est aussi ce qui rend le cas negatif *distinguant* : avant le correctif la
//! route sortait sur la bio stockee et ne consultait jamais le cache ; le test
//! `…ne_court_circuite_plus_le_reste` ne peut donc pas passer sur l'ancien code.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;

const TITRE: &str = "Animals";
const BIO_FR: &str = "Animals est le dixieme album studio du groupe Pink Floyd.";
const BIO_EN: &str = "Animals is the tenth studio album by Pink Floyd.";

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// Un album nu (sans artiste) portant une bio et la langue de cette bio.
///
/// `langue` vide ecrit une colonne `bio_lang` vide — c'est exactement l'etat
/// des lignes enrichies avant que la provenance ne soit renseignee.
fn album_avec_bio(state: &tune_server::state::AppState, id: i64, langue: &str) {
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title) VALUES (?, ?)",
            &[&id as &dyn ToSqlValue, &TITRE as &dyn ToSqlValue],
        )
        .expect("insertion de l'album");
    AlbumRepo::with_backend(state.backend.clone())
        .update_bio_full(
            id,
            BIO_FR,
            "wikipedia",
            Some("https://fr.wikipedia.org/wiki/Animals".to_string()),
            "CC-BY-SA-3.0",
            langue,
        )
        .expect("ecriture de la bio et de sa langue");
}

/// Seme l'entree de cache que la route consulte pour la langue `langue`.
///
/// La cle est celle de `album_bio` : `cache:albumbio:{titre}:{artiste}:{langue}`
/// — l'artiste est vide ici, l'album n'en a pas.
fn semer_le_cache(state: &tune_server::state::AppState, langue: &str, bio: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("cache:albumbio:{TITRE}::{langue}"),
            &json!({ "album": TITRE, "bio": bio, "source": "communaute" }).to_string(),
        )
        .expect("ecriture de l'entree de cache");
}

async fn bio(app: &axum::Router, id: i64, lang: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/library/albums/{id}/bio?lang={lang}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Non-regression : le lecteur francais garde sa bio francaise, servie
/// localement avec sa provenance, sans le moindre appel au proxy.
#[tokio::test]
async fn une_bio_dans_la_langue_demandee_est_servie_telle_quelle() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr");

    let (status, body) = bio(&app, 1, "fr").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
    assert_eq!(
        body["bio_provenance"]["lang"].as_str(),
        Some("fr"),
        "la provenance doit accompagner la bio locale"
    );
}

/// **Le defaut de #1849, cote album.** Dimitri lit l'interface en anglais ; la
/// bibliotheque a ete enrichie depuis une interface en francais.
///
/// Avant le correctif, la route sortait sur la bio francaise stockee sans
/// jamais regarder les deux langues, donc sans jamais atteindre le cache. Ce
/// test ne peut pas passer sur l'ancien code.
///
/// Contre-epreuve (mesuree) : retirer `&& stored_ok` de la branche « bio
/// stockee » dans `album_bio` fait rougir CE test et
/// `le_cache_reste_indexe_par_langue` — les deux seuls du depot, les 194
/// autres cas de `server_contracts` restant verts. Le second rougit a bon
/// droit : sans le garde-fou la route ne consulte plus le cache du tout, donc
/// son indexation par langue n'est plus observable.
#[tokio::test]
async fn une_bio_dans_la_mauvaise_langue_ne_court_circuite_plus_le_reste() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr");
    semer_le_cache(&state, "en", BIO_EN);

    let (status, body) = bio(&app, 1, "en").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_EN),
        "la bio francaise stockee a de nouveau ete resservie a un lecteur anglophone"
    );
}

/// La langue demandee ne gouverne pas que la sortie : elle designe l'entree de
/// cache. Une bio anglaise en cache ne doit pas repondre a une demande
/// allemande — sinon le correctif deplacerait simplement le defaut.
#[tokio::test]
async fn le_cache_reste_indexe_par_langue() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr");
    semer_le_cache(&state, "en", BIO_EN);
    semer_le_cache(
        &state,
        "de",
        "Animals ist das zehnte Studioalbum von Pink Floyd.",
    );

    let (_, body) = bio(&app, 1, "de").await;

    assert_eq!(
        body["bio"].as_str(),
        Some("Animals ist das zehnte Studioalbum von Pink Floyd.")
    );
}

/// Choix delibere, repris de la route artiste (#2126) : une langue INCONNUE est
/// acceptee. La refuser declencherait un appel reseau pour chaque album dont la
/// provenance n'a jamais ete renseignee, a la premiere ouverture de chaque
/// fiche — et rien ne prouve que ces bios soient dans la mauvaise langue.
///
/// Le cache anglais est seme ici pour que le test ait quelque chose d'autre a
/// rendre s'il choisissait de passer outre : c'est bien la bio stockee qui doit
/// gagner.
#[tokio::test]
async fn une_bio_de_langue_inconnue_reste_servie() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "");
    semer_le_cache(&state, "en", BIO_EN);

    let (_, body) = bio(&app, 1, "en").await;

    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_FR),
        "une ligne ancienne, sans bio_lang, doit rester servie"
    );
}

/// `fr-FR` designe le francais. Un client qui annonce sa variante regionale ne
/// doit pas declencher un aller-retour pour rien.
#[tokio::test]
async fn une_variante_regionale_convient_a_la_langue_de_base() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr");
    semer_le_cache(&state, "fr-CA", BIO_EN);

    let (_, body) = bio(&app, 1, "fr-CA").await;

    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
}
