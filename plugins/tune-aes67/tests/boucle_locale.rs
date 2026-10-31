//! Essai d'intégration en boucle locale (#7336, étape E1) :
//! émetteur → récepteur de test → empreinte.
//!
//! Aucun réseau extérieur, aucun son : le flux part en UNICAST vers
//! 127.0.0.1 sur un port libre non privilégié, l'annonce SAP aussi. Le
//! récepteur ne joue rien ; il compare le PCM reçu à celui que le décodeur
//! rend hors réseau pour le même fichier.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_aes67::banc::{self, RecepteurDeTest};
use tune_aes67::config::FluxAes67;
use tune_aes67::droit::DroitDuBanc;
use tune_aes67::horloge::HorlogeSysteme;
use tune_aes67::l24;
use tune_aes67::sap::{self, TypeSap};
use tune_aes67::sdp;
use tune_aes67::sortie::{ReglagesEmission, SortieAes67};
use tune_core::outputs::traits::{OutputTarget, PlayMedia, TransportState};

fn flux_vers(destination: SocketAddrV4) -> FluxAes67 {
    serde_json::from_value(serde_json::json!({
        "id": "banc",
        "nom": "Banc",
        "adresse": destination.ip().to_string(),
        "port": destination.port(),
        "interface": "127.0.0.1",
    }))
    .unwrap()
}

fn locale() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
}

async fn attendre_la_fin(sortie: &SortieAes67, limite: Duration) {
    let debut = Instant::now();
    loop {
        let s = sortie.get_status().await.unwrap();
        if s.state == TransportState::Stopped && s.ended_naturally {
            return;
        }
        if s.state == TransportState::Stopped
            && let Some(e) = sortie.take_output_failure()
        {
            panic!("lecture en échec : {e}");
        }
        assert!(debut.elapsed() < limite, "pas de fin naturelle : {s:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_fichier_44_1_khz_arrive_intact_en_l24_48_khz_annonce_par_sap() {
    let dossier = tempfile::tempdir().unwrap();
    let wav = dossier.path().join("sinus-44100.wav");
    // Gauche 1 kHz, droite 440 Hz : un canal inversé se verrait.
    banc::ecrire_wav_sinus(&wav, 44_100, &[1000.0, 440.0], Duration::from_millis(1500)).unwrap();
    let chemin = wav.to_str().unwrap();

    let rtp = RecepteurDeTest::ecouter(locale()).unwrap();
    let sap_ecoute = RecepteurDeTest::ecouter(locale()).unwrap();
    let flux = flux_vers(rtp.adresse());
    let sortie = SortieAes67::new(
        flux.clone(),
        Arc::new(HorlogeSysteme::new()),
        Arc::new(DroitDuBanc),
        ReglagesEmission {
            destination_sap: sap_ecoute.adresse(),
            intervalle_sap: Duration::from_millis(500),
            temps_reel: false,
            avance: Duration::ZERO,
            decalage_media: 0,
        },
    )
    .unwrap();

    let debut = Instant::now();
    sortie
        .play_media(&PlayMedia {
            url: "file:///inutile",
            mime_type: "audio/wav",
            file_path: Some(chemin),
            ..Default::default()
        })
        .await
        .unwrap();
    attendre_la_fin(&sortie, Duration::from_secs(10)).await;
    let duree = debut.elapsed();
    let statut = sortie.get_status().await.unwrap();
    let diagnostic = sortie.diagnostics_json().unwrap();
    sortie.fermer();
    drop(sortie);
    std::thread::sleep(Duration::from_millis(100));
    let capture = rtp.terminer();
    let annonces = sap_ecoute.terminer();

    // Ce que le décodeur rend hors réseau, au même format.
    let attendu = banc::pcm_attendu(chemin, 48_000, 2, 48).unwrap();
    let analyse = capture.analyser(48, Duration::from_millis(1));
    println!(
        "analyse = {}",
        serde_json::to_string_pretty(&analyse).unwrap()
    );
    println!("emission = {}", diagnostic["emission"]);
    println!("duree de lecture = {duree:?}");

    // 1. Le transport : rien de perdu, dupliqué ni déplacé.
    assert_eq!(analyse.erreurs_d_en_tete, 0);
    assert_eq!(analyse.pertes, 0);
    assert_eq!(analyse.desordres, 0);
    assert_eq!(analyse.sauts_d_horodatage, 0);
    // Un départ marqué, plus une marque par resynchronisation. En build de
    // débogage, le rééchantillonneur sinc peut décoder moins vite que le
    // temps réel : le cadenceur se tait alors et repart marqué, SANS perdre
    // d'échantillon (l'empreinte plus bas le prouve). En release, le banc
    // (`banc/banc_aes67.py`) exige un seul départ.
    let resynchronisations = diagnostic["emission"]["resynchronisations"]
        .as_u64()
        .unwrap();
    assert_eq!(analyse.marqueurs as u64, 1 + resynchronisations);
    assert_eq!(
        analyse.types_de_charge.iter().copied().collect::<Vec<_>>(),
        vec![98]
    );
    assert_eq!(analyse.ssrc.len(), 1);
    assert_eq!(
        analyse
            .tailles_de_charge
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![288],
        "1 ms = 48 trames stéréo L24"
    );
    assert_eq!(analyse.octets_pcm, attendu.len());
    assert_eq!(
        analyse.sha256_pcm,
        banc::hex(&<sha2::Sha256 as sha2::Digest>::digest(&attendu)),
        "empreinte du PCM reçu = empreinte attendue"
    );

    // 2. La cadence : 1,5 s de musique en ~1,5 s, un paquet par ms en moyenne.
    let paquets_attendus = (1.5f64 * 48_000.0 / 48.0).ceil() as usize;
    assert!(
        analyse.paquets.abs_diff(paquets_attendus) <= 2,
        "{} paquets pour {paquets_attendus} attendus",
        analyse.paquets
    );
    assert!(
        duree >= Duration::from_millis(1450),
        "émis trop vite : {duree:?}"
    );
    assert!(
        (analyse.intervalle_moyen_us - 1000.0).abs() < 50.0,
        "intervalle moyen {} µs",
        analyse.intervalle_moyen_us
    );
    assert_eq!(statut.position_ms, analyse.paquets as u64);

    // 3. Le rééchantillonnage : à 48 kHz, la gauche est un 1 kHz (et non un
    // 1088 Hz, ce que donnerait du 44,1 kHz relabellisé en 48 kHz), la droite
    // un 440 Hz.
    let pcm = l24::lire_l24(&capture.pcm());
    let canal = |c: usize| -> Vec<f64> {
        pcm.iter()
            .skip(c)
            .step_by(2)
            .skip(4800)
            .take(48_000)
            .map(|&v| f64::from(v) / 8_388_608.0)
            .collect()
    };
    let (g, d) = (canal(0), canal(1));
    let g_1000 = banc::puissance_a(&g, 48_000, 1000.0);
    let g_1088 = banc::puissance_a(&g, 48_000, 1000.0 * 48_000.0 / 44_100.0);
    let d_440 = banc::puissance_a(&d, 48_000, 440.0);
    let d_1000 = banc::puissance_a(&d, 48_000, 1000.0);
    assert!(g_1000 > 100.0 * g_1088, "gauche : {g_1000} contre {g_1088}");
    assert!(d_440 > 100.0 * d_1000, "droite : {d_440} contre {d_1000}");
    // Amplitude ~0,5 (−6 dBFS) conservée : puissance Goertzel ≈ A²/4.
    assert!(
        (g_1000 - 0.0625).abs() < 0.01,
        "amplitude gauche : {g_1000}"
    );

    // 4. SAP : des annonces dont le SDP décrit exactement ce flux, puis le
    // retrait.
    let sap_lus: Vec<sap::SapLu> = annonces
        .datagrammes
        .iter()
        .map(|d| sap::lire(&d.donnees).unwrap())
        .collect();
    assert!(sap_lus.len() >= 3, "{} paquets SAP", sap_lus.len());
    assert!(
        sap_lus[..sap_lus.len() - 1]
            .iter()
            .all(|s| s.type_sap == TypeSap::Annonce)
    );
    assert_eq!(sap_lus.last().unwrap().type_sap, TypeSap::Retrait);
    let lu = sdp::lire(&sap_lus[0].sdp).unwrap();
    assert_eq!(lu.nom, "Tune - Banc");
    assert_eq!(lu.destination, Some(Ipv4Addr::LOCALHOST));
    assert_eq!(lu.port, flux.port);
    assert_eq!(lu.type_charge, 98);
    assert_eq!(
        (lu.codage.as_str(), lu.cadence, lu.canaux),
        ("L24", 48_000, 2)
    );
    assert_eq!(lu.ptime, "1");
    assert_eq!(lu.ts_refclk, "local");
    assert_eq!(lu.mediaclk, "direct=0");
    assert_eq!(sap_lus[0].source, Ipv4Addr::LOCALHOST);

    // 5. Le pcap s'écrit (pour Wireshark / l'analyseur du banc).
    let pcap = dossier.path().join("flux.pcap");
    capture.ecrire_pcap(&pcap).unwrap();
    let taille = std::fs::metadata(&pcap).unwrap().len() as usize;
    assert_eq!(taille, 24 + analyse.paquets * (16 + 20 + 8 + 12 + 288));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_reprise_et_volume_logiciel() {
    let dossier = tempfile::tempdir().unwrap();
    let wav = dossier.path().join("sinus-48000.wav");
    banc::ecrire_wav_sinus(&wav, 48_000, &[1000.0, 1000.0], Duration::from_millis(600)).unwrap();
    let chemin = wav.to_str().unwrap();
    let rtp = RecepteurDeTest::ecouter(locale()).unwrap();
    let sap_ecoute = RecepteurDeTest::ecouter(locale()).unwrap();
    let mut flux = flux_vers(rtp.adresse());
    flux.volume_logiciel = true;
    let sortie = SortieAes67::new(
        flux,
        Arc::new(HorlogeSysteme::new()),
        Arc::new(DroitDuBanc),
        ReglagesEmission {
            destination_sap: sap_ecoute.adresse(),
            intervalle_sap: Duration::from_secs(3600),
            temps_reel: false,
            avance: Duration::ZERO,
            decalage_media: 0,
        },
    )
    .unwrap();
    assert!(sortie.capabilities().can_set_volume);
    assert!(sortie.capabilities().can_seek);
    // Volume 50 % : −30 dB sur la loi perceptive de Tune, avant le départ.
    sortie.set_volume(0.5).await.unwrap();
    sortie
        .play_media(&PlayMedia {
            url: "file:///inutile",
            mime_type: "audio/wav",
            file_path: Some(chemin),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    sortie.pause().await.unwrap();
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Paused
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    sortie.resume().await.unwrap();
    attendre_la_fin(&sortie, Duration::from_secs(10)).await;
    let diagnostic = sortie.diagnostics_json().unwrap();
    sortie.fermer();
    std::thread::sleep(Duration::from_millis(50));
    let capture = rtp.terminer();
    let _ = sap_ecoute.terminer();
    let analyse = capture.analyser(48, Duration::from_millis(1));
    println!(
        "analyse = {}",
        serde_json::to_string_pretty(&analyse).unwrap()
    );
    assert_eq!(analyse.pertes, 0);
    assert_eq!(analyse.sauts_d_horodatage, 0);
    // Départ, puis reprise marquée (comptée comme resynchronisation), plus
    // celles d'un décodeur en retard sur une machine chargée.
    let resynchronisations = diagnostic["emission"]["resynchronisations"]
        .as_u64()
        .unwrap();
    assert!(resynchronisations >= 1, "la reprise repart marquée");
    assert_eq!(analyse.marqueurs as u64, 1 + resynchronisations);
    // Toute la piste est passée, pause comprise : 600 paquets.
    assert_eq!(analyse.paquets, 600);
    let pcm = l24::lire_l24(&capture.pcm());
    let crete = pcm.iter().map(|v| v.unsigned_abs()).max().unwrap();
    let attendue = 0.5 * 0.031_622_8 * 8_388_607.0; // 0,5 × 10^(−30/20)
    assert!(
        (f64::from(crete) - attendue).abs() / attendue < 0.02,
        "crête {crete}, attendue {attendue}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_premium_la_lecture_est_refusee() {
    struct Refus;
    #[async_trait::async_trait]
    impl tune_aes67::droit::DroitPremium for Refus {
        async fn autorise(&self) -> bool {
            false
        }
    }
    let rtp = RecepteurDeTest::ecouter(locale()).unwrap();
    let sap_ecoute = RecepteurDeTest::ecouter(locale()).unwrap();
    let sortie = SortieAes67::new(
        flux_vers(rtp.adresse()),
        Arc::new(HorlogeSysteme::new()),
        Arc::new(Refus),
        ReglagesEmission {
            destination_sap: sap_ecoute.adresse(),
            ..ReglagesEmission::default()
        },
    )
    .unwrap();
    let e = sortie
        .play_media(&PlayMedia {
            url: "file:///inutile",
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.contains("premium_required"), "{e}");
    sortie.fermer();
    assert_eq!(
        rtp.terminer().datagrammes.len(),
        0,
        "aucun RTP sans Premium"
    );
    let _ = sap_ecoute.terminer();
}
