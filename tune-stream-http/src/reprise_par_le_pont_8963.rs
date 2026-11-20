//! #8963 — écoute à distance par l'application (fil 2880, rc12, Linux) :
//! une piste convertie ne redémarre plus après une reprise.
//!
//! Le journal : égaliseur actif, FLAC 88,2/24 → WAV 88,2/16. Le téléphone,
//! par le pont, ouvre le flux sans `Range` ; le relais tire TOUTE la
//! conversion (73 355 924 octets en 1,4 s : il pousse au débit de la liaison
//! montante, pas à celui du lecteur). Neuf secondes plus tard, le téléphone
//! revient en `bytes=44-`, juste après l'en-tête WAV : `reprise_hors_retenue
//! reprise=44 retenue_debut=9863212`, une connexion fermée sans un octet ;
//! puis 24 `range="-"` → 404 (`debut_de_piste_perdu_piste_finie`, #6050).
//!
//! Le contrat gardé ici : un flux de longueur connue tiré par le pont (en-tête
//! `ENTETE_RELAIS`, posé par le relais et par l'écoute directe) est retenu en
//! entier ; la reprise à 44 et le redémarrage depuis le début sont servis à
//! l'octet depuis la retenue. Contre-cas : sans la marque du pont, la fenêtre
//! de toujours (180 s) s'applique, et le début d'une piste plus longue est
//! perdu — le comportement local ne change pas.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;

const DART: &str = "Dart/3.12 (dart:io)";

/// Une conversion 8 bits mono à 8 kHz, de longueur annoncée : sa fenêtre
/// ordinaire tombe au plancher de 8 Mio, qu'un flux de 10 Mo fait glisser.
async fn servir(id: &str, pcm: usize) -> (String, Arc<StreamSession>, Vec<u8>) {
    servir_avec(id, pcm, true, |_| {}).await
}

/// `adresse_du_pair` : servi comme par `aiguillage_des_flux`, qui pose
/// l'adresse du pair sur chaque requête. `avant` voit la session avant le
/// premier octet (réservation sur un budget de test).
async fn servir_avec(
    id: &str,
    pcm: usize,
    adresse_du_pair: bool,
    avant: impl FnOnce(&StreamSession),
) -> (String, Arc<StreamSession>, Vec<u8>) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 8_000,
        channels: 1,
        bit_depth: 8,
        // 8 000 o/s : la longueur annoncée tombe juste sur `pcm`.
        duration_ms: Some(pcm as u64 / 8),
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
    avant(&session);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let envoi = source.clone();
    tokio::spawn(async move {
        for bloc in envoi.chunks(32_768) {
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
        if adresse_du_pair {
            axum::serve(
                ecoute,
                router(sessions).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .ok();
        } else {
            axum::serve(ecoute, router(sessions)).await.ok();
        }
    });
    (
        format!("http://127.0.0.1:{port}/stream/{id}.wav"),
        session,
        source,
    )
}

async fn demander(url: &str, range: Option<&str>, par_le_pont: bool) -> reqwest::Response {
    envoyer(url, range, par_le_pont).await.expect("réponse")
}

async fn envoyer(
    url: &str,
    range: Option<&str>,
    par_le_pont: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", DART);
    if let Some(r) = range {
        req = req.header("Range", r);
    }
    if par_le_pont {
        req = req.header(tune_core::cloud::ENTETE_RELAIS, "1");
    }
    req.send().await
}

/// Tout le corps, ou ce qui est arrivé avant une coupure ou 10 s de silence.
async fn corps(mut r: reqwest::Response) -> Vec<u8> {
    let mut v = Vec::new();
    while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
        v.extend_from_slice(&b);
    }
    v
}

/// LE CAS DU JOURNAL : tirée en entier par le pont, puis `bytes=44-`, puis
/// un redémarrage depuis le début. Avant le correctif : 0 octet, puis 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_piste_tiree_en_entier_par_le_pont_se_reprend_a_44_et_au_debut() {
    let pcm = 10_000_000usize;
    let (url, session, source) = servir("pont-8963", pcm).await;

    let premier = corps(demander(&url, None, true).await).await;
    assert_eq!(premier.len(), 44 + pcm, "mise en scène : la piste entière");
    assert!(
        session.fin_du_canal().is_some(),
        "mise en scène : canal fini"
    );
    assert_eq!(
        session.etendue_retenue().0,
        0,
        "tirée par le pont, la piste doit être retenue depuis son premier octet"
    );

    let reprise = demander(&url, Some("bytes=44-"), true).await;
    assert_eq!(reprise.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let octets = corps(reprise).await;
    assert_eq!(
        octets.len(),
        pcm,
        "reprise à 44 par le pont : {} octets sur {pcm} — la piste est perdue (#8963)",
        octets.len()
    );
    assert!(
        octets == source[44..],
        "reprise à 44 : octets différents de la source"
    );

    let redemarrage = demander(&url, None, true).await;
    assert!(
        redemarrage.status().is_success(),
        "redémarrage par le pont : {} (#6050 rendait 404)",
        redemarrage.status()
    );
    let tout = corps(redemarrage).await;
    assert!(
        tout == source,
        "redémarrage : {} octets, pas la piste",
        tout.len()
    );
}

/// Contre-cas : la même piste tirée SANS la marque du pont garde la fenêtre
/// de toujours ; son début quitte la retenue et la reprise à 44 ne rend pas
/// la piste. Le comportement local ne change pas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_la_marque_du_pont_la_fenetre_de_toujours_s_applique() {
    let pcm = 10_000_000usize;
    let (url, session, _) = servir("local-8963", pcm).await;

    let premier = corps(demander(&url, None, false).await).await;
    assert_eq!(premier.len(), 44 + pcm);
    assert!(
        session.etendue_retenue().0 > 0,
        "sans le pont, la retenue doit avoir glissé (fenêtre de 8 Mio)"
    );
    // Le défaut du journal : la connexion peut se fermer sans même une
    // réponse (`connection closed before message completed` côté pont).
    let reprise = match envoyer(&url, Some("bytes=44-"), false).await {
        Ok(r) => corps(r).await,
        Err(_) => Vec::new(),
    };
    assert!(
        reprise.len() < pcm,
        "sans le pont, la reprise ne peut pas rendre toute la piste ({} octets)",
        reprise.len()
    );
}

/// Une radio (longueur inconnue) tirée par le pont garde sa fenêtre : la
/// retenue d'un direct sans fin ne grossit pas jusqu'au plafond.
#[test]
fn une_radio_tiree_par_le_pont_garde_sa_fenetre() {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        duration_ms: None,
        ..StreamInfo::default()
    };
    let fenetre = info.fenetre_de_retenue();
    let session = StreamSession::new("radio-8963".into(), info, false, 16);
    session.noter_tirage_par_le_pont();
    assert_eq!(session.fenetre_de_retenue_effective(), fenetre);
}

/// Le plafond : une piste plus longue que lui garde ses premiers
/// `RETENUE_PONT_MAX_OCTETS`, en plus de la fenêtre ordinaire.
#[test]
fn la_retenue_par_le_pont_est_plafonnee() {
    static BUDGET: tune_core::http::streamer::BudgetDuPont =
        tune_core::http::streamer::BudgetDuPont::new(u64::MAX);
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 192_000,
        channels: 2,
        bit_depth: 24,
        // Une demi-heure de 24/192 : 2 Go.
        duration_ms: Some(1_800_000),
        ..StreamInfo::default()
    };
    let session = StreamSession::new("long-8963".into(), info, false, 16);
    let avant = session.fenetre_de_retenue_effective();
    assert_eq!(session.tete_de_retenue_effective(), 0);
    assert!(session.reserver_pour_le_pont(
        &BUDGET,
        tune_core::http::streamer::RETENUE_PONT_MAX_OCTETS
    ));
    assert_eq!(session.fenetre_de_retenue_effective(), avant);
    assert_eq!(
        session.tete_de_retenue_effective(),
        tune_core::http::streamer::RETENUE_PONT_MAX_OCTETS
    );
}

/// Sans adresse de pair connue, la marque du pont n'est pas crue : un appareil
/// du réseau ne peut pas, en la posant, faire retenir des pistes entières.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_marque_du_pont_n_est_crue_que_d_un_pair_en_boucle_locale() {
    let pcm = 10_000_000usize;
    let (url, session, _) = servir_avec("sans-pair-8963", pcm, false, |_| {}).await;
    let premier = corps(demander(&url, None, true).await).await;
    assert_eq!(premier.len(), 44 + pcm);
    assert!(
        session.etendue_retenue().0 > 0,
        "marque crue sans adresse de pair : la retenue n'aurait pas dû s'élargir"
    );
    assert!(pair_en_boucle_locale("127.0.0.1".parse().unwrap()));
    assert!(pair_en_boucle_locale("::1".parse().unwrap()));
    assert!(pair_en_boucle_locale("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!pair_en_boucle_locale("192.168.1.20".parse().unwrap()));
    assert!(!pair_en_boucle_locale("::ffff:192.168.1.20".parse().unwrap()));
}

/// Le budget commun : une réservation qui ne tient plus est refusée, la
/// session garde la fenêtre ordinaire ; la mort d'une session rend son dû.
#[test]
fn le_budget_commun_du_pont_borne_les_retenues_et_se_rend() {
    static BUDGET: tune_core::http::streamer::BudgetDuPont =
        tune_core::http::streamer::BudgetDuPont::new(15_000_000);
    let info = || StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 8_000,
        channels: 1,
        bit_depth: 8,
        duration_ms: Some(1_250_000),
        ..StreamInfo::default()
    };
    let a = StreamSession::new("budget-a".into(), info(), false, 16);
    let b = StreamSession::new("budget-b".into(), info(), false, 16);
    let ordinaire = a.fenetre_de_retenue_effective();
    assert!(a.reserver_pour_le_pont(&BUDGET, u64::MAX));
    assert!(a.fenetre_de_retenue_effective() > ordinaire);
    assert!(
        !b.reserver_pour_le_pont(&BUDGET, u64::MAX),
        "deux pistes de 10 Mo ne tiennent pas dans 15 Mo"
    );
    assert_eq!(b.fenetre_de_retenue_effective(), ordinaire);
    assert!(a.reserver_pour_le_pont(&BUDGET, u64::MAX), "déjà réservée");
    assert_eq!(BUDGET.utilise(), a.info.wav_content_length().unwrap());
    drop(a);
    assert_eq!(BUDGET.utilise(), 0, "la session morte rend sa réservation");
    assert!(b.reserver_pour_le_pont(&BUDGET, u64::MAX));
}

/// Au-delà du plafond, le DÉBUT de la piste est gardé : la reprise à 44 est
/// servie. Plafond de 9 Mo pour une piste de 10 Mo : tête [0, 9 Mo) et
/// fenêtre ordinaire de 8 Mio se touchent, toute la piste se reprend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn au_dela_du_plafond_le_debut_de_la_piste_est_garde() {
    static BUDGET: tune_core::http::streamer::BudgetDuPont =
        tune_core::http::streamer::BudgetDuPont::new(u64::MAX);
    let pcm = 10_000_000usize;
    let (url, session, source) = servir_avec("tete-8963", pcm, true, |s| {
        assert!(s.reserver_pour_le_pont(&BUDGET, 9_000_000));
    })
    .await;
    assert_eq!(session.tete_de_retenue_effective(), 9_000_000);
    let premier = corps(demander(&url, None, false).await).await;
    assert_eq!(premier.len(), 44 + pcm);
    assert_eq!(session.etendue_retenue().0, 0, "la tête commence à 0");
    let reprise = demander(&url, Some("bytes=44-"), false).await;
    assert_eq!(reprise.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let octets = corps(reprise).await;
    assert_eq!(octets.len(), pcm, "reprise à 44 au-delà du plafond");
    assert!(octets == source[44..]);
}

/// Tête et fenêtre disjointes (plafond de 1 Mo) : la reprise à 44 rend au
/// moins toute la tête, à l'octet — de quoi démarrer, au lieu de rien.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_tete_disjointe_rend_au_moins_le_debut_a_l_octet() {
    static BUDGET: tune_core::http::streamer::BudgetDuPont =
        tune_core::http::streamer::BudgetDuPont::new(u64::MAX);
    let pcm = 10_000_000usize;
    let (url, _session, source) = servir_avec("tete-disjointe-8963", pcm, true, |s| {
        assert!(s.reserver_pour_le_pont(&BUDGET, 1_000_000));
    })
    .await;
    let premier = corps(demander(&url, None, false).await).await;
    assert_eq!(premier.len(), 44 + pcm);
    let octets = match envoyer(&url, Some("bytes=44-"), false).await {
        Ok(r) => corps(r).await,
        Err(_) => Vec::new(),
    };
    assert!(
        octets.len() >= 1_000_000 - 44,
        "reprise à 44 : {} octets, la tête n'a pas été servie",
        octets.len()
    );
    assert!(octets[..1_000_000 - 44] == source[44..1_000_000]);
}
