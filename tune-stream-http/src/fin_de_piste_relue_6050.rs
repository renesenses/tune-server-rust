//! #6050 — Tidal sur WiiM mini (DLNA, 1.0.0-rc3, fil 2198) : « la piste
//! suivante ne démarre jamais ».
//!
//! Tidal rend de l'AAC, converti en WAV CHUNKÉ (`pretranscoder_en_flac`,
//! durée inconnue). Une connexion `bytes=0-` tire toute la piste ; à la fin,
//! le renderer (`Lavf/58.76.100`) ROUVRE `bytes=0-` sur la piste en cours au
//! lieu de tirer l'URL armée par `SetNextAVTransportURI`. La retenue avait
//! glissé (piste de 235 s, retenue de 180 s), le canal était fini : la
//! connexion recevait l'en-tête WAV seul, 44 octets, puis une fin de corps.
//! Un fichier WAV vide, mais un 200 : le renderer recommençait toutes les
//! 15 s, ne passait jamais à la suivante, et la zone restait « en lecture ».
//!
//! Le contrat gardé ici : une lecture depuis le DÉBUT d'une conversion dont
//! le canal est FINI et dont le début a quitté la retenue ne peut plus rien
//! rendre de vrai. Elle reçoit un refus net (404, ce que reçoit une session
//! qui n'existe plus), pas un 200 de 44 octets : c'est l'erreur de lecture
//! qui fait passer le renderer — ou le repli du poller — à la suivante.
//!
//! Contre-cas gardé : tant que le canal n'est pas fini (la sonde de #5991,
//! producteur encore en vie), rien ne change — en-tête puis direct.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;

const LAVF: &str = "Lavf/58.76.100";

/// Une conversion 8 bits mono à 8 kHz : sa fenêtre de retenue tombe au
/// plancher de 8 Mio, qu'un flux de 10 Mo fait glisser. `duree` choisit le
/// contrat : `None` = chunké (le cas Tidal AAC), `Some` = longueur annoncée.
async fn servir(
    id: &str,
    duree: Option<u64>,
    pcm: usize,
    fermer: bool,
) -> (
    String,
    Arc<StreamSession>,
    Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 8_000,
        channels: 1,
        bit_depth: 8,
        duration_ms: duree,
        ..StreamInfo::default()
    };
    assert_eq!(
        info.fenetre_de_retenue(),
        tune_core::http::streamer::RETENUE_MIN_OCTETS
    );
    let mut source = tune_core::audio::wav::build_wav_header(1, 8_000, 8).to_vec();
    source.extend((0..pcm).map(|i| (i % 251) as u8));
    let session = Arc::new(StreamSession::new(id.into(), info, false, 16));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let garde = (!fermer).then(|| tx.clone());
    tokio::spawn(async move {
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
        garde,
    )
}

async fn depuis_zero(url: &str, quoi: &str) -> reqwest::Response {
    tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap_or_else(|e| panic!("{quoi} : aucune réponse — {e:?}"))
}

/// Lit `au_plus` octets, ou jusqu'au bout. Rend ce qui a été lu et si le
/// corps a fini.
async fn lire(r: &mut reqwest::Response, au_plus: usize) -> (usize, bool) {
    let mut recu = 0usize;
    while recu < au_plus {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => return (recu, false),
            Ok(Err(_)) => return (recu, true),
            Ok(Ok(None)) => return (recu, true),
            Ok(Ok(Some(b))) => recu += b.len(),
        }
    }
    (recu, false)
}

/// LE CAS DU JOURNAL : la piste entière tirée, puis `bytes=0-` en fin de
/// piste. Avant le correctif : 200, 44 octets, et le renderer boucle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_relecture_du_debut_apres_la_fin_d_une_conversion_chunkee_est_refusee() {
    let pcm = 10_000_000usize;
    let (url, session, _) = servir("fin-relue-chunke", None, pcm, true).await;

    let mut lecture = depuis_zero(&url, "lecture").await;
    assert!(
        lecture.headers().get("Content-Length").is_none(),
        "mise en scène : la conversion Tidal AAC part en chunké"
    );
    let (lu, fini) = lire(&mut lecture, usize::MAX).await;
    assert!(fini, "lecture : le corps doit finir");
    assert_eq!(lu, 44 + pcm, "lecture : la piste entière");
    drop(lecture);
    assert!(
        session.fin_du_canal().is_some(),
        "mise en scène : la fin du canal est connue"
    );
    let (debut_retenue, _) = session.etendue_retenue();
    assert!(debut_retenue > 0, "mise en scène : la retenue a glissé");

    // Le WiiM rouvre la piste EN COURS.
    let mut relue = depuis_zero(&url, "relecture").await;
    let statut = relue.status();
    let (lu, _) = lire(&mut relue, 1_000_000).await;
    assert!(
        !statut.is_success(),
        "relecture du début d'une conversion finie, début perdu : {statut} et {lu} octets — \
         un WAV vide en 200 fait reboucler le renderer sur la piste en cours au lieu de \
         passer à la suivante (#6050)"
    );
    assert_eq!(statut, reqwest::StatusCode::NOT_FOUND);
}

/// Le même cas sur une conversion à longueur annoncée : l'en-tête puis un
/// direct vide finissait le corps 10 Mo avant son `Content-Length`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_relecture_du_debut_apres_la_fin_d_une_conversion_bornee_est_refusee() {
    let pcm = 10_000_000usize;
    // 1 250 s × 8 000 o/s = 10 000 000 octets : la longueur tombe juste.
    let (url, session, _) = servir("fin-relue-bornee", Some(1_250_000), pcm, true).await;

    let mut lecture = depuis_zero(&url, "lecture").await;
    assert!(
        lecture.headers().get("Content-Length").is_some(),
        "mise en scène : longueur annoncée"
    );
    let (lu, fini) = lire(&mut lecture, usize::MAX).await;
    assert!(fini, "lecture : le corps doit finir");
    assert_eq!(lu, 44 + pcm, "lecture : la piste entière");
    drop(lecture);
    assert!(session.fin_du_canal().is_some());
    assert!(session.etendue_retenue().0 > 0);

    let relue = depuis_zero(&url, "relecture").await;
    assert_eq!(
        relue.status(),
        reqwest::StatusCode::NOT_FOUND,
        "relecture du début d'une conversion finie, début perdu (#6050)"
    );
}

/// Contre-cas : le canal n'est PAS fini (producteur encore en vie). La
/// relecture garde le comportement de #5991 — 200, l'en-tête, puis le direct.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_relecture_du_debut_canal_vivant_recoit_toujours_l_entete_puis_le_direct() {
    let pcm = 10_000_000usize;
    // `garde` tient l'émetteur ouvert : le canal ne finit jamais.
    let (url, session, _garde) = servir("fin-relue-vivant", None, pcm, false).await;

    let mut sonde = depuis_zero(&url, "sonde").await;
    let (lu, _) = lire(&mut sonde, 9_500_000).await;
    assert!(lu >= 9_500_000, "sonde : {lu} octets");
    drop(sonde);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        session.fin_du_canal().is_none(),
        "mise en scène : canal vivant"
    );
    assert!(
        session.etendue_retenue().0 > 0,
        "mise en scène : retenue glissée"
    );

    let mut relue = depuis_zero(&url, "relecture").await;
    assert_eq!(relue.status(), reqwest::StatusCode::OK);
    let (lu, _) = lire(&mut relue, 44).await;
    assert!(lu >= 44, "relecture : l'en-tête au moins ({lu} octets)");
}
