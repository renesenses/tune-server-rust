//! Émetteur du banc d'essai (#7336, étapes E0-E1).
//!
//! Joue UN fichier vers un récepteur en UNICAST sur la boucle locale, annonce
//! en SAP vers un autre port local, puis écrit sur la sortie standard, en
//! JSON, l'empreinte du PCM que le récepteur doit trouver.
//!
//! ```text
//! cargo run -p tune-aes67 --example banc_emetteur -- \
//!     <fichier> 127.0.0.1:<port RTP> 127.0.0.1:<port SAP>
//! ```
//!
//! Refuse toute destination hors de 127.0.0.0/8 : le banc n'émet jamais sur
//! le réseau, et aucun son n'est joué nulle part.

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(
        args.len() == 4,
        "usage : banc_emetteur <fichier> 127.0.0.1:<rtp> 127.0.0.1:<sap>"
    );
    let fichier = &args[1];
    let rtp = boucle_locale(&args[2]);
    let sap = boucle_locale(&args[3]);
    let flux: FluxAes67 = serde_json::from_value(serde_json::json!({
        "id": "banc",
        "nom": "Banc",
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
    sortie
        .play_media(&PlayMedia {
            url: "file:///banc",
            mime_type: "audio/*",
            file_path: Some(fichier),
            ..Default::default()
        })
        .await
        .expect("lecture");
    loop {
        let s = sortie.get_status().await.unwrap();
        if s.state == TransportState::Stopped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let duree = debut.elapsed();
    let echec = sortie.take_output_failure();
    let diagnostic = sortie.diagnostics_json().unwrap();
    sortie.fermer();
    let attendu = banc::pcm_attendu(fichier, 48_000, 2, 48).expect("pcm attendu");
    println!(
        "{}",
        serde_json::json!({
            "sha256_attendu": banc::hex(&Sha256::digest(&attendu)),
            "octets_attendus": attendu.len(),
            "duree_s": duree.as_secs_f64(),
            "echec": echec,
            "emission": diagnostic["emission"],
            "sdp": diagnostic["sdp"],
        })
    );
}
