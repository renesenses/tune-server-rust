//! #8082 — « Flux continu entre les pistes » pour les renderers DLNA qui
//! raccordent mal deux pistes (Eversolo DMP-A8, suite de #8076).
//!
//! Réglage par zone DLNA, désactivé par défaut
//! (`outputs::dlna_flux_continu::cle`). Quand il est armé et qu'une piste de
//! la bibliothèque partirait TELLE QUELLE (passthrough : aucun traitement,
//! PURE respecté), les pistes suivantes de la file qui ont la même cadence, la
//! même profondeur et le même nombre de canaux partent avec elle dans UN seul
//! WAV (`audio::wav_continu`). Au premier changement de format, la suite
//! s'arrête : la piste qui suit reprend le chemin normal (`SetNext`).
//!
//! #8968 — une tranche CUE d'une image sans perte y entre aussi : ses bornes
//! sont converties à l'échantillon près (`audio::bornes_cue`), et la tranche
//! n'a pas besoin du passthrough (une tranche se transcode toujours) : il
//! suffit que la tranche soit le SEUL motif de transcodage (aucun
//! traitement), voir `resolve_local`.
//!
//! Le suivi de piste (titre, pochette, historique) passe par le plan posé sur
//! l'appareil (`outputs::dlna_flux_continu`) et par
//! [`PlaybackOrchestrator::avancer_dans_le_flux_continu`]. Un déplacement dans
//! la piste relance le flux à la position (`deplacer_la_sortie`) ; suivant et
//! précédent passent par `play_from_queue`, qui pose un flux neuf depuis la
//! piste visée. Le volume n'est pas touché par ce chemin.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{info, warn};

use super::PlaybackOrchestrator;
use crate::audio::formats::AudioFormat;
use crate::audio::wav_continu::{self, FormatPcm, Segment, WavContinu};
use crate::outputs::dlna_flux_continu::{self as registre, PisteDuFlux, Plan};
use crate::playback::RepeatMode;

/// Au plus tant de pistes dans un même flux (la sonde ouvre chaque fichier).
pub(crate) const PISTES_MAX: usize = 50;

/// Un flux continu prêt à servir.
pub(crate) struct FluxContinuPrepare {
    pub wav: WavContinu,
    /// Position de file et piste de chaque segment, dans l'ordre.
    pub lignes: Vec<(i64, i64)>,
    /// Trames de la PREMIÈRE piste avant le départ (position de relance).
    pub trames_avant_depart: u64,
}

/// Les bornes d'une tranche CUE telles que la base les range : début et fin
/// en millisecondes dans le fichier image (`cue_start_ms`, `cue_end_ms`),
/// converties à l'échantillon près par `audio::bornes_cue` (#8968).
pub(crate) type TrancheMs = (u64, Option<u64>);

/// Une piste admise dans le flux continu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Admise {
    /// Le fichier sur disque (l'image, pour une tranche CUE).
    pub chemin: String,
    /// La tranche à servir, pour une piste de feuille CUE.
    pub tranche: Option<TrancheMs>,
}

/// Une piste admise : lossless entier, fichier présent, ni image ISO ni
/// image CDDA. #8968 — une tranche CUE d'une image sans perte est admise :
/// ses bornes se convertissent à l'échantillon près, et deux tranches
/// contiguës du même fichier se servent alors sans le moindre raccord.
pub(crate) fn chemin_admissible(
    db: &Arc<dyn crate::db::backend::DbBackend>,
    track_id: i64,
) -> Option<Admise> {
    let mut piste = crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .get(track_id)
        .ok()
        .flatten()?;
    crate::library::exemplaires::appliquer_a_la_lecture(&**db, &mut piste);
    let bornes = piste.bornes_cue();
    let format = piste
        .format
        .as_deref()
        .and_then(AudioFormat::from_extension);
    if !matches!(
        format,
        Some(
            AudioFormat::Flac
                | AudioFormat::Wav
                | AudioFormat::Aiff
                | AudioFormat::Alac
                | AudioFormat::M4a
        )
    ) {
        return None;
    }
    let stocke = match (piste.file_path.as_deref(), &bornes) {
        (Some(p), _) => p.to_string(),
        (None, Some((media, _, _))) => media.clone(),
        (None, None) => return None,
    };
    let chemin = crate::library::local_path::resolve_existing_local_path(&stocke)?;
    if crate::audio::iso9660::est_chemin_virtuel(&chemin)
        || crate::audio::image_cdda::est_image_cdda(Path::new(&chemin))
    {
        return None;
    }
    Some(Admise {
        chemin,
        tranche: bornes.map(|(_, debut, fin)| (debut, fin)),
    })
}

/// La portion `[début, fin)` du fichier, en trames, qu'une piste sert : le
/// fichier entier, ou la tranche CUE à l'échantillon près.
fn portion(
    sonde: &wav_continu::PisteSondee,
    tranche: Option<TrancheMs>,
) -> Result<(u64, u64), String> {
    let Some((debut_ms, fin_ms)) = tranche else {
        return Ok((0, sonde.trames));
    };
    let (debut, fin) =
        crate::audio::bornes_cue::tranche_en_echantillons(debut_ms, fin_ms, sonde.format.cadence);
    let fin = fin.unwrap_or(sonde.trames).min(sonde.trames);
    if debut >= fin {
        return Err(format!(
            "tranche CUE vide ou hors du fichier ({debut}..{fin} sur {} trames)",
            sonde.trames
        ));
    }
    Ok((debut, fin))
}

/// [`assembler_avec_tranches`] pour des fichiers entiers.
#[cfg(test)]
pub(crate) fn assembler(
    premiere: (i64, i64, PathBuf),
    depart_ms: u64,
    suivantes: Vec<(i64, i64, PathBuf)>,
) -> Result<FluxContinuPrepare, String> {
    assembler_avec_tranches(
        (premiere.0, premiere.1, premiere.2, None),
        depart_ms,
        suivantes
            .into_iter()
            .map(|(p, t, c)| (p, t, c, None))
            .collect(),
    )
}

/// Assemble la suite à partir de la piste en cours (déjà sondée) et des
/// candidates suivantes `(position, track_id, chemin, tranche CUE)`, dans
/// l'ordre de la file. S'arrête au premier format différent, à la première
/// illisible, ou au plafond RIFF. Bloquant (sondes de fichiers).
///
/// `depart_ms` se compte depuis le début de la PREMIÈRE PISTE (de sa tranche,
/// pour une piste CUE).
pub(crate) fn assembler_avec_tranches(
    premiere: (i64, i64, PathBuf, Option<TrancheMs>),
    depart_ms: u64,
    suivantes: Vec<(i64, i64, PathBuf, Option<TrancheMs>)>,
) -> Result<FluxContinuPrepare, String> {
    let sonde = wav_continu::sonder(&premiere.2)?;
    let format: FormatPcm = sonde.format;
    let (debut, fin) = portion(&sonde, premiere.3)?;
    let longueur = fin - debut;
    let depart = (depart_ms.saturating_mul(u64::from(format.cadence)) / 1000)
        .min(longueur.saturating_sub(1));
    let max = WavContinu::trames_max(&format);
    let mut cumul = longueur - depart;
    let mut segments = vec![Segment {
        fichier: premiere.2,
        depart: debut + depart,
        trames: longueur - depart,
    }];
    let mut lignes = vec![(premiere.0, premiere.1)];
    for (position, track_id, chemin, tranche) in suivantes {
        if segments.len() >= PISTES_MAX {
            break;
        }
        let s = match wav_continu::sonder(&chemin) {
            Ok(s) => s,
            Err(motif) => {
                info!(
                    position,
                    track_id, motif, "flux_continu_suite_arretee_piste_non_admise"
                );
                break;
            }
        };
        if s.format != format {
            info!(
                position,
                track_id,
                cadence = s.format.cadence,
                profondeur = s.format.profondeur,
                canaux = s.format.canaux,
                "flux_continu_suite_arretee_changement_de_format"
            );
            break;
        }
        let (debut, fin) = match portion(&s, tranche) {
            Ok(p) => p,
            Err(motif) => {
                info!(
                    position,
                    track_id, motif, "flux_continu_suite_arretee_piste_non_admise"
                );
                break;
            }
        };
        let trames = fin - debut;
        if cumul.saturating_add(trames) > max {
            info!(position, "flux_continu_suite_arretee_plafond_riff");
            break;
        }
        cumul += trames;
        segments.push(Segment {
            fichier: chemin,
            depart: debut,
            trames,
        });
        lignes.push((position, track_id));
    }
    Ok(FluxContinuPrepare {
        wav: WavContinu::nouveau(format, segments)?,
        lignes,
        trames_avant_depart: depart,
    })
}

/// Le plan de suivi : chaque frontière comptée depuis le DÉBUT de la première
/// piste (le départ éventuel est rapporté au relevé par `dlna_depart_natif`).
pub(crate) fn plan_du_flux(zone_id: i64, session_id: &str, flux: &FluxContinuPrepare) -> Plan {
    let cadence = u64::from(flux.wav.format().cadence);
    let ms = |trames: u64| (trames.saturating_mul(1000) + cadence / 2) / cadence;
    let mut debut = 0u64;
    let mut pistes = Vec::with_capacity(flux.lignes.len());
    for (i, (seg, (position, track_id))) in flux.wav.segments().iter().zip(&flux.lignes).enumerate()
    {
        let trames = if i == 0 {
            seg.trames + flux.trames_avant_depart
        } else {
            seg.trames
        };
        pistes.push(PisteDuFlux {
            position: *position,
            track_id: *track_id,
            debut_ms: ms(debut),
            duree_ms: ms(debut + trames) - ms(debut),
        });
        debut += trames;
    }
    Plan {
        zone_id,
        session_id: session_id.to_string(),
        pistes,
    }
}

impl PlaybackOrchestrator {
    /// La suite à servir en flux continu pour cette lecture, ou `None` (chemin
    /// normal) : réglage éteint, sortie non DLNA, file aléatoire ou en
    /// répétition d'une piste, piste hors de la file, moins de deux pistes.
    pub(crate) async fn preparer_le_flux_continu(
        &self,
        zone_id: i64,
        output_type: Option<&str>,
        track_id: Option<i64>,
        file_path: &str,
        seek_ms: Option<u64>,
    ) -> Option<FluxContinuPrepare> {
        if output_type != Some("dlna") || !registre::active(&self.db, zone_id) {
            return None;
        }
        let track_id = track_id?;
        // Gardes parentales (#8088, #7974) : une piste DANS le flux est jouée
        // sans repasser par elles. Profil qui masque l'explicite, ou bail
        // d'écoute borné : pas de flux continu, chemin normal. #8611 : de
        // même pour un profil qui bloque des artistes.
        if self.garde_parentale_par_piste(zone_id).await
            || !self
                .bail_couvre_l_enchainement(zone_id, std::time::Duration::from_secs(86_400))
                .await
        {
            info!(zone_id, "flux_continu_ecarte_garde_parentale");
            return None;
        }
        let etat = self.playback.get_state(zone_id).await;
        if etat.shuffle || etat.repeat == RepeatMode::One {
            info!(zone_id, "flux_continu_ecarte_aleatoire_ou_repetition");
            return None;
        }
        let file = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
        let p = etat.queue_position;
        let courante = file.get_at(zone_id, p).ok().flatten()?;
        if courante.track_id != Some(track_id) {
            return None;
        }
        let admise = chemin_admissible(&self.db, track_id)?;
        let mut suivantes = Vec::new();
        for k in 1..PISTES_MAX as i64 {
            let pos = p + k;
            // Une ligne bannie pour le profil : la suite s'arrête, la file
            // l'enjambera par le chemin normal.
            if super::lignes_bannies_a_enjamber(&self.db, zone_id, pos, etat.session_profile_id)
                .0
                .position_depuis(pos)
                != Some(pos)
            {
                break;
            }
            let Some(ligne) = file.get_at(zone_id, pos).ok().flatten() else {
                break;
            };
            let Some(tid) = ligne.track_id else {
                break;
            };
            if ligne
                .source
                .as_deref()
                .is_some_and(|s| !s.is_empty() && s != "local")
            {
                break;
            }
            let Some(suivante) = chemin_admissible(&self.db, tid) else {
                break;
            };
            suivantes.push((pos, tid, PathBuf::from(suivante.chemin), suivante.tranche));
        }
        if suivantes.is_empty() {
            return None;
        }
        let premiere = (p, track_id, PathBuf::from(file_path), admise.tranche);
        let depart_ms = seek_ms.unwrap_or(0);
        let prepare = tokio::task::spawn_blocking(move || {
            assembler_avec_tranches(premiere, depart_ms, suivantes)
        })
        .await
        .ok()?;
        match prepare {
            Ok(f) if f.lignes.len() >= 2 => Some(f),
            Ok(_) => {
                info!(
                    zone_id,
                    "flux_continu_ecarte_piste_suivante_de_format_different"
                );
                None
            }
            Err(motif) => {
                info!(zone_id, motif, "flux_continu_ecarte_piste_non_admise");
                None
            }
        }
    }

    /// Le renderer est entré dans la piste `piste` du flux continu : la file
    /// avance sans rien envoyer au renderer. La zone garde le MÊME flux : il
    /// est rangé comme flux « pré-armé » pour que l'avance l'adopte
    /// (`advance_queue_metadata`, #3442) au lieu de poser un flux absent.
    pub(crate) async fn avancer_dans_le_flux_continu(
        &self,
        zone_id: i64,
        device_id: &str,
        piste: &PisteDuFlux,
    ) -> Result<(), String> {
        let ligne = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, piste.position)
            .ok()
            .flatten();
        if ligne.and_then(|l| l.track_id) != Some(piste.track_id) {
            // La file a changé sous le flux : il ne dit plus la vérité. On le
            // quitte et on rejoue la file depuis cette ligne.
            warn!(
                zone_id,
                position = piste.position,
                track_id = piste.track_id,
                "flux_continu_file_modifiee_relance"
            );
            registre::oublier(device_id);
            return self
                .play_from_queue(zone_id, piste.position)
                .await
                .map(|_| ());
        }
        let flux = self
            .playback
            .get_state(zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id);
        if let Some(sid) = flux {
            self.gapless_sessions.lock().await.insert(zone_id, sid);
        }
        self.advance_queue_metadata(zone_id, piste.position).await
    }
}

#[cfg(test)]
#[path = "flux_continu_8082_tests.rs"]
mod tests;
