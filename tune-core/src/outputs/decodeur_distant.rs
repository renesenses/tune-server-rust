//! Décodage progressif d'une source HTTP pour les sorties qui décodent
//! elles-mêmes la piste : Devialet AIR (#6911) et Sendspin (#9175).
//!
//! La source est lue par `Range` si le serveur le permet (le saut est alors
//! celui du décodeur), sinon EN CONTINU : rien n'est téléchargé d'avance, rien
//! n'est gardé en mémoire au-delà de quelques blocs. En continu, rien ne se
//! saute dans le corps : le PCM d'avant `depuis_s` est écarté à la sortie du
//! décodeur, sauf pour un flux en direct (radio), qui reprend où il en est.
//!
//! Avant #9175, Sendspin téléchargeait tout le corps en mémoire
//! (`url_to_local_path`, `resp.bytes()`) avant le premier échantillon : de la
//! latence, plus d'un gigaoctet pour une heure en 24/96, et un
//! retéléchargement à chaque saut.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::outputs::airplay::flux_progressif::{
    CorpsEnContinu, TAILLE_ENTETE_WAV, extension_de_l_url,
};

/// Les octets PCM du décodeur (en-tête WAV en tête), et la tâche qui les
/// produit (profondeur, débit).
pub(crate) type DecodeurOuvert = (
    mpsc::Receiver<Vec<u8>>,
    JoinHandle<Result<(u16, u32), String>>,
);

/// La source est-elle une adresse HTTP(S) plutôt qu'un fichier ?
pub(crate) fn est_distante(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Octets d'une trame, lus dans l'en-tête WAV (canaux × octets par
/// échantillon). `None` sur un en-tête illisible.
pub(crate) fn octets_par_trame_de_l_entete(entete: &[u8]) -> Option<usize> {
    if entete.len() < TAILLE_ENTETE_WAV {
        return None;
    }
    let canaux = usize::from(u16::from_le_bytes([entete[22], entete[23]]));
    let bits = usize::from(u16::from_le_bytes([entete[34], entete[35]]));
    let octets = canaux * bits.div_ceil(8);
    (octets > 0).then_some(octets)
}

fn cadence_de_l_entete(entete: &[u8]) -> Option<u32> {
    (entete.len() >= TAILLE_ENTETE_WAV)
        .then(|| u32::from_le_bytes([entete[24], entete[25], entete[26], entete[27]]))
}

/// Ouvre le décodage progressif de `url` vers du PCM à la `cadence`, aux
/// `canaux` et à la `profondeur` demandés, à partir de `depuis_s`.
pub(crate) fn ouvrir_decodeur_distant(
    url: String,
    cadence: Option<u32>,
    canaux: Option<u32>,
    profondeur: Option<u16>,
    depuis_s: f64,
    en_direct: bool,
) -> DecodeurOuvert {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let indice = format!("flux-distant.{}", extension_de_l_url(&url));
    let (tx_decodeur, mut rx_decodeur) = mpsc::channel::<Vec<u8>>(8);
    let a_ecarter = Arc::new(Mutex::new(None::<f64>));
    let decodeur = {
        let a_ecarter = a_ecarter.clone();
        tokio::task::spawn_blocking(move || {
            let (source, depuis_s): (Box<dyn symphonia::core::io::MediaSource>, f64) =
                match crate::audio::http_range::HttpRangeSource::open(&url) {
                    Ok(source) => (Box::new(source), depuis_s),
                    Err(_) => {
                        let reponse = crate::http::client::blocking_builder()
                            .timeout(None)
                            .connect_timeout(Duration::from_secs(10))
                            .build()
                            .map_err(|e| format!("client http : {e}"))?
                            .get(&url)
                            .send()
                            .map_err(|e| {
                                format!(
                                    "source injoignable ({})",
                                    crate::http::client::decrire_erreur_http(&e)
                                )
                            })?;
                        if !reponse.status().is_success() {
                            return Err(format!("la source a répondu HTTP {}", reponse.status()));
                        }
                        if !en_direct && depuis_s > 0.0 {
                            *a_ecarter
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(depuis_s);
                        }
                        (Box::new(CorpsEnContinu(Mutex::new(reponse))), 0.0)
                    }
                };
            crate::audio::decode::decode_vers_pcm_en_continu_depuis(
                &indice,
                Some(source),
                cadence,
                canaux,
                profondeur,
                tx_decodeur,
                32 * 1024,
                depuis_s,
            )
        })
    };
    // Le relais : l'en-tête WAV passe tel quel, puis le PCM d'avant la
    // position voulue est écarté quand la source n'a pas pu sauter.
    tokio::spawn(async move {
        let mut entete: Vec<u8> = Vec::with_capacity(TAILLE_ENTETE_WAV);
        // Octets de PCM encore à écarter ; `None` tant que l'en-tête n'est
        // pas passé.
        let mut a_ecarter_octets: Option<usize> = None;
        while let Some(bloc) = rx_decodeur.recv().await {
            let mut bloc = &bloc[..];
            if a_ecarter_octets.is_none() {
                let n = (TAILLE_ENTETE_WAV - entete.len()).min(bloc.len());
                entete.extend_from_slice(&bloc[..n]);
                bloc = &bloc[n..];
                if entete.len() < TAILLE_ENTETE_WAV {
                    continue;
                }
                let depuis = *a_ecarter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                a_ecarter_octets = Some(
                    match (
                        depuis,
                        cadence_de_l_entete(&entete),
                        octets_par_trame_de_l_entete(&entete),
                    ) {
                        (Some(s), Some(c), Some(o)) => (s * f64::from(c)) as usize * o,
                        _ => 0,
                    },
                );
                if tx.send(std::mem::take(&mut entete)).await.is_err() {
                    return;
                }
            }
            if let Some(reste) = a_ecarter_octets.as_mut() {
                let n = (*reste).min(bloc.len());
                *reste -= n;
                bloc = &bloc[n..];
            }
            if !bloc.is_empty() && tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    (rx, decodeur)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entete(canaux: u16, cadence: u32, bits: u16) -> Vec<u8> {
        let mut e = vec![0u8; TAILLE_ENTETE_WAV];
        e[..4].copy_from_slice(b"RIFF");
        e[8..12].copy_from_slice(b"WAVE");
        e[22..24].copy_from_slice(&canaux.to_le_bytes());
        e[24..28].copy_from_slice(&cadence.to_le_bytes());
        e[34..36].copy_from_slice(&bits.to_le_bytes());
        e
    }

    /// Devialet AIR (stéréo 24 bits) garde ses 6 octets par trame ; Sendspin
    /// en déduit d'autres de son format.
    #[test]
    fn la_trame_se_lit_dans_l_entete_9175() {
        assert_eq!(
            octets_par_trame_de_l_entete(&entete(2, 96_000, 24)),
            Some(6)
        );
        assert_eq!(
            octets_par_trame_de_l_entete(&entete(2, 44_100, 16)),
            Some(4)
        );
        assert_eq!(
            octets_par_trame_de_l_entete(&entete(1, 48_000, 32)),
            Some(4)
        );
        assert_eq!(octets_par_trame_de_l_entete(&entete(0, 48_000, 24)), None);
        assert_eq!(octets_par_trame_de_l_entete(&[0u8; 10]), None);
        assert_eq!(cadence_de_l_entete(&entete(2, 96_000, 24)), Some(96_000));
    }
}
