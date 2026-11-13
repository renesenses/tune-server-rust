//! #7478 — exclusions du scan PAR EMPLACEMENT : motif glob, aperçu exact,
//! rien de retiré sans confirmation, retrait au scan suivant, liste globale
//! inchangée. Vrai scan d'un dossier temporaire, SQLite (et PostgreSQL si
//! `TUNE_TEST_PG_URL`).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let (sr, bits): (u64, u64) = (44_100, 16);
    let mut out = b"fLaC".to_vec();
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let canaux: u64 = 2 - 1;
    let total: u64 = sr * 180;
    let packed: u64 = (sr << 44) | (canaux << 41) | ((bits - 1) << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut vc = Vec::new();
    let vendeur = b"banc-7478";
    vc.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendeur);
    vc.extend_from_slice(&(balises.len() as u32).to_le_bytes());
    for (k, v) in balises {
        let c = format!("{k}={v}");
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    out.push(0x80 | 0x04);
    let l = vc.len() as u32;
    out.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    out.extend_from_slice(&vc);
    let mut x = graine.wrapping_mul(2_654_435_761).wrapping_add(1);
    for _ in 0..(96 * 1024) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push(x as u8);
    }
    out
}

async fn requete(
    state: &AppState,
    methode: &str,
    route: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut r = Request::builder().method(methode).uri(route);
    let body = match corps {
        Some(c) => {
            r = r.header("Content-Type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.oneshot(r.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn scanner(state: &AppState, complet: bool) {
    let route = if complet {
        "/api/v1/system/scan?force=true"
    } else {
        "/api/v1/system/scan"
    };
    let (s, v) = requete(state, "POST", route, None).await;
    assert!(s.is_success(), "POST {route} → {s} : {v}");
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, etat) = requete(state, "GET", "/api/v1/system/scan/status", None).await;
        dernier = etat;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
}

fn chemins(db: &dyn DbBackend) -> Vec<String> {
    let mut v: Vec<String> = db
        .query_many("SELECT file_path FROM tracks", &[])
        .unwrap()
        .into_iter()
        .filter_map(|l| l.first().and_then(|v| v.as_string()))
        .collect();
    v.sort();
    v
}

fn poser(chemin: &std::path::Path, titre: &str, graine: u32) {
    std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
    let b = [
        ("TITLE", titre),
        ("ARTIST", "Banc 7478"),
        ("ALBUM", titre),
        ("TRACKNUMBER", "1"),
    ];
    std::fs::write(chemin, flac(&b, graine)).unwrap();
}

async fn jouer(state: &AppState, etiquette: &str) {
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-7478-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (nas, disque) = (base.join("nas"), base.join("disque"));
    let fichiers = [
        nas.join("Jazz/Album/01.flac"),
        nas.join("Jazz/Album/02.flac"),
        nas.join("Jazz/Incoming/03.flac"),
        nas.join("Rock/Incoming/sous/04.flac"),
        nas.join("Brouillons/05.flac"),
        disque.join("Incoming/06.flac"),
    ];
    for (i, f) in fichiers.iter().enumerate() {
        poser(f, &format!("Titre {i} {etiquette}"), i as u32 + 11);
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let (r_nas, r_disque) = (
        nas.to_string_lossy().into_owned(),
        disque.to_string_lossy().into_owned(),
    );
    settings
        .set("music_dirs", &json!([r_nas, r_disque]).to_string())
        .unwrap();
    // La liste GLOBALE d'avant : sous-chaînes, toujours appliquée.
    settings
        .set("scan_exclude_paths", &json!(["brouillons"]).to_string())
        .unwrap();
    let db = state.backend.as_ref();

    scanner(state, false).await;
    assert_eq!(
        chemins(db).len(),
        5,
        "{etiquette} : Brouillons exclu globalement : {:?}",
        chemins(db)
    );

    let (s, v) = requete(state, "GET", "/api/v1/system/scan-exclusions", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["global"], json!(["brouillons"]), "{etiquette}");
    assert_eq!(v["locations"], json!([]), "{etiquette}");

    let regles = json!([{"root": r_nas, "patterns": ["**/incoming/**"]}]);

    // ── Aperçu exact : 03 et 04 (casse ignorée, toute profondeur), pas 06.
    let (s, v) = requete(
        state,
        "POST",
        "/api/v1/system/scan-exclusions/preview",
        Some(json!({"locations": regles})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{etiquette} : {v}");
    assert_eq!(v["tracks_to_remove"], json!(2), "{etiquette} : {v}");

    // ── Motif illisible, racine inconnue : 400.
    let (s, _) = requete(
        state,
        "POST",
        "/api/v1/system/scan-exclusions/preview",
        Some(json!({"locations": [{"root": r_nas, "patterns": ["[abc"]}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{etiquette}");
    let (s, _) = requete(
        state,
        "PUT",
        "/api/v1/system/scan-exclusions",
        Some(json!({"locations": [{"root": "/ailleurs", "patterns": ["*.bak"]}], "confirm": 0})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{etiquette}");

    // ── Sans confirmation, ou avec un autre nombre : 409, rien d'écrit.
    for corps in [
        json!({"locations": regles}),
        json!({"locations": regles, "confirm": 1}),
    ] {
        let (s, v) = requete(state, "PUT", "/api/v1/system/scan-exclusions", Some(corps)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{etiquette} : {v}");
        assert_eq!(v["tracks_to_remove"], json!(2));
    }
    assert_eq!(
        settings.get("scan_exclusions_par_emplacement").unwrap(),
        None,
        "{etiquette} : rien d'écrit sans confirmation"
    );
    // La porte générique ne contourne pas la confirmation.
    let (s, _) = requete(
        state,
        "PATCH",
        "/api/v1/system/config",
        Some(json!({"scan_exclusions_par_emplacement": regles.to_string()})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{etiquette}");
    scanner(state, false).await;
    assert_eq!(
        chemins(db).len(),
        5,
        "{etiquette} : rien retiré sans confirmation"
    );

    // ── Confirmé : enregistré, retiré au scan suivant.
    let (s, v) = requete(
        state,
        "PUT",
        "/api/v1/system/scan-exclusions",
        Some(json!({"locations": regles, "confirm": 2})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{etiquette} : {v}");
    assert_eq!(v["locations"][0]["patterns"], json!(["**/incoming/**"]));
    assert_eq!(
        v["global"],
        json!(["brouillons"]),
        "{etiquette} : liste globale inchangée"
    );
    scanner(state, false).await;
    let restants = chemins(db);
    let noms: Vec<&str> = restants
        .iter()
        .map(|p| p.rsplit('/').next().unwrap())
        .collect();
    let mut tries = noms.clone();
    tries.sort();
    assert_eq!(
        tries,
        ["01.flac", "02.flac", "06.flac"],
        "{etiquette} : {restants:?}"
    );
    for f in &fichiers {
        assert!(
            f.exists(),
            "{etiquette} : un fichier a été effacé : {}",
            f.display()
        );
    }
    // Un scan complet ne réimporte pas les exclus.
    scanner(state, true).await;
    assert_eq!(chemins(db).len(), 3, "{etiquette}");
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusions_par_emplacement_apercu_confirmation_retrait_7478() {
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    jouer(&sqlite, "sqlite").await;

    #[cfg(feature = "postgres")]
    if let Ok(url) = std::env::var("TUNE_TEST_PG_URL") {
        let config = tune_server::config::TuneConfig {
            database_url: Some(url),
            ..Default::default()
        };
        let pg = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
        pg.backend
            .execute_batch(
                "TRUNCATE tracks, albums, artists RESTART IDENTITY CASCADE; \
                 DELETE FROM track_copies; \
                 DELETE FROM settings WHERE key IN ('music_dirs', 'scan_exclude_paths', 'scan_exclusions_par_emplacement')",
            )
            .expect("vider la bibliothèque PostgreSQL");
        jouer(&pg, "postgres").await;
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #7478 SAUTÉE");
}
