//! #5677 — banc des blocages de Tune OS (Belkadi Yacine, tickets 223 et 224).
//!
//! Le terrain : Tune OS sur une machine qui ne voit qu'UN processeur
//! (`cpu_count: 1` dans le diagnostic), 51 159 pistes, SQLite, sortie locale
//! ALSA qui lit son flux en HTTP auprès du serveur lui-même. En 1.0.0-rc1,
//! `#[tokio::main]` y créait un seul fil de travail ; le chien de garde a vu
//! l'exécuteur figé 12 à 24 s, avec 5 à 12 s de silence au DAC.
//!
//! Ce banc n'est PAS un témoin de correctif : c'est une mesure, `#[ignore]`,
//! à lancer à la main, de préférence sous `taskset -c 0` pour n'avoir qu'un
//! processeur comme le testeur :
//!
//! ```text
//! TOKIO_WORKER_THREADS=1 taskset -c 0 cargo test -p tune-server \
//!   --test blocages_tuneos_5677 -- --ignored --nocapture
//! ```
//!
//! Le moteur est celui de la production
//! ([`tune_server::fils_de_travail::construire_le_moteur`] : plancher de 4
//! fils, `TOKIO_WORKER_THREADS` respectée), avec ses crochets de garage, et
//! la veille [`tune_server::gel_executeur`] tourne avec un seuil de
//! `BANC_SEUIL_MS` (défaut 2 000 ms) : le premier relevé est imprimé, ce qui
//! montre aussi ce que le relevé de #5677 dit d'un gel.
//!
//! Pendant `BANC_DUREE_S` secondes (défaut 30), en même temps :
//! - **lecture** : un « renderer » tire `/stream/{id}` à 8 Mio/s (voie fichier,
//!   comme la sortie locale qui lit son propre serveur) ;
//! - **navigation** : Répertoires, facettes, pages de pistes de 2 000,
//!   collections intelligentes, en boucle ;
//! - **scan** : des lots d'écriture de 500 lignes sur la connexion d'écriture
//!   (`write_tx`), comme les lots du scan ;
//! - **analyse** : `BANC_BRULEURS` fils (défaut 1) qui brûlent du processeur,
//!   comme l'analyse acoustique et les empreintes en fond.
//!
//! Mesures : pire retard d'une minuterie de 5 ms de l'exécuteur (ce que
//! regarde le chien de garde), pire silence du flux, nombre de relevés.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tune_core::http::streamer::StreamInfo;
use tune_server::state::AppState;

const PISTES_PAR_ALBUM: i64 = 12;
const ARTISTES: i64 = 2_196;
const BLOC_RENDERER: usize = 32 * 1024;
const CADENCE_RENDERER: Duration = Duration::from_millis(4);
const TAILLE_FICHIER: usize = 48 * 1024 * 1024;

fn env_ou(nom: &str, defaut: u64) -> u64 {
    std::env::var(nom)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(defaut)
}

/// La base : `pistes` pistes rangées en `<musique>/Artiste A/Album B/NN.flac`,
/// générées par SQLite (CTE récursives). Les dossiers existent sur le disque
/// (Répertoires les vérifie), pas les fichiers.
fn remplir(state: &AppState, musique: &std::path::Path, pistes: i64) -> i64 {
    let albums = (pistes + PISTES_PAR_ALBUM - 1) / PISTES_PAR_ALBUM;
    let m = musique.to_str().unwrap();
    let al = format!("((n.i - 1) / {PISTES_PAR_ALBUM} + 1)");
    let ar = format!("({al} % {ARTISTES} + 1)");
    let sql = format!(
        "BEGIN;
         INSERT INTO artists (id, name)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {ARTISTES})
           SELECT i, 'Artiste ' || i FROM n;
         INSERT INTO albums (id, title, artist_id, source, genre, year)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {albums})
           SELECT i, 'Album ' || i, i % {ARTISTES} + 1, 'local',
                  CASE i % 4 WHEN 0 THEN 'Jazz' WHEN 1 THEN 'Rock' WHEN 2 THEN 'Classical' ELSE 'Pop' END,
                  1950 + i % 70 FROM n;
         INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number,
                             file_path, format, sample_rate, bit_depth, source, album_artist,
                             genre, year, label, duration_ms)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {pistes})
           SELECT n.i, 'Piste ' || n.i, {al}, {ar}, 1, (n.i - 1) % {PISTES_PAR_ALBUM} + 1,
                  '{m}/Artiste ' || {ar} || '/Album ' || {al} || '/' || ((n.i - 1) % {PISTES_PAR_ALBUM} + 1) || '.flac',
                  'flac', CASE WHEN n.i % 3 = 0 THEN 96000 ELSE 44100 END, 24, 'local',
                  'Artiste ' || {ar},
                  CASE {al} % 4 WHEN 0 THEN 'Jazz' WHEN 1 THEN 'Rock' WHEN 2 THEN 'Classical' ELSE 'Pop' END,
                  1950 + {al} % 70, 'Label ' || ({al} % 150), 240000
           FROM n;
         CREATE TABLE IF NOT EXISTS banc_lots_5677 (id INTEGER PRIMARY KEY, v TEXT);
         COMMIT;"
    );
    state.backend.execute_batch(&sql).unwrap();
    // Les dossiers d'artistes et d'albums, pour Répertoires.
    for a in 1..=ARTISTES.min(200) {
        std::fs::create_dir_all(musique.join(format!("Artiste {a}"))).unwrap();
    }
    for al in 1..=albums.min(400) {
        let a = al % ARTISTES + 1;
        std::fs::create_dir_all(musique.join(format!("Artiste {a}/Album {al}"))).unwrap();
    }
    albums
}

fn obtenir(addr: SocketAddr, chemin: &str) -> u16 {
    let Ok(mut s) = TcpStream::connect(addr) else {
        return 0;
    };
    let _ = write!(
        s,
        "GET {chemin} HTTP/1.1\r\nHost: banc\r\nConnection: close\r\n\r\n"
    );
    let mut brut = Vec::new();
    let _ = s.read_to_end(&mut brut);
    String::from_utf8_lossy(&brut[..brut.len().min(64)])
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn encoder(nom: &str) -> String {
    nom.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'/' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn renderer(addr: SocketAddr, stream_id: String, arret: Arc<AtomicBool>) -> Vec<Instant> {
    let mut arrivees = Vec::new();
    let mut position: usize = 0;
    let mut tampon = vec![0u8; BLOC_RENDERER];
    while !arret.load(Ordering::Relaxed) {
        let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        s.set_recv_buffer_size(64 * 1024).unwrap();
        s.connect(&addr.into()).unwrap();
        let mut s: TcpStream = s.into();
        write!(
            s,
            "GET /stream/{stream_id} HTTP/1.1\r\nHost: banc\r\nRange: bytes={position}-\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut entete = Vec::new();
        let mut octet = [0u8; 1];
        while !entete.ends_with(b"\r\n\r\n") {
            if s.read(&mut octet).unwrap_or(0) == 0 {
                break;
            }
            entete.push(octet[0]);
        }
        arrivees.push(Instant::now());
        loop {
            if arret.load(Ordering::Relaxed) {
                return arrivees;
            }
            let n = s.read(&mut tampon).unwrap_or(0);
            if n == 0 {
                break;
            }
            arrivees.push(Instant::now());
            position = (position + n) % TAILLE_FICHIER;
            std::thread::sleep(CADENCE_RENDERER);
        }
    }
    arrivees
}

#[test]
#[ignore = "banc de mesure #5677 — à lancer à la main, sous taskset -c 0"]
fn banc_blocages_tuneos_5677() {
    let pistes = env_ou("BANC_PISTES", 100_000) as i64;
    let duree = Duration::from_secs(env_ou("BANC_DUREE_S", 30));
    let bruleurs = env_ou("BANC_BRULEURS", 1);
    let seuil = Duration::from_millis(env_ou("BANC_SEUIL_MS", 2_000));
    let rt = tune_server::fils_de_travail::construire_le_moteur();
    let fils = tune_server::fils_de_travail::retenu();
    let dossier = tempfile::tempdir().unwrap();
    let musique = dossier.path().join("musique");
    std::fs::create_dir_all(&musique).unwrap();
    let releves = dossier.path().join("gels");
    let chemin_base = dossier.path().join("tune.db");
    let chemin_piste = dossier.path().join("piste.flac");
    std::fs::write(&chemin_piste, vec![0x5Au8; TAILLE_FICHIER]).unwrap();

    let (addr, stream_id, collections, backend, albums) = rt.block_on(async {
        let state = AppState::new(chemin_base.to_str().unwrap(), 0, Default::default()).unwrap();
        let t0 = Instant::now();
        let albums = remplir(&state, &musique, pistes);
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&vec![musique.to_str().unwrap()]).unwrap(),
            )
            .unwrap();
        eprintln!(
            "banc 5677 : base remplie en {:.1} s",
            t0.elapsed().as_secs_f64()
        );
        let collections: Vec<i64> = state
            .backend
            .query_many("SELECT id FROM smart_collections ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 96_000,
            bit_depth: 24,
            channels: 2,
            file_size: Some(TAILLE_FICHIER as u64),
            duration_ms: Some(240_000),
            ..StreamInfo::default()
        };
        let stream_id = state
            .streamer
            .create_file_session(info, chemin_piste.to_str().unwrap().to_string(), true)
            .await;
        let backend = state.backend.clone();
        let app = tune_server::routes::router(state);
        let brute =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        brute.set_send_buffer_size(64 * 1024).unwrap();
        brute.set_reuse_address(true).unwrap();
        brute
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        brute.listen(128).unwrap();
        brute.set_nonblocking(true).unwrap();
        let ecoute = tokio::net::TcpListener::from_std(brute.into()).unwrap();
        let addr = ecoute.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        (addr, stream_id, collections, backend, albums)
    });

    let _veille = tune_server::gel_executeur::Veille::demarrer(
        rt.handle(),
        seuil,
        Duration::from_millis(200),
        releves.clone(),
    );

    // Minuterie témoin de 5 ms : pire retard et histogramme.
    let retard_max_us = Arc::new(AtomicU64::new(0));
    let au_dela: Arc<[AtomicU64; 4]> = Arc::new(Default::default());
    rt.spawn({
        let (max, h) = (retard_max_us.clone(), au_dela.clone());
        async move {
            loop {
                let t0 = Instant::now();
                tokio::time::sleep(Duration::from_millis(5)).await;
                let r = t0.elapsed().saturating_sub(Duration::from_millis(5));
                max.fetch_max(r.as_micros() as u64, Ordering::Relaxed);
                for (i, borne) in [100u64, 500, 1_000, 5_000].iter().enumerate() {
                    if r >= Duration::from_millis(*borne) {
                        h[i].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });

    let arret = Arc::new(AtomicBool::new(false));
    let lecteur = std::thread::spawn({
        let (arret, id) = (arret.clone(), stream_id.clone());
        move || renderer(addr, id, arret)
    });
    let navigation = std::thread::spawn({
        let arret = arret.clone();
        let m = encoder(musique.to_str().unwrap());
        move || {
            let mut chemins = vec![
                "/api/v1/library/browse".to_string(),
                format!("/api/v1/library/browse/dir?path={m}"),
                format!("/api/v1/library/browse/dir?path={m}/Artiste%201"),
                "/api/v1/library/facets".to_string(),
                "/api/v1/library/smart-collections".to_string(),
            ];
            for id in collections.iter().take(4) {
                chemins.push(format!("/api/v1/library/smart-collections/{id}/albums"));
            }
            for k in 0..5 {
                chemins.push(format!(
                    "/api/v1/library/tracks?limit=2000&offset={}",
                    k * 2000
                ));
            }
            let mut statuts = std::collections::BTreeMap::<u16, u64>::new();
            while !arret.load(Ordering::Relaxed) {
                let lots: Vec<_> = chemins
                    .iter()
                    .cloned()
                    .map(|c| std::thread::spawn(move || obtenir(addr, &c)))
                    .collect();
                for l in lots {
                    *statuts.entry(l.join().unwrap()).or_default() += 1;
                }
            }
            statuts
        }
    });
    let scan = std::thread::spawn({
        let (arret, backend) = (arret.clone(), backend.clone());
        move || {
            let (mut lots, mut pire) = (0u64, Duration::ZERO);
            let mut id = 0i64;
            while !arret.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                backend
                    .write_tx(&mut |tx| {
                        for _ in 0..500 {
                            id += 1;
                            tx.execute(
                                "INSERT OR REPLACE INTO banc_lots_5677 (id, v) VALUES (?1, ?2)",
                                &[&(id % 50_000), &"x".repeat(200)],
                            )?;
                        }
                        Ok(())
                    })
                    .unwrap();
                pire = pire.max(t0.elapsed());
                lots += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            (lots, pire)
        }
    });
    let bruleurs: Vec<_> = (0..bruleurs)
        .map(|_| {
            let arret = arret.clone();
            std::thread::spawn(move || {
                let mut x = 0u64;
                while !arret.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = std::hint::black_box(
                            x.wrapping_mul(6364136223846793005).wrapping_add(1),
                        );
                    }
                }
            })
        })
        .collect();

    let debut = Instant::now();
    std::thread::sleep(duree);
    let fin = Instant::now();
    arret.store(true, Ordering::Relaxed);
    let arrivees = lecteur.join().unwrap();
    let statuts = navigation.join().unwrap();
    let (lots, pire_lot) = scan.join().unwrap();
    for b in bruleurs {
        b.join().unwrap();
    }
    let pire_silence = arrivees
        .windows(2)
        .filter(|w| w[1] >= debut && w[0] <= fin)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    let retard = Duration::from_micros(retard_max_us.load(Ordering::Relaxed));
    let h: Vec<u64> = au_dela.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let (premiers, total) = tune_server::gel_executeur::derniers_releves(&releves, 50);
    eprintln!(
        "banc 5677 : {fils} fils de travail, {} processeur(s) vus ; {pistes} pistes, {albums} albums ; \
         {:.0} s ; pire retard de minuterie {:.0} ms (≥100 ms : {}, ≥500 ms : {}, ≥1 s : {}, ≥5 s : {}) ; \
         pire silence du flux {:.0} ms ; {lots} lots d'écriture, pire {:.0} ms ; réponses {statuts:?} ; \
         {total} relevé(s) de gel au seuil de {} ms",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        (fin - debut).as_secs_f64(),
        retard.as_secs_f64() * 1e3,
        h[0],
        h[1],
        h[2],
        h[3],
        pire_silence.as_secs_f64() * 1e3,
        pire_lot.as_secs_f64() * 1e3,
        seuil.as_millis(),
    );
    if let Some((nom, texte)) = premiers.last() {
        eprintln!("--- premier relevé : {nom}\n{texte}");
    }
}
