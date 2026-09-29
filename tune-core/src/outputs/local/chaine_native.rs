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

/// La réserve de la piste suivante, vue par une chaîne : ce qu'il faut pour
/// décider d'un enchaînement, quel que soit le bras qui lit les pistes.
///
/// Séparée de [`HoteDeChaineNative`] pour le bras ASIO (#5204), qui lit ses
/// pistes par la boucle commune (`BoucleProducteur::tourner`) : il n'emprunte
/// à la chaîne que la réserve et la frontière, pas la lecture.
pub(super) trait ReserveDeLaChaine {
    type Lecteur: Read;
    /// Le silence forcé par `stop()` / un `play_url` plus récent.
    fn silence_force(&self) -> bool;
    /// Retire la suivante de sa réserve, ouvre son flux, lit son en-tête.
    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur>;
    /// L'enchaînement est acquis : publier le morceau suivant (adresse, titre,
    /// durée) et remettre la position à zéro. Le sondeur y lit la chute de
    /// position d'une transition interne et avance la file sans `play`.
    fn piste_enchainee(&mut self);
}

/// Ce que la chaîne emprunte au bras WASAPI : la réserve, plus les témoins
/// d'arrêt et la publication vers la zone que sa boucle de lecture consulte.
pub(super) trait HoteDeChaineNative: ReserveDeLaChaine {
    /// Un ordre d'arrêt est-il arrivé ? (consomme le message, comme le bras.)
    fn arret_recu(&mut self) -> bool;
    /// Après chaque poussée : l'état DoP, le volume qui le suit, le verdict
    /// bit-perfect du chemin de signal.
    fn publier_le_verdict(&mut self, dop: bool, bit_perfect: bool);
    fn publier_la_position(&mut self, position_ms: u64);
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

        let (suivant, entete) = match accepter_la_suivante(hote, etage) {
            Ok(acceptee) => acceptee,
            Err(fin) => return issue(fin, true, trames, pistes_enchainees),
        };
        pistes_enchainees += 1;
        seek_offset = 0;
        trames = pousser(hote, etage, puits, entete.amorce());
        lecteur = suivant;
    }
}

/// LA frontière d'une piste enchaînée sur un transport natif exclusif, commune
/// aux bras WASAPI et ASIO : la piste courante vient d'atteindre sa fin de
/// flux (son reliquat est déjà parti).
///
/// `Ok` : la suivante est acquise — son flux est ouvert, son en-tête lu, son
/// format est celui du flux ouvert ([`EtageNatif::enchainer_la_piste`]), et
/// elle est publiée à la zone. Il reste à l'appelant à pousser son amorce et
/// à lire son flux dans le MÊME puits.
///
/// `Err` : on n'enchaîne pas, et la raison. La piste courante se termine ; la
/// fin naturelle prend le relais (et rouvre au nouveau format s'il change).
pub(super) fn accepter_la_suivante<R: ReserveDeLaChaine>(
    reserve: &mut R,
    etage: &mut EtageNatif<'_>,
) -> Result<(R::Lecteur, EnteteEnchainee), FinDeChaine> {
    if reserve.silence_force() {
        return Err(FinDeChaine::Interrompue);
    }
    let (suivant, entete) = match reserve.preparer_la_suivante() {
        Suivante::Aucune => return Err(FinDeChaine::RienEnReserve),
        Suivante::Refusee => return Err(FinDeChaine::SuivanteRefusee),
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
        return Err(FinDeChaine::FormatDifferent);
    }
    reserve.piste_enchainee();
    info!(
        sample_rate = entete.spec.cadence(),
        bit_depth = entete.spec.profondeur().bits_declares(),
        channels = entete.spec.canaux(),
        "local_audio_exclusive_gapless_chained"
    );
    Ok((suivant, entete))
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

/// La réserve réelle des bras exclusifs Windows (WASAPI, ASIO) : la piste
/// que `set_next_media` a préparée, son flux HTTP, et ce qu'il faut publier à
/// la zone quand elle est enchaînée. Un seul exemplaire pour les deux bras
/// (#5204) : c'était le corps de `HoteWasapi`.
#[cfg(target_os = "windows")]
pub(super) struct ReserveHttp<'a> {
    pub(super) force_silent: &'a Arc<AtomicBool>,
    pub(super) position_ms: &'a AtomicU64,
    pub(super) next_media: &'a std::sync::Mutex<Option<PendingNextMedia>>,
    /// La suivante retirée de la réserve, en attente de la décision de format.
    pub(super) en_cours: Option<PendingNextMedia>,
    pub(super) current_uri: &'a std::sync::Mutex<Option<String>>,
    pub(super) track_title: &'a std::sync::Mutex<Option<String>>,
    pub(super) track_artist: &'a std::sync::Mutex<Option<String>>,
    pub(super) duration_ms: &'a AtomicU64,
    pub(super) seek_offset_ms: &'a AtomicU64,
    pub(super) track_ended_naturally: &'a AtomicBool,
    pub(super) track_ended_generation: &'a AtomicU64,
    pub(super) dop_active: &'a AtomicBool,
    pub(super) volume: &'a Arc<AtomicU32>,
    pub(super) user_volume: &'a Arc<AtomicU32>,
    pub(super) rg_factor: &'a Arc<AtomicU32>,
}

#[cfg(target_os = "windows")]
impl ReserveDeLaChaine for ReserveHttp<'_> {
    type Lecteur = super::LecteurHttpAnnulable;

    fn silence_force(&self) -> bool {
        self.force_silent.load(Ordering::Relaxed)
    }

    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur> {
        use super::enchainement_exclusif::{lire_l_entete_enchainee, ouvrir_la_piste_suivante};
        let Some(suivante) = self.next_media.lock().unwrap().take() else {
            return Suivante::Aucune;
        };
        info!(
            next_title = ?suivante.title,
            next_url = %suivante.url,
            "local_audio_gapless_chaining_next_track"
        );
        let Some(mut lecteur) = ouvrir_la_piste_suivante(&suivante.url, self.force_silent) else {
            return Suivante::Refusee;
        };
        match lire_l_entete_enchainee(&mut lecteur, self.force_silent) {
            Ok(entete) => {
                self.en_cours = Some(suivante);
                Suivante::Prete { lecteur, entete }
            }
            Err(_) => Suivante::Refusee,
        }
    }

    /// Même bascule que le chemin partagé, une fois l'enchaînement acquis :
    /// le morceau suivant est publié, la position repart de zéro — le sondeur
    /// y lit une transition interne et avance la file sans `play`. La
    /// décision DoP de la nouvelle piste repart de zéro, comme en début de
    /// piste : elle n'hérite pas de l'état DoP/volume de la précédente.
    fn piste_enchainee(&mut self) {
        let Some(suivante) = self.en_cours.take() else {
            return;
        };
        self.track_ended_naturally.store(false, Ordering::SeqCst);
        self.track_ended_generation.store(0, Ordering::SeqCst);
        *self.current_uri.lock().unwrap() = Some(suivante.url);
        *self.track_title.lock().unwrap() = suivante.title;
        *self.track_artist.lock().unwrap() = suivante.artist;
        if let Some(duree) = suivante.duration_ms {
            self.duration_ms.store(duree, Ordering::SeqCst);
        }
        self.seek_offset_ms.store(0, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
        if self.dop_active.swap(false, Ordering::SeqCst) {
            sync_volume_to_dop(self.volume, self.user_volume, self.rg_factor, false);
        }
    }
}
