//! BIB-A2 (phase 1) : l'absorption d'un éclat d'album par la route publique.
//!
//! Le cas est celui de l'enregistreur : un dossier, un disque, deux lignes
//! `albums` parce qu'un fichier a été indexé sous l'artiste de la piste.
//! Chaque témoin porte sa contre-épreuve : ce que la phase 0 voit avant, ce
//! qu'elle ne voit plus après, et les marqueurs qui doivent survivre.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn appel(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::empty())
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn inserer(state: &Etat, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    state.backend.execute(sql, params).unwrap();
    state.backend.last_insert_rowid()
}

fn artiste(state: &Etat, nom: &str) -> i64 {
    inserer(
        state,
        "INSERT INTO artists (name) VALUES (?)",
        &[&nom as &dyn ToSqlValue],
    )
}

fn album(state: &Etat, titre: &str, artist_id: i64) -> i64 {
    inserer(
        state,
        "INSERT INTO albums (title, artist_id, source, track_count) VALUES (?, ?, 'local', 0)",
        &[&titre as &dyn ToSqlValue, &artist_id],
    )
}

fn piste(state: &Etat, album_id: i64, artist_id: i64, numero: i64, chemin: &str) {
    inserer(
        state,
        "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path) VALUES (?, ?, ?, ?, ?)",
        &[
            &format!("Piste {numero}") as &dyn ToSqlValue,
            &album_id,
            &artist_id,
            &numero,
            &chemin,
        ],
    );
}

fn compte(state: &Etat, sql: &str, id: i64) -> i64 {
    state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn album_existe(state: &Etat, id: i64) -> bool {
    compte(state, "SELECT COUNT(*) FROM albums WHERE id = ?", id) > 0
}

/// Le disque de l'enregistreur : « The Wall » sous Pink Floyd (deux pistes),
/// et un éclat « The Wall » sous David Gilmour (une piste), même dossier.
fn le_disque_eclate(state: &Etat) -> (i64, i64, i64, i64) {
    let floyd = artiste(state, "Pink Floyd");
    let gilmour = artiste(state, "David Gilmour");
    let cible = album(state, "The Wall", floyd);
    let eclat = album(state, "The Wall", gilmour);
    piste(
        state,
        cible,
        floyd,
        1,
        "/musique/the wall/01 In the Flesh.flac",
    );
    piste(
        state,
        cible,
        floyd,
        2,
        "/musique/the wall/02 The Thin Ice.flac",
    );
    piste(
        state,
        eclat,
        gilmour,
        3,
        "/musique/the wall/03 Another Brick.flac",
    );
    (cible, eclat, floyd, gilmour)
}

#[tokio::test]
async fn l_eclat_de_l_enregistreur_est_absorbe_avec_ses_marqueurs() {
    let (app, state) = serveur();
    let (cible, eclat, _, _) = le_disque_eclate(&state);
    // Marqueurs posés sur l'ÉCLAT : ils doivent survivre sur la cible.
    inserer(
        &state,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
        &[&eclat as &dyn ToSqlValue],
    );
    inserer(
        &state,
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (?, 1, 5)",
        &[&eclat as &dyn ToSqlValue],
    );
    let tag = inserer(&state, "INSERT INTO tags (name) VALUES ('progressif')", &[]);
    inserer(
        &state,
        "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, 'album', ?)",
        &[&tag as &dyn ToSqlValue, &eclat],
    );
    state
        .backend
        .execute(
            "UPDATE albums SET cover_path = 'abcdef0123456789' WHERE id = ?",
            &[&eclat as &dyn ToSqlValue],
        )
        .unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "collections",
            &json!([{ "id": 1, "name": "Rock", "album_ids": [eclat, 999] }]).to_string(),
        )
        .unwrap();

    // Contre-épreuve : la phase 0 voit le faisceau.
    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert_eq!(avant["count"].as_u64(), Some(1), "avant : {avant}");

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["pistes"].as_u64(), Some(1));
    assert_eq!(corps["collections_reecrites"].as_u64(), Some(1));
    assert!(
        corps["champs_repris"].as_u64().unwrap_or(0) >= 1,
        "la pochette : {corps}"
    );

    let (_, apres) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert_eq!(apres["count"].as_u64(), Some(0), "après : {apres}");
    assert!(
        !album_existe(&state, eclat),
        "la ligne de l'éclat disparaît"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
            cible
        ),
        3
    );
    assert_eq!(
        compte(&state, "SELECT track_count FROM albums WHERE id = ?", cible),
        3
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'album' AND item_id = ?",
            cible
        ),
        1,
        "le favori suit"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT rating FROM album_ratings WHERE album_id = ?",
            cible
        ),
        5,
        "la note suit"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM item_tags WHERE item_type = 'album' AND item_id = ?",
            cible
        ),
        1,
        "l'étiquette suit"
    );
    let collections: Vec<Value> =
        serde_json::from_str(&settings.get("collections").unwrap().unwrap()).unwrap();
    assert_eq!(
        collections[0]["album_ids"],
        json!([cible, 999]),
        "le dossier pointe la cible"
    );
    let pochette = state
        .backend
        .query_one(
            "SELECT cover_path FROM albums WHERE id = ?",
            &[&cible as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(
        pochette.as_deref(),
        Some("abcdef0123456789"),
        "la pochette de l'éclat est reprise"
    );

    // Idempotence : l'éclat n'existe plus, rien ne se supprime deux fois.
    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn la_cible_garde_ses_propres_marqueurs_quand_les_deux_en_ont() {
    let (app, state) = serveur();
    let (cible, eclat, _, _) = le_disque_eclate(&state);
    inserer(
        &state,
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (?, 1, 2)",
        &[&cible as &dyn ToSqlValue],
    );
    inserer(
        &state,
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (?, 1, 5)",
        &[&eclat as &dyn ToSqlValue],
    );
    inserer(
        &state,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
        &[&cible as &dyn ToSqlValue],
    );
    inserer(
        &state,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
        &[&eclat as &dyn ToSqlValue],
    );
    state
        .backend
        .execute(
            "UPDATE albums SET cover_path = 'cible00' WHERE id = ?",
            &[&cible as &dyn ToSqlValue],
        )
        .unwrap();
    state
        .backend
        .execute(
            "UPDATE albums SET cover_path = 'eclat00' WHERE id = ?",
            &[&eclat as &dyn ToSqlValue],
        )
        .unwrap();

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(
        compte(
            &state,
            "SELECT rating FROM album_ratings WHERE album_id = ?",
            cible
        ),
        2,
        "la note de la cible ne cède pas"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM album_ratings WHERE album_id = ?",
            cible
        ),
        1
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'album' AND item_id = ?",
            cible
        ),
        1,
        "un seul favori, pas de doublon de clé"
    );
    let pochette = state
        .backend
        .query_one(
            "SELECT cover_path FROM albums WHERE id = ?",
            &[&cible as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(
        pochette.as_deref(),
        Some("cible00"),
        "la pochette de la cible ne cède pas"
    );
}

#[tokio::test]
async fn un_autre_dossier_un_autre_titre_ou_une_paire_distincte_sont_refuses() {
    let (app, state) = serveur();
    let (cible, eclat, _, gilmour) = le_disque_eclate(&state);

    let ailleurs = album(&state, "The Wall", gilmour);
    piste(
        &state,
        ailleurs,
        gilmour,
        1,
        "/musique/autre dossier/01.flac",
    );
    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{ailleurs}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("dossiers_differents")),
        "{corps}"
    );

    let autre_titre = album(&state, "Animals", gilmour);
    piste(
        &state,
        autre_titre,
        gilmour,
        1,
        "/musique/the wall/09 Pigs.flac",
    );
    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{autre_titre}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("titres_differents")),
        "{corps}"
    );

    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/distinct/{eclat}"),
    )
    .await;
    assert!(statut.is_success(), "déclaration distincte : {statut}");
    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("paire_declaree_distincte")),
        "{corps}"
    );
    assert!(
        album_existe(&state, cible) && album_existe(&state, eclat),
        "rien n'a bougé"
    );

    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{cible}"),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/424242"),
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}
// ---------------------------------------------------------------------------
// #3396 — les albums DÉCOUPÉS : « Disc 1 » / « Disc 2 », et un dossier par CD.
//
// BIB-A2 existait déjà et ne voyait ni l'un ni l'autre : sa clé de titre ne
// retirait aucun marqueur de tranche, et son dossier était celui du FICHIER.
// Les témoins passent par les DEUX routes publiques — la phase 0 qui propose,
// la phase 1 qui exécute — parce qu'une clé corrigée d'un seul côté ouvrirait
// une surface sur un geste que le serveur refuse.
// ---------------------------------------------------------------------------

/// Un coffret dont chaque disque est un dossier : `.../CD1`, `.../CD2`.
fn le_coffret_par_dossier(state: &Etat) -> (i64, i64) {
    let artiste_id = artiste(state, "The Beatles");
    let un = album(state, "The White Album", artiste_id);
    let deux = album(state, "The White Album", artiste_id);
    piste(
        state,
        un,
        artiste_id,
        1,
        "/musique/white album/CD1/01 Back.flac",
    );
    piste(
        state,
        un,
        artiste_id,
        2,
        "/musique/white album/CD1/02 Dear.flac",
    );
    piste(
        state,
        deux,
        artiste_id,
        1,
        "/musique/white album/CD2/01 Birthday.flac",
    );
    (un, deux)
}

/// Les numéros du groupe, dans l'ordre où la phase 0 les rend.
fn groupe_contenant(corps: &Value, ids: (i64, i64)) -> Option<Value> {
    corps["groups"]
        .as_array()?
        .iter()
        .find(|g| {
            let membres: Vec<i64> = g["albums"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["id"].as_i64()).collect())
                .unwrap_or_default();
            membres.contains(&ids.0) && membres.contains(&ids.1)
        })
        .cloned()
}

/// Premier cas nommé par le ticket : « un dossier par CD ». La phase 0 doit
/// les réunir, et la phase 1 doit accepter de les fusionner.
///
/// Contre-épreuve dans le même témoin : deux albums de même titre rangés dans
/// deux dossiers qui ne sont PAS des dossiers de disque restent séparés, et
/// leur absorption reste refusée par `dossiers_differents`. Sans elle, une
/// remontée d'un cran inconditionnelle passerait pour un correctif.
#[tokio::test]
async fn un_dossier_par_cd_est_un_seul_album() {
    let (app, state) = serveur();
    let (un, deux) = le_coffret_par_dossier(&state);

    let (statut, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let groupe = groupe_contenant(&corps, (un, deux))
        .unwrap_or_else(|| panic!("CD1 et CD2 doivent former un faisceau : {corps}"));
    assert_eq!(
        groupe["dossier"].as_str(),
        Some("/musique/white album"),
        "le faisceau porte le dossier de l'ALBUM, pas celui du disque"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{un}/absorber/{deux}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(!album_existe(&state, deux), "le second CD est absorbé");
    assert_eq!(
        compte(&state, "SELECT COUNT(*) FROM tracks WHERE album_id = ?", un),
        3,
        "les trois pistes du coffret sont sous un seul album"
    );

    // Contre-épreuve : « Bonus » n'est pas un numéro de disque.
    let (app, state) = serveur();
    let artiste_id = artiste(&state, "The Beatles");
    let a = album(&state, "The White Album", artiste_id);
    let b = album(&state, "The White Album", artiste_id);
    piste(
        &state,
        a,
        artiste_id,
        1,
        "/musique/white album/Bonus/01.flac",
    );
    piste(
        &state,
        b,
        artiste_id,
        1,
        "/musique/white album/Inedits/01.flac",
    );
    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&corps, (a, b)).is_none(),
        "deux dossiers qui ne sont pas des disques ne se rejoignent pas : {corps}"
    );
    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/absorber/{b}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("dossiers_differents")),
        "{corps}"
    );
}

/// Deuxième cas nommé par le ticket : la tranche est dans le TITRE.
///
/// Contre-épreuve dans le même témoin : `Vol. 1` / `Vol. 2` restent DEUX
/// albums. Le corps de l'issue demande de retirer aussi ce suffixe ; le
/// commentaire de `numero_de_disque` dit l'inverse et donne sa raison, et
/// c'est lui qui est suivi — fusionner deux volumes détruirait une
/// distinction voulue.
#[tokio::test]
async fn un_titre_qui_porte_sa_tranche_est_un_seul_album() {
    let (app, state) = serveur();
    let artiste_id = artiste(&state, "The Beatles");
    let un = album(&state, "The White Album (Disc 1)", artiste_id);
    let deux = album(&state, "The White Album — Disc 2", artiste_id);
    piste(
        &state,
        un,
        artiste_id,
        1,
        "/musique/white album/01 Back.flac",
    );
    piste(
        &state,
        deux,
        artiste_id,
        2,
        "/musique/white album/02 Birthday.flac",
    );

    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    let groupe = groupe_contenant(&corps, (un, deux))
        .unwrap_or_else(|| panic!("« Disc 1 » et « Disc 2 » sont un seul album : {corps}"));
    assert_eq!(
        groupe["titre_normalise"].as_str(),
        Some("the white album"),
        "la clé du faisceau ne porte plus la tranche"
    );
    assert_eq!(
        groupe["numeros_complementaires"].as_bool(),
        Some(true),
        "les numéros se complètent : c'est le garde-fou contre la réédition"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{un}/absorber/{deux}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(!album_existe(&state, deux));

    // Contre-épreuve : les VOLUMES ne sont pas des tranches.
    let (app, state) = serveur();
    let artiste_id = artiste(&state, "Queen");
    let v1 = album(&state, "Greatest Hits Vol. 1", artiste_id);
    let v2 = album(&state, "Greatest Hits Vol. 2", artiste_id);
    piste(&state, v1, artiste_id, 1, "/musique/queen/01 Bohemian.flac");
    piste(
        &state,
        v2,
        artiste_id,
        2,
        "/musique/queen/02 One Vision.flac",
    );
    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&corps, (v1, v2)).is_none(),
        "« Vol. 1 » et « Vol. 2 » sont deux albums : {corps}"
    );
    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{v1}/absorber/{v2}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("titres_differents")),
        "{corps}"
    );

    // Et un titre qui n'est QUE sa tranche garde sa clé : sinon tous les
    // albums nommés « CD 2 » convergeraient sur la clé vide.
    let (app, state) = serveur();
    let artiste_id = artiste(&state, "Inconnu");
    let seul = album(&state, "CD 2", artiste_id);
    let autre = album(&state, "CD 3", artiste_id);
    piste(&state, seul, artiste_id, 1, "/musique/vrac/01.flac");
    piste(&state, autre, artiste_id, 2, "/musique/vrac/02.flac");
    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&corps, (seul, autre)).is_none(),
        "« CD 2 » et « CD 3 » ne sont pas le même album : {corps}"
    );
}

// ── #3396 — le faisceau « pochette identique » ──────────────────────────────

/// Un condensat de pochette bien formé (64 hexadécimaux), distinct par graine.
fn condensat(graine: u8) -> String {
    format!("{graine:02x}").repeat(32)
}

fn poser_pochette(state: &Etat, album_id: i64, condensat: &str) {
    state
        .backend
        .execute(
            "UPDATE albums SET cover_path = ? WHERE id = ?",
            &[&condensat as &dyn ToSqlValue, &album_id],
        )
        .unwrap();
}

/// La compilation éclatée par ARTISTE : deux dossiers, deux titres, aucun des
/// deux indices historiques — mais la même pochette à l'octet près et des
/// numéros de piste qui se complètent. Elle doit être proposée en phase 0 ET
/// acceptée en phase 1 : proposer un geste que le serveur refuse est le défaut
/// que ce faisceau ne doit pas rouvrir.
#[tokio::test]
async fn la_compilation_eclatee_par_artiste_est_proposee_puis_absorbee() {
    let (app, state) = serveur();
    let davis = artiste(&state, "Miles Davis");
    let coltrane = artiste(&state, "John Coltrane");
    let cible = album(&state, "Jazz 70 — Davis", davis);
    let eclat = album(&state, "Jazz 70 — Coltrane", coltrane);
    piste(&state, cible, davis, 1, "/musique/jazz70/davis/01.flac");
    piste(&state, cible, davis, 2, "/musique/jazz70/davis/02.flac");
    piste(
        &state,
        eclat,
        coltrane,
        3,
        "/musique/jazz70/coltrane/01.flac",
    );
    let p = condensat(0xab);
    poser_pochette(&state, cible, &p);
    poser_pochette(&state, eclat, &p);

    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    let groupe = groupe_contenant(&avant, (cible, eclat))
        .unwrap_or_else(|| panic!("la phase 0 doit voir le faisceau : {avant}"));
    assert_eq!(groupe["indice"], "pochette_identique", "{groupe}");
    assert_eq!(groupe["pochette"], p, "{groupe}");

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["pistes"].as_u64(), Some(1), "{corps}");
    assert!(
        !album_existe(&state, eclat),
        "la ligne de l'éclat disparaît"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
            cible
        ),
        3
    );
}

/// Le garde-fou : une RÉÉDITION partage la pochette de son original et
/// recommence à la piste 1. Ni proposée, ni absorbable — l'écran ne doit pas
/// inviter à détruire une distinction voulue.
#[tokio::test]
async fn une_reedition_a_la_meme_pochette_mais_reste_un_album_a_part() {
    let (app, state) = serveur();
    let coltrane = artiste(&state, "John Coltrane");
    let original = album(&state, "Blue Train", coltrane);
    let remaster = album(&state, "Blue Train (Remaster)", coltrane);
    piste(&state, original, coltrane, 1, "/musique/orig/01.flac");
    piste(&state, original, coltrane, 2, "/musique/orig/02.flac");
    piste(&state, remaster, coltrane, 1, "/musique/remaster/01.flac");
    piste(&state, remaster, coltrane, 2, "/musique/remaster/02.flac");
    let p = condensat(0x5c);
    poser_pochette(&state, original, &p);
    poser_pochette(&state, remaster, &p);

    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&corps, (original, remaster)).is_none(),
        "une réédition n'est pas un éclat : {corps}"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{original}/absorber/{remaster}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("titres_differents")),
        "{corps}"
    );
}

/// Deux pochettes DIFFÉRENTES ne dispensent de rien : sans dossier commun ni
/// titre commun, le refus reste celui d'avant.
#[tokio::test]
async fn sans_pochette_commune_le_refus_reste_entier() {
    let (app, state) = serveur();
    let davis = artiste(&state, "Miles Davis");
    let coltrane = artiste(&state, "John Coltrane");
    let un = album(&state, "Jazz 70 — Davis", davis);
    let deux = album(&state, "Jazz 70 — Coltrane", coltrane);
    piste(&state, un, davis, 1, "/musique/jazz70/davis/01.flac");
    piste(
        &state,
        deux,
        coltrane,
        2,
        "/musique/jazz70/coltrane/01.flac",
    );
    poser_pochette(&state, un, &condensat(0x01));
    poser_pochette(&state, deux, &condensat(0x02));

    let (_, corps) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&corps, (un, deux)).is_none(),
        "deux pochettes différentes ne rapprochent rien : {corps}"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{un}/absorber/{deux}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("titres_differents")),
        "{corps}"
    );
}

/// Une paire déclarée distincte le reste, MÊME quand la pochette est la même :
/// l'arbitrage de l'utilisateur prime sur tous les indices.
#[tokio::test]
async fn la_paire_declaree_distincte_prime_sur_la_pochette() {
    let (app, state) = serveur();
    let davis = artiste(&state, "Miles Davis");
    let coltrane = artiste(&state, "John Coltrane");
    let cible = album(&state, "Jazz 70 — Davis", davis);
    let eclat = album(&state, "Jazz 70 — Coltrane", coltrane);
    piste(&state, cible, davis, 1, "/musique/jazz70/davis/01.flac");
    piste(
        &state,
        eclat,
        coltrane,
        2,
        "/musique/jazz70/coltrane/01.flac",
    );
    let p = condensat(0x7f);
    poser_pochette(&state, cible, &p);
    poser_pochette(&state, eclat, &p);
    inserer(
        &state,
        "INSERT INTO album_distinct_pairs (profile_id, album_a_id, album_b_id) VALUES (1, ?, ?)",
        &[&cible as &dyn ToSqlValue, &eclat],
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/absorber/{eclat}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("paire_declaree_distincte")),
        "{corps}"
    );
}

// ── #3396 — « de quoi écarter un groupe, pour qu'un faux positif ne revienne
//    pas indéfiniment » ───────────────────────────────────────────────────────

/// Le geste d'écart existait (`POST /albums/{id}/distinct/{autre}`, #1276) et
/// la phase 1 le respectait ; la phase 0 l'IGNORAIT. Le faux positif écarté
/// était donc reproposé à chaque analyse, et l'écran Métadonnées n'offrait que
/// de le réécarter — sans fin.
///
/// Le témoin part de la charge utile réelle : un vrai éclatement d'enregistreur
/// inséré en base, lu par la route publique. Il vérifie les trois temps, et
/// surtout le troisième : l'écart est un ARBITRAGE, pas une suppression. Les
/// deux lignes `albums` survivent, et révoquer l'arbitrage rend le groupe
/// identique — rien n'a été perdu.
#[tokio::test]
async fn la_paire_ecartee_sort_du_rapport_et_y_revient_si_l_arbitrage_est_revoque() {
    let (app, state) = serveur();
    let (cible, eclat, _, _) = le_disque_eclate(&state);

    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    let groupe_avant = groupe_contenant(&avant, (cible, eclat))
        .unwrap_or_else(|| panic!("la phase 0 doit voir l'éclatement : {avant}"));
    assert_eq!(groupe_avant["indice"], "dossier_et_titre", "{groupe_avant}");

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{cible}/distinct/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let (_, pendant) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&pendant, (cible, eclat)).is_none(),
        "un groupe écarté ne doit plus être proposé : {pendant}"
    );
    // L'écart ne détruit RIEN : les deux albums et leurs pistes sont intacts.
    assert!(album_existe(&state, cible) && album_existe(&state, eclat));
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
            eclat
        ),
        1,
        "l'écart ne déplace aucune piste"
    );

    let (statut, corps) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/albums/{cible}/distinct/{eclat}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let (_, apres) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    let groupe_apres = groupe_contenant(&apres, (cible, eclat))
        .unwrap_or_else(|| panic!("l'arbitrage révoqué, le groupe revient : {apres}"));
    assert_eq!(
        groupe_apres, groupe_avant,
        "le groupe revient à l'identique"
    );
}

/// Un coffret de trois tranches dont l'utilisateur n'a écarté qu'UNE paire.
///
/// L'arbitrage porte sur une paire, pas sur le groupe : le membre écarté sort,
/// les deux autres restent proposés, et les champs dérivés — `pistes`,
/// `numeros_complementaires` — ne décrivent plus que ce qui est rendu. Sans
/// cela, un groupe amputé annoncerait un total qu'il ne contient pas.
#[tokio::test]
async fn un_groupe_de_trois_perd_le_membre_ecarte_et_garde_les_autres() {
    let (app, state) = serveur();
    let artiste_id = artiste(&state, "Keith Jarrett");
    let un = album(&state, "Sun Bear Concerts Disc 1", artiste_id);
    let deux = album(&state, "Sun Bear Concerts Disc 2", artiste_id);
    let trois = album(&state, "Sun Bear Concerts Disc 3", artiste_id);
    piste(&state, un, artiste_id, 1, "/musique/sun bear/01.flac");
    piste(&state, un, artiste_id, 2, "/musique/sun bear/02.flac");
    piste(&state, deux, artiste_id, 3, "/musique/sun bear/03.flac");
    piste(&state, deux, artiste_id, 4, "/musique/sun bear/04.flac");
    piste(&state, trois, artiste_id, 5, "/musique/sun bear/05.flac");
    piste(&state, trois, artiste_id, 6, "/musique/sun bear/06.flac");

    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    let groupe = groupe_contenant(&avant, (un, trois))
        .unwrap_or_else(|| panic!("les trois tranches forment un groupe : {avant}"));
    assert_eq!(groupe["pistes"].as_u64(), Some(6), "{groupe}");

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{un}/distinct/{trois}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let (_, apres) = appel(&app, "GET", "/api/v1/library/albums/eclates").await;
    assert!(
        groupe_contenant(&apres, (un, trois)).is_none(),
        "la paire arbitrée ne survit dans aucun groupe : {apres}"
    );
    let restant = groupe_contenant(&apres, (un, deux))
        .unwrap_or_else(|| panic!("les deux autres tranches restent proposées : {apres}"));
    let membres: Vec<i64> = restant["albums"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_i64())
        .collect();
    assert_eq!(membres, vec![un, deux], "{restant}");
    assert_eq!(
        restant["pistes"].as_u64(),
        Some(4),
        "le total ne compte que les membres rendus : {restant}"
    );
    assert_eq!(restant["numeros_complementaires"], true, "{restant}");
    // Le troisième disque existe toujours, avec ses pistes.
    assert!(album_existe(&state, trois));
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
            trois
        ),
        2
    );
}
