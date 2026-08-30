//! Precedence de la langue sur les DEUX routes « bio » (#1849, Dimitri).
//!
//! ## Le trou que ces tests bouchent
//!
//! La moitie « selection de langue » etait livree : `langue_convient` compare
//! la langue de la bio stockee a celle qu'on demande, et le cache est indexe
//! par langue — sur la route artiste comme sur la route album.
//!
//! Mais les deux poignees ne lisaient QUE `?lang=`, avec un repli sec sur
//! `"fr"`. Or le client web n'envoie `?lang=` sur aucune des deux routes : il
//! transmet la locale choisie par l'en-tete `Accept-Language`, sur chaque
//! requete. Un utilisateur en interface anglaise arrivait donc avec
//! `Accept-Language: en` et sans parametre ; le serveur retenait `"fr"` ;
//! `langue_convient(Some("fr"), "fr")` etait vrai ; la bio francaise lui etait
//! resservie — exactement le defaut d'avant les correctifs, que ceux-ci ne
//! pouvaient pas voir puisque AUCUN test ne couvrait le chemin sans `?lang=`.
//!
//! ## La precedence gardee ici, pour chaque route
//!
//! 1. `?lang=` explicite **gagne** sur l'en-tete ;
//! 2. l'en-tete sert quand le parametre est absent ;
//! 3. ni l'un ni l'autre -> repli sur `fr`.
//!
//! ## Hermetique : aucun appel reseau
//!
//! Meme montage que `bios_langue_album.rs`. Chaque cas repond AVANT la moindre
//! requete sortante : soit la bio stockee convient et la route sort tout de
//! suite, soit elle ne convient pas et l'entree de cache — semee par le test,
//! indexee par langue — repond a la place du proxy communautaire.
//!
//! C'est ce qui rend les cas « en-tete » distinguants : sur l'ancien code la
//! langue retenue etait `fr`, la bio francaise stockee sortait, et le cache
//! anglais/allemand seme ici n'etait jamais consulte.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;

const NOM: &str = "Pink Floyd";
const TITRE: &str = "Animals";
const BIO_FR: &str = "Pink Floyd est un groupe britannique de rock progressif.";
const BIO_EN: &str = "Pink Floyd were an English progressive rock band.";
const BIO_DE: &str = "Pink Floyd war eine britische Rockband.";

type Etat = tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

// --- Artiste ---------------------------------------------------------------

/// Un artiste portant une bio et la langue de cette bio.
fn artiste_avec_bio(state: &Etat, id: i64, langue: &str, texte: &str) {
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&id as &dyn ToSqlValue, &NOM as &dyn ToSqlValue],
        )
        .expect("insertion de l'artiste");
    ArtistRepo::with_backend(state.backend.clone())
        .update_bio_full(
            id,
            texte,
            "wikipedia",
            Some("https://fr.wikipedia.org/wiki/Pink_Floyd".to_string()),
            "CC-BY-SA-3.0",
            langue,
        )
        .expect("ecriture de la bio et de sa langue");
}

/// Seme l'entree de cache de la route artiste : `cache:artistbio:{nom}:{langue}`.
fn cache_artiste(state: &Etat, langue: &str, bio: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("cache:artistbio:{NOM}:{langue}"),
            &json!({ "artist": NOM, "bio": bio, "source": "communaute" }).to_string(),
        )
        .expect("ecriture de l'entree de cache");
}

// --- Album -----------------------------------------------------------------

/// Un album nu (sans artiste) portant une bio et la langue de cette bio.
fn album_avec_bio(state: &Etat, id: i64, langue: &str, texte: &str) {
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
            texte,
            "wikipedia",
            Some("https://fr.wikipedia.org/wiki/Animals".to_string()),
            "CC-BY-SA-3.0",
            langue,
        )
        .expect("ecriture de la bio et de sa langue");
}

/// Seme l'entree de cache de la route album :
/// `cache:albumbio:{titre}:{artiste}:{langue}` — l'artiste est vide ici.
fn cache_album(state: &Etat, langue: &str, bio: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("cache:albumbio:{TITRE}::{langue}"),
            &json!({ "album": TITRE, "bio": bio, "source": "communaute" }).to_string(),
        )
        .expect("ecriture de l'entree de cache");
}

// --- Appel -----------------------------------------------------------------

/// Interroge `chemin`, en joignant `Accept-Language` seulement si `entete` est
/// donne — un `None` doit produire une requete SANS en-tete du tout, pas une
/// requete avec un en-tete vide.
async fn demander(app: &axum::Router, chemin: &str, entete: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(chemin);
    if let Some(valeur) = entete {
        req = req.header(axum::http::header::ACCEPT_LANGUAGE, valeur);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn bio_artiste(
    app: &axum::Router,
    requete: &str,
    entete: Option<&str>,
) -> (StatusCode, Value) {
    demander(
        app,
        &format!("/api/v1/library/artists/1/bio{requete}"),
        entete,
    )
    .await
}

async fn bio_album(app: &axum::Router, requete: &str, entete: Option<&str>) -> (StatusCode, Value) {
    demander(
        app,
        &format!("/api/v1/library/albums/1/bio{requete}"),
        entete,
    )
    .await
}

// === 1. `?lang=` explicite gagne sur l'en-tete =============================

/// Distinguant : la bio stockee est ANGLAISE et le cache seme est ALLEMAND. Si
/// l'en-tete l'emportait, la langue retenue serait `de`, la bio anglaise ne
/// conviendrait pas, et c'est la bio allemande du cache qui sortirait.
#[tokio::test]
async fn artiste_le_parametre_explicite_gagne_sur_l_entete() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "en", BIO_EN);
    cache_artiste(&state, "de", BIO_DE);

    let (status, body) = bio_artiste(&app, "?lang=en", Some("de")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_EN),
        "`?lang=en` doit gagner sur `Accept-Language: de`"
    );
    assert_eq!(body["bio_provenance"]["lang"].as_str(), Some("en"));
}

#[tokio::test]
async fn album_le_parametre_explicite_gagne_sur_l_entete() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "en", BIO_EN);
    cache_album(&state, "de", BIO_DE);

    let (status, body) = bio_album(&app, "?lang=en", Some("de")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_EN),
        "`?lang=en` doit gagner sur `Accept-Language: de`"
    );
    assert_eq!(body["bio_provenance"]["lang"].as_str(), Some("en"));
}

// === 2. Sans `?lang=`, l'en-tete gouverne — LE DEFAUT DE #1849 =============

/// **Le cas de Dimitri.** Interface anglaise : le client n'envoie pas `?lang=`,
/// il envoie `Accept-Language: en`. La bibliotheque a ete enrichie depuis une
/// interface francaise, donc la bio stockee est francaise.
///
/// Sur l'ancien code la langue retenue etait `fr`, la bio francaise sortait, et
/// le cache anglais n'etait jamais consulte : ce test ne peut pas passer.
#[tokio::test]
async fn artiste_sans_parametre_l_entete_gouverne() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "fr", BIO_FR);
    cache_artiste(&state, "en", BIO_EN);

    let (status, body) = bio_artiste(&app, "", Some("en")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_EN),
        "sans `?lang=`, `Accept-Language: en` doit etre entendu — la bio \
         francaise stockee a de nouveau ete resservie a un anglophone"
    );
}

#[tokio::test]
async fn album_sans_parametre_l_entete_gouverne() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr", BIO_FR);
    cache_album(&state, "en", BIO_EN);

    let (status, body) = bio_album(&app, "", Some("en")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some(BIO_EN),
        "sans `?lang=`, `Accept-Language: en` doit etre entendu — la bio \
         francaise stockee a de nouveau ete resservie a un anglophone"
    );
}

/// L'en-tete reel d'un navigateur est une liste ponderee, pas un code nu.
/// `lang_from_header` en tire la base supportee ; les routes « bio » doivent en
/// beneficier comme le reste de l'API.
#[tokio::test]
async fn artiste_l_entete_pondere_est_reduit_a_sa_base() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "fr", BIO_FR);
    cache_artiste(&state, "de", BIO_DE);

    let (_, body) = bio_artiste(&app, "", Some("de-DE,de;q=0.9,en;q=0.8")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_DE));
}

#[tokio::test]
async fn album_l_entete_pondere_est_reduit_a_sa_base() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr", BIO_FR);
    cache_album(&state, "de", BIO_DE);

    let (_, body) = bio_album(&app, "", Some("de-DE,de;q=0.9,en;q=0.8")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_DE));
}

/// Un `?lang=` vide ne nomme aucune langue : il ne doit pas court-circuiter
/// l'en-tete. Le laisser passer donnerait `lang = ""`, qui ne convient a aucune
/// bio estampillee et provoquerait un aller-retour reseau pour rien.
#[tokio::test]
async fn artiste_un_parametre_vide_laisse_parler_l_entete() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "fr", BIO_FR);
    cache_artiste(&state, "en", BIO_EN);

    let (_, body) = bio_artiste(&app, "?lang=", Some("en")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_EN));
}

#[tokio::test]
async fn album_un_parametre_vide_laisse_parler_l_entete() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr", BIO_FR);
    cache_album(&state, "en", BIO_EN);

    let (_, body) = bio_album(&app, "?lang=", Some("en")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_EN));
}

// === 3. Ni parametre ni en-tete -> repli sur `fr` ==========================

/// Non-regression : un appel nu (script, `curl`, ancien client) garde le
/// comportement d'avant. Le cache anglais est seme pour que le test ait quelque
/// chose d'autre a rendre s'il derivait vers l'anglais.
#[tokio::test]
async fn artiste_sans_parametre_ni_entete_le_repli_reste_le_francais() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "fr", BIO_FR);
    cache_artiste(&state, "en", BIO_EN);

    let (status, body) = bio_artiste(&app, "", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
    assert_eq!(body["bio_provenance"]["lang"].as_str(), Some("fr"));
}

#[tokio::test]
async fn album_sans_parametre_ni_entete_le_repli_reste_le_francais() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr", BIO_FR);
    cache_album(&state, "en", BIO_EN);

    let (status, body) = bio_album(&app, "", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
    assert_eq!(body["bio_provenance"]["lang"].as_str(), Some("fr"));
}

/// Une locale que l'interface ne parle pas (`lang_from_header` la refuse) doit
/// retomber sur `fr`, pas produire un code inconnu qui ne conviendrait a
/// aucune bio et declencherait un appel reseau.
#[tokio::test]
async fn artiste_une_locale_non_supportee_retombe_sur_le_francais() {
    let (app, state) = app_et_etat();
    artiste_avec_bio(&state, 1, "fr", BIO_FR);
    cache_artiste(&state, "en", BIO_EN);

    let (_, body) = bio_artiste(&app, "", Some("pt-BR")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
}

#[tokio::test]
async fn album_une_locale_non_supportee_retombe_sur_le_francais() {
    let (app, state) = app_et_etat();
    album_avec_bio(&state, 1, "fr", BIO_FR);
    cache_album(&state, "en", BIO_EN);

    let (_, body) = bio_album(&app, "", Some("pt-BR")).await;

    assert_eq!(body["bio"].as_str(), Some(BIO_FR));
}
