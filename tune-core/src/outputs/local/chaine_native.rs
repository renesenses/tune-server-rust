//! #5204 — la boucle de lecture d'un bras natif exclusif (WASAPI), et
//! l'enchaînement de la piste suivante SANS refermer le flux.
//!
//! Avant #5204, `bras_wasapi.rs` lisait sa piste jusqu'à l'EOF puis rendait la
//! main : le périphérique était arrêté, et le sondeur relançait la suivante par
//! `play_url`, qui le rouvrait au même format — un blanc à chaque piste.
//!
//! La boucle vit ici, générique sur la source (`Read`) et sur un
//! [`HoteDeChaineNative`] qui porte ce qui touche au périphérique et à la
//! zone : c'est ce qui permet à `cargo test` de la juger sur Linux, avec des
//! pistes en mémoire et le puits de capture, alors que le bras, lui, ne se
//! compile que sous Windows. Même `cfg` que `etage_natif.rs`.
//!
//! À l'EOF d'une piste :
//! 1. le reliquat de quarantaine 24 bits part brut (comme avant) ;
//! 2. si une suivante est en réserve, son flux est ouvert et son en-tête lu ;
//! 3. à format égal ([`EtageNatif::enchainer_la_piste`]), ses mots entrent
//!    dans le MÊME puits : le flux reste ouvert, aucun blanc ;
//! 4. sinon la chaîne rend la main — la piste courante se termine et la fin
//!    naturelle rouvre le périphérique au nouveau format, comme en 0.9.165.

use std::io::Read;

use super::enchainement_exclusif::EnteteEnchainee;
use super::etage_natif::{EcritureNative, EtageNatif};
use super::*;
use crate::outputs::traits::PuitsNatif;

/// Ce que [`HoteDeChaineNative::preparer_la_suivante`] a trouvé.
pub(super) enum Suivante<R> {
    /// Rien en réserve : `set_next_media` n'a rien préparé.
    Aucune,
    /// Une suivante était en réserve, mais son flux est injoignable, vide, en
    /// erreur, ou n'est pas un WAV : elle ne peut pas être enchaînée.
    Refusee,
    /// Flux ouvert, en-tête lu et typé.
    Prete { lecteur: R, entete: EnteteEnchainee },
}

/// Ce que la chaîne emprunte au bras : les témoins d'arrêt, la publication
/// vers la zone, et la réserve de la piste suivante.
pub(super) trait HoteDeChaineNative {
    type Lecteur: Read;
    /// Un ordre d'arrêt est-il arrivé ? (consomme le message, comme le bras.)
    fn arret_recu(&mut self) -> bool;
    /// Le silence forcé par `stop()` / un `play_url` plus récent.
    fn silence_force(&self) -> bool;
    /// Après chaque poussée : l'état DoP, le volume qui le suit, le verdict
    /// bit-perfect du chemin de signal.
    fn publier_le_verdict(&mut self, dop: bool, bit_perfect: bool);
    fn publier_la_position(&mut self, position_ms: u64);
    /// Retire la suivante de sa réserve, ouvre son flux, lit son en-tête.
    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur>;
    /// L'enchaînement est acquis : publier le morceau suivant (adresse, titre,
    /// durée) et remettre la position à zéro. Le sondeur y lit la chute de
    /// position d'une transition interne et avance la file sans `play`.
    fn piste_enchainee(&mut self);
}

/// Pourquoi la chaîne s'est arrêtée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinDeChaine {
    /// Arrêt demandé, ou silence forcé.
    Interrompue,
    /// Erreur de lecture : traitée comme une fin de flux (comme avant), mais
    /// on n'enchaîne pas derrière une piste peut-être tronquée.
    LectureEchouee,
    /// Fin de flux, rien en réserve.
    RienEnReserve,
    /// Une suivante était en réserve mais n'a pas pu être ouverte.
    SuivanteRefusee,
    /// La suivante a un autre format : il faut rouvrir le périphérique.
    FormatDifferent,
}

/// Le bilan de la chaîne, pour la fin du bras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IssueDeLaChaine {
    pub(super) fin: FinDeChaine,
    /// La DERNIÈRE piste a-t-elle atteint sa fin de flux ? C'est ce qui
    /// décide du signal de fin naturelle.
    pub(super) http_eof: bool,
    /// Trames source de la dernière piste (sa position).
    pub(super) trames: u64,
    /// Nombre de pistes enchaînées sans refermer le flux.
    pub(super) pistes_enchainees: u32,
}

enum LectureDePiste {
    FinDeFlux,
    Interrompue,
    Erreur,
}

/// Décode `octets`, pousse, publie le verdict ; rend les trames consommées.
fn pousser<H: HoteDeChaineNative>(
    hote: &mut H,
    etage: &mut EtageNatif<'_>,
    puits: &mut dyn PuitsNatif,
    octets: &[u8],
) -> u64 {
    // Un puits mort est traité comme une poussée : le bras ignorait déjà ce
    // verdict (« famine muette » de la carte §1.2, conservée telle quelle).
    match etage.decoder_et_pousser(octets, puits) {
        EcritureNative::Poussee {
            trames_source,
            dop,
            bit_perfect,
        }
        | EcritureNative::PuitsMort {
            trames_source,
            dop,
            bit_perfect,
        } => {
            hote.publier_le_verdict(dop, bit_perfect);
            trames_source
        }
        EcritureNative::RienAPousser => 0,
    }
}

/// La boucle de lecture d'UNE piste, telle que le bras WASAPI l'écrivait.
fn lire_la_piste<H: HoteDeChaineNative>(
    hote: &mut H,
    lecteur: &mut H::Lecteur,
    tampon: &mut [u8],
    etage: &mut EtageNatif<'_>,
    puits: &mut dyn PuitsNatif,
    trames: &mut u64,
    seek_offset: u64,
) -> LectureDePiste {
    let cadence = u64::from(etage.spec().cadence()).max(1);
    loop {
        if hote.arret_recu() {
            return LectureDePiste::Interrompue;
        }
        if hote.silence_force() {
            debug!("local_audio_wasapi_exclusive_aborted_by_stop");
            return LectureDePiste::Interrompue;
        }
        match lecteur.read(tampon) {
            Ok(0) => return LectureDePiste::FinDeFlux,
            Ok(n) => {
                *trames += pousser(hote, etage, puits, &tampon[..n]);
                let position = (*trames as f64 / cadence as f64 * 1000.0) as u64 + seek_offset;
                hote.publier_la_position(position);
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => {
                warn!(error = %e, "local_audio_wasapi_exclusive_read_error");
                return LectureDePiste::Erreur;
            }
        }
    }
}

/// Joue la piste courante jusqu'à sa fin de flux, puis enchaîne tant qu'une
/// suivante au même format est en réserve.
///
/// `trames` : ce que l'amorce (octets lus avec l'en-tête) a déjà poussé pour
/// la première piste. Ni la queue du DSP, ni le signal de fin naturelle, ni le
/// vidage ne sont faits ici : ils appartiennent à la FIN de la chaîne, et le
/// bras les fait au retour.
pub(super) fn jouer_la_chaine_native<H: HoteDeChaineNative>(
    hote: &mut H,
    premier: H::Lecteur,
    etage: &mut EtageNatif<'_>,
    puits: &mut dyn PuitsNatif,
    trames: u64,
    seek_offset: u64,
) -> IssueDeLaChaine {
    let mut lecteur = premier;
    let mut tampon = vec![0u8; 65536];
    let mut trames = trames;
    let mut seek_offset = seek_offset;
    let mut pistes_enchainees = 0u32;
    let issue = |fin, http_eof, trames, pistes_enchainees| IssueDeLaChaine {
        fin,
        http_eof,
        trames,
        pistes_enchainees,
    };
    loop {
        match lire_la_piste(
            hote,
            &mut lecteur,
            &mut tampon,
            etage,
            puits,
            &mut trames,
            seek_offset,
        ) {
            LectureDePiste::Interrompue => {
                return issue(FinDeChaine::Interrompue, false, trames, pistes_enchainees);
            }
            LectureDePiste::Erreur => {
                forcer_le_reliquat(etage, puits, &mut trames);
                return issue(FinDeChaine::LectureEchouee, true, trames, pistes_enchainees);
            }
            LectureDePiste::FinDeFlux => forcer_le_reliquat(etage, puits, &mut trames),
        }

        if hote.silence_force() {
            return issue(FinDeChaine::Interrompue, true, trames, pistes_enchainees);
        }
        let (suivant, entete) = match hote.preparer_la_suivante() {
            Suivante::Aucune => {
                return issue(FinDeChaine::RienEnReserve, true, trames, pistes_enchainees);
            }
            Suivante::Refusee => {
                return issue(
                    FinDeChaine::SuivanteRefusee,
                    true,
                    trames,
                    pistes_enchainees,
                );
            }
            Suivante::Prete { lecteur, entete } => (lecteur, entete),
        };
        if etage.enchainer_la_piste(entete.spec).is_err() {
            info!(
                prev_sr = etage.spec().cadence(),
                prev_bd = etage.spec().profondeur().bits_declares(),
                prev_ch = etage.spec().canaux(),
                new_sr = entete.spec.cadence(),
                new_bd = entete.spec.profondeur().bits_declares(),
                new_ch = entete.spec.canaux(),
                "local_audio_exclusive_gapless_format_change_reopen"
            );
            return issue(
                FinDeChaine::FormatDifferent,
                true,
                trames,
                pistes_enchainees,
            );
        }
        hote.piste_enchainee();
        pistes_enchainees += 1;
        seek_offset = 0;
        trames = pousser(hote, etage, puits, entete.amorce());
        lecteur = suivant;
        info!(
            sample_rate = entete.spec.cadence(),
            bit_depth = entete.spec.profondeur().bits_declares(),
            channels = entete.spec.canaux(),
            "local_audio_exclusive_gapless_chained"
        );
    }
}

/// Moins de 32 trames 24 bits initiales ne se classent pas, mais l'anneau
/// entier peut les porter : elles partent brutes et à l'unité plutôt que
/// devinées PCM (ce que le bras faisait à l'EOF).
fn forcer_le_reliquat(etage: &mut EtageNatif<'_>, puits: &mut dyn PuitsNatif, trames: &mut u64) {
    if let Some(reliquat) = etage.vider(puits) {
        *trames += reliquat.trames;
        info!(
            backend = "WASAPI",
            bytes = reliquat.octets,
            "windows_exclusive_short_24bit_stream_forced_raw"
        );
    }
}
