//! Étape 3 de l'Assistant IA (suivi de consommation par usage, alerte à
//! 80 % du plafond, tarifs datés), par ses ROUTES, avec un FAUX fournisseur —
//! aucun appel réseau.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{
    Acces, Demande, ErreurFournisseur, Fabrique, Fournisseur, Reponse,
};
use tune_assistant_ia::{CritereMoteur, Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_http_types::AuthUser;

const CLE: &str = "sk-ant-api03-ETAPE-DEUX-0123456789-wxyz";

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

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Karajan');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (10, 'Beethoven 9', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (11, 'Brahms 1', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (12, 'Mahler 5', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (13, 'Bruckner 7', 1, 'Classical');
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number)
                VALUES (101, 'Allegro ma non troppo', 10, 1, '/m/101.flac', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number)
                VALUES (102, 'Molto vivace', 10, 1, '/m/102.flac', 2);",
    )
    .unwrap();
    Arc::new(db)
}

/// Le faux fournisseur : il compte ses appels, retient la dernière demande
/// et rend un texte fixé.
#[derive(Clone)]
#[allow(dead_code)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    json_demande: Arc<Mutex<Option<bool>>>,
    jetons_entree: u64,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        *self.json_demande.lock().unwrap() = Some(d.json);
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: self.jetons_entree,
            jetons_sortie: 0,
        })
    }
}

impl Faux {
    fn new(jetons_entree: u64) -> Self {
        Self {
            appels: Arc::new(AtomicUsize::new(0)),
            texte: Arc::new(Mutex::new(String::new())),
            json_demande: Arc::new(Mutex::new(None)),
            jetons_entree,
        }
    }
    fn fabrique(&self) -> Fabrique {
        let f = self.clone();
        Arc::new(move |_a: Acces| -> Box<dyn Fournisseur> { Box::new(f.clone()) })
    }
    fn repondre(&self, v: Value) {
        *self.texte.lock().unwrap() = v.to_string();
    }
    fn appels(&self) -> usize {
        self.appels.load(Ordering::SeqCst)
    }
}

#[allow(dead_code)]
struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    faux: Faux,
    _dossier: tempfile::TempDir,
}

/// 1 M de jetons d'entrée sur Haiku 5.5 coûtent 0,10 $ = 0,10 € (taux 1).
const UN_MILLION: u64 = 1_000_000;

fn banc(jetons_entree: u64) -> Banc {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let faux = Faux::new(jetons_entree);
    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        criteres(),
        faux.fabrique(),
    );
    Banc {
        app: router_vers(etat),
        db,
        faux,
        _dossier: dossier,
    }
}

async fn appel_en(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
    qui: Option<AuthUser>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    if let Some(u) = qui {
        req = req.extension(u);
    }
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    appel_en(app, methode, uri, corps, None).await
}

async fn configurer(b: &Banc, plafond: &str) {
    let (s, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": plafond})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

fn suggestion_de_genre() -> Value {
    json!({"suggestions": [
        {"field": "subgenre", "value": "Symphony", "confidence": 0.9, "reason": "r"}
    ]})
}

#[tokio::test]
async fn la_consommation_se_suit_par_usage_et_l_alerte_part_a_80_pour_cent() {
    let b = banc(UN_MILLION);
    // Chaque appel coûte 0,10 € ; plafond 0,25 € : alerte à 0,20 €.
    configurer(&b, "0,25").await;
    b.faux.repondre(suggestion_de_genre());

    let (s, v) = appel(&b.app, "POST", "/albums/10/suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plafond"]["alerte"], false, "{v}");
    assert!(
        (v["plafond"]["part"].as_f64().unwrap() - 0.4).abs() < 1e-9,
        "{v}"
    );

    // Un autre usage : « Tester la clé » franchit le seuil.
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plafond"]["alerte"], true, "{v}");
    assert_eq!(v["plafond"]["atteint"], false, "{v}");
    assert_eq!(v["plafond"]["seuil_alerte"], 80);

    // La même question : servie par le cache, comptée pour son usage.
    let (s, v) = appel(&b.app, "POST", "/albums/10/suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["depuis_cache"], true);
    assert_eq!(b.faux.appels(), 2);

    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    let c = &r["consommation"];
    assert_eq!(c["appels"], 2, "{r}");
    assert_eq!(c["alerte"], true, "{r}");
    assert_eq!(c["plafond_atteint"], false, "{r}");
    assert!(
        (c["part_du_plafond"].as_f64().unwrap() - 0.8).abs() < 1e-9,
        "{r}"
    );
    let album = &c["par_usage"]["album"];
    assert_eq!(album["appels"], 1, "{r}");
    assert_eq!(album["depuis_cache"], 1, "{r}");
    assert_eq!(album["jetons_entree"], UN_MILLION, "{r}");
    assert!(
        (album["cout_eur"].as_f64().unwrap() - 0.1).abs() < 1e-9,
        "{r}"
    );
    assert_eq!(c["par_usage"]["test"]["appels"], 1, "{r}");
    // Le détail par usage refait le total.
    let somme: f64 = c["par_usage"]
        .as_object()
        .unwrap()
        .values()
        .map(|u| u["cout_eur"].as_f64().unwrap())
        .sum();
    assert!(
        (somme - c["cout_eur"].as_f64().unwrap()).abs() < 1e-9,
        "{r}"
    );
}

#[tokio::test]
async fn un_lot_rend_aussi_l_etat_du_plafond() {
    let b = banc(UN_MILLION);
    configurer(&b, "0,25").await;
    b.faux.repondre(suggestion_de_genre());
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/suggestions",
        Some(json!({"album_ids": [10, 11]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plafond"]["alerte"], true, "0,20 € sur 0,25 € : {v}");
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["consommation"]["par_usage"]["album"]["appels"], 2, "{r}");
}

#[tokio::test]
async fn les_reglages_montrent_le_tarif_date_et_sa_source() {
    let b = banc(0);
    configurer(&b, "2").await;
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["tarif"]["origine"], "table", "{r}");
    assert_eq!(r["tarif"]["releve_le"], "2026-10-12", "{r}");
    assert_eq!(r["consommation"]["alerte"], false, "{r}");

    let (s, r) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "gemini"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["tarif"]["origine"], "table", "{r}");
    assert_eq!(r["tarif"]["a_relever_avant"], "2027-01-01", "{r}");
    assert_eq!(
        r["tarif"]["source_url"],
        "https://ai.google.dev/gemini-api/docs/pricing"
    );

    let (_, r) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"prix_entree_usd_mtok": "1", "prix_sortie_usd_mtok": "2"})),
    )
    .await;
    assert_eq!(r["tarif"]["origine"], "saisi", "{r}");
    assert_eq!(r["tarif"]["releve_le"], Value::Null, "{r}");
}

#[tokio::test]
async fn un_plafond_nul_est_en_alerte_d_emblee() {
    let b = banc(UN_MILLION);
    configurer(&b, "0").await;
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["consommation"]["alerte"], true, "{r}");
    assert_eq!(r["consommation"]["plafond_atteint"], true, "{r}");
    let (s, v) = appel(&b.app, "POST", "/albums/10/suggestions", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(b.faux.appels(), 0);
}
