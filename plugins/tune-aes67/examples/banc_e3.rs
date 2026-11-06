//! Émetteur du banc d'essai de l'étape E3 (#7336) : saut, enchaînement sans
//! blanc avec changement de cadence, pause et reprise, en un seul scénario.
//!
//! ```text
//! cargo run --release -p tune-aes67 --example banc_e3 -- \
//!     <compteur 48 kHz> <piste 44,1 kHz> <piste 96 kHz> \
//!     127.0.0.1:<port RTP> 127.0.0.1:<port SAP>
//! ```
//!
//! Scénario (horloge système, pas de PTP) :
//! 1. lecture du compteur (chaque trame porte son numéro) ;
//! 2. à 500 ms, saut à 2 000 ms ; la piste 44,1 kHz est préparée en suivante ;
//! 3. quand la 44,1 kHz joue, la 96 kHz est préparée en suivante ;
//! 4. à 400 ms dans la 96 kHz : pause 300 ms, puis reprise ;
//! 5. fin naturelle.
//!
//! Écrit en JSON, sur la sortie standard, les évènements vus par la sortie
//! (instants, URI, positions), l'empreinte du PCM que le récepteur doit
//! trouver APRÈS le compteur (les deux pistes rééchantillonnées puis mises
//! bout à bout), et les statistiques d'émission.
//!
//! Refuse toute destination hors de 127.0.0.0/8 : aucun son, aucun réseau.

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};
use tune_aes67::banc;
use tune_aes67::config::FluxAes67;
use tune_aes67::droit::DroitDuBanc;
use tune_aes67::horloge::HorlogeSysteme;
use tune_aes67::sortie::{ReglagesEmission, SortieAes67};
use tune_core::outputs::traits::{OutputTarget, PlayMedia, TransportState};

fn boucle_locale(texte: &str) -> SocketAddrV4 {
    let a: SocketAddrV4 = texte.parse().expect("adresse IPv4:port attendue");
    assert!(
        a.ip().is_loopback(),
        "le banc n'émet que sur 127.0.0.0/8 : {a}"
    );
    assert!(a.port() >= 1024, "ports non privilégiés seulement : {a}");
    a
}

fn media<'a>(chemin: &'a str, uri: &'a str) -> PlayMedia<'a> {
    PlayMedia {
        url: uri,
        mime_type: "audio/wav",
        file_path: Some(chemin),
        ..Default::default()
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(
        args.len() == 6,
        "usage : banc_e3 <compteur> <44100> <96000> 127.0.0.1:<rtp> 127.0.0.1:<sap>"
    );
    let (compteur, p44, p96) = (&args[1], &args[2], &args[3]);
    let rtp = boucle_locale(&args[4]);
    let sap = boucle_locale(&args[5]);
    let flux: FluxAes67 = serde_json::from_value(json!({
        "id": "banc-e3",
        "nom": "Banc E3",
        "adresse": rtp.ip().to_string(),
        "port": rtp.port(),
        "interface": "127.0.0.1",
    }))
    .unwrap();
    let sortie = SortieAes67::new(
        flux,
        Arc::new(HorlogeSysteme::new()),
        Arc::new(DroitDuBanc),
        ReglagesEmission {
            destination_sap: sap,
            intervalle_sap: Duration::from_secs(1),
            temps_reel: true,
            avance: Duration::ZERO,
            decalage_media: 0,
        },
    )
    .expect("sortie");
    let debut = Instant::now();
    let ms = || debut.elapsed().as_secs_f64() * 1000.0;
    let mut evenements = Vec::new();

    sortie
        .play_media(&media(compteur, "file:///compteur"))
        .await
        .expect("lecture");
    evenements.push(json!({"t_ms": ms(), "quoi": "lecture compteur"}));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let avant = sortie.get_status().await.unwrap();
    sortie.seek(2_000).await.expect("saut");
    let apres = sortie.get_status().await.unwrap();
    evenements.push(json!({
        "t_ms": ms(), "quoi": "saut vers 2000 ms",
        "position_avant_ms": avant.position_ms, "position_apres_ms": apres.position_ms,
    }));
    sortie
        .set_next_media(&media(p44, "file:///p44"))
        .await
        .expect("suivante 44,1");
    evenements.push(json!({"t_ms": ms(), "quoi": "suivante 44,1 kHz préparée"}));

    let mut etape = 0;
    let mut derniere_uri = String::from("file:///compteur");
    loop {
        let s = sortie.get_status().await.unwrap();
        let uri = s.current_uri.clone().unwrap_or_default();
        if uri != derniere_uri {
            evenements.push(json!({
                "t_ms": ms(), "quoi": "bascule vue par l'état",
                "uri": uri, "position_ms": s.position_ms,
            }));
            derniere_uri = uri.clone();
        }
        if etape == 0 && uri == "file:///p44" {
            sortie
                .set_next_media(&media(p96, "file:///p96"))
                .await
                .expect("suivante 96");
            evenements.push(json!({"t_ms": ms(), "quoi": "suivante 96 kHz préparée"}));
            etape = 1;
        }
        if etape == 1 && uri == "file:///p96" && s.position_ms >= 400 {
            sortie.pause().await.unwrap();
            let p = sortie.get_status().await.unwrap();
            evenements.push(json!({
                "t_ms": ms(), "quoi": "pause", "etat": format!("{:?}", p.state),
                "position_ms": p.position_ms,
            }));
            tokio::time::sleep(Duration::from_millis(300)).await;
            let p2 = sortie.get_status().await.unwrap();
            sortie.resume().await.unwrap();
            evenements.push(json!({
                "t_ms": ms(), "quoi": "reprise", "position_avant_reprise_ms": p2.position_ms,
            }));
            etape = 2;
        }
        if s.state == TransportState::Stopped {
            evenements.push(json!({
                "t_ms": ms(), "quoi": "fin", "fin_naturelle": s.ended_naturally,
                "uri": s.current_uri, "position_ms": s.position_ms,
            }));
            break;
        }
        assert!(debut.elapsed() < Duration::from_secs(30), "pas de fin");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let echec = sortie.take_output_failure();
    let diagnostic = sortie.diagnostics_json().unwrap();
    sortie.fermer();
    let (suite, trames_par_piste) =
        banc::pcm_attendu_enchaine(&[p44, p96], 48_000, 2, 48).expect("pcm attendu");
    println!(
        "{}",
        json!({
            "evenements": evenements,
            "echec": echec,
            "emission": diagnostic["emission"],
            "sdp": diagnostic["sdp"],
            "suite_sha256_attendu": banc::hex(&Sha256::digest(&suite)),
            "suite_octets_attendus": suite.len(),
            "trames_par_piste_48k": trames_par_piste,
        })
    );
}
