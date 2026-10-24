//! Le diagnostic en lecture seule (décisions du 23/10/2026), par ses routes,
//! avec une FAUSSE collecte d'hôte et un FAUX fournisseur injecté par la
//! `Fabrique` — aucun appel réel.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::diagnostic::{Collecte, ContexteBrut};
use tune_assistant_ia::fournisseur::{Acces, Demande, ErreurFournisseur, Fournisseur, Reponse};
use tune_assistant_ia::{Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-api03-DIAG-0123456789-wxyz";
/// Ce qui ne doit JAMAIS partir chez le fournisseur.
const SENSIBLES: &[&str] = &[
    "192.168.1.42",
    "fe80::1c2b:3aff:fe4d:5e6f",
    "Naim Uniti Atom",
    "Salon du bas",
    "So What",
    "Miles Davis",
    "nas-maison.local",
    "00:1A:2B:3C:4D:5E",
    "/home/bertrand/Musique",
];

#[derive(Clone, Default)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    message: Arc<Mutex<String>>,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        *self.message.lock().unwrap() = d.message.clone();
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: 2000,
            jetons_sortie: 300,
        })
    }
}

fn collecte() -> Collecte {
    Arc::new(|_zone: Option<i64>| {
        Box::pin(async {
            let mut noms = BTreeMap::new();
            noms.insert("zone".to_string(), vec!["Salon du bas".to_string()]);
            noms.insert("appareil".to_string(), vec!["Naim Uniti Atom".to_string()]);
            noms.insert("piste".to_string(), vec!["So What".to_string()]);
            noms.insert("artiste".to_string(), vec!["Miles Davis".to_string()]);
            Ok(ContexteBrut {
                zone: Some(json!({
                    "id": 3, "name": "Salon du bas", "state": "stopped",
                    "output_type": "dlna", "output_device_id": "uuid:4d696e69-444c-164e-9d41-b827eb54e2a1",
                    "renderer_host": "192.168.1.42", "signal_path": null,
                    "current_track": {"title": "So What", "artist": "Miles Davis"}
                })),
                journal: vec![
                    "2026-10-23T10:00:00Z INFO scan: 12 albums".into(),
                    "2026-10-23T10:01:00Z WARN dlna: Naim Uniti Atom (192.168.1.42, fe80::1c2b:3aff:fe4d:5e6f) ne répond pas, mac 00:1A:2B:3C:4D:5E".into(),
                    "2026-10-23T10:02:00Z ERROR ssdp_startup_zone_hidden_skipping zone=\"Salon du bas\" hote=nas-maison.local".into(),
                    "2026-10-23T10:03:00Z WARN lecture: title=\"So What\" open /home/bertrand/Musique/Miles Davis/Kind of Blue/01.flac".into(),
                ],
                systeme: json!({"version": "1.0.0-rc8", "os": "linux"}),
                noms,
            })
        })
    })
}

struct Banc {
    app: axum::Router,
    faux: Faux,
    _d: tempfile::TempDir,
}

fn banc() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let d = tempfile::tempdir().unwrap();
    let faux = Faux::default();
    let f2 = faux.clone();
    let etat = Etat::new(
        db,
        d.path(),
        d.path().join("cache"),
        Vec::new(),
        Arc::new(move |_a: Acces| -> Box<dyn Fournisseur> { Box::new(f2.clone()) }),
    )
    .avec_diagnostic(Some(collecte()));
    Banc {
        app: router_vers(etat),
        faux,
        _d: d,
    }
}

async fn appel(b: &Banc, methode: &str, uri: &str, corps: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let rep = b.app.clone().oneshot(req).await.unwrap();
    let s = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), 1 << 22)
        .await
        .unwrap();
    (s, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn configurer(b: &Banc, plafond: &str) {
    let (s, _) = appel(
        b,
        "PUT",
        "/reglages",
        json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": plafond}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn bonne_reponse() -> String {
    json!({
        "constat": "La zone <zone-1> est masquée au démarrage.",
        "causes_probables": [
            {"cause": "Zone masquée", "indices": ["ssdp_startup_zone_hidden_skipping"], "confiance": "elevee"},
            {"cause": "Câble débranché", "indices": ["le câble est débranché"]}
        ],
        "a_essayer": [{"geste": "Rendre <zone-1> visible", "ou": "Réglages ▸ Zones"}],
        "rapport_utile": true,
        "resume_pour_le_rapport": "La zone <zone-1> est masquée (ssdp_startup_zone_hidden_skipping)."
    })
    .to_string()
}

#[tokio::test]
async fn rien_de_sensible_ne_part_et_l_envoi_est_l_apercu_montre() {
    let b = banc();
    configurer(&b, "5").await;
    let (s, a) = appel(
        &b,
        "POST",
        "/diagnostic/apercu",
        json!({"zone_id": 3, "question": "Pourquoi Salon du bas ne joue pas ?"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(
        b.faux.appels.load(Ordering::SeqCst),
        0,
        "l'aperçu a appelé le fournisseur"
    );
    let montre = a["envoye"]["message"].as_str().unwrap().to_string();
    for interdit in SENSIBLES {
        assert!(
            !montre.contains(interdit),
            "« {interdit} » partirait chez le fournisseur :\n{montre}"
        );
    }
    assert!(
        montre.contains("chemin_statique"),
        "zone arrêtée : l'équivalent statique manque"
    );
    assert!(montre.contains("ssdp_startup_zone_hidden_skipping"));
    assert!(
        !montre.contains("INFO scan"),
        "une ligne INFO sans rapport est partie"
    );
    assert!(a["cout_estime_eur"].as_f64().unwrap() > 0.0, "{a}");

    *b.faux.texte.lock().unwrap() = bonne_reponse();
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        *b.faux.message.lock().unwrap(),
        montre,
        "le texte envoyé n'est pas EXACTEMENT celui de l'aperçu"
    );
    // Les vrais noms rétablis pour l'écran ; la cause sans indice vérifié écartée.
    assert!(
        v["constat"].as_str().unwrap().contains("Salon du bas"),
        "{v}"
    );
    assert_eq!(v["causes_probables"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(
        v["actions"],
        json!([]),
        "le diagnostic ne fait AUCUNE action"
    );
    assert_eq!(v["marque"], "ia");
    assert!(
        !v["resume_pour_le_rapport"]
            .as_str()
            .unwrap()
            .contains("Salon du bas"),
        "le résumé destiné au forum doit rester pseudonymisé"
    );
}

#[tokio::test]
async fn chaque_envoi_exige_son_apercu() {
    let b = banc();
    configurer(&b, "5").await;
    *b.faux.texte.lock().unwrap() = bonne_reponse();
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": "jamais-montre"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "apercu_requis");
    let (_, a) = appel(&b, "POST", "/diagnostic/apercu", json!({"zone_id": 3})).await;
    let (s, _) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "un aperçu a servi deux fois : {v}");
    assert_eq!(b.faux.appels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn schema_refuse_plafond_et_cle_absente() {
    let b = banc();
    // Sans clé : l'aperçu se montre, l'envoi est refusé, rien ne part.
    let (_, a) = appel(&b, "POST", "/diagnostic/apercu", json!({"zone_id": 3})).await;
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "non_configure");

    configurer(&b, "5").await;
    *b.faux.texte.lock().unwrap() = json!({"causes_probables": []}).to_string();
    let (_, a) = appel(&b, "POST", "/diagnostic/apercu", json!({"zone_id": 3})).await;
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "réponse hors schéma acceptée : {v}"
    );

    configurer(&b, "0").await;
    let (_, a) = appel(&b, "POST", "/diagnostic/apercu", json!({"zone_id": 3})).await;
    let n = b.faux.appels.load(Ordering::SeqCst);
    let (s, v) = appel(
        &b,
        "POST",
        "/diagnostic",
        json!({"apercu_id": a["apercu_id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "plafond_atteint");
    assert_eq!(
        b.faux.appels.load(Ordering::SeqCst),
        n,
        "un appel est parti plafond atteint"
    );
}
