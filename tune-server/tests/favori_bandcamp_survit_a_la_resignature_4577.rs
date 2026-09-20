//! #4577 — un favori Bandcamp posé une fois se retrouve la fois suivante.
//!
//! # Le signalement
//!
//! FabienM, fil 1862 point 4, en 0.9.158 : « on peut mettre un titre bandcamp
//! en favori mais celui-ci n'est pas conservé » — exemple donné, *Tiny
//! Darkness* de Soda Blonde.
//!
//! # Ce que ce témoin mesure, et pourquoi il fallait le mesurer ICI
//!
//! Le cœur d'un objet de service est tenu par Tune, dans
//! `streaming_favorites`, par `POST /profiles/{id}/favorites/streaming/add` —
//! pas chez Bandcamp, qui n'accepte aucune écriture sans session d'achat. Ce
//! chemin-là marchait déjà… à la clé près.
//!
//! Le `service_id` d'un titre Bandcamp est son URL de flux mp3-128, parce que
//! c'est elle que la file rejoue. Or Bandcamp la **resigne à chaque lecture de
//! la page** : `ts`, `t` et `token` changent, le chemin non. Deux lectures de
//! `https://sodablonde.bandcamp.com/album/dream-big` à trois secondes
//! d'écart, mesurées le 20/09/2026, donnent les deux URL que ce fichier
//! emploie telles quelles. La ligne écrite sous la première n'était donc
//! jamais retrouvée sous la seconde : cœur vide au rechargement, retrait sans
//! effet, et une ligne de plus à chaque clic.
//!
//! Les essais passent donc par les ROUTES montées, avec les vraies URL
//! mesurées, et vérifient la BASE — pas seulement un code de retour. Un 200
//! qui n'écrit rien est le mensonge que ce ticket décrit.
//!
//! **Contre-épreuve** : retirer l'appel à `identite_de_favori` dans
//! `StreamingFavoritesRepo` (`tune-core/src/db/streaming_favorites_repo.rs`)
//! rend rouges `le_coeur_se_retrouve_sous_une_autre_signature`,
//! `reposer_le_meme_titre_ne_cree_pas_de_doublon` et
//! `le_retrait_trouve_la_ligne_sous_une_troisieme_signature`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;
use tune_server::state::AppState;

// --- les trois signatures MESURÉES -------------------------------------
//
// Même piste (« Midnight Show », empreinte `58db2888…`, id de piste
// `2639113545`), trois requêtes différentes. La troisième est fabriquée sur le
// même modèle : ce que l'essai a besoin de dire, c'est qu'une signature encore
// inconnue retrouve la ligne.

const SIGNATURE_A: &str = "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545?p=0&ts=1789982173&t=a130059f109193afe2e59864b82c20f5f3446c7d&token=1789982173_7285db0763aa47e79bc49785feea7c459b90ec0e";
const SIGNATURE_B: &str = "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545?p=0&ts=1789982176&t=d8bb27b0915e21b4430a9c4eca79f57ac9fb39c7&token=1789982176_edadeaf99874538aee5934700e58a102414dfd8c";
const SIGNATURE_C: &str = "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545?p=0&ts=1789990000&t=0000000000000000000000000000000000000000&token=1789990000_1111111111111111111111111111111111111111";

/// L'identité attendue : le chemin, sans la requête resignée.
const IDENTITE: &str =
    "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545";

/// Un ALBUM Bandcamp est désigné par l'adresse de sa page, qui elle est déjà
/// stable — le témoin le vérifie pour qu'aucune normalisation trop large ne
/// vienne la tronquer.
const ALBUM: &str = "https://sodablonde.bandcamp.com/album/dream-big";

const PROFIL: i64 = 1;

// --- outillage ---------------------------------------------------------

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

async fn envoyer(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

fn poster(chemin: &str, corps: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(chemin)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Profile-Id", PROFIL.to_string())
        .body(Body::from(corps.to_string()))
        .unwrap()
}

fn lire(chemin: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(chemin)
        .header("X-Profile-Id", PROFIL.to_string())
        .body(Body::empty())
        .unwrap()
}

fn corps_piste(service_id: &str) -> Value {
    json!({
        "item_type": "track",
        "service": "bandcamp",
        "service_id": service_id,
        "title": "Midnight Show",
        "artist": "Soda Blonde",
        "album": "Dream Big",
        "cover_url": "https://f4.bcbits.com/img/a0000000000_10.jpg",
    })
}

fn corps_album(service_id: &str) -> Value {
    json!({
        "item_type": "album",
        "service": "bandcamp",
        "service_id": service_id,
        "title": "Dream Big",
        "artist": "Soda Blonde",
    })
}

/// Les `service_id` réellement en base pour un type, lus par le dépôt et non
/// par la route : c'est la base qui doit avoir changé, pas la réponse.
fn ids_en_base(state: &AppState, item_type: &str) -> Vec<String> {
    StreamingFavoritesRepo::with_backend(state.backend.clone())
        .list(PROFIL, Some(item_type))
        .expect("lecture des favoris de service")
        .into_iter()
        .map(|f| f.service_id)
        .collect()
}

// --- les essais --------------------------------------------------------

/// Le défaut de FabienM, dans l'ordre où il l'a vécu : cœur posé sur une page,
/// page rechargée, cœur relu.
#[tokio::test]
async fn le_coeur_se_retrouve_sous_une_autre_signature() {
    let state = etat();
    let app = appli(&state);

    let (statut, _) = envoyer(
        &app,
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_piste(SIGNATURE_A),
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "l'ajout doit aboutir");

    // Ce que la base retient n'est pas la signature : c'est l'identité.
    assert_eq!(
        ids_en_base(&state, "track"),
        vec![IDENTITE.to_string()],
        "la ligne doit porter l'identité stable, pas l'URL signée"
    );

    // Et c'est bien la clé sous laquelle une signature NEUVE la retrouve.
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    assert!(
        repo.is_favorite(PROFIL, "track", "bandcamp", SIGNATURE_B)
            .unwrap(),
        "la page rechargée, resignée, doit retrouver le favori"
    );
}

/// Ce que voyait FabienM au clic suivant : une ligne de plus, jamais la même.
#[tokio::test]
async fn reposer_le_meme_titre_ne_cree_pas_de_doublon() {
    let state = etat();
    let app = appli(&state);

    for signature in [SIGNATURE_A, SIGNATURE_B, SIGNATURE_C] {
        let (statut, _) = envoyer(
            &app,
            poster(
                "/api/v1/profiles/1/favorites/streaming/add",
                corps_piste(signature),
            ),
        )
        .await;
        assert_eq!(statut, StatusCode::CREATED);
    }

    assert_eq!(
        ids_en_base(&state, "track"),
        vec![IDENTITE.to_string()],
        "trois signatures du même titre = UN favori"
    );
}

/// « retirer un favori bandcamp ne fait rien » : le DELETE ne trouvait pas la
/// ligne, parce qu'il la cherchait sous une signature que personne n'avait
/// écrite.
#[tokio::test]
async fn le_retrait_trouve_la_ligne_sous_une_troisieme_signature() {
    let state = etat();
    let app = appli(&state);

    envoyer(
        &app,
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_piste(SIGNATURE_A),
        ),
    )
    .await;
    assert_eq!(ids_en_base(&state, "track").len(), 1, "posé");

    let (statut, _) = envoyer(
        &app,
        poster(
            "/api/v1/profiles/1/favorites/streaming/remove",
            json!({
                "item_type": "track",
                "service": "bandcamp",
                "service_id": SIGNATURE_C,
            }),
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert!(
        ids_en_base(&state, "track").is_empty(),
        "le retrait doit vider la table, quelle que soit la signature présentée"
    );
}

/// Un album Bandcamp est désigné par l'adresse de sa page : rien à normaliser,
/// et surtout rien à couper. Sans cet essai, une normalisation trop large
/// passerait inaperçue jusqu'à ce que les albums favoris ne s'ouvrent plus.
#[tokio::test]
async fn l_adresse_d_un_album_traverse_intacte() {
    let state = etat();
    let app = appli(&state);

    let (statut, _) = envoyer(
        &app,
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_album(ALBUM),
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED);
    assert_eq!(ids_en_base(&state, "album"), vec![ALBUM.to_string()]);
}

/// Le favori survit à une relecture par la ROUTE que l'écran Favoris emploie,
/// avec son libellé et sa pochette — un cœur qui se rallume sur une ligne vide
/// ne vaudrait pas mieux.
#[tokio::test]
async fn la_route_des_favoris_le_rend_avec_ses_libelles() {
    let state = etat();
    let app = appli(&state);

    envoyer(
        &app,
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_piste(SIGNATURE_A),
        ),
    )
    .await;

    let (statut, corps) = envoyer(
        &app,
        lire("/api/v1/profiles/1/favorites/streaming?item_type=track"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let lignes = corps.as_array().expect("une liste");
    assert_eq!(lignes.len(), 1, "un favori et un seul");
    assert_eq!(lignes[0]["service_id"], json!(IDENTITE));
    assert_eq!(lignes[0]["service"], json!("bandcamp"));
    assert_eq!(lignes[0]["title"], json!("Midnight Show"));
    assert_eq!(lignes[0]["artist"], json!("Soda Blonde"));
}

/// Le favori est tenu par Tune, donc il survit au redémarrage : un second
/// `AppState` sur le MÊME fichier de base relit la ligne. `:memory:` ne le
/// dirait pas — il faut un fichier.
#[tokio::test]
async fn le_favori_survit_au_redemarrage() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let base = dossier.path().join("tune-4577.db");
    let chemin = base.to_str().expect("chemin utf-8").to_string();

    {
        let state = AppState::new(&chemin, 0, Default::default()).unwrap();
        let app = appli(&state);
        let (statut, _) = envoyer(
            &app,
            poster(
                "/api/v1/profiles/1/favorites/streaming/add",
                corps_piste(SIGNATURE_A),
            ),
        )
        .await;
        assert_eq!(statut, StatusCode::CREATED);
    }

    let state = AppState::new(&chemin, 0, Default::default()).unwrap();
    assert_eq!(
        ids_en_base(&state, "track"),
        vec![IDENTITE.to_string()],
        "le favori doit être là après redémarrage"
    );
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    assert!(
        repo.is_favorite(PROFIL, "track", "bandcamp", SIGNATURE_B)
            .unwrap(),
        "et se retrouver sous la signature que la page servira ensuite"
    );
}

/// Aucune réponse 501 ne sort du chemin du cœur. Le 501 du journal de FabienM
/// venait de la RECOPIE vers le service (`/streaming/bandcamp/favorites/…`),
/// que Bandcamp ne peut pas accepter ; le chemin qui porte réellement le
/// favori, lui, n'a jamais à le rendre — et cet essai interdit qu'il s'y mette.
#[tokio::test]
async fn le_chemin_du_coeur_ne_rend_jamais_501() {
    let state = etat();
    let app = appli(&state);

    let gestes = [
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_piste(SIGNATURE_A),
        ),
        poster(
            "/api/v1/profiles/1/favorites/streaming/add",
            corps_album(ALBUM),
        ),
        poster(
            "/api/v1/profiles/1/favorites/streaming/remove",
            json!({"item_type": "track", "service": "bandcamp", "service_id": SIGNATURE_B}),
        ),
        lire("/api/v1/profiles/1/favorites/streaming"),
    ];
    for geste in gestes {
        let chemin = geste.uri().to_string();
        let (statut, _) = envoyer(&app, geste).await;
        assert_ne!(
            statut,
            StatusCode::NOT_IMPLEMENTED,
            "501 sur {chemin} : le cœur ne doit dépendre d'aucune écriture chez Bandcamp"
        );
        assert!(statut.is_success(), "{chemin} → {statut}");
    }
}
