//! #2794 / #3073 — les cinq surfaces laissées hors du périmètre.
//!
//! #3073 a cloisonné les douze accès par id de `routes/playlists.rs`. Le même
//! défaut — un `WHERE id = ?` nu sur des ids séquentiels — restait nu ailleurs :
//!
//! | route | ce que l'id d'un autre profil donnait |
//! |---|---|
//! | `POST /playlist-manager/transfer` | le nom, les titres, et une copie chez soi |
//! | `POST /playlist-manager/merge` | le contenu, recopié dans une playlist à soi |
//! | `POST /playlist-manager/export` | tout, en clair, sans aucune identité d'appelant |
//! | `POST /playlist-manager/links` + `/links/{id}/sync` | l'écriture de pistes chez le voisin |
//! | `POST /zones/{id}/play` (`playlist_id`) | la file de la zone, puis la lecture |
//!
//! ## Ce que ce fichier prouve, et comment
//!
//! Même patron de preuve que #3073, et pour la même raison : **le code de
//! retour ne prouve rien**. Un 404 poli posé devant une base déjà modifiée
//! passerait. Chaque refus est donc vérifié EN BASE — aucune playlist créée,
//! aucun lien inscrit dans les réglages, aucune entrée d'historique, file de
//! zone vide.
//!
//! Et chaque refus opposé au profil 2 est doublé du **même appel par le profil
//! 1, qui doit réussir**. Sans ce témoin, un handler qui répondrait 404 à tout
//! le monde — ou une route simplement cassée — passerait le test.
//!
//! Le contrat retenu est celui de #3073 : **404, jamais 403**. Un 403
//! confirmerait l'existence de la playlist et rendrait l'énumération des ids
//! exploitable malgré le refus.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// --- outillage ---------------------------------------------------------

const P1: &str = "1";
const P2: &str = "2";

fn etat() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le profil visé par `X-Profile-Id` doit exister, sinon l'extracteur
    // retombe sur le profil actif global et les deux « utilisateurs » de
    // l'essai seraient le même.
    let profils = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone());
    let id = profils
        .create("voisin", Some("Le voisin"), None)
        .expect("create profile");
    assert_eq!(id, 2, "le second profil doit porter l'id 2");
    state
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    profil: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", profil);
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

// --- lectures en base (la seule preuve qui compte) ---------------------

/// Les playlists appartenant à `profil`, par leur nom.
fn playlists_du_profil(state: &tune_server::state::AppState, profil: i64) -> Vec<String> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .list(profil, 9999, 0)
        .expect("list playlists")
        .into_iter()
        .map(|p| p.name)
        .collect()
}

fn pistes_en_base(state: &tune_server::state::AppState, id: i64) -> Vec<i64> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .get_track_ids(id)
        .expect("get_track_ids")
}

/// Le contenu brut d'un réglage-fourre-tout (`playlist_links`,
/// `playlist_transfer_history`) : ces deux-là sont communs au foyer, donc
/// lisibles tels quels.
fn reglage_json(state: &tune_server::state::AppState, cle: &str) -> Vec<Value> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(cle)
        .expect("lecture réglage")
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

/// Une playlist du profil 1, garnie de deux pistes présentes en bibliothèque.
async fn playlist_du_profil_1(
    state: &tune_server::state::AppState,
    app: &axum::Router,
) -> (i64, i64, i64) {
    let t1 = piste(state, "Piste Un", "/musique/un.flac");
    let t2 = piste(state, "Piste Deux", "/musique/deux.flac");
    let (st, body) = appel(
        app,
        "POST",
        "/api/v1/playlists",
        P1,
        Some(json!({"name": "Privee du profil 1"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = body["id"].as_i64().expect("id playlist");
    let (st, _) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"track_ids": [t1, t2]})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    (id, t1, t2)
}

// --- 1. transfert ------------------------------------------------------

/// `POST /playlist-manager/transfer`, `source_service = "local"` : l'id de la
/// source partait d'un `get_track_ids` + `get` nus. Le profil 2 obtenait le nom
/// de la playlist, la liste de ses titres dans `details[]`, **et** une copie
/// créée à son propre nom.
#[tokio::test]
async fn transferer_la_playlist_d_un_autre_profil_ne_cree_aucune_copie() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let corps = json!({
        "source_service": "local",
        "source_playlist_id": id.to_string(),
        "target_service": "local",
    });

    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        P2,
        Some(corps.clone()),
    )
    .await;

    // Preuve en base D'ABORD : rien n'a été créé, ni chez le profil 2, ni
    // ailleurs, et l'historique de transfert — écrit systématiquement en fin de
    // route — est resté vide. Le code de retour ne vient qu'ensuite : un 404
    // poli posé devant une base déjà modifiée ne prouverait rien.
    assert!(
        playlists_du_profil(&state, 2).is_empty(),
        "une copie a été créée chez le profil 2 : {:?}",
        playlists_du_profil(&state, 2)
    );
    assert_eq!(
        playlists_du_profil(&state, 1),
        vec!["Privee du profil 1".to_string()],
        "une playlist est apparue malgré le refus"
    );
    assert!(
        reglage_json(&state, "playlist_transfer_history").is_empty(),
        "le transfert refusé a laissé une trace dans l'historique"
    );
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(
        !body.to_string().contains("Privee du profil 1") && !body.to_string().contains("Piste Un"),
        "le refus a quand même récité la source : {body}"
    );

    // Témoin : le propriétaire, lui, transfère — et la route lit bien sa source.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        P1,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["source_playlist_name"], "Privee du profil 1",
        "le témoin doit prouver que la source a été LUE : {body}"
    );
    assert_eq!(body["total_tracks"], 2, "{body}");
    assert_eq!(
        playlists_du_profil(&state, 1).len(),
        2,
        "le témoin n'a rien créé : le refus ne prouverait alors rien"
    );
}

// --- 2. fusion ---------------------------------------------------------

/// `POST /playlist-manager/merge` : seule la CRÉATION portait `profile_id`.
/// Les sources partaient d'un `get_track_ids` nu — la fusion recopiait donc
/// chez l'appelant le contenu de n'importe quelle playlist du foyer.
#[tokio::test]
async fn fusionner_la_playlist_d_un_autre_profil_ne_recopie_aucune_piste() {
    let state = etat();
    let app = appli(&state);
    let (id, t1, t2) = playlist_du_profil_1(&state, &app).await;

    let corps = json!({
        "playlists": [{"service": "local", "playlist_id": id.to_string()}],
        "target_name": "Butin",
    });

    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/merge",
        P2,
        Some(corps.clone()),
    )
    .await;

    // Preuve en base D'ABORD : aucune playlist « Butin » n'existe, donc aucune
    // piste du profil 1 n'a été recopiée.
    assert!(
        playlists_du_profil(&state, 2).is_empty(),
        "la fusion refusée a créé une playlist : {body}"
    );
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Témoin : le propriétaire fusionne sa propre playlist, et les deux pistes
    // arrivent bien dans la cible.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/merge",
        P1,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cible = body["playlist_id"].as_i64().expect("id de la fusion");
    assert_eq!(
        pistes_en_base(&state, cible),
        vec![t1, t2],
        "le témoin n'a rien recopié : le refus ne prouverait alors rien"
    );
}

// --- 3. export ---------------------------------------------------------

/// `POST /playlist-manager/export` n'avait **aucun** `ActiveProfile` : la route
/// rendait en clair le nom, les titres, les artistes et les albums de la
/// playlist désignée par l'id, quel qu'en soit le propriétaire. C'est la fuite
/// la plus large des cinq — un export est une lecture complète.
#[tokio::test]
async fn exporter_la_playlist_d_un_autre_profil_ne_rend_aucun_titre() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let corps = json!({"service": "local", "playlist_id": id.to_string(), "format": "json"});

    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/export",
        P2,
        Some(corps.clone()),
    )
    .await;

    // Ici la preuve n'est pas en base — un export ne modifie rien — mais dans
    // le CORPS rendu : c'est la donnée elle-même qui fuyait.
    let brut = body.to_string();
    assert!(
        !brut.contains("Piste Un") && !brut.contains("Piste Deux"),
        "un titre a fuité par l'export : {body}"
    );
    assert!(
        !brut.contains("Privee du profil 1"),
        "le nom de la playlist a fuité par l'export : {body}"
    );
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Témoin : le propriétaire exporte, et l'export récite bien son contenu.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/export",
        P1,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // Le corps est une chaîne JSON rendue telle quelle par la route.
    let contenu: Value = match &body {
        Value::String(s) => serde_json::from_str(s).expect("export JSON"),
        autre => autre.clone(),
    };
    assert_eq!(contenu["name"], "Privee du profil 1", "{contenu}");
    assert_eq!(contenu["track_count"], 2, "{contenu}");
    assert_eq!(contenu["tracks"][0]["title"], "Piste Un", "{contenu}");
}

// --- 4. liens de synchronisation ---------------------------------------

/// Les liens vivent dans un réglage commun au foyer (`playlist_links`) : ils
/// sont donc visibles de tous, et `POST /links/{id}/sync` écrivait des pistes
/// dans la playlist locale qu'ils désignent — celle d'un autre profil comprise.
/// Le refus est posé AVANT l'appel au service distant.
#[tokio::test]
async fn synchroniser_un_lien_vers_la_playlist_d_un_autre_profil_n_ecrit_rien() {
    let state = etat();
    let app = appli(&state);
    let (id, t1, t2) = playlist_du_profil_1(&state, &app).await;

    // Le lien est créé par son propriétaire légitime — c'est le cas réel : les
    // liens sont ensuite lisibles et énumérables par tout le foyer.
    let (st, lien) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/links",
        P1,
        Some(json!({
            "local_playlist_id": id,
            "service": "qobuz",
            "service_playlist_id": "distant-1",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{lien}");
    let lien_id = lien["id"].as_i64().expect("id du lien");

    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlist-manager/links/{lien_id}/sync"),
        P2,
        None,
    )
    .await;

    // Preuve en base D'ABORD : la playlist n'a pas bougé, et le lien n'a même
    // pas été marqué comme synchronisé.
    assert_eq!(
        pistes_en_base(&state, id),
        vec![t1, t2],
        "la synchronisation refusée a écrit dans la playlist"
    );
    let liens = reglage_json(&state, "playlist_links");
    assert_eq!(liens.len(), 1, "{liens:?}");
    assert!(
        liens[0]["last_synced_at"].is_null(),
        "le refus a quand même horodaté le lien : {liens:?}"
    );
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Témoin : pour le propriétaire, la route va jusqu'au bout — et le prouve
    // EN BASE, en horodatant le lien. Sans ce témoin, un handler qui répondrait
    // 404 à tout le monde passerait l'essai.
    let (st, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlist-manager/links/{lien_id}/sync"),
        P1,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "le propriétaire doit dépasser le contrôle d'accès : {body}"
    );
    let liens = reglage_json(&state, "playlist_links");
    assert!(
        liens[0]["last_synced_at"].is_string(),
        "le témoin n'a rien écrit : le refus ne prouverait alors rien : {liens:?}"
    );
}

/// Corollaire : on ne peut pas non plus **inscrire** un lien qui pointe sur la
/// playlist d'un autre profil. Sans ce refus, il suffirait que le propriétaire
/// déclenche ensuite la synchronisation.
#[tokio::test]
async fn creer_un_lien_vers_la_playlist_d_un_autre_profil_n_inscrit_rien() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;

    let corps = json!({
        "local_playlist_id": id,
        "service": "qobuz",
        "service_playlist_id": "distant-1",
    });

    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/links",
        P2,
        Some(corps.clone()),
    )
    .await;

    // Preuve en base D'ABORD : aucun lien inscrit dans les réglages.
    assert!(
        reglage_json(&state, "playlist_links").is_empty(),
        "un lien a été inscrit malgré le refus"
    );
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Témoin : le propriétaire crée bien le sien.
    let (st, body) = appel(
        &app,
        "POST",
        "/api/v1/playlist-manager/links",
        P1,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(reglage_json(&state, "playlist_links").len(), 1);
}

// --- 5. lecture d'une zone ---------------------------------------------

/// `POST /zones/{id}/play` accepte un `playlist_id` dans son corps. Il partait
/// d'un `get_track_ids` nu : n'importe quel profil versait les pistes de
/// n'importe quelle playlist du foyer dans la file de la zone, puis les jouait
/// — sans jamais passer par `/playlists`.
#[tokio::test]
async fn jouer_la_playlist_d_un_autre_profil_ne_remplit_aucune_file() {
    let state = etat();
    let app = appli(&state);
    let (id, _, _) = playlist_du_profil_1(&state, &app).await;
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = zones.create("Salon", Some("local"), None).expect("zone");
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone());

    let (st, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone}/play"),
        P2,
        Some(json!({"playlist_id": id})),
    )
    .await;

    // Preuve en base D'ABORD, et c'est elle qui compte : la file est remplie
    // AVANT que l'orchestrateur ne joue. Sans le cloisonnement, cette zone
    // repartait avec les deux pistes du profil 1 en file, et seul le défaut de
    // sortie audio (409) empêchait de les entendre — un refus tardif qui
    // n'aurait rien prouvé.
    assert_eq!(
        file.count_all(zone).expect("lecture file"),
        0,
        "les pistes d'un autre profil sont arrivées dans la file"
    );
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");

    // Témoin : le propriétaire demande la même chose, et la file se remplit.
    // Le statut final dépend de l'orchestrateur (aucune sortie audio dans cet
    // essai) ; ce qui est éprouvé ici, c'est que la route a bien résolu la
    // playlist au lieu de la refuser à tout le monde.
    let (st, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone}/play"),
        P1,
        Some(json!({"playlist_id": id})),
    )
    .await;
    assert_ne!(
        st,
        StatusCode::NOT_FOUND,
        "le propriétaire s'est vu opposer le même refus : le 404 est universel"
    );
    assert_eq!(
        file.count_all(zone).expect("lecture file"),
        2,
        "le témoin n'a rien mis en file : le refus ne prouverait alors rien"
    );
}
