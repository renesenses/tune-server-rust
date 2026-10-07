//! Sortie Sendspin : une enceinte `player@v1` connectée à Tune (#3326, S2-c).
//!
//! À la différence de DLNA ou de Chromecast, l'enceinte ne va pas chercher
//! une URL : c'est Tune qui POUSSE l'audio, horodaté dans son horloge, sur la
//! connexion WebSocket chiffrée que l'enceinte a ouverte. La sortie ne parle
//! donc pas au réseau elle-même : elle passe des [`OrdreLecteur`] au pilote de
//! la connexion par une [`LiaisonLecteur`], et la [`SessionLecteur`] décide
//! des messages (voir `sendspin::lecteur`).
//!
//! Première version : PCM seul, décodage progressif du fichier (même chemin
//! que la sortie locale), morceaux de 50 ms, avance de départ et comptabilité
//! du tampon selon `roles/player/v1.md`.
//!
//! « Pause » n'existe pas dans Sendspin. Choix de cette version, à confirmer
//! (question écrite dans la PR) : pause = `stream/end` et groupe `stopped`
//! (l'activité `playback` reste déclarée) ; reprise = nouveau `stream/start`
//! depuis la position atteinte. Seek et saut de piste = `stream/clear`, comme
//! la spécification le demande.
//!
//! [`SessionLecteur`]: crate::sendspin::lecteur::SessionLecteur

use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::sendspin::horloge::maintenant_us;
use crate::sendspin::lecteur::{
    ComptabiliteTampon, FormatAudio, LiaisonLecteur, OrdreLecteur, TAILLE_ENTETE_AUDIO,
    avance_de_depart_us, choisir_format, duree_us,
};

/// `output_type` des zones Sendspin.
pub const TYPE_DE_SORTIE: &str = "sendspin";

/// Durée visée d'un morceau : dans la fourchette 15-150 ms de la spécification.
pub const DUREE_MORCEAU_US: i64 = 50_000;

/// Jusqu'où, au-delà de l'avance de départ, Tune envoie en avance. Borné en
/// plus par `buffer_capacity`.
pub const HORIZON_US: i64 = 1_500_000;

/// L'identifiant de sortie d'une enceinte : son `client_id` (clé publique,
/// prouvée par la poignée de main), seule identité durable du protocole.
#[must_use]
pub fn identifiant_de_sortie(client_id: &str) -> String {
    format!("sendspin:{client_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Arret,
    Lecture,
    Pause,
}

#[derive(Debug, Default)]
struct Diffusion {
    phase: Phase,
    /// Position (ms) du premier morceau de la diffusion en cours.
    debut_ms: u64,
    /// Horodatage serveur du premier morceau.
    t0_us: Option<i64>,
    /// Horodatage serveur de la fin du dernier morceau, une fois tout envoyé.
    fin_us: Option<i64>,
    position_pause_ms: u64,
    format: Option<FormatAudio>,
    flux_ouvert: bool,
    erreur: Option<String>,
}

#[derive(Debug, Clone)]
struct Media {
    source: String,
    uri: String,
    titre: Option<String>,
    artiste: Option<String>,
    duree_ms: u64,
}

pub struct SendspinOutput {
    nom: String,
    device_id: String,
    liaison: LiaisonLecteur,
    diffusion: Arc<Mutex<Diffusion>>,
    tache: AsyncMutex<Option<JoinHandle<()>>>,
    media: AsyncMutex<Option<Media>>,
}

impl SendspinOutput {
    #[must_use]
    pub fn new(nom: String, client_id: &str, liaison: LiaisonLecteur) -> Self {
        Self {
            nom,
            device_id: identifiant_de_sortie(client_id),
            liaison,
            diffusion: Arc::new(Mutex::new(Diffusion::default())),
            tache: AsyncMutex::new(None),
            media: AsyncMutex::new(None),
        }
    }

    fn diffusion(&self) -> std::sync::MutexGuard<'_, Diffusion> {
        self.diffusion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn interrompre(&self) {
        if let Some(t) = self.tache.lock().await.take() {
            t.abort();
            let _ = t.await;
        }
    }

    async fn ordonner(&self, ordre: OrdreLecteur) -> Result<(), String> {
        self.liaison
            .ordonner(ordre)
            .await
            .map_err(|e| e.to_string())
    }

    fn position_ms(d: &Diffusion, duree_ms: u64) -> u64 {
        let p = match d.phase {
            Phase::Arret => 0,
            Phase::Pause => d.position_pause_ms,
            Phase::Lecture => {
                let ecoule = d
                    .t0_us
                    .map_or(0, |t0| (maintenant_us() - t0).max(0) / 1_000);
                d.debut_ms + u64::try_from(ecoule).unwrap_or(0)
            }
        };
        if duree_ms > 0 { p.min(duree_ms) } else { p }
    }

    async fn lancer(&self, media: Media, format: FormatAudio, debut_ms: u64) {
        {
            let mut d = self.diffusion();
            d.phase = Phase::Lecture;
            d.debut_ms = debut_ms;
            d.t0_us = None;
            d.fin_us = None;
            d.format = Some(format.clone());
            d.flux_ouvert = true;
            d.erreur = None;
        }
        let liaison = self.liaison.clone();
        let diffusion = self.diffusion.clone();
        let nom = self.nom.clone();
        let tache = tokio::spawn(async move {
            if let Err(e) = diffuser(&liaison, &media.source, &format, debut_ms, &diffusion).await {
                warn!(sortie = %nom, error = %e, "sendspin_diffusion_echouee");
                let mut d = diffusion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                d.erreur = Some(e);
                // Rien ne joue plus : la fin est maintenant, pas une promesse.
                d.fin_us.get_or_insert_with(maintenant_us);
            }
        });
        *self.tache.lock().await = Some(tache);
    }
}

/// Le premier message du décodeur progressif est un en-tête WAV de 44 octets
/// quand une profondeur cible est demandée : il ne fait pas partie du PCM.
#[must_use]
pub fn est_entete_wav(premier: &[u8]) -> bool {
    premier.len() == 44 && premier.starts_with(b"RIFF") && &premier[8..12] == b"WAVE"
}

/// Taille d'un morceau en trames : 50 ms, réduite si `buffer_capacity` ne
/// pourrait pas en contenir deux.
#[must_use]
pub fn trames_par_morceau(format: &FormatAudio, capacite: u64) -> u64 {
    let visees = (u64::from(format.sample_rate) * DUREE_MORCEAU_US as u64 / 1_000_000).max(1);
    let par_trame = format.octets_par_trame().max(1) as u64;
    let plafond = (capacite / 2).saturating_sub(TAILLE_ENTETE_AUDIO as u64) / par_trame;
    visees.min(plafond).max(1)
}

async fn attendre_jusqu_a(echeance_us: i64) {
    let reste = echeance_us - maintenant_us();
    if reste > 0 {
        tokio::time::sleep(std::time::Duration::from_micros(reste as u64)).await;
    }
}

/// Décode `source` à partir de `debut_ms` dans `format`, découpe en morceaux,
/// les horodate sur une ligne de temps continue et les envoie au rythme que
/// permettent l'horizon et `buffer_capacity`.
async fn diffuser(
    liaison: &LiaisonLecteur,
    source: &str,
    format: &FormatAudio,
    debut_ms: u64,
    diffusion: &Mutex<Diffusion>,
) -> Result<(), String> {
    let (chemin, _garde) = super::airplay::url_to_local_path(source).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (niveaux, niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    // Personne ne lit les niveaux de cette sortie : un récepteur fermé fait
    // échouer chaque envoi en silence, au lieu d'accumuler des fenêtres.
    drop(niveaux_rx);
    let (rate, canaux, bits) = (
        format.sample_rate,
        u32::from(format.channels),
        format.bit_depth,
    );
    let seek_s = debut_ms as f64 / 1_000.0;
    let decodeur = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_seeked(
            &chemin,
            Some(rate),
            Some(canaux),
            Some(bits),
            tx,
            32 * 1024,
            Arc::new(tokio::sync::Notify::new()),
            niveaux,
            seek_s,
        )
    });

    let octets_trame = format.octets_par_trame();
    let trames_morceau = trames_par_morceau(format, liaison.capacite());
    let octets_morceau = trames_morceau as usize * octets_trame;
    let avance = avance_de_depart_us(&liaison.etat().lecteur.unwrap_or_default());
    let t0 = maintenant_us() + avance;
    if let Ok(mut d) = diffusion.lock() {
        d.t0_us = Some(t0);
    }
    let mut compta = ComptabiliteTampon::nouvelle(liaison.capacite());
    let mut tampon: Vec<u8> = Vec::with_capacity(octets_morceau * 2);
    let mut premier = true;
    let mut decodage_fini = false;
    let mut trames_emises: u64 = 0;

    loop {
        while tampon.len() < octets_morceau && !decodage_fini {
            match rx.recv().await {
                Some(bloc) => {
                    let entete = premier && est_entete_wav(&bloc);
                    premier = false;
                    if !entete {
                        tampon.extend_from_slice(&bloc);
                    }
                }
                None => decodage_fini = true,
            }
        }
        let n = (tampon.len().min(octets_morceau) / octets_trame) * octets_trame;
        if n == 0 {
            break;
        }
        let donnees: Vec<u8> = tampon.drain(..n).collect();
        let trames = (n / octets_trame) as u64;
        let ts = t0 + duree_us(trames_emises, rate);
        let duree = duree_us(trames_emises + trames, rate) - duree_us(trames_emises, rate);
        attendre_jusqu_a(ts - avance - HORIZON_US).await;
        let taille = (TAILLE_ENTETE_AUDIO + n) as u64;
        loop {
            let delai = liaison
                .etat()
                .lecteur
                .map_or(0, |l| i64::from(l.output_delay_ms) * 1_000);
            if compta.admet(taille, maintenant_us(), delai) {
                break;
            }
            match compta.prochaine_liberation(delai) {
                Some(t) => attendre_jusqu_a(t.max(maintenant_us() + 1_000)).await,
                None => return Err("sendspin: chunk larger than buffer_capacity".into()),
            }
        }
        liaison
            .ordonner(OrdreLecteur::Morceau {
                timestamp_us: ts,
                donnees,
            })
            .await
            .map_err(|e| e.to_string())?;
        compta.enregistrer(ts, duree, taille);
        trames_emises += trames;
    }

    let fin = t0 + duree_us(trames_emises, rate);
    if let Ok(mut d) = diffusion.lock() {
        d.fin_us = Some(fin);
    }
    match decodeur.await {
        Ok(Err(e)) if trames_emises == 0 => Err(format!("decode: {e}")),
        Err(e) if trames_emises == 0 => Err(format!("decode join: {e}")),
        _ => Ok(()),
    }
}

#[async_trait::async_trait]
impl OutputTarget for SendspinOutput {
    fn name(&self) -> &str {
        &self.nom
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        TYPE_DE_SORTIE
    }

    fn capabilities(&self) -> OutputCapabilities {
        // Volume et sourdine dépendent de `supported_commands`, qui peut
        // changer en cours de connexion : ils sont déclarés, et un refus de
        // l'enceinte remonte comme une erreur nommée.
        OutputCapabilities::v1(true, true, true, true, true, false).with_percent_volume()
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.interrompre().await;
        let etat = self.liaison.etat();
        let prefere = etat.lecteur.as_ref().and_then(|l| l.format.clone());
        let format = choisir_format(self.liaison.formats(), prefere.as_ref())
            .ok_or_else(|| "sendspin: no producible format".to_string())?;
        if self.diffusion().flux_ouvert {
            // Saut de piste : le flux continue, ses tampons sont vidés.
            self.ordonner(OrdreLecteur::Vider).await?;
        }
        self.ordonner(OrdreLecteur::Demarrer(format.clone()))
            .await?;
        let m = Media {
            source: media.file_path.unwrap_or(media.url).to_string(),
            uri: media.url.to_string(),
            titre: media.title.map(str::to_owned),
            artiste: media.artist.map(str::to_owned),
            duree_ms: media.duration_ms.unwrap_or(0),
        };
        *self.media.lock().await = Some(m.clone());
        info!(sortie = %self.nom, format = ?format, "sendspin_lecture");
        self.lancer(m, format, 0).await;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        let duree = self.media.lock().await.as_ref().map_or(0, |m| m.duree_ms);
        let position = {
            let d = self.diffusion();
            if d.phase != Phase::Lecture {
                return Ok(());
            }
            Self::position_ms(&d, duree)
        };
        self.interrompre().await;
        self.ordonner(OrdreLecteur::Suspendre).await?;
        let mut d = self.diffusion();
        d.phase = Phase::Pause;
        d.position_pause_ms = position;
        d.flux_ouvert = false;
        d.t0_us = None;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        let (format, position) = {
            let d = self.diffusion();
            if d.phase != Phase::Pause {
                return Ok(());
            }
            (d.format.clone(), d.position_pause_ms)
        };
        let format = format.ok_or("sendspin: no format to resume")?;
        let media = self
            .media
            .lock()
            .await
            .clone()
            .ok_or("sendspin: nothing to resume")?;
        self.ordonner(OrdreLecteur::Demarrer(format.clone()))
            .await?;
        self.lancer(media, format, position).await;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.interrompre().await;
        let resultat = match self.liaison.ordonner(OrdreLecteur::Arreter).await {
            // Enceinte partie : il n'y a plus rien à arrêter chez elle.
            Err(crate::sendspin::lecteur::RefusOrdre::Deconnecte) => Ok(()),
            r => r.map_err(|e| e.to_string()),
        };
        let mut d = self.diffusion();
        d.phase = Phase::Arret;
        d.flux_ouvert = false;
        d.t0_us = None;
        d.fin_us = None;
        resultat
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let phase = self.diffusion().phase;
        match phase {
            Phase::Arret => Err("sendspin: nothing to seek".into()),
            Phase::Pause => {
                self.diffusion().position_pause_ms = position_ms;
                Ok(())
            }
            Phase::Lecture => {
                let format = self
                    .diffusion()
                    .format
                    .clone()
                    .ok_or("sendspin: no format")?;
                let media = self
                    .media
                    .lock()
                    .await
                    .clone()
                    .ok_or("sendspin: nothing to seek")?;
                self.interrompre().await;
                self.ordonner(OrdreLecteur::Vider).await?;
                self.lancer(media, format, position_ms).await;
                Ok(())
            }
        }
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let v = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.ordonner(OrdreLecteur::Volume(v)).await
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.ordonner(OrdreLecteur::Sourdine(muted)).await
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let media = self.media.lock().await.clone();
        let duree = media.as_ref().map_or(0, |m| m.duree_ms);
        let etat = self.liaison.etat();
        let (state, position, fini) = {
            let d = self.diffusion();
            let fini = d.phase == Phase::Lecture
                && d.erreur.is_none()
                && d.fin_us.is_some_and(|f| maintenant_us() >= f);
            let state = match d.phase {
                Phase::Lecture if fini || d.erreur.is_some() => TransportState::Stopped,
                Phase::Lecture => TransportState::Playing,
                Phase::Pause => TransportState::Paused,
                Phase::Arret => TransportState::Stopped,
            };
            (state, Self::position_ms(&d, duree), fini)
        };
        let lecteur = etat.lecteur.unwrap_or_default();
        Ok(OutputStatus {
            state,
            position_ms: position,
            duration_ms: duree,
            volume: lecteur.volume.map_or(1.0, |v| f64::from(v) / 100.0),
            muted: lecteur.muted.unwrap_or(false),
            current_uri: media.as_ref().map(|m| m.uri.clone()),
            track_title: media.as_ref().and_then(|m| m.titre.clone()),
            track_artist: media.as_ref().and_then(|m| m.artiste.clone()),
            ended_naturally: fini,
            realtime: true,
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        self.liaison.connectee() && self.liaison.etat().disponible()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i3326_entete_wav_reconnu_seulement_en_tete_de_44_octets() {
        let h = crate::audio::wav::build_wav_header(2, 48_000, 24);
        assert!(est_entete_wav(&h));
        assert!(!est_entete_wav(&h[..43]));
        assert!(
            !est_entete_wav(&[0u8; 44]),
            "du PCM silencieux n'est pas un en-tête"
        );
    }

    #[test]
    fn i3326_morceaux_de_50_ms_bornes_par_la_capacite() {
        let f = FormatAudio::pcm(48_000, 2, 24);
        assert_eq!(trames_par_morceau(&f, 10_000_000), 2_400, "50 ms à 48 kHz");
        // Capacité de 13 + 600 octets ×2 : 100 trames de 6 octets au plus.
        assert_eq!(trames_par_morceau(&f, 2 * (13 + 600)), 100);
        assert_eq!(trames_par_morceau(&f, 0), 1, "jamais zéro trame");
        assert_eq!(identifiant_de_sortie("abc"), "sendspin:abc");
    }
}
