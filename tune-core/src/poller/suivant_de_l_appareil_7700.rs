//! #7700 — un « suivant » pressé SUR L'APPAREIL enchaîne la file de Tune
//! en quelques secondes.
//!
//! Constat (Eversolo DMP-A6 en DLNA, rc9, Linux) : un appui sur « suivant »
//! à la télécommande de l'A6 ne joue rien, puis le son revient plusieurs
//! dizaines de secondes plus tard. Le « suivant » de Tune n'a pas ce défaut.
//!
//! ## La cause
//!
//! Un renderer DLNA n'envoie aucune action « Next » à Tune : Tune ne voit que
//! ce que l'appareil rapporte au sondage. L'A6 n'a pas de suivante à jouer
//! (la `NextURI` n'est armée que dans les 30 dernières secondes, et il ne
//! l'exécute pas de lui-même, #4382) : il quitte notre flux et passe
//! `Stopped`, la position retombe. En milieu de piste, `played_enough` est
//! faux ; le bras `Stopped` du sondeur attendait donc 30 tours ET 30 s
//! (`STOPPED_FAILURE_THRESHOLD`, `STOPPED_FAILURE_MIN_SECS`), puis prenait
//! l'arrêt pour un renderer qui cale et relançait LA MÊME piste à la position
//! atteinte (#4645). D'où le silence, puis le son bien plus tard.
//!
//! ## Le remède
//!
//! Sur une sortie DLNA, zone en lecture, hors des grâces (déplacement,
//! chargement, gapless) que le bras a déjà écartées, l'arrêt est lu comme un
//! départ voulu de l'appareil quand, pendant [`TOURS_D_ARRET_DU_SUIVANT`]
//! tours d'affilée :
//!
//! - la piste a réellement joué (pic d'au moins [`PIC_MINIMAL_MS`]) sans
//!   approcher sa fin (`played_enough` faux : la fin normale garde son
//!   chemin) ;
//! - l'appareil a lâché notre piste : sa position est retombée loin sous le
//!   pic, ou il rapporte une URI vide ; et il ne joue pas le flux d'un tiers.
//!   Une URI absente ne compte pas : beaucoup de renderers n'en rapportent
//!   jamais ;
//! - il n'était PAS à court d'audio : il avait reçu au moins
//!   [`AVANCE_MINIMALE_MS`] d'audio devant le pic. C'est ce qui le distingue
//!   d'un renderer qui cale sur un flux incomplet (#4645, #2394), qui garde
//!   sa reprise à la position atteinte ;
//! - et il ne tenait pas déjà le fichier EN ENTIER, sauf s'il a vidé son URI.
//!   Un renderer qui a tout reçu peut annoncer `Stopped` à la position 0
//!   pendant qu'il joue encore son tampon (DarTZeel LHC-208, #4661) : ce cas
//!   garde l'attente à l'horloge de la piste.
//!
//! La piste est alors déclarée finie (`track_end_gap`, motif
//! `dlna_device_skipped_track`) et la file avance comme après une fin
//! normale. Une ligne `dlna_suivant_de_l_appareil` dit ce que l'appareil
//! rapportait : le premier journal d'un vrai A6 dira lequel de ces signaux il
//! émet.
//!
//! Hors périmètre : un « arrêt » à la télécommande rapporte la même chose
//! qu'un « suivant » sur un renderer qui ne les distingue pas ; il fait alors
//! avancer la file (choix de la demande, à confirmer sur l'appareil réel).

use super::*;

/// Tours `Stopped` d'affilée avant de lire l'arrêt comme un départ de
/// l'appareil. Deux, pour qu'un sondage isolé (réponse lente, transition
/// annoncée en deux temps) n'avance jamais la file.
pub(super) const TOURS_D_ARRET_DU_SUIVANT: u8 = 2;

/// La piste a réellement joué : en deçà, c'est un démarrage qui n'a pas pris
/// (#2394, #5522), pas un geste sur l'appareil.
pub(super) const PIC_MINIMAL_MS: u64 = 10_000;

/// La position rapportée est retombée d'au moins autant sous le pic :
/// l'appareil ne tient plus la position de notre piste.
pub(super) const CHUTE_DE_POSITION_MS: u64 = 5_000;

/// Audio servie devant le pic au-delà de laquelle le renderer n'était pas à
/// sec : il s'est arrêté avec de la musique en main.
pub(super) const AVANCE_MINIMALE_MS: u64 = 5_000;

/// Ce que l'appareil rapporte de ce tour `Stopped`, réduit à ce qui décide.
#[derive(Debug, Clone, Copy)]
pub(super) struct ArretRapporte<'a> {
    pub output_type: &'a str,
    pub realtime: bool,
    pub zone_en_lecture: bool,
    pub played_enough: bool,
    pub duree_ms: u64,
    /// Compte d'arrêts APRÈS ce tour.
    pub tours: u8,
    pub pic_ms: u64,
    pub position_ms: u64,
    pub tenue: &'a decisions::TenueDuRenderer,
}

/// L'appareil a-t-il quitté notre piste en milieu de lecture ? Prédicat pur,
/// sans mesure du flux : il écarte à bas prix tout ce qui n'est pas le cas.
pub(super) fn depart_de_l_appareil(a: &ArretRapporte<'_>) -> bool {
    let notre_piste_lachee = a.position_ms.saturating_add(CHUTE_DE_POSITION_MS) <= a.pic_ms
        || *a.tenue == decisions::TenueDuRenderer::LecteurInterne;
    let pas_un_tiers = matches!(
        a.tenue,
        decisions::TenueDuRenderer::LeNotre | decisions::TenueDuRenderer::LecteurInterne
    );
    a.output_type == "dlna"
        && a.realtime
        && a.zone_en_lecture
        && !a.played_enough
        && a.duree_ms > 0
        && a.tours >= TOURS_D_ARRET_DU_SUIVANT
        && a.pic_ms >= PIC_MINIMAL_MS
        && pas_un_tiers
        && notre_piste_lachee
}

/// L'arrêt est-il un départ de l'appareil, d'après le flux servi ?
///
/// - URI vidée : l'appareil a lâché notre flux ; il suffit qu'il n'ait pas
///   été à sec (de l'avance, ou le fichier entier) ;
/// - notre URI toujours rapportée : il faut de l'avance ET un fichier pas
///   encore servi en entier. Servi en entier, un `Stopped` à 0 peut être un
///   renderer qui joue son tampon (#4661).
///
/// `false` dès que la mesure manque : sans elle, on ne distingue pas un
/// renderer à sec, et sa reprise à la position (#4645) reste le chemin.
pub(super) fn le_renderer_a_lache_un_flux_qu_il_avait_en_main(
    audio_servi_ms: Option<u64>,
    pic_ms: u64,
    octets_servis: Option<u64>,
    octets_total: Option<u64>,
    uri_videe: bool,
) -> bool {
    let avance = audio_servi_ms.is_some_and(|a| a >= pic_ms.saturating_add(AVANCE_MINIMALE_MS));
    let en_entier = matches!(
        (octets_servis, octets_total),
        (Some(servis), Some(total)) if total > 0 && servis >= total
    );
    if uri_videe {
        avance || en_entier
    } else {
        avance && !en_entier
    }
}

impl PositionPoller {
    /// La mesure du flux pour
    /// [`le_renderer_a_lache_un_flux_qu_il_avait_en_main`] : audio
    /// servie, octets servis, taille du flux. Payée seulement quand
    /// [`depart_de_l_appareil`] est déjà vrai.
    pub(super) async fn reserve_du_renderer_7700(
        &self,
        stream_id: Option<&str>,
        pic_ms: u64,
        uri_videe: bool,
    ) -> (bool, Option<u64>, Option<u64>, Option<u64>) {
        let Some(sid) = stream_id else {
            return (false, None, None, None);
        };
        let audio = self.orchestrator.streamer_audio_servi_ms(sid).await;
        let servis = self.orchestrator.streamer_bytes_sent(sid).await;
        let total = self.orchestrator.streamer_total_bytes(sid).await;
        (
            le_renderer_a_lache_un_flux_qu_il_avait_en_main(
                audio, pic_ms, servis, total, uri_videe,
            ),
            audio,
            servis,
            total,
        )
    }
}
