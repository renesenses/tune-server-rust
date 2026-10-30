//! Le fil qui décode la piste et prépare les charges RTP.
//!
//! Même chemin que la sortie AirPlay 1 (#6673) : le décodeur progressif du
//! cœur, [`tune_core::audio::decode::decode_vers_pcm_en_continu`], à cadence,
//! canaux et profondeur IMPOSÉS. Le rééchantillonnage (rubato, noyau choisi
//! selon le couple de cadences) et l'adaptation des canaux se font donc dans
//! le décodeur ; ce fil vérifie l'en-tête WAV qu'il envoie en tête, applique
//! le volume logiciel s'il est allumé, et découpe en paquets.
//!
//! Tout est bloquant : ce fil et celui du décodeur sont des fils système,
//! hors du runtime tokio. La contre-pression vient du canal borné vers le
//! cadenceur : le décodeur ne prend que quelques centaines de millisecondes
//! d'avance.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use symphonia::core::io::MediaSource;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::cadenceur::Message;
use crate::paquetisation::Paquetiseur;

/// Profondeur demandée au décodeur : 32 bits pleine échelle, ramenés à 24 à
/// la paquétisation.
pub const PROFONDEUR_DU_DECODEUR: u16 = 32;
const TAILLE_DES_BLOCS: usize = 32_768;
const CAPACITE_DU_CANAL: usize = 16;
const TAILLE_ENTETE_WAV: usize = 44;

/// D'où vient la piste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceAudio {
    Fichier(String),
    Url(String),
}

impl SourceAudio {
    /// Le chemin local s'il est lisible, sinon l'URL (`file://` compris).
    pub fn choisir(chemin: Option<&str>, url: &str) -> Self {
        if let Some(c) = chemin.filter(|c| std::path::Path::new(c).is_file()) {
            return Self::Fichier(c.to_string());
        }
        if let Some(c) = url.strip_prefix("file://") {
            return Self::Fichier(c.to_string());
        }
        Self::Url(url.to_string())
    }
}

/// Le gain logiciel partagé : un `f32` dans un atomique. 1,0 = pleine échelle.
#[derive(Debug)]
pub struct Gain(AtomicU32);

impl Gain {
    pub fn new(g: f32) -> Self {
        Self(AtomicU32::new(g.to_bits()))
    }
    pub fn poser(&self, g: f32) {
        self.0.store(g.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn lire(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Applique un gain à des échantillons 32 bits. Le gain unité ne touche à
/// rien : volume fixe = échantillons intacts.
pub fn appliquer_le_gain(echantillons: &mut [i32], gain: f32) {
    if gain >= 1.0 {
        return;
    }
    let g = f64::from(gain.max(0.0));
    for e in echantillons {
        *e = (f64::from(*e) * g).round() as i32;
    }
}

/// Le corps HTTP lu en continu, pour un serveur qui ignore `Range`.
struct CorpsEnContinu(Mutex<reqwest::blocking::Response>);

impl Read for CorpsEnContinu {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("corps HTTP empoisonné"))?
            .read(destination)
    }
}

impl Seek for CorpsEnContinu {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "corps HTTP lu en continu",
        ))
    }
}

impl MediaSource for CorpsEnContinu {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// L'extension du chemin d'une URL, indice de format pour le décodeur.
fn extension_de_l_url(url: &str) -> String {
    let chemin = url.split(['?', '#']).next().unwrap_or(url);
    let dernier = chemin.rsplit('/').next().unwrap_or(chemin);
    dernier
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty() && ext.len() <= 5)
        .unwrap_or_default()
}

/// Fait tourner le décodeur sur la source. Bloquant.
pub fn decoder(
    source: &SourceAudio,
    cadence: u32,
    canaux: u16,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<(u16, u32), String> {
    let decoder_sur = |chemin: &str, s: Option<Box<dyn MediaSource>>| {
        tune_core::audio::decode::decode_vers_pcm_en_continu(
            chemin,
            s,
            Some(cadence),
            Some(u32::from(canaux)),
            Some(PROFONDEUR_DU_DECODEUR),
            tx.clone(),
            TAILLE_DES_BLOCS,
        )
    };
    let url = match source {
        SourceAudio::Fichier(chemin) => return decoder_sur(chemin, None),
        SourceAudio::Url(url) => url,
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return decoder_sur(url, None);
    }
    let indice = format!("flux-distant.{}", extension_de_l_url(url));
    match tune_core::audio::http_range::HttpRangeSource::open(url) {
        Ok(s) => decoder_sur(&indice, Some(Box::new(s))),
        Err(raison) => {
            info!(url, raison = %raison, "aes67_flux_http_continu");
            let reponse = tune_core::http::client::blocking_builder()
                .timeout(None)
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| format!("client http : {e}"))?
                .get(url)
                .send()
                .map_err(|e| format!("download {url}: {e}"))?;
            if !reponse.status().is_success() {
                return Err(format!("download {url}: HTTP {}", reponse.status()));
            }
            decoder_sur(&indice, Some(Box::new(CorpsEnContinu(Mutex::new(reponse)))))
        }
    }
}

/// La tâche du décodeur : `(profondeur, cadence)` rendues, ou l'erreur.
pub type TacheDuDecodeur = tokio::task::JoinHandle<Result<(u16, u32), String>>;

/// Lance le décodeur sur un fil BLOQUANT du runtime tokio, comme AirPlay
/// (`spawn_blocking`) : certaines branches du décodeur (DSD, Opus…) passent
/// par `Handle::block_on` et exigent un runtime. Rend le canal du PCM et la
/// tâche, que l'on attend par [`attendre_le_decodeur`] depuis un fil système.
pub fn lancer_le_decodeur(
    runtime: &tokio::runtime::Handle,
    source: SourceAudio,
    cadence: u32,
    canaux: u16,
) -> (mpsc::Receiver<Vec<u8>>, TacheDuDecodeur) {
    let (tx, rx) = mpsc::channel(CAPACITE_DU_CANAL);
    let tache = runtime.spawn_blocking(move || decoder(&source, cadence, canaux, tx));
    (rx, tache)
}

/// Attend la fin du décodeur depuis un fil SYSTÈME (jamais depuis une tâche
/// asynchrone : `block_on` y paniquerait).
pub fn attendre_le_decodeur(
    runtime: &tokio::runtime::Handle,
    tache: TacheDuDecodeur,
) -> Result<(u16, u32), String> {
    runtime
        .block_on(tache)
        .map_err(|e| format!("le décodeur a paniqué : {e}"))?
}

/// L'en-tête WAV du décodeur doit annoncer exactement le format du SDP.
pub fn verifier_l_entete(entete: &[u8], cadence: u32, canaux: u16) -> Result<(), String> {
    if entete.len() < TAILLE_ENTETE_WAV || &entete[0..4] != b"RIFF" || &entete[8..12] != b"WAVE" {
        return Err("en-tête WAV absent en tête du flux décodé".into());
    }
    let c = u16::from_le_bytes([entete[22], entete[23]]);
    let r = u32::from_le_bytes([entete[24], entete[25], entete[26], entete[27]]);
    let b = u16::from_le_bytes([entete[34], entete[35]]);
    if c != canaux || r != cadence || b != PROFONDEUR_DU_DECODEUR {
        return Err(format!(
            "flux décodé en {c} canaux, {r} Hz, {b} bits : attendu {canaux} canaux, \
             {cadence} Hz, {PROFONDEUR_DU_DECODEUR} bits"
        ));
    }
    Ok(())
}

/// Lit le PCM 32 bits d'un décodeur en retirant et vérifiant l'en-tête.
pub struct LecteurPcm {
    rx: mpsc::Receiver<Vec<u8>>,
    entete: Vec<u8>,
    reste: Vec<u8>,
    cadence: u32,
    canaux: u16,
}

impl LecteurPcm {
    pub fn new(rx: mpsc::Receiver<Vec<u8>>, cadence: u32, canaux: u16) -> Self {
        Self {
            rx,
            entete: Vec::with_capacity(TAILLE_ENTETE_WAV),
            reste: Vec::new(),
            cadence,
            canaux,
        }
    }

    /// Le bloc suivant d'échantillons entrelacés ; `Ok(None)` en fin de flux.
    pub fn suivant(&mut self) -> Result<Option<Vec<i32>>, String> {
        loop {
            let Some(bloc) = self.rx.blocking_recv() else {
                if self.entete.len() < TAILLE_ENTETE_WAV {
                    return Err("flux décodé sans en-tête WAV".into());
                }
                return Ok(None);
            };
            let mut bloc = &bloc[..];
            if self.entete.len() < TAILLE_ENTETE_WAV {
                let n = (TAILLE_ENTETE_WAV - self.entete.len()).min(bloc.len());
                self.entete.extend_from_slice(&bloc[..n]);
                bloc = &bloc[n..];
                if self.entete.len() == TAILLE_ENTETE_WAV {
                    verifier_l_entete(&self.entete, self.cadence, self.canaux)?;
                }
            }
            self.reste.extend_from_slice(bloc);
            let complets = self.reste.len() / 4 * 4;
            if complets == 0 {
                continue;
            }
            let e: Vec<i32> = self.reste[..complets]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|o| i32::from_le_bytes([o[0], o[1], o[2], o[3]]))
                .collect();
            self.reste.drain(..complets);
            return Ok(Some(e));
        }
    }

    pub fn fermer(&mut self) {
        self.rx.close();
    }
}

/// Lance le décodeur (son propre fil) et le producteur qui remplit `tx`.
/// L'erreur éventuelle (décodeur, en-tête) est déposée dans `echec`.
#[allow(clippy::too_many_arguments)]
pub fn lancer(
    runtime: tokio::runtime::Handle,
    source: SourceAudio,
    cadence: u32,
    canaux: u16,
    trames_par_paquet: usize,
    gain: Arc<Gain>,
    tx: SyncSender<Message>,
    arret: Arc<AtomicBool>,
    echec: Arc<Mutex<Option<String>>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("aes67-pcm".into())
        .spawn(move || {
            let (rx_dec, decodeur) = lancer_le_decodeur(&runtime, source.clone(), cadence, canaux);
            let mut lecteur = LecteurPcm::new(rx_dec, cadence, canaux);
            let mut paquetiseur = Paquetiseur::new(usize::from(canaux), trames_par_paquet);
            let mut erreur = None;
            let mut interrompu = false;
            'lecture: loop {
                if arret.load(Ordering::Acquire) {
                    interrompu = true;
                    break;
                }
                match lecteur.suivant() {
                    Ok(Some(mut bloc)) => {
                        appliquer_le_gain(&mut bloc, gain.lire());
                        for charge in paquetiseur.pousser(&bloc) {
                            if tx.send(Message::Charge(charge)).is_err() {
                                interrompu = true;
                                break 'lecture;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        erreur = Some(e);
                        break;
                    }
                }
            }
            lecteur.fermer();
            if !interrompu
                && erreur.is_none()
                && let Some(dernier) = paquetiseur.vider()
            {
                let _ = tx.send(Message::Charge(dernier));
            }
            let _ = tx.send(Message::Fin);
            // L'erreur du décodeur prime : « flux sans en-tête » n'en est
            // que la conséquence.
            match attendre_le_decodeur(&runtime, decodeur) {
                Ok(_) => {}
                Err(e) if !interrompu => erreur = Some(e),
                Err(_) => {}
            }
            if let Some(e) = erreur {
                warn!(error = %e, ?source, "aes67_decodage_echoue");
                *echec.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_source_prefere_le_fichier_local_lisible() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let chemin = f.path().to_str().unwrap();
        assert_eq!(
            SourceAudio::choisir(Some(chemin), "http://h/stream/1.flac"),
            SourceAudio::Fichier(chemin.into())
        );
        assert_eq!(
            SourceAudio::choisir(Some("/n/existe/pas.flac"), "http://h/stream/1.flac"),
            SourceAudio::Url("http://h/stream/1.flac".into())
        );
        assert_eq!(
            SourceAudio::choisir(None, "file:///musique/a.flac"),
            SourceAudio::Fichier("/musique/a.flac".into())
        );
    }

    #[test]
    fn le_gain_unite_laisse_les_echantillons_intacts() {
        let mut e = vec![i32::MAX, i32::MIN, 12345];
        appliquer_le_gain(&mut e, 1.0);
        assert_eq!(e, vec![i32::MAX, i32::MIN, 12345]);
        appliquer_le_gain(&mut e, 0.5);
        assert_eq!(e[2], 6173);
        assert_eq!(e[0], 1_073_741_824);
        appliquer_le_gain(&mut e, 0.0);
        assert!(e.iter().all(|&v| v == 0));
        let g = Gain::new(1.0);
        g.poser(3.0);
        assert_eq!(g.lire(), 1.0, "jamais d'amplification");
    }

    #[test]
    fn un_entete_qui_ne_correspond_pas_au_sdp_est_refuse() {
        let bon = tune_core::audio::wav::build_wav_header_streaming(2, 48_000, 32);
        assert!(verifier_l_entete(&bon, 48_000, 2).is_ok());
        let cadence = tune_core::audio::wav::build_wav_header_streaming(2, 44_100, 32);
        assert!(
            verifier_l_entete(&cadence, 48_000, 2)
                .unwrap_err()
                .contains("44100 Hz")
        );
        let canaux = tune_core::audio::wav::build_wav_header_streaming(1, 48_000, 32);
        assert!(verifier_l_entete(&canaux, 48_000, 2).is_err());
        assert!(verifier_l_entete(b"RIFF", 48_000, 2).is_err());
    }

    #[test]
    fn l_extension_de_l_url_sert_d_indice() {
        assert_eq!(extension_de_l_url("http://h/stream/ab.wav?x=1"), "wav");
        assert_eq!(extension_de_l_url("http://h/stream/ab"), "");
    }
}
