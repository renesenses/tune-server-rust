//! #6990 — TIDAL AAC 320 → WAV : le canal `produire_le_canal_aac` sert-il un
//! signal intact ? Mesures sur un AAC-LC 320 kbit/s généré (afconvert) d'un
//! signal connu : 3 s de sinus (997 Hz à gauche, 440 Hz à droite, −6 dBFS),
//! puis 3 s d'un sinus écrêté à 0,98 (L) et son opposé (R).
use super::*;

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");

struct Wav {
    sr: u32,
    ch: u16,
    bits: u16,
    data: Vec<u8>,
}

fn parse_wav(b: &[u8]) -> Wav {
    assert_eq!(&b[0..4], b"RIFF");
    let mut p = 12;
    let (mut sr, mut ch, mut bits) = (0, 0, 0);
    loop {
        let id = &b[p..p + 4];
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            ch = u16::from_le_bytes(b[p + 10..p + 12].try_into().unwrap());
            sr = u32::from_le_bytes(b[p + 12..p + 16].try_into().unwrap());
            bits = u16::from_le_bytes(b[p + 22..p + 24].try_into().unwrap());
        }
        if id == b"data" {
            return Wav {
                sr,
                ch,
                bits,
                data: b[p + 8..].to_vec(),
            };
        }
        p += 8 + len + (len & 1);
    }
}

fn samples(w: &Wav) -> Vec<f64> {
    let bps = (w.bits / 8) as usize;
    w.data
        .chunks_exact(bps)
        .map(|s| match bps {
            2 => i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0,
            3 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f64 / 8_388_608.0,
            _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64 / 2_147_483_648.0,
        })
        .collect()
}

fn servir(fichier: &str, sr: u32, bd: u16) -> Vec<u8> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4096);
        let (ltx, _lrx) = tokio::sync::mpsc::unbounded_channel();
        let f = fichier.to_string();
        let h = tokio::task::spawn_blocking(move || {
            decode_to_pcm_streaming_seeked(
                &f,
                Some(sr),
                Some(2),
                Some(bd),
                tx,
                32768,
                std::sync::Arc::new(tokio::sync::Notify::new()),
                ltx,
                0.0,
            )
        });
        let mut out = Vec::new();
        while let Some(c) = rx.recv().await {
            out.extend_from_slice(&c);
        }
        h.await.unwrap().unwrap();
        out
    })
}

/// (saut max entre échantillons voisins, fréquence estimée par passages à zéro)
fn mesure(s: &[f64], ch: usize, voie: usize, sr: u32, de: f64, a: f64) -> (f64, f64) {
    let v: Vec<f64> = s.iter().skip(voie).step_by(ch).copied().collect();
    let (i0, i1) = (
        (de * sr as f64) as usize,
        ((a * sr as f64) as usize).min(v.len()),
    );
    let w = &v[i0..i1];
    let saut = w
        .windows(2)
        .map(|p| (p[1] - p[0]).abs())
        .fold(0.0, f64::max);
    let zc = w.windows(2).filter(|p| p[0] < 0.0 && p[1] >= 0.0).count();
    (saut, zc as f64 / ((i1 - i0) as f64 / sr as f64))
}

fn rapport(nom: &str, fichier: &str, sr: u32, bd: u16) -> (Wav, Vec<f64>) {
    let w = parse_wav(&servir(&format!("{FIX}{fichier}"), sr, bd));
    let s = samples(&w);
    let ch = w.ch as usize;
    let duree = s.len() as f64 / ch as f64 / w.sr as f64;
    let (sl, fl) = mesure(&s, ch, 0, w.sr, 0.3, 2.7);
    let (sr_, fr) = mesure(&s, ch, 1, w.sr, 0.3, 2.7);
    let (ll, _) = mesure(&s, ch, 0, w.sr, 3.3, 5.7);
    let crete = s.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    eprintln!(
        "6990 {nom}: entete {} Hz {} bits {} voies, octets data {} (reste trame {}), duree {duree:.3} s, \
         sinus G {fl:.1} Hz saut {sl:.4}, D {fr:.1} Hz saut {sr_:.4}, fort saut {ll:.4}, crete {crete:.4}",
        w.sr,
        w.bits,
        w.ch,
        w.data.len(),
        w.data.len() % (ch * w.bits as usize / 8)
    );
    (w, s)
}

/// Rapport signal/erreur (dB) contre la source, par fenêtre de 50 ms, après
/// recalage du délai de l'encodeur (priming). Rend (délai, pire RSB, instant).
fn rsb_contre_source(s: &[f64], sr: u32) -> (usize, f64, f64) {
    let src = parse_wav(&std::fs::read(format!("{FIX}src.wav")).unwrap());
    assert_eq!(src.sr, sr);
    let r = samples(&src);
    let n = (r.len() / 2).min(s.len() / 2);
    let mut meilleur = (0usize, f64::MAX);
    for lag in 0..4096usize {
        let mut e = 0.0;
        for i in 22050..26460 {
            if i + lag >= s.len() / 2 {
                break;
            }
            e += (s[2 * (i + lag)] - r[2 * i]).powi(2);
        }
        if e < meilleur.1 {
            meilleur = (lag, e);
        }
    }
    let lag = meilleur.0;
    let fen = (sr / 20) as usize;
    let mut pire = (f64::MAX, 0.0);
    let mut i = 0;
    while i + fen + lag < n {
        let (mut ps, mut pe) = (0.0, 0.0);
        for k in i..i + fen {
            for c in 0..2 {
                let x = r[2 * k + c];
                ps += x * x;
                pe += (s[2 * (k + lag) + c] - x).powi(2);
            }
        }
        let db = 10.0 * (ps / pe.max(1e-30)).log10();
        if db < pire.0 {
            pire = (db, i as f64 / sr as f64);
        }
        i += fen;
    }
    (lag, pire.0, pire.1)
}

#[test]
fn canal_aac_6990_rsb_contre_la_source() {
    for bd in [16u16, 24] {
        let (_w, s) = rapport("rsb", "aac320.m4a", 44100, bd);
        let (lag, pire, t) = rsb_contre_source(&s, 44100);
        eprintln!("6990 rsb {bd} bits: delai {lag} trames, pire fenetre {pire:.1} dB a {t:.2} s");
        // Le délai de l'encodeur (2 112 trames) n'est pas retiré : 48 ms de
        // silence en tête, pas une déformation. Au-delà, le signal servi doit
        // suivre la source à la précision d'un AAC 320 kbit/s.
        assert!(
            pire > 40.0,
            "{bd} bits: fenetre a {pire:.1} dB de la source a {t:.2} s"
        );
    }
}

#[test]
fn canal_aac_6990_signal_intact_aac_lc_320() {
    for (nom, sr, bd) in [
        ("lc 44100/16", 44100, 16),
        ("lc 44100/24", 44100, 24),
        ("lc 48000/24", 48000, 24),
    ] {
        let (w, s) = rapport(nom, "aac320.m4a", sr, bd);
        let ch = w.ch as usize;
        assert_eq!((w.sr, w.ch, w.bits), (sr, 2, bd), "{nom}: en-tete WAV");
        assert_eq!(
            w.data.len() % (ch * bd as usize / 8),
            0,
            "{nom}: trame coupee"
        );
        let (sl, fl) = mesure(&s, ch, 0, w.sr, 0.3, 2.7);
        let (sd, fd) = mesure(&s, ch, 1, w.sr, 0.3, 2.7);
        assert!(
            (fl - 997.0).abs() < 5.0 && (fd - 440.0).abs() < 5.0,
            "{nom}: frequences {fl} / {fd}"
        );
        // pente max d'un sinus 0,5 a 997 Hz : 0,5*2pi*997/sr ~ 0,071
        assert!(
            sl < 0.09 && sd < 0.09,
            "{nom}: clic dans le sinus (saut {sl} / {sd})"
        );
        let (ll, _) = mesure(&s, ch, 0, w.sr, 3.3, 5.7);
        assert!(
            ll < 0.3,
            "{nom}: discontinuite dans la partie forte (saut {ll}) : repliement d'ecretage ?"
        );
    }
}

/// Le chemin réel : le fichier téléchargé s'appelle `tune-stream-<uuid>.aac`
/// (`codec_lower`), et Tidal sert du MP4 FRAGMENTÉ (moof/mdat, façon DASH).
#[test]
fn canal_aac_6990_mp4_fragmente_nomme_aac() {
    // tmp-autorise: dossier de test propre au processus, supprimé en fin de test.
    let dir = std::env::temp_dir().join(format!("tune-6990-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (src, nom) in [
        ("aac320.m4a", "plat.aac"),
        ("aac320_frag.mp4", "frag.aac"),
        ("aac320_frag.mp4", "frag.mp4"),
    ] {
        let dst = dir.join(nom);
        std::fs::copy(format!("{FIX}{src}"), &dst).unwrap();
        let w = parse_wav(&servir(dst.to_str().unwrap(), 44100, 16));
        let s = samples(&w);
        let duree = s.len() as f64 / 2.0 / w.sr as f64;
        let (sl, fl) = mesure(&s, 2, 0, w.sr, 0.3, 2.7);
        let (ll, _) = mesure(&s, 2, 0, w.sr, 3.3, 5.7);
        let (lag, pire, t) = rsb_contre_source(&s, 44100);
        eprintln!(
            "6990 {src} -> {nom}: {} Hz {} bits, duree {duree:.3} s, G {fl:.1} Hz saut {sl:.4}, fort saut {ll:.4}, delai {lag}, pire {pire:.1} dB a {t:.2} s",
            w.sr, w.bits
        );
        assert_eq!((w.sr, w.bits), (44100, 16), "{src} -> {nom}: en-tete WAV");
        assert!((duree - 6.06).abs() < 0.05, "{src} -> {nom}: duree {duree}");
        assert!(
            (fl - 997.0).abs() < 5.0 && sl < 0.09 && ll < 0.3,
            "{src} -> {nom}: {fl} Hz, sauts {sl} / {ll}"
        );
        assert!(
            pire > 40.0,
            "{src} -> {nom}: fenetre a {pire:.1} dB de la source a {t:.2} s"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
