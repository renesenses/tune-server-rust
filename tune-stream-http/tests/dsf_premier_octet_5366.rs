//! #5366 — un DSF servi en passthrough part EN FLUX : son premier octet ne
//! dépend pas de la taille du fichier.
//!
//! # Le signalement
//!
//! Fil 2019 (28/09/2026, Tune 0.9.167, tuneOS) : « Combien de temps pour la
//! préparation de la lecture pour lire du DSD 64 », vers un Diretta Renderer
//! sur la même machine. Le journal joint donne `playback_timing total_ms=72`
//! puis `254` : Tune prépare en un quart de seconde, et sert le `.dsf` tel
//! quel (`passthrough=true`, `audio/x-dsd`). Le renderer lit la fin du
//! fichier, puis `bytes=28-` : 3 Mio, il raccroche, et revient sur un autre
//! port — son processus a redémarré.
//!
//! # Ce que ce témoin fixe
//!
//! La seule part de Tune dans ce délai est le service du fichier. Ce témoin
//! la mesure sur un DSF DSD64 synthétique de 96 Mio, par les deux requêtes
//! du Diretta Renderer du journal (`bytes=<fin>-`, puis `bytes=28-`), par
//! `handle_stream` lui-même : aucune conversion, aucune lecture du fichier
//! entier avant le premier octet. Le premier morceau du corps fait au plus
//! un tampon de lecture (64 Kio) et arrive bien avant que le fichier ait pu
//! être lu en entier. Il ne dit RIEN du renderer : sa chute reste à établir
//! par son propre journal (voir l'issue).
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

/// En-tête DSF minimal (DSD64 stéréo, blocs de 4096 octets) suivi de
/// `octets_donnees` octets DSD.
fn dsf_synthetique(octets_donnees: usize) -> Vec<u8> {
    let canaux: u32 = 2;
    let taux: u32 = 2_822_400;
    let echantillons = (octets_donnees as u64 / canaux as u64) * 8;
    let mut buf = Vec::with_capacity(octets_donnees + 92);
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(92 + octets_donnees as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&canaux.to_le_bytes());
    buf.extend_from_slice(&taux.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&echantillons.to_le_bytes());
    buf.extend_from_slice(&4096u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + octets_donnees as u64).to_le_bytes());
    let mut s: u32 = 0x5366;
    buf.extend((0..octets_donnees).map(|_| {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 24) as u8
    }));
    buf
}

/// Une requête `Range` sur la session ; rend (délai du premier morceau,
/// taille du premier morceau, octets reçus en tout, délai total).
async fn servir(
    sessions: &SharedSessions,
    range: &str,
    jusqu_a: Option<usize>,
) -> (u128, usize, usize, u128) {
    let mut entetes = axum::http::HeaderMap::new();
    entetes.insert("Range", range.parse().unwrap());
    entetes.insert("User-Agent", "DirettaRenderer/1.0".parse().unwrap());
    let t = Instant::now();
    let reponse =
        tune_stream_http::handle_stream(Path("r5366.dsf".into()), State(sessions.clone()), entetes)
            .await;
    assert!(
        reponse.status().is_success(),
        "{range} : statut {}",
        reponse.status()
    );
    let mut corps = reponse.into_body().into_data_stream();
    let premier = corps
        .next()
        .await
        .expect("le corps doit rendre un premier morceau")
        .expect("erreur de flux");
    let premier_us = t.elapsed().as_micros();
    let mut recus = premier.len();
    while let Some(m) = corps.next().await {
        recus += m.expect("erreur de flux").len();
        if jusqu_a.is_some_and(|n| recus >= n) {
            break; // le renderer raccroche (3 Mio dans le journal)
        }
    }
    (premier_us, premier.len(), recus, t.elapsed().as_micros())
}

#[tokio::test]
async fn un_dsf_en_passthrough_part_en_flux_sans_lecture_prealable_du_fichier() {
    const DONNEES: usize = 96 * 1024 * 1024;
    let fichier = tune_core::test_scratch::scratch_file("r5366-dsf", ".dsf");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, dsf_synthetique(DONNEES)).expect("fichier de test");
    let taille = std::fs::metadata(&chemin).unwrap().len() as usize;

    let info = StreamInfo {
        format: "dsf".into(),
        mime_type: "audio/x-dsd".into(),
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new("r5366".into(), info, false, 8));
    *session.file_path.lock().await = Some(chemin.to_string_lossy().into_owned());
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("r5366".to_string(), session.clone())]
            .into_iter()
            .collect(),
    ));

    // 1) La fin du fichier (métadonnées), comme `bytes=506904668-` au journal.
    let (p1, _, recus1, _) = servir(&sessions, &format!("bytes={}-", taille - 2215), None).await;
    assert_eq!(recus1, 2215, "la fin du fichier doit être servie entière");

    // 2) La lecture des données : `bytes=28-`, 3 Mio puis le renderer raccroche.
    let (p2, taille_premier, recus2, total2) =
        servir(&sessions, "bytes=28-", Some(3 * 1024 * 1024)).await;
    // 3) Tout le fichier, pour comparer le premier octet à une lecture entière.
    let (_, _, recus3, total3) = servir(&sessions, "bytes=28-", None).await;
    assert_eq!(
        recus3,
        taille - 28,
        "le corps doit pouvoir servir tout le fichier"
    );

    eprintln!(
        "r5366 dsf={taille}o premier_octet_fin={:.3}ms premier_octet_donnees={:.3}ms \
         premier_morceau={taille_premier}o 3Mio_en={:.3}ms fichier_entier_en={:.3}ms",
        p1 as f64 / 1000.0,
        p2 as f64 / 1000.0,
        total2 as f64 / 1000.0,
        total3 as f64 / 1000.0,
    );
    assert!(recus2 >= 3 * 1024 * 1024);
    assert!(
        taille_premier <= 64 * 1024,
        "le premier morceau d'un DSF en passthrough fait {taille_premier} octets : \
         le corps doit partir par tampons de 64 Kio, pas après lecture du fichier"
    );
    // Le premier octet part avant qu'un dixième du fichier entier ait pu être
    // servi : il ne dépend pas de la taille du fichier.
    assert!(
        p2 * 10 < total3,
        "premier octet des données à {p2} µs pour un fichier servi en {total3} µs : \
         le service d'un DSF en passthrough attend la lecture du fichier"
    );
    drop(fichier);
}
