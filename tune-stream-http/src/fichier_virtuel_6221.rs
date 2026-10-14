//! #6221 — la branche fichier sert un fichier VIRTUEL (le DSF fabriqué depuis
//! un DST) avec le même contrat qu'un fichier sur disque : `Content-Length`
//! exact, `Accept-Ranges`, `206` + `Content-Range` exacts pour une tranche au
//! milieu, `416` hors fichier — et jamais les octets du fichier source.
//!
//! La source virtuelle est ici un motif déterministe : ce module garde la
//! mécanique HTTP ; l'exactitude du DSF lui-même (en-tête, octets du DST
//! décodé, tranches) est gardée dans `tune-core` (`dsf_depuis_dst_tests_6221`).
use super::*;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use std::sync::Arc;
use tune_core::http::streamer::{SharedSessions, SourceVirtuelle};

const TOTAL: u64 = 300_000;

fn octet(i: u64) -> u8 {
    (i.wrapping_mul(31).wrapping_add(7) % 251) as u8
}

#[derive(Debug)]
struct Motif;

impl SourceVirtuelle for Motif {
    fn total(&self) -> u64 {
        TOTAL
    }
    fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        let fin = (debut + longueur).min(TOTAL);
        let mut i = debut;
        while i < fin {
            let n = (fin - i).min(8192);
            rendre((i..i + n).map(octet).collect())?;
            i += n;
        }
        Ok(())
    }
}

async fn demander(range: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let id = format!(
        "v6221-{}",
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let bac = tune_core::test_scratch::scratch_dir("tune-6221-virtuel");
    // Le fichier SOURCE : court, et d'un tout autre contenu. Rien de lui ne
    // doit partir sur le fil.
    let source = bac.join("piste.dff");
    std::fs::write(&source, b"FRM8 ceci est du DST").unwrap();
    let info = StreamInfo {
        format: "dsf".into(),
        mime_type: "audio/x-dsf".into(),
        sample_rate: 2_822_400,
        bit_depth: 1,
        channels: 2,
        file_size: Some(TOTAL),
        dsd_reconstruit_depuis_dst: true,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.clone(), info, false, 8));
    *session.file_path.lock().await = Some(source.to_string_lossy().into_owned());
    *session.source_virtuelle.lock().unwrap() = Some(Arc::new(Motif));
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.clone(), session)].into_iter().collect(),
    ));
    let mut h = HeaderMap::new();
    if let Some(r) = range {
        h.insert("Range", r.parse().unwrap());
    }
    let rep = handle_stream(Path(format!("{id}.dsf")), State(sessions), h).await;
    let statut = rep.status();
    let entetes = rep.headers().clone();
    let mut flux = rep.into_body().into_data_stream();
    let mut corps = Vec::new();
    while let Some(m) = flux.next().await {
        corps.extend_from_slice(&m.unwrap());
    }
    drop(bac);
    (statut, entetes, corps)
}

fn entete(h: &HeaderMap, nom: &str) -> String {
    h.get(nom).unwrap().to_str().unwrap().to_string()
}

/// Sans Range : 200, la taille du fichier VIRTUEL, ses octets.
#[tokio::test]
async fn sans_range_le_fichier_virtuel_part_entier() {
    let (statut, h, corps) = demander(None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(entete(&h, "Content-Length"), TOTAL.to_string());
    assert_eq!(entete(&h, "Accept-Ranges"), "bytes");
    assert_eq!(entete(&h, "Content-Type"), "audio/x-dsf");
    assert_eq!(corps.len() as u64, TOTAL);
    assert!(corps.iter().enumerate().all(|(i, o)| *o == octet(i as u64)));
}

/// 🔴 Une tranche au MILIEU : 206, `Content-Range` exact, les octets du
/// fichier virtuel à ces positions.
#[tokio::test]
async fn une_tranche_au_milieu_rend_un_206_exact() {
    let (statut, h, corps) = demander(Some("bytes=150001-170000")).await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 150001-170000/{TOTAL}")
    );
    assert_eq!(entete(&h, "Content-Length"), "20000");
    let attendu: Vec<u8> = (150_001..=170_000).map(octet).collect();
    assert_eq!(corps, attendu);
}

/// Une reprise ouverte `bytes=N-` va jusqu'à la fin du fichier virtuel (pas
/// du fichier source, bien plus court).
#[tokio::test]
async fn une_reprise_ouverte_va_jusqu_au_bout_du_fichier_virtuel() {
    let (statut, h, corps) = demander(Some("bytes=299000-")).await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 299000-{}/{TOTAL}", TOTAL - 1)
    );
    assert_eq!(corps.len(), 1000);
    assert_eq!(corps[0], octet(299_000));
}

/// Hors fichier : 416 `bytes */total`, la taille VIRTUELLE.
#[tokio::test]
async fn hors_du_fichier_virtuel_416() {
    let (statut, h, _) = demander(Some(&format!("bytes={TOTAL}-"))).await;
    assert_eq!(statut, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(entete(&h, "Content-Range"), format!("bytes */{TOTAL}"));
}
