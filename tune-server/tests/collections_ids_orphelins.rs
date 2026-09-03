//! Identifiants d'albums MORTS dans un dossier « Collections » (#3285).
//!
//! Signalé par Lulu : la tuile d'un dossier et l'en-tête du dossier ouvert
//! affichaient deux nombres différents.
//!
//! Un dossier est une simple liste d'identifiants rangée dans le réglage
//! `collections` (`SettingsRepo`). RIEN ne la nettoie quand un album disparaît
//! — suppression, rescan qui réattribue un id, changement de chemin. La tuile
//! comptait `col.album_ids.length` (la liste servie telle quelle), l'en-tête
//! comptait ce que `GET /library/collections/{id}/albums` avait réellement
//! rendu, et cette route jetait les identifiants morts EN SILENCE :
//!
//! ```ignore
//! album_ids.iter().filter_map(|&aid| album_repo.get(aid).ok().flatten())
//! ```
//!
//! L'écart affiché valait donc exactement le nombre d'identifiants orphelins.
//! Le seuil « au-delà de 100 albums » du signalement était un leurre : aucun
//! `LIMIT` sur ce chemin, seulement une corrélation avec la taille du dossier.
//!
//! Ce que ces gardes tiennent :
//!   1. le compte ANNONCÉ (liste des dossiers) et le nombre d'éléments RENDUS
//!      (dossier ouvert) concordent, même avec des identifiants morts ;
//!   2. les identifiants morts sont DITS, dans `orphan_album_ids` ;
//!   3. la liste STOCKÉE n'est pas purgée par une simple lecture.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

fn make_app_with_state() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
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

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

fn seed_album(state: &tune_server::state::AppState, artist: &str, title: &str) -> i64 {
    let artists = ArtistRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let a = artists.get_or_create(artist, None, None).unwrap();
    let album = albums
        .get_or_create(title, a.id.unwrap(), None)
        .unwrap_or_else(|e| panic!("album {title}: {e}"));
    album.id.unwrap()
}

/// Monte un dossier contenant TROIS albums vivants et DEUX morts, exactement
/// comme la vie le fait : cinq albums rangés, puis deux qui disparaissent de la
/// base sans que personne ne touche au dossier.
async fn dossier_trois_vivants_deux_morts(
    app: &axum::Router,
    state: &tune_server::state::AppState,
) -> (i64, Vec<i64>) {
    let vivants = [
        seed_album(state, "ABBA", "Arrival"),
        seed_album(state, "Beethoven", "Symphonies"),
        seed_album(state, "Frank Zappa", "Hot Rats"),
    ];
    let condamnes = [
        seed_album(state, "Disque Retiré", "Rescan Perdu"),
        seed_album(state, "Chemin Changé", "Volume Démonté"),
    ];

    let (st, col) = post_json(
        app,
        "/api/v1/library/collections",
        json!({"name": "Coffret"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création du dossier: {col}");
    let cid = col["id"].as_i64().unwrap();

    // Rangés dans le désordre : les morts au milieu, pas en queue.
    for id in [
        vivants[0],
        condamnes[0],
        vivants[1],
        condamnes[1],
        vivants[2],
    ] {
        let (st, _) = post_json(
            app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "ajout de l'album {id}");
    }

    // Les albums s'en vont. Le dossier, lui, garde leurs identifiants.
    let albums = AlbumRepo::with_backend(state.backend.clone());
    for id in condamnes {
        albums.delete(id).unwrap();
    }
    (cid, condamnes.to_vec())
}

/// Combien d'identifiants le réglage `collections` garde-t-il pour ce dossier ?
fn ids_stockes(state: &tune_server::state::AppState, cid: i64) -> Vec<i64> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let brut = settings.get("collections").unwrap().unwrap();
    let dossiers: Vec<Value> = serde_json::from_str(&brut).unwrap();
    dossiers
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .expect("le dossier est dans le réglage")["album_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_i64())
        .collect()
}

/// 🔴 L'ÉPREUVE QUI TRANCHE. Trois vivants, deux morts : le compte annoncé par
/// la tuile et le nombre d'éléments rendus dans l'en-tête doivent concorder.
#[tokio::test]
async fn le_compte_annonce_egale_le_nombre_rendu() {
    let (app, state) = make_app_with_state();
    let (cid, _) = dossier_trois_vivants_deux_morts(&app, &state).await;

    // Ce que REND le dossier ouvert — `collectionAlbums.length` côté écran.
    let (st, rendus) = get(&app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    assert_eq!(st, StatusCode::OK, "lecture du dossier: {rendus}");
    let rendus = rendus.as_array().expect("un tableau nu, comme avant");
    assert_eq!(
        rendus.len(),
        3,
        "seuls les trois albums vivants sont rendus"
    );

    // Ce qu'ANNONCE la tuile — `col.album_ids?.length ?? col.album_count ?? 0`.
    let (st, liste) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK, "liste des dossiers: {liste}");
    let dossier = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .expect("le dossier est dans la liste")
        .clone();
    let annonce = dossier["album_ids"].as_array().unwrap().len();

    assert_eq!(
        annonce,
        rendus.len(),
        "la tuile annonce {annonce} et l'en-tête rend {} — c'est #3285",
        rendus.len()
    );
}

/// Les identifiants morts sont DITS, pas seulement retranchés.
#[tokio::test]
async fn les_identifiants_morts_sont_comptes_dans_un_champ_lisible() {
    let (app, state) = make_app_with_state();
    let (cid, _) = dossier_trois_vivants_deux_morts(&app, &state).await;

    let (_, liste) = get(&app, "/api/v1/library/collections").await;
    let dossier = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .unwrap()
        .clone();
    assert_eq!(
        dossier["orphan_album_ids"].as_i64(),
        Some(2),
        "les deux albums disparus doivent être comptés: {dossier}"
    );
    assert_eq!(dossier["album_count"].as_i64(), Some(3));

    // ⚠️ `GET /library/collections/{id}` rend DÉLIBÉRÉMENT la liste stockée,
    // verbatim : c'est la route du « ce qui est rangé », et aucun écran n'en
    // tire un compte (`api.ts` n'a pas de `getCollection` pour les dossiers
    // manuels). Elle garde donc les cinq identifiants, morts compris.
    let (st, seul) = get(&app, &format!("/api/v1/library/collections/{cid}")).await;
    assert_eq!(st, StatusCode::OK, "{seul}");
    assert_eq!(
        seul["album_ids"].as_array().unwrap().len(),
        5,
        "la route du stocké ne filtre pas: {seul}"
    );
}

/// ⚠️ Une simple LECTURE ne purge rien. Un album peut manquer parce qu'un
/// disque n'est pas monté ; détruire le rangement fait à la main sur un `GET`
/// serait pire que le bug.
#[tokio::test]
async fn une_lecture_ne_purge_pas_la_liste_stockee() {
    let (app, state) = make_app_with_state();
    let (cid, condamnes) = dossier_trois_vivants_deux_morts(&app, &state).await;

    assert_eq!(ids_stockes(&state, cid).len(), 5, "cinq au départ");
    let _ = get(&app, "/api/v1/library/collections").await;
    let _ = get(&app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    let _ = get(&app, &format!("/api/v1/library/collections/{cid}")).await;

    let apres = ids_stockes(&state, cid);
    assert_eq!(apres.len(), 5, "aucune écriture sur un GET: {apres:?}");
    for mort in condamnes {
        assert!(
            apres.contains(&mort),
            "l'identifiant {mort} est resté rangé, il est seulement SIGNALÉ"
        );
    }
}

/// Un dossier sain n'a rien d'orphelin, et rien n'est retranché : le témoin qui
/// interdit une garde verte contre un dossier vide.
#[tokio::test]
async fn un_dossier_sain_ne_declare_aucun_orphelin() {
    let (app, state) = make_app_with_state();
    let a = seed_album(&state, "Nina Simone", "Pastel Blues");
    let b = seed_album(&state, "Ella Fitzgerald", "Songbook");

    let (st, col) = post_json(&app, "/api/v1/library/collections", json!({"name": "Sain"})).await;
    assert_eq!(st, StatusCode::CREATED, "{col}");
    let cid = col["id"].as_i64().unwrap();
    for id in [a, b] {
        let (st, _) = post_json(
            &app,
            &format!("/api/v1/library/collections/{cid}/albums/{id}"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    let (_, liste) = get(&app, "/api/v1/library/collections").await;
    let dossier = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .unwrap()
        .clone();
    assert_eq!(dossier["orphan_album_ids"].as_i64(), Some(0));
    assert_eq!(dossier["album_ids"].as_array().unwrap().len(), 2);

    let (_, rendus) = get(&app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    assert_eq!(rendus.as_array().unwrap().len(), 2);
}

/// 🔴 LE POINT LE PLUS GRAVE. `.ok().flatten()` avalait aussi bien une PANNE de
/// base qu'un album supprimé : une lecture qui échoue rendait `200 []`, le même
/// écran qu'un dossier dont tous les albums ont disparu, et sans une ligne de
/// journal. Une panne n'est pas une absence : elle doit se voir.
///
/// La panne est fabriquée en retirant la table sous les pieds de la route —
/// c'est ce qu'une base corrompue, une migration à moitié passée ou des droits
/// retirés produisent : `SELECT ... FROM albums` échoue, il ne rend pas zéro
/// ligne.
#[tokio::test]
async fn une_panne_de_base_ne_se_deguise_pas_en_albums_absents() {
    let (app, state) = make_app_with_state();
    let (cid, _) = dossier_trois_vivants_deux_morts(&app, &state).await;

    // Témoin AVANT la panne : la route répond, et elle répond trois albums.
    let (st, avant) = get(&app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(avant.as_array().unwrap().len(), 3);

    state
        .backend
        .execute("DROP TABLE albums", &[])
        .expect("la table part");

    let (st, corps) = get(&app, &format!("/api/v1/library/collections/{cid}/albums")).await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une base en panne doit se dire, pas rendre un dossier vide: {corps}"
    );

    let (st, corps) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "la liste des dossiers non plus ne doit pas inventer un compte: {corps}"
    );

    // `GET /library/collections/{id}` ne touche pas à la table des albums : il
    // relit le réglage. Il répond donc encore, et c'est ce qu'on veut — il dit
    // ce qui est RANGÉ, pas ce qui est lisible.
    let (st, corps) = get(&app, &format!("/api/v1/library/collections/{cid}")).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "la route du stocké n'a pas besoin de la table des albums: {corps}"
    );
}
