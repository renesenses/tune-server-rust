//! Les notes de pochette (décision du 23/10/2026), par leurs routes, avec un
//! FAUX fournisseur injecté par la `Fabrique` — aucun appel réel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{Acces, Demande, ErreurFournisseur, Fournisseur, Reponse};
use tune_assistant_ia::{Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-api03-NOTES-0123456789-wxyz";
const CHEMIN: &str = "/home/bertrand/Musique/Mahler/Symphonie3/01.flac";

#[derive(Clone, Default)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    vu: Arc<Mutex<Option<(String, String, String)>>>,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        *self.vu.lock().unwrap() = Some((d.modele.clone(), d.systeme.clone(), d.message.clone()));
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: 1000,
            jetons_sortie: 100,
        })
    }
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
    db.execute_batch(&format!(
        "INSERT INTO artists (id, name, bio, bio_source, bio_license) VALUES (5, 'Gustav Mahler', 'Compositeur autrichien.', 'wikipedia', 'CC BY-SA 4.0');
         INSERT INTO albums (id, title, artist_id, genre, year) VALUES (10, 'Symphonie n° 3', 5, 'Classique', 1970);
         INSERT INTO albums (id, title, artist_id, genre, year) VALUES (11, 'Symphonie n° 3 (autre)', 5, 'Classique', 1990);
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number) VALUES (100, 'Kräftig', 10, 5, '{CHEMIN}', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number) VALUES (101, 'Kräftig', 11, 5, '/x/autre.flac', 1);
         INSERT INTO track_metadata (track_id, key, value) VALUES (100, 'work', 'Symphonie n° 3');
         INSERT INTO track_metadata (track_id, key, value) VALUES (101, 'work', 'Symphonie n° 3');
         INSERT INTO track_credits (track_id, artist_name, role) VALUES (100, 'Zubin Mehta', 'conductor');"
    ))
    .unwrap();
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
    );
    Banc {
        app: router_vers(etat),
        faux,
        _d: d,
    }
}

async fn appel(b: &Banc, methode: &str, uri: &str, corps: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = b
        .app
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let s = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (s, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn configurer(b: &Banc) {
    let (s, _) = appel(
        b,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn bonne_reponse() -> String {
    json!({
        "paragraphes": [
            {"texte": "Une symphonie dirigée par Zubin Mehta.", "sources": ["champ:title", "credits"]},
            {"texte": "Un détail inventé.", "sources": ["mon_savoir"]},
            {"texte": "Sans source."}
        ],
        "oeuvre": {"titre": "Symphonie n° 3", "mouvements": ["Kräftig"]},
        "versions_presentes": [11, 999]
    })
    .to_string()
}

#[tokio::test]
async fn seules_les_metadonnees_de_la_base_partent_et_les_notes_sont_marquees_ia() {
    let b = banc();
    configurer(&b).await;
    *b.faux.texte.lock().unwrap() = bonne_reponse();
    let (s, v) = appel(
        &b,
        "POST",
        "/albums/10/notes",
        Some(json!({"langue": "fr"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (modele, _systeme, message) = b.faux.vu.lock().unwrap().clone().unwrap();
    assert_eq!(modele, "claude-haiku-5-5", "Haiku 5.5 par défaut");
    assert!(message.contains("Zubin Mehta") && message.contains("Compositeur autrichien"));
    assert!(
        !message.contains("Musique") && !message.contains(".flac"),
        "un chemin de fichier part chez le fournisseur : {message}"
    );
    assert_eq!(v["marque"], "ia");
    let p = v["notes"]["paragraphes"].as_array().unwrap();
    assert_eq!(
        p.len(),
        1,
        "un paragraphe sans source ou à source inventée est passé : {v}"
    );
    assert_eq!(v["notes"]["versions_presentes"], json!([11]));
    assert_eq!(
        v["attributions"][0]["attribution"]["licence"],
        "CC BY-SA 4.0"
    );

    // Rangées, relues sans appel, puis effacées.
    let (s, l) = appel(&b, "GET", "/albums/10/notes", None).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(l["notes"], v["notes"]);
    assert_eq!(b.faux.appels.load(Ordering::SeqCst), 1);
    let (s, _) = appel(&b, "DELETE", "/albums/10/notes", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = appel(&b, "GET", "/albums/10/notes", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn une_reponse_sans_paragraphe_source_est_refusee_422() {
    let b = banc();
    configurer(&b).await;
    *b.faux.texte.lock().unwrap() =
        json!({"paragraphes": [{"texte": "Tout inventé.", "sources": ["ailleurs"]}]}).to_string();
    let (s, v) = appel(&b, "POST", "/albums/10/notes", None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], "aucun_paragraphe");
    let (s, _) = appel(&b, "GET", "/albums/10/notes", None).await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "une réponse refusée ne doit rien ranger"
    );
    // Le nouvel essai repart chez le fournisseur : la réponse refusée n'est
    // pas servie par le cache.
    *b.faux.texte.lock().unwrap() = bonne_reponse();
    let (s, v) = appel(&b, "POST", "/albums/10/notes", None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "la réponse refusée est restée en cache : {v}"
    );
    assert_eq!(b.faux.appels.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn le_second_avis_demande_sonnet_et_l_apercu_n_envoie_rien() {
    let b = banc();
    configurer(&b).await;
    let (s, v) = appel(
        &b,
        "POST",
        "/albums/10/notes",
        Some(json!({"apercu": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        b.faux.appels.load(Ordering::SeqCst),
        0,
        "l'aperçu a appelé le fournisseur"
    );
    assert!(v["cout_estime_eur"].as_f64().unwrap() > 0.0, "{v}");
    assert!(
        v["envoye"]["message"]
            .as_str()
            .unwrap()
            .contains("Zubin Mehta")
    );

    *b.faux.texte.lock().unwrap() = bonne_reponse();
    let (s, v) = appel(
        &b,
        "POST",
        "/albums/10/notes",
        Some(json!({"second_avis": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        b.faux.vu.lock().unwrap().clone().unwrap().0,
        "claude-sonnet-5-5"
    );
    assert_eq!(v["modele"], "claude-sonnet-5-5");
}

#[tokio::test]
async fn sans_cle_rien_ne_part() {
    let b = banc();
    let (s, v) = appel(&b, "POST", "/albums/10/notes", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "non_configure");
    assert_eq!(b.faux.appels.load(Ordering::SeqCst), 0);
}
