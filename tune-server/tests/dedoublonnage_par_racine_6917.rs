//! #6917 (fil 2397, Tades) — ne dédoublonner que les copies d'un même
//! support, par le VRAI scan et les VRAIES routes.
//!
//! Deux racines de musique, `disque` et `nas`, portent le même album, octet
//! pour octet. Le banc règle l'option par `PATCH /system/config`, lance
//! `POST /system/scan` et `POST /system/cleanup`, et compte les entrées.
//!
//! Témoins, dans l'ordre :
//!
//! 0. option ALLUMÉE dès le premier scan : les deux copies sont deux entrées
//!    (6 pistes, aucun exemplaire) ;
//! 1. option ÉTEINTE (le défaut) : « Nettoyage du serveur » retire la copie
//!    de l'autre racine, et le scan suivant en refait un exemplaire — le
//!    comportement d'avant ;
//! 2. option rallumée, analyse complète : les exemplaires de l'autre racine
//!    redeviennent des pistes ; le nettoyage les GARDE ;
//! 3. option éteinte : le nettoyage les retire de nouveau.
//!
//! À chaque étape, aucun fichier n'est effacé.
//!
//! Doctrine du saut, reprise de `exemplaires_par_repertoire_4907.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::dedoublonnage_par_racine::CLE;
use tune_server::state::AppState;

/// Un FLAC minimal valide pour `lofty` (même fabrique que le banc #4907).
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
    let vendeur = b"banc-6917";
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

const DOSSIER: &str = "Miles Davis/Kind of Blue";
const TITRES: [&str; 3] = ["So What", "Freddie Freeloader", "Blue in Green"];

/// L'album sous `racine`, graines fixes : deux racines posées ainsi portent
/// des fichiers OCTET POUR OCTET identiques. Rend les chemins.
fn poser_album(racine: &std::path::Path) -> Vec<std::path::PathBuf> {
    let d = racine.join(DOSSIER);
    std::fs::create_dir_all(&d).unwrap();
    (0..3)
        .map(|i| {
            let numero = (i + 1).to_string();
            let b = [
                ("TITLE", TITRES[i]),
                ("ARTIST", "Miles Davis"),
                ("ALBUM", "Kind of Blue"),
                ("TRACKNUMBER", numero.as_str()),
                ("DATE", "1959"),
            ];
            let chemin = d.join(format!("0{}.flac", i + 1));
            std::fs::write(&chemin, flac(&b, i as u32 + 1)).unwrap();
            chemin
        })
        .collect()
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

fn compte(db: &dyn DbBackend, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or(-1)
}

/// (lignes `tracks`, exemplaires `track_copies`).
fn entrees(db: &dyn DbBackend) -> (i64, i64) {
    (
        compte(db, "SELECT COUNT(*) FROM tracks"),
        compte(db, "SELECT COUNT(*) FROM track_copies"),
    )
}

fn pistes_sous(db: &dyn DbBackend, racine: &std::path::Path) -> i64 {
    let r = racine.to_string_lossy();
    db.query_many("SELECT file_path FROM tracks", &[])
        .unwrap()
        .into_iter()
        .filter(|l| {
            l[0].as_string()
                .is_some_and(|p| p.starts_with(&format!("{r}/")))
        })
        .count() as i64
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

async fn nettoyer(state: &AppState) -> Value {
    let (s, v) = requete(state, "POST", "/api/v1/system/cleanup", None).await;
    assert_eq!(s, StatusCode::OK, "POST /system/cleanup : {v}");
    v
}

async fn regler(state: &AppState, valeur: Value) -> (StatusCode, Value) {
    requete(
        state,
        "PATCH",
        "/api/v1/system/config",
        Some(Value::Object(serde_json::Map::from_iter([(
            CLE.to_string(),
            valeur,
        )]))),
    )
    .await
}

async fn reglage_publie(state: &AppState) -> Value {
    let (s, v) = requete(state, "GET", "/api/v1/system/config", None).await;
    assert_eq!(s, StatusCode::OK);
    v[CLE].clone()
}

fn aucun_fichier_efface(fichiers: &[std::path::PathBuf], etape: &str) {
    for f in fichiers {
        assert!(
            f.exists(),
            "{etape} : un fichier a été effacé : {}",
            f.display()
        );
    }
}

async fn jouer(state: &AppState, etiquette: &str) {
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-6917-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (disque, nas) = (base.join("disque"), base.join("nas"));
    let mut fichiers = poser_album(&disque);
    fichiers.extend(poser_album(&nas));
    let db = state.backend.as_ref();
    let dirs = json!([disque.to_string_lossy(), nas.to_string_lossy()]);
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &dirs.to_string())
        .unwrap();

    // ── Le réglage : publié éteint, écrit par PATCH, refusé s'il est illisible.
    assert_eq!(
        reglage_publie(state).await,
        json!(false),
        "{etiquette} : le réglage doit être publié ÉTEINT par défaut"
    );
    let (s, v) = regler(state, json!("peut-être")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{etiquette} : {v}");
    let (s, v) = regler(state, json!(true)).await;
    assert_eq!(s, StatusCode::OK, "{etiquette} : {v}");
    assert_eq!(reglage_publie(state).await, json!(true));

    // ── ⭐ Témoin 0 : option allumée dès le premier scan.
    scanner(state, false).await;
    assert_eq!(
        entrees(db),
        (6, 0),
        "{etiquette} : option allumée, les copies du disque et du NAS doivent \
         rester DEUX entrées chacune (6 pistes, aucun exemplaire)"
    );
    assert_eq!(pistes_sous(db, &nas), 3, "{etiquette}");
    let rapport = nettoyer(state).await;
    assert_eq!(
        rapport["duplicate_tracks_removed"],
        json!(0),
        "{etiquette} : option allumée, « Nettoyage du serveur » a retiré la copie \
         d'une AUTRE racine : {rapport}"
    );
    assert_eq!(
        rapport["dedup_same_root_only"],
        json!(true),
        "{etiquette} : {rapport}"
    );
    assert_eq!(
        entrees(db),
        (6, 0),
        "{etiquette} : le nettoyage a replié deux racines"
    );
    aucun_fichier_efface(&fichiers, "témoin 0");

    // ── ⭐ Témoin 1 : option éteinte, le comportement d'avant.
    let (s, v) = regler(state, json!(false)).await;
    assert_eq!(s, StatusCode::OK, "{etiquette} : {v}");
    let rapport = nettoyer(state).await;
    assert_eq!(
        rapport["duplicate_tracks_removed"],
        json!(3),
        "{etiquette} : option éteinte, le nettoyage retire les copies de l'autre racine \
         comme avant : {rapport}"
    );
    assert_eq!(rapport["dedup_same_root_only"], json!(false));
    assert_eq!(entrees(db).0, 3, "{etiquette}");
    aucun_fichier_efface(&fichiers, "témoin 1, nettoyage");
    scanner(state, false).await;
    assert_eq!(
        entrees(db),
        (3, 3),
        "{etiquette} : option éteinte, le scan range la copie de l'autre racine \
         comme EXEMPLAIRE (#4907), comme avant"
    );

    // ── ⭐ Témoin 2 : option rallumée, analyse complète, puis nettoyage.
    let (s, v) = regler(state, json!("true")).await;
    assert_eq!(s, StatusCode::OK, "{etiquette} : {v}");
    scanner(state, true).await;
    assert_eq!(
        entrees(db),
        (6, 0),
        "{etiquette} : après une analyse complète, les exemplaires de l'autre \
         racine doivent redevenir des pistes"
    );
    let rapport = nettoyer(state).await;
    assert_eq!(
        rapport["duplicate_tracks_removed"],
        json!(0),
        "{etiquette} : option allumée, le nettoyage a retiré une copie d'une AUTRE \
         racine : {rapport}"
    );
    assert_eq!(entrees(db), (6, 0), "{etiquette}");
    assert_eq!(pistes_sous(db, &disque), 3, "{etiquette}");
    assert_eq!(pistes_sous(db, &nas), 3, "{etiquette}");
    aucun_fichier_efface(&fichiers, "témoin 2");

    // ── ⭐ Témoin 3 : option éteinte, le nettoyage reprend la règle d'avant.
    regler(state, json!(false)).await;
    let rapport = nettoyer(state).await;
    assert_eq!(
        rapport["duplicate_tracks_removed"],
        json!(3),
        "{etiquette} : {rapport}"
    );
    assert_eq!(entrees(db).0, 3, "{etiquette}");
    aucun_fichier_efface(&fichiers, "témoin 3");

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread")]
async fn deux_copies_identiques_sur_deux_racines_restent_deux_entrees_6917() {
    // Un seul test, les deux moteurs l'un après l'autre : le bail de scan est
    // global au processus, deux scans ne se chevauchent pas.
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
            .execute_batch(&format!(
                "TRUNCATE tracks, albums, artists RESTART IDENTITY CASCADE; \
                 DELETE FROM track_copies; \
                 DELETE FROM settings WHERE key IN ('{CLE}', 'music_dirs', 'ordre_des_repertoires')"
            ))
            .expect("vider la bibliothèque PostgreSQL");
        jouer(&pg, "postgres").await;
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #6917 SAUTÉE");
}
