//! #4907 — la même musique dans plusieurs répertoires, par le VRAI scan.
//!
//! Trois racines de musique : `nas` (l'original), `sauvegarde` (la copie
//! octet pour octet du même dossier) et `hires` (le même album, piste 1 en
//! 96 kHz/24 bits). Le banc lance `POST /system/scan` et relit les routes
//! que les clients lisent.
//!
//! Témoins, dans l'ordre de l'issue :
//!
//! 1. la copie identique devient un EXEMPLAIRE (`track_copies`) — sur la base
//!    `b4c12445`, le dossier recopié sous une autre racine fabriquait un
//!    SECOND album ;
//! 2. les vues restent sans doublon, avec les MÊMES compteurs qu'avec la
//!    seule racine d'origine ;
//! 5. une playlist, un favori et l'historique visent toujours la même piste ;
//! 6. un fichier supprimé retire son exemplaire, pas la piste ;
//!
//! plus les routes neuves (ordre des répertoires, répertoire préféré, champ
//! `exemplaires` de la fiche d'album).
//!
//! Doctrine du saut, reprise de `pg_scan_converge_4602.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Un FLAC minimal valide pour `lofty` (même fabrique que le banc #4602),
/// avec sa fréquence et sa profondeur.
fn flac(balises: &[(&str, &str)], graine: u32, sr: u64, bits: u64) -> Vec<u8> {
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
    let vendeur = b"banc-4907";
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

fn balises(i: usize) -> Vec<(&'static str, String)> {
    vec![
        ("TITLE", TITRES[i].to_string()),
        ("ARTIST", "Miles Davis".to_string()),
        ("ALBUM", "Kind of Blue".to_string()),
        ("TRACKNUMBER", (i + 1).to_string()),
        ("DATE", "1959".to_string()),
    ]
}

fn fichier(i: usize) -> String {
    format!("0{}.flac", i + 1)
}

/// L'album sous `racine` : les trois pistes en 44,1/16, graines fixes — deux
/// racines posées ainsi portent des fichiers OCTET POUR OCTET identiques.
fn poser_album(racine: &std::path::Path) {
    let d = racine.join(DOSSIER);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..3 {
        let bal = balises(i);
        let b: Vec<(&str, &str)> = bal.iter().map(|(k, v)| (*k, v.as_str())).collect();
        std::fs::write(d.join(fichier(i)), flac(&b, i as u32 + 1, 44_100, 16)).unwrap();
    }
}

/// La piste 1 seule, en 96 kHz / 24 bits : une AUTRE qualité, mêmes balises.
fn poser_hires(racine: &std::path::Path) {
    let d = racine.join(DOSSIER);
    std::fs::create_dir_all(&d).unwrap();
    let bal = balises(0);
    let b: Vec<(&str, &str)> = bal.iter().map(|(k, v)| (*k, v.as_str())).collect();
    std::fs::write(d.join(fichier(0)), flac(&b, 99, 96_000, 24)).unwrap();
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

async fn lire(state: &AppState, route: &str) -> Value {
    let (s, v) = requete(state, "GET", route, None).await;
    assert!(s.is_success(), "GET {route} → {s} : {v}");
    v
}

fn compte(db: &dyn DbBackend, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or(-1)
}

fn music_dirs(state: &AppState, racines: &[&std::path::Path]) {
    let v: Vec<String> = racines
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &serde_json::to_string(&v).unwrap())
        .unwrap();
}

async fn scanner(state: &AppState) {
    let (s, v) = requete(state, "POST", "/api/v1/system/scan", None).await;
    assert!(s.is_success(), "POST /system/scan → {s} : {v}");
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        dernier = lire(state, "/api/v1/system/scan/status").await;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
}

/// Ce que les clients voient d'une bibliothèque : les compteurs et les vues.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Vues {
    albums_en_base: i64,
    pistes_en_base: i64,
    albums_route: i64,
    pistes_route: i64,
    stats: (Value, Value, Value, Value),
    pistes_de_l_album: usize,
    track_count_de_l_album: i64,
    recherche: (usize, usize),
}

async fn vues(state: &AppState) -> Vues {
    let db = state.backend.as_ref();
    let album = compte(db, "SELECT MIN(id) FROM albums");
    let stats = lire(state, "/api/v1/library/stats").await;
    let pistes = lire(state, &format!("/api/v1/library/albums/{album}/tracks")).await;
    let fiche = lire(state, &format!("/api/v1/library/albums/{album}")).await;
    let r = lire(state, "/api/v1/library/search?q=Freeloader").await;
    let recherche = (
        r["tracks"]
            .as_array()
            .map(Vec::len)
            .expect("`tracks` dans la recherche"),
        r["albums"]
            .as_array()
            .map(Vec::len)
            .expect("`albums` dans la recherche"),
    );
    Vues {
        albums_en_base: compte(db, "SELECT COUNT(*) FROM albums"),
        pistes_en_base: compte(db, "SELECT COUNT(*) FROM tracks"),
        albums_route: lire(state, "/api/v1/library/albums/count").await["count"]
            .as_i64()
            .unwrap_or(-1),
        pistes_route: lire(state, "/api/v1/library/tracks/count").await["count"]
            .as_i64()
            .unwrap_or(-1),
        stats: (
            stats["albums"].clone(),
            stats["tracks"].clone(),
            stats["total_duration_ms"].clone(),
            stats["total_size_bytes"].clone(),
        ),
        pistes_de_l_album: pistes.as_array().map(Vec::len).unwrap_or(0),
        track_count_de_l_album: fiche["track_count"].as_i64().unwrap_or(-1),
        recherche,
    }
}

fn references(db: &dyn DbBackend) -> Vec<(i64, String)> {
    db.query_many(
        "SELECT t.id, t.title FROM playlist_tracks p JOIN tracks t ON t.id = p.track_id \
         UNION ALL SELECT t.id, t.title FROM favorites f JOIN tracks t ON t.id = f.item_id \
           WHERE f.item_type = 'track' \
         UNION ALL SELECT t.id, t.title FROM listen_history h JOIN tracks t ON t.id = h.track_id \
         ORDER BY 1, 2",
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|r| (r[0].as_i64().unwrap(), r[1].as_string().unwrap()))
    .collect()
}

fn id_de(db: &dyn DbBackend, titre: &str) -> i64 {
    compte(
        db,
        &format!("SELECT MIN(id) FROM tracks WHERE title = '{titre}'"),
    )
}

async fn jouer(state: &AppState, etiquette: &str) {
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-4907-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (nas, sauvegarde, hires) = (
        base.join("nas"),
        base.join("sauvegarde"),
        base.join("hires"),
    );
    poser_album(&nas);
    poser_album(&sauvegarde);
    poser_hires(&hires);
    let db = state.backend.as_ref();

    // ── Référence : la seule racine d'origine.
    music_dirs(state, &[&nas]);
    scanner(state).await;
    let reference = vues(state).await;
    eprintln!("[{etiquette}] référence (nas seul) : {reference:?}");
    assert_eq!(reference.albums_en_base, 1, "{reference:?}");
    assert_eq!(reference.pistes_en_base, 3, "{reference:?}");
    assert_eq!(reference.recherche.0, 1, "{reference:?}");

    // Une playlist, un favori, l'historique — sur les pistes d'origine.
    let so_what = id_de(db, "So What");
    let freddie = id_de(db, "Freddie Freeloader");
    db.execute_batch(&format!(
        "INSERT INTO playlists (name) VALUES ('Banc 4907');
         INSERT INTO playlist_tracks (playlist_id, track_id, position)
           SELECT MAX(id), {so_what}, 0 FROM playlists;
         INSERT INTO playlist_tracks (playlist_id, track_id, position)
           SELECT MAX(id), {freddie}, 1 FROM playlists;
         INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', {so_what});
         INSERT INTO listen_history (track_id, title) VALUES ({freddie}, 'Freddie Freeloader');"
    ))
    .unwrap();
    let refs_avant = references(db);
    assert_eq!(refs_avant.len(), 4, "{refs_avant:?}");

    // ── ⭐ Témoins 1 et 2 : la copie de sauvegarde, OCTET POUR OCTET.
    music_dirs(state, &[&nas, &sauvegarde]);
    scanner(state).await;
    let avec_copie = vues(state).await;
    eprintln!("[{etiquette}] nas + sauvegarde : {avec_copie:?}");
    assert_eq!(
        avec_copie, reference,
        "{etiquette} : la copie de sauvegarde a changé ce que voient les clients \
         (doublon d'album ou de piste) — référence {reference:?}, obtenu {avec_copie:?}"
    );
    let copies: Vec<String> = db
        .query_many("SELECT file_path FROM track_copies ORDER BY file_path", &[])
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_string().unwrap())
        .collect();
    assert_eq!(
        copies.len(),
        3,
        "{etiquette} : les trois fichiers de la sauvegarde doivent être des EXEMPLAIRES : {copies:?}"
    );
    let sous_sauvegarde = sauvegarde.to_string_lossy().into_owned();
    assert!(
        copies.iter().all(|c| c.starts_with(&sous_sauvegarde)),
        "{copies:?}"
    );
    assert_eq!(
        references(db),
        refs_avant,
        "{etiquette} : une référence a changé de piste"
    );

    // Un second scan ne bouge rien.
    scanner(state).await;
    assert_eq!(vues(state).await, reference);
    assert_eq!(compte(db, "SELECT COUNT(*) FROM track_copies"), 3);

    // ── Une AUTRE qualité, même dossier sous une troisième racine : même
    //    album, une ligne sœur masquée à l'affichage (comportement #1362).
    music_dirs(state, &[&nas, &sauvegarde, &hires]);
    scanner(state).await;
    let avec_hires = vues(state).await;
    eprintln!("[{etiquette}] + hires : {avec_hires:?}");
    assert_eq!(avec_hires.albums_en_base, 1, "{avec_hires:?}");
    assert_eq!(avec_hires.albums_route, 1);
    assert_eq!(avec_hires.pistes_de_l_album, 3, "{avec_hires:?}");
    assert_eq!(avec_hires.track_count_de_l_album, 3, "{avec_hires:?}");
    assert_eq!(references(db), refs_avant);

    // La fiche d'album dit ses exemplaires : un par racine.
    let album = compte(db, "SELECT MIN(id) FROM albums");
    let fiche = lire(state, &format!("/api/v1/library/albums/{album}")).await;
    let ex = fiche["exemplaires"]
        .as_array()
        .expect("champ `exemplaires`")
        .clone();
    eprintln!("[{etiquette}] exemplaires : {ex:?}");
    assert_eq!(ex.len(), 3, "{ex:?}");
    let r = |i: usize| ex[i]["racine"].as_str().unwrap_or_default().to_string();
    assert_eq!(r(0), nas.to_string_lossy());
    assert_eq!(ex[0]["pistes"], 3);
    assert_eq!(r(2), hires.to_string_lossy());
    assert_eq!(ex[2]["sample_rate"], 96_000);
    assert!(
        ex.iter()
            .all(|e| e["joignable"] == true && e["prefere"] == false)
    );

    // ── Routes : répertoire préféré de l'album.
    let route = format!("/api/v1/library/albums/{album}/repertoire-prefere");
    let (s, v) = requete(
        state,
        "PUT",
        &route,
        Some(json!({"racine": "/pas/configuré"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = requete(
        state,
        "PUT",
        &route,
        Some(json!({"racine": sauvegarde.to_string_lossy()})),
    )
    .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        lire(state, &route).await["racine"],
        json!(sauvegarde.to_string_lossy())
    );
    let fiche = lire(state, &format!("/api/v1/library/albums/{album}")).await;
    assert_eq!(
        fiche["exemplaires"][1]["prefere"], true,
        "{}",
        fiche["exemplaires"]
    );
    let (s, v) = requete(state, "DELETE", &route, None).await;
    assert!(s.is_success() && v["retire"] == true, "{v}");
    assert_eq!(lire(state, &route).await["racine"], Value::Null);
    let (s, _) = requete(
        state,
        "GET",
        "/api/v1/library/albums/999999/repertoire-prefere",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // ── Routes : ordre des répertoires.
    let ordre = lire(state, "/api/v1/library/repertoires/ordre").await;
    assert_eq!(
        ordre["ordre"],
        json!([
            nas.to_string_lossy(),
            sauvegarde.to_string_lossy(),
            hires.to_string_lossy()
        ]),
        "sans réglage, l'ordre est celui de music_dirs : {ordre}"
    );
    let (s, v) = requete(
        state,
        "PUT",
        "/api/v1/library/repertoires/ordre",
        Some(json!({"ordre": [hires.to_string_lossy()]})),
    )
    .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        v["ordre"],
        json!([
            hires.to_string_lossy(),
            nas.to_string_lossy(),
            sauvegarde.to_string_lossy()
        ])
    );
    let (s, _) = requete(
        state,
        "PUT",
        "/api/v1/library/repertoires/ordre",
        Some(json!({"ordre": ["/inconnu"]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // ── ⭐ Témoin 6 : un fichier supprimé retire son exemplaire, pas la piste.
    std::fs::remove_file(nas.join(DOSSIER).join(fichier(0))).unwrap();
    std::fs::remove_file(sauvegarde.join(DOSSIER).join(fichier(1))).unwrap();
    scanner(state).await;
    let apres = vues(state).await;
    eprintln!("[{etiquette}] après suppressions : {apres:?}");
    assert_eq!(
        id_de(db, "So What"),
        so_what,
        "{etiquette} : « So What » a perdu son identifiant alors qu'une copie existait"
    );
    let chemin: String = db
        .query_one(
            &format!("SELECT file_path FROM tracks WHERE id = {so_what}"),
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_string()
        .unwrap();
    assert!(
        chemin.starts_with(&sous_sauvegarde),
        "{etiquette} : la copie de sauvegarde doit avoir pris la place du fichier disparu : {chemin}"
    );
    assert_eq!(
        compte(db, "SELECT COUNT(*) FROM track_copies"),
        1,
        "{etiquette} : il doit rester UNE copie (piste 3) — la 1 est promue, la 2 supprimée"
    );
    assert_eq!(apres.pistes_de_l_album, 3, "{apres:?}");
    assert_eq!(apres.albums_en_base, 1, "{apres:?}");
    assert_eq!(
        references(db),
        refs_avant,
        "{etiquette} : une référence a changé de piste"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread")]
async fn la_meme_musique_dans_plusieurs_repertoires_reste_une_seule_bibliotheque() {
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
            .execute_batch(
                "TRUNCATE tracks, albums, artists, playlists RESTART IDENTITY CASCADE; \
                 DELETE FROM favorites; DELETE FROM listen_history; \
                 DELETE FROM settings WHERE key = 'ordre_des_repertoires'",
            )
            .expect("vider la bibliothèque PostgreSQL");
        jouer(&pg, "postgres").await;
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #4907 SAUTÉE");
}
