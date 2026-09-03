//! #2798 — duplication et import M3U ne doivent plus annoncer un succès après
//! une écriture partielle.
//!
//! Les deux chemins créaient la playlist, puis jetaient l'échec de l'ajout des
//! pistes avec `.ok()` : `201 Created` pour une playlist vide, un compteur
//! `matched_tracks` qui décrivait la boucle de parsing et non la base, et un
//! objet à moitié créé laissé derrière.
//!
//! L'échec est injecté par un `DbBackend` qui délègue tout à une vraie base
//! SQLite mais refuse toute écriture dans `playlist_tracks` : déterministe,
//! sans horloge ni course, et il frappe APRÈS la création de la playlist —
//! exactement le point demandé par les critères d'acceptation.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};
use tune_core::db::engine::Engine;

// --- injection d'échec -------------------------------------------------

const REFUS: &str = "echec injecte: ecriture playlist_tracks refusee";

fn est_ecriture_de_pistes(sql: &str) -> bool {
    sql.contains("INSERT INTO playlist_tracks")
}

/// Délègue tout à `inner`, sauf l'insertion d'une piste de playlist.
struct RefuseLesPistes {
    inner: Arc<dyn DbBackend>,
}

impl DbBackend for RefuseLesPistes {
    fn engine(&self) -> Engine {
        self.inner.engine()
    }

    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        if est_ecriture_de_pistes(sql) {
            return Err(REFUS.to_string());
        }
        self.inner.execute(sql, params)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.inner.query_one(sql, params)
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.inner.query_many(sql, params)
    }

    fn query_one_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.inner.query_one_strong(sql, params)
    }

    fn query_many_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.inner.query_many_strong(sql, params)
    }

    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.inner.execute_batch(sql)
    }

    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        // La transaction reste celle du backend réel : c'est bien LUI qui
        // annule. On n'intercepte que les ordres qui la traversent.
        self.inner.write_tx(&mut |tx| {
            let filtre = TxRefusant { inner: tx };
            f(&filtre)
        })
    }
}

struct TxRefusant<'a> {
    inner: &'a dyn DbTxHandle,
}

impl DbTxHandle for TxRefusant<'_> {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        if est_ecriture_de_pistes(sql) {
            return Err(REFUS.to_string());
        }
        self.inner.execute(sql, params)
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.inner.query_one(sql, params)
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.inner.query_many(sql, params)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }
}

// --- outillage ---------------------------------------------------------

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

/// Même base, mais toute écriture dans `playlist_tracks` échoue.
fn appli_en_panne(state: &tune_server::state::AppState) -> axum::Router {
    let mut casse = state.clone();
    let reel = state.backend.clone();
    casse.backend = Arc::new(RefuseLesPistes { inner: reel });
    tune_server::routes::router(casse)
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

async fn poste_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn poste_m3u(app: &axum::Router, nom_fichier: &str, contenu: &str) -> (StatusCode, Value) {
    let b = "----tune2798";
    let corps = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{nom_fichier}\"\r\n\
         Content-Type: audio/x-mpegurl\r\n\r\n{contenu}\r\n--{b}--\r\n"
    );
    reponse(
        app,
        Request::post("/api/v1/playlists/import/m3u")
            .header("Content-Type", format!("multipart/form-data; boundary={b}"))
            .body(Body::from(corps))
            .unwrap(),
    )
    .await
}

async fn lis(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json)
}

fn nombre_de_playlists(state: &tune_server::state::AppState) -> i64 {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .count(1)
        .expect("count")
}

// --- chemin 1 : duplication -------------------------------------------

#[tokio::test]
async fn duplication_dont_les_pistes_echouent_ne_repond_pas_201() {
    let state = etat();
    let app = appli(&state);
    let t1 = piste(&state, "Piste Un", "/musique/un.flac");
    let t2 = piste(&state, "Piste Deux", "/musique/deux.flac");

    let (st, body) = poste_json(&app, "/api/v1/playlists", json!({"name": "Source"})).await;
    assert_eq!(st, StatusCode::CREATED);
    let src = body["id"].as_i64().expect("id source");
    let (st, _) = poste_json(
        &app,
        &format!("/api/v1/playlists/{src}/tracks"),
        json!({"track_ids": [t1, t2]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let avant = nombre_de_playlists(&state);

    // L'échec frappe APRÈS la création de la copie.
    let panne = appli_en_panne(&state);
    let (st, _) = poste_json(
        &panne,
        &format!("/api/v1/playlists/{src}/duplicate"),
        json!({}),
    )
    .await;

    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une duplication qui n'a copié aucune piste ne doit pas répondre 201"
    );
    assert_eq!(
        nombre_de_playlists(&state),
        avant,
        "la copie partielle est restée en base"
    );

    let (_, liste) = lis(&app, "/api/v1/playlists/all").await;
    let noms: Vec<String> = liste
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        !noms.iter().any(|n| n == "Source (copy)"),
        "playlists après échec : {noms:?}"
    );
}

#[tokio::test]
async fn duplication_reussie_annonce_le_nombre_de_pistes_persistees() {
    let state = etat();
    let app = appli(&state);
    let t1 = piste(&state, "Piste Un", "/musique/un.flac");
    let t2 = piste(&state, "Piste Deux", "/musique/deux.flac");

    let (_, body) = poste_json(&app, "/api/v1/playlists", json!({"name": "Source"})).await;
    let src = body["id"].as_i64().unwrap();
    poste_json(
        &app,
        &format!("/api/v1/playlists/{src}/tracks"),
        json!({"track_ids": [t1, t2]}),
    )
    .await;

    let (st, copie) = poste_json(
        &app,
        &format!("/api/v1/playlists/{src}/duplicate"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(copie["name"], "Source (copy)");
    assert_eq!(copie["track_count"], 2);

    let id = copie["id"].as_i64().unwrap();
    let (_, pistes) = lis(&app, &format!("/api/v1/playlists/{id}/tracks")).await;
    assert_eq!(
        pistes.as_array().unwrap().len(),
        2,
        "track_count doit décrire la base : {pistes}"
    );
}

// --- chemin 2 : import M3U --------------------------------------------

#[tokio::test]
async fn import_m3u_dont_les_pistes_echouent_ne_repond_pas_201() {
    let state = etat();
    piste(&state, "Piste Un", "/musique/un.flac");
    let avant = nombre_de_playlists(&state);

    let panne = appli_en_panne(&state);
    let (st, _) = poste_m3u(&panne, "Liste.m3u", "#EXTM3U\n/musique/un.flac\n").await;

    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "un import qui n'a persisté aucune piste ne doit pas répondre 201"
    );
    assert_eq!(
        nombre_de_playlists(&state),
        avant,
        "l'import a laissé une playlist vide derrière lui"
    );
}

/// Le compte-rendu doit dire EXACTEMENT ce qui est arrivé à chaque ligne :
/// deux pistes trouvées, une répétition écartée, une ligne absente de la
/// bibliothèque — et `imported` = ce qui est réellement en base.
#[tokio::test]
async fn import_m3u_partiel_rend_compte_ligne_par_ligne() {
    let state = etat();
    let app = appli(&state);
    piste(&state, "Piste Un", "/musique/un.flac");
    piste(&state, "Piste Deux", "/musique/deux.flac");

    let (st, body) = poste_m3u(
        &app,
        "Liste.m3u",
        "#EXTM3U\n\
         /musique/un.flac\n\
         /musique/deux.flac\n\
         /musique/un.flac\n\
         /absent/Qwertzuiop Introuvable 4242.flac\n",
    )
    .await;

    assert_eq!(st, StatusCode::CREATED, "corps : {body}");
    assert_eq!(body["total_entries"], 4, "corps : {body}");
    assert_eq!(body["matched"], 3, "corps : {body}");
    assert_eq!(body["imported"], 2, "corps : {body}");
    assert_eq!(body["duplicates_skipped"], 1, "corps : {body}");
    assert_eq!(body["not_found"], 1, "corps : {body}");
    assert_eq!(body["lookup_errors"], 0, "corps : {body}");
    assert_eq!(body["track_count"], 2, "corps : {body}");
    assert_eq!(
        body["not_found_paths"],
        json!(["/absent/Qwertzuiop Introuvable 4242.flac"]),
        "corps : {body}"
    );

    // total_entries = matched + not_found + lookup_errors : l'arithmétique du
    // compte-rendu doit fermer.
    let somme = body["matched"].as_i64().unwrap()
        + body["not_found"].as_i64().unwrap()
        + body["lookup_errors"].as_i64().unwrap();
    assert_eq!(somme, body["total_entries"].as_i64().unwrap());

    // Et la base contient bien ce qui est annoncé.
    let id = body["id"].as_i64().unwrap();
    let (_, pistes) = lis(&app, &format!("/api/v1/playlists/{id}/tracks")).await;
    assert_eq!(pistes.as_array().unwrap().len(), 2, "en base : {pistes}");
}

// --- contre-épreuves ---------------------------------------------------

/// L'injection d'échec doit vraiment mordre. Si `RefuseLesPistes` laissait
/// passer les écritures, les deux tests ci-dessus seraient verts sans rien
/// prouver — c'est le faux garde-fou qu'on cherche à éviter.
#[tokio::test]
async fn contre_epreuve_l_injection_fait_bien_echouer_un_ajout_de_pistes() {
    let state = etat();
    let app = appli(&state);
    let t1 = piste(&state, "Piste Un", "/musique/un.flac");
    // Une SECONDE piste : `add_tracks_deduped` n'insère rien pour une piste
    // déjà présente et rendrait 201 sans jamais toucher `playlist_tracks` —
    // la contre-épreuve passerait alors pour une bonne raison, ce qui est
    // exactement le faux garde-fou qu'on veut éviter.
    let t2 = piste(&state, "Piste Deux", "/musique/deux.flac");

    let (_, body) = poste_json(&app, "/api/v1/playlists", json!({"name": "Cible"})).await;
    let id = body["id"].as_i64().unwrap();

    // Sur l'application saine : 201.
    let (st, _) = poste_json(
        &app,
        &format!("/api/v1/playlists/{id}/tracks"),
        json!({"track_ids": [t1]}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Sur l'application en panne : 500, et rien d'autre n'est cassé (la
    // création de playlist, elle, passe toujours).
    let panne = appli_en_panne(&state);
    let (st, _) = poste_json(&panne, "/api/v1/playlists", json!({"name": "Passe"})).await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "l'injection ne doit refuser QUE playlist_tracks"
    );
    let (st, _) = poste_json(
        &panne,
        &format!("/api/v1/playlists/{id}/tracks"),
        json!({"track_ids": [t2]}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "l'injection d'échec ne mord pas : les tests d'atomicité ne prouveraient rien"
    );
}
