//! #7148 — la sentinelle de streaming : chaque service connecté sait-il
//! ENCORE livrer un flux qui se décode ?
//!
//! ## Le fait
//!
//! 26/10/2026 : la v1.0.0-rc8.1 est taguée avec un correctif TIDAL (#7106)
//! prouvé contre un faux CDN, construit sur notre propre hypothèse de la forme
//! du MPD. Sur le .18, toutes les URL de segments partaient encore avec
//! `amp;` dans la requête : 403 partout. Aucun test ne pouvait le voir, parce
//! qu'aucun ne parlait au VRAI service.
//!
//! ## Ce que fait la sentinelle
//!
//! Pour chaque service connecté, un titre de référence (choisi une fois, puis
//! rangé dans `settings` ; réglable) est résolu par le chemin de la LECTURE :
//! [`super::request_stream_at_quality`] (avec son rafraîchissement de jeton),
//! puis le repli de qualité Qobuz (#6401). Le début du flux est téléchargé
//! en rejouant les en-têtes du résolveur, comme le fait la lecture — ou lu sur
//! disque pour un DASH TIDAL déjà assemblé par `get_track_url` — puis DÉCODÉ
//! par le décodeur du serveur ([`crate::audio::decode::decode_to_pcm`]). Une
//! radio de référence passe par le lecteur radio lui-même
//! (`sonder_une_station_sans_sortie`) jusqu'à sa première trame.
//!
//! **Jamais de son** : aucune session, aucune sortie, aucune zone n'est
//! touchée. Si une zone joue, le tour est passé ; si une zone se met à jouer
//! pendant le tour, il est interrompu et ne compte pas.
//!
//! Spotify n'a pas d'URL de flux (librespot / Connect) : il est listé
//! « non couvert », jamais compté vert.
//!
//! Le module ne parle ni à GitHub ni à un service de courrier : c'est le
//! minuteur du .18 (`scripts/sentinelle-streaming/`) qui publie le statut de
//! commit et prévient Bertrand. Le serveur ne porte aucun secret de plus.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use crate::streaming::StreamUrl;

/// La configuration, dans `settings`.
pub const CLE_CONFIG: &str = "sentinelle_streaming_config";
/// L'historique des tours, dans `settings` (les plus récents en tête).
pub const CLE_HISTORIQUE: &str = "sentinelle_streaming_historique";
/// Tours gardés : trois jours au rythme horaire.
pub const TOURS_GARDES: usize = 72;

/// La radio de référence par défaut : FIP, servie par Radio France.
pub const RADIO_PAR_DEFAUT: &str = "https://icecast.radiofrance.fr/fip-midfi.mp3";
/// La requête qui choisit un titre de référence quand aucun n'est réglé.
pub const REQUETE_PAR_DEFAUT: &str = "Miles Davis So What";

/// Octets lus en tête d'un flux progressif : plusieurs secondes même en 24/192.
const OCTETS_DE_TETE: usize = 4 * 1024 * 1024;
/// Un MP4 non fragmenté peut porter son `moov` à la fin : la lecture le
/// télécharge en entier (`telecharger_amont`), la sentinelle aussi, borné.
const OCTETS_MP4_MAX: usize = 96 * 1024 * 1024;
/// Secondes décodées : assez pour traverser plusieurs trames ou segments.
const SECONDES_DECODEES: f64 = 3.0;
/// Délai d'une résolution : un DASH TIDAL est assemblé en entier.
const DELAI_RESOLUTION: Duration = Duration::from_secs(240);
/// Délai du téléchargement de tête.
const DELAI_TELECHARGEMENT: Duration = Duration::from_secs(90);
/// Délai de la sonde d'une radio.
const DELAI_RADIO: Duration = Duration::from_secs(45);

/// Services sans URL de flux : listés, jamais comptés verts.
const NON_COUVERTS: &[(&str, &str)] = &[(
    "spotify",
    "Spotify joue par librespot / Spotify Connect, sans URL de flux : la sentinelle ne le couvre pas",
)];

/// Ce que Bertrand règle (`PUT /api/v1/sentinelle/streaming/config`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigSentinelle {
    /// Service → identifiant de piste de référence.
    #[serde(default)]
    pub references: BTreeMap<String, String>,
    /// L'adresse de la radio de référence. `None` : [`RADIO_PAR_DEFAUT`].
    /// Une chaîne vide désactive la sonde radio.
    #[serde(default)]
    pub radio: Option<String>,
    /// Services connectés qu'on ne sonde pas.
    #[serde(default)]
    pub exclus: Vec<String>,
    /// Requête de choix automatique d'une référence. `None` :
    /// [`REQUETE_PAR_DEFAUT`].
    #[serde(default)]
    pub requete_auto: Option<String>,
}

impl ConfigSentinelle {
    pub fn lire(settings: &SettingsRepo) -> Self {
        settings
            .get(CLE_CONFIG)
            .ok()
            .flatten()
            .and_then(|brut| serde_json::from_str(&brut).ok())
            .unwrap_or_default()
    }

    pub fn ecrire(&self, settings: &SettingsRepo) -> Result<(), String> {
        let brut = serde_json::to_string(self).map_err(|e| e.to_string())?;
        settings.set(CLE_CONFIG, &brut)
    }

    fn radio_effective(&self) -> Option<String> {
        match self.radio.as_deref().map(str::trim) {
            None => Some(RADIO_PAR_DEFAUT.to_string()),
            Some("") => None,
            Some(url) => Some(url.to_string()),
        }
    }
}

/// La cause d'un échec. Une ÉTIQUETTE pour trier ; `detail` porte le message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// Le service n'est pas connecté, ou refuse le jeton (401).
    Authentification,
    /// Une réponse HTTP en erreur (403, 404, 5xx…) ou un type non audio.
    Http,
    /// Les octets sont arrivés mais ne se décodent pas.
    Decodage,
    /// Le service n'a pas su rendre de flux (autre erreur).
    Resolution,
    /// Aucune piste de référence n'a pu être choisie.
    Reference,
    /// Délai dépassé.
    Delai,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Etat {
    Ok,
    Echec,
    NonCouvert,
}

/// Le verdict d'un service (ou de la radio).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResultatSonde {
    pub service: String,
    /// `service` ou `radio`.
    pub nature: String,
    pub reference: Option<String>,
    pub etat: Etat,
    pub cause: Option<Cause>,
    pub detail: Option<String>,
    pub codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub octets_lus: Option<u64>,
    pub duree_ms: u64,
}

/// Le décodage réussi d'une tête de flux.
#[derive(Debug, Clone, PartialEq)]
pub struct FluxDecode {
    pub sample_rate: u32,
    pub channels: u32,
    pub echantillons: usize,
    pub octets_lus: u64,
}

/// Un échec de sonde : sa cause et son message, déjà nettoyé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchecSonde {
    pub cause: Cause,
    pub detail: String,
}

impl EchecSonde {
    fn new(cause: Cause, detail: impl AsRef<str>) -> Self {
        Self {
            cause,
            detail: masquer_les_requetes(detail.as_ref()),
        }
    }
}

/// Le changement d'état d'un service d'un tour au suivant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transition {
    pub service: String,
    pub avant: Option<Etat>,
    pub apres: Etat,
}

/// Un tour de sentinelle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RapportSentinelle {
    /// Secondes Unix du début du tour.
    pub horodatage: u64,
    /// `ok`, `echec` ou `passe` (tour non joué ou interrompu, qui ne compte
    /// pas).
    pub statut: String,
    pub raison: Option<String>,
    pub resultats: Vec<ResultatSonde>,
    /// Rempli par l'appelant au vu du tour précédent qui a compté.
    #[serde(default)]
    pub transitions: Vec<Transition>,
}

impl RapportSentinelle {
    fn passe(raison: impl Into<String>) -> Self {
        Self {
            horodatage: maintenant(),
            statut: "passe".into(),
            raison: Some(raison.into()),
            resultats: Vec::new(),
            transitions: Vec::new(),
        }
    }

    /// Vert si au moins une sonde a été faite et qu'aucune n'a échoué. Les
    /// « non couverts » ne verdissent rien et ne rougissent rien.
    pub fn statut_des_resultats(resultats: &[ResultatSonde]) -> &'static str {
        let sondes = resultats.iter().filter(|r| r.etat != Etat::NonCouvert);
        let mut vu = false;
        for r in sondes {
            vu = true;
            if r.etat == Etat::Echec {
                return "echec";
            }
        }
        if vu { "ok" } else { "echec" }
    }
}

fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Une URL signée est un secret le temps de sa validité, et ce message part
/// dans un journal et une réponse d'API : toute requête (`?…`) d'une adresse
/// est remplacée par `?…`. Le chemin reste, il suffit au diagnostic ; la
/// PRÉSENCE de `amp;` est dite à part, c'est le défaut de #7106.
pub fn masquer_les_requetes(texte: &str) -> String {
    let mut sortie = String::with_capacity(texte.len());
    for (i, morceau) in texte.split(' ').enumerate() {
        if i > 0 {
            sortie.push(' ');
        }
        if (morceau.contains("://") || morceau.starts_with('/'))
            && let Some((avant, requete)) = morceau.split_once('?')
        {
            sortie.push_str(avant);
            sortie.push_str("?…");
            if requete.contains("amp;") {
                sortie.push_str("[requête avec amp;]");
            }
        } else {
            sortie.push_str(morceau);
        }
    }
    sortie
}

/// L'étiquette d'une erreur de RÉSOLUTION, d'après son message.
pub fn cause_de_resolution(message: &str) -> Cause {
    let m = message.to_ascii_lowercase();
    if m.contains("401")
        || m.contains("unauthori")
        || m.contains("not authenticated")
        || m.contains("non connecté")
        || m.contains("token")
    {
        Cause::Authentification
    } else if m.contains("403")
        || m.contains("404")
        || m.contains("http ")
        || m.contains("status")
        || m.contains(" 5")
    {
        Cause::Http
    } else {
        Cause::Resolution
    }
}

/// L'extension que le décodeur attend pour un codec annoncé.
fn extension_du_codec(codec: &str, mime: &str) -> &'static str {
    let c = codec.to_ascii_lowercase();
    let m = mime.to_ascii_lowercase();
    if c.contains("flac") || m.contains("flac") {
        "flac"
    } else if c.contains("mp3") || m.contains("mpeg") {
        "mp3"
    } else if c.contains("ogg") || c.contains("vorbis") || c.contains("opus") || m.contains("ogg") {
        "ogg"
    } else if c.contains("wav") || m.contains("wav") {
        "wav"
    } else {
        // AAC, ALAC, M4A, MP4 : un conteneur MP4 dans tous les cas des services.
        "m4a"
    }
}

fn est_un_mp4(extension: &str) -> bool {
    extension == "m4a"
}

/// Le cœur testable : télécharger (ou lire) le début du flux, puis le DÉCODER.
///
/// - `file://` (DASH TIDAL assemblé par `get_track_url`) : décodé sur place ;
/// - `http(s)://` : un GET qui rejoue les en-têtes du résolveur (#4366), le
///   statut vérifié, le type vérifié (une page HTML n'est pas un flux), la
///   tête écrite dans un fichier temporaire supprimé à la sortie.
pub async fn sonder_un_flux(flux: &StreamUrl) -> Result<FluxDecode, EchecSonde> {
    if let Some(chemin) = flux.url.strip_prefix("file://") {
        let taille = std::fs::metadata(chemin).map(|m| m.len()).unwrap_or(0);
        if taille == 0 {
            return Err(EchecSonde::new(
                Cause::Resolution,
                format!("fichier DASH absent ou vide : {chemin}"),
            ));
        }
        let mut decode = decoder_le_fichier(chemin.to_string()).await?;
        decode.octets_lus = taille;
        return Ok(decode);
    }
    if !(flux.url.starts_with("https://") || flux.url.starts_with("http://")) {
        return Err(EchecSonde::new(
            Cause::Resolution,
            format!("adresse de flux inattendue : {}", flux.url),
        ));
    }

    let extension = extension_du_codec(&flux.quality.codec, &flux.mime_type);
    let borne = if est_un_mp4(extension) {
        OCTETS_MP4_MAX
    } else {
        OCTETS_DE_TETE
    };
    let client = crate::http::client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| EchecSonde::new(Cause::Resolution, format!("client HTTP : {e}")))?;
    let mut requete = client.get(&flux.url);
    for (nom, valeur) in &flux.headers {
        requete = requete.header(nom, valeur);
    }
    let telechargement = async {
        let mut reponse = requete
            .send()
            .await
            .map_err(|e| EchecSonde::new(Cause::Http, format!("GET du flux : {e}")))?;
        let statut = reponse.status();
        if !statut.is_success() {
            return Err(EchecSonde::new(
                Cause::Http,
                format!("le flux répond HTTP {} ({})", statut.as_u16(), flux.url),
            ));
        }
        let type_recu = reponse
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if let Some(mauvais) = super::non_audio_content_type(&type_recu) {
            return Err(EchecSonde::new(
                Cause::Http,
                format!("le flux répond « {mauvais} » au lieu d'un flux audio"),
            ));
        }
        let mut octets = Vec::with_capacity(borne.min(OCTETS_DE_TETE));
        while octets.len() < borne {
            match reponse.chunk().await {
                Ok(Some(c)) => octets.extend_from_slice(&c),
                Ok(None) => break,
                Err(e) => {
                    if octets.is_empty() {
                        return Err(EchecSonde::new(
                            Cause::Http,
                            format!("lecture du flux : {e}"),
                        ));
                    }
                    break;
                }
            }
        }
        octets.truncate(borne);
        Ok(octets)
    };
    let octets = tokio::time::timeout(DELAI_TELECHARGEMENT, telechargement)
        .await
        .map_err(|_| EchecSonde::new(Cause::Delai, "téléchargement de la tête du flux"))??;
    if octets.is_empty() {
        return Err(EchecSonde::new(Cause::Http, "le flux est vide (0 octet)"));
    }
    let lus = octets.len() as u64;
    let fichier = tempfile::Builder::new()
        .prefix("tune-sentinelle-")
        .suffix(&format!(".{extension}"))
        .tempfile()
        .map_err(|e| EchecSonde::new(Cause::Resolution, format!("fichier temporaire : {e}")))?;
    std::fs::write(fichier.path(), &octets)
        .map_err(|e| EchecSonde::new(Cause::Resolution, format!("écriture temporaire : {e}")))?;
    let mut decode = decoder_le_fichier(fichier.path().to_string_lossy().into_owned()).await?;
    decode.octets_lus = lus;
    drop(fichier);
    Ok(decode)
}

async fn decoder_le_fichier(chemin: String) -> Result<FluxDecode, EchecSonde> {
    let resultat = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm(&chemin, None, None, 0.0, SECONDES_DECODEES)
    })
    .await
    .map_err(|e| EchecSonde::new(Cause::Decodage, format!("décodeur interrompu : {e}")))?;
    let decode = resultat.map_err(|e| EchecSonde::new(Cause::Decodage, e))?;
    if decode.samples_i32.is_empty() || decode.sample_rate == 0 || decode.channels == 0 {
        return Err(EchecSonde::new(
            Cause::Decodage,
            "le décodeur n'a rendu aucun échantillon",
        ));
    }
    Ok(FluxDecode {
        sample_rate: decode.sample_rate,
        channels: decode.channels,
        echantillons: decode.samples_i32.len(),
        octets_lus: 0,
    })
}

/// Le DASH assemblé par `get_track_url` pour la sentinelle est supprimé : la
/// lecture le fait après consommation, et un fichier laissé là ferait servir
/// le prochain tour par le cache d'URL de TIDAL, sans parler au CDN. Seul un
/// fichier au nom que lui donne TIDAL (`tune-dash-<uuid>.mp4`) est supprimé.
fn liberer_le_fichier_dash(flux: &StreamUrl) {
    if let Some(chemin) = flux.url.strip_prefix("file://") {
        let p = std::path::Path::new(chemin);
        let du_dash = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("tune-dash-") && n.ends_with(".mp4"));
        if du_dash {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// La résolution d'un service par le chemin de la lecture. Seule étape qui
/// tient le verrou du service : le téléchargement et le décodage se font
/// après, sans bloquer une lecture qui démarrerait.
pub async fn resoudre_le_flux(
    service: &mut dyn crate::streaming::StreamingService,
    nom: &str,
    piste: &str,
) -> Result<StreamUrl, EchecSonde> {
    if !service.auth_status().await.authenticated {
        return Err(EchecSonde::new(
            Cause::Authentification,
            format!("{nom} n'est plus connecté"),
        ));
    }
    let flux = tokio::time::timeout(
        DELAI_RESOLUTION,
        super::request_stream_at_quality(service, piste, None),
    )
    .await
    .map_err(|_| EchecSonde::new(Cause::Delai, "résolution du flux"))?
    .map_err(|e| EchecSonde::new(cause_de_resolution(&e), e))?;
    let retenu = crate::streaming::qobuz_repli_qualite::retenir_le_flux_du_service(
        nom,
        &*service,
        crate::http::client::shared(),
        piste,
        None,
        flux,
    )
    .await;
    Ok(retenu.flux)
}

/// Le décodage d'un flux résolu, puis la libération de son DASH.
pub async fn decoder_le_flux_resolu(
    flux: StreamUrl,
) -> Result<(StreamUrl, FluxDecode), EchecSonde> {
    let decode = sonder_un_flux(&flux).await;
    liberer_le_fichier_dash(&flux);
    decode.map(|d| (flux, d))
}

/// La résolution puis le décodage d'un service : la forme d'un seul tenant,
/// pour un service factice.
pub async fn sonder_le_service(
    service: &mut dyn crate::streaming::StreamingService,
    nom: &str,
    piste: &str,
) -> Result<(StreamUrl, FluxDecode), EchecSonde> {
    let flux = resoudre_le_flux(service, nom, piste).await?;
    decoder_le_flux_resolu(flux).await
}

/// Les transitions d'un tour qui compte, face au dernier tour qui a compté.
pub fn transitions(
    precedent: Option<&RapportSentinelle>,
    courant: &[ResultatSonde],
) -> Vec<Transition> {
    courant
        .iter()
        .filter_map(|r| {
            let avant = precedent.and_then(|p| {
                p.resultats
                    .iter()
                    .find(|q| q.service == r.service && q.nature == r.nature)
                    .map(|q| q.etat)
            });
            (avant != Some(r.etat)).then(|| Transition {
                service: format!("{}:{}", r.nature, r.service),
                avant,
                apres: r.etat,
            })
        })
        .collect()
}

pub fn lire_l_historique(settings: &SettingsRepo) -> Vec<RapportSentinelle> {
    settings
        .get(CLE_HISTORIQUE)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str(&brut).ok())
        .unwrap_or_default()
}

fn ecrire_l_historique(settings: &SettingsRepo, historique: &[RapportSentinelle]) {
    let garde = &historique[..historique.len().min(TOURS_GARDES)];
    match serde_json::to_string(garde) {
        Ok(brut) => {
            if let Err(e) = settings.set(CLE_HISTORIQUE, &brut) {
                warn!(error = %e, "sentinelle_streaming_historique_non_ecrit");
            }
        }
        Err(e) => warn!(error = %e, "sentinelle_streaming_historique_non_serialise"),
    }
}

/// Un seul tour à la fois.
static UN_TOUR_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl PlaybackOrchestrator {
    /// La zone qui joue (ou se prépare à jouer), s'il y en a une.
    async fn zone_qui_joue(&self) -> Option<i64> {
        if crate::taches_de_fond::priorite::lecture_en_cours() {
            return Some(-1);
        }
        self.playback
            .all_states()
            .await
            .into_iter()
            .find(|z| z.state == crate::playback::PlayState::Playing || z.resolving)
            .map(|z| z.zone_id)
    }

    /// Un tour de sentinelle (voir l'en-tête du module). Historisé, sauf
    /// s'il est passé ou interrompu.
    pub async fn sentinelle_streaming(&self) -> RapportSentinelle {
        let Ok(_tour) = UN_TOUR_A_LA_FOIS.try_lock() else {
            return RapportSentinelle::passe("un tour est déjà en cours");
        };
        if let Some(zone) = self.zone_qui_joue().await {
            info!(zone, "sentinelle_streaming_tour_passe_une_zone_joue");
            return RapportSentinelle::passe(format!(
                "une zone joue (zone {zone}) : la sentinelle ne gêne pas la lecture"
            ));
        }
        let settings = SettingsRepo::with_backend(self.db.clone());
        let mut config = ConfigSentinelle::lire(&settings);
        let debut_du_tour = maintenant();
        let mut resultats = Vec::new();

        let noms = self.services.lock().await.list();
        for nom in noms {
            if config.exclus.iter().any(|e| e.eq_ignore_ascii_case(&nom)) {
                continue;
            }
            let Some(svc) = self.services.lock().await.get(&nom) else {
                continue;
            };
            if !svc.read().await.utilisable().await {
                continue;
            }
            if let Some((_, motif)) = NON_COUVERTS.iter().find(|(n, _)| *n == nom) {
                resultats.push(ResultatSonde {
                    service: nom.clone(),
                    nature: "service".into(),
                    reference: None,
                    etat: Etat::NonCouvert,
                    cause: None,
                    detail: Some((*motif).to_string()),
                    codec: None,
                    sample_rate: None,
                    channels: None,
                    octets_lus: None,
                    duree_ms: 0,
                });
                continue;
            }
            if let Some(zone) = self.zone_qui_joue().await {
                return RapportSentinelle::passe(format!(
                    "tour interrompu : une zone s'est mise à jouer (zone {zone})"
                ));
            }
            let chrono = Instant::now();
            let reference = match config.references.get(&nom).cloned() {
                Some(r) => Some(r),
                None => {
                    let requete = config
                        .requete_auto
                        .clone()
                        .unwrap_or_else(|| REQUETE_PAR_DEFAUT.to_string());
                    let choisie = svc
                        .read()
                        .await
                        .search(&requete, 5)
                        .await
                        .ok()
                        .and_then(|r| r.tracks.into_iter().next())
                        .map(|t| t.id);
                    if let Some(id) = &choisie {
                        config.references.insert(nom.clone(), id.clone());
                        if let Err(e) = config.ecrire(&settings) {
                            warn!(service = %nom, error = %e, "sentinelle_streaming_reference_non_ecrite");
                        }
                        info!(service = %nom, reference = %id, "sentinelle_streaming_reference_choisie");
                    }
                    choisie
                }
            };
            let resultat = match reference.as_deref() {
                None => Err(EchecSonde::new(
                    Cause::Reference,
                    format!(
                        "aucune piste de référence pour {nom} : la poser par PUT /api/v1/sentinelle/streaming/config"
                    ),
                )),
                Some(piste) => {
                    let flux = {
                        let mut garde = svc.write().await;
                        resoudre_le_flux(&mut **garde, &nom, piste).await
                    };
                    match flux {
                        Ok(flux) => decoder_le_flux_resolu(flux).await,
                        Err(e) => Err(e),
                    }
                }
            };
            resultats.push(resultat_en_ligne(
                nom.clone(),
                "service",
                reference,
                resultat.map(|(flux, d)| (Some(flux.quality.codec), d)),
                chrono,
            ));
        }

        if let Some(radio) = config.radio_effective() {
            if let Some(zone) = self.zone_qui_joue().await {
                return RapportSentinelle::passe(format!(
                    "tour interrompu : une zone s'est mise à jouer (zone {zone})"
                ));
            }
            let chrono = Instant::now();
            let adresse = self
                .resolve_playlist_url(&radio)
                .await
                .unwrap_or(radio.clone());
            let sonde = tokio::time::timeout(
                DELAI_RADIO,
                tokio::task::spawn_blocking(move || {
                    super::radio::sonder_une_station_sans_sortie(&adresse)
                }),
            )
            .await;
            let resultat = match sonde {
                Err(_) => Err(EchecSonde::new(Cause::Delai, "sonde de la radio")),
                Ok(Err(e)) => Err(EchecSonde::new(Cause::Decodage, e.to_string())),
                Ok(Ok(Err(e))) => {
                    let cause = if e.contains("HTTP") || e.contains("radio_not_audio") {
                        Cause::Http
                    } else {
                        Cause::Decodage
                    };
                    Err(EchecSonde::new(cause, e))
                }
                Ok(Ok(Ok((cadence, canaux)))) => Ok((
                    None,
                    FluxDecode {
                        sample_rate: cadence,
                        channels: u32::from(canaux),
                        echantillons: 0,
                        octets_lus: 0,
                    },
                )),
            };
            resultats.push(resultat_en_ligne(
                "radio".into(),
                "radio",
                Some(radio),
                resultat,
                chrono,
            ));
        }

        let mut historique = lire_l_historique(&settings);
        let precedent = historique.iter().find(|r| r.statut != "passe");
        let statut = RapportSentinelle::statut_des_resultats(&resultats);
        let rapport = RapportSentinelle {
            horodatage: debut_du_tour,
            statut: statut.into(),
            raison: None,
            transitions: transitions(precedent, &resultats),
            resultats,
        };
        for r in &rapport.resultats {
            if r.etat == Etat::Echec {
                warn!(
                    service = %r.service,
                    nature = %r.nature,
                    cause = ?r.cause,
                    detail = r.detail.as_deref().unwrap_or(""),
                    "sentinelle_streaming_echec"
                );
            }
        }
        info!(
            statut = %rapport.statut,
            sondes = rapport.resultats.len(),
            transitions = rapport.transitions.len(),
            "sentinelle_streaming_tour"
        );
        historique.insert(0, rapport.clone());
        ecrire_l_historique(&settings, &historique);
        rapport
    }
}

fn resultat_en_ligne(
    service: String,
    nature: &str,
    reference: Option<String>,
    resultat: Result<(Option<String>, FluxDecode), EchecSonde>,
    chrono: Instant,
) -> ResultatSonde {
    let duree_ms = chrono.elapsed().as_millis() as u64;
    match resultat {
        Ok((codec, d)) => ResultatSonde {
            service,
            nature: nature.into(),
            reference,
            etat: Etat::Ok,
            cause: None,
            detail: None,
            codec,
            sample_rate: Some(d.sample_rate),
            channels: Some(d.channels),
            octets_lus: (d.octets_lus > 0).then_some(d.octets_lus),
            duree_ms,
        },
        Err(e) => ResultatSonde {
            service,
            nature: nature.into(),
            reference,
            etat: Etat::Echec,
            cause: Some(e.cause),
            detail: Some(e.detail),
            codec: None,
            sample_rate: None,
            channels: None,
            octets_lus: None,
            duree_ms,
        },
    }
}

#[cfg(test)]
#[path = "sentinelle_streaming_tests.rs"]
mod tests;
