//! Les fusions AUTOMATIQUES d'albums en double (reste de #5005, décision de
//! Bertrand du 26/09/2026).
//!
//! Avant : la fin de scan (`routes/system/scan.rs`, #593) et `POST
//! /system/cleanup` (`routes/system/enrich.rs`) écrivaient `GROUP_CONCAT(id)`
//! en dur — PostgreSQL : « function group_concat(bigint) does not exist »,
//! erreur avalée, fusion MORTE. Et sur SQLite, où elles tournaient, elles
//! ignoraient les paires déclarées distinctes (#1276) et ne déplaçaient que
//! les pistes : favoris, étiquettes et écoutes du perdant mouraient avec lui.
//!
//! Ce banc joue les trois chemins — le VRAI scan (`POST /system/scan`), le
//! nettoyage et la route manuelle — sur SQLite et sur PostgreSQL
//! (`TUNE_TEST_PG_URL`), et exige de chacun :
//!
//! * deux doublons fusionnés en un ;
//! * une paire déclarée distincte jamais fusionnée ;
//! * favori, étiquette et écoute du perdant retrouvés sur l'album conservé.
//!
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

async fn requete(state: &AppState, methode: &str, route: &str) -> (u16, Value) {
    let app = tune_server::routes::router(state.clone());
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
    let statut = r.status().as_u16();
    let o = axum::body::to_bytes(r.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&o)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&o).into_owned())),
    )
}

fn scalaire(state: &AppState, sql: &str) -> i64 {
    state
        .backend
        .query_one(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"))
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Une base PostgreSQL sert à plusieurs étapes de la CI : on retire ce que
/// ce banc a pu y laisser, et rien d'autre.
fn purger_les_sondes(state: &AppState) {
    for sql in [
        "DELETE FROM favorites WHERE item_type = 'album' AND item_id IN \
         (SELECT id FROM albums WHERE LOWER(title) LIKE 'sonde fusion%')",
        "DELETE FROM item_tags WHERE tag_id IN (SELECT id FROM tags WHERE name = 'sonde-fusion')",
        "DELETE FROM tags WHERE name = 'sonde-fusion'",
        "DELETE FROM listen_history WHERE title LIKE 'Sonde Fusion%'",
        "DELETE FROM album_distinct_pairs WHERE album_a_id IN \
         (SELECT id FROM albums WHERE LOWER(title) LIKE 'sonde fusion%') \
         OR album_b_id IN (SELECT id FROM albums WHERE LOWER(title) LIKE 'sonde fusion%')",
        "DELETE FROM tracks WHERE file_path LIKE '/sonde-fusion/%' OR album_id IN \
         (SELECT id FROM albums WHERE LOWER(title) LIKE 'sonde fusion%')",
        "DELETE FROM albums WHERE LOWER(title) LIKE 'sonde fusion%'",
    ] {
        executer(state, sql);
    }
}

fn artiste(state: &AppState, nom: &str) -> i64 {
    executer(
        state,
        &format!("INSERT INTO artists (name) VALUES ('{nom}')"),
    );
    scalaire(
        state,
        &format!("SELECT MAX(id) FROM artists WHERE name = '{nom}'"),
    )
}

/// Un album local et ses `pistes` pistes (chemins fictifs : le scan n'est
/// pas joué sur ce jeu-là).
fn album(state: &AppState, titre: &str, artiste: i64, pistes: usize) -> i64 {
    executer(
        state,
        &format!(
            "INSERT INTO albums (title, artist_id, source, track_count) \
             VALUES ('{titre}', {artiste}, 'local', 0)"
        ),
    );
    let id = scalaire(
        state,
        &format!("SELECT MAX(id) FROM albums WHERE title = '{titre}' AND artist_id = {artiste}"),
    );
    for i in 0..pistes {
        executer(
            state,
            &format!(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path, source) \
                 VALUES ('{titre} {i}', {id}, {artiste}, 1000, '/sonde-fusion/{id}/{i}.flac', 'local')"
            ),
        );
    }
    id
}

/// Favori, étiquette et écoute posés sur `album`.
fn marquer(state: &AppState, album: i64) {
    executer(
        state,
        &format!(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', {album})"
        ),
    );
    executer(state, "INSERT INTO tags (name) VALUES ('sonde-fusion')");
    let tag = scalaire(state, "SELECT id FROM tags WHERE name = 'sonde-fusion'");
    executer(
        state,
        &format!(
            "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES ({tag}, 'album', {album})"
        ),
    );
    executer(
        state,
        &format!(
            "INSERT INTO listen_history (title, album_id, source) \
             VALUES ('Sonde Fusion écoute', {album}, 'local')"
        ),
    );
}

fn existe(state: &AppState, id: i64) -> bool {
    scalaire(
        state,
        &format!("SELECT COUNT(*) FROM albums WHERE id = {id}"),
    ) > 0
}

/// Les marqueurs du perdant ont-ils suivi jusqu'à `garde` ?
fn marqueurs_sur(state: &AppState, garde: i64) -> Vec<String> {
    let mut manquants = Vec::new();
    for (nom, sql) in [
        (
            "favori",
            format!(
                "SELECT COUNT(*) FROM favorites WHERE item_type = 'album' AND item_id = {garde}"
            ),
        ),
        (
            "étiquette",
            format!(
                "SELECT COUNT(*) FROM item_tags it JOIN tags t ON t.id = it.tag_id \
                 WHERE t.name = 'sonde-fusion' AND it.item_type = 'album' AND it.item_id = {garde}"
            ),
        ),
        (
            "écoute",
            format!(
                "SELECT COUNT(*) FROM listen_history \
                 WHERE title = 'Sonde Fusion écoute' AND album_id = {garde}"
            ),
        ),
    ] {
        if scalaire(state, &sql) != 1 {
            manquants.push(format!("{nom} absent de l'album conservé {garde}"));
        }
    }
    manquants
}

/// `POST /system/cleanup` puis `POST /library/albums/merge-duplicates`.
async fn nettoyage_et_manuel(state: &AppState, moteur: &str) -> Vec<String> {
    purger_les_sondes(state);
    let mut ecarts = Vec::new();
    let x = artiste(state, &format!("Sonde Fusion Artiste {moteur}"));

    // ── Nettoyage ──
    let garde = album(state, "Sonde Fusion Nettoyage", x, 2);
    let perdant = album(state, "sonde fusion nettoyage", x, 1);
    marquer(state, perdant);
    let d1 = album(state, "Sonde Fusion Distinct", x, 1);
    let d2 = album(state, "sonde fusion distinct", x, 1);
    let (st, _) = requete(
        state,
        "POST",
        &format!("/api/v1/library/albums/{d1}/distinct/{d2}"),
    )
    .await;
    assert_eq!(st, 200, "{moteur} : déclarer la paire distincte");

    let (st, corps) = requete(state, "POST", "/api/v1/system/cleanup").await;
    if st != 200 || corps["duplicate_albums_merged"].as_i64() != Some(1) {
        ecarts.push(format!("nettoyage : {st} {corps}"));
    }
    if !existe(state, garde) || existe(state, perdant) {
        ecarts.push(format!(
            "nettoyage : {garde} (2 pistes) doit rester, {perdant} disparaître"
        ));
    }
    if !(existe(state, d1) && existe(state, d2)) {
        ecarts.push("nettoyage : la paire déclarée distincte a été fusionnée".into());
    }
    let pistes = scalaire(
        state,
        &format!("SELECT COUNT(*) FROM tracks WHERE album_id = {garde}"),
    );
    if pistes != 3 {
        ecarts.push(format!(
            "nettoyage : {pistes} pistes sur l'album conservé, 3 attendues"
        ));
    }
    ecarts.extend(
        marqueurs_sur(state, garde)
            .into_iter()
            .map(|m| format!("nettoyage : {m}")),
    );

    // ── Manuel : même logique ──
    purger_les_sondes(state);
    let garde = album(state, "Sonde Fusion Manuel", x, 2);
    let perdant = album(state, "SONDE FUSION MANUEL", x, 1);
    marquer(state, perdant);
    let d1 = album(state, "Sonde Fusion Distinct", x, 1);
    let d2 = album(state, "sonde fusion distinct", x, 1);
    requete(
        state,
        "POST",
        &format!("/api/v1/library/albums/{d1}/distinct/{d2}"),
    )
    .await;
    let (st, corps) = requete(state, "POST", "/api/v1/library/albums/merge-duplicates").await;
    if st != 200 || corps["merged"].as_i64() != Some(1) || corps["protected"].as_i64() != Some(1) {
        ecarts.push(format!("manuel : {st} {corps}"));
    }
    if !existe(state, garde) || existe(state, perdant) || !existe(state, d1) || !existe(state, d2) {
        ecarts.push("manuel : mauvais albums fusionnés".into());
    }
    ecarts.extend(
        marqueurs_sur(state, garde)
            .into_iter()
            .map(|m| format!("manuel : {m}")),
    );
    purger_les_sondes(state);
    ecarts
}

// ─── Le VRAI scan ─────────────────────────────────────────────────────────

/// Un FLAC minimal mais valide pour `lofty` (repris de
/// `pg_scan_converge_4602.rs`), avec des « trames » propres à chaque fichier.
fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | (44_100 * 180);
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut vc = Vec::new();
    let vendeur = b"banc-fusions";
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

async fn scanner(state: &AppState) {
    let (st, corps) = requete(state, "POST", "/api/v1/system/scan").await;
    assert!((200..300).contains(&st), "scan : {st} {corps}");
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        dernier = requete(state, "GET", "/api/v1/system/scan/status").await.1;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
}

/// Les pistes de deux dossiers, deux albums « Sonde Fusion Scan » et « Sonde
/// Fusion Scan Distinct ». Après un premier scan, on fabrique pour chacun le
/// doublon que #593 décrit (une ligne `albums` de même titre et même artiste
/// qui a reçu une des pistes), puis on rescanne : la fin de scan doit fusionner
/// le premier et laisser le second, déclaré distinct.
async fn fin_de_scan(state: &AppState, etiquette: &str) -> Vec<String> {
    purger_les_sondes(state);
    let mut ecarts = Vec::new();
    // Hors de `temp_dir()` : le scan y écarte tout (`is_tune_temp_file`).
    let racine = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-fusions-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&racine);
    let artiste_nom = format!("Sonde Fusion Scan Artiste {etiquette}");
    for (dossier, album_titre) in [
        ("scan", "Sonde Fusion Scan"),
        ("distinct", "Sonde Fusion Scan Distinct"),
    ] {
        let d = racine.join(dossier);
        std::fs::create_dir_all(&d).unwrap();
        for n in 1..=2u32 {
            let titre = format!("{album_titre} piste {n}");
            let numero = n.to_string();
            let balises = [
                ("TITLE", titre.as_str()),
                ("ARTIST", artiste_nom.as_str()),
                ("ALBUMARTIST", artiste_nom.as_str()),
                ("ALBUM", album_titre),
                ("TRACKNUMBER", numero.as_str()),
            ];
            let graine = n + if dossier == "scan" { 10 } else { 20 };
            std::fs::write(d.join(format!("{n:02}.flac")), flac(&balises, graine)).unwrap();
        }
    }
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &format!("[{}]", serde_json::json!(racine.to_string_lossy())),
        )
        .unwrap();
    scanner(state).await;

    let id_de = |titre: &str| {
        scalaire(
            state,
            &format!("SELECT MIN(id) FROM albums WHERE title = '{titre}' AND source = 'local'"),
        )
    };
    let original = id_de("Sonde Fusion Scan");
    let original_d = id_de("Sonde Fusion Scan Distinct");
    assert!(
        original > 0 && original_d > 0,
        "{etiquette} : le premier scan n'a pas créé les albums sondes"
    );

    // Le doublon de #593, pour chacun des deux albums.
    let mut doublons = Vec::new();
    for (orig, titre) in [
        (original, "sonde fusion scan"),
        (original_d, "sonde fusion scan distinct"),
    ] {
        executer(
            state,
            &format!(
                "INSERT INTO albums (title, artist_id, source, track_count) \
                 SELECT '{titre}', artist_id, 'local', 0 FROM albums WHERE id = {orig}"
            ),
        );
        let dbl = scalaire(
            state,
            &format!("SELECT MAX(id) FROM albums WHERE title = '{titre}'"),
        );
        executer(
            state,
            &format!(
                "UPDATE tracks SET album_id = {dbl} WHERE id = \
                 (SELECT MAX(id) FROM tracks WHERE album_id = {orig})"
            ),
        );
        doublons.push(dbl);
    }
    let (doublon, doublon_d) = (doublons[0], doublons[1]);
    marquer(state, doublon);
    let (st, _) = requete(
        state,
        "POST",
        &format!("/api/v1/library/albums/{original_d}/distinct/{doublon_d}"),
    )
    .await;
    assert_eq!(st, 200, "{etiquette} : déclarer la paire distincte");

    scanner(state).await;

    let restants = scalaire(
        state,
        "SELECT COUNT(*) FROM albums WHERE LOWER(title) = 'sonde fusion scan'",
    );
    if restants != 1 {
        ecarts.push(format!(
            "fin de scan : {restants} albums « sonde fusion scan », 1 attendu (doublon non fusionné)"
        ));
    }
    // Une piste chacun : à égalité, le plus ancien est conservé.
    let garde = if existe(state, original) {
        original
    } else {
        doublon
    };
    if garde != original {
        ecarts.push(format!(
            "fin de scan : {original} (le plus ancien) devait être conservé"
        ));
    }
    if restants == 1 {
        let pistes = scalaire(
            state,
            &format!("SELECT COUNT(*) FROM tracks WHERE album_id = {garde}"),
        );
        if pistes != 2 {
            ecarts.push(format!(
                "fin de scan : {pistes} pistes sur l'album conservé, 2 attendues"
            ));
        }
        ecarts.extend(
            marqueurs_sur(state, garde)
                .into_iter()
                .map(|m| format!("fin de scan : {m}")),
        );
    }
    if !(existe(state, original_d) && existe(state, doublon_d)) {
        ecarts.push("fin de scan : la paire déclarée distincte a été fusionnée".into());
    }

    let _ = std::fs::remove_dir_all(&racine);
    purger_les_sondes(state);
    ecarts
}

fn etat_pg() -> Option<AppState> {
    let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    // Pas de `ok()?` : une base posée mais injoignable doit ROUGIR.
    Some(AppState::new("", 0, config).expect("AppState sur PostgreSQL"))
}

#[tokio::test(flavor = "multi_thread")]
async fn nettoyage_et_fusion_manuelle_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    let ecarts = nettoyage_et_manuel(&state, "sqlite").await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_nettoyage_et_fusion_manuelle() {
    let Some(state) = etat_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let ecarts = nettoyage_et_manuel(&state, "postgres").await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}

/// Un seul test pour les deux moteurs : le bail de scan est global au
/// processus, deux scans ne se chevauchent pas.
#[tokio::test(flavor = "multi_thread")]
async fn la_fin_de_scan_fusionne_les_doublons_et_respecte_les_paires_distinctes() {
    // Les deux moteurs jouent, puis on juge : un rouge SQLite ne masque pas
    // le verdict PostgreSQL.
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    let mut ecarts: Vec<String> = fin_de_scan(&sqlite, "sqlite")
        .await
        .into_iter()
        .map(|e| format!("SQLite — {e}"))
        .collect();

    #[cfg(feature = "postgres")]
    match etat_pg() {
        Some(pg) => ecarts.extend(
            fin_de_scan(&pg, "postgres")
                .await
                .into_iter()
                .map(|e| format!("PostgreSQL — {e}")),
        ),
        None => eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de la fin de scan SAUTÉE"),
    }
    assert!(ecarts.is_empty(), "{ecarts:#?}");
}
