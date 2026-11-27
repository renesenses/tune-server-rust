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
//! grand silence du flux ne doit pas dépasser [`SEUIL_SILENCE`], et une tâche
//! soumise à l'exécuteur ne doit pas attendre plus de [`SEUIL_EXECUTEUR`] un
//! fil libre (la sonde de l'exécuteur, voir sa note).
//!
//! ⚠️ Le routeur est servi COMME EN PRODUCTION
//! (`into_make_service_with_connect_info`, `bootstrap.rs`). Servi par
//! `axum::serve(ecoute, app)`, axum reconstruit le routeur ENTIER à chaque
//! connexion (`Router::with_state(())` sur toutes ses routes) : 27 ms par connexion
//! en profil de test, contre 0,4 ms servi comme en production (mesuré le
//! 02/10/2026 sur Shrek). Les 22 requêtes du clic ouvrent chacune leur
//! connexion : ce coût, que la production ne paie pas, faisait à lui seul
//! l'essentiel du silence mesuré (médiane 262 ms, 24 clics sur 100 au-delà de
//! 300 ms, sans charge ajoutée) — d'où les rouges intermittents de #5438.
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
///
/// ⚠️ 600 ms et non 300 : le test tourne en profil de test (`opt-level = 0`),
/// où sérialiser la réponse de `/library/tracks?limit=2000` (1,5 Mo de JSON)
/// prend 100 à 250 ms d'UN fil de l'exécuteur — une seule scrutation, que
/// Tokio ne coupe pas — et le flux peut attendre derrière elle dans la file de
/// ce fil. Mesuré le 02/10/2026 sur Shrek, code correct : pire silence
/// 290 ms sous charge (8 boucles `yes`, charge 16 à 29), 375 ms sans charge
/// ajoutée, sur 100 clics ; lecture synchrone réintroduite : 471 à 1 468 ms.
/// La garde fine est [`SEUIL_EXECUTEUR`].
const SEUIL_SILENCE: Duration = Duration::from_millis(600);

/// La plus longue attente admise, pendant le clic, d'une tâche soumise à
/// l'exécuteur depuis un fil système (la sonde). Tant qu'UN fil de
/// l'exécuteur est libre il la prend aussitôt : la sonde ne mesure que les
/// moments où une lecture synchrone tient TOUS les fils — le défaut de #5438 —,
/// pas la sérialisation d'une grosse réponse sur l'un d'eux. Mesuré le
/// 02/10/2026 sur Shrek, code correct : 40 ms au pire sur 85 clics sous charge ;
/// lecture synchrone réintroduite (`hors_executeur` exécuté sur place) : 196 et
/// 270 ms (2 exécutions rouges sur 3) ; un `std::thread::sleep(117 ms)` dans
/// le gestionnaire des albums : 233, 338 et 381 ms (3 sur 3).
const SEUIL_EXECUTEUR: Duration = Duration::from_millis(150);

/// Clics mesurés, une annonce `library.updated` avant chacun : les comptes de
/// la liste sont recalculés à chaque fois, comme au premier clic après un scan.
/// Une lecture synchrone ne tient pas les deux fils à CHAQUE clic : quand le
/// pool est plein, l'attente d'une connexion passe par `block_in_place`, qui
/// rend le cœur de l'exécuteur, et ce qui suit dans la même scrutation tourne
/// sur un fil qui ne le tient plus (voir [`FILS_EXECUTEUR`]). Lecture
/// synchrone réintroduite, d'un clic à l'autre : 183 ms puis 1 362 ms.
const CLICS: usize = 5;

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
            "INSERT INTO albums (id, title, artist_id, source, genre, year, cover_path) \
             VALUES ({al}, 'Album {al}', {}, 'local', '{genre}', {}, {});\n",
            al % ARTISTES + 1,
            1950 + al % 70,
            // Un album sur cinq sans pochette : la mosaïque doit les sauter.
            if al % 5 == 0 {
                "NULL".to_string()
            } else {
                format!("'/pochettes/{al}.jpg'")
            },
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

    let (addr, albums, collections, backend, bus) = rt.block_on(async {
        let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
        let albums = remplir_au_profil_du_testeur(&state);
        let collections: Vec<(i64, String)> = state
            .backend
            .query_many("SELECT id, name FROM smart_collections ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|r| (r[0].as_i64().unwrap(), r[1].as_string().unwrap()))
            .collect();
        // Un dossier manuel, trois de ses albums sans pochette (5, 10, 20).
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(
                "collections",
                r#"[{"id":1,"name":"Soirée","album_ids":[5,10,3,7,12,20,1,44,61]}]"#,
            )
            .unwrap();
        mesurer_la_resolution_seule(&state);
        let backend = state.backend.clone();
        let bus = state.event_bus.clone();
        let app = tune_server::routes::router(state).route("/banc/flux", get(flux));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ecoute.local_addr().unwrap();
        // Comme `bootstrap.rs` : le routeur est fini UNE fois, pas par connexion.
        tokio::spawn(async move {
            axum::serve(
                ecoute,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (addr, albums, collections, backend, bus)
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
    // La sonde de l'exécuteur : toutes les 5 ms, depuis un fil système, une
    // tâche vide confiée à l'exécuteur ; combien attend-elle qu'un fil la
    // prenne ? La file d'injection est vue par tout fil libre.
    let sonde_executeur = std::thread::spawn({
        let (arret, executeur) = (arret.clone(), rt.handle().clone());
        move || {
            let mut attentes = Vec::new();
            while !arret.load(Ordering::Relaxed) {
                let (fait, recu) = std::sync::mpsc::channel();
                let t0 = Instant::now();
                executeur.spawn(async move {
                    let _ = fait.send(());
                });
                recu.recv().unwrap();
                attentes.push((t0, t0.elapsed()));
                std::thread::sleep(CADENCE_FLUX);
            }
            attentes
        }
    });
    // Le témoin de l'ordonnanceur du SYSTÈME : un fil qui dort 5 ms en boucle.
    // S'il se réveille tard, c'est la machine qui manque de cœurs, pas le code.
    let temoin_systeme = std::thread::spawn({
        let arret = arret.clone();
        move || {
            let mut reveils = Vec::new();
            while !arret.load(Ordering::Relaxed) {
                std::thread::sleep(CADENCE_FLUX);
                reveils.push(Instant::now());
            }
            reveils
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

    let mut clics = Vec::new();
    let mut premiere_rafale = None;
    for clic in 0..CLICS {
        if clic > 0 {
            bus.emit_typed(
                tune_core::event_types::EventType::LibraryUpdated,
                serde_json::json!({ "source": "banc_5438" }),
            );
            std::thread::sleep(Duration::from_millis(100));
        }
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
        clics.push((debut, fin));
        premiere_rafale.get_or_insert((reponses, durees));
    }
    let (reponses, mut durees) = premiere_rafale.unwrap();
    std::thread::sleep(Duration::from_millis(50));
    arret.store(true, Ordering::Relaxed);
    let arrivees = renderer.join().unwrap();
    let pire_lecture_triviale = sonde.join().unwrap();
    let attentes = sonde_executeur.join().unwrap();
    let reveils = temoin_systeme.join().unwrap();

    // Le plus long écart entre deux instants PENDANT un clic.
    let pire_ecart = |instants: &[Instant], (debut, fin): (Instant, Instant)| {
        instants
            .windows(2)
            .filter(|w| w[1] >= debut && w[0] <= fin)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or_default()
    };
    let ms = |d: Duration| d.as_millis();
    let silences: Vec<Duration> = clics.iter().map(|c| pire_ecart(&arrivees, *c)).collect();
    let executeur: Vec<Duration> = clics
        .iter()
        .map(|(debut, fin)| {
            attentes
                .iter()
                .filter(|(t, _)| t >= debut && t <= fin)
                .map(|(_, attente)| *attente)
                .max()
                .unwrap_or_default()
        })
        .collect();
    // Retard du réveil d'un fil qui dort 5 ms : la part du système.
    let systeme: Vec<Duration> = clics
        .iter()
        .map(|c| pire_ecart(&reveils, *c).saturating_sub(CADENCE_FLUX))
        .collect();
    let pire_silence = silences.iter().copied().max().unwrap_or_default();
    let pire_attente_executeur = executeur.iter().copied().max().unwrap_or_default();
    let (debut, fin) = clics[0];
    eprintln!(
        "#5438 : {} requêtes par clic, premier clic en {:.0} ms ; sur {CLICS} clics, \
         pire silence du flux {:?} ms, pire attente d'un fil de l'exécuteur {:?} ms, \
         retard du système {:?} ms ({FILS_EXECUTEUR} fils d'exécuteur, {PISTES} pistes, \
         {albums} albums)",
        chemins.len(),
        (fin - debut).as_secs_f64() * 1e3,
        silences.iter().map(|d| ms(*d)).collect::<Vec<_>>(),
        executeur.iter().map(|d| ms(*d)).collect::<Vec<_>>(),
        systeme.iter().map(|d| ms(*d)).collect::<Vec<_>>(),
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

    // Le client d'aujourd'hui : la liste porte ses pochettes (#5438, suite).
    // L'écran n'a plus à demander `/{id}/albums` pour CHAQUE collection.
    ouvrir_l_ecran_une_seconde_fois(addr, &collections, liste, &backend, &bus, albums);

    // L'exécuteur, puis le flux.
    let retard_systeme = systeme.iter().copied().max().unwrap_or_default();
    assert!(
        pire_attente_executeur < SEUIL_EXECUTEUR,
        "une tâche a attendu {} ms un fil de l'exécuteur pendant le clic sur une collection \
         intelligente : une lecture synchrone tient les {FILS_EXECUTEUR} fils de \
         l'exécuteur (#5438) — retard du système au même moment : {} ms",
        pire_attente_executeur.as_millis(),
        retard_systeme.as_millis()
    );
    assert!(
        pire_silence < SEUIL_SILENCE,
        "le flux vers le renderer est resté muet {} ms pendant le clic sur une collection \
         intelligente (#5438) — retard du système au même moment : {} ms",
        pire_silence.as_millis(),
        retard_systeme.as_millis()
    );
}

/// `quatreDistinctes` du client web (`src/lib/mosaique.ts`), réécrite ici À
/// PART de celle du serveur : sinon le test comparerait une fonction à
/// elle-même.
fn quatre_distinctes_du_client(albums: &Value) -> Vec<String> {
    fn sans_suffixe(t: &str) -> &str {
        let mut t = t.trim();
        loop {
            let ouvrant = match t.chars().last() {
                Some(')') => '(',
                Some(']') => '[',
                _ => return t,
            };
            let Some(i) = t.rfind(ouvrant) else { return t };
            let reste = t[..i].trim_end();
            if reste.is_empty() {
                return t;
            }
            t = reste;
        }
    }
    let (mut vues, mut cles) = (Vec::<String>::new(), Vec::<String>::new());
    for a in albums.as_array().unwrap() {
        let Some(c) = a["cover_path"].as_str().filter(|c| !c.is_empty()) else {
            continue;
        };
        let base = sans_suffixe(a["title"].as_str().unwrap_or(""));
        let cle = if base.is_empty() { c } else { base }.to_lowercase();
        if cles.contains(&cle) || vues.iter().any(|v| v == c) {
            continue;
        }
        cles.push(cle);
        vues.push(c.to_string());
        if vues.len() == 4 {
            break;
        }
    }
    vues
}

/// Deuxième ouverture de l'écran des collections, par un client qui lit les
/// `covers` de la liste : DEUX requêtes au lieu de deux plus une par
/// collection, des comptes servis par le cache, et justes après un scan.
fn ouvrir_l_ecran_une_seconde_fois(
    addr: SocketAddr,
    collections: &[(i64, String)],
    premiere_liste: &Value,
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    bus: &tune_core::event_bus::EventBus,
    albums: i64,
) {
    // (a) Les pochettes de la liste sont celles que le client tirait de
    // `/{id}/albums` : même albums, même ordre, même règle.
    for (id, nom) in collections {
        let fiche = premiere_liste
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"].as_i64() == Some(*id))
            .unwrap();
        let (statut, grille) = obtenir(
            addr,
            &format!("/api/v1/library/smart-collections/{id}/albums"),
        );
        assert_eq!(statut, 200);
        let attendu: Vec<Value> = quatre_distinctes_du_client(&grille)
            .into_iter()
            .map(Value::from)
            .collect();
        assert_eq!(
            fiche["covers"],
            Value::Array(attendu),
            "« {nom} » : la liste doit porter les pochettes que l'écran composait lui-même"
        );
    }
    let (statut, dossiers) = obtenir(addr, "/api/v1/library/collections");
    assert_eq!(statut, 200);
    let dossiers = dossiers.as_array().unwrap();
    assert_eq!(dossiers.len(), 1, "{dossiers:?}");
    let (_, grille) = obtenir(addr, "/api/v1/library/collections/1/albums");
    let attendu: Vec<Value> = quatre_distinctes_du_client(&grille)
        .into_iter()
        .map(Value::from)
        .collect();
    assert_eq!(attendu.len(), 4, "le banc : quatre pochettes à trouver");
    assert_eq!(
        dossiers[0]["covers"],
        Value::Array(attendu),
        "le dossier manuel doit porter les pochettes que l'écran composait lui-même"
    );

    // (b) L'écran rouvert : deux requêtes, et les comptes viennent du cache.
    let t0 = Instant::now();
    let (s1, _) = obtenir(addr, "/api/v1/library/collections");
    let (s2, liste) = obtenir(addr, "/api/v1/library/smart-collections");
    let reouverture = t0.elapsed();
    assert_eq!((s1, s2), (200, 200));
    eprintln!(
        "#5438 : écran rouvert en 2 requêtes au lieu de {} : {:.0} ms",
        2 + collections.len() + dossiers.len(),
        reouverture.as_secs_f64() * 1e3
    );
    let jazz = |liste: &Value| -> (i64, i64) {
        let f = liste
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str().is_some_and(|n| n.contains("Jazz")))
            .unwrap()
            .clone();
        (
            f["album_count"].as_i64().unwrap(),
            f["track_count"].as_i64().unwrap(),
        )
    };
    let avant = jazz(&liste);
    assert_eq!(
        avant,
        jazz(premiere_liste),
        "le cache rend ce qui a été compté"
    );

    // (c) Un scan ajoute un album de jazz de douze pistes. Tant que rien n'est
    // annoncé, le cache garde l'ancien compte — c'est lui qu'on lit.
    let al = albums + 1;
    let mut sql = format!(
        "BEGIN; INSERT INTO albums (id, title, artist_id, source, genre, year, cover_path) \
         VALUES ({al}, 'Album {al}', 1, 'local', 'Jazz', 2026, '/pochettes/{al}.jpg');"
    );
    for n in 1..=12 {
        let id = PISTES + n;
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, genre, duration_ms) \
             VALUES ({id}, 'Nouvelle {n}', {al}, 1, 1, {n}, '/musique/nouveau/{n}.flac', \
             'flac', 44100, 24, 'local', 'Jazz', 240000);"
        ));
    }
    sql.push_str("COMMIT;");
    backend.execute_batch(&sql).unwrap();
    let (_, liste) = obtenir(addr, "/api/v1/library/smart-collections");
    assert_eq!(
        jazz(&liste),
        avant,
        "sans annonce, le compte vient du cache"
    );

    // Le scan l'annonce comme `auto_scan` le fait en fin de lot.
    bus.emit_typed(
        tune_core::event_types::EventType::LibraryUpdated,
        serde_json::json!({ "source": "banc_5438" }),
    );
    let (_, liste) = obtenir(addr, "/api/v1/library/smart-collections");
    assert_eq!(
        jazz(&liste),
        (avant.0 + 1, avant.1 + 12),
        "après `library.updated`, la liste doit compter l'album scanné : un compte \
         resté en cache après un scan (#5438)"
    );
}
