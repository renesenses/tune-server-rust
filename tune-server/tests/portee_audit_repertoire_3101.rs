//! Portée de répertoire de l'audit de bibliothèque (#3101).
//!
//! `GET /export/library-audit.csv?dir=…` est le seul point d'entrée du serveur
//! où l'utilisateur SÉLECTIONNE UN RÉPERTOIRE et attend en retour ce que ce
//! répertoire contient. Le contrat de portée du dépôt tient en deux moitiés
//! inséparables — `folder_like_pattern` pour la valeur, `like_escape_clause`
//! pour la clause. Ce site en avait reconstruit une à la main et laissé tomber
//! le reste : le motif valait `<dir>%` au lieu de `<dir>/%`.
//!
//! Conséquence, celle du ticket : auditer `…/Rock` ramenait aussi les pistes de
//! `…/Rockabilly`. Le disque n'étant parcouru que sous `…/Rock`, tout le
//! voisinage ressortait classé « fantôme » — c'est-à-dire proposé à la
//! suppression. Un filtre qui ne filtre pas rend PLUS que demandé, et l'écran
//! affiche le reste de la bibliothèque là où l'utilisateur avait choisi UN
//! répertoire.
//!
//! Les épreuves appellent la ROUTE, pas le constructeur de motif : c'est le
//! site d'appel qui avait dérivé, pas `folder_like_pattern`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// Une bibliothèque à deux répertoires VOISINS dont l'un est le préfixe de
/// l'autre — le cas que le motif sans séparateur confond.
///
/// Rend le dossier temporaire (à garder vivant), le routeur, et le chemin
/// absolu de la racine.
fn bibliotheque_voisine() -> (tempfile::TempDir, axum::Router, String) {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let racine = tmp.path().to_string_lossy().to_string();

    for (dossier, fichier) in [("Rock", "a.flac"), ("Rockabilly", "b.flac")] {
        let d = tmp.path().join(dossier);
        std::fs::create_dir_all(&d).expect("création du sous-dossier");
        std::fs::write(d.join(fichier), b"").expect("fichier témoin");
    }

    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
        .expect("racines musique");
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO tracks (id, title, file_path, source) \
               VALUES (10, 'Rock', '{r}/Rock/a.flac', 'local'); \
             INSERT INTO tracks (id, title, file_path, source) \
               VALUES (11, 'Rockabilly', '{r}/Rockabilly/b.flac', 'local');",
            r = racine
        ))
        .expect("pistes témoins");

    let app = tune_server::routes::router(state);
    (tmp, app, racine)
}

async fn audit(app: &axum::Router, dir: &str) -> (StatusCode, String) {
    let uri = format!(
        "/api/v1/export/library-audit.csv?dir={}",
        urlencoding_minimal(dir)
    );
    let resp = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .expect("la route doit répondre");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("corps de la réponse");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Encodage minimal des seuls caractères qu'un chemin temporaire peut porter et
/// qu'une chaîne de requête interprète. Pas de dépendance de plus pour trois
/// substitutions.
fn urlencoding_minimal(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '+' => "%2B".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            autre => autre.to_string(),
        })
        .collect()
}

/// 🔴 CONTRE-ÉPREUVE #3101 — auditer `…/Rock` ne ramène pas `…/Rockabilly`.
///
/// Sans le séparateur, le motif `<dir>%` avale le voisin dont le nom commence
/// par le même préfixe. Le disque n'ayant été parcouru que sous `…/Rock`, la
/// piste du voisin est déclarée « fantôme » : l'audit propose de supprimer une
/// piste parfaitement présente, dans un répertoire que l'utilisateur n'a pas
/// choisi.
#[tokio::test]
async fn l_audit_d_un_repertoire_ne_ramene_pas_le_repertoire_voisin() {
    let (_tmp, app, racine) = bibliotheque_voisine();

    let (status, csv) = audit(&app, &format!("{racine}/Rock")).await;
    assert_eq!(status, StatusCode::OK, "l'audit doit répondre 200");

    assert!(
        !csv.contains("Rockabilly"),
        "la portée `…/Rock` ne doit rien dire de `…/Rockabilly` ; CSV rendu :\n{csv}"
    );
    assert!(
        !csv.contains("b.flac"),
        "la piste du répertoire voisin n'a rien à faire dans cet audit ; CSV rendu :\n{csv}"
    );
}

/// 🟢 TÉMOIN #3101 — la portée demandée rend BIEN son propre contenu.
///
/// L'autre moitié de la contre-épreuve : refuser le voisin ne doit pas revenir
/// à ne plus rien rendre. Ce témoin est VERT avant comme après le correctif —
/// une garde qui rougirait ici prouverait qu'elle a coupé trop large.
#[tokio::test]
async fn temoin_l_audit_d_un_repertoire_liste_bien_ses_propres_pistes() {
    let (_tmp, app, racine) = bibliotheque_voisine();

    let (status, csv) = audit(&app, &format!("{racine}/Rock")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        csv.contains("a.flac"),
        "la piste du répertoire demandé doit être auditée ; CSV rendu :\n{csv}"
    );
}

/// 🟢 TÉMOIN #3101 — sans `dir`, l'audit porte sur TOUTES les racines.
///
/// La portée est facultative : la resserrer ne doit pas amputer l'audit
/// complet, qui reste le comportement par défaut de la route.
#[tokio::test]
async fn temoin_l_audit_sans_portee_couvre_les_deux_repertoires() {
    let (_tmp, app, _racine) = bibliotheque_voisine();

    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/export/library-audit.csv")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("la route doit répondre");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("corps");
    let csv = String::from_utf8_lossy(&bytes).to_string();

    assert!(csv.contains("a.flac"), "CSV rendu :\n{csv}");
    assert!(csv.contains("b.flac"), "CSV rendu :\n{csv}");
}
