//! Le fil qui décode la piste et prépare les charges RTP.
//!
//! Même chemin que la sortie AirPlay 1 (#6673) : le décodeur progressif du
//! cœur, [`tune_core::audio::decode::decode_vers_pcm_en_continu_depuis`], à
//! cadence, canaux et profondeur IMPOSÉS. Le rééchantillonnage (rubato, noyau
//! choisi selon le couple de cadences) et l'adaptation des canaux se font donc
//! dans le décodeur ; ce fil vérifie l'en-tête WAV qu'il envoie en tête,
//! applique le volume logiciel s'il est allumé, et découpe en paquets.
//!
//! Étape E3 : un producteur porte un FLUX, c'est-à-dire une suite de pistes.
//! Quand la piste en cours est entièrement décodée, il prend la suivante
//! préparée ([`PisteSuivante`], posée par `set_next_media`, son décodeur déjà
//! lancé) et continue de remplir le MÊME paquetiseur : le dernier paquet de
//! la piste A porte les premières trames de la piste B, sans silence ni
//! rupture d'horodatage. Chaque piste est rééchantillonnée vers la cadence du
//! flux par son propre décodeur : une suite 44,1 kHz → 96 kHz reste un flux
//! 48 kHz continu. La trame du flux où chaque piste commence est notée dans
//! les [`Frontiere`]s, pour que la sortie dise quelle piste joue et à quelle
//! position.
//!
//! Tout est bloquant : ce fil et celui du décodeur sont des fils système,
//! hors du runtime tokio. La contre-pression vient du canal borné vers le
//! cadenceur : le décodeur ne prend que quelques centaines de millisecondes
//! d'avance.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

/// Fait tourner le décodeur sur la source, à partir de `depuis_ms`.
/// Bloquant.
///
/// Un corps HTTP lu en continu ne sait pas sauter : le décodeur part alors
/// du début et `saut` reçoit, AVANT le premier bloc envoyé, le nombre de
/// trames (à la cadence du flux) que le lecteur doit écarter.
pub fn decoder(
    source: &SourceAudio,
    cadence: u32,
    canaux: u16,
    depuis_ms: u64,
    saut: &AtomicU64,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<(u16, u32), String> {
    let depuis_s = depuis_ms as f64 / 1000.0;
    let decoder_sur = |chemin: &str, s: Option<Box<dyn MediaSource>>, depuis_s: f64| {
        tune_core::audio::decode::decode_vers_pcm_en_continu_depuis(
            chemin,
            s,
            Some(cadence),
            Some(u32::from(canaux)),
            Some(PROFONDEUR_DU_DECODEUR),
            tx.clone(),
            TAILLE_DES_BLOCS,
            depuis_s,
        )
    };
    let url = match source {
        SourceAudio::Fichier(chemin) => return decoder_sur(chemin, None, depuis_s),
        SourceAudio::Url(url) => url,
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return decoder_sur(url, None, depuis_s);
    }
    let indice = format!("flux-distant.{}", extension_de_l_url(url));
    match tune_core::audio::http_range::HttpRangeSource::open(url) {
        Ok(s) => decoder_sur(&indice, Some(Box::new(s)), depuis_s),
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
            saut.store(trames_de(depuis_ms, cadence), Ordering::Release);
            decoder_sur(
                &indice,
                Some(Box::new(CorpsEnContinu(Mutex::new(reponse)))),
                0.0,
            )
        }
    }
}

/// Les trames de `ms` millisecondes à `cadence`.
pub fn trames_de(ms: u64, cadence: u32) -> u64 {
    ms * u64::from(cadence) / 1000
}

/// La tâche du décodeur : `(profondeur, cadence)` rendues, ou l'erreur.
pub type TacheDuDecodeur = tokio::task::JoinHandle<Result<(u16, u32), String>>;

/// Un décodeur lancé : son PCM, sa tâche, les trames à écarter en tête.
pub struct DecodeurLance {
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub tache: TacheDuDecodeur,
    pub saut: Arc<AtomicU64>,
}

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
    let d = lancer_le_decodeur_depuis(runtime, source, cadence, canaux, 0);
    (d.rx, d.tache)
}

/// [`lancer_le_decodeur`] à partir de `depuis_ms` (saut dans la piste).
pub fn lancer_le_decodeur_depuis(
    runtime: &tokio::runtime::Handle,
    source: SourceAudio,
    cadence: u32,
    canaux: u16,
    depuis_ms: u64,
) -> DecodeurLance {
    let (tx, rx) = mpsc::channel(CAPACITE_DU_CANAL);
    let saut = Arc::new(AtomicU64::new(0));
    let s = saut.clone();
    let tache =
        runtime.spawn_blocking(move || decoder(&source, cadence, canaux, depuis_ms, &s, tx));
    DecodeurLance { rx, tache, saut }
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
    /// Trames à écarter en tête (saut qu'une source n'a pas su faire), lues
    /// après l'en-tête : le décodeur les pose avant d'envoyer quoi que ce soit.
    saut: Option<Arc<AtomicU64>>,
    a_ecarter: u64,
}

impl LecteurPcm {
    pub fn new(rx: mpsc::Receiver<Vec<u8>>, cadence: u32, canaux: u16) -> Self {
        Self {
            rx,
            entete: Vec::with_capacity(TAILLE_ENTETE_WAV),
            reste: Vec::new(),
            cadence,
            canaux,
            saut: None,
            a_ecarter: 0,
        }
    }

    /// Écarte en tête les trames que le décodeur posera dans `saut`.
    pub fn avec_saut(mut self, saut: Arc<AtomicU64>) -> Self {
        self.saut = Some(saut);
        self
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
                    if let Some(s) = self.saut.as_ref() {
                        self.a_ecarter = s.load(Ordering::Acquire) * u64::from(self.canaux) * 4;
                    }
                }
            }
            if self.a_ecarter > 0 {
                let n = (self.a_ecarter.min(bloc.len() as u64)) as usize;
                bloc = &bloc[n..];
                self.a_ecarter -= n as u64;
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

/// Ce que la sortie sait d'une piste : de quoi la relire (saut) et la
/// décrire (état).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfosPiste {
    pub source: SourceAudio,
    pub uri: String,
    pub titre: Option<String>,
    pub artiste: Option<String>,
    pub duree_ms: u64,
}

/// Une piste dont le décodeur tourne déjà.
pub struct PisteLancee {
    pub infos: InfosPiste,
    pub depuis_ms: u64,
    pub decodeur: DecodeurLance,
}

impl PisteLancee {
    pub fn lancer(
        runtime: &tokio::runtime::Handle,
        infos: InfosPiste,
        depuis_ms: u64,
        cadence: u32,
        canaux: u16,
    ) -> Self {
        let decodeur =
            lancer_le_decodeur_depuis(runtime, infos.source.clone(), cadence, canaux, depuis_ms);
        Self {
            infos,
            depuis_ms,
            decodeur,
        }
    }
}

/// La piste suivante préparée (`set_next_media`), que le producteur prend
/// quand la piste en cours est entièrement décodée.
pub type PisteSuivante = Arc<Mutex<Option<PisteLancee>>>;

/// Où commence une piste dans un flux : la trame du flux (0 = première trame
/// émise après la pose du flux), la piste, et sa position de départ dans le
/// fichier (non nulle après un saut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontiere {
    pub trame: u64,
    pub infos: InfosPiste,
    pub depuis_ms: u64,
}

/// Les frontières d'un flux, la première posée par la sortie.
pub type Frontieres = Arc<Mutex<Vec<Frontiere>>>;

/// La frontière de la piste qui joue à la trame `trame` du flux.
pub fn frontiere_a(frontieres: &[Frontiere], trame: u64) -> Option<&Frontiere> {
    frontieres.iter().rev().find(|f| f.trame <= trame)
}

/// Ce qu'un producteur reçoit, hors piste et canal.
pub struct ParametresProducteur {
    pub cadence: u32,
    pub canaux: u16,
    pub trames_par_paquet: usize,
    pub gain: Arc<Gain>,
    pub suivante: PisteSuivante,
    pub frontieres: Frontieres,
    pub arret: Arc<AtomicBool>,
    pub echec: Arc<Mutex<Option<String>>>,
}

/// Lance le producteur d'un flux : la piste `premiere` (décodeur déjà
/// lancé), puis les suivantes préparées, dans `tx`. La première frontière
/// est à poser par l'appelant ; le producteur ajoute celles des suivantes.
/// L'erreur éventuelle (décodeur, en-tête) est déposée dans `echec`.
pub fn lancer(
    runtime: tokio::runtime::Handle,
    premiere: PisteLancee,
    p: ParametresProducteur,
    tx: SyncSender<Message>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("aes67-pcm".into())
        .spawn(move || produire(&runtime, premiere, &p, &tx))
}

fn produire(
    runtime: &tokio::runtime::Handle,
    premiere: PisteLancee,
    p: &ParametresProducteur,
    tx: &SyncSender<Message>,
) {
    let canaux = u64::from(p.canaux);
    let mut paquetiseur = Paquetiseur::new(usize::from(p.canaux), p.trames_par_paquet);
    let mut echantillons_pousses: u64 = 0;
    let mut erreur = None;
    let mut interrompu = false;
    let mut piste = premiere;
    loop {
        let PisteLancee {
            infos,
            decodeur: DecodeurLance { rx, tache, saut },
            ..
        } = piste;
        let mut lecteur = LecteurPcm::new(rx, p.cadence, p.canaux).avec_saut(saut);
        'lecture: loop {
            if p.arret.load(Ordering::Acquire) {
                interrompu = true;
                break;
            }
            match lecteur.suivant() {
                Ok(Some(mut bloc)) => {
                    appliquer_le_gain(&mut bloc, p.gain.lire());
                    echantillons_pousses += bloc.len() as u64;
                    for charge in paquetiseur.pousser(&bloc) {
                        // Un bloc fait ~85 paquets et le canal se vide à un
                        // paquet par ms : sans ce test, un saut attendrait
                        // la fin du bloc.
                        if p.arret.load(Ordering::Acquire)
                            || tx.send(Message::Charge(charge)).is_err()
                        {
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
        // L'erreur du décodeur prime : « flux sans en-tête » n'en est que la
        // conséquence.
        match attendre_le_decodeur(runtime, tache) {
            Ok(_) => {}
            Err(e) if !interrompu => erreur = Some(e),
            Err(_) => {}
        }
        if let Some(e) = erreur.as_ref() {
            warn!(error = %e, source = ?infos.source, "aes67_decodage_echoue");
        }
        if p.arret.load(Ordering::Acquire) {
            interrompu = true;
        }
        if interrompu || erreur.is_some() {
            break;
        }
        let Some(suivante) = p.suivante.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            break;
        };
        let trame = echantillons_pousses / canaux;
        info!(
            trame,
            precedente = %infos.uri,
            suivante = %suivante.infos.uri,
            "aes67_enchainement_sans_blanc"
        );
        p.frontieres
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Frontiere {
                trame,
                infos: suivante.infos.clone(),
                depuis_ms: suivante.depuis_ms,
            });
        piste = suivante;
    }
    if !interrompu
        && erreur.is_none()
        && let Some(dernier) = paquetiseur.vider()
    {
        let _ = tx.send(Message::Charge(dernier));
    }
    let _ = tx.send(Message::Fin);
    if let Some(e) = erreur {
        *p.echec.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
    }
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
