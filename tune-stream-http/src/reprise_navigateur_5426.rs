//! #5426 — zone navigateur : une reprise `Range` EN DEÇÀ de ce que le tuyau a
//! déjà rendu doit repartir au bon octet.
//!
//! Journal du testeur (fil 2034, Firefox 140, Docker Debian, 0.9.167), cinq
//! fois : une série de `stream_delivery_stall`, puis le navigateur revient
//! avec `range="bytes=45289018-"` alors que `bytes_sent=51904556` — et
//! toujours APRÈS que le canal s'était vidé. Le tuyau ne rejoue rien : la
//! reprise recevait la suite du tuyau sous l'offset demandé, ou, canal vidé,
//! un 206 sans un octet. Le son s'arrête, l'horloge de la zone avance.
//!
//! Ces gardes passent par le VRAI routeur (`router`), servi sur une socket,
//! et un vrai client HTTP : c'est hyper qui tire le corps, c'est la socket
//! qui fait la contre-pression.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;

const NAVIGATEUR: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Octet `i` du PCM : identifiable, pour qu'un saut ou un décalage se voie.
fn octet(i: usize) -> u8 {
    ((i % 251) as u8).wrapping_add((i / 65_536) as u8)
}

/// Le flux tel que le producteur l'émet : en-tête WAV (dans le canal, comme
/// le décodeur progressif) puis `pcm` octets.
fn flux_source(pcm: usize) -> Vec<u8> {
    let mut v = tune_core::audio::wav::build_wav_header(2, 44_100, 16).to_vec();
    v.extend((0..pcm).map(octet));
    v
}

/// Une session de conversion WAV comme celle de la zone navigateur
/// (`transcoder_en_session` : le producteur émet l'en-tête), servie par le
/// vrai routeur. Rend l'URL, la session et la tâche productrice.
async fn servir(
    id: &str,
    duree_ms: u64,
    source: Vec<u8>,
) -> (String, Arc<StreamSession>, tokio::task::JoinHandle<()>) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        duration_ms: Some(duree_ms),
        ..StreamInfo::default()
    };
    // 16 blocs de 32 Kio : le producteur bute sur un canal plein, comme le
    // décodeur réel sur ses 256 blocs.
    let session = Arc::new(StreamSession::new(id.into(), info, false, 16));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    // La fin du canal sera la chute de NOTRE émetteur, comme quand le
    // décodeur termine et que `close_sender` a été appelé.
    session.close_sender().await;
    let producteur = tokio::spawn(async move {
        for bloc in source.chunks(32_768) {
            if tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });
    (
        format!("http://127.0.0.1:{port}/stream/{id}.wav"),
        session,
        producteur,
    )
}

async fn demander(url: &str, debut: u64, quoi: &str) -> reqwest::Response {
    demander_comme(url, Some(debut), NAVIGATEUR, quoi).await
}

/// `debut = None` : une requête SANS en-tête `Range`.
async fn demander_comme(
    url: &str,
    debut: Option<u64>,
    agent: &str,
    quoi: &str,
) -> reqwest::Response {
    // Le constructeur partagé du dépôt (garde `http_client_seam`) ; un
    // client neuf par requête : une connexion neuve, comme la reprise du
    // navigateur.
    let mut requete = tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", agent);
    if let Some(debut) = debut {
        requete = requete.header("Range", format!("bytes={debut}-"));
    }
    requete
        .send()
        .await
        .unwrap_or_else(|e| panic!("{quoi} (`Range: bytes={debut:?}-`) : aucune réponse — {e:?}"))
}

fn entete(r: &reqwest::Response, nom: &str) -> String {
    r.headers()
        .get(nom)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Lit le corps jusqu'au bout. `Err` si le corps s'interrompt (connexion
/// coupée avant le Content-Length) ou reste muet plus de 10 s — le blocage
/// que le testeur entend comme un silence.
async fn lire_jusqu_au_bout(mut r: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut recu = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => {
                return Err(format!(
                    "corps muet depuis 10 s après {} octets",
                    recu.len()
                ));
            }
            Ok(Err(e)) => {
                return Err(format!(
                    "corps interrompu après {} octets : {e}",
                    recu.len()
                ));
            }
            Ok(Ok(None)) => return Ok(recu),
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

fn premier_ecart(recu: &[u8], attendu: &[u8]) -> Option<usize> {
    recu.iter().zip(attendu).position(|(a, b)| a != b)
}

/// LE CAS DU JOURNAL. Le navigateur lit lentement, le serveur prend de
/// l'avance dans les tampons de la connexion, puis le navigateur coupe et
/// revient avec un `Range` qui tombe EN DEÇÀ de ce que le tuyau a rendu.
/// La reprise doit recevoir un 206 cohérent et, à l'octet près, la suite de
/// ce qu'il a reçu — jusqu'à la fin, sans coupure ni blocage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_client_lent_qui_reprend_en_deca_du_tuyau_repart_au_bon_octet() {
    // 34 s de CD : la longueur annoncée (44 + 34 000 ms × 176 400 o/s) est
    // exactement celle du flux.
    let pcm = 5_997_600;
    let source = flux_source(pcm);
    let total = source.len() as u64;
    let (url, session, producteur) = servir("i5426-lent", 34_000, source.clone()).await;

    // ── Connexion 1 : l'onglet qui lit à son rythme, puis s'arrête ──
    let mut premiere = demander(&url, 0, "connexion 1").await;
    assert_eq!(premiere.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(entete(&premiere, "Content-Length"), total.to_string());
    let mut recu = Vec::new();
    while recu.len() < 1_000_000 {
        let b = tokio::time::timeout(Duration::from_secs(10), premiere.chunk())
            .await
            .expect("la première connexion doit être servie")
            .expect("corps")
            .expect("le flux ne doit pas finir si tôt");
        recu.extend_from_slice(&b);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(&recu[..], &source[..recu.len()]);
    let reprise = recu.len() as u64;

    // Le serveur a tiré du tuyau PLUS que ce que le client a reçu : ces
    // octets sont en vol, et ils seront perdus quand la connexion tombe.
    let mut tire = 0;
    for _ in 0..200 {
        tire = session.octets_du_canal.load(SeqCst);
        if tire > reprise + 256 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        tire > reprise + 256 * 1024,
        "mise en scène : le tuyau devait être en avance sur le client ({tire} tirés, {reprise} reçus)"
    );
    drop(premiere);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Connexion 2 : la reprise, en plein milieu ──
    let seconde = demander(&url, reprise, "reprise").await;
    assert_eq!(seconde.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&seconde, "Content-Range"),
        format!("bytes {reprise}-{}/{total}", total - 1)
    );
    let suite = lire_jusqu_au_bout(seconde)
        .await
        .unwrap_or_else(|e| panic!("reprise à {reprise} : {e}"));
    let attendu = &source[reprise as usize..];
    if let Some(i) = premier_ecart(&suite, attendu) {
        panic!(
            "reprise à {reprise} (tuyau déjà à {tire}) : l'octet {} du flux diffère — la reprise \
             n'est pas repartie au bon octet",
            reprise as usize + i
        );
    }
    assert_eq!(
        suite.len(),
        attendu.len(),
        "reprise à {reprise} : {} octets reçus, {} attendus jusqu'à la fin",
        suite.len(),
        attendu.len()
    );
    producteur.await.unwrap();
}

/// LE CAS DU JOURNAL, canal VIDÉ. Le producteur a fini, la première
/// connexion a tiré tout le tuyau — et son corps s'est arrêté avant le
/// Content-Length déduit de la durée (ce qu'un MP3 dont la durée en
/// bibliothèque déborde du PCM décodé produit). Le navigateur revient en plein
/// milieu : avant le correctif, 206 annonçant la fin et pas un octet.
///
/// La reprise reçoit la longueur VRAIE, les octets exacts, une fin propre ;
/// une reprise au-delà de la fin reçoit un 416 qui dit la taille.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_reprise_apres_un_canal_vide_recoit_les_octets_et_la_vraie_longueur() {
    // Annoncé : 25 s (4 410 044 octets). Produit : 4 000 000 octets de PCM.
    let source = flux_source(4_000_000);
    let fin = source.len() as u64;
    let (url, session, producteur) = servir("i5426-vide", 25_000, source.clone()).await;

    // Connexion 1 : tout le tuyau passe, puis le corps s'arrête court.
    let premiere = demander(&url, 0, "connexion 1").await;
    let tout = lire_jusqu_au_bout(premiere).await;
    producteur.await.unwrap();
    assert_eq!(
        session.octets_du_canal.load(SeqCst),
        fin,
        "mise en scène : le canal doit être vidé ({tout:?})",
        tout = tout.as_ref().map(|v| v.len())
    );

    // Connexion 2 : le navigateur reprend là où SON cache s'arrête.
    let reprise: u64 = 3_000_000;
    let seconde = demander(&url, reprise, "reprise").await;
    assert_eq!(seconde.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&seconde, "Content-Range"),
        format!("bytes {reprise}-{}/{fin}", fin - 1),
        "la reprise d'un canal fini doit annoncer sa longueur vraie"
    );
    assert_eq!(
        entete(&seconde, "Content-Length"),
        (fin - reprise).to_string()
    );
    let suite = lire_jusqu_au_bout(seconde)
        .await
        .unwrap_or_else(|e| panic!("reprise à {reprise} sur un canal vidé : {e}"));
    assert_eq!(
        suite.len() as u64,
        fin - reprise,
        "reprise à {reprise} sur un canal vidé : {} octets reçus",
        suite.len()
    );
    assert!(
        suite == source[reprise as usize..],
        "reprise à {reprise} : octets différents de la source"
    );

    // Connexion 3 : au-delà de la fin, rien à servir — et on le dit.
    let au_dela = demander(&url, fin, "reprise au-delà de la fin").await;
    assert_eq!(au_dela.status(), reqwest::StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(entete(&au_dela, "Content-Range"), format!("bytes */{fin}"));
}

/// La retenue est bornée : 180 s d'audio au débit du flux, 64 Mio au plus.
#[test]
fn la_fenetre_de_retenue_est_bornee() {
    let info = StreamInfo {
        format: "wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        ..StreamInfo::default()
    };
    // 180 s de CD.
    assert_eq!(info.fenetre_de_retenue(), 31_752_000);
    let hi_res = StreamInfo {
        sample_rate: 384_000,
        channels: 2,
        bit_depth: 32,
        ..info.clone()
    };
    assert_eq!(
        hi_res.fenetre_de_retenue(),
        tune_core::http::streamer::RETENUE_MAX_OCTETS
    );
}

// ── Début de piste perdu derrière une sonde Lavf (.18, 1.0.0-rc1, 02/10) ──
//
// Zone 10 (Eversolo DMP-A8, Qobuz FLAC → WAV à la volée, contrat chunké) :
// à chaque piste, DEUX `GET … range="bytes=0-" agent="Lavf/58.45.100"` à
// ~200 ms d'écart, et « les premières secondes semblent perdues ». La
// première connexion est une sonde : elle tire une partie du préchargement
// puis se ferme. La seconde, la vraie lecture, demande elle aussi le début.

const LAVF: &str = "Lavf/58.45.100";

/// Une sonde lit `signal` octets APRÈS l'en-tête, puis coupe ; une seconde
/// connexion demande le début (`debut_lecture`). Rend ce que la seconde a
/// reçu, son statut, et la source.
async fn sonde_puis_lecture(
    id: &str,
    debut_sonde: Option<u64>,
    debut_lecture: Option<u64>,
    signal: usize,
) -> (Vec<u8>, reqwest::StatusCode, Vec<u8>, u64, Option<String>) {
    // 17 s de CD : la longueur annoncée est exactement celle du flux.
    let source = flux_source(2_998_800);
    let (url, session, producteur) = servir(id, 17_000, source.clone()).await;

    let mut sonde = demander_comme(&url, debut_sonde, LAVF, "sonde").await;
    let mut recu = Vec::new();
    while recu.len() < 44 + signal {
        let b = tokio::time::timeout(Duration::from_secs(10), sonde.chunk())
            .await
            .expect("la sonde doit être servie")
            .expect("corps")
            .expect("le flux ne doit pas finir si tôt");
        recu.extend_from_slice(&b);
    }
    assert_eq!(&recu[..], &source[..recu.len()], "la sonde reçoit le début");
    drop(sonde);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tire = session.octets_du_canal.load(SeqCst);

    let lecture = demander_comme(&url, debut_lecture, LAVF, "lecture").await;
    let statut = lecture.status();
    let (suite, interruption) = lire_ce_qui_arrive(lecture).await;
    producteur.await.unwrap();
    (suite, statut, source, tire, interruption)
}

/// Ce qui arrive, même si le corps s'interrompt : ce sont les octets reçus
/// qui disent où le signal repart.
async fn lire_ce_qui_arrive(mut r: reqwest::Response) -> (Vec<u8>, Option<String>) {
    let mut recu = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => return (recu, Some("corps muet depuis 10 s".to_string())),
            Ok(Err(e)) => return (recu, Some(format!("corps interrompu : {e}"))),
            Ok(Ok(None)) => return (recu, None),
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

fn verifier_debut_intact(suite: &[u8], source: &[u8], tire: u64, interruption: Option<String>) {
    assert!(
        suite.starts_with(b"RIFF"),
        "la lecture n'a pas reçu l'en-tête WAV"
    );
    if let Some(i) = premier_ecart(suite, source) {
        // Où la charge reçue retombe-t-elle dans la source ? C'est ce que
        // la sonde a emporté.
        let perdu = (44..source.len())
            .find(|&k| source[k..].starts_with(&suite[44..suite.len().min(44 + 4096)]))
            .map(|k| k - 44);
        panic!(
            "la lecture diffère de la source dès l'octet {i} (tuyau déjà à {tire} à son \
             arrivée, {} octets reçus, {interruption:?}) : son signal repart {perdu:?} octets \
             après le début — le début de la piste est parti dans la sonde",
            suite.len()
        );
    }
    assert_eq!(interruption, None, "la lecture doit aller jusqu'au bout");
    assert_eq!(
        suite.len(),
        source.len(),
        "la lecture doit recevoir toute la piste"
    );
}

/// LE CAS DU JOURNAL : sonde `bytes=0-` puis lecture `bytes=0-`. La lecture
/// reçoit l'en-tête PUIS le signal depuis l'octet 44, pas depuis 44 + N.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_sonde_lavf_ne_vole_pas_le_debut_de_la_piste() {
    let (suite, statut, source, tire, interruption) =
        sonde_puis_lecture("debut-perdu-206", Some(0), Some(0), 300_000).await;
    assert_eq!(statut, reqwest::StatusCode::PARTIAL_CONTENT);
    verifier_debut_intact(&suite, &source, tire, interruption);
}

/// Même chose sans en-tête `Range` : une requête depuis le début reste une
/// requête depuis le début.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_sonde_sans_range_ne_vole_pas_le_debut_de_la_piste() {
    let (suite, statut, source, tire, interruption) =
        sonde_puis_lecture("debut-perdu-200", None, None, 300_000).await;
    assert_eq!(statut, reqwest::StatusCode::OK);
    verifier_debut_intact(&suite, &source, tire, interruption);
}

/// La retenue a glissé au-delà de 0 : le début n'existe plus nulle part. On
/// garde le comportement d'avant — l'en-tête rejoué, puis le direct, contigu
/// jusqu'à la fin — sans bloquer ni répondre 416.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_lecture_depuis_le_debut_apres_glissement_recoit_l_entete_puis_le_direct() {
    // Une session 8 bits mono à 8 kHz : sa fenêtre tombe au plancher de
    // 8 Mio, qu'une sonde de 9 Mo fait glisser.
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 8_000,
        channels: 1,
        bit_depth: 8,
        duration_ms: Some(2_400_000),
        ..StreamInfo::default()
    };
    let fenetre = info.fenetre_de_retenue();
    assert_eq!(fenetre, tune_core::http::streamer::RETENUE_MIN_OCTETS);
    // 2 400 s × 8 000 o/s : la sonde n'en tire que la moitié, même avec
    // ce que les tampons de sa connexion morte emportent.
    let pcm = 19_200_000usize;
    let mut source = tune_core::audio::wav::build_wav_header(1, 8_000, 8).to_vec();
    source.extend((0..pcm).map(octet));
    let session = Arc::new(StreamSession::new("debut-glisse".into(), info, false, 16));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let a_envoyer = source.clone();
    let producteur = tokio::spawn(async move {
        for bloc in a_envoyer.chunks(32_768) {
            if tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("debut-glisse".to_string(), session.clone())]
            .into_iter()
            .collect(),
    ));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });
    let url = format!("http://127.0.0.1:{port}/stream/debut-glisse.wav");

    // La sonde tire 9 Mo : la retenue a glissé.
    let mut sonde = demander_comme(&url, Some(0), LAVF, "sonde").await;
    let mut recu = 0usize;
    while recu < 9_000_000 {
        let b = tokio::time::timeout(Duration::from_secs(10), sonde.chunk())
            .await
            .expect("la sonde doit être servie")
            .expect("corps")
            .expect("le flux ne doit pas finir si tôt");
        recu += b.len();
    }
    drop(sonde);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (debut_retenue, _) = session.etendue_retenue();
    assert!(
        debut_retenue > 0,
        "mise en scène : la retenue doit avoir glissé"
    );

    let lecture = demander_comme(&url, Some(0), LAVF, "lecture").await;
    assert_eq!(lecture.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    // Le corps finit court de son Content-Length (les octets partis dans la
    // sonde manquent) : c'est le comportement d'avant, gardé tel quel.
    let (suite, _interruption) = lire_ce_qui_arrive(lecture).await;
    producteur.await.unwrap();
    assert_eq!(&suite[..44], &source[..44], "l'en-tête est rejoué");
    let charge = &suite[44..];
    assert!(!charge.is_empty(), "le direct doit suivre l'en-tête");
    // Le direct repart d'où le tuyau en est : un morceau CONTIGU de la
    // source, situé après tout ce que la sonde a tiré.
    let repere = &charge[..charge.len().min(4096)];
    let k = (44..source.len())
        .find(|&k| source[k..].starts_with(repere))
        .expect("le direct doit être un morceau de la source");
    assert!(k >= 9_000_000, "le direct repart après la sonde (à {k})");
    assert!(
        source[k..].starts_with(charge),
        "après l'en-tête, la lecture reçoit le direct contigu ({} octets depuis {k})",
        charge.len()
    );
}
