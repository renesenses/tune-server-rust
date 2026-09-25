//! **Le chemin HTTP progressif d'un DSF produit le MÊME PCM que le chemin
//! fichier** — mesuré, sans oreille (#4833, seconde passe).
//!
//! Sur le .18 (23/09/2026, zone DMP-A8 en `pcm`), le DSF distant décodé au
//! fil de l'eau donnait un son « bruité et très ralenti » là où le relais
//! brut du même fichier était propre. Les deux chemins partagent le lecteur
//! de blocs et le convertisseur ; ce module compare ce qu'ils PRODUISENT :
//! en-tête WAV rendu, nombre d'échantillons par canal, cadence, durée, RMS par
//! canal, corrélation des deux flux — et mesure la vitesse de décodage face au
//! temps réel, qui est la contrainte d'un flux servi au fil de l'eau.
//!
//! Trois sources : un DSF SYNTHÉTIQUE écrit ici (sinus 1 kHz modulé sigma-delta
//! au second ordre, dont fréquence et niveau se vérifient après décodage), le
//! DSF de référence du dépôt (écrit par un autre écrivain, toujours joué), et,
//! quand `TUNE_DSF_REEL` nomme un fichier, un vrai DSF du terrain en plus.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CADENCE_DSD: u32 = 2_822_400;
const CADENCE_PCM: u32 = 176_400;
const BLOC: usize = 4096;
const CANAUX: usize = 2;

/// Un sinus modulé en DSD64 au second ordre (sigma-delta), bits LSB en
/// premier par octet, blocs de 4 096 octets par canal entrelacés par
/// super-bloc — le format `data` d'un DSF. Le canal droit porte le même sinus
/// à moitié du niveau, pour que les canaux soient discernables.
fn dsf_sinus(frequence_hz: f64, amplitude: f64, duree_s: f64) -> Vec<u8> {
    let echantillons = (CADENCE_DSD as f64 * duree_s) as usize;
    let octets_par_canal = echantillons.div_ceil(8);
    let blocs = octets_par_canal.div_ceil(BLOC);
    let mut canaux: Vec<Vec<u8>> = vec![vec![0u8; blocs * BLOC]; CANAUX];
    for (c, canal) in canaux.iter_mut().enumerate() {
        let gain = if c == 0 { amplitude } else { amplitude / 2.0 };
        let (mut i1, mut i2, mut y) = (0.0f64, 0.0f64, 1.0f64);
        for n in 0..echantillons {
            let x = gain
                * (2.0 * std::f64::consts::PI * frequence_hz * n as f64 / CADENCE_DSD as f64).sin();
            i1 += x - y;
            i2 += i1 - y;
            y = if i2 >= 0.0 { 1.0 } else { -1.0 };
            if y > 0.0 {
                canal[n / 8] |= 1 << (n % 8);
            }
        }
    }
    let mut data = Vec::with_capacity(blocs * BLOC * CANAUX);
    for b in 0..blocs {
        for canal in &canaux {
            data.extend_from_slice(&canal[b * BLOC..(b + 1) * BLOC]);
        }
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&(CANAUX as u32).to_le_bytes());
    buf.extend_from_slice(&CADENCE_DSD.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&(echantillons as u64).to_le_bytes());
    buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    buf
}

/// Ce qu'un chemin de décodage a produit, et en combien de temps.
struct Produit {
    entete: Vec<u8>,
    pcm: Vec<u8>,
    elapsed_s: f64,
}

impl Produit {
    fn cadence(&self) -> u32 {
        u32::from_le_bytes(self.entete[24..28].try_into().unwrap())
    }
    fn canaux(&self) -> u16 {
        u16::from_le_bytes(self.entete[22..24].try_into().unwrap())
    }
    fn bits(&self) -> u16 {
        u16::from_le_bytes(self.entete[34..36].try_into().unwrap())
    }
    fn trames(&self) -> usize {
        self.pcm.len() / (3 * self.canaux() as usize)
    }
    fn duree_s(&self) -> f64 {
        self.trames() as f64 / self.cadence() as f64
    }
    /// Échantillons d'un canal, en unités de pleine échelle.
    fn canal(&self, c: usize) -> Vec<f64> {
        let n = self.canaux() as usize;
        self.pcm
            .chunks_exact(3 * n)
            .map(|t| {
                let o = &t[c * 3..c * 3 + 3];
                let v = (o[0] as i32) | ((o[1] as i32) << 8) | ((o[2] as i32) << 16);
                ((v << 8) >> 8) as f64 / 8_388_608.0
            })
            .collect()
    }
}

fn rms(x: &[f64]) -> f64 {
    (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let (ma, mb) = (
        a.iter().sum::<f64>() / n as f64,
        b.iter().sum::<f64>() / n as f64,
    );
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (da, db) = (a[i] - ma, b[i] - mb);
        sab += da * db;
        saa += da * da;
        sbb += db * db;
    }
    sab / (saa * sbb).sqrt().max(f64::MIN_POSITIVE)
}

/// Fréquence estimée par les passages à zéro (montants) par seconde.
fn frequence_hz(x: &[f64], cadence: u32) -> f64 {
    let montants = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    montants as f64 * cadence as f64 / x.len() as f64
}

async fn collecter(mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> (Vec<u8>, Vec<u8>) {
    let mut tout = Vec::new();
    while let Some(bloc) = rx.recv().await {
        tout.extend_from_slice(&bloc);
    }
    assert!(
        tout.len() >= 44 && &tout[..4] == b"RIFF",
        "en-tête WAV attendu"
    );
    let pcm = tout.split_off(44);
    (tout, pcm)
}

/// Le chemin FICHIER — celui des pistes locales et des services téléchargés.
async fn decoder_par_fichier(chemin: &str) -> Produit {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let (levels_tx, _levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let chemin = chemin.to_string();
    let depart = std::time::Instant::now();
    let decodeur = tokio::task::spawn_blocking(move || {
        super::decode::decode_to_pcm_streaming_seeked(
            &chemin,
            Some(CADENCE_PCM),
            Some(2),
            Some(24),
            tx,
            32768,
            data_ready,
            levels_tx,
            0.0,
        )
    });
    let (entete, pcm) = collecter(rx).await;
    decodeur.await.unwrap().expect("décodage fichier");
    Produit {
        entete,
        pcm,
        elapsed_s: depart.elapsed().as_secs_f64(),
    }
}

/// Un serveur média qui sert le fichier par morceaux de 64 Kio.
async fn servir(corps: Arc<Vec<u8>>) -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/tracks/1/audio", listener.local_addr().unwrap());
    let requetes = Arc::new(AtomicUsize::new(0));
    let compteur = requetes.clone();
    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            compteur.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let entete = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-dsd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                for morceau in corps.chunks(65_536) {
                    if socket.write_all(morceau).await.is_err() {
                        return;
                    }
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, tache, requetes)
}

/// Le chemin HTTP PROGRESSIF — celui d'un DSF de serveur média vers un
/// renderer en `pcm`.
async fn decoder_par_http(corps: Arc<Vec<u8>>) -> Produit {
    let (url, _serveur, _requetes) = servir(corps).await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let (levels_tx, _levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let depart = std::time::Instant::now();
    let decodeur = tokio::task::spawn_blocking(move || {
        super::decode::decode_dsf_http_to_pcm_streaming(
            &url,
            Some(CADENCE_PCM),
            Some(2),
            24,
            tx,
            32768,
            data_ready,
            levels_tx,
        )
    });
    let (entete, pcm) = collecter(rx).await;
    decodeur.await.unwrap().expect("décodage http");
    Produit {
        entete,
        pcm,
        elapsed_s: depart.elapsed().as_secs_f64(),
    }
}

/// Les deux chemins doivent produire la même chose, aux tolérances de la
/// demande : mêmes comptes (± un bloc PCM), même cadence, durée et RMS à 1 %,
/// corrélation > 0,99 sur les cinq premières secondes.
fn comparer(nom: &str, fichier: &Produit, http: &Produit, duree_dsd_s: f64) {
    let bloc_pcm = 32768 / 6;
    eprintln!(
        "[{nom}] fichier : {} trames, {} Hz, {} canaux, {} bits, {:.3} s, décodé en {:.2} s ({:.2}× temps réel)",
        fichier.trames(),
        fichier.cadence(),
        fichier.canaux(),
        fichier.bits(),
        fichier.duree_s(),
        fichier.elapsed_s,
        fichier.duree_s() / fichier.elapsed_s
    );
    eprintln!(
        "[{nom}] http    : {} trames, {} Hz, {} canaux, {} bits, {:.3} s, décodé en {:.2} s ({:.2}× temps réel)",
        http.trames(),
        http.cadence(),
        http.canaux(),
        http.bits(),
        http.duree_s(),
        http.elapsed_s,
        http.duree_s() / http.elapsed_s
    );
    assert_eq!(
        (http.cadence(), http.canaux(), http.bits()),
        (CADENCE_PCM, 2, 24),
        "en-tête WAV http"
    );
    assert_eq!(
        (fichier.cadence(), fichier.canaux(), fichier.bits()),
        (CADENCE_PCM, 2, 24),
        "en-tête WAV fichier"
    );
    assert!(
        fichier.trames().abs_diff(http.trames()) <= bloc_pcm,
        "[{nom}] comptes d'échantillons : fichier {} vs http {}",
        fichier.trames(),
        http.trames()
    );
    assert!(
        (fichier.duree_s() - duree_dsd_s).abs() / duree_dsd_s < 0.01,
        "[{nom}] durée fichier {:.3} s vs DSD {duree_dsd_s:.3} s",
        fichier.duree_s()
    );
    assert!(
        (http.duree_s() - duree_dsd_s).abs() / duree_dsd_s < 0.01,
        "[{nom}] durée http {:.3} s vs DSD {duree_dsd_s:.3} s",
        http.duree_s()
    );
    let cinq_s = (5.0 * CADENCE_PCM as f64) as usize;
    for c in 0..2 {
        let (a, b) = (fichier.canal(c), http.canal(c));
        let (ra, rb) = (rms(&a), rms(&b));
        eprintln!("[{nom}] canal {c} : RMS fichier {ra:.5}, http {rb:.5}");
        assert!(ra > 0.0, "[{nom}] canal {c} muet côté fichier");
        assert!(
            (ra - rb).abs() / ra < 0.01,
            "[{nom}] RMS canal {c} : fichier {ra:.5} vs http {rb:.5}"
        );
        let n = a.len().min(b.len()).min(cinq_s);
        let corr = correlation(&a[..n], &b[..n]);
        eprintln!("[{nom}] canal {c} : corrélation sur {n} trames = {corr:.5}");
        assert!(corr > 0.99, "[{nom}] corrélation canal {c} = {corr:.4}");
    }
}

/// Le DSF synthétique : sinus 1 kHz à 0,4 de pleine échelle (canal droit à
/// 0,2), deux secondes. Fréquence et niveau se vérifient après décodage — le
/// décimateur applique `DSD_SACD_GAIN` (+6 dB), d'où 0,8 et 0,4 crête.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_chemin_http_produit_le_meme_pcm_que_le_fichier_sur_un_sinus() {
    let corps = dsf_sinus(1000.0, 0.4, 2.0);
    let fichier = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    std::fs::write(fichier.path(), &corps).unwrap();
    let corps = Arc::new(corps);

    let par_fichier = decoder_par_fichier(fichier.path().to_str().unwrap()).await;
    let par_http = decoder_par_http(corps).await;
    comparer("sinus", &par_fichier, &par_http, 2.0);

    // Le signal lui-même : 1 kHz, et un niveau cohérent avec le gain SACD.
    let gauche = par_http.canal(0);
    let stable = &gauche[CADENCE_PCM as usize / 10..]; // après l'amorce du filtre
    let f = frequence_hz(stable, CADENCE_PCM);
    let niveau = rms(stable) * std::f64::consts::SQRT_2;
    eprintln!("[sinus] fréquence {f:.1} Hz, crête estimée {niveau:.3}");
    assert!((f - 1000.0).abs() < 20.0, "fréquence {f:.1} Hz");
    assert!(
        (0.7..0.9).contains(&niveau),
        "crête {niveau:.3} (attendu 0,4 × 2 = 0,8)"
    );
    let droite = par_http.canal(1);
    let ratio = rms(&droite[CADENCE_PCM as usize / 10..]) / rms(stable);
    assert!(
        (ratio - 0.5).abs() < 0.05,
        "le canal droit est à la moitié : {ratio:.3}"
    );
}

/// Le DSF de référence du dépôt, écrit par un AUTRE écrivain que `dsf_sinus`
/// (`tests/fixtures/dsd/generer_fixtures_dsd.py`, d'après la spécification
/// Sony v1.01) : le conteneur ne vient pas du même code que le témoin.
const DSF_DE_REFERENCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dsd/ref_dsd64_stereo.dsf"
);

async fn comparer_sur_un_dsf(nom: &str, chemin: &str) {
    let corps = std::fs::read(chemin).expect("lire le DSF");
    let info = super::dsf::parse_dsf_from_bytes(&corps).expect("en-tête DSF");
    let duree = info.total_samples as f64 / info.sample_rate as f64;
    eprintln!(
        "[{nom}] {chemin} : {} Hz, {} canaux, {} échantillons/canal, {duree:.3} s",
        info.sample_rate, info.channels, info.total_samples
    );
    let par_fichier = decoder_par_fichier(chemin).await;
    let par_http = decoder_par_http(Arc::new(corps)).await;
    comparer(nom, &par_fichier, &par_http, duree);
}

/// Un DSF écrit hors de ce module — TOUJOURS joué, en CI comme ailleurs.
///
/// `TUNE_DSF_REEL` peut nommer EN PLUS un vrai DSF du terrain (le fichier de
/// 15 Mio du .18 ne peut pas entrer dans le dépôt) : il est alors comparé à
/// la suite. Sans elle, le témoin ne saute pas — il joue la référence du
/// dépôt, et c'est elle que la CI garde.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_chemin_http_produit_le_meme_pcm_que_le_fichier_sur_un_dsf_reel() {
    comparer_sur_un_dsf("référence", DSF_DE_REFERENCE).await;
    if let Ok(chemin) = std::env::var("TUNE_DSF_REEL") {
        comparer_sur_un_dsf("réel", &chemin).await;
    }
}
