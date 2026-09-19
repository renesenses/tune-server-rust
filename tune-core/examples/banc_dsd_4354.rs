//! Banc de débit de la conversion DSD → PCM, étage par étage (#4354).
//!
//! Sur le .42 (Windows, Core i5), un DSD128 converti en WAV 352,8 kHz / 24 bits
//! pour un renderer DLNA sortait à ~0,6× le temps réel : son haché. Le banc
//! criterion (`benches/dsd_to_pcm.rs`) ne mesure que `feed()` sur des octets
//! synthétiques ; il ne dit pas où part le temps sur la chaîne réellement
//! parcourue par la lecture. Ce binaire la découpe :
//!
//! - **A — lecture** : `DsfStreamReader` seul (lecture disque + désentrelacement
//!   des blocs DSF) ;
//! - **B — lecture + FIR** : A puis `DsdToPcmStreamer::feed()` / `flush()` ;
//! - **C — chaîne complète** : `decode_to_pcm_streaming_seeked(.., Some(taux),
//!   Some(2), Some(24), .., 32768, ..)` — l'appel du chemin réseau, WAV
//!   progressif 24 bits —, consommateur qui draine, fenêtres de niveaux drainées.
//!
//! Chaque étage rend : secondes d'audio, secondes de mur, facteur temps réel,
//! échantillons PCM par seconde, et l'empreinte SHA-256 des octets produits
//! (B et C) — c'est elle qui prouve qu'un correctif rend le MÊME PCM au bit près.
//!
//! À lancer en profil **release** (le seul qui dise quelque chose d'un débit) :
//!
//! ```text
//! cargo run --release -p tune-core --example banc_dsd_4354 -- <fichier.dsf> [passages]
//! cargo run --release -p tune-core --example banc_dsd_4354 -- --synth <secondes> <taux_dsd> [passages]
//! ```
//!
//! `--synth` écrit un DSF stéréo pseudo-aléatoire (LCG déterministe, même
//! graine à chaque fois : même empreinte d'une machine à l'autre) dans le
//! répertoire temporaire, puis le mesure comme un vrai fichier.

use sha2::{Digest, Sha256};
use std::time::Instant;
use tune_core::audio::dsd_to_pcm::{DsdToPcmStreamer, choose_output_rate};
use tune_core::audio::dsf::{DsfStreamReader, parse_dsf};

fn ecrire_dsf_synthetique(path: &std::path::Path, secondes: u32, taux_dsd: u32) {
    let canaux: u32 = 2;
    let bloc: u32 = 4096;
    let octets_par_canal = (taux_dsd as u64 / 8) * secondes as u64;
    let blocs = octets_par_canal.div_ceil(bloc as u64);
    let mut data = Vec::with_capacity((blocs * bloc as u64 * canaux as u64) as usize);
    let mut s: u32 = 0x1234_5678;
    for _ in 0..blocs * canaux as u64 {
        for _ in 0..bloc {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((s >> 24) as u8);
        }
    }
    let total_samples = octets_par_canal * 8;
    let mut buf = Vec::with_capacity(data.len() + 92);
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&canaux.to_le_bytes());
    buf.extend_from_slice(&taux_dsd.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&bloc.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(path, &buf).expect("écriture du DSF synthétique");
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn ligne(etage: &str, audio_s: f64, mur_s: f64, echantillons: u64, empreinte: &str) {
    println!(
        "{etage:<22} audio_s={audio_s:.3} mur_s={mur_s:.4} temps_reel={:.3}x echantillons_pcm_par_s={:.0} sha256={empreinte}",
        audio_s / mur_s,
        echantillons as f64 / mur_s,
    );
}

fn etage_lecture(path: &str) -> (u64, f64) {
    let info = parse_dsf(path).expect("parse_dsf");
    let mut r = DsfStreamReader::open(path, info).expect("open");
    let t = Instant::now();
    let mut n = 0u64;
    while let Some(c) = r.next_chunk().expect("lecture") {
        n += c.len() as u64;
        std::hint::black_box(&c);
    }
    (n, t.elapsed().as_secs_f64())
}

fn etage_fir(path: &str, taux_sortie: u32) -> (u64, f64, String) {
    let info = parse_dsf(path).expect("parse_dsf");
    let (taux, canaux) = (info.sample_rate, info.channels as usize);
    let mut r = DsfStreamReader::open(path, info).expect("open");
    let mut st = DsdToPcmStreamer::new(taux, taux_sortie, canaux, true);
    let mut h = Sha256::new();
    let mut octets = 0u64;
    let t = Instant::now();
    while let Some(c) = r.next_chunk().expect("lecture") {
        let pcm = st.feed(&c);
        octets += pcm.len() as u64;
        h.update(&pcm);
    }
    let fin = st.flush();
    octets += fin.len() as u64;
    h.update(&fin);
    (octets / 3, t.elapsed().as_secs_f64(), hex(&h.finalize()))
}

fn etage_chaine(rt: &tokio::runtime::Runtime, path: &str, taux_sortie: u32) -> (u64, f64, String) {
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let (ltx, mut lrx) = tokio::sync::mpsc::unbounded_channel();
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let conso = tokio::spawn(async move {
            let mut h = Sha256::new();
            let mut n = 0u64;
            while let Some(c) = rx.recv().await {
                n += c.len() as u64;
                h.update(&c);
            }
            (n, hex(&h.finalize()))
        });
        let niveaux = tokio::spawn(async move { while lrx.recv().await.is_some() {} });
        let p = path.to_string();
        let t = Instant::now();
        tokio::task::spawn_blocking(move || {
            tune_core::audio::decode::decode_to_pcm_streaming_seeked(
                &p,
                Some(taux_sortie),
                Some(2),
                Some(24),
                tx,
                32768,
                ready,
                ltx,
                0.0,
            )
        })
        .await
        .expect("tâche")
        .expect("décodage");
        let (n, emp) = conso.await.expect("consommateur");
        let mur = t.elapsed().as_secs_f64();
        let _ = niveaux.await;
        // 44 octets d'en-tête WAV, puis 3 octets par échantillon.
        ((n.saturating_sub(44)) / 3, mur, emp)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, passages, _tmp) = if args.first().map(String::as_str) == Some("--synth") {
        let secondes: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
        let taux: u32 = args
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_644_800);
        let passages: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join(format!("synth_{taux}_{secondes}s.dsf"));
        ecrire_dsf_synthetique(&p, secondes, taux);
        (p.to_string_lossy().to_string(), passages, Some(dir))
    } else {
        let p = args.first().cloned().expect(
            "usage : banc_dsd_4354 <fichier.dsf> [passages] | --synth <s> <taux_dsd> [passages]",
        );
        let passages: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
        (p, passages, None)
    };

    let info = parse_dsf(&path).expect("parse_dsf");
    let taux_sortie = choose_output_rate(info.sample_rate);
    let audio_s = info.total_samples as f64 / info.sample_rate as f64;
    println!(
        "fichier={path} taux_dsd={} canaux={} audio_s={audio_s:.3} taux_pcm={taux_sortie} coeurs={}",
        info.sample_rate,
        info.channels,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    for i in 1..=passages {
        println!("-- passage {i}/{passages}");
        let (octets, mur) = etage_lecture(&path);
        println!(
            "{:<22} octets_dsd={octets} mur_s={mur:.4} temps_reel={:.1}x",
            "A_lecture",
            audio_s / mur
        );
        let (n, mur, emp) = etage_fir(&path, taux_sortie);
        ligne("B_lecture_fir", audio_s, mur, n, &emp);
        let (n, mur, emp) = etage_chaine(&rt, &path, taux_sortie);
        ligne("C_chaine_reseau_24b", audio_s, mur, n, &emp);
    }
}
