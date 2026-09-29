//! #4661 (fil forum 1912) — un darTZeel LHC-208 factice, au contrat HTTP du
//! vrai, qui SONDE la piste puis la LÂCHE.
//!
//! Le vrai (Sevy Tabroc, User-Agent `player/100`) :
//!
//! - refuse le transfert chunké : il exige `Content-Length` et
//!   `Accept-Ranges` ;
//! - ouvre chaque WAV par une requête sans `Range` qu'il abandonne après
//!   quelques centaines de Kio, puis repart à `Range: bytes=44-` ;
//! - le 24/09/2026 à 14:27:27, sur « Close That Gap », a refermé AUSSI la
//!   seconde requête au bout de 44 ms (720 896 octets), et n'est jamais
//!   revenu : 1 572 864 octets servis sur 52 510 796.
//!
//! Ce banc rejoue ces deux abandons contre le VRAI routeur, sur une vraie
//! socket, et vérifie ce que la session rend ensuite au sondeur : des octets
//! servis NON NULS et une taille connue qu'ils n'atteignent pas. C'est
//! exactement la mesure qui, en 0.9.163, tombait entre la relance du
//! démarrage mort (qui voulait 0 octet) et la reprise à la position atteinte
//! (qui voulait une position > 0) — voir
//! `tune-core/src/poller/demarrage_mort_apres_sondage_4661.rs`, qui éprouve la
//! décision du sondeur sur cette mesure.
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.
//!
//! Refs renesenses/tune-server-rust#4661

use std::time::Duration;

use tune_core::http::streamer::{AudioStreamer, StreamInfo};

/// Ce que le LHC tire avant de lâcher, relevé au journal du fil 1912.
const SONDE_SANS_RANGE: usize = 851_968;
const SONDE_BYTES_44: usize = 720_896;
/// Assez grand pour que les tampons TCP des deux côtés ne puissent pas
/// avaler le fichier entier : l'abandon doit rester un abandon.
const TAILLE: usize = 48 * 1024 * 1024;

/// Le LHC ne lit que ce que le contrat lui promet. Il lit `quota` octets du
/// corps, puis referme la connexion.
async fn lhc_lit_puis_lache(reponse: reqwest::Response, quota: usize) -> usize {
    let mut reponse = reponse;
    let mut lus = 0usize;
    while lus < quota {
        match reponse.chunk().await.expect("corps illisible") {
            Some(morceau) => lus += morceau.len(),
            None => break,
        }
    }
    drop(reponse);
    lus
}

/// Le contrat que le LHC-208 exige avant de lire quoi que ce soit.
fn contrat_du_lhc(reponse: &reqwest::Response, longueur_attendue: u64) {
    let h = reponse.headers();
    assert!(
        h.get(reqwest::header::TRANSFER_ENCODING).is_none(),
        "le LHC-208 refuse le transfert chunké : {h:?}"
    );
    assert_eq!(
        h.get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok()),
        Some(longueur_attendue),
        "le LHC-208 exige un Content-Length exact : {h:?}"
    );
    assert_eq!(
        h.get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes"),
        "le LHC-208 exige Accept-Ranges: bytes : {h:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_lhc_qui_sonde_puis_lache_laisse_une_mesure_de_demarrage_mort() {
    let fichier = tune_core::test_scratch::scratch_file("4661-sonde", ".wav");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, vec![0x11u8; TAILLE]).expect("fichier de test");
    let taille = TAILLE as u64;

    let streamer = AudioStreamer::new(0);
    let sid = streamer
        .create_file_session(
            StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                file_size: Some(taille),
                ..StreamInfo::default()
            },
            chemin.to_string_lossy().into_owned(),
            false,
        )
        .await;

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port d'écoute");
    let adresse = ecoute.local_addr().expect("adresse locale");
    let routeur = tune_stream_http::router(streamer.sessions_state());
    let serveur = tokio::spawn(async move {
        axum::serve(ecoute, routeur).await.ok();
    });
    let url = format!("http://{adresse}/stream/{sid}.wav");
    let client = reqwest::Client::builder()
        .user_agent("player/100")
        .build()
        .expect("client");

    // ─── 1. La sonde sans Range : 200, longueur entière, puis abandon. ───
    let r1 = client.get(&url).send().await.expect("requête sans Range");
    assert_eq!(r1.status(), reqwest::StatusCode::OK);
    contrat_du_lhc(&r1, taille);
    let lus1 = lhc_lit_puis_lache(r1, SONDE_SANS_RANGE).await;
    assert!(
        (SONDE_SANS_RANGE..TAILLE).contains(&lus1),
        "sonde 1 : {lus1}"
    );

    // ─── 2. La reprise après l'en-tête WAV : 206, puis abandon AUSSI. ───
    let r2 = client
        .get(&url)
        .header(reqwest::header::RANGE, "bytes=44-")
        .send()
        .await
        .expect("requête bytes=44-");
    assert_eq!(r2.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    contrat_du_lhc(&r2, taille - 44);
    assert_eq!(
        r2.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some(format!("bytes 44-{}/{}", taille - 1, taille).as_str()),
    );
    let lus2 = lhc_lit_puis_lache(r2, SONDE_BYTES_44).await;
    assert!((SONDE_BYTES_44..TAILLE).contains(&lus2), "sonde 2 : {lus2}");

    // Le LHC ne revient pas. On laisse le serveur constater les deux
    // abandons, puis on lit ce que le sondeur lira.
    let mut servis = 0u64;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let n = streamer.stream_bytes_sent(&sid).await.unwrap_or(0);
        if n == servis && n > 0 {
            break;
        }
        servis = n;
    }
    let total = streamer.stream_total_bytes(&sid).await;
    let audio_servi_ms = streamer.stream_audio_servi_ms(&sid).await;

    assert!(
        servis >= (lus1 + lus2) as u64,
        "les deux corps lus doivent être comptés : {servis} < {}",
        lus1 + lus2
    );
    assert_eq!(total, Some(taille), "la taille du flux doit être connue");
    assert!(
        servis > 0 && servis < taille,
        "🔴 LA MESURE DU FIL 1912 : des octets servis (jamais « zéro », le LHC \
         sonde toujours) et un flux INCOMPLET — {servis}/{taille}. Le sondeur \
         doit la lire comme un démarrage mort, pas comme un simple arrêt"
    );
    assert!(
        audio_servi_ms.is_some_and(|ms| ms > 0 && ms < 60_000),
        "quelques secondes d'audio servies, pas la piste : {audio_servi_ms:?}"
    );

    serveur.abort();
    drop(fichier);
}
