//! #5438 — un clic sur un raccourci vers une collection intelligente
//! provoquait une micro-coupure, à chaque fois (Bertrand chez Yves Corbat,
//! darTZeel LHC-208, 58 359 pistes, 29/09/2026). Ouvrir la Bibliothèque
//! complète pendant la lecture, elle, ne coupait rien : `list_albums` passe
//! par `spawn_blocking` depuis #4800.
//!
//! Ce que le clic déclenche côté client web (`CollectionsV2.svelte`,
//! `charger()` puis `completerPochettes()`) : `GET /library/collections`,
//! `GET /library/smart-collections` — deux `COUNT(DISTINCT …)` sur toute la
//! bibliothèque PAR collection — puis, en parallèle, `GET
//! /library/smart-collections/{id}/albums` pour CHACUNE (les listes ne portent
//! pas de `covers`). La vue Oxygen filtrée par collection y ajoute
//! `/library/facets`, `/library/albums-detailed` et `/library/folder-facet`
//! (`?collection=`), qui résolvent la collection par `resolve_collection`.
//! Toutes ces lectures étaient synchrones, posées sur les fils de l'exécuteur :
//! quand elles les tenaient TOUS, le flux HTTP vers le renderer ne recevait
//! plus un octet.
//!
//! Le banc : une base SQLite FICHIER de 58 359 pistes, le VRAI routeur servi
//! sur une VRAIE socket par un exécuteur Tokio à [`FILS_EXECUTEUR`] fils (lire
//! sa note : le témoin ne vaut que pour un petit hôte), un
//! flux qui émet un bloc toutes les [`CADENCE_FLUX`] (il a besoin d'un fil
//! libre pour chaque bloc, comme l'envoi du flux réel), lu par un « renderer »
//! sur un fil système À PART. Pendant que la rafale du clic tourne, le plus
//! grand silence du flux ne doit pas dépasser [`SEUIL_SILENCE`].
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::Value;

use tune_server::state::AppState;

/// Fils de l'exécuteur. En production `#[tokio::main]` en prend un par cœur :
/// deux est celui d'un hôte à deux cœurs (NAS, conteneur borné).
///
/// ⚠️ Mesuré sur ce banc AVANT le correctif (29/09/2026, Shrek) : pire silence
/// 1 719 ms à 2 fils, 247 ms à 4, 57 ms à 8. Le pool de lecture SQLite n'a que
/// TROIS connexions (`READ_POOL_SIZE`) et l'attente d'une connexion rend son
/// fil (`attendre_hors_executeur`) : au plus trois lectures tiennent un fil à
/// la fois. Au-delà de trois fils, l'exécuteur garde donc un fil libre et le
/// silence reste court. Le témoin n'est décisif qu'à deux fils ; il ne prouve
/// RIEN pour une machine à huit cœurs comme le MacBook M1 du testeur : à huit
/// fils, 76 et 189 ms sans le correctif, 76 et 67 ms avec — et une lecture
/// triviale de la base y attend 0,7 à 1,3 s dans les DEUX cas (pool saturé).
const FILS_EXECUTEUR: usize = 2;

/// Un bloc du flux toutes les 5 ms : un renderer tire en continu.
const CADENCE_FLUX: Duration = Duration::from_millis(5);

/// Le plus long silence admis sur le flux pendant le clic. Un tampon de
/// renderer tient couramment quelques centaines de millisecondes ; au-delà,
/// c'est la micro-coupure.
const SEUIL_SILENCE: Duration = Duration::from_millis(300);

/// Pistes du testeur (Yves Corbat, 29/09/2026).
const PISTES: i64 = 58_359;
const PISTES_PAR_ALBUM: i64 = 12;
const ARTISTES: i64 = 900;
/// Genres qui recoupent les collections intelligentes semées par la migration
/// (Jazz, Rock, Classique, Electro, Bandes originales, Soul & Funk…).
const GENRES: [&str; 8] = [
    "Jazz",
    "Rock",
    "Classical",
    "Electronic",
    "Soundtrack",
    "Soul",
    "Chanson",
    "Pop",
];

fn remplir_au_profil_du_testeur(state: &AppState) -> i64 {
    let albums = (PISTES + PISTES_PAR_ALBUM - 1) / PISTES_PAR_ALBUM;
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=ARTISTES {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    for al in 1..=albums {
        let genre = GENRES[(al % GENRES.len() as i64) as usize];
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source, genre, year) \
             VALUES ({al}, 'Album {al}', {}, 'local', '{genre}', {});\n",
            al % ARTISTES + 1,
            1950 + al % 70,
        ));
    }
    for id in 1..=PISTES {
        let al = (id - 1) / PISTES_PAR_ALBUM + 1;
        let n = (id - 1) % PISTES_PAR_ALBUM + 1;
        let genre = GENRES[(al % GENRES.len() as i64) as usize];
        let (format, cadence) = if id % 7 == 0 {
            ("dsf", 2_822_400)
        } else {
            ("flac", if id % 3 == 0 { 192_000 } else { 44_100 })
        };
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, album_artist, genre, year, \
             label, duration_ms) \
             VALUES ({id}, 'Piste {n}', {al}, {artiste}, 1, {n}, \
             '/musique/Artiste {artiste}/Album {al}/{n:02}.{format}', '{format}', {cadence}, 24, \
             'local', 'Artiste {artiste}', '{genre}', {annee}, 'Label {label}', 240000);\n",
            artiste = al % ARTISTES + 1,
            annee = 1950 + al % 70,
            label = al % 150,
        ));
    }
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
    albums
}

/// Le flux : un bloc de 4 Kio toutes les [`CADENCE_FLUX`], sans fin. Chaque
/// bloc attend une minuterie de l'exécuteur, puis un fil libre pour l'écrire.
async fn flux() -> impl IntoResponse {
    let blocs = futures_util::stream::unfold((), |()| async {
        tokio::time::sleep(CADENCE_FLUX).await;
        Some((
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[0u8; 4096])),
            (),
        ))
    });
    Body::from_stream(blocs)
}

/// Un GET HTTP/1.1 bloquant, `Connection: close` : statut et corps. Les
/// réponses JSON d'axum portent un `Content-Length`, pas de découpage.
fn obtenir(addr: SocketAddr, chemin: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {chemin} HTTP/1.1\r\nHost: banc\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut brut = Vec::new();
    s.read_to_end(&mut brut).unwrap();
    let fin_entete = brut
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("réponse HTTP sans en-tête");
    let entete = String::from_utf8_lossy(&brut[..fin_entete]).to_ascii_lowercase();
    assert!(
        !entete.contains("transfer-encoding: chunked"),
        "{chemin} : réponse découpée inattendue"
    );
    let statut: u16 = entete
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    let corps = serde_json::from_slice(&brut[fin_entete + 4..]).unwrap_or(Value::Null);
    (statut, corps)
}

/// Le nom d'une collection, pour une chaîne de requête (les noms semés portent
/// un émoji).
fn encoder(nom: &str) -> String {
    nom.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Le coût de la requête elle-même, hors contention : la résolution de la
/// collection « Jazz » (le SQL de `smart_collection_track_ids`) et son plan.
/// Rien n'est affirmé sur la durée — elle dépend de la machine —, elle est
/// imprimée pour le rapport.
fn mesurer_la_resolution_seule(state: &AppState) {
    use tune_server::routes::{smart_collections::build_album_query, smart_refs};
    let r = state
        .backend
        .query_one(
            "SELECT rules, match_mode FROM smart_collections WHERE name LIKE '%Jazz%'",
            &[],
        )
        .unwrap()
        .unwrap();
    let (regles, mode) = (r[0].as_string().unwrap(), r[1].as_string().unwrap());
    let resolveur = smart_refs::DbRefResolver::new(&state.backend);
    let ctx = smart_refs::RefCtx::root(&resolveur, Some(1));
    let (ou, _, _) = build_album_query(&regles, &mode, "title", "asc", None, &ctx);
    let sql = format!(
        "SELECT DISTINCT t.id FROM albums al LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id {ou}"
    );
    let t0 = Instant::now();
    let n = state.backend.query_many(&sql, &[]).unwrap().len();
    let duree = t0.elapsed();
    let plan: Vec<String> = state
        .backend
        .query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
        .unwrap()
        .into_iter()
        .filter_map(|l| l.last().and_then(|v| v.as_string()))
        .collect();
    eprintln!(
        "#5438 : résolution seule de « Jazz » : {n} pistes en {:.0} ms ; {ou} ; plan {plan:?}",
        duree.as_secs_f64() * 1e3
    );
}

#[test]
fn un_clic_sur_une_collection_intelligente_ne_coupe_pas_le_flux() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(FILS_EXECUTEUR)
        .enable_all()
        .build()
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");

    let (addr, albums, collections, backend) = rt.block_on(async {
        let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
        let albums = remplir_au_profil_du_testeur(&state);
        let collections: Vec<(i64, String)> = state
            .backend
            .query_many("SELECT id, name FROM smart_collections ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|r| (r[0].as_i64().unwrap(), r[1].as_string().unwrap()))
            .collect();
        mesurer_la_resolution_seule(&state);
        let backend = state.backend.clone();
        let app = tune_server::routes::router(state).route("/banc/flux", get(flux));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ecoute.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        (addr, albums, collections, backend)
    });
    assert!(
        collections.len() >= 8,
        "la migration sème les collections intelligentes : {collections:?}"
    );
    let jazz = collections
        .iter()
        .find(|(_, n)| n.contains("Jazz"))
        .expect("collection semée « Jazz »")
        .clone();

    // Le renderer : un fil système à lui, jamais un fil de l'exécuteur.
    let arret = Arc::new(AtomicBool::new(false));
    let renderer = std::thread::spawn({
        let arret = arret.clone();
        move || {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET /banc/flux HTTP/1.1\r\nHost: banc\r\n\r\n").unwrap();
            let mut tampon = [0u8; 65_536];
            let mut arrivees = Vec::new();
            while !arret.load(Ordering::Relaxed) {
                let n = s.read(&mut tampon).unwrap();
                assert!(n > 0, "le flux s'est fermé");
                arrivees.push(Instant::now());
            }
            arrivees
        }
    });
    // Une lecture TRIVIALE de la base, toutes les 10 ms, depuis un fil
    // système : combien attend-elle une connexion du pool pendant le clic ?
    // `spawn_blocking` n'y change rien — mesuré, imprimé, pas affirmé.
    let sonde = std::thread::spawn({
        let (arret, backend) = (arret.clone(), backend.clone());
        move || {
            let mut pire = Duration::ZERO;
            while !arret.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                backend
                    .query_one("SELECT id FROM tracks WHERE id = 1", &[])
                    .unwrap();
                pire = pire.max(t0.elapsed());
                std::thread::sleep(Duration::from_millis(10));
            }
            pire
        }
    });
    // Laisser le flux prendre son rythme.
    std::thread::sleep(Duration::from_millis(300));

    // La rafale du clic, requêtes lancées ensemble comme le client web.
    let mut chemins = vec![
        "/api/v1/library/collections".to_string(),
        "/api/v1/library/smart-collections".to_string(),
    ];
    for (id, _) in &collections {
        chemins.push(format!("/api/v1/library/smart-collections/{id}/albums"));
    }
    let nom = encoder(&jazz.1);
    chemins.push(format!("/api/v1/library/facets?collection={nom}"));
    chemins.push(format!("/api/v1/library/albums-detailed?collection={nom}"));
    chemins.push(format!("/api/v1/library/folder-facet?collection={nom}"));
    chemins.push(format!(
        "/api/v1/library/tracks?collection={nom}&limit=2000"
    ));

    let debut = Instant::now();
    let requetes: Vec<_> = chemins
        .iter()
        .cloned()
        .map(|c| {
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let r = obtenir(addr, &c);
                (c, r, t0.elapsed())
            })
        })
        .collect();
    let mut durees = Vec::new();
    let reponses: Vec<(String, (u16, Value))> = requetes
        .into_iter()
        .map(|r| {
            let (c, r, d) = r.join().unwrap();
            durees.push((d, c.clone()));
            (c, r)
        })
        .collect();
    let fin = Instant::now();
    std::thread::sleep(Duration::from_millis(50));
    arret.store(true, Ordering::Relaxed);
    let arrivees = renderer.join().unwrap();
    let pire_lecture_triviale = sonde.join().unwrap();

    // Le plus long silence du flux PENDANT la rafale.
    let pire_silence = arrivees
        .windows(2)
        .filter(|w| w[1] >= debut && w[0] <= fin)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    eprintln!(
        "#5438 : {} requêtes du clic en {:.0} ms ; pire silence du flux {:.0} ms \
         ({FILS_EXECUTEUR} fils d'exécuteur, {PISTES} pistes, {albums} albums)",
        chemins.len(),
        (fin - debut).as_secs_f64() * 1e3,
        pire_silence.as_secs_f64() * 1e3,
    );
    eprintln!(
        "#5438 : pire attente d'une lecture triviale de la base pendant le clic : {:.0} ms",
        pire_lecture_triviale.as_secs_f64() * 1e3
    );
    durees.sort();
    for (d, c) in durees.iter().rev().take(5) {
        eprintln!("  {:>6.0} ms  {c}", d.as_secs_f64() * 1e3);
    }

    // Les réponses : le correctif ne change aucun résultat.
    for (c, (statut, _)) in &reponses {
        assert_eq!(*statut, 200, "{c}");
    }
    let corps = |suffixe: &str| {
        &reponses
            .iter()
            .find(|(c, _)| c.ends_with(suffixe))
            .unwrap_or_else(|| panic!("{suffixe}"))
            .1
            .1
    };
    let albums_jazz = albums / GENRES.len() as i64;
    let liste = corps("/smart-collections");
    let fiche_jazz = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(jazz.0))
        .unwrap();
    assert!(
        (fiche_jazz["album_count"].as_i64().unwrap() - albums_jazz).abs() <= 1,
        "album_count de « {} » : {fiche_jazz}",
        jazz.1
    );
    let grille = corps(&format!("/smart-collections/{}/albums", jazz.0));
    assert_eq!(
        grille.as_array().unwrap().len() as i64,
        fiche_jazz["album_count"].as_i64().unwrap(),
        "la grille rend les albums que la liste annonce"
    );
    let cartes = corps(&format!("/albums-detailed?collection={nom}"));
    assert_eq!(cartes["total"], fiche_jazz["album_count"]);
    let rail = corps(&format!("/facets?collection={nom}"));
    let genres = rail["genre"].as_array().unwrap();
    assert_eq!(genres.len(), 1, "le rail ne voit que le jazz : {genres:?}");
    assert_eq!(
        genres[0]["count"], fiche_jazz["track_count"],
        "le rail compte les pistes que la liste annonce"
    );

    // Le flux.
    assert!(
        pire_silence < SEUIL_SILENCE,
        "le flux vers le renderer est resté muet {} ms pendant le clic sur une collection \
         intelligente : une lecture synchrone tient les {FILS_EXECUTEUR} fils de \
         l'exécuteur (#5438)",
        pire_silence.as_millis()
    );
}
