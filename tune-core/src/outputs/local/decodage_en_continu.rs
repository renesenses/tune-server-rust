//! #5439 — un flux compressé (FLAC, MP3) servi tel quel par un serveur
//! multimédia se DÉCODE AU FIL DE L'EAU, comme un fichier local.
//!
//! Belkadi Yacine, fil 1909, Tune 0.9.167 : un FLAC de la Freebox ne sonnait
//! qu'au bout de 17 s. La branche compressée de `play_url` lisait tout le
//! corps HTTP, décodait tout, rééchantillonnait tout, puis seulement
//! pré-remplissait l'anneau.
//!
//! Un fichier local, lui, n'attend rien : l'orchestrateur le décode paquet par
//! paquet en WAV (`audio::decode::decode_to_pcm_streaming_*`), et la sortie le
//! lit par son chemin PCM — décision de cadence, rééchantillonneur en flux,
//! DSP, seek par saut d'octets, enchaînement et arrêt. Ce module branche le
//! MÊME décodeur sur le corps HTTP que `play_url` vient d'ouvrir : la suite du
//! flux arrive à la sortie en WAV, et le chemin PCM le joue sans rien savoir
//! de son origine. Aucun second mécanisme d'alimentation.
//!
//! Le décodeur tourne dans un fil à lui, borné par un canal de
//! [`CAPACITE_DU_CANAL`] blocs ; il lit le corps HTTP sous un témoin que le
//! lecteur lève à l'arrêt ET à l'abandon, et le lecteur le rejoint en se
//! détruisant : aucun fil n'est détaché (#4220).
//!
//! Formats pris ici : ceux que la boucle symphonia décode sur une source qui
//! ne se rembobine pas — FLAC et MP3. Les autres (M4A dont l'atome `moov` est
//! en fin de fichier, Ogg-Opus décodé par libopus, AAC ADTS…) gardent la
//! branche compressée d'avant, qui charge tout.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use symphonia::core::io::MediaSource;
use tracing::warn;

use super::lecture_http::{CorpsHttp, PAS_ANNULATION, interrompue};

/// Blocs en attente entre le décodeur et la sortie. 64 × 32 Kio ≈ 2 Mio,
/// environ 5 s de 48 kHz stéréo 32 bits : le décodeur prend de l'avance sans
/// jamais charger la piste.
pub(super) const CAPACITE_DU_CANAL: usize = 64;
/// La taille des blocs du transcodage des fichiers locaux.
const TAILLE_DES_BLOCS: usize = 32_768;
/// Le PCM que le chemin PCM de la sortie locale reçoit des fichiers locaux.
const PROFONDEUR_DE_SORTIE: u16 = 32;

/// Le format que le premier bloc du corps annonce, s'il se décode en continu.
///
/// `None` : la branche compressée d'avant garde ce flux.
pub(super) fn extension_decodable_en_continu(debut: &[u8]) -> Option<&'static str> {
    if debut.starts_with(b"fLaC") {
        return Some("flac");
    }
    if debut.starts_with(b"ID3") {
        return Some("mp3");
    }
    // Synchronisation d'une trame MPEG audio couche III : 11 bits à 1, puis
    // la couche `01`. L'ADTS (AAC) porte la couche `00` et n'est pas pris.
    if debut.len() >= 2 && debut[0] == 0xFF && (debut[1] & 0xE0) == 0xE0 {
        let couche = (debut[1] >> 1) & 0b11;
        if couche == 0b01 {
            return Some("mp3");
        }
    }
    None
}

/// Le corps HTTP, précédé de ce que `play_url` en a déjà lu, vu par symphonia.
struct SourceEnFlux {
    prefixe: Vec<u8>,
    position: usize,
    /// `Mutex` pour `Sync`, qu'exige `MediaSource` ; un seul lecteur, jamais
    /// disputé.
    http: Mutex<CorpsHttp>,
    abandon: Arc<AtomicBool>,
}

impl Read for SourceEnFlux {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if self.position < self.prefixe.len() {
            let n = destination.len().min(self.prefixe.len() - self.position);
            destination[..n].copy_from_slice(&self.prefixe[self.position..self.position + n]);
            self.position += n;
            return Ok(n);
        }
        let mut http = self
            .http
            .lock()
            .map_err(|_| io::Error::other("corps HTTP empoisonné"))?;
        match http.read(destination) {
            // Abandon (arrêt, ou lecteur détruit) : une fin de flux pour le
            // décodeur, qui sort sans journaliser une erreur de paquet.
            Err(_) if self.abandon.load(Ordering::SeqCst) => Ok(0),
            autre => autre,
        }
    }
}

impl Seek for SourceEnFlux {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "flux HTTP lu en continu",
        ))
    }
}

impl MediaSource for SourceEnFlux {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Ce que le fil de décodage a rendu : `(profondeur, cadence)` ou son erreur.
type IssueDuDecodeur = Arc<Mutex<Option<Result<(u16, u32), String>>>>;

/// La suite d'un flux compressé, en WAV, telle que le décodeur la produit.
pub(super) struct FluxDecode {
    rx: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    tampon: Vec<u8>,
    position: usize,
    /// Le témoin d'arrêt de la lecture (`force_silent`).
    arret: Arc<AtomicBool>,
    /// Le témoin sous lequel le fil de décodage lit le corps HTTP.
    abandon: Arc<AtomicBool>,
    issue: IssueDuDecodeur,
    fil: Option<JoinHandle<()>>,
    moteur: Option<tokio::runtime::Runtime>,
}

impl FluxDecode {
    /// Lance le décodeur sur `http` ; `prefixe` est ce qui en a déjà été lu.
    pub(super) fn lancer(mut http: CorpsHttp, prefixe: Vec<u8>, extension: &'static str) -> Self {
        let arret = http.arret_courant();
        let abandon = Arc::new(AtomicBool::new(false));
        http.observer(abandon.clone());
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CAPACITE_DU_CANAL);
        let issue = Arc::new(Mutex::new(None));
        let source = SourceEnFlux {
            prefixe,
            position: 0,
            http: Mutex::new(http),
            abandon: abandon.clone(),
        };
        let issue_du_fil = issue.clone();
        let fil = std::thread::Builder::new()
            .name("tune-decodage-en-continu".into())
            .spawn(move || {
                let resultat = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(moteur) => {
                        // Le décodeur des fichiers locaux attend un contexte
                        // tokio pour ses envois sur le canal.
                        let _contexte = moteur.enter();
                        crate::audio::decode::decode_source_to_pcm_streaming(
                            Box::new(source),
                            extension,
                            Some(PROFONDEUR_DE_SORTIE),
                            tx,
                            TAILLE_DES_BLOCS,
                        )
                    }
                    Err(e) => Err(format!("moteur du décodage en continu : {e}")),
                };
                if let Ok(mut slot) = issue_du_fil.lock() {
                    *slot = Some(resultat);
                }
            });
        let fil = match fil {
            Ok(fil) => Some(fil),
            Err(e) => {
                if let Ok(mut slot) = issue.lock() {
                    *slot = Some(Err(format!("fil du décodage en continu : {e}")));
                }
                None
            }
        };
        let moteur = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok();
        Self {
            rx: Some(rx),
            tampon: Vec::new(),
            position: 0,
            arret,
            abandon,
            issue,
            fil,
            moteur,
        }
    }

    /// L'erreur rendue par le décodeur, s'il est sorti sur une erreur.
    pub(super) fn echec(&self) -> Option<String> {
        self.issue
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|r| r.as_ref().err().cloned()))
    }
}

impl Read for FluxDecode {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        loop {
            if self.arret.load(Ordering::SeqCst) {
                self.abandon.store(true, Ordering::SeqCst);
                return Err(interrompue());
            }
            if self.position < self.tampon.len() {
                let n = destination.len().min(self.tampon.len() - self.position);
                destination[..n].copy_from_slice(&self.tampon[self.position..self.position + n]);
                self.position += n;
                return Ok(n);
            }
            let (Some(rx), Some(moteur)) = (self.rx.as_mut(), self.moteur.as_ref()) else {
                return Ok(0);
            };
            // Le délai se construit DANS le moteur : hors de lui, tokio n'a
            // pas d'horloge où l'inscrire.
            match moteur.block_on(async { tokio::time::timeout(PAS_ANNULATION, rx.recv()).await }) {
                Ok(Some(bloc)) => {
                    self.tampon = bloc;
                    self.position = 0;
                }
                Ok(None) => {
                    if self.arret.load(Ordering::SeqCst) {
                        return Err(interrompue());
                    }
                    return Ok(0);
                }
                // Rien encore : revoir le témoin d'arrêt.
                Err(_) => {}
            }
        }
    }
}

impl Drop for FluxDecode {
    fn drop(&mut self) {
        // Le fil de décodage sort soit de sa lecture HTTP (témoin levé), soit
        // de son envoi (récepteur rendu) — en un pas d'attente au plus.
        self.abandon.store(true, Ordering::SeqCst);
        self.rx.take();
        if let Some(fil) = self.fil.take()
            && fil.join().is_err()
        {
            warn!("local_audio_decodage_en_continu_fil_panique");
        }
    }
}

/// L'en-tête WAV que le décodeur pose en tête du flux décodé.
pub(super) enum EnteteDecodee {
    /// `octets` : ce qui a été lu (l'en-tête, et peut-être du PCM) ;
    /// `format` : ce que `parse_wav_header` en rend.
    Pret {
        octets: Vec<u8>,
        format: (u16, u32, u16, usize),
    },
    /// Un arrêt est tombé avant l'en-tête.
    Interrompu,
    /// Le décodeur a rendu la main sans en-tête.
    Echec,
}

/// Lit l'en-tête WAV du flux décodé — les mêmes relances que l'en-tête du
/// chemin PCM (`header_read_should_retry`).
pub(super) fn lire_l_entete_decodee<R: Read>(lecteur: &mut R, arret: &AtomicBool) -> EnteteDecodee {
    let mut octets = Vec::new();
    let mut bloc = [0u8; 4096];
    loop {
        if let Some(format) = super::parse_wav_header(&octets) {
            return EnteteDecodee::Pret { octets, format };
        }
        if arret.load(Ordering::SeqCst) {
            return EnteteDecodee::Interrompu;
        }
        match lecteur.read(&mut bloc) {
            Ok(0) => break,
            Ok(n) => octets.extend_from_slice(&bloc[..n]),
            Err(ref e) if super::header_read_should_retry(e.kind()) => {}
            Err(_) => break,
        }
    }
    if arret.load(Ordering::SeqCst) {
        EnteteDecodee::Interrompu
    } else {
        EnteteDecodee::Echec
    }
}

/// Le motif d'échec à nommer (#3270), lu dans l'erreur du décodeur.
pub(super) fn motif_de_l_echec(erreur: Option<&str>) -> super::CompressedDecodeFailure {
    use super::CompressedDecodeFailure as Motif;
    match erreur {
        Some(e) if e.starts_with("probe") => Motif::ContainerUnrecognised,
        Some(e) if e.starts_with("no default audio track") || e.starts_with("track has no") => {
            Motif::NoAudioTrack
        }
        Some(e) if e.starts_with("decoder") => Motif::CodecUnsupported,
        _ => Motif::NoSamplesDecoded,
    }
}
