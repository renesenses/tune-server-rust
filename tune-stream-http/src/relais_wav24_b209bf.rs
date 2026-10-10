//! Zone 10 du .18 (Eversolo DMP-A8, `dlna_wav24`, 1.0.0-rc2, 08/10) : une
//! piste Qobuz FLAC 24/44,1 servie en WAV 24 bits CONVERTI, en transfert
//! chunké (durée inconnue, donc ni `Content-Length` ni `Range` honoré), se
//! jouait « déformée, avec du bruit ». Le renderer (`Lavf/58.45.100`) ouvre
//! TROIS connexions `bytes=0-` de suite ; la troisième supersède la deuxième.
//!
//! Relevé du journal : la troisième connexion a reçu l'en-tête rejoué, PUIS le
//! tuyau à sa position courante — l'octet 38 139 668 du flux, soit 2 min 24 s
//! dans la piste. Deux requêtes identiques (`bytes=0-`) recevaient donc deux
//! suites d'octets DIFFÉRENTES sous les mêmes positions : là où un fichier
//! rend le début, la conversion rendait un en-tête collé au milieu du signal.
//!
//! Le rejeu de la retenue « depuis le début » (zone 10, 02/10) ne valait que
//! si la longueur du flux était connue (`wav_length`) : le flux chunké d'une
//! conversion Qobuz progressive, sans durée, en était exclu, alors que la
//! retenue y est tenue tout pareil.
//!
//! Le contrat gardé ici est celui d'un fichier : TOUTE connexion `bytes=0-`
//! reçoit le flux depuis son premier octet, à l'octet — l'en-tête puis les
//! trames 24 bits alignées, sans trou ni recollage — tant que la retenue
//! commence à 0.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;

const LAVF: &str = "Lavf/58.45.100";
/// La trame d'un PCM 24 bits stéréo.
const TRAME: usize = 6;
/// Le lot du décodeur progressif en 24 bits stéréo : 32 768 arrondi à la trame.
const BLOC: usize = 32_766;

/// Trame `n` : canal gauche = `n`, droit = `n ^ 0x5A5A5A`, en 24 bits LE.
/// Un décalage d'un octet, un trou ou un recollage se voient tous.
fn trame(n: usize) -> [u8; TRAME] {
    let g = (n as u32) & 0xFF_FFFF;
    let d = g ^ 0x5A_5A5A;
    let (g, d) = (g.to_le_bytes(), d.to_le_bytes());
    [g[0], g[1], g[2], d[0], d[1], d[2]]
}

/// Le flux tel que le décodeur progressif l'émet : l'en-tête WAV 24 bits de
/// durée inconnue, seul dans son bloc, puis le PCM en blocs alignés.
fn flux_source(trames: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let entete = tune_core::audio::wav::build_wav_header(2, 44_100, 24).to_vec();
    let pcm: Vec<u8> = (0..trames).flat_map(trame).collect();
    let mut blocs = vec![entete.clone()];
    blocs.extend(pcm.chunks(BLOC).map(<[u8]>::to_vec));
    let mut tout = entete;
    tout.extend_from_slice(&pcm);
    (tout, blocs)
}

async fn servir(id: &str, blocs: Vec<Vec<u8>>) -> (String, Arc<StreamSession>) {
    // Durée inconnue : c'est la conversion Qobuz progressive
    // (`ouvrir_la_session_wav`), servie en chunké.
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 24,
        duration_ms: None,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 16));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    tokio::spawn(async move {
        for bloc in blocs {
            if tx.send(bloc).await.is_err() {
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
    (format!("http://127.0.0.1:{port}/stream/{id}.wav"), session)
}

async fn depuis_zero(url: &str, quoi: &str) -> reqwest::Response {
    let r = tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap_or_else(|e| panic!("{quoi} : aucune réponse — {e:?}"));
    assert!(
        r.headers().get("Content-Length").is_none(),
        "{quoi} : la mise en scène veut le flux chunké, sans longueur"
    );
    r
}

/// Lit au moins `au_moins` octets (sans aller au bout).
async fn lire_au_moins(r: &mut reqwest::Response, au_moins: usize, quoi: &str) -> Vec<u8> {
    let mut recu = Vec::new();
    while recu.len() < au_moins {
        let b = tokio::time::timeout(Duration::from_secs(10), r.chunk())
            .await
            .unwrap_or_else(|_| panic!("{quoi} : corps muet après {} octets", recu.len()))
            .expect("corps")
            .unwrap_or_else(|| panic!("{quoi} : fin du corps après {} octets", recu.len()));
        recu.extend_from_slice(&b);
    }
    recu
}

async fn lire_jusqu_au_bout(mut r: reqwest::Response, mut recu: Vec<u8>, quoi: &str) -> Vec<u8> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => panic!("{quoi} : corps muet après {} octets", recu.len()),
            Ok(Err(e)) => panic!(
                "{quoi} : corps interrompu après {} octets : {e}",
                recu.len()
            ),
            Ok(Ok(None)) => return recu,
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

/// Ce qu'un renderer lit de `recu` : l'en-tête, puis la première trame. Rend
/// le numéro de cette trame, ou un diagnostic.
fn premiere_trame(recu: &[u8]) -> Result<usize, String> {
    if recu.len() < 44 + TRAME || !recu.starts_with(b"RIFF") || &recu[8..12] != b"WAVE" {
        return Err("pas d'en-tête WAV en tête de corps".into());
    }
    let t = &recu[44..44 + TRAME];
    let g = (t[0] as usize) | ((t[1] as usize) << 8) | ((t[2] as usize) << 16);
    if trame(g)[..] != t[..] {
        return Err(format!(
            "la première « trame » après l'en-tête n'en est pas une : {t:02x?} — PCM désaligné"
        ));
    }
    Ok(g)
}

/// LE CAS DU JOURNAL : trois `bytes=0-`, la deuxième lue en partie (à un
/// octet hors trame) puis supersédée par la troisième, qui joue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trois_requetes_depuis_zero_recoivent_toutes_le_debut_du_flux_wav24() {
    // 1 000 000 trames : 6 Mo, ~22,7 s — sous la retenue (180 s au débit du
    // flux), comme les 38 Mo du journal sous ses 47,6 Mo.
    let (source, blocs) = flux_source(1_000_000);
    let (url, session) = servir("wav24-relais", blocs).await;

    // ── Connexion 1 : la sonde, qui lit un peu puis ferme ──
    let mut sonde = depuis_zero(&url, "connexion 1").await;
    let lu1 = lire_au_moins(&mut sonde, 200_000, "connexion 1").await;
    drop(sonde);

    // ── Connexion 2 : lue en partie, jusqu'à un octet HORS trame ──
    let mut deuxieme = depuis_zero(&url, "connexion 2").await;
    let lu2 = lire_au_moins(&mut deuxieme, 1_000_003, "connexion 2").await;

    // ── Connexion 3 : supersède la deuxième ; c'est elle qui joue ──
    let mut troisieme = depuis_zero(&url, "connexion 3").await;
    let lu3 = lire_au_moins(&mut troisieme, 44 + TRAME, "connexion 3").await;
    let lu2 = lire_jusqu_au_bout(deuxieme, lu2, "connexion 2").await;
    let lu3 = lire_jusqu_au_bout(troisieme, lu3, "connexion 3").await;
    let tire = session.octets_du_canal.load(SeqCst);

    for (quoi, recu) in [("connexion 1", &lu1), ("connexion 2", &lu2)] {
        let n = premiere_trame(recu).unwrap_or_else(|e| panic!("{quoi} : {e}"));
        assert_eq!(
            n, 0,
            "{quoi} : `bytes=0-` a reçu l'en-tête puis la trame {n} — le début de la piste \
             est parti dans une connexion précédente"
        );
        assert!(
            recu[..] == source[..recu.len()],
            "{quoi} : {} octets reçus qui ne sont pas le début du flux",
            recu.len()
        );
    }

    // La connexion qui joue : le flux ENTIER, comme un fichier.
    let n = premiere_trame(&lu3).unwrap_or_else(|e| panic!("connexion 3 : {e}"));
    assert_eq!(
        n,
        0,
        "connexion 3 : `bytes=0-` a reçu l'en-tête rejoué puis le tuyau à sa position \
         courante (trame {n}, {:.1} s dans la piste) au lieu du début — deux requêtes \
         identiques rendent deux suites d'octets différentes (tuyau tiré : {tire} octets)",
        n as f64 / 44_100.0
    );
    assert_eq!(
        (lu3.len() - 44) % TRAME,
        0,
        "connexion 3 : PCM qui ne finit pas sur une trame"
    );
    assert_eq!(lu3.len(), source.len(), "connexion 3 : longueur du flux");
    if let Some(i) = lu3.iter().zip(&source).position(|(a, b)| a != b) {
        panic!(
            "connexion 3 : premier octet faux à {i} (trame {})",
            (i - 44) / TRAME
        );
    }
}
