//! #5138 — parcourir Bibliothèque, Oxygen ou Répertoires gelait le serveur
//! (JeromeQ, 0.9.165, Linux, 34 091 pistes) : `slow_query` de 7 à 9,7 s sur
//! la liste des pistes et sur son compteur, `gel_executeur_detecte` pendant
//! la navigation, lecture hachée (`famine_anneau_debut`).
//!
//! Ce fichier rejoue ce que la vue Oxygen demande — `api.getAllTracks()` du
//! client web : `GET /library/tracks?limit=2000&offset=…` jusqu'à la page
//! incomplète, dix-huit pages pour 34 000 pistes — par la ROUTE montée, sur
//! une base FICHIER au profil du testeur. Il cloue :
//!
//!  (a) le résultat : les pages mises bout à bout sont la liste visible
//!      entière (`list_visible`), le `total` est `count_visible` ;
//!  (b) l'exécuteur : un battement de 10 ms sur l'UNIQUE fil de l'exécuteur
//!      ne doit jamais attendre plus de 500 ms pendant les dix-huit pages.
//!      Avant #5138, les lectures synchrones de la route tenaient ce fil
//!      pendant toute la page.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

/// Le profil de JeromeQ : 530 artistes, 1 752 albums dont 52 distants (30
/// doublent un local), ≈ 34 000 pistes dont des copies MP3 de moindre
/// qualité et 60 sans album, 8 albums masqués.
fn remplir_au_profil_du_testeur(state: &AppState) {
    const LOCAUX: i64 = 1_700;
    const ARTISTES: i64 = 530;
    let artiste_de = |al: i64| al % ARTISTES + 1;
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=ARTISTES {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    for al in 1..=LOCAUX + 52 {
        let (titre, source, artiste) = match al - LOCAUX {
            k if k <= 0 => (format!("Album {al}"), "local", artiste_de(al)),
            k if k <= 30 => (format!("ALBUM {k}"), "upnp", artiste_de(k)),
            _ => (format!("Distant {al}"), "upnp", artiste_de(al)),
        };
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, '{titre}', {artiste}, '{source}');\n"
        ));
    }
    let mut id = 0_i64;
    let mut piste = |sql: &mut String, album: Option<i64>, artiste: i64, n: i64, format: &str| {
        id += 1;
        let source = if album.is_some_and(|al| al > LOCAUX) {
            "upnp"
        } else {
            "local"
        };
        let album = album.map_or("NULL".to_string(), |a| a.to_string());
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, album_artist) \
             VALUES ({id}, 'Piste {n}', {album}, {artiste}, 1, {n}, '/banc/{id}.{format}', \
             '{format}', 44100, 16, '{source}', '');\n"
        ));
    };
    for al in 1..=LOCAUX + 52 {
        for n in 1..=19 {
            piste(&mut sql, Some(al), artiste_de(al), n, "flac");
        }
    }
    for al in (1..=LOCAUX).step_by(40) {
        for n in 1..=19 {
            piste(&mut sql, Some(al), artiste_de(al), n, "mp3");
        }
    }
    for n in 1..=60 {
        piste(&mut sql, None, n % ARTISTES + 1, n, "flac");
    }
    // Un album de 1 000 WAV sans numéro de piste, rangés par dossier : le cas
    // où la recherche de copie par album seul devient quadratique.
    sql.push_str("INSERT INTO albums (id, title, artist_id, source) VALUES (1800, 'Sans titre', 1, 'local');\n");
    for n in 1..=1_000 {
        id += 1;
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, album_artist) \
             VALUES ({id}, 'Morceau {n}', 1800, 1, 1, 0, '/banc/gros-{n}.wav', 'wav', 44100, 16, 'local', '');\n"
        ));
    }
    for al in (5..=LOCAUX).step_by(211).take(8) {
        sql.push_str(&format!(
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {al});\n"
        ));
    }
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
}

async fn page(app: &axum::Router, offset: i64) -> (Vec<i64>, i64, Duration) {
    let app = app.clone();
    let uri = format!("/api/v1/library/tracks?limit=2000&offset={offset}");
    // La requête part sur une TÂCHE de l'exécuteur, comme celles d'axum en
    // production : c'est ce fil-là que la route tenait.
    tokio::spawn(async move {
        let debut = Instant::now();
        let resp = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let duree = debut.elapsed();
        // Le décodage de 2 000 pistes est le travail du TEST, pas de la route :
        // hors de l'exécuteur, pour que le battement ne mesure que la route.
        let (ids, total) = tokio::task::spawn_blocking(move || {
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            let ids: Vec<i64> = v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_i64().unwrap())
                .collect();
            (ids, v["total"].as_i64().unwrap())
        })
        .await
        .unwrap();
        (ids, total, duree)
    })
    .await
    .unwrap()
}

/// Un seul fil d'exécuteur : tout ce qui le tient se voit dans le battement.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn la_vue_oxygen_rend_toute_la_bibliotheque_sans_geler_l_executeur() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir_au_profil_du_testeur(&state);
    let app = tune_server::routes::router(state.clone());

    let arret = Arc::new(AtomicBool::new(false));
    let pire_battement_ms = Arc::new(AtomicU64::new(0));
    let battement = tokio::spawn({
        let (arret, pire) = (arret.clone(), pire_battement_ms.clone());
        async move {
            while !arret.load(Ordering::Relaxed) {
                let avant = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                pire.fetch_max(avant.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
        }
    });
    // Laisser le battement prendre son rythme.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let debut = Instant::now();
    let (mut ids, mut totaux, mut pire_page) = (Vec::new(), Vec::new(), Duration::ZERO);
    let mut offset = 0;
    let mut pages = 0;
    loop {
        let (lot, total, duree) = page(&app, offset).await;
        pages += 1;
        pire_page = pire_page.max(duree);
        totaux.push(total);
        let n = lot.len();
        ids.extend(lot);
        if n < 2_000 {
            break;
        }
        offset += 2_000;
    }
    let ecran = debut.elapsed();
    arret.store(true, Ordering::Relaxed);
    battement.await.unwrap();
    let pire_battement = pire_battement_ms.load(Ordering::Relaxed);
    eprintln!(
        "vue Oxygen : {pages} pages, {} pistes en {:.0} ms ; pire page {:.0} ms ; \
         pire battement de l'exécuteur {pire_battement} ms",
        ids.len(),
        ecran.as_secs_f64() * 1e3,
        pire_page.as_secs_f64() * 1e3,
    );

    // (a) le résultat.
    let repo = TrackRepo::with_backend(state.backend.clone());
    let attendu = repo.count_visible().unwrap();
    assert!(attendu > 32_000, "le banc : {attendu} pistes visibles");
    assert!(
        totaux.iter().all(|t| *t == attendu),
        "chaque page annonce le total visible {attendu} : {totaux:?}"
    );
    let entiere: Vec<i64> = repo
        .list_visible(100_000, 0)
        .unwrap()
        .into_iter()
        .filter_map(|t| t.id)
        .collect();
    assert_eq!(ids, entiere, "pages bout à bout ≠ liste visible entière");

    // (b) l'exécuteur.
    assert!(
        pire_battement < 500,
        "le battement de l'exécuteur a attendu {pire_battement} ms pendant la vue Oxygen : \
         une lecture synchrone de `GET /library/tracks` tient le fil de l'exécuteur (#5138)"
    );
}
