//! La forme d'onde (fil 2793) et le spectrogramme (#7857) d'une piste QOBUZ
//! ou TIDAL — go de Bertrand du 13/11/2026, rc12.
//!
//! Une piste de service n'a pas de fichier sur le disque. À la demande, et
//! seulement quand le résultat n'est pas déjà en cache :
//!
//! 1. les ADRESSES de l'audio sont résolues par le service, à sa qualité
//!    « Hi-Res », sous son garde de lecture : Qobuz par `get_track_url` (une
//!    URL d'un seul fichier), TIDAL par `TidalService::flux_pour_analyse`
//!    (une URL, ou l'initialisation et les segments d'un DASH — sans rien
//!    télécharger, et sans toucher au cache d'adresses de la lecture) ;
//! 2. le garde est RENDU, puis l'audio est téléchargé hors de lui (relecture
//!    de #8739 : une lecture, qui prend le service en écriture, n'attend
//!    jamais une analyse) dans un fichier temporaire sous le dossier du cache
//!    (`<cache>/telechargements/`), segments bout à bout, avec un plafond de
//!    taille vérifié en route ([`PLAFOND_DEFAUT_OCTETS`], ou
//!    `TUNE_ANALYSE_SERVICE_MO`) et un délai ([`DELAI_DEFAUT`], ou
//!    `TUNE_ANALYSE_SERVICE_DELAI_S`). Jamais le fichier d'un service (le
//!    fMP4 d'une lecture peut être encore en croissance) ; une réponse plus
//!    courte que sa longueur annoncée est un échec : un fichier incomplet
//!    n'est ni analysé ni mis en cache ;
//! 3. la forme d'onde ou l'image se calcule sur ce fichier, comme pour une
//!    piste locale (mêmes verrous, même sémaphore de deux calculs) ;
//! 4. le fichier audio est EFFACÉ ([`AudioTemporaire`] l'efface en partant,
//!    même sur une erreur ou une requête abandonnée). Seul le résultat du
//!    calcul est gardé en cache, sous une clé qui porte le service,
//!    l'identifiant de la piste chez lui et la qualité demandée.
//!
//! Ce n'est pas une écoute : aucun appel de compte rendu de lecture n'est
//! fait (Qobuz `reportStreamingStart/End`, journal de lecture TIDAL) : la
//! résolution ne fait que demander les adresses.
//!
//! Les refus se disent en 422 avec un code (`raison`) que l'écran traduit :
//! [`Refus`].

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::db::models::Track;
use tune_core::streaming::quality::StreamingQualityPreference;
use tune_core::streaming::tidal::analyse_service::FluxPourAnalyse;

use crate::state::AppState;

/// Les services dont une piste s'analyse par téléchargement.
pub(crate) const SERVICES_ANALYSABLES: &[&str] = &["qobuz", "tidal"];

/// Plafond d'un téléchargement : 400 Mo (une piste 24/192 de plus de
/// vingt minutes).
pub(crate) const PLAFOND_DEFAUT_OCTETS: u64 = 400 * 1024 * 1024;

/// Délai de la résolution et du téléchargement réunis.
pub(crate) const DELAI_DEFAUT: Duration = Duration::from_secs(180);

/// Un fichier temporaire plus vieux que ceci est un reste (arrêt brutal) :
/// il part au téléchargement suivant.
const AGE_D_UN_RESTE: Duration = Duration::from_secs(3600);

/// Deux téléchargements au plus en même temps, toutes pistes confondues : le
/// disque ne porte jamais plus de deux pistes en transit.
static TELECHARGEMENTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

/// Une piste de service analysable : le service, son identifiant chez lui et
/// la qualité demandée (le jeton du service pour « Hi-Res »).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PisteDeService {
    pub(crate) service: String,
    pub(crate) id_service: String,
    pub(crate) qualite: String,
}

impl PisteDeService {
    /// `None` : piste locale, service non pris en charge, ou piste sans
    /// identifiant de service.
    pub(crate) fn de(track: &Track) -> Option<Self> {
        let service = track.source.trim();
        if !SERVICES_ANALYSABLES.contains(&service) {
            return None;
        }
        let id = track
            .source_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let qualite = StreamingQualityPreference::Hires.service_token(service)?;
        Some(Self {
            service: service.to_string(),
            id_service: id.to_string(),
            qualite: qualite.to_string(),
        })
    }

    /// La part de la clé du cache qui remplace l'empreinte du fichier :
    /// `<service>-<empreinte du service, de l'identifiant et de la qualité>`.
    pub(crate) fn cle(&self) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.service.hash(&mut h);
        self.id_service.hash(&mut h);
        self.qualite.hash(&mut h);
        format!("{}-{:016x}", self.service, h.finish())
    }
}

/// Pourquoi une piste de service ne s'analyse pas. 422, avec `raison`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refus {
    /// Ni fichier local, ni service analysable (Spotify, YouTube…).
    NonPrisEnCharge,
    /// Piste d'un serveur multimédia (UPnP) : pas de fichier sur ce disque
    /// (recette du 14/11). Le texte reste celui d'avant pour un vieil écran.
    SourceReseau,
    /// Service désactivé dans les Réglages.
    ServiceInactif,
    /// Service sans session (jamais connecté, session expirée).
    ServiceNonConnecte,
    /// Le service ne diffuse pas cette piste (retirée, extrait seulement).
    NonDiffusable,
    /// Le service ne diffuse pas cette piste dans le pays du compte.
    Region,
    /// Le fichier dépasse le plafond de téléchargement.
    TropVolumineux,
    /// Résolution et téléchargement n'ont pas abouti dans le délai.
    DelaiDepasse,
    /// Tout autre échec de la résolution ou du téléchargement.
    TelechargementImpossible,
    /// Une plage (zoom) ou un autre nombre de colonnes : pour une piste de
    /// service, seule la piste entière au nombre par défaut se calcule (un
    /// zoom retéléchargerait la piste entière à chaque fois).
    ZoomNonDisponible,
}

impl Refus {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NonPrisEnCharge => "service_non_pris_en_charge",
            Self::SourceReseau => "source_reseau",
            Self::ServiceInactif => "service_inactif",
            Self::ServiceNonConnecte => "service_non_connecte",
            Self::NonDiffusable => "non_diffusable",
            Self::Region => "region",
            Self::TropVolumineux => "trop_volumineux",
            Self::DelaiDepasse => "delai_depasse",
            Self::TelechargementImpossible => "telechargement_impossible",
            Self::ZoomNonDisponible => "zoom_non_disponible",
        }
    }

    /// Le texte de `error`. « track has no local file » reste celui d'une
    /// piste sans service analysable : un écran d'avant le reconnaît.
    fn message(self) -> &'static str {
        match self {
            Self::NonPrisEnCharge | Self::SourceReseau => "track has no local file",
            Self::ServiceInactif => "streaming service disabled",
            Self::ServiceNonConnecte => "streaming service not connected",
            Self::NonDiffusable => "track not streamable",
            Self::Region => "track not available in this region",
            Self::TropVolumineux => "track too large to download for analysis",
            Self::DelaiDepasse => "download for analysis timed out",
            Self::TelechargementImpossible => "download for analysis failed",
            Self::ZoomNonDisponible => {
                "only the whole track at the default size is available for a streaming track"
            }
        }
    }

    pub(crate) fn reponse(self) -> Response {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": self.message(), "raison": self.code() })),
        )
            .into_response()
    }
}

/// Le classement d'un échec de `get_track_url`. `disponible` : ce que la
/// fiche de la piste chez le service en dit, quand elle a pu être lue.
pub(crate) fn classer(message: &str, disponible: Option<bool>) -> Refus {
    let m = message.to_lowercase();
    let contient = |mots: &[&str]| mots.iter().any(|x| m.contains(x));
    if contient(&[
        "session expired",
        "not authenticated",
        "reconnect",
        "token expired",
        "unauthorized",
    ]) {
        Refus::ServiceNonConnecte
    } else if contient(&["region", "country", "territor", "geo"]) {
        Refus::Region
    } else if disponible == Some(false)
        || contient(&[
            "sample",
            "not entitled",
            "streamable",
            "no usable stream",
            "not available",
        ])
    {
        Refus::NonDiffusable
    } else {
        Refus::TelechargementImpossible
    }
}

/// Les limites d'un téléchargement.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limites {
    pub(crate) plafond_octets: u64,
    pub(crate) delai: Duration,
}

impl Limites {
    pub(crate) fn de_l_environnement() -> Self {
        let lire = |cle: &str| {
            std::env::var(cle)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v > 0)
        };
        Self {
            plafond_octets: lire("TUNE_ANALYSE_SERVICE_MO")
                .map(|mo| mo * 1024 * 1024)
                .unwrap_or(PLAFOND_DEFAUT_OCTETS),
            delai: lire("TUNE_ANALYSE_SERVICE_DELAI_S")
                .map(Duration::from_secs)
                .unwrap_or(DELAI_DEFAUT),
        }
    }
}

/// Le dossier des fichiers en transit, sous le dossier d'un cache.
pub(crate) fn dossier_temporaire(cache: &Path) -> PathBuf {
    cache.join("telechargements")
}

/// Un fichier audio téléchargé pour une analyse : effacé quand il part, quoi
/// qu'il arrive (calcul fini, erreur, requête abandonnée).
#[derive(Debug)]
pub(crate) struct AudioTemporaire {
    chemin: PathBuf,
}

impl AudioTemporaire {
    pub(crate) fn chemin(&self) -> &Path {
        &self.chemin
    }
}

impl Drop for AudioTemporaire {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.chemin) {
            Ok(()) => {
                tracing::debug!(chemin = %self.chemin.display(), "analyse_service_audio_efface")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                chemin = %self.chemin.display(),
                error = %e,
                "analyse_service_audio_non_efface"
            ),
        }
    }
}

/// Résout et télécharge la piste `piste` dans `dossier_tmp`. Le fichier rendu
/// s'efface quand il part.
pub(crate) async fn telecharger(
    state: &AppState,
    piste: &PisteDeService,
    dossier_tmp: &Path,
    limites: Limites,
) -> Result<AudioTemporaire, Refus> {
    let _place = Arc::clone(&TELECHARGEMENTS)
        .acquire_owned()
        .await
        .map_err(|_| Refus::TelechargementImpossible)?;
    let debut = std::time::Instant::now();
    let issue = match tokio::time::timeout(
        limites.delai,
        telecharger_sans_delai(state, piste, dossier_tmp, limites.plafond_octets),
    )
    .await
    {
        Ok(issue) => issue,
        Err(_) => Err(Refus::DelaiDepasse),
    };
    tracing::info!(
        service = %piste.service,
        id_service = %piste.id_service,
        duree_ms = debut.elapsed().as_millis() as u64,
        octets = issue
            .as_ref()
            .ok()
            .and_then(|a| std::fs::metadata(a.chemin()).ok())
            .map(|m| m.len()),
        raison = issue.as_ref().err().map(|r| r.code()),
        "analyse_service_telechargement"
    );
    issue
}

async fn telecharger_sans_delai(
    state: &AppState,
    piste: &PisteDeService,
    dossier_tmp: &Path,
    plafond: u64,
) -> Result<AudioTemporaire, Refus> {
    nettoyer_les_restes(dossier_tmp);
    std::fs::create_dir_all(dossier_tmp).map_err(|e| {
        tracing::warn!(error = %e, "analyse_service_dossier_impossible");
        Refus::TelechargementImpossible
    })?;
    let flux = resoudre(state, piste).await?;
    // Le garde du service est RENDU : le téléchargement se fait hors de lui,
    // une lecture (qui prend le service en écriture) n'attend jamais une
    // analyse (relecture de #8739).
    match flux {
        FluxPourAnalyse::Adresse { url, .. } if !url.starts_with("http") => {
            // Jamais le fichier d'un service (un fMP4 de lecture, peut-être
            // encore en croissance) : seule une adresse réseau se télécharge.
            tracing::info!(service = %piste.service, "analyse_service_adresse_locale_refusee");
            Err(Refus::TelechargementImpossible)
        }
        FluxPourAnalyse::Adresse { url, mime_type } => {
            telecharger_http(&[url], &mime_type, dossier_tmp, plafond).await
        }
        FluxPourAnalyse::Segments {
            init,
            segments,
            mime_type,
        } => {
            let mut adresses = Vec::with_capacity(segments.len() + 1);
            adresses.push(init);
            adresses.extend(segments);
            telecharger_http(&adresses, &mime_type, dossier_tmp, plafond).await
        }
    }
}

/// Les adresses de l'audio, résolues sous le garde de LECTURE du service, que
/// la fonction rend en sortant : TIDAL par `flux_pour_analyse` (adresses
/// seulement, rien n'est téléchargé), les autres par `get_track_url`.
async fn resoudre(state: &AppState, piste: &PisteDeService) -> Result<FluxPourAnalyse, Refus> {
    let svc = state
        .services
        .lock()
        .await
        .get(&piste.service)
        .ok_or(Refus::NonPrisEnCharge)?;
    let service = svc.read().await;
    if !service.enabled() {
        return Err(Refus::ServiceInactif);
    }
    if !service.auth_status().await.authenticated {
        return Err(Refus::ServiceNonConnecte);
    }
    let tidal = service
        .as_any()
        .downcast_ref::<tune_core::streaming::tidal::TidalService>();
    let issue = match tidal {
        Some(t) => {
            t.flux_pour_analyse(&piste.id_service, Some(&piste.qualite))
                .await
        }
        None => service
            .get_track_url(&piste.id_service, Some(&piste.qualite))
            .await
            .map(|f| FluxPourAnalyse::Adresse {
                url: f.url,
                mime_type: f.mime_type,
            })
            .map_err(|e| e.to_string()),
    };
    match issue {
        Ok(flux) => Ok(flux),
        Err(message) => {
            let disponible = service
                .get_track(&piste.id_service)
                .await
                .ok()
                .and_then(|t| t.disponible);
            let refus = classer(&message, disponible);
            tracing::info!(
                service = %piste.service,
                id_service = %piste.id_service,
                error = %message,
                raison = refus.code(),
                "analyse_service_resolution_refusee"
            );
            Err(refus)
        }
    }
}

/// Télécharge `adresses` bout à bout dans un seul fichier (une adresse : un
/// fichier ; plusieurs : l'initialisation puis les segments d'un DASH). Le
/// plafond se vérifie pendant le téléchargement ; une adresse refusée, une
/// coupure ou le plafond font tout effacer : jamais de fichier partiel.
async fn telecharger_http(
    adresses: &[String],
    mime_type: &str,
    dossier: &Path,
    plafond: u64,
) -> Result<AudioTemporaire, Refus> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    if adresses.is_empty() {
        return Err(Refus::TelechargementImpossible);
    }
    let client = tune_core::http::client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| Refus::TelechargementImpossible)?;
    // Le fichier partiel est déjà sous garde : une erreur ou une requête
    // abandonnée l'efface.
    let mut audio = AudioTemporaire {
        chemin: dossier.join(format!("{}.part", uuid::Uuid::new_v4())),
    };
    let mut fichier = tokio::fs::File::create(&audio.chemin)
        .await
        .map_err(|_| Refus::TelechargementImpossible)?;
    let mut tete: Vec<u8> = Vec::with_capacity(16);
    let mut total: u64 = 0;
    for adresse in adresses {
        let reponse = client.get(adresse).send().await.map_err(|e| {
            tracing::info!(error = %e, "analyse_service_telechargement_refuse");
            Refus::TelechargementImpossible
        })?;
        let statut = reponse.status();
        if !statut.is_success() {
            tracing::info!(
                statut = statut.as_u16(),
                "analyse_service_telechargement_refuse"
            );
            return Err(match statut.as_u16() {
                451 => Refus::Region,
                _ => Refus::TelechargementImpossible,
            });
        }
        let annonce = reponse.content_length();
        if annonce.is_some_and(|n| total.saturating_add(n) > plafond) {
            return Err(Refus::TropVolumineux);
        }
        let avant = total;
        let mut morceaux = reponse.bytes_stream();
        while let Some(morceau) = morceaux.next().await {
            let morceau = morceau.map_err(|e| {
                tracing::info!(error = %e, "analyse_service_telechargement_coupe");
                Refus::TelechargementImpossible
            })?;
            total += morceau.len() as u64;
            if total > plafond {
                return Err(Refus::TropVolumineux);
            }
            if tete.len() < 16 {
                let n = (16 - tete.len()).min(morceau.len());
                tete.extend_from_slice(&morceau[..n]);
            }
            fichier
                .write_all(&morceau)
                .await
                .map_err(|_| Refus::TelechargementImpossible)?;
        }
        // Relecture de #8739 — une réponse plus courte que sa longueur
        // annoncée n'est pas la piste : rien de tronqué n'entre au cache.
        if annonce.is_some_and(|n| total - avant != n) {
            tracing::info!(
                annonce = ?annonce,
                recu = total - avant,
                "analyse_service_telechargement_incomplet"
            );
            return Err(Refus::TelechargementImpossible);
        }
    }
    fichier
        .flush()
        .await
        .map_err(|_| Refus::TelechargementImpossible)?;
    drop(fichier);
    if total == 0 {
        return Err(Refus::TelechargementImpossible);
    }
    let extension = extension_selon(&tete, Some(mime_type));
    let fin = audio.chemin.with_extension(extension);
    std::fs::rename(&audio.chemin, &fin).map_err(|_| Refus::TelechargementImpossible)?;
    audio.chemin = fin;
    Ok(audio)
}

/// L'extension d'un fichier d'après son contenu (le décodeur choisit son
/// lecteur par l'extension), à défaut d'après le type annoncé.
pub(crate) fn extension_selon(tete: &[u8], mime: Option<&str>) -> &'static str {
    if tete.starts_with(b"fLaC") {
        return "flac";
    }
    if tete.len() >= 12 && &tete[..4] == b"RIFF" && &tete[8..12] == b"WAVE" {
        return "wav";
    }
    if tete.len() >= 8 && matches!(&tete[4..8], b"ftyp" | b"styp" | b"moof" | b"moov") {
        return "mp4";
    }
    if tete.starts_with(b"OggS") {
        return "ogg";
    }
    if tete.starts_with(b"ID3") || (tete.len() >= 2 && tete[0] == 0xFF && tete[1] & 0xE0 == 0xE0) {
        return "mp3";
    }
    match mime.map(|m| m.to_ascii_lowercase()) {
        Some(m) if m.contains("flac") => "flac",
        Some(m) if m.contains("wav") => "wav",
        Some(m) if m.contains("mpeg") || m.contains("mp3") => "mp3",
        Some(m) if m.contains("ogg") => "ogg",
        _ => "mp4",
    }
}

/// Efface les fichiers de `dossier` laissés par un arrêt brutal.
fn nettoyer_les_restes(dossier: &Path) {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return;
    };
    let maintenant = SystemTime::now();
    for e in entrees.flatten() {
        let vieux = e
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|m| maintenant.duration_since(m).ok())
            .is_some_and(|age| age > AGE_D_UN_RESTE);
        if vieux {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
#[path = "audio_de_service_tests.rs"]
mod tests;
