//! #3685 — « Remplacer » dans la récupération de playlist ne remplaçait rien.
//!
//! `POST /playlists/{id}/recover/apply` ne prenait AUCUN corps (trois
//! extracteurs, pas de `Json<…>`) : la liste `{replacements}` envoyée par
//! `api.applyRecovery()` était reçue puis jetée, le handler recomptait les
//! fichiers présents et répondait 200. L'écran, en optimiste, passait la piste
//! à « disponible » — un no-op déguisé en succès.
//!
//! Ce banc éprouve le contrat corrigé :
//! - un remplacement par une piste LOCALE est ÉCRIT dans `playlist_tracks`, à
//!   la même position, et `applied` le dit ;
//! - une piste de service est REFUSÉE nommément (`rejected` + motif) : la
//!   récupération ne remplace que par une piste de la bibliothèque (depuis
//!   #4889, un titre de service entre par « Ajouter à une playlist ») ;
//! - `still_missing` est recompté APRÈS les écritures, sur la base ;
//! - un corps absent ou vide est un 400 explicite, pas un recomptage ;
//! - quand rien n'a pu être appliqué, la réponse est un 422 qui porte le même
//!   compte rendu.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// --- socle -------------------------------------------------------------

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

/// Une piste dont le fichier EXISTE (créé dans un dossier temporaire).
fn piste_presente(
    state: &tune_server::state::AppState,
    dossier: &std::path::Path,
    nom: &str,
) -> i64 {
    let chemin = dossier.join(nom);
    std::fs::write(&chemin, b"fLaC").unwrap();
    piste(state, nom, chemin.to_str().unwrap())
}

/// Une piste dont le fichier N'EXISTE PAS (disque débranché, fichier déplacé).
fn piste_manquante(
    state: &tune_server::state::AppState,
    dossier: &std::path::Path,
    nom: &str,
) -> i64 {
    let chemin = dossier.join("disparu").join(nom);
    piste(state, nom, chemin.to_str().unwrap())
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

fn pistes_de(state: &tune_server::state::AppState, playlist: i64) -> Vec<i64> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .get_track_ids(playlist)
        .unwrap()
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

async fn poste(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// Le POST tel que l'ANCIEN contrat le tolérait : aucun corps du tout.
async fn poste_sans_corps(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    reponse(app, Request::post(path).body(Body::empty()).unwrap()).await
}

async fn playlist_avec(app: &axum::Router, nom: &str, track_ids: &[i64]) -> i64 {
    let (st, v) = poste(app, "/api/v1/playlists", json!({ "name": nom })).await;
    assert_eq!(st, StatusCode::CREATED, "création de playlist: {v}");
    let id = v.get("id").and_then(Value::as_i64).expect("id de playlist");
    let (st, v) = poste(
        app,
        &format!("/api/v1/playlists/{id}/tracks"),
        json!({ "track_ids": track_ids }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "ajout des pistes: {v}");
    id
}

fn remplacement(track_id: i64, source: &str, source_id: &str) -> Value {
    json!({ "track_id": track_id, "new_source": source, "new_source_id": source_id })
}

struct Banc {
    _dossier: tempfile::TempDir,
    state: tune_server::state::AppState,
    app: axum::Router,
    presente: i64,
    manquante: i64,
    remplacante: i64,
    playlist: i64,
}

/// Une playlist de deux pistes : la première présente, la seconde manquante ;
/// et une troisième piste présente, hors playlist, qui servira de remplaçante.
async fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let state = etat();
    let app = appli(&state);
    let presente = piste_presente(&state, dossier.path(), "presente.flac");
    let manquante = piste_manquante(&state, dossier.path(), "manquante.flac");
    let remplacante = piste_presente(&state, dossier.path(), "remplacante.flac");
    let playlist = playlist_avec(&app, "À récupérer", &[presente, manquante]).await;
    Banc {
        _dossier: dossier,
        state,
        app,
        presente,
        manquante,
        remplacante,
        playlist,
    }
}

// --- le défaut ---------------------------------------------------------

/// Le cœur de #3685 : le remplacement est ÉCRIT, et le compte rendu le dit.
#[tokio::test]
async fn remplacer_par_une_piste_locale_reecrit_la_playlist() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);

    let (st, corps) = poste(
        &b.app,
        &chemin,
        json!({ "replacements": [remplacement(b.manquante, "local", &b.remplacante.to_string())] }),
    )
    .await;

    assert_eq!(st, StatusCode::OK, "corps rendu {corps}");
    assert_eq!(
        pistes_de(&b.state, b.playlist),
        vec![b.presente, b.remplacante],
        "la piste manquante doit être remplacée EN BASE, à la même position — \
         c'est le défaut #3685 : le serveur jetait la liste des remplacements"
    );
    assert_eq!(corps["applied_count"], 1, "corps rendu {corps}");
    assert_eq!(corps["applied"][0]["track_id"], b.manquante);
    assert_eq!(corps["applied"][0]["new_track_id"], b.remplacante);
    assert_eq!(corps["rejected_count"], 0, "corps rendu {corps}");
    assert_eq!(
        corps["still_missing"], 0,
        "recompté APRÈS l'écriture : plus rien ne manque — corps rendu {corps}"
    );
    assert_eq!(corps["total_tracks"], 2);
    assert!(
        corps.get("recovered").is_none(),
        "le recomptage `recovered` de l'ancien no-op ne doit plus être rendu"
    );
}

/// `recover` relu après `apply` ne fait plus réapparaître la piste
/// manquante : c'est le symptôme d'`applyAllRecovery` dans l'issue.
#[tokio::test]
async fn apres_remplacement_la_verification_ne_liste_plus_la_piste_manquante() {
    let b = banc().await;
    let apply = format!("/api/v1/playlists/{}/recover/apply", b.playlist);
    let recover = format!("/api/v1/playlists/{}/recover", b.playlist);

    let (st, avant) = poste_sans_corps(&b.app, &recover).await;
    assert_eq!(st, StatusCode::OK, "{avant}");
    assert_eq!(
        avant["unavailable"], 1,
        "état initial : une manquante — {avant}"
    );

    let (st, corps) = poste(
        &b.app,
        &apply,
        json!({ "replacements": [remplacement(b.manquante, "local", &b.remplacante.to_string())] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");

    let (st, apres) = poste_sans_corps(&b.app, &recover).await;
    assert_eq!(st, StatusCode::OK, "{apres}");
    assert_eq!(
        apres["unavailable"], 0,
        "la piste remplacée ne doit plus réapparaître comme manquante — {apres}"
    );
    assert_eq!(apres["available"], 2, "{apres}");
}

/// La récupération ne remplace une piste manquante que par une piste de la
/// bibliothèque. Depuis #4889 une playlist PEUT porter un titre de service —
/// par le geste « Ajouter à une playlist », pas par ce remplacement, qui n'a
/// ni le titre ni l'artiste à écrire : le refus reste nommé, par piste, et
/// RIEN n'est écrit.
#[tokio::test]
async fn une_piste_de_service_est_refusee_nommement_et_rien_n_est_ecrit() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);

    let (st, corps) = poste(
        &b.app,
        &chemin,
        json!({ "replacements": [remplacement(b.manquante, "qobuz", "52818331")] }),
    )
    .await;

    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "rien d'appliqué = pas un succès — corps rendu {corps}"
    );
    assert_eq!(
        pistes_de(&b.state, b.playlist),
        vec![b.presente, b.manquante],
        "rien ne doit avoir bougé en base"
    );
    assert_eq!(corps["applied_count"], 0, "{corps}");
    assert_eq!(corps["rejected_count"], 1, "{corps}");
    assert_eq!(corps["rejected"][0]["track_id"], b.manquante);
    let motif = corps["rejected"][0]["reason"].as_str().unwrap_or("");
    assert!(
        motif.contains("qobuz") && motif.contains("bibliothèque"),
        "le motif doit nommer la source et dire pourquoi — motif rendu : {motif:?}"
    );
    assert_eq!(
        corps["still_missing"], 1,
        "toujours une manquante — {corps}"
    );
}

/// Un lot mixte : ce qui peut l'être est appliqué, le reste est refusé avec
/// son motif, et le statut est 200 parce qu'au moins une écriture a eu lieu.
#[tokio::test]
async fn un_lot_mixte_rend_applied_et_rejected_separement() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);
    let hors_playlist = b.remplacante; // pas dans la playlist : refusée

    let (st, corps) = poste(
        &b.app,
        &chemin,
        json!({ "replacements": [
            remplacement(hors_playlist, "local", &b.presente.to_string()),
            remplacement(b.manquante, "local", "pas-un-nombre"),
            remplacement(b.manquante, "local", "999999"),
            remplacement(b.manquante, "local", &b.remplacante.to_string()),
        ] }),
    )
    .await;

    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(corps["applied_count"], 1, "{corps}");
    assert_eq!(corps["rejected_count"], 3, "{corps}");
    let motifs: Vec<String> = corps["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        motifs[0].contains("n'est pas dans cette playlist"),
        "{motifs:?}"
    );
    assert!(motifs[1].contains("identifiant"), "{motifs:?}");
    assert!(motifs[2].contains("n'existe pas"), "{motifs:?}");
    assert_eq!(
        pistes_de(&b.state, b.playlist),
        vec![b.presente, b.remplacante]
    );
}

/// Sans corps — exactement ce que l'ancien handler acceptait — c'est un 400
/// explicite, et rien n'est recompté ni annoncé.
#[tokio::test]
async fn sans_corps_c_est_un_400_explicite_pas_un_recomptage() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);

    let (st, corps) = poste_sans_corps(&b.app, &chemin).await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "un POST sans corps était accepté et recompté : c'est #3685 — corps rendu {corps}"
    );
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or("")
            .contains("replacements"),
        "le 400 doit dire ce qu'il attendait — {corps}"
    );
    assert!(
        corps.get("still_missing").is_none(),
        "pas de recomptage — {corps}"
    );

    let (st, corps) = poste(&b.app, &chemin, json!({ "replacements": [] })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "liste vide — {corps}");
    assert!(
        corps.get("still_missing").is_none(),
        "pas de recomptage — {corps}"
    );

    let (st, corps) = poste(&b.app, &chemin, json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "champ absent — {corps}");
}

/// Le remplaçant est déjà dans la playlist : la ligne manquante est retirée
/// plutôt que réécrite en doublon (invariant « jamais deux fois la même
/// piste », `add_tracks_deduped`).
#[tokio::test]
async fn remplacer_par_une_piste_deja_dans_la_playlist_ne_cree_pas_de_doublon() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);

    let (st, corps) = poste(
        &b.app,
        &chemin,
        json!({ "replacements": [remplacement(b.manquante, "local", &b.presente.to_string())] }),
    )
    .await;

    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(
        pistes_de(&b.state, b.playlist),
        vec![b.presente],
        "la piste manquante est retirée, la présente n'est pas dupliquée"
    );
    assert_eq!(corps["still_missing"], 0, "{corps}");
    assert_eq!(corps["total_tracks"], 1, "{corps}");
}

/// La playlist d'un AUTRE profil reste hors d'atteinte (#2794) : 404, comme
/// pour toute route qui désigne une playlist par son id.
#[tokio::test]
async fn la_playlist_d_un_autre_profil_repond_404() {
    let b = banc().await;
    let chemin = format!("/api/v1/playlists/{}/recover/apply", b.playlist);
    // Le profil visé par `X-Profile-Id` doit EXISTER, sinon l'extracteur
    // retombe sur le profil actif global et le test ne cloisonne rien.
    let voisin = tune_core::db::profile_repo::ProfileRepo::with_backend(b.state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .expect("create profile");
    assert_ne!(voisin, 1, "le voisin ne doit pas être le profil 1");

    let (st, corps) = reponse(
        &b.app,
        Request::post(&chemin)
            .header("Content-Type", "application/json")
            .header("X-Profile-Id", voisin.to_string())
            .body(Body::from(
                json!({ "replacements": [remplacement(b.manquante, "local", &b.remplacante.to_string())] })
                    .to_string(),
            ))
            .unwrap(),
    )
    .await;

    assert_eq!(st, StatusCode::NOT_FOUND, "{corps}");
    assert_eq!(
        pistes_de(&b.state, b.playlist),
        vec![b.presente, b.manquante]
    );
}
