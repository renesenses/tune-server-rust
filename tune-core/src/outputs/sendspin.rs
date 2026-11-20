//! Sortie Sendspin : une enceinte `player@v1` connectée à Tune (#3326, S2-c).
//!
//! À la différence de DLNA ou de Chromecast, l'enceinte ne va pas chercher
//! une URL : c'est Tune qui POUSSE l'audio, horodaté dans son horloge, sur la
//! connexion WebSocket chiffrée que l'enceinte a ouverte. La sortie ne parle
//! donc pas au réseau elle-même : elle passe des [`OrdreLecteur`] au pilote de
//! la connexion par une [`LiaisonLecteur`], et la [`SessionLecteur`] décide
//! des messages (voir `sendspin::lecteur`).
//!
//! - Codecs : PCM et FLAC (les deux que la spécification impose au serveur),
//!   décodage progressif du fichier (même chemin que la sortie locale),
//!   morceaux de 50 ms, avance de départ et comptabilité du tampon selon
//!   `roles/player/v1.md`.
//! - **Une seule ligne de temps** : l'enchaînement d'une piste à la suivante
//!   (préparée par `set_next_media`) ne coupe pas le flux ; si le format
//!   change (taux natif de la nouvelle piste, ou préférence de l'enceinte
//!   reçue en `client/state` en pleine lecture), un `stream/start` en place
//!   l'annonce et le premier morceau du nouveau format est horodaté à la
//!   suite du dernier de l'ancien (« MUST » de la spécification).
//! - « Pause » n'existe pas dans Sendspin. Choix de cette version, à
//!   confirmer (question écrite dans la PR) : pause = `stream/end` et groupe
//!   `stopped` (l'activité `playback` reste déclarée) ; reprise = nouveau
//!   `stream/start` depuis la position atteinte. Seek et saut de piste =
//!   `stream/clear`, comme la spécification le demande.
//!
//! [`SessionLecteur`]: crate::sendspin::lecteur::SessionLecteur

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::audio::encoder::EncodeurTramesFlac;
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

/// Une piste suivante préparée moins de tant avant la fin de la ligne de
/// temps n'est plus enchaînée : la fin naturelle la jouera (petit trou).
pub const MARGE_SUIVANTE_US: i64 = 300_000;

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

#[derive(Debug, Clone)]
struct Media {
    source: String,
    uri: String,
    titre: Option<String>,
    artiste: Option<String>,
    duree_ms: u64,
    /// Fréquence native annoncée par l'orchestrateur, pour éviter un
    /// rééchantillonnage quand l'enceinte annonce ce taux.
    taux: Option<u32>,
    /// #9175 — la source se RELIT-elle (fichier, session fichier ou
    /// mandataire, radio en direct) ? Un flux traité servi par un canal
    /// (`PlayMedia::byte_seekable == false`) ne se lit qu'une fois : une
    /// reprise ou un saut ne peuvent pas le rouvrir, c'est la zone qui
    /// relance le flux à la position.
    relisible: bool,
    /// #9175 — position, dans la piste, où commence le flux servi : non nulle
    /// quand la zone a relancé un flux décodé à une position (saut, reprise).
    /// Le décodeur compte depuis le début du flux, l'écran depuis le début de
    /// la piste.
    depart_du_flux_ms: u64,
}

impl Media {
    fn depuis(media: &PlayMedia<'_>) -> Self {
        Self {
            // #9175 — `file_path` n'est renseigné pour cette sortie que si le
            // flux servi EST le fichier d'origine (passthrough, aucun
            // traitement : `fichier_d_origine_pour_la_sortie`). Dès que le
            // serveur transcode (égaliseur, correction de pièce, ReplayGain,
            // crossfeed), il vaut `None` et c'est le flux traité de `url` qui
            // est décodé.
            source: media.file_path.unwrap_or(media.url).to_string(),
            uri: media.url.to_string(),
            titre: media.title.map(str::to_owned),
            artiste: media.artist.map(str::to_owned),
            duree_ms: media.duration_ms.unwrap_or(0),
            taux: media.sample_rate,
            relisible: media.file_path.is_some() || media.byte_seekable || media.live_stream,
            depart_du_flux_ms: 0,
        }
    }

    /// Le flux commence à `depart_ms` dans la piste. La durée de la piste,
    /// quand l'orchestrateur la donne, fait foi ; sinon la durée annoncée
    /// était celle qui restait (#6630), et on rend celle de la piste.
    fn noter_le_depart(&mut self, depart_ms: u64, duree_piste_ms: Option<u64>) {
        if let Some(d) = duree_piste_ms.filter(|d| *d > 0) {
            self.duree_ms = d;
        } else if self.duree_ms > 0 {
            self.duree_ms = self.duree_ms.saturating_sub(self.depart_du_flux_ms) + depart_ms;
        }
        self.depart_du_flux_ms = depart_ms;
    }
}

/// Une piste posée sur la ligne de temps : elle commence à `debut_us`
/// (horloge du serveur) à la position `debut_ms` du fichier.
#[derive(Debug, Clone)]
struct Segment {
    debut_us: i64,
    debut_ms: u64,
    media: Media,
}

#[derive(Debug, Default)]
struct Diffusion {
    phase: Phase,
    /// La piste en cours et, une fois enchaînée, la suivante.
    segments: Vec<Segment>,
    /// Horodatage serveur de la fin du dernier morceau, une fois tout envoyé
    /// et aucune suivante enchaînée.
    fin_us: Option<i64>,
    position_pause_ms: u64,
    /// La piste hors lecture (pause, arrêt) : ce que rend l'état.
    media_hors_lecture: Option<Media>,
    format: Option<FormatAudio>,
    flux_ouvert: bool,
    erreur: Option<String>,
    /// Préparée par `set_next_media`, enchaînée par la diffusion.
    suivante: Option<Media>,
}

impl Diffusion {
    fn segment_courant(&self, maintenant: i64) -> Option<&Segment> {
        self.segments
            .iter()
            .rev()
            .find(|s| s.debut_us <= maintenant)
            .or_else(|| self.segments.first())
    }

    fn media_courant(&self, maintenant: i64) -> Option<Media> {
        match self.phase {
            Phase::Lecture => self.segment_courant(maintenant).map(|s| s.media.clone()),
            _ => self.media_hors_lecture.clone(),
        }
    }

    fn position_ms(&self, maintenant: i64) -> u64 {
        match self.phase {
            Phase::Arret => 0,
            Phase::Pause => self.position_pause_ms,
            Phase::Lecture => self.segment_courant(maintenant).map_or(0, |s| {
                let ecoule = u64::try_from((maintenant - s.debut_us).max(0) / 1_000).unwrap_or(0);
                let p = s.debut_ms + ecoule;
                if s.media.duree_ms > 0 {
                    p.min(s.media.duree_ms)
                } else {
                    p
                }
            }),
        }
    }
}

/// Le codec d'une diffusion : la taille des morceaux et, en FLAC,
/// l'encodeur et son en-tête.
struct Codec {
    trames_morceau: u64,
    flac: Option<EncodeurTramesFlac>,
    entete_b64: Option<String>,
}

impl Codec {
    fn nouveau(format: &FormatAudio, capacite: u64) -> Result<Self, String> {
        let trames_morceau = trames_par_morceau(format, capacite);
        if !format.est_flac() {
            return Ok(Self {
                trames_morceau,
                flac: None,
                entete_b64: None,
            });
        }
        // Un bloc FLAC fait au moins 16 trames (sauf le dernier).
        let bloc = u16::try_from(trames_morceau.max(16)).unwrap_or(u16::MAX);
        let flac = EncodeurTramesFlac::nouveau(
            format.sample_rate,
            u32::from(format.bit_depth),
            u32::from(format.channels),
            bloc,
        )?;
        let entete_b64 = Some(base64::engine::general_purpose::STANDARD.encode(flac.entete()));
        Ok(Self {
            trames_morceau: u64::from(bloc),
            flac: Some(flac),
            entete_b64,
        })
    }

    fn ordre_de_depart(&self, format: &FormatAudio) -> OrdreLecteur {
        OrdreLecteur::Demarrer {
            format: format.clone(),
            codec_header: self.entete_b64.clone(),
        }
    }

    fn encoder(&mut self, pcm: Vec<u8>) -> Result<Vec<u8>, String> {
        match self.flac.as_mut() {
            Some(f) => f.trame(&pcm),
            None => Ok(pcm),
        }
    }
}

/// Le format que la diffusion doit utiliser maintenant : préférence de
/// l'enceinte, sinon taux natif de la piste, sinon première entrée.
fn format_voulu(liaison: &LiaisonLecteur, taux: Option<u32>) -> Option<FormatAudio> {
    let prefere = liaison.etat().lecteur.and_then(|l| l.format);
    choisir_format(liaison.formats(), prefere.as_ref(), taux)
}

pub struct SendspinOutput {
    nom: String,
    device_id: String,
    liaison: LiaisonLecteur,
    diffusion: Arc<Mutex<Diffusion>>,
    tache: AsyncMutex<Option<JoinHandle<()>>>,
}

/// #8029 — une sortie retirée du registre sans `stop()` (enceinte partie,
/// zone supprimée) laissait sa tâche de diffusion envoyer des trames à une
/// liaison que plus personne ne tenait. La tâche s'arrête avec la sortie.
impl Drop for SendspinOutput {
    fn drop(&mut self) {
        if let Some(tache) = self.tache.get_mut().take() {
            tache.abort();
        }
    }
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
        }
    }

    fn diffusion(&self) -> std::sync::MutexGuard<'_, Diffusion> {
        verrou(&self.diffusion)
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

    /// Lance une diffusion de `media` depuis `debut_ms`, sur une ligne de temps
    /// neuve (après `stream/start` à vide ou `stream/clear`).
    async fn lancer(&self, media: Media, format: FormatAudio, codec: Codec, debut_ms: u64) {
        let avance = avance_de_depart_us(&self.liaison.etat().lecteur.unwrap_or_default());
        let t0 = maintenant_us() + avance;
        {
            let mut d = self.diffusion();
            d.phase = Phase::Lecture;
            d.segments = vec![Segment {
                debut_us: t0,
                debut_ms,
                media: media.clone(),
            }];
            d.fin_us = None;
            d.media_hors_lecture = None;
            d.format = Some(format.clone());
            d.flux_ouvert = true;
            d.erreur = None;
        }
        let liaison = self.liaison.clone();
        let diffusion = self.diffusion.clone();
        let nom = self.nom.clone();
        let tache = tokio::spawn(async move {
            let depart = Depart {
                media,
                debut_ms,
                format,
                codec,
                t0,
            };
            if let Err(e) = diffuser(&liaison, depart, &diffusion).await {
                warn!(sortie = %nom, error = %e, "sendspin_diffusion_echouee");
                let mut d = verrou(&diffusion);
                d.erreur = Some(e);
                // Rien ne joue plus : la fin est maintenant, pas une promesse.
                d.fin_us.get_or_insert_with(maintenant_us);
            }
        });
        *self.tache.lock().await = Some(tache);
    }

    /// #9175 — la zone a relancé un flux DÉCODÉ qui commence à `depart_ms`
    /// dans la piste (saut ou reprise sur un flux traité, qui ne se relit
    /// pas) : la position et la durée se comptent depuis le début de la
    /// piste, le décodeur depuis le début du flux.
    pub fn noter_le_depart_du_flux(&self, depart_ms: u64, duree_piste_ms: Option<u64>) {
        let mut d = self.diffusion();
        if let Some(s) = d.segments.first_mut() {
            s.debut_ms = s.debut_ms.saturating_sub(s.media.depart_du_flux_ms) + depart_ms;
            s.media.noter_le_depart(depart_ms, duree_piste_ms);
        }
        if let Some(m) = d.media_hors_lecture.as_mut() {
            m.noter_le_depart(depart_ms, duree_piste_ms);
        }
    }

    /// Fige l'état hors lecture (pause ou arrêt) sur la piste en cours.
    fn figer(&self, phase: Phase) -> (u64, Option<Media>) {
        let mut d = self.diffusion();
        let maintenant = maintenant_us();
        let position = d.position_ms(maintenant);
        let media = d.media_courant(maintenant);
        d.phase = phase;
        d.position_pause_ms = position;
        d.media_hors_lecture = media.clone();
        d.segments.clear();
        d.flux_ouvert = false;
        d.fin_us = None;
        (position, media)
    }
}

fn verrou(diffusion: &Mutex<Diffusion>) -> std::sync::MutexGuard<'_, Diffusion> {
    diffusion
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// Ce qu'une diffusion reçoit au départ.
struct Depart {
    media: Media,
    debut_ms: u64,
    format: FormatAudio,
    codec: Codec,
    /// Horodatage serveur du premier morceau.
    t0: i64,
}

/// Les octets PCM du décodeur, et la tâche qui les produit (profondeur, débit).
type DecodeurOuvert = (
    tokio::sync::mpsc::Receiver<Vec<u8>>,
    tokio::task::JoinHandle<Result<(u16, u32), String>>,
);

/// Le décodeur progressif d'un fichier, à partir de `depuis_s`, dans `format`.
fn ouvrir_decodeur(
    source: &str,
    format: &FormatAudio,
    depuis_s: f64,
    en_direct: bool,
    relisible: bool,
) -> DecodeurOuvert {
    if super::decodeur_distant::est_distante(source) {
        return super::decodeur_distant::ouvrir_decodeur_distant(
            source.to_owned(),
            Some(format.sample_rate),
            Some(u32::from(format.channels)),
            Some(format.bit_depth),
            depuis_s,
            en_direct,
            relisible,
        );
    }
    let chemin = source.strip_prefix("file://").unwrap_or(source);
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (niveaux, niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    // Personne ne lit les niveaux de cette sortie : un récepteur fermé fait
    // échouer chaque envoi en silence, au lieu d'accumuler des fenêtres.
    drop(niveaux_rx);
    let (rate, canaux, bits) = (
        format.sample_rate,
        u32::from(format.channels),
        format.bit_depth,
    );
    let chemin = chemin.to_owned();
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
            depuis_s,
        )
    });
    (rx, decodeur)
}

/// Décode, découpe, encode et horodate les morceaux sur UNE ligne de temps
/// continue, et les envoie au rythme que permettent l'horizon et
/// `buffer_capacity`. Enchaîne la piste suivante préparée sans couper le
/// flux, et suit un changement de format (piste ou préférence de l'enceinte)
/// par un `stream/start` en place, le temps continuant.
async fn diffuser(
    liaison: &LiaisonLecteur,
    depart: Depart,
    diffusion: &Mutex<Diffusion>,
) -> Result<(), String> {
    let Depart {
        mut media,
        debut_ms,
        mut format,
        mut codec,
        t0,
    } = depart;
    let mut compta = ComptabiliteTampon::nouvelle(liaison.capacite());
    // Origine de la série de morceaux dans le format courant, et trames
    // émises depuis : l'horodatage d'un morceau est `base + durée(trames)`.
    let mut base_us = t0;
    let mut trames_base: u64 = 0;
    // Position dans le FLUX : il commence à `depart_du_flux_ms` dans la piste.
    let mut position_s = debut_ms.saturating_sub(media.depart_du_flux_ms) as f64 / 1_000.0;
    let mut trames_total: u64 = 0;

    'pistes: loop {
        // #9175 — la source (fichier, ou flux servi par Tune) est décodée au
        // fil de l'eau, comme chez Devialet AIR (#6911) : plus de
        // téléchargement complet en mémoire avant le premier échantillon.
        let chemin = media.source.clone();
        let en_direct = media.duree_ms == 0;
        'decodeur: loop {
            // Le format se re-dérive à chaque ouverture du décodeur : nouvelle
            // piste à un autre taux, ou préférence changée par l'enceinte.
            if let Some(voulu) = format_voulu(liaison, media.taux)
                && voulu != format
            {
                base_us += duree_us(trames_base, format.sample_rate);
                trames_base = 0;
                let nouveau = Codec::nouveau(&voulu, liaison.capacite())?;
                liaison
                    .ordonner(nouveau.ordre_de_depart(&voulu))
                    .await
                    .map_err(|e| e.to_string())?;
                info!(ancien = ?format, nouveau = ?voulu, a_us = base_us, "sendspin_changement_de_format");
                format = voulu;
                codec = nouveau;
                verrou(diffusion).format = Some(format.clone());
            }
            let rate = format.sample_rate;
            let octets_trame = format.octets_par_trame();
            let octets_morceau = codec.trames_morceau as usize * octets_trame;
            let avance = avance_de_depart_us(&liaison.etat().lecteur.unwrap_or_default());
            // Le décodeur tronque `secondes × taux` : 4,672 s × 48 kHz donne
            // 224 255,99… et reprendrait UNE trame trop tôt, renvoyant une
            // trame déjà partie (mesuré contre aiosendspin). Une demi-trame de
            // plus vise le milieu de la trame voulue.
            let depuis_s = if position_s > 0.0 {
                position_s + 0.5 / f64::from(rate)
            } else {
                0.0
            };
            let (mut rx, decodeur) =
                ouvrir_decodeur(&chemin, &format, depuis_s, en_direct, media.relisible);
            let mut tampon: Vec<u8> = Vec::with_capacity(octets_morceau * 2);
            let mut premier = true;
            let mut decodage_fini = false;
            let mut trames_decodeur: u64 = 0;
            loop {
                if format_voulu(liaison, media.taux).is_some_and(|v| v != format) {
                    // Reprendre à la trame qui suit la dernière ENVOYÉE : rien
                    // n'est renvoyé, rien n'est sauté.
                    position_s += trames_decodeur as f64 / f64::from(rate);
                    drop(rx);
                    let _ = decodeur.await;
                    continue 'decodeur;
                }
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
                let trames = (n / octets_trame) as u64;
                let donnees = codec.encoder(tampon.drain(..n).collect())?;
                if trames_total == 0 {
                    // Premier morceau d'une ligne de temps neuve : « servers MUST
                    // schedule the first audio timestamp far enough in the
                    // future ». L'avance a été comptée au lancement ; ouvrir le
                    // fichier et décoder le premier bloc a pu en manger une
                    // part (mesuré sous charge : 94 ms de retard chez
                    // aiosendspin). La ligne de temps part donc de maintenant.
                    let au_plus_tot = maintenant_us() + avance;
                    if base_us < au_plus_tot {
                        let decalage = au_plus_tot - base_us;
                        base_us = au_plus_tot;
                        let mut d = verrou(diffusion);
                        if let Some(s) = d.segments.first_mut() {
                            s.debut_us += decalage;
                        }
                    }
                }
                let ts = base_us + duree_us(trames_base, rate);
                let duree = duree_us(trames_base + trames, rate) - duree_us(trames_base, rate);
                attendre_jusqu_a(ts - avance - HORIZON_US).await;
                let taille = (TAILLE_ENTETE_AUDIO + donnees.len()) as u64;
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
                trames_base += trames;
                trames_decodeur += trames;
                trames_total += trames;
            }
            match decodeur.await {
                Ok(Err(e)) if trames_total == 0 => return Err(format!("decode: {e}")),
                Err(e) if trames_total == 0 => return Err(format!("decode join: {e}")),
                _ => break 'decodeur,
            }
        }

        // Piste entièrement envoyée. La suivante, si elle est préparée à
        // temps, continue la MÊME ligne de temps (« Track transitions » : pas
        // de stream/clear ni de stream/end).
        let fin = base_us + duree_us(trames_base, format.sample_rate);
        loop {
            let suivante = {
                let mut d = verrou(diffusion);
                let suivante = d.suivante.take();
                if let Some(m) = suivante.as_ref() {
                    let maintenant = maintenant_us();
                    // Les pistes déjà entièrement jouées ne servent plus.
                    let courante = d
                        .segments
                        .iter()
                        .rposition(|s| s.debut_us <= maintenant)
                        .unwrap_or(0);
                    d.segments.drain(..courante);
                    d.segments.push(Segment {
                        debut_us: fin,
                        debut_ms: 0,
                        media: m.clone(),
                    });
                } else if maintenant_us() >= fin - MARGE_SUIVANTE_US {
                    d.fin_us = Some(fin);
                    return Ok(());
                }
                suivante
            };
            if let Some(m) = suivante {
                info!(titre = ?m.titre, a_us = fin, "sendspin_enchainement_sans_coupure");
                media = m;
                position_s = 0.0;
                continue 'pistes;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
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
        // l'enceinte remonte comme une erreur nommée. L'enchaînement sans
        // coupure est interne : la suivante est posée sur la même ligne de
        // temps.
        OutputCapabilities::v1(true, true, true, true, true, true).with_percent_volume()
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.interrompre().await;
        let m = Media::depuis(media);
        let format = format_voulu(&self.liaison, m.taux)
            .ok_or_else(|| "sendspin: no producible format".to_string())?;
        let codec = Codec::nouveau(&format, self.liaison.capacite())?;
        let flux_ouvert = {
            let mut d = self.diffusion();
            d.suivante = None;
            d.flux_ouvert
        };
        if flux_ouvert {
            // Saut de piste : le flux continue, ses tampons sont vidés.
            self.ordonner(OrdreLecteur::Vider).await?;
        }
        self.ordonner(codec.ordre_de_depart(&format)).await?;
        info!(sortie = %self.nom, format = ?format, "sendspin_lecture");
        self.lancer(m, format, codec, 0).await;
        Ok(())
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let m = Media::depuis(media);
        info!(sortie = %self.nom, titre = ?m.titre, "sendspin_suivante_preparee");
        self.diffusion().suivante = Some(m);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        if self.diffusion().phase != Phase::Lecture {
            return Ok(());
        }
        self.interrompre().await;
        self.ordonner(OrdreLecteur::Suspendre).await?;
        self.figer(Phase::Pause);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        let (position, media) = {
            let d = self.diffusion();
            if d.phase != Phase::Pause {
                return Ok(());
            }
            (d.position_pause_ms, d.media_hors_lecture.clone())
        };
        let media = media.ok_or("sendspin: nothing to resume")?;
        if !media.relisible {
            // #9175 — `device_released_on_pause` l'a déjà dit à la zone, qui
            // relance le flux à la position au lieu d'appeler ceci.
            return Err(
                "sendspin: le flux traité ne se relit pas : la zone doit le relancer".into(),
            );
        }
        let format = format_voulu(&self.liaison, media.taux)
            .ok_or_else(|| "sendspin: no producible format".to_string())?;
        let codec = Codec::nouveau(&format, self.liaison.capacite())?;
        self.ordonner(codec.ordre_de_depart(&format)).await?;
        self.lancer(media, format, codec, position).await;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.interrompre().await;
        let resultat = match self.liaison.ordonner(OrdreLecteur::Arreter).await {
            // Enceinte partie : il n'y a plus rien à arrêter chez elle.
            Err(crate::sendspin::lecteur::RefusOrdre::Deconnecte) => Ok(()),
            r => r.map_err(|e| e.to_string()),
        };
        self.figer(Phase::Arret);
        self.diffusion().suivante = None;
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
                let (format, media) = {
                    let d = self.diffusion();
                    (d.format.clone(), d.media_courant(maintenant_us()))
                };
                let format = format.ok_or("sendspin: no format")?;
                let media = media.ok_or("sendspin: nothing to seek")?;
                if !media.relisible {
                    // #9175 — le saut se joue par une relance du flux à la
                    // position (`deplacer_la_sortie`), jamais ici.
                    return Err(
                        "sendspin: le flux traité ne se relit pas : la zone doit le relancer"
                            .into(),
                    );
                }
                let codec = Codec::nouveau(&format, self.liaison.capacite())?;
                self.interrompre().await;
                self.ordonner(OrdreLecteur::Vider).await?;
                self.lancer(media, format, codec, position_ms).await;
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
        let etat = self.liaison.etat();
        let maintenant = maintenant_us();
        // Enceinte prise par autre chose (`available: false`, `client/leave`) :
        // la connexion a terminé le flux ; ce n'est pas une fin naturelle,
        // même si tout était déjà envoyé.
        let retiree = etat.available == Some(false) || etat.lecture_quittee;
        let (state, position, fini, media) = {
            let d = self.diffusion();
            let fini = d.phase == Phase::Lecture
                && d.erreur.is_none()
                && !retiree
                && d.fin_us.is_some_and(|f| maintenant >= f);
            let state = match d.phase {
                Phase::Lecture if fini || retiree || d.erreur.is_some() => TransportState::Stopped,
                Phase::Lecture => TransportState::Playing,
                Phase::Pause => TransportState::Paused,
                Phase::Arret => TransportState::Stopped,
            };
            (
                state,
                d.position_ms(maintenant),
                fini,
                d.media_courant(maintenant),
            )
        };
        let lecteur = etat.lecteur.unwrap_or_default();
        Ok(OutputStatus {
            state,
            position_ms: position,
            duration_ms: media.as_ref().map_or(0, |m| m.duree_ms),
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

    /// #9175 — en pause sur un flux traité servi par un canal : il ne se
    /// relit pas, la reprise doit relancer le flux à la position.
    fn device_released_on_pause(&self) -> bool {
        let d = self.diffusion();
        d.phase == Phase::Pause && d.media_hors_lecture.as_ref().is_some_and(|m| !m.relisible)
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

    /// Une enceinte factice : chaque ordre est accepté et noté.
    fn enceinte_factice() -> (SendspinOutput, Arc<Mutex<Vec<&'static str>>>) {
        let (liaison, mut cote) = crate::sendspin::lecteur::relier(
            vec![crate::sendspin::lecteur::FormatAudio::pcm(44_100, 2, 16)],
            None,
        );
        let ordres = Arc::new(Mutex::new(Vec::new()));
        let notes = ordres.clone();
        tokio::spawn(async move {
            while let Some(d) = cote.ordres.recv().await {
                let nom = match d.ordre {
                    OrdreLecteur::Morceau { .. } => "morceau",
                    OrdreLecteur::Demarrer { .. } => "demarrer",
                    OrdreLecteur::Vider => "vider",
                    OrdreLecteur::Suspendre => "suspendre",
                    OrdreLecteur::Arreter => "arreter",
                    _ => "autre",
                };
                notes.lock().unwrap().push(nom);
                let _ = d.reponse.send(Ok(()));
            }
        });
        (
            SendspinOutput::new("Banc 9175".into(), "banc-9175", liaison),
            ordres,
        )
    }

    /// Un WAV stéréo 16 bits à 44,1 kHz de `secondes`, en octets.
    fn wav(secondes: usize) -> Vec<u8> {
        let donnees = vec![0x11u8; 44_100 * 4 * secondes];
        let mut f =
            crate::audio::wav::build_wav_header_with_data_size(2, 44_100, 16, donnees.len() as u32)
                .to_vec();
        f.extend_from_slice(&donnees);
        f
    }

    async fn attendre_un_morceau(ordres: &Mutex<Vec<&'static str>>) -> bool {
        for _ in 0..300 {
            if ordres.lock().unwrap().contains(&"morceau") {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// #9175 — un flux TRAITÉ servi par un canal (`byte_seekable == false`,
    /// pas de fichier d'origine) ne se lit qu'une fois : la sortie ne le
    /// rouvre ni au saut ni à la reprise, elle dit à la zone de le relancer
    /// (`device_released_on_pause`) et compte depuis le début de la piste
    /// quand la zone lui apprend où le flux relancé commence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn un_flux_traite_en_canal_se_relance_par_la_zone_9175() {
        let dossier = crate::test_scratch::scratch_dir("sendspin-canal-9175");
        let chemin = dossier.path().join("piste.wav");
        std::fs::write(&chemin, wav(3)).unwrap();
        let chemin = chemin.to_string_lossy().into_owned();
        let (sortie, ordres) = enceinte_factice();

        let canal = PlayMedia {
            url: &chemin,
            file_path: None,
            byte_seekable: false,
            mime_type: "audio/wav",
            duration_ms: Some(2_000),
            ..Default::default()
        };
        sortie.play_media(&canal).await.unwrap();
        assert!(attendre_un_morceau(&ordres).await, "témoin : le flux part");

        assert!(
            sortie.seek(1_000).await.is_err(),
            "#9175 : un canal ne se rouvre pas à la position, la zone doit le relancer"
        );
        sortie.noter_le_depart_du_flux(60_000, None);
        let st = sortie.get_status().await.unwrap();
        assert!(
            st.position_ms >= 60_000,
            "#9175 : position comptée depuis le début de la piste ({})",
            st.position_ms
        );
        assert_eq!(st.duration_ms, 62_000, "durée de la piste, pas du reste");

        sortie.pause().await.unwrap();
        assert!(
            sortie.device_released_on_pause(),
            "#9175 : en pause sur un canal, la reprise doit relancer le flux"
        );
        assert!(sortie.resume().await.is_err());
        sortie.stop().await.unwrap();

        // Contre-épreuve : la même piste servie par son fichier se rouvre.
        let fichier = PlayMedia {
            url: &chemin,
            file_path: Some(&chemin),
            mime_type: "audio/wav",
            duration_ms: Some(3_000),
            ..Default::default()
        };
        sortie.play_media(&fichier).await.unwrap();
        sortie.pause().await.unwrap();
        assert!(!sortie.device_released_on_pause());
        sortie.resume().await.unwrap();
        sortie.stop().await.unwrap();
    }

    /// #9175 — le flux servi par Tune est décodé AU FIL DE L'EAU. Le serveur
    /// du banc envoie l'en-tête et deux secondes, puis se tait sans fermer :
    /// l'ancien téléchargement complet (`url_to_local_path`, `bytes()`)
    /// attendait la fin du corps, et rien ne partait vers l'enceinte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn le_flux_servi_est_decode_au_fil_de_l_eau_9175() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let serveur = tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = ecoute.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut requete = [0u8; 2048];
                    let _ = c.read(&mut requete).await;
                    // Un canal : ni `Range`, ni longueur, et une fin qui ne
                    // vient pas (60 s annoncées, 2 s envoyées).
                    let _ = c
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let corps = wav(60);
                    let _ = c.write_all(&corps[..44 + 44_100 * 4 * 2]).await;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                });
            }
        });
        let url = format!("http://{adresse}/stream/banc-9175.wav");
        let (sortie, ordres) = enceinte_factice();
        let canal = PlayMedia {
            url: &url,
            byte_seekable: false,
            mime_type: "audio/wav",
            duration_ms: Some(60_000),
            ..Default::default()
        };
        sortie.play_media(&canal).await.unwrap();
        assert!(
            attendre_un_morceau(&ordres).await,
            "#9175 : le premier morceau doit partir sans attendre la fin du flux"
        );
        sortie.stop().await.unwrap();
        serveur.abort();
    }

    /// Suite de #9175 — un canal ne se lit qu'une fois : une seule connexion,
    /// sans sonde `Range` (qui en ouvrirait une de plus, puis d'autres pour
    /// lire par plages, jusqu'à une fin réclamée au-delà de ce que le canal a
    /// produit). Contre-épreuve : une source qui se relit est sondée.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn un_canal_se_lit_d_une_seule_connexion_9175() {
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn connexions_pour(byte_seekable: bool) -> usize {
            let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let adresse = ecoute.local_addr().unwrap();
            let compte = Arc::new(AtomicUsize::new(0));
            let c2 = compte.clone();
            let serveur = tokio::spawn(async move {
                while let Ok((mut c, _)) = ecoute.accept().await {
                    c2.fetch_add(1, O::SeqCst);
                    tokio::spawn(async move {
                        let mut requete = [0u8; 2048];
                        let _ = c.read(&mut requete).await;
                        let corps = wav(2);
                        let _ = c
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                                    corps.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = c.write_all(&corps).await;
                    });
                }
            });
            let url = format!("http://{adresse}/stream/banc-connexions-9175.wav");
            let (sortie, ordres) = enceinte_factice();
            let media = PlayMedia {
                url: &url,
                byte_seekable,
                mime_type: "audio/wav",
                duration_ms: Some(2_000),
                ..Default::default()
            };
            sortie.play_media(&media).await.unwrap();
            assert!(attendre_un_morceau(&ordres).await, "témoin : le flux part");
            sortie.stop().await.unwrap();
            serveur.abort();
            compte.load(O::SeqCst)
        }
        assert_eq!(
            connexions_pour(false).await,
            1,
            "suite de #9175 : un canal se lit d'une seule connexion, sans sonde Range"
        );
        assert_eq!(
            connexions_pour(true).await,
            2,
            "contre-épreuve : une source qui se relit est sondée par Range, puis lue"
        );
    }
}
