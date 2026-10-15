//! #6401 — repli Qobuz automatique en 16/44,1 quand un flux haute résolution
//! échoue au démarrage.
//!
//! Certains titres Qobuz en haute résolution échouent côté CDN (problème
//! connu de Qobuz, vu aussi chez Roon) : `getFileUrl` rend bien une adresse,
//! mais le CDN répond une erreur HTTP, ou coupe le flux avant le premier
//! octet. Le contournement manuel était de brider TOUTE la qualité Qobuz à
//! 16/44,1 (Didier Gross, fil 2266).
//!
//! Ici, et pour CETTE piste seulement :
//!
//! 1. si la qualité demandée est au-dessus du CD (format 27 ou 7) et que le
//!    service n'a pas déjà rendu du 16/44,1, le début du flux HR est sondé ;
//! 2. si la sonde échoue (statut HTTP d'échec, connexion refusée, flux vide ou
//!    coupé avant le premier octet), la même piste est redemandée UNE fois au
//!    format 6 (16/44,1), et `qobuz_repli_qualite` est écrit au journal avec
//!    le `track_id`, le format demandé, l'erreur et le format obtenu ;
//! 3. si le format 6 échoue aussi (`qobuz_repli_qualite_echec`), le flux
//!    d'origine est rendu tel quel et la lecture échoue comme elle échouait
//!    avant ce repli — aucune nouvelle tentative, aucune boucle.
//!
//! Le réglage de qualité de la zone n'est pas touché : la piste suivante est
//! de nouveau demandée à la qualité réglée.
//!
//! L'échec à l'obtention de l'URL (`getFileUrl` en erreur) est, lui, déjà
//! couvert par l'échelle de `QobuzService::get_track_url` (27 → 7 → 6), qui
//! écrit la même ligne `qobuz_repli_qualite`.

use std::time::Duration;

use futures_util::StreamExt;
use tracing::{info, warn};

use super::traits::{StreamQuality, StreamUrl, StreamingService};

/// Le format Qobuz du repli : FLAC 16 bits / 44,1 kHz.
pub const FORMAT_DE_REPLI: &str = "6";

/// Le jeton de qualité qui fait demander [`FORMAT_DE_REPLI`] à
/// `QobuzService::get_track_url`.
pub const JETON_DE_REPLI: &str = "cd";

/// Le temps laissé au CDN pour rendre son premier octet. Au-delà, le flux est
/// tenu pour coupé au démarrage.
const DELAI_DE_SONDE: Duration = Duration::from_secs(10);

/// Le format Qobuz que `get_track_url` demandera pour ce jeton de qualité —
/// le même tableau que dans `QobuzService::get_track_url`.
pub fn format_demande(jeton: Option<&str>) -> &'static str {
    match jeton {
        Some("hires") => "27",
        Some("cd") => "6",
        Some("mp3") => "5",
        _ => "27",
    }
}

/// Le format HR demandé quand un repli est possible, `None` sinon.
///
/// Pas de repli quand la qualité demandée est déjà 16/44,1 ou moins, ni quand
/// le service a DÉJÀ rendu du 16/44,1 (son échelle est descendue jusqu'au
/// CD) : redemander le format 6 serait une seconde tentative du même flux.
/// Une cadence ou une profondeur rendue à 0 (inconnue, #5283) ne prouve pas
/// le CD : le repli reste possible.
pub fn repli_possible(jeton: Option<&str>, obtenu: &StreamQuality) -> Option<&'static str> {
    let format = format_demande(jeton);
    if !matches!(format, "27" | "7") {
        return None;
    }
    let deja_en_cd = obtenu.sample_rate > 0
        && obtenu.bit_depth > 0
        && obtenu.sample_rate <= 44_100
        && obtenu.bit_depth <= 16;
    if deja_en_cd {
        return None;
    }
    Some(format)
}

/// Sonde le début du flux : un statut de succès ET au moins un octet.
///
/// `Range: bytes=0-`, comme le premier GET d'un renderer. La connexion est
/// lâchée dès le premier bloc reçu : rien n'est téléchargé au-delà.
pub async fn sonder_le_debut(client: &reqwest::Client, url: &str) -> Result<(), String> {
    let sonde = async {
        let reponse = client
            .get(url)
            .header("Range", "bytes=0-")
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .map_err(|e| format!("connexion au flux : {e}"))?;
        let statut = reponse.status();
        if !statut.is_success() {
            return Err(format!("HTTP {}", statut.as_u16()));
        }
        let mut corps = reponse.bytes_stream();
        loop {
            match corps.next().await {
                Some(Ok(bloc)) if bloc.is_empty() => continue,
                Some(Ok(_)) => return Ok(()),
                Some(Err(e)) => return Err(format!("flux coupé au démarrage : {e}")),
                None => return Err("flux vide au démarrage".to_string()),
            }
        }
    };
    match tokio::time::timeout(DELAI_DE_SONDE, sonde).await {
        Ok(issue) => issue,
        Err(_) => Err(format!(
            "aucun octet en {} s au démarrage",
            DELAI_DE_SONDE.as_secs()
        )),
    }
}

/// Le flux à jouer après la sonde, et le jeton de qualité que toute
/// re-résolution ultérieure de CETTE piste doit employer.
#[derive(Debug)]
pub struct FluxRetenu {
    pub flux: StreamUrl,
    /// `Some("cd")` après un repli ; sinon le jeton reçu, inchangé.
    pub jeton: Option<String>,
    /// Vrai quand le repli a eu lieu.
    pub replie: bool,
}

/// Sonde le flux HR rendu par Qobuz et, s'il échoue au démarrage, redemande
/// UNE fois la même piste au format 6. Voir l'en-tête du module.
///
/// Le format 6 en échec aussi : le flux d'origine revient, inchangé, et
/// l'erreur ordinaire suit son cours — le repli ne rend jamais une lecture
/// pire qu'avant lui.
pub async fn retenir_le_flux(
    service: &dyn StreamingService,
    client: &reqwest::Client,
    track_id: &str,
    jeton: Option<&str>,
    flux: StreamUrl,
) -> FluxRetenu {
    let inchange = |flux| FluxRetenu {
        flux,
        jeton: jeton.map(str::to_string),
        replie: false,
    };
    // Un `file://` (fichier déjà assemblé) n'a pas de début à sonder.
    if !flux.url.starts_with("http") {
        return inchange(flux);
    }
    let Some(format_hr) = repli_possible(jeton, &flux.quality) else {
        return inchange(flux);
    };
    let erreur = match sonder_le_debut(client, &flux.url).await {
        Ok(()) => return inchange(flux),
        Err(e) => e,
    };

    // Un seul repli : le format 6, sondé une fois. Pas de nouvelle échelle.
    let echec = |erreur_repli: String| {
        warn!(
            track_id,
            format_demande = format_hr,
            erreur = %erreur,
            format_de_repli = FORMAT_DE_REPLI,
            erreur_repli = %erreur_repli,
            "qobuz_repli_qualite_echec"
        );
    };
    let repli = match service.get_track_url(track_id, Some(JETON_DE_REPLI)).await {
        Ok(repli) => repli,
        Err(e) => {
            echec(e.to_string());
            return inchange(flux);
        }
    };
    if let Err(e) = sonder_le_debut(client, &repli.url).await {
        echec(e);
        return inchange(flux);
    }
    info!(
        track_id,
        format_demande = format_hr,
        erreur = %erreur,
        format_obtenu = FORMAT_DE_REPLI,
        "qobuz_repli_qualite"
    );
    FluxRetenu {
        flux: repli,
        jeton: Some(JETON_DE_REPLI.to_string()),
        replie: true,
    }
}

#[cfg(test)]
#[path = "qobuz_repli_qualite_tests_6401.rs"]
mod tests;
