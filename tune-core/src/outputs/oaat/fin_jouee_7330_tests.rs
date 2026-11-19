//! #7330 — bancs du vrai `OaatOutput` : en fin NATURELLE, le `stop` (qui
//! purge la file du récepteur) n'arrive plus avant l'échéance du dernier paquet
//! audio, sur le chemin HTTP comme sur le chemin PCM direct ; et un `stop()`
//! demandé pendant cette attente l'interrompt sur-le-champ.
//!
//! Sans le correctif, la piste part et le `stop` arrive avant le PTS du dernier
//! paquet, qui porte encore l'avance de départ (0,5 s au moins, #7331).

use std::time::Duration;

use super::banc_fin_7330::{self, Observe};
use crate::outputs::traits::{OutputTarget, PlayMedia};

const S: u64 = 1_000_000_000;

fn verifier_la_fin(o: &Observe) {
    let dernier_pts = o
        .dernier_pts_audio
        .expect("aucun paquet audio reçu : le banc n'a rien prouvé");
    let last_packet = o
        .last_packet_recu_ns
        .expect("aucun LAST_PACKET : la fin de piste n'a pas été atteinte");
    let stop = o.stop_recu_ns.expect("aucun stop reçu en fin naturelle");
    assert!(
        dernier_pts > last_packet,
        "le banc suppose que le dernier paquet est encore à jouer au LAST_PACKET"
    );
    assert!(
        stop >= dernier_pts,
        "stop reçu {} ms AVANT l'échéance du dernier paquet : le récepteur purge \
         une file encore à jouer, la fin de la piste est coupée (#7330)",
        (dernier_pts - stop) / 1_000_000
    );
    assert!(
        stop.saturating_sub(last_packet)
            <= super::fin_jouee_7330::PLAFOND_ATTENTE.as_nanos() as u64 + S,
        "l'attente avant stop doit rester bornée"
    );
}

/// Chemin HTTP (le flux WAV servi par l'orchestrateur).
#[tokio::test]
async fn la_fin_naturelle_n_envoie_pas_stop_avant_le_dernier_pts_7330() {
    let banc = banc_fin_7330::monter(500, Duration::ZERO).await;
    let output = banc.sortie();
    output
        .play_media(&PlayMedia {
            url: &banc.url,
            mime_type: "audio/wav",
            title: Some("Fin 7330"),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .expect("play_media");
    assert!(
        banc.attendre(Duration::from_secs(25), |o| o.stop_recu_ns.is_some())
            .await,
        "aucun stop en fin naturelle : {:?}",
        banc.photo()
    );
    verifier_la_fin(&banc.photo());
    output.stop().await.ok();
}

/// Chemin PCM direct (fichier WAV local lu par la sortie elle-même).
#[tokio::test]
async fn la_fin_naturelle_du_chemin_direct_attend_aussi_le_dernier_pts_7330() {
    let banc = banc_fin_7330::monter(500, Duration::ZERO).await;
    let dossier = tempfile::tempdir().unwrap();
    let fichier = dossier.path().join("fin-7330.wav");
    std::fs::write(&fichier, banc_fin_7330::wav_16_bits(500)).unwrap();
    let chemin = fichier.to_string_lossy().into_owned();
    let output = banc.sortie();
    output
        .play_media(&PlayMedia {
            url: &banc.url,
            mime_type: "audio/wav",
            title: Some("Fin 7330 direct"),
            duration_ms: Some(500),
            file_path: Some(&chemin),
            ..Default::default()
        })
        .await
        .expect("play_media");
    assert!(
        banc.attendre(Duration::from_secs(25), |o| o.stop_recu_ns.is_some())
            .await,
        "aucun stop en fin naturelle (chemin direct) : {:?}",
        banc.photo()
    );
    verifier_la_fin(&banc.photo());
    output.stop().await.ok();
}

/// Un `stop()` demandé pendant l'attente l'interrompt sur-le-champ : la
/// connexion se ferme bien avant l'échéance du dernier paquet. L'avance est
/// portée au-delà de 3 s par un RTT d'horloge de 1,6 s.
#[tokio::test]
async fn un_stop_pendant_l_attente_coupe_tout_de_suite_7330() {
    let banc = banc_fin_7330::monter(500, Duration::from_millis(800)).await;
    let output = banc.sortie();
    output
        .play_media(&PlayMedia {
            url: &banc.url,
            mime_type: "audio/wav",
            title: Some("Fin 7330 stop"),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .expect("play_media");
    assert!(
        banc.attendre(Duration::from_secs(25), |o| o.last_packet_recu_ns.is_some())
            .await,
        "aucun LAST_PACKET : {:?}",
        banc.photo()
    );
    let o = banc.photo();
    let (premier_pts, premier_recu) = o.premier.expect("premier paquet");
    assert!(
        premier_pts.saturating_sub(premier_recu) >= 3 * S,
        "le banc veut une avance d'au moins 3 s ({} ms)",
        premier_pts.saturating_sub(premier_recu) / 1_000_000
    );
    let debut = std::time::Instant::now();
    output.stop().await.ok();
    assert!(
        debut.elapsed() < Duration::from_millis(500),
        "stop() a attendu la fin jouée ({:?}) : un arrêt de l'auditeur doit couper",
        debut.elapsed()
    );
    assert!(
        banc.attendre(Duration::from_secs(2), |o| o.ferme_ns.is_some())
            .await,
        "la connexion n'a pas été fermée par stop()"
    );
    let o = banc.photo();
    assert!(
        o.ferme_ns.unwrap() < o.dernier_pts_audio.unwrap(),
        "la fermeture doit tomber AVANT l'échéance : sinon le banc n'a pas \
         interrompu l'attente"
    );
}

/// Sans paquet audio envoyé, rien à attendre : le `stop` part tout de suite.
#[tokio::test]
async fn l_attente_de_fin_n_existe_pas_sans_paquet() {
    let fin = super::fin_jouee_7330::FinJouee::default();
    let debut = std::time::Instant::now();
    fin.attendre_avant_stop("banc").await;
    assert!(debut.elapsed() < Duration::from_millis(50));
}
