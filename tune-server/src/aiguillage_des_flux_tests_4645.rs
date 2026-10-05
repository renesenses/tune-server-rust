//! 🔴 #4645 — un faux darTZeel face à un exécuteur principal gelé.
//!
//! Le faux renderer suit ce que les journaux d'Yves montrent du LHC-208 :
//!
//! - il tire le WAV à un rythme fixé, sans grande avance ;
//! - il ne patiente pas : une lecture qui ne rend rien pendant
//!   [`PATIENCE_DU_RENDERER`] lui fait refermer la connexion (le vrai le fait
//!   au bout d'une vingtaine de secondes, fils 1871, 1892, 1935 ; l'échelle
//!   est réduite ici pour que le test dure quelques secondes) ;
//! - il reprend par `Range: bytes=N-` (fil 1934 : « il s'est interrompu, a
//!   redemandé la suite, puis est reparti »).
//!
//! Pendant qu'il lit, deux requêtes d'API bloquent CHACUNE un fil de
//! l'exécuteur principal (qui n'en a que deux) avec un `std::thread::sleep` :
//! c'est la forme d'une lecture SQLite synchrone dans un gestionnaire async
//! (#5438). L'exécuteur principal est alors entièrement gelé pendant [`GEL`].
//!
//! Le flux doit continuer, et la reprise demandée en plein gel doit recevoir
//! son `206`. Avant le correctif, le serveur entier (connexions comprises)
//! tournait sur l'exécuteur gelé : le faux renderer n'avait plus rien pendant
//! tout le gel et refermait la connexion — ce que le darTZeel d'Yves a fait
//! pendant le gel de 12 s du fil 2046.
//!
//! Tout le pilotage du test se fait sur des `std::thread` et des sockets
//! bloquantes : rien de ce qui mesure ne dépend d'un exécuteur tokio.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::routing::get;

use super::aiguillage_des_flux::{est_un_flux_audio, servir_sur};

/// Fils de l'exécuteur principal du test ; autant de requêtes bloquantes le
/// gèlent en entier.
const FILS_PRINCIPAUX: usize = 2;

/// Durée du gel imposé à l'exécuteur principal.
const GEL: Duration = Duration::from_secs(6);

/// Silence au-delà duquel le faux renderer referme la connexion.
const PATIENCE_DU_RENDERER: Duration = Duration::from_secs(3);

/// Rythme de lecture du faux renderer. Assez haut pour que les tampons du
/// noyau (quelques Mio sur la boucle locale) ne couvrent qu'une fraction du
/// gel : ce sont eux qui, sinon, masqueraient la famine.
const OCTETS_PAR_SECONDE: f64 = 16.0 * 1024.0 * 1024.0;

/// Taille du WAV servi : de quoi lire au rythme ci-dessus au-delà de la fin du
/// gel. Fichier creux : rien n'est écrit sur le disque au-delà de l'en-tête.
const TAILLE_DU_WAV: u64 = 160 * 1024 * 1024;

const ID_DU_FLUX: &str = "flux-gel-4645";

fn moteur(nom: &'static str, fils: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(fils)
        .thread_name(nom)
        .enable_all()
        .build()
        .unwrap()
}

/// Un WAV creux de [`TAILLE_DU_WAV`] octets, en-tête compris.
fn wav_creux() -> tune_core::test_scratch::ScratchFile {
    // Unique par appel et supprimé par `Drop`, panique comprise.
    let chemin = tune_core::test_scratch::scratch_file("tune-4645-flux", ".wav");
    let mut f = std::fs::File::create(&*chemin).unwrap();
    f.write_all(&tune_core::audio::wav::build_wav_header(2, 44_100, 16))
        .unwrap();
    f.set_len(TAILLE_DU_WAV).unwrap();
    chemin
}

/// Le routeur du test : le VRAI routeur des flux de `tune_stream_http`, plus
/// une route d'API qui bloque son fil, et une qui dit où elle a tourné.
fn routeur(
    sessions: tune_core::http::streamer::SharedSessions,
    entrees: Arc<AtomicUsize>,
) -> axum::Router {
    tune_stream_http::router(sessions)
        .route(
            "/api/v1/bloque",
            get(move || {
                let entrees = entrees.clone();
                async move {
                    entrees.fetch_add(1, SeqCst);
                    // Un appel synchrone dans un gestionnaire async : le fil
                    // de l'exécuteur est pris, rien d'autre n'y tourne.
                    std::thread::sleep(GEL);
                    "fini"
                }
            }),
        )
        .route(
            "/api/v1/ou",
            get(|ConnectInfo(client): ConnectInfo<SocketAddr>| async move {
                format!(
                    "{}|{}",
                    std::thread::current().name().unwrap_or("?"),
                    client.ip()
                )
            }),
        )
}

struct Serveur {
    adresse: SocketAddr,
    entrees: Arc<AtomicUsize>,
    // Gardés vivants jusqu'à la fin du test.
    _principal: tokio::runtime::Runtime,
    _transport: tokio::runtime::Runtime,
    _wav: tune_core::test_scratch::ScratchFile,
}

fn demarrer() -> Serveur {
    let principal = moteur("principal-4645", FILS_PRINCIPAUX);
    let transport = moteur("transport-4645", 2);
    let wav = wav_creux();
    let entrees = Arc::new(AtomicUsize::new(0));
    let adresse = principal.block_on(async {
        let info = tune_core::http::streamer::StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let session = Arc::new(tune_core::http::streamer::StreamSession::new(
            ID_DU_FLUX.into(),
            info,
            false,
            4,
        ));
        *session.file_path.lock().await = Some(wav.to_string_lossy().into_owned());
        let sessions: tune_core::http::streamer::SharedSessions = Arc::new(
            tokio::sync::Mutex::new([(ID_DU_FLUX.to_string(), session)].into_iter().collect()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = listener.local_addr().unwrap();
        let app = routeur(sessions, entrees.clone());
        let transport = transport.handle().clone();
        let principal = tokio::runtime::Handle::current();
        tokio::spawn(servir_sur(
            transport,
            principal,
            listener,
            app,
            std::future::pending::<()>(),
        ));
        adresse
    });
    Serveur {
        adresse,
        entrees,
        _principal: principal,
        _transport: transport,
        _wav: wav,
    }
}

/// Une requête GET bloquante ; rend le corps.
fn get_simple(adresse: SocketAddr, chemin: &str) -> String {
    let mut c = TcpStream::connect(adresse).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        c,
        "GET {chemin} HTTP/1.1\r\nHost: tune\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut tout = String::new();
    c.read_to_string(&mut tout).unwrap();
    tout.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

/// Ouvre `/stream/<id>.wav` à partir de `debut` et rend la socket placée
/// juste après les en-têtes, avec le statut et `Content-Range` lus.
fn ouvrir_le_flux(adresse: SocketAddr, debut: u64) -> Result<(TcpStream, Vec<u8>), String> {
    let mut c = TcpStream::connect(adresse).map_err(|e| e.to_string())?;
    c.set_read_timeout(Some(PATIENCE_DU_RENDERER)).unwrap();
    write!(
        c,
        "GET /stream/{ID_DU_FLUX}.wav HTTP/1.1\r\nHost: tune\r\nUser-Agent: player/100\r\n\
         Range: bytes={debut}-\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let ouvert = Instant::now();
    let mut lu = Vec::new();
    let mut bloc = [0u8; 4096];
    let fin_des_entetes = loop {
        match c.read(&mut bloc) {
            Ok(0) => return Err("connexion fermée avant les en-têtes".into()),
            Ok(n) => lu.extend_from_slice(&bloc[..n]),
            Err(_) => {
                return Err(format!(
                    "la reprise `Range: bytes={debut}-` n'a reçu AUCUNE réponse en {} ms : \
                     le renderer abandonne",
                    ouvert.elapsed().as_millis()
                ));
            }
        }
        if let Some(i) = lu.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let entetes = String::from_utf8_lossy(&lu[..fin_des_entetes]).to_lowercase();
    let attendu = format!(
        "content-range: bytes {debut}-{}/{TAILLE_DU_WAV}",
        TAILLE_DU_WAV - 1
    );
    if !entetes.starts_with("http/1.1 206") || !entetes.contains(&attendu) {
        return Err(format!("reprise mal servie : {entetes}"));
    }
    let longueur = format!("content-length: {}", TAILLE_DU_WAV - debut);
    if !entetes.contains(&longueur) || entetes.contains("transfer-encoding") {
        return Err(format!("longueur de la reprise fausse : {entetes}"));
    }
    Ok((c, lu[fin_des_entetes..].to_vec()))
}

/// Ce que le faux renderer a vécu.
struct Bilan {
    octets: u64,
    plus_long_silence: Duration,
    reprise_faite: bool,
}

/// Le faux darTZeel : lit au rythme fixé jusqu'à `jusqu_a` ; reprend une fois
/// par `Range` quand `reprendre_apres` est atteint. Rend `Err` dès qu'il
/// referme la connexion faute de données.
fn faux_renderer(
    adresse: SocketAddr,
    reprendre_apres: Arc<Mutex<Option<Instant>>>,
    jusqu_a: Arc<Mutex<Option<Instant>>>,
) -> Result<Bilan, String> {
    let depart = Instant::now();
    let mut position: u64 = 44;
    let (mut c, deja) = ouvrir_le_flux(adresse, position)?;
    position += deja.len() as u64;
    let mut bilan = Bilan {
        octets: deja.len() as u64,
        plus_long_silence: Duration::ZERO,
        reprise_faite: false,
    };
    let mut dernier = Instant::now();
    let mut tampon = vec![0u8; 64 * 1024];
    loop {
        if let Some(t) = *jusqu_a.lock().unwrap()
            && Instant::now() >= t
        {
            return Ok(bilan);
        }
        if !bilan.reprise_faite
            && let Some(t) = *reprendre_apres.lock().unwrap()
            && Instant::now() >= t
        {
            // Le renderer lâche sa connexion et redemande la suite, en plein
            // gel de l'exécuteur principal.
            drop(c);
            let (nouvelle, deja) = ouvrir_le_flux(adresse, position)?;
            c = nouvelle;
            position += deja.len() as u64;
            bilan.octets += deja.len() as u64;
            bilan.reprise_faite = true;
            dernier = Instant::now();
        }
        match c.read(&mut tampon) {
            Ok(0) => {
                return Err(format!(
                    "le serveur a fermé le flux à l'octet {position} sur {TAILLE_DU_WAV}"
                ));
            }
            Ok(n) => {
                let silence = dernier.elapsed();
                bilan.plus_long_silence = bilan.plus_long_silence.max(silence);
                dernier = Instant::now();
                position += n as u64;
                bilan.octets += n as u64;
            }
            Err(_) => {
                return Err(format!(
                    "le faux renderer n'a rien reçu pendant {} ms (patience {} ms) à l'octet {position} : \
                     il referme la connexion, la musique s'arrête — le flux attendait l'exécuteur gelé",
                    dernier.elapsed().as_millis(),
                    PATIENCE_DU_RENDERER.as_millis()
                ));
            }
        }
        // Rythme de lecture : pas plus vite que OCTETS_PAR_SECONDE.
        let du = Duration::from_secs_f64(bilan.octets as f64 / OCTETS_PAR_SECONDE);
        let ecoule = depart.elapsed();
        if du > ecoule {
            std::thread::sleep(du - ecoule);
        }
    }
}

/// 🔴 Le témoin : l'exécuteur principal est gelé pendant 6 s ; le faux
/// darTZeel continue de recevoir, et sa reprise `Range` en plein gel est
/// servie.
#[test]
fn un_gel_de_l_executeur_principal_ne_tait_pas_le_flux_du_renderer() {
    let serveur = demarrer();
    let adresse = serveur.adresse;
    let reprendre_apres = Arc::new(Mutex::new(None));
    let jusqu_a = Arc::new(Mutex::new(None));

    let renderer = {
        let (r, j) = (reprendre_apres.clone(), jusqu_a.clone());
        std::thread::spawn(move || faux_renderer(adresse, r, j))
    };
    // Le flux est établi et tiré au rythme.
    std::thread::sleep(Duration::from_millis(800));

    // Le gel : une requête bloquante par fil de l'exécuteur principal.
    let bloqueurs: Vec<_> = (0..FILS_PRINCIPAUX)
        .map(|_| std::thread::spawn(move || get_simple(adresse, "/api/v1/bloque")))
        .collect();
    let attente = Instant::now();
    while serveur.entrees.load(SeqCst) < FILS_PRINCIPAUX {
        assert!(
            attente.elapsed() < Duration::from_secs(5),
            "le gel n'a pas pris : {} requête(s) bloquante(s) entrée(s) sur {FILS_PRINCIPAUX}",
            serveur.entrees.load(SeqCst)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let debut_du_gel = Instant::now();
    *reprendre_apres.lock().unwrap() = Some(debut_du_gel + Duration::from_millis(500));
    *jusqu_a.lock().unwrap() = Some(debut_du_gel + GEL + Duration::from_millis(500));

    let bilan = renderer.join().unwrap();
    for b in bloqueurs {
        assert_eq!(b.join().unwrap(), "fini");
    }
    let bilan = match bilan {
        Ok(b) => b,
        Err(e) => panic!("{e}"),
    };
    assert!(bilan.reprise_faite, "la reprise Range n'a pas eu lieu");
    assert!(
        bilan.plus_long_silence < PATIENCE_DU_RENDERER,
        "plus long silence {} ms",
        bilan.plus_long_silence.as_millis()
    );
    // Il a lu pendant le gel, pas seulement avant et après.
    let pendant_le_gel = (GEL.as_secs_f64() * OCTETS_PAR_SECONDE * 0.5) as u64;
    assert!(
        bilan.octets > pendant_le_gel,
        "{} octets lus seulement",
        bilan.octets
    );
}

/// Les autres requêtes vont toujours à l'exécuteur principal, avec l'adresse
/// du client posée comme `into_make_service_with_connect_info` le faisait.
#[test]
fn l_api_reste_servie_par_l_executeur_principal_avec_l_adresse_du_client() {
    let serveur = demarrer();
    let corps = get_simple(serveur.adresse, "/api/v1/ou");
    let (fil, client) = corps.split_once('|').unwrap_or_default();
    assert!(
        fil.starts_with("principal-4645"),
        "la route d'API a tourné sur « {fil} »"
    );
    assert_eq!(client, "127.0.0.1");
}

#[test]
fn seul_le_corps_des_pistes_est_un_flux_audio() {
    assert!(est_un_flux_audio("/stream/2338763c.wav"));
    assert!(est_un_flux_audio("/stream/x"));
    assert!(!est_un_flux_audio("/api/v1/system/health"));
    assert!(!est_un_flux_audio("/streaming"));
    assert!(!est_un_flux_audio("/api/v1/radios/3/stream"));
    assert!(!est_un_flux_audio("/"));
}
