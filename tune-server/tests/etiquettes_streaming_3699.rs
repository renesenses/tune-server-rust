//! Étiqueter un album de STREAMING — les deux espaces d'identifiants (#3699).
//!
//! Relevé par Bertrand sur la 0.9.143 : « sur un album de streaming il manque
//! le bouton Étiquettes ». Le bouton était bien absent, et le client avait
//! **raison** de le cacher : `POST /tags/{id}/items` prend un `item_id: i64`,
//! la clef primaire d'un objet de la base LOCALE. Un album Qobuz, Tidal ou
//! Bandcamp n'en a pas — il porte la paire `source` + `source_id`.
//!
//! ## La forme reprise : celle des favoris
//!
//! Les favoris avaient déjà rencontré exactement ce mur. `favorites` est
//! indexée sur un entier, donc les favoris de streaming ont reçu leur propre
//! table `streaming_favorites`, indexée sur la paire, avec un INSTANTANÉ
//! d'affichage (`title`, `artist`, `album`, `cover_url`) posé à l'ajout. C'est
//! pourquoi le cœur s'affiche sur un album Qobuz et pas les étiquettes.
//!
//! `streaming_item_tags` suit cette forme, délibérément — pas un troisième
//! mécanisme.
//!
//! ## Ce que ce fichier garde, et par où il passe
//!
//! Tout passe par la **ROUTE MONTÉE** (`tune_server::routes::router`) : un
//! test qui appellerait `TagRepo` directement resterait vert alors que le
//! client, lui, ne peut atteindre le dépôt que par HTTP. C'est le défaut
//! dominant du dépôt — « écrit mais pas branché ».
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;

fn app_avec_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    (tune_server::routes::router(state.clone()), state)
}

async fn lire(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    lire(
        app.clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    lire(
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await
}

/// Crée une étiquette par la route et rend son identifiant.
async fn creer_etiquette(app: &axum::Router, nom: &str) -> i64 {
    // Sans barre oblique finale : `POST /api/v1/tags/` rend un **308**, pas un
    // 201 — la route est montee sur `/api/v1/tags` et la variante a barre
    // oblique n'est qu'une redirection. Mesure du 09/09.
    let (st, v) = post(app, "/api/v1/tags", json!({"name": nom})).await;
    assert_eq!(st, StatusCode::CREATED, "création de l'étiquette : {v}");
    v["id"].as_i64().expect("id d'étiquette")
}

/// Un album LOCAL, avec son artiste, et son identifiant entier.
fn semer_album_local(state: &tune_server::state::AppState, artiste: &str, titre: &str) -> i64 {
    let artistes = ArtistRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let a = artistes.get_or_create(artiste, None, None).unwrap();
    albums
        .get_or_create(titre, a.id.unwrap(), None)
        .unwrap_or_else(|e| panic!("album {titre} : {e}"))
        .id
        .unwrap()
}

/// Les titres d'albums rendus par `/tags/{id}/albums`, dans l'ordre.
async fn titres_des_albums(app: &axum::Router, tag: i64) -> Vec<String> {
    let (st, v) = get(app, &format!("/api/v1/tags/{tag}/albums")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["albums"]
        .as_array()
        .expect("albums")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

// --- 1. Le geste que le ticket réclame ---

/// Étiqueter un album Qobuz, le retrouver, le retirer — par la route.
#[tokio::test]
async fn etiqueter_un_album_de_streaming_par_la_route() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;

    let (st, v) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({
            "item_type": "album",
            "source": "qobuz",
            "source_id": "0060254735368",
            "title": "The Köln Concert",
            "artist": "Keith Jarrett",
            "cover_url": "https://static.qobuz.com/koln.jpg",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "pose sur un album Qobuz : {v}");

    // La liste par étiquette le rend, avec la PAIRE et sans identifiant local.
    let (st, v) = get(&app, &format!("/api/v1/tags/{tag}/albums")).await;
    assert_eq!(st, StatusCode::OK);
    let albums = v["albums"].as_array().expect("albums");
    assert_eq!(albums.len(), 1, "{v}");
    assert!(
        albums[0]["id"].is_null(),
        "un album de streaming ne doit PAS porter d'identifiant local : {}",
        albums[0]
    );
    assert_eq!(albums[0]["source"], "qobuz");
    assert_eq!(albums[0]["source_id"], "0060254735368");
    assert_eq!(albums[0]["title"], "The Köln Concert");
    assert_eq!(albums[0]["artist_name"], "Keith Jarrett");
    assert_eq!(albums[0]["cover_path"], "https://static.qobuz.com/koln.jpg");

    // Et la route symétrique de `/for/{item_type}/{item_id}` le sait aussi :
    // c'est elle que le panneau interroge à l'ouverture.
    let (st, v) = get(
        &app,
        "/api/v1/tags/for-streaming?item_type=album&source=qobuz&source_id=0060254735368",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.as_array().expect("étiquettes").len(), 1, "{v}");
    assert_eq!(v[0]["name"], "Nuit");

    // Retrait : par le corps de requête, jamais par le chemin.
    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items/remove"),
        json!({"item_type": "album", "source": "qobuz", "source_id": "0060254735368"}),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(titres_des_albums(&app, tag).await.is_empty());
}

// --- 2. Premier point du ticket : les DEUX espaces ---

/// `/tags/{id}/albums` rend l'espace local ET l'espace du streaming.
///
/// C'est le point 1 du ticket. Avant, la route ne connaissait que les entiers :
/// on pouvait poser une étiquette et ne jamais retrouver l'album.
#[tokio::test]
async fn la_liste_par_etiquette_rend_les_deux_espaces() {
    let (app, state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;

    let local = semer_album_local(&state, "Bill Evans", "Sunday at the Village Vanguard");
    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "album", "item_id": local}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({
            "item_type": "album", "source": "qobuz", "source_id": "12345",
            "title": "The Köln Concert", "artist": "Keith Jarrett",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let titres = titres_des_albums(&app, tag).await;
    assert!(
        titres.contains(&"Sunday at the Village Vanguard".to_string())
            && titres.contains(&"The Köln Concert".to_string()),
        "la liste par étiquette ne rend pas les deux espaces : {titres:?}"
    );

    // Le compte annoncé par `/tags` porte lui aussi sur les deux — sans quoi
    // l'écran promettrait deux objets et n'en montrerait qu'un.
    let (st, v) = get(&app, "/api/v1/tags").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v[0]["count"], 2,
        "le compte ignore l'espace du streaming : {v}"
    );

    // `/{id}/items` aussi : la ligne locale porte un entier et pas de source,
    // la ligne de streaming porte la paire et pas d'entier. **Jamais un entier
    // seul** — l'identifiant 1 de deux espaces ne désigne pas le même objet.
    let (st, v) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(st, StatusCode::OK);
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{v}");
    assert!(
        items
            .iter()
            .any(|i| i["item_id"].as_i64() == Some(local) && i["source"].is_null()),
        "la ligne locale a perdu sa forme : {v}"
    );
    assert!(
        items
            .iter()
            .any(|i| i["item_id"].is_null() && i["source"] == "qobuz" && i["source_id"] == "12345"),
        "la ligne de streaming manque, ou se fait passer pour un entier : {v}"
    );
}

// --- 3. Deuxième point du ticket : l'unicité porte sur la PAIRE ---

/// Le même album Qobuz étiqueté deux fois ne crée pas deux lignes — et deux
/// sources différentes portant le même identifiant restent deux objets.
#[tokio::test]
async fn l_unicite_porte_sur_la_paire() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    let pose = |src: &'static str, id: &'static str, titre: &'static str| {
        let app = app.clone();
        async move {
            post(
                &app,
                &format!("/api/v1/tags/{tag}/streaming-items"),
                json!({"item_type": "album", "source": src, "source_id": id, "title": titre}),
            )
            .await
        }
    };

    pose("qobuz", "12345", "Un").await;
    pose("qobuz", "12345", "Un").await;
    assert_eq!(
        titres_des_albums(&app, tag).await.len(),
        1,
        "le même album Qobuz étiqueté deux fois a créé deux lignes"
    );

    pose("tidal", "12345", "Autre").await;
    assert_eq!(
        titres_des_albums(&app, tag).await.len(),
        2,
        "« qobuz/12345 » et « tidal/12345 » ont été confondus : un identifiant \
         seul ne désigne rien sans sa source"
    );
}

// --- 4. Troisième point du ticket : l'album disparu du catalogue ---

/// **Un `source_id` mort ne bloque pas l'écran, et n'emporte pas ses voisins.**
///
/// C'est le point que le ticket demande explicitement de garder. Un album de
/// streaming peut disparaître : le service le retire, la licence change, le
/// compte est déconnecté. La règle est celle d'une pochette morte — la ligne
/// dégrade, elle ne bloque rien.
///
/// ## Ce que ce témoin fait mordre
///
/// Les trois albums de streaming posés ici sont **injoignables** :
///
///   * `qobuz/00000000000` — un identifiant qui n'existe dans aucun catalogue ;
///   * `tidal/id-retire-du-catalogue` — pas même la forme d'un identifiant Tidal ;
///   * `service-qui-nexiste-pas/…` — une source qu'aucun client de ce dépôt
///     ne sait instancier.
///
/// Et **aucun service de streaming n'est authentifié** dans cet `AppState`.
///
/// Une implémentation qui hydraterait la moitié streaming auprès du service
/// — le réflexe naturel, et ce que fait la moitié LOCALE avec
/// `AlbumRepo::get` — perdrait ces trois lignes (rien à résoudre), ou pire,
/// attendrait le réseau sur chacune. Le témoin exige qu'elles arrivent
/// entières, avec leur instantané, ET que l'album local qui les suit arrive
/// aussi : c'est la formulation mesurable de « jamais bloquer l'écran ».
#[tokio::test]
async fn un_album_de_streaming_disparu_du_catalogue_ne_bloque_pas_l_ecran() {
    let (app, state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;

    for (source, source_id, titre) in [
        ("qobuz", "00000000000", "Album retiré du catalogue"),
        ("tidal", "id-retire-du-catalogue", "Deuxième disparu"),
        (
            "service-qui-nexiste-pas",
            "peu-importe",
            "Troisième disparu",
        ),
    ] {
        let (st, v) = post(
            &app,
            &format!("/api/v1/tags/{tag}/streaming-items"),
            json!({
                "item_type": "album", "source": source, "source_id": source_id,
                "title": titre, "artist": "Artiste",
                "cover_url": "https://exemple.invalid/pochette-morte.jpg",
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
    }

    // Un album local POSÉ APRÈS les disparus : s'ils bloquaient la route, il
    // n'arriverait pas non plus.
    let local = semer_album_local(&state, "Bill Evans", "Waltz for Debby");
    post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "album", "item_id": local}),
    )
    .await;

    let (st, v) = get(&app, &format!("/api/v1/tags/{tag}/albums")).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "un `source_id` mort a fait tomber la route entière : {v}"
    );
    let titres = titres_des_albums(&app, tag).await;
    assert_eq!(
        titres.len(),
        4,
        "la liste a perdu des lignes en croisant un album disparu : {titres:?}"
    );
    assert!(
        titres.contains(&"Waltz for Debby".to_string()),
        "l'album LOCAL a disparu derrière les albums de streaming morts : {titres:?}"
    );
    for attendu in [
        "Album retiré du catalogue",
        "Deuxième disparu",
        "Troisième disparu",
    ] {
        assert!(
            titres.contains(&attendu.to_string()),
            "« {attendu} » a été escamoté : la liste par étiquette hydrate \
             donc auprès du service au lieu de lire son instantané, et un \
             album retiré du catalogue devient invisible : {titres:?}"
        );
    }

    // L'instantané est intact — c'est lui qui rend la ligne.
    let mort = v["albums"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["source"] == "tidal")
        .expect("l'album Tidal disparu");
    assert_eq!(mort["title"], "Deuxième disparu");
    assert_eq!(mort["artist_name"], "Artiste");
    assert_eq!(
        mort["cover_path"], "https://exemple.invalid/pochette-morte.jpg",
        "la pochette morte doit rester une URL que le client dégradera lui-même"
    );

    // Et l'étiquette reste retirable de l'album mort : sans quoi elle serait
    // indéracinable, exactement le défaut que `untag_item` évite côté local.
    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items/remove"),
        json!({"item_type": "album", "source": "tidal", "source_id": "id-retire-du-catalogue"}),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(titres_des_albums(&app, tag).await.len(), 3);
}

// --- 5. Les pièges de désignation ---

/// Un `source_id` de Bandcamp peut porter une barre oblique. Il voyage dans le
/// CORPS de la requête, jamais dans le chemin : un chemin le couperait en deux
/// et la route ne serait même pas atteinte.
#[tokio::test]
async fn un_source_id_avec_une_barre_oblique_voyage_entier() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    let id = "artiste.bandcamp.com/album/mon-album";

    let (st, v) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({"item_type": "album", "source": "bandcamp", "source_id": id, "title": "Mon album"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");

    let (_, v) = get(&app, &format!("/api/v1/tags/{tag}/albums")).await;
    assert_eq!(
        v["albums"][0]["source_id"], id,
        "le `source_id` a été tronqué : {v}"
    );

    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items/remove"),
        json!({"item_type": "album", "source": "bandcamp", "source_id": id}),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(titres_des_albums(&app, tag).await.is_empty());
}

/// Un `item_type` inconnu est refusé ici AUSSI (#2256). Le garde-fou posé côté
/// local ne doit pas rouvrir par la porte du streaming.
#[tokio::test]
async fn un_item_type_inconnu_est_refuse_dans_l_espace_du_streaming() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    let (st, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({"item_type": "albums", "source": "qobuz", "source_id": "1"}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(titres_des_albums(&app, tag).await.is_empty());
}

/// La PAIRE, ou rien : une source sans identifiant ne désigne aucun objet.
#[tokio::test]
async fn une_designation_incomplete_est_refusee() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    for corps in [
        json!({"item_type": "album", "source": "qobuz", "source_id": ""}),
        json!({"item_type": "album", "source": "", "source_id": "12345"}),
    ] {
        let (st, _) = post(&app, &format!("/api/v1/tags/{tag}/streaming-items"), corps).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }
    assert!(titres_des_albums(&app, tag).await.is_empty());
}

/// Les quatre familles étiquetables valent aussi dans l'espace du streaming :
/// un artiste Qobuz se range comme un album Qobuz.
#[tokio::test]
async fn les_autres_familles_valent_aussi_pour_le_streaming() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({
            "item_type": "artist", "source": "qobuz", "source_id": "a-42",
            "title": "Keith Jarrett", "cover_url": "https://static.qobuz.com/kj.jpg",
        }),
    )
    .await;

    let (st, v) = get(&app, &format!("/api/v1/tags/{tag}/artists")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["artists"].as_array().unwrap().len(), 1, "{v}");
    assert!(v["artists"][0]["id"].is_null());
    assert_eq!(v["artists"][0]["name"], "Keith Jarrett");
    assert_eq!(v["artists"][0]["source"], "qobuz");
    assert_eq!(v["artists"][0]["source_id"], "a-42");

    // Et un artiste ne se retrouve pas dans les albums.
    assert!(titres_des_albums(&app, tag).await.is_empty());
}

/// Supprimer l'étiquette emporte ses poses de streaming : la table ne porte
/// aucune clef étrangère, le nettoyage est explicite dans `TagRepo::delete`.
#[tokio::test]
async fn supprimer_l_etiquette_emporte_ses_poses_de_streaming() {
    let (app, _state) = app_avec_etat();
    let tag = creer_etiquette(&app, "Nuit").await;
    post(
        &app,
        &format!("/api/v1/tags/{tag}/streaming-items"),
        json!({"item_type": "album", "source": "qobuz", "source_id": "12345", "title": "Un"}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/tags/{tag}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (st, v) = get(
        &app,
        "/api/v1/tags/for-streaming?item_type=album&source=qobuz&source_id=12345",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        v.as_array().expect("étiquettes").is_empty(),
        "la pose a survécu à la suppression de son étiquette : {v}"
    );
}
