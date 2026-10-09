//! Sur PostgreSQL, la règle de pochette d'album du scan par lots paniquait à
//! chaque album : « there is no reactor running ».
//!
//! `scan_import` exécute `pochette_disque::suivre_la_piste` (qui lit le disque
//! ET écrit en base) dans le fil de `lecture_bornee::lire_avec_delai`, pour
//! borner la lecture (#5202). Ce fil brut n'avait pas de contexte tokio ; or
//! le backend PostgreSQL rejoint le runtime par `Handle::current()`. Le fil
//! paniquait, la lecture rendait « échouée », et la pochette de l'album
//! n'était ni posée ni suivie (#5682). SQLite, synchrone, ne voyait rien.
//! Relevé par la sonde du lot albums-vides (#5860), présent en rc2.
//!
//! Le banc pose un album de deux FLAC étiquetés avec sa `cover.png`, lance le
//! VRAI scan (`POST /system/scan`), puis vérifie qu'aucun fil `scan-*` n'a
//! paniqué et que la pochette d'album est posée, tirée du dossier, et suivie
//! (fichier source et empreinte renseignés).
//!
//! Doctrine du saut, reprise de `pg_scan_converge_4602.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::SourcePochette;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Paniques survenues dans un fil de lecture bornée (`scan-*`).
static PANIQUES_DE_LECTURE: AtomicUsize = AtomicUsize::new(0);

fn compter_les_paniques_de_lecture() {
    let defaut = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current()
            .name()
            .is_some_and(|n| n.starts_with("scan-"))
        {
            PANIQUES_DE_LECTURE.fetch_add(1, Ordering::SeqCst);
        }
        defaut(info);
    }));
}

/// Un FLAC minimal mais valide pour `lofty` (repris de `pg_scan_converge_4602`).
fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let sr: u64 = 44_100;
    let canaux: u64 = 2 - 1;
    let bps: u64 = 16 - 1;
    let total: u64 = 44_100 * 180;
    let packed: u64 = (sr << 44) | (canaux << 41) | (bps << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut vc = Vec::new();
    let vendeur = b"banc-pochette-pg";
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

fn pochette() -> Vec<u8> {
    let img = image::RgbImage::from_fn(32, 32, |x, y| {
        image::Rgb([(x * 7) as u8, (y * 13) as u8, ((x * y) % 256) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

async fn requete(state: &AppState, methode: &str, route: &str) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let r = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(route)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let o = axum::body::to_bytes(r.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let t = String::from_utf8_lossy(&o).into_owned();
    assert!(statut.is_success(), "{methode} {route} → {statut} : {t}");
    serde_json::from_str(&t).unwrap_or(Value::Null)
}

async fn jouer(state: &AppState, moteur: &str) {
    // Hors de `temp_dir()` : le scan y écarte tout (`is_tune_temp_file`).
    let racine = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-pochette-pg-{moteur}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&racine);
    let dossier = racine.join("Nina Simone/Pastel Blues");
    std::fs::create_dir_all(&dossier).unwrap();
    for (i, titre) in ["Be My Husband", "Nobody's Fault But Mine"]
        .iter()
        .enumerate()
    {
        let n = (i + 1).to_string();
        std::fs::write(
            dossier.join(format!("0{n}.flac")),
            flac(
                &[
                    ("TITLE", titre),
                    ("ARTIST", "Nina Simone"),
                    ("ALBUM", "Pastel Blues"),
                    ("TRACKNUMBER", &n),
                ],
                i as u32 + 1,
            ),
        )
        .unwrap();
    }
    std::fs::write(dossier.join("cover.png"), pochette()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &format!("[{}]", serde_json::json!(racine.to_string_lossy())),
        )
        .unwrap();

    let avant = PANIQUES_DE_LECTURE.load(Ordering::SeqCst);
    requete(state, "POST", "/api/v1/system/scan").await;
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        dernier = requete(state, "GET", "/api/v1/system/scan/status").await;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(
        dernier["status"], "scanning",
        "{moteur} : scan jamais terminé"
    );
    let paniques = PANIQUES_DE_LECTURE.load(Ordering::SeqCst) - avant;
    assert_eq!(
        paniques, 0,
        "{moteur} : {paniques} panique(s) dans le fil de lecture bornée pendant le scan \
         (« there is no reactor running ») — la règle de pochette n'a pas tourné"
    );

    let album_id = state
        .backend
        .query_one("SELECT id FROM albums WHERE title = 'Pastel Blues'", &[])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or_else(|| panic!("{moteur} : album « Pastel Blues » absent"));
    let etat = AlbumRepo::with_backend(state.backend.clone())
        .etat_pochette(album_id)
        .unwrap()
        .unwrap_or_else(|| panic!("{moteur} : état de pochette absent"));
    eprintln!("[{moteur}] état de pochette : {etat:?}");
    assert!(
        etat.cover_path.is_some(),
        "{moteur} : l'album n'a pas de pochette alors que son dossier porte cover.png"
    );
    assert_eq!(
        etat.source,
        Some(SourcePochette::Dossier),
        "{moteur} : la pochette d'album n'est pas tirée de l'image du dossier"
    );
    assert!(
        etat.fichier
            .as_deref()
            .is_some_and(|f| f.ends_with("cover.png")),
        "{moteur} : le fichier source de la pochette n'est pas suivi (#5682) : {:?}",
        etat.fichier
    );
    assert!(
        etat.empreinte.is_some(),
        "{moteur} : l'empreinte du fichier source n'est pas relevée (#5682)"
    );
    let _ = std::fs::remove_dir_all(&racine);
}

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_suit_la_pochette_d_album_sans_paniquer_sur_les_deux_moteurs() {
    compter_les_paniques_de_lecture();
    // Les deux moteurs l'un après l'autre : le bail de scan est global au
    // processus (`try_begin_scan`).
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    jouer(&sqlite, "sqlite").await;

    #[cfg(feature = "postgres")]
    if let Ok(url) = std::env::var("TUNE_TEST_PG_URL") {
        let config = tune_server::config::TuneConfig {
            database_url: Some(url),
            ..Default::default()
        };
        // Pas de `ok()?` : une base posée mais injoignable doit ROUGIR.
        let pg = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
        pg.backend
            .execute_batch("TRUNCATE tracks, albums, artists RESTART IDENTITY CASCADE")
            .expect("vider la bibliothèque PostgreSQL");
        jouer(&pg, "postgres").await;
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de la pochette SAUTÉE");
}
