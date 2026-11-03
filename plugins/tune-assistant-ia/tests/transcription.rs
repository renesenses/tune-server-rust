//! La transcription vocale par le COFFRE et dans le PLAFOND (décision de
//! Bertrand du 23/10/2026), éprouvée par le vrai client HTTP contre un faux
//! service de transcription local (127.0.0.1) — aucun appel réel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::appel::ErreurAppel;
use tune_assistant_ia::fournisseur::fabrique_reelle;
use tune_assistant_ia::transcription::{
    CLE_TRANSCRIPTION_CHIFFREE, CLE_TRANSCRIPTION_URL, SourceCle, USAGE_TRANSCRIPTION,
};
use tune_assistant_ia::{Etat, Passerelle, reglages, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

const CLE_WHISPER: &str = "sk-proj-WHISPER-SECRETE-0123456789";
const CLE_ENV: &str = "sk-proj-VARIABLE-HERITEE-987654321";

/// 3 secondes de WAV 16 kHz, 16 bits, mono.
fn wav_3s() -> Vec<u8> {
    let donnees = 96_000usize;
    let mut a = Vec::new();
    a.extend_from_slice(b"RIFF");
    a.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    a.extend_from_slice(b"WAVEfmt ");
    a.extend_from_slice(&16u32.to_le_bytes());
    a.extend_from_slice(&1u16.to_le_bytes());
    a.extend_from_slice(&1u16.to_le_bytes());
    a.extend_from_slice(&16_000u32.to_le_bytes());
    a.extend_from_slice(&32_000u32.to_le_bytes());
    a.extend_from_slice(&2u16.to_le_bytes());
    a.extend_from_slice(&16u16.to_le_bytes());
    a.extend_from_slice(b"data");
    a.extend_from_slice(&(donnees as u32).to_le_bytes());
    a.extend(std::iter::repeat_n(0u8, donnees));
    a
}

struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    passerelle: Passerelle,
    recus: Arc<AtomicUsize>,
    auth: Arc<Mutex<Option<String>>>,
    _dossier: tempfile::TempDir,
}

async fn banc() -> Banc {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let dossier = tempfile::tempdir().unwrap();

    // Le faux service : il compte les requêtes et retient l'en-tête reçu.
    let recus = Arc::new(AtomicUsize::new(0));
    let auth = Arc::new(Mutex::new(None));
    let (r2, a2) = (recus.clone(), auth.clone());
    let service = axum::Router::new().route(
        "/audio/transcriptions",
        axum::routing::post(move |entetes: axum::http::HeaderMap| {
            let (r2, a2) = (r2.clone(), a2.clone());
            async move {
                r2.fetch_add(1, Ordering::SeqCst);
                *a2.lock().unwrap() = entetes
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                axum::Json(json!({"text": "kind of blue"}))
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, service).await.unwrap();
    });
    SettingsRepo::with_backend(db.clone())
        .set(CLE_TRANSCRIPTION_URL, &adresse)
        .unwrap();

    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        Vec::new(),
        fabrique_reelle(),
    );
    Banc {
        app: router_vers(etat),
        passerelle: Passerelle::new(db.clone(), dossier.path()),
        db,
        recus,
        auth,
        _dossier: dossier,
    }
}

async fn regler(b: &Banc, corps: Value) -> Value {
    let req = Request::builder()
        .method("PUT")
        .uri("/reglages")
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let rep = b.app.clone().oneshot(req).await.unwrap();
    assert_eq!(rep.status(), StatusCode::OK);
    let o = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&o).unwrap()
}

#[tokio::test]
async fn la_cle_de_transcription_vient_du_coffre_et_l_appel_est_compte() {
    let b = banc().await;
    let v = regler(
        &b,
        json!({"plafond_eur": "2", "cle_transcription": CLE_WHISPER}),
    )
    .await;
    assert_eq!(v["transcription"]["source"], "coffre", "{v}");
    assert!(
        !v.to_string().contains(CLE_WHISPER),
        "la clé part à l'écran : {v}"
    );
    // Scellée au repos : la base ne la porte pas en clair.
    let brut = SettingsRepo::with_backend(b.db.clone())
        .get(CLE_TRANSCRIPTION_CHIFFREE)
        .unwrap()
        .unwrap();
    assert!(!brut.contains(CLE_WHISPER), "clé en clair dans la base");

    let t = b
        .passerelle
        .transcrire(&wav_3s(), Some(CLE_ENV))
        .await
        .expect("transcription");
    assert_eq!(t.texte, "kind of blue");
    assert_eq!(t.source, SourceCle::Coffre);
    assert_eq!(
        b.auth.lock().unwrap().clone(),
        Some(format!("Bearer {CLE_WHISPER}")),
        "la clé du coffre doit primer sur la variable d'environnement"
    );
    // 3 s à 0,006 $/min, taux 1 : 300 micro-euros, comptés à l'usage.
    let c = reglages::charger_consommation(&b.db);
    let u = c
        .par_usage
        .get(USAGE_TRANSCRIPTION)
        .expect("la transcription n'est pas comptée dans la consommation");
    assert_eq!((u.appels, u.micro_eur), (1, 300));
    assert_eq!(c.micro_eur, 300);
}

#[tokio::test]
async fn le_plafond_refuse_la_transcription_avant_l_envoi() {
    let b = banc().await;
    // 0,0002 € de plafond : moins que les 300 micro-euros de 3 s.
    regler(
        &b,
        json!({"plafond_eur": "0.0002", "cle_transcription": CLE_WHISPER}),
    )
    .await;
    let e = b
        .passerelle
        .transcrire(&wav_3s(), None)
        .await
        .expect_err("le plafond devait refuser");
    assert_eq!(e, ErreurAppel::PlafondAtteint);
    assert_eq!(
        b.recus.load(Ordering::SeqCst),
        0,
        "l'audio est parti alors que son coût franchissait le plafond"
    );
}

#[tokio::test]
async fn la_variable_heritee_reste_un_repli_compte() {
    let b = banc().await;
    regler(&b, json!({"plafond_eur": "2"})).await;
    let t = b
        .passerelle
        .transcrire(&wav_3s(), Some(CLE_ENV))
        .await
        .expect("repli sur TUNE_OPENAI_API_KEY");
    assert_eq!(t.source, SourceCle::Environnement);
    assert_eq!(
        b.auth.lock().unwrap().clone(),
        Some(format!("Bearer {CLE_ENV}"))
    );
    assert_eq!(
        reglages::charger_consommation(&b.db).par_usage[USAGE_TRANSCRIPTION].appels,
        1,
        "le repli hérité doit compter dans le plafond lui aussi"
    );
}

#[tokio::test]
async fn sans_aucune_cle_rien_ne_part() {
    let b = banc().await;
    let e = b
        .passerelle
        .transcrire(&wav_3s(), None)
        .await
        .expect_err("sans clé");
    assert_eq!(e, ErreurAppel::NonConfigure);
    assert_eq!(b.recus.load(Ordering::SeqCst), 0);
}
