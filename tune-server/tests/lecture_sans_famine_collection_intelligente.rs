//! Coupure de lecture pendant le chargement d'une grosse collection
//! intelligente (Yves Corbat, 0.9.168, 30/09/2026, fil 2046, suite de #5438).
//!
//! Le terrain : 109 004 pistes, MacBook M1 (huit fils pour `#[tokio::main]`),
//! renderer DLNA qui TIRE le fichier par `GET /stream/{id}` (voie fichier de
//! `tune-stream-http`). Le chien de garde a vu l'exécuteur figé 12 s, et le
//! renderer a lâché le flux puis l'a repris par `Range`.
//!
//! Le banc joue la VRAIE voie de lecture DLNA : une session fichier créée par
//! `AudioStreamer::create_file_session` (celle d'une piste locale), servie par
//! le vrai routeur sur une vraie socket, tirée par un « renderer » sur un fil
//! système à part, à cadence régulière, qui reprend par `Range` à chaque fin
//! de corps — comme le darTZeel. Pendant ce temps, la rafale du clic sur une
//! collection intelligente (celle du client web de la 0.9.168) est jouée
//! [`RAFALES`] fois sur une base SQLite fichier de [`PISTES`] pistes.
//!
//! Trois mesures :
//! - le plus long silence du flux vu par le renderer ;
//! - le plus grand retard d'une minuterie de l'exécuteur (ce que regarde le
//!   chien de garde `gel_executeur`) ;
//! - l'attente d'une lecture triviale de la base (imprimée, pas affirmée).
//!
//! Les tampons des sockets sont bornés à 64 Kio des deux côtés : sans cela,
//! plusieurs Mio de tampon noyau masqueraient une famine de plusieurs
//! secondes. C'est un réglage du banc, pas du produit.
//!
//! `BANC_FILS` (défaut 2) règle le nombre de fils de l'exécuteur ;
//! `BANC_PISTES` la taille de la base ; `BANC_SEUL=<motif>` ne joue que les
//! routes qui le contiennent.
//!
//! Mesuré sur Shrek le 30/09/2026 (109 004 pistes, pire silence du flux) :
//!
//! | build   | fils | 0.9.168        | rc/v0.9.169   |
//! |---------|------|----------------|---------------|
//! | test    | 2    | 6 706 ms       | 262 à 542 ms  |
//! | test    | 4    | 410 ms         | 50 ms         |
//! | test    | 8    | 57 à 399 ms    | 51 à 390 ms   |
//! | release | 2    | 2 535, 3 001 ms| 142, 191 ms   |
//! | release | 8    | 37, 38 ms      | 23, 34 ms     |
//!
//! Le témoin ne DÉPARTAGE qu'avec un petit exécuteur : à huit fils, la 0.9.168
//! elle-même ne prive pas le flux sur ce banc. Il ne reproduit donc PAS le gel
//! de 12 s vu chez le testeur, qui avait huit fils. Le reste de la 0.9.169 à
//! deux fils (~0,2 s en release, ~0,3 à 0,5 s en build de test) vient des
//! `/smart-collections/{id}/albums` : la réponse JSON est sérialisée sur
//! l'exécuteur.
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

/// Pistes du testeur au rapport du 30/09/2026.
const PISTES: i64 = 109_004;
const PISTES_PAR_ALBUM: i64 = 12;
const ARTISTES: i64 = 1_635;
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

/// Rafales du clic jouées l'une après l'autre (le testeur a ouvert plusieurs
/// raccourcis de suite).
const RAFALES: usize = 3;

/// Le renderer lit 32 Kio toutes les 4 ms, soit 8 Mio/s : bien plus qu'un
/// DSD128, pour que les tampons (64 Kio) ne masquent pas plus de ~25 ms.
const BLOC_RENDERER: usize = 32 * 1024;
const CADENCE_RENDERER: Duration = Duration::from_millis(4);

/// Taille du fichier servi ; le renderer le reprend par `Range` en boucle.
const TAILLE_FICHIER: usize = 48 * 1024 * 1024;

/// Silence admis : une seconde. Un renderer DLNA tient plusieurs secondes de
/// tampon ; la 0.9.168 en prive le flux de 2,5 à 6,7 s à deux fils, la
/// 0.9.169 de 0,5 s au pire en build de test (voir le tableau plus haut).
const SEUIL_SILENCE: Duration = Duration::from_secs(1);
/// Retard admis d'une minuterie de l'exécuteur, même raison.
const SEUIL_RETARD: Duration = Duration::from_secs(1);

fn env_ou(nom: &str, defaut: usize) -> usize {
    std::env::var(nom)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(defaut)
}

/// La base au profil du testeur, générée par SQLite lui-même (CTE
/// récursives) : 109 004 `INSERT` textuels prenaient un quart d'heure en
/// build de test.
fn remplir(state: &AppState, pistes: i64) -> i64 {
    let albums = (pistes + PISTES_PAR_ALBUM - 1) / PISTES_PAR_ALBUM;
    let genre_de = |al: &str| {
        let mut cas = String::from("CASE (");
        cas.push_str(al);
        cas.push_str(&format!(") % {}", GENRES.len()));
        for (i, g) in GENRES.iter().enumerate() {
            cas.push_str(&format!(" WHEN {i} THEN '{g}'"));
        }
        cas.push_str(" END");
        cas
    };
    let al = format!("((n.i - 1) / {PISTES_PAR_ALBUM} + 1)");
    let sql = format!(
        "BEGIN;
         INSERT INTO artists (id, name)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {ARTISTES})
           SELECT i, 'Artiste ' || i FROM n;
         INSERT INTO albums (id, title, artist_id, source, genre, year, cover_path)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {albums})
           SELECT i, 'Album ' || i, i % {ARTISTES} + 1, 'local', {genre_album},
                  1950 + i % 70, '/pochettes/' || i || '.jpg' FROM n;
         INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number,
                             file_path, format, sample_rate, bit_depth, source, album_artist,
                             genre, year, label, duration_ms)
           WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {pistes})
           SELECT n.i, 'Piste ' || ((n.i - 1) % {PISTES_PAR_ALBUM} + 1), {al},
                  {al} % {ARTISTES} + 1, 1, (n.i - 1) % {PISTES_PAR_ALBUM} + 1,
                  '/musique/' || n.i || CASE WHEN n.i % 7 = 0 THEN '.dsf' ELSE '.flac' END,
                  CASE WHEN n.i % 7 = 0 THEN 'dsf' ELSE 'flac' END,
                  CASE WHEN n.i % 7 = 0 THEN 2822400 WHEN n.i % 3 = 0 THEN 192000 ELSE 44100 END,
                  24, 'local', 'Artiste ' || ({al} % {ARTISTES} + 1), {genre_piste},
                  1950 + {al} % 70, 'Label ' || ({al} % 150), 240000
           FROM n;
         COMMIT;",
        genre_album = genre_de("i"),
        genre_piste = genre_de(&al),
    );
    state.backend.execute_batch(&sql).unwrap();
    let compte = state
        .backend
        .query_one("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    assert_eq!(compte, pistes, "la base du banc");
    albums
}

/// Un GET bloquant, `Connection: close` : statut seulement.
fn obtenir(addr: SocketAddr, chemin: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {chemin} HTTP/1.1\r\nHost: banc\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut brut = Vec::new();
    s.read_to_end(&mut brut).unwrap();
    let entete = String::from_utf8_lossy(&brut[..brut.len().min(64)]).to_string();
    entete
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

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

/// Une socket cliente à petit tampon de réception.
fn connecter_petit_tampon(addr: SocketAddr) -> TcpStream {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    s.set_recv_buffer_size(64 * 1024).unwrap();
    s.connect(&addr.into()).unwrap();
    s.into()
}

/// Le renderer : tire `/stream/{id}` à cadence fixe, reprend par `Range` à
/// chaque fin de corps. Rend l'instant de chaque lecture non vide.
fn renderer(addr: SocketAddr, stream_id: String, arret: Arc<AtomicBool>) -> Vec<Instant> {
    let mut arrivees = Vec::new();
    let mut position: usize = 0;
    let mut tampon = vec![0u8; BLOC_RENDERER];
    while !arret.load(Ordering::Relaxed) {
        let mut s = connecter_petit_tampon(addr);
        write!(
            s,
            "GET /stream/{stream_id} HTTP/1.1\r\nHost: banc\r\nUser-Agent: banc-renderer\r\n\
             Range: bytes={position}-\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        // En-tête.
        let mut entete = Vec::new();
        let mut octet = [0u8; 1];
        while !entete.ends_with(b"\r\n\r\n") {
            let n = s.read(&mut octet).unwrap();
            assert!(n > 0, "flux fermé avant la fin de l'en-tête");
            entete.push(octet[0]);
        }
        let texte = String::from_utf8_lossy(&entete);
        assert!(
            texte.starts_with("HTTP/1.1 206") || texte.starts_with("HTTP/1.1 200"),
            "réponse du flux : {texte}"
        );
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
fn la_lecture_dlna_ne_manque_pas_d_octets_pendant_une_grosse_collection_intelligente() {
    let fils = env_ou("BANC_FILS", 2);
    let pistes = env_ou("BANC_PISTES", PISTES as usize) as i64;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(fils)
        .enable_all()
        .build()
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let chemin_base = dossier.path().join("tune.db");
    let chemin_piste = dossier.path().join("piste.flac");
    std::fs::write(&chemin_piste, vec![0x5Au8; TAILLE_FICHIER]).unwrap();

    let retard_max_us = Arc::new(AtomicU64::new(0));
    let mesurer_retard = Arc::new(AtomicBool::new(false));

    let (addr, stream_id, collections, backend, albums) = rt.block_on(async {
        let state = AppState::new(chemin_base.to_str().unwrap(), 0, Default::default()).unwrap();
        let t0 = Instant::now();
        let albums = remplir(&state, pistes);
        eprintln!(
            "banc coupure : base remplie en {:.1} s",
            t0.elapsed().as_secs_f64()
        );
        let collections: Vec<(i64, String)> = state
            .backend
            .query_many("SELECT id, name FROM smart_collections ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|r| (r[0].as_i64().unwrap(), r[1].as_string().unwrap()))
            .collect();
        // La session d'une piste locale servie telle quelle au renderer DLNA.
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 192_000,
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
        // Tampon d'émission borné : les sockets acceptées l'héritent.
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
    assert!(
        collections.len() >= 8,
        "collections semées : {collections:?}"
    );
    let jazz = collections
        .iter()
        .find(|(_, n)| n.contains("Jazz"))
        .expect("collection semée « Jazz »")
        .clone();

    // La minuterie témoin : ce que regarde `gel_executeur`, à 5 ms près.
    rt.spawn({
        let (max, actif) = (retard_max_us.clone(), mesurer_retard.clone());
        async move {
            loop {
                let t0 = Instant::now();
                tokio::time::sleep(Duration::from_millis(5)).await;
                let retard = t0.elapsed().saturating_sub(Duration::from_millis(5));
                if actif.load(Ordering::Relaxed) {
                    max.fetch_max(retard.as_micros() as u64, Ordering::Relaxed);
                }
            }
        }
    });

    let arret = Arc::new(AtomicBool::new(false));
    let lecteur = std::thread::spawn({
        let (arret, id) = (arret.clone(), stream_id.clone());
        move || renderer(addr, id, arret)
    });
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
    // Le flux prend son rythme.
    std::thread::sleep(Duration::from_millis(500));

    let nom = encoder(&jazz.1);
    let mut chemins = vec![
        "/api/v1/library/collections".to_string(),
        "/api/v1/library/smart-collections".to_string(),
    ];
    for (id, _) in &collections {
        chemins.push(format!("/api/v1/library/smart-collections/{id}/albums"));
    }
    chemins.push(format!("/api/v1/library/facets?collection={nom}"));
    chemins.push(format!("/api/v1/library/albums-detailed?collection={nom}"));
    chemins.push(format!("/api/v1/library/folder-facet?collection={nom}"));
    chemins.push(format!(
        "/api/v1/library/tracks?collection={nom}&limit=2000"
    ));

    // `BANC_SEUL=<motif>` : ne jouer que les routes qui le contiennent, pour
    // attribuer un retard à une route.
    if let Ok(motif) = std::env::var("BANC_SEUL") {
        chemins.retain(|c| c.contains(&motif));
        assert!(
            !chemins.is_empty(),
            "BANC_SEUL={motif} ne garde aucune route"
        );
    }

    mesurer_retard.store(true, Ordering::Relaxed);
    let debut = Instant::now();
    let mut statuts = Vec::new();
    for _ in 0..RAFALES {
        let requetes: Vec<_> = chemins
            .iter()
            .cloned()
            .map(|c| std::thread::spawn(move || (obtenir(addr, &c), c)))
            .collect();
        statuts.extend(requetes.into_iter().map(|r| r.join().unwrap()));
    }
    let fin = Instant::now();
    mesurer_retard.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    arret.store(true, Ordering::Relaxed);
    let arrivees = lecteur.join().unwrap();
    let pire_lecture_triviale = sonde.join().unwrap();

    let pire_silence = arrivees
        .windows(2)
        .filter(|w| w[1] >= debut && w[0] <= fin)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    let retard = Duration::from_micros(retard_max_us.load(Ordering::Relaxed));
    eprintln!(
        "banc coupure : {} version {} ; {fils} fils ; {pistes} pistes, {albums} albums ; \
         {RAFALES} rafales de {} requêtes en {:.0} ms ; pire silence du flux {:.0} ms ; \
         pire retard de minuterie {:.0} ms ; pire attente d'une lecture triviale {:.0} ms ; \
         {} lectures du renderer",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        chemins.len(),
        (fin - debut).as_secs_f64() * 1e3,
        pire_silence.as_secs_f64() * 1e3,
        retard.as_secs_f64() * 1e3,
        pire_lecture_triviale.as_secs_f64() * 1e3,
        arrivees.len(),
    );
    for (statut, c) in &statuts {
        assert_eq!(*statut, 200, "{c}");
    }
    assert!(
        pire_silence < SEUIL_SILENCE,
        "le renderer DLNA n'a rien reçu pendant {} ms pendant le chargement d'une grosse \
         collection intelligente ({fils} fils, {pistes} pistes)",
        pire_silence.as_millis()
    );
    assert!(
        retard < SEUIL_RETARD,
        "une minuterie de l'exécuteur a pris {} ms de retard pendant le chargement d'une \
         grosse collection intelligente ({fils} fils, {pistes} pistes)",
        retard.as_millis()
    );
}
