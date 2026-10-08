//! #5837 (fil 2153) — une compilation dont l'artiste d'album change de
//! résolution entre deux scans garde SA ligne album.
//!
//! Avant, l'importeur ne retrouvait pas la ligne d'origine — sa clé porte
//! l'ancien artiste — et en créait une nouvelle. L'ancienne gardait titre,
//! pochette et année, perdait toutes ses pistes et restait dans la grille
//! jusqu'à la purge des orphelins de FIN de scan : une fiche « 0 piste », sans
//! artiste, pendant des heures sur une grande bibliothèque. L'album changeait
//! aussi d'identifiant.
//!
//! Deux façons d'y arriver, jouées sur de vrais FLAC par le VRAI scan
//! (`POST /system/scan`), sur SQLite et sur PostgreSQL :
//!
//! 1. les balises changent sur le disque (artiste d'album générique remplacé
//!    par « Various Artists »), `quality_split` coupé, scan ordinaire ;
//! 2. une base héritée : la ligne album est sous un artiste d'album vide et
//!    n'a pas de `folder_path` ; scan complet, `quality_split` par défaut.
//!
//! Le témoin est l'identifiant : la ligne d'origine porte toujours toutes les
//! pistes, sous « Various Artists », et aucune autre ligne n'a ce titre. Une
//! nouvelle ligne voudrait dire une ancienne vidée pendant le scan.
//!
//! Doctrine du saut, reprise de `pg_scan_converge_4602.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Un FLAC minimal mais valide pour `lofty` : `fLaC`, STREAMINFO, puis un
/// bloc VORBIS_COMMENT (dernier bloc), puis des octets de « trames » propres à
/// chaque fichier (le hachage de doublon échantillonne à 25 %).
fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    // STREAMINFO (type 0), 34 octets.
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
    // VORBIS_COMMENT (type 4), dernier bloc.
    let mut vc = Vec::new();
    let vendeur = b"banc-5837";
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

/// Une pochette `cover.png` propre au dossier : c'est elle qui arme la
/// recherche des compilations éparpillées (`find_scattered_compilation`), qui
/// renonce sans pochette. Une vraie bibliothèque en a presque partout.
fn pochette(graine: u32) -> Vec<u8> {
    let graine = graine % 251;
    let img = image::RgbImage::from_fn(32, 32, |x, y| {
        let v = (x * 7 + y * 13 + graine * 37) % 256;
        image::Rgb([
            v as u8,
            ((v * 3) % 256) as u8,
            ((x * y + graine) % 256) as u8,
        ])
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

const TITRE: &str = "Nuits du swing - volume 1";
const PISTES: usize = 6;

/// La compilation : six interprètes, un même ALBUM, `COMPILATION=1`, et
/// l'artiste d'album donné (`None` = balise absente).
fn poser(dossier: &std::path::Path, album_artiste: Option<&str>) {
    std::fs::create_dir_all(dossier).unwrap();
    for i in 1..=PISTES {
        let titre = format!("Chanson {i}");
        let artiste = format!("Interprete {i}");
        let num = i.to_string();
        let mut b: Vec<(&str, &str)> = vec![
            ("TITLE", &titre),
            ("ARTIST", &artiste),
            ("ALBUM", TITRE),
            ("TRACKNUMBER", &num),
            ("DATE", "1992"),
            ("COMPILATION", "1"),
        ];
        if let Some(a) = album_artiste {
            b.push(("ALBUMARTIST", a));
        }
        std::fs::write(dossier.join(format!("{i:02}.flac")), flac(&b, i as u32)).unwrap();
    }
    std::fs::write(dossier.join("cover.png"), pochette(7)).unwrap();
}

async fn scanner(state: &AppState, complet: bool) {
    let route = if complet {
        "/api/v1/system/scan?full=true"
    } else {
        "/api/v1/system/scan"
    };
    requete(state, "POST", route).await;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if requete(state, "GET", "/api/v1/system/scan/status").await["status"] != "scanning" {
            return;
        }
    }
    panic!("scan jamais terminé");
}

fn ids_du_titre(state: &AppState) -> Vec<i64> {
    state
        .backend
        .query_many(
            &format!("SELECT id FROM albums WHERE title = '{TITRE}' ORDER BY id"),
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first()?.as_i64())
        .collect()
}

/// La ligne `id` porte toutes les pistes, sous « Various Artists », et elle
/// est la seule de ce titre.
async fn verifier(state: &AppState, etiquette: &str, id: i64) {
    assert_eq!(
        ids_du_titre(state),
        vec![id],
        "{etiquette} : la compilation a changé de ligne album — l'ancienne ({id}) a été \
         vidée de ses pistes pendant le scan et une nouvelle créée (#5837)"
    );
    let pistes = requete(state, "GET", &format!("/api/v1/library/albums/{id}/tracks")).await;
    assert_eq!(
        pistes.as_array().map(Vec::len),
        Some(PISTES),
        "{etiquette} : la fiche de l'album {id} n'a pas toutes ses pistes : {pistes}"
    );
    let album = requete(state, "GET", &format!("/api/v1/library/albums/{id}")).await;
    assert_eq!(
        album["artist_name"], "Various Artists",
        "{etiquette} : artiste d'album : {album}"
    );
    assert_eq!(album["is_compilation"], true, "{etiquette} : {album}");
}

async fn jouer(state: &AppState, etiquette: &str) {
    // Hors de `temp_dir()` : le scan y écarte tout (`is_tune_temp_file`).
    let racine = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-5837-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&racine);
    let dossier = racine.join("Compilations").join(TITRE);
    poser(&dossier, Some("Var"));
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set(
            "music_dirs",
            &format!("[{}]", serde_json::json!(racine.to_string_lossy())),
        )
        .unwrap();

    // 1. Les balises changent sur le disque, `quality_split` coupé.
    reglages.set("quality_split", "false").unwrap();
    scanner(state, false).await;
    let ids = ids_du_titre(state);
    assert_eq!(ids.len(), 1, "{etiquette} : premier scan : {ids:?}");
    let id = ids[0];
    // L'heure de modification doit bouger pour que le scan ordinaire relise.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    poser(&dossier, Some("Various Artists"));
    scanner(state, false).await;
    verifier(state, &format!("{etiquette}, balises changées"), id).await;

    // 2. Base héritée : artiste d'album vide, pas de `folder_path` ;
    //    `quality_split` par défaut, scan complet.
    reglages.set("quality_split", "true").unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (name) VALUES (''); \
             UPDATE albums SET artist_id = (SELECT MIN(id) FROM artists WHERE name = ''), \
             is_compilation = 0, folder_path = NULL WHERE id = {id}"
        ))
        .expect("simuler une base héritée");
    scanner(state, true).await;
    verifier(state, &format!("{etiquette}, base héritée"), id).await;

    let _ = std::fs::remove_dir_all(&racine);
}

#[tokio::test(flavor = "multi_thread")]
async fn une_compilation_reclassee_garde_sa_ligne_album_5837() {
    // Les deux moteurs l'un après l'autre : le bail de scan est global au
    // processus, deux scans ne se chevauchent pas.
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    jouer(&sqlite, "SQLite").await;

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
        jouer(&pg, "PostgreSQL").await;
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #5837 SAUTÉE");
}
