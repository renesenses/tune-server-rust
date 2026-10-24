//! #6839 — l'assistant et les collections intelligentes, par leurs routes,
//! avec un FAUX fournisseur injecté par la `Fabrique` : aucun appel réel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{Acces, Demande, ErreurFournisseur, Fournisseur, Reponse};
use tune_assistant_ia::{CritereMoteur, Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-api03-COLL-0123456789-wxyz";
const TITRE_PRIVE: &str = "Album Tres Prive Du Salon";

fn criteres() -> Vec<CritereMoteur> {
    tune_smart_http::criteres::criteres_exposes()
        .into_iter()
        .map(|c| CritereMoteur {
            collection: c.collection.map(String::from),
            piste: c.piste.map(String::from),
            famille: c.famille.to_string(),
            operateurs: c.operateurs.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

#[derive(Clone, Default)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    message: Arc<Mutex<String>>,
    effort: Arc<Mutex<Option<&'static str>>>,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        *self.message.lock().unwrap() = d.message.clone();
        *self.effort.lock().unwrap() = d.effort;
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: 500,
            jetons_sortie: 100,
        })
    }
}

struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    faux: Faux,
    licence: Arc<tune_core::license::LicenseManager>,
    _d: tempfile::TempDir,
}

fn banc() -> Banc {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&sqlite).unwrap();
    sqlite
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (2, 'Miles Davis');
             INSERT INTO albums (id, title, artist_id, genre, year, label) VALUES (2, '{TITRE_PRIVE}', 2, 'Jazz', 1959, 'Columbia');
             INSERT INTO albums (id, title, artist_id, genre, year) VALUES (3, 'Best of Jazz', 2, 'Jazz', 1990);
             INSERT INTO tags (id, name) VALUES (7, 'Vinyle');"
        ))
        .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let d = tempfile::tempdir().unwrap();
    let faux = Faux::default();
    let f2 = faux.clone();
    let licence = Arc::new(tune_core::license::LicenseManager::new(db.clone()));
    let etat = Etat::new(
        db.clone(),
        d.path(),
        d.path().join("cache"),
        criteres(),
        Arc::new(move |_a: Acces| -> Box<dyn Fournisseur> { Box::new(f2.clone()) }),
    )
    .avec_licence(Some(licence.clone()));
    Banc {
        app: router_vers(etat),
        db,
        faux,
        licence,
        _d: d,
    }
}

async fn appel(b: &Banc, uri: &str, corps: Value) -> (StatusCode, Value) {
    let methode = if uri == "/reglages" { "PUT" } else { "POST" };
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let rep = b.app.clone().oneshot(req).await.unwrap();
    let s = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (s, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn configurer(b: &Banc) {
    let (s, _) = appel(
        b,
        "/reglages",
        json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn repondre(b: &Banc, v: Value) {
    *b.faux.texte.lock().unwrap() = v.to_string();
}

fn nombre_de_collections(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one("SELECT COUNT(*) FROM smart_collections", &[])
        .unwrap()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        .unwrap()
}

#[tokio::test]
async fn proposer_n_envoie_que_des_statistiques_et_valide_chaque_collection() {
    let b = banc();
    configurer(&b).await;
    repondre(
        &b,
        json!({"collections": [
            {"name": "Jazz de Columbia", "description": "Le label historique", "match_mode": "all",
             "rules": [{"field": "label", "op": "=", "value": "Columbia"}]},
            {"name": "Ambiance", "rules": [{"field": "mood", "op": "=", "value": "calme"}]},
            {"name": "Vide", "rules": []}
        ]}),
    );
    let avant = nombre_de_collections(&b.db);
    let (s, v) = appel(&b, "/collections/proposer", json!({"langue": "fr"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let message = b.faux.message.lock().unwrap().clone();
    assert!(
        message.contains("Columbia") && message.contains("Jazz"),
        "{message}"
    );
    assert!(
        !message.contains(TITRE_PRIVE) && !message.contains("Best of Jazz"),
        "un titre d'album est parti chez le fournisseur : {message}"
    );
    let c = v["collections"].as_array().unwrap();
    assert_eq!(
        c.len(),
        1,
        "une collection sans règle exploitable est passée : {v}"
    );
    assert_eq!(c[0]["marque"], "ia");
    assert_eq!(v["ecartees"].as_array().unwrap().len(), 2);
    assert_eq!(
        nombre_de_collections(&b.db),
        avant,
        "rien n'est créé sans geste"
    );

    repondre(&b, json!({"collections": [{"name": "X", "rules": []}]}));
    let (s, v) = appel(&b, "/collections/proposer", json!({"langue": "en"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], "aucune_collection");
}

#[tokio::test]
async fn affiner_rend_les_regles_modifiees_et_l_ecart_sans_rien_enregistrer() {
    let b = banc();
    configurer(&b).await;
    repondre(
        &b,
        json!({"name": "Jazz", "match_mode": "all", "rules": [
            {"field": "genre", "op": "contains", "value": "Jazz"},
            {"field": "title", "op": "not_contains", "value": "Best of"}
        ]}),
    );
    let actuelle = json!({"name": "Jazz", "match_mode": "all", "rules": [
        {"field": "genre", "op": "contains", "value": "Jazz"},
        {"field": "tag", "op": "is", "value": "7"}
    ]});
    let (s, v) = appel(
        &b,
        "/collections/affiner",
        json!({"actuelle": actuelle, "phrase": "enlève les compilations best of"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let message = b.faux.message.lock().unwrap().clone();
    assert!(
        message.contains("Vinyle"),
        "l'étiquette doit partir par son NOM, pas son identifiant : {message}"
    );
    assert_eq!(v["ecart"]["ajoutees"].as_array().unwrap().len(), 1);
    assert_eq!(v["ecart"]["retirees"].as_array().unwrap().len(), 1);
    assert_eq!(v["ecart"]["inchangees"], 1);
    assert_eq!(
        *b.faux.effort.lock().unwrap(),
        Some("low"),
        "affiner est un usage court"
    );

    // Une réponse qui retire tout : refusée, comme #6731.
    repondre(
        &b,
        json!({"name": "Jazz", "rules": [{"field": "mood", "op": "=", "value": "x"}]}),
    );
    let (s, v) = appel(
        &b,
        "/collections/affiner",
        json!({"actuelle": actuelle, "phrase": "autre chose"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], "aucune_regle");
}

#[tokio::test]
async fn nommer_et_pourquoi_valident_leur_schema() {
    let b = banc();
    configurer(&b).await;
    let rules = json!([{"field": "genre", "op": "contains", "value": "Jazz"}]);
    repondre(
        &b,
        json!({"name": "Jazz pour tous", "description": "Tout le jazz"}),
    );
    let (s, v) = appel(&b, "/collections/nommer", json!({"rules": rules})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["name"].as_str(), v["marque"].as_str()),
        (Some("Jazz pour tous"), Some("ia"))
    );

    repondre(
        &b,
        json!({"explication": "Le genre est Jazz.", "regles": [3]}),
    );
    let (s, v) = appel(
        &b,
        "/collections/pourquoi",
        json!({"rules": rules, "album_id": 2}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "règle inexistante acceptée : {v}"
    );
    assert_eq!(v["error"], "regle_inconnue");

    repondre(
        &b,
        json!({"explication": "Le genre est Jazz.", "regles": [0]}),
    );
    let (s, v) = appel(
        &b,
        "/collections/pourquoi",
        json!({"rules": rules, "album_id": 2}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["regles"], json!([0]));
}

#[tokio::test]
async fn l_apercu_n_envoie_rien_et_estime_le_cout() {
    let b = banc();
    configurer(&b).await;
    let (s, v) = appel(&b, "/collections/proposer", json!({"apercu": true})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["apercu"], true);
    assert!(v["cout_estime_eur"].as_f64().unwrap() > 0.0, "{v}");
    assert_eq!(
        b.faux.appels.load(Ordering::SeqCst),
        0,
        "l'aperçu a appelé le fournisseur"
    );
}

#[tokio::test]
async fn les_collections_d_ecoute_sont_premium() {
    let b = banc();
    configurer(&b).await;
    repondre(
        &b,
        json!({"collections": [{"name": "À redécouvrir", "rules": [
            {"field": "play_count", "op": ">=", "value": 3},
            {"field": "last_played_at", "op": "<", "value": "now-365d"}
        ]}]}),
    );
    b.licence.set_account_premium(false, None).await;
    let (s, v) = appel(&b, "/collections/ecoute", json!({})).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "sans Premium : {v}");
    assert_eq!(v["error"], "premium_requis");
    assert_eq!(
        b.faux.appels.load(Ordering::SeqCst),
        0,
        "un appel est parti sans Premium"
    );

    b.licence.set_account_premium(true, None).await;
    let (s, v) = appel(&b, "/collections/ecoute", json!({"profil_id": 1})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["collections"].as_array().unwrap().len(), 1);
}
