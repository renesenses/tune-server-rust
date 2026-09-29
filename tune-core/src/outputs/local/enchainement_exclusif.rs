//! #5204 — l'enchaînement sans blanc sur la sortie locale en mode exclusif.
//!
//! Jusqu'ici `supports_internal_gapless()` rendait `!exclusive_mode` : toute
//! sortie exclusive se déclarait incapable d'enchaîner, parce que ses bras
//! (`bras_wasapi.rs`, `bras_asio.rs`, `bras_coreaudio.rs`) sortaient à l'EOF
//! sans consommer le `next_media` préparé. Le sondeur attendait donc la fin
//! naturelle et relançait un `play_url`, qui fermait puis rouvrait le
//! périphérique **au même format** : 267 ms côté serveur plus la reprise du
//! DAC, un blanc audible entre deux pistes d'un même album (Jean Valjean,
//! fil 1890, WASAPI exclusif + PURE, 88,2 kHz / 32 bits).
//!
//! Ce module porte ce qui se décide SANS périphérique, donc ce que `cargo test`
//! juge sur Linux :
//!
//! - [`bras_de_lecture`] : quel bras `play_url` prend, et s'il sait enchaîner.
//!   La capacité publiée au sondeur en découle — elle ne peut plus mentir sur
//!   le bras réellement emprunté ;
//! - [`decider_l_enchainement_natif`] : à format égal, on enchaîne sans
//!   refermer le flux ; à format différent, on laisse la piste finir et la fin
//!   naturelle rouvre le périphérique au nouveau format, comme en 0.9.165 ;
//! - [`lire_l_entete_enchainee`] et [`ouvrir_la_piste_suivante`] : la lecture
//!   de l'en-tête WAV de la piste suivante, jusque-là écrite en ligne dans la
//!   boucle du chemin partagé. Un seul exemplaire désormais, pour les deux
//!   chemins.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, info, warn};

use super::lecture_http::LecteurHttpAnnulable;
use super::{AudioSpec, FormatOuvert, header_read_should_retry, parse_wav_header};

/// Le bras de `play_url` qu'une sortie emprunte, déduit exactement comme les
/// `if` du fil de lecture le déduisent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrasDeLecture {
    /// Le chemin cpal partagé (mixeur système, ou ALSA `hw:` sous Linux).
    CpalPartage,
    /// macOS, mode « hog » CoreAudio (`bras_coreaudio.rs`).
    CoreAudioExclusif,
    /// Windows, pilote ASIO (`bras_asio.rs`, feature `asio`).
    AsioExclusif,
    /// Windows, WASAPI exclusif événementiel (`bras_wasapi.rs`).
    WasapiExclusif,
}

impl BrasDeLecture {
    /// Ce bras enchaîne-t-il la piste préparée par `set_next_media` sans
    /// refermer le périphérique ?
    ///
    /// - cpal partagé : oui, depuis toujours ;
    /// - WASAPI exclusif : oui depuis #5204, à format égal (sinon il rend la
    ///   main et la fin naturelle rouvre) ;
    /// - ASIO exclusif : oui depuis #5204 (seconde tranche), à format égal,
    ///   sur la route native (`chaine_par_la_boucle.rs`). Sa route traitée
    ///   (anneau flottant) n'enchaîne pas : le bras lève `chain_exhausted` dès
    ///   l'ouverture, et la sonde de la sortie retombe à « non » avant que le
    ///   sondeur arme — sans quoi il attendrait une transition qui ne vient
    ///   jamais (DEvir, ASIO Fireface : album figé après chaque piste) ;
    /// - CoreAudio exclusif : **non**, son bras sort encore à l'EOF sans
    ///   consommer la suivante.
    pub(crate) fn sait_enchainer(self) -> bool {
        matches!(
            self,
            Self::CpalPartage | Self::WasapiExclusif | Self::AsioExclusif
        )
    }
}

/// Quel bras `play_url` prend — fonction pure, la même règle que ses `if` :
///
/// - macOS + exclusif → CoreAudio ;
/// - Windows + exclusif + `audio_backend == "asio"` + feature `asio` → ASIO ;
/// - Windows + exclusif + `audio_backend != "asio"` → WASAPI ;
/// - tout le reste → cpal partagé (dont Linux, où l'exclusif n'existe pas :
///   le bras partagé ouvre `hw:` et enchaîne ; et Windows + `"asio"` sans la
///   feature, qui retombe sur le partagé).
pub(crate) fn bras_de_lecture(
    target_os: &str,
    asio_feature: bool,
    exclusive_mode: bool,
    audio_backend: &str,
) -> BrasDeLecture {
    if !exclusive_mode {
        return BrasDeLecture::CpalPartage;
    }
    match target_os {
        "macos" => BrasDeLecture::CoreAudioExclusif,
        "windows" if audio_backend == "asio" => {
            if asio_feature {
                BrasDeLecture::AsioExclusif
            } else {
                BrasDeLecture::CpalPartage
            }
        }
        "windows" => BrasDeLecture::WasapiExclusif,
        _ => BrasDeLecture::CpalPartage,
    }
}

/// Le bras de CETTE compilation.
pub(crate) fn bras_de_cette_plateforme(exclusive_mode: bool, audio_backend: &str) -> BrasDeLecture {
    bras_de_lecture(
        std::env::consts::OS,
        cfg!(feature = "asio"),
        exclusive_mode,
        audio_backend,
    )
}

/// Ce que la frontière d'une piste enchaînée décide sur un transport natif
/// exclusif (WASAPI), qui ouvre le périphérique AU FORMAT SOURCE et ne
/// convertit rien.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnchainementNatif {
    /// Même cadence, même profondeur, mêmes canaux : les mots de la suivante
    /// entrent dans le même anneau, le flux reste ouvert.
    Enchainer,
    /// Le format change : le périphérique exclusif ne peut pas le suivre sans
    /// être réinitialisé. La piste courante se termine proprement, la fin
    /// naturelle relance la suivante par `play_url`, qui rouvre au nouveau
    /// format — le comportement de 0.9.165, limité désormais à ce cas.
    Rouvrir,
}

/// LA règle de la frontière exclusive — fonction pure.
///
/// `source` est le format de la piste qui se termine, `ouvert` celui que le
/// périphérique a accepté. Un transport exclusif n'a ni rééchantillonneur ni
/// adaptation de canaux : le moindre écart de cadence, de profondeur ou de
/// canaux impose de rouvrir.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn decider_l_enchainement_natif(
    source: AudioSpec,
    ouvert: FormatOuvert,
    suivante: AudioSpec,
) -> EnchainementNatif {
    let meme_format_que_la_source = suivante == source;
    let le_peripherique_le_porte =
        suivante.cadence() == ouvert.cadence && suivante.canaux() == ouvert.canaux;
    if meme_format_que_la_source && le_peripherique_le_porte {
        EnchainementNatif::Enchainer
    } else {
        EnchainementNatif::Rouvrir
    }
}

/// L'en-tête WAV d'une piste enchaînée, lu et typé.
#[derive(Debug)]
pub(crate) struct EnteteEnchainee {
    /// Les octets lus (jusqu'à 4 096) : ce qui suit `data_offset` est déjà du
    /// PCM de la piste suivante.
    pub(crate) octets: Vec<u8>,
    pub(crate) spec: AudioSpec,
    pub(crate) data_offset: usize,
}

impl EnteteEnchainee {
    /// Le PCM déjà lu avec l'en-tête.
    pub(crate) fn amorce(&self) -> &[u8] {
        if self.data_offset < self.octets.len() {
            &self.octets[self.data_offset..]
        } else {
            &[]
        }
    }
}

/// Pourquoi une piste suivante ne peut pas être enchaînée. Tous ont la même
/// issue : la piste courante se termine, la fin naturelle prend le relais.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusDEnchainement {
    /// Un arrêt est tombé pendant la lecture de l'en-tête.
    Interrompu,
    /// Erreur de lecture non transitoire.
    LectureEchouee,
    /// Aucun octet.
    EnteteVide,
    /// Pas un WAV, ou un format que Tune ne sait pas typer.
    PasDuWav,
}

/// Lit l'en-tête WAV d'une piste enchaînée.
///
/// La session de transcodage suivante vient peut-être de démarrer : sa
/// première lecture peut expirer avant que l'en-tête soit prêt. On reboucle
/// sur `TimedOut`/`WouldBlock`, comme pour la piste initiale, au lieu de
/// rompre la chaîne — ce qui sauterait la piste. Les noms d'événement sont
/// ceux de la boucle du chemin partagé, d'où ce code vient.
pub(crate) fn lire_l_entete_enchainee<R: Read>(
    lecteur: &mut R,
    arret: &AtomicBool,
) -> Result<EnteteEnchainee, RefusDEnchainement> {
    let mut octets = vec![0u8; 4096];
    let lus = loop {
        if arret.load(Ordering::Relaxed) {
            return Err(RefusDEnchainement::Interrompu);
        }
        match lecteur.read(&mut octets) {
            Ok(n) => break n,
            Err(ref e) if header_read_should_retry(e.kind()) => continue,
            Err(e) => {
                if arret.load(Ordering::SeqCst) {
                    return Err(RefusDEnchainement::Interrompu);
                }
                warn!(error = %e, "local_audio_gapless_header_read_failed");
                return Err(RefusDEnchainement::LectureEchouee);
            }
        }
    };
    if lus == 0 {
        if arret.load(Ordering::SeqCst) {
            return Err(RefusDEnchainement::Interrompu);
        }
        warn!("local_audio_gapless_header_read_empty");
        return Err(RefusDEnchainement::EnteteVide);
    }
    octets.truncate(lus);
    // Le format devient un TYPE avant qu'une seule trame ne soit décodée :
    // un refus ici laisse la piste courante se terminer proprement.
    let Some((canaux, cadence, bits, data_offset)) = parse_wav_header(&octets) else {
        info!("local_audio_gapless_next_not_wav_falling_back");
        return Err(RefusDEnchainement::PasDuWav);
    };
    let Some(spec) = AudioSpec::depuis_entete(cadence, bits, canaux) else {
        info!("local_audio_gapless_next_not_wav_falling_back");
        return Err(RefusDEnchainement::PasDuWav);
    };
    Ok(EnteteEnchainee {
        octets,
        spec,
        data_offset,
    })
}

/// Ouvre le flux HTTP de la piste suivante. `None` : injoignable ou en
/// erreur — la chaîne s'arrête, la fin naturelle prend le relais.
pub(crate) fn ouvrir_la_piste_suivante(
    url: &str,
    arret: &Arc<AtomicBool>,
) -> Option<LecteurHttpAnnulable> {
    match LecteurHttpAnnulable::ouvrir(url, arret.clone()) {
        Ok(r) if r.status().is_success() || r.status().as_u16() == 206 => Some(r),
        Ok(r) => {
            warn!(
                status = %r.status(),
                url = %url,
                "local_audio_gapless_http_error"
            );
            None
        }
        Err(e) => {
            if arret.load(Ordering::SeqCst) {
                debug!("local_audio_gapless_http_fetch_cancelled");
            } else {
                warn!(
                    error = %e,
                    url = %url,
                    "local_audio_gapless_http_fetch_failed"
                );
            }
            None
        }
    }
}
