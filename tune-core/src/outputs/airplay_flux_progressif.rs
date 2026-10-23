//! #6673 — AirPlay 1 : le PCM arrive AU FIL DE L'EAU après RECORD.
//!
//! `stream_to_airplay` téléchargeait tout le corps de `/stream/…`, décodait
//! toute la piste, puis seulement émettait le premier paquet RTP. Le récepteur
//! était en RECORD depuis le début de cette attente : un Denon NR1602
//! (fil 2261) raccroche après quelques secondes sans RTP. Décision de
//! Bertrand du 22/10 : voie A, le flux progressif, comme les autres sorties.
//!
//! Ce module ouvre le décodeur progressif des fichiers locaux
//! ([`crate::audio::decode::decode_vers_pcm_en_continu`]) sur la source de la
//! piste — le fichier lui-même, le corps HTTP lu par `Range` quand le serveur
//! le permet (M4A dont l'atome `moov` est en fin de fichier), ou le corps lu en
//! continu sinon (session WAV à la volée) — et rend le PCM L16 gros-boutiste
//! bloc par bloc. La cadence (44,1 kHz) et les canaux (2) sont des garanties
//! du décodeur sur chaque bloc ; l'en-tête WAV qu'il envoie en tête est lu et
//! VÉRIFIÉ ici, puis retiré.
//!
//! Le décodeur sort en 32 bits : la conversion vers L16 reste
//! [`super::pcm_i32_to_l16_be`], la seule de la sortie, qui sature (#2311).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;

use symphonia::core::io::MediaSource;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{CHANNELS, SAMPLE_RATE};

/// La profondeur demandée au décodeur. Voir le module.
const PROFONDEUR_DU_DECODEUR: u16 = 32;
/// Taille des blocs du décodeur, comme pour le transcodage des fichiers.
const TAILLE_DES_BLOCS: usize = 32_768;
/// Blocs en attente entre le décodeur et la boucle RTP : ~6 s de 44,1 kHz
/// stéréo 32 bits. Le décodeur prend de l'avance sans charger la piste.
const CAPACITE_DU_CANAL: usize = 64;
/// L'en-tête WAV que toutes les branches du décodeur envoient en tête.
const TAILLE_ENTETE_WAV: usize = 44;

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

/// L'extension du chemin d'une URL (`/stream/<id>.wav?x=1` → `wav`), qui sert
/// d'indice de format au décodeur.
pub(super) fn extension_de_l_url(url: &str) -> String {
    let chemin = url.split(['?', '#']).next().unwrap_or(url);
    let dernier = chemin.rsplit('/').next().unwrap_or(chemin);
    dernier
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty() && ext.len() <= 5)
        .unwrap_or_default()
}

/// Ouvre la source de la piste et y fait tourner le décodeur. Bloquant :
/// appelé dans `spawn_blocking`.
fn decoder(url: &str, tx: mpsc::Sender<Vec<u8>>) -> Result<(u16, u32), String> {
    let decoder_sur = |chemin: &str, source: Option<Box<dyn MediaSource>>| {
        crate::audio::decode::decode_vers_pcm_en_continu(
            chemin,
            source,
            Some(SAMPLE_RATE),
            Some(CHANNELS as u32),
            Some(PROFONDEUR_DU_DECODEUR),
            tx.clone(),
            TAILLE_DES_BLOCS,
        )
    };
    if let Some(chemin) = url.strip_prefix("file://") {
        return decoder_sur(chemin, None);
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return decoder_sur(url, None);
    }
    let indice = format!("flux-distant.{}", extension_de_l_url(url));
    match crate::audio::http_range::HttpRangeSource::open(url) {
        Ok(source) => {
            info!(url, "airplay_flux_progressif_range");
            decoder_sur(&indice, Some(Box::new(source)))
        }
        Err(raison) => {
            info!(url, raison = %raison, "airplay_flux_progressif_continu");
            let reponse = crate::http::client::blocking_builder()
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

/// Le PCM de la piste, en L16 gros-boutiste, bloc par bloc.
pub(super) struct FluxPcmAirplay {
    rx: mpsc::Receiver<Vec<u8>>,
    /// Octets d'en-tête WAV encore à lire avant le PCM.
    entete: Vec<u8>,
    /// Octets PCM 32 bits en attente d'un échantillon complet.
    reste: Vec<u8>,
    /// Octets de PCM rendus jusqu'ici.
    rendus: u64,
    decodeur: Option<tokio::task::JoinHandle<Result<(u16, u32), String>>>,
    echec: Option<String>,
}

impl FluxPcmAirplay {
    /// Lance le décodeur sur `url`. Ne bloque pas : le premier bloc arrive
    /// par [`Self::suivant`].
    pub(super) fn ouvrir(url: &str) -> Self {
        let (tx, rx) = mpsc::channel(CAPACITE_DU_CANAL);
        let url = url.to_string();
        let decodeur = tokio::task::spawn_blocking(move || decoder(&url, tx));
        Self {
            rx,
            entete: Vec::with_capacity(TAILLE_ENTETE_WAV),
            reste: Vec::new(),
            rendus: 0,
            decodeur: Some(decodeur),
            echec: None,
        }
    }

    /// Octets PCM (L16) déjà rendus. Zéro à la fin du flux : rien n'a joué, la
    /// piste peut repartir par le chemin d'avant.
    pub(super) fn octets_rendus(&self) -> u64 {
        self.rendus
    }

    /// L'erreur du décodeur ou de l'en-tête, une fois le flux fini.
    pub(super) fn echec(&self) -> Option<&str> {
        self.echec.as_deref()
    }

    /// Le bloc L16 suivant, ou `None` à la fin du flux (ou sur erreur, que
    /// [`Self::echec`] rend ensuite).
    pub(super) async fn suivant(&mut self) -> Option<Vec<u8>> {
        loop {
            let Some(bloc) = self.rx.recv().await else {
                self.terminer().await;
                return None;
            };
            let mut bloc = &bloc[..];
            if self.entete.len() < TAILLE_ENTETE_WAV {
                let n = (TAILLE_ENTETE_WAV - self.entete.len()).min(bloc.len());
                self.entete.extend_from_slice(&bloc[..n]);
                bloc = &bloc[n..];
                if self.entete.len() == TAILLE_ENTETE_WAV
                    && let Err(raison) = verifier_l_entete(&self.entete)
                {
                    warn!(raison = %raison, "airplay_flux_progressif_entete_refusee");
                    self.echec = Some(raison);
                    self.rx.close();
                    return None;
                }
            }
            self.reste.extend_from_slice(bloc);
            let complets = self.reste.len() / 4 * 4;
            if complets == 0 {
                continue;
            }
            let echantillons: Vec<i32> = self.reste[..complets]
                .chunks_exact(4)
                .map(|o| i32::from_le_bytes([o[0], o[1], o[2], o[3]]))
                .collect();
            self.reste.drain(..complets);
            let l16 = super::pcm_i32_to_l16_be(&echantillons, PROFONDEUR_DU_DECODEUR);
            self.rendus += l16.len() as u64;
            return Some(l16);
        }
    }

    async fn terminer(&mut self) {
        if let Some(decodeur) = self.decodeur.take() {
            match decodeur.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => self.echec = self.echec.take().or(Some(e)),
                Err(e) => self.echec = self.echec.take().or(Some(format!("decode join: {e}"))),
            }
        }
        if self.echec.is_none() && self.entete.len() < TAILLE_ENTETE_WAV {
            self.echec = Some("flux décodé sans en-tête WAV".into());
        }
    }
}

/// L'en-tête doit annoncer exactement ce que la session RTP a déclaré dans
/// son ANNOUNCE : sinon on émettrait un payload mensonger.
fn verifier_l_entete(entete: &[u8]) -> Result<(), String> {
    if &entete[0..4] != b"RIFF" || &entete[8..12] != b"WAVE" {
        return Err("en-tête WAV absent en tête du flux décodé".into());
    }
    let canaux = u16::from_le_bytes([entete[22], entete[23]]);
    let cadence = u32::from_le_bytes([entete[24], entete[25], entete[26], entete[27]]);
    let profondeur = u16::from_le_bytes([entete[34], entete[35]]);
    if canaux != CHANNELS || cadence != SAMPLE_RATE || profondeur != PROFONDEUR_DU_DECODEUR {
        return Err(format!(
            "flux décodé en {canaux} canaux, {cadence} Hz, {profondeur} bits : \
             attendu {CHANNELS} canaux, {SAMPLE_RATE} Hz, {PROFONDEUR_DU_DECODEUR} bits"
        ));
    }
    Ok(())
}

impl Drop for FluxPcmAirplay {
    fn drop(&mut self) {
        // Fermer le canal : le prochain envoi du décodeur échoue et il
        // s'arrête de lui-même (arrêt, piste suivante).
        self.rx.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_extension_de_l_url_sert_d_indice() {
        assert_eq!(
            extension_de_l_url("http://10.0.0.2:8888/stream/ab12.wav"),
            "wav"
        );
        assert_eq!(
            extension_de_l_url("http://h/stream/ab12.FLAC?t=1#x"),
            "flac"
        );
        assert_eq!(extension_de_l_url("http://h/stream/ab12"), "");
        assert_eq!(extension_de_l_url("http://h.example/stream/"), "");
    }

    #[test]
    fn un_entete_qui_ne_correspond_pas_a_l_announce_est_refuse() {
        let bon = crate::audio::wav::build_wav_header_streaming(2, 44_100, 32);
        assert!(verifier_l_entete(&bon).is_ok());
        let mono = crate::audio::wav::build_wav_header_streaming(1, 44_100, 32);
        assert!(verifier_l_entete(&mono).unwrap_err().contains("1 canaux"));
        let cadence = crate::audio::wav::build_wav_header_streaming(2, 48_000, 32);
        assert!(
            verifier_l_entete(&cadence)
                .unwrap_err()
                .contains("48000 Hz")
        );
    }
}
