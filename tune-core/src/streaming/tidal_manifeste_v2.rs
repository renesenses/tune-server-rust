//! #6334 — la lecture TIDAL par `openapi.tidal.com/v2/trackManifests/{id}`.
//!
//! Avec le client de Tune (`fX2JxdmntZWK0ixT`, connexion par code appareil),
//! l'ancien `v1/tracks/{id}/playbackinfopostpaywall` rend `HIGH` (AAC 320)
//! pour des titres que TIDAL sert en FLAC 16/44,1 (fils 2265, 2381, 2399,
//! 2408, 2409). D'après EbbLabs/python-tidal#404, la v2 `trackManifests`,
//! celle du lecteur web, rend le FLAC pour ces mêmes titres.
//!
//! Ce module demande d'abord la v2 ; tout échec (statut, extrait, DRM,
//! manifeste illisible) laisse la cascade v1 d'avant faire son travail,
//! inchangée. La connexion (PKCE, code appareil) n'est pas touchée : c'est le
//! même jeton.
//!
//! La réponse v2 est un document JSON:API dont `data.attributes.uri` porte le
//! manifeste MPEG-DASH en URI `data:` (`uriScheme=DATA`). Elle est ramenée à
//! la forme d'une réponse v1 (`audioQuality`, `manifestMimeType`, `manifest`
//! en base64, `bitDepth`, `sampleRate`), pour que la suite du chemin — lecture
//! du manifeste DASH, téléchargement des segments, codec lu dans le manifeste
//! (#6605) — reste une seule et même voie. `voie` dit d'où vient la réponse.
//!
//! Le FORMAT RÉELLEMENT REÇU fait foi : le codec est celui que nomme le MPD
//! (`flac`, `mp4a.40.2`), jamais celui qu'on a demandé. Un AAC reçu sur une
//! demande FLAC est donc annoncé AAC (pas d'étiquette CD sur de l'AAC).
use base64::Engine as _;

use super::*;

/// Racine de l'API v2 de TIDAL.
pub(super) const OPENAPI_BASE: &str = "https://openapi.tidal.com/v2";

/// La valeur de `voie` d'une réponse v2 ramenée à la forme v1.
pub(super) const VOIE_V2: &str = "trackManifests_v2";
/// La valeur de `voie` d'une réponse de l'ancien point d'accès.
pub(super) const VOIE_V1: &str = "playbackinfopostpaywall_v1";

/// Les formats v2 à demander, un par requête et dans l'ordre, pour une
/// qualité Tune. Un seul format par requête : la profondeur annoncée en
/// découle sans ambiguïté (`FLAC_HIRES` = 24 bits, `FLAC` = 16 bits), et le
/// manifeste n'a qu'une représentation. AAC n'est pas demandé sur une demande
/// sans perte : si la v2 ne rend pas de FLAC, la cascade v1 décide, comme
/// avant.
pub(super) fn formats_demandes(qualite: &str) -> &'static [&'static str] {
    match qualite {
        "HI_RES_LOSSLESS" | "HI_RES" => &["FLAC_HIRES", "FLAC"],
        "LOSSLESS" => &["FLAC"],
        "LOW" => &["HEAACV1"],
        _ => &["AACLC"],
    }
}

/// `audioSamplingRate` de la première représentation du MPD, s'il y en a un.
fn cadence_du_mpd(mpd: &str) -> Option<u64> {
    let debut = mpd.find("audioSamplingRate=\"")? + "audioSamplingRate=\"".len();
    let fin = mpd[debut..].find('"')?;
    mpd[debut..debut + fin]
        .trim()
        .parse()
        .ok()
        .filter(|v| *v > 0)
}

/// Ramène un document `trackManifests` à la forme d'une réponse v1, ou dit
/// pourquoi il ne peut pas servir (et la v1 prend le relais).
pub(super) fn interpreter_manifeste_v2(
    document: &serde_json::Value,
    format_demande: &str,
) -> Result<serde_json::Value, String> {
    let attributs = &document["data"]["attributes"];
    if !attributs.is_object() {
        return Err("réponse v2 sans data.attributes".into());
    }
    // Un extrait de 30 s n'est pas une lecture : la v1 dira mieux.
    if attributs["trackPresentation"].as_str() == Some("PREVIEW")
        || attributs["previewReason"].as_str().is_some()
    {
        return Err(format!(
            "v2 : extrait seulement ({})",
            attributs["previewReason"].as_str().unwrap_or("PREVIEW")
        ));
    }
    // Un flux chiffré, Tune ne sait pas le lire.
    if attributs["drmData"]
        .as_object()
        .is_some_and(|o| !o.is_empty())
    {
        return Err("v2 : flux chiffré (drmData)".into());
    }
    let uri = attributs["uri"]
        .as_str()
        .ok_or("v2 : pas d'uri de manifeste")?;
    let (entete, charge) = uri
        .strip_prefix("data:")
        .and_then(|r| r.split_once(','))
        .ok_or("v2 : uri de manifeste non `data:`")?;
    if !entete.ends_with(";base64") {
        return Err(format!("v2 : uri `data:` non base64 ({entete})"));
    }
    let mpd = base64::engine::general_purpose::STANDARD
        .decode(charge.trim())
        .map_err(|e| format!("v2 : base64 du manifeste : {e}"))?;
    let mpd = String::from_utf8(mpd).map_err(|e| format!("v2 : manifeste non UTF-8 : {e}"))?;
    let dash = parse_dash_manifest(&mpd).ok_or("v2 : manifeste DASH illisible")?;
    let codec = dash.codec.as_deref().unwrap_or("").to_ascii_lowercase();
    let formats: Vec<String> = attributs["formats"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let sample_rate = cadence_du_mpd(&mpd);

    let mut reponse = json!({
        "manifestMimeType": "application/dash+xml",
        "manifest": base64::engine::general_purpose::STANDARD.encode(mpd.as_bytes()),
        "voie": VOIE_V2,
        "formatsV2": formats,
    });
    if codec == "flac" || codec.starts_with("fla") {
        let hires = formats.iter().any(|f| f == "FLAC_HIRES")
            || (formats.is_empty() && format_demande == "FLAC_HIRES");
        let (qualite, profondeur) = if hires {
            ("HI_RES_LOSSLESS", 24)
        } else {
            ("LOSSLESS", 16)
        };
        reponse["audioQuality"] = json!(qualite);
        reponse["bitDepth"] = json!(profondeur);
        reponse["sampleRate"] = json!(sample_rate.unwrap_or(44_100));
    } else if codec.starts_with("mp4a") || codec.starts_with("aac") {
        let qualite = if codec == "mp4a.40.5" || formats.iter().any(|f| f == "HEAACV1") {
            "LOW"
        } else {
            "HIGH"
        };
        reponse["audioQuality"] = json!(qualite);
        if let Some(s) = sample_rate {
            reponse["sampleRate"] = json!(s);
        }
    } else {
        return Err(format!("v2 : codec inconnu dans le manifeste ({codec})"));
    }
    Ok(reponse)
}

impl TidalService {
    /// La racine v2 : celle des essais quand une base est forcée (aucun appel
    /// réel depuis un banc), sinon `openapi.tidal.com/v2`.
    pub(super) fn openapi_base(&self) -> &str {
        self.base_forcee.as_deref().unwrap_or(OPENAPI_BASE)
    }

    /// Un `GET trackManifests/{id}` pour UN format. Rend la réponse ramenée à
    /// la forme v1, ou l'erreur qui fait passer la main à la v1.
    async fn manifeste_v2_pour(
        &self,
        track_id: &str,
        format: &str,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}/trackManifests/{track_id}", self.openapi_base());
        let params = [
            ("manifestType", "MPEG_DASH"),
            ("formats", format),
            ("uriScheme", "DATA"),
            ("usage", "PLAYBACK"),
            ("adaptive", "false"),
        ];
        let mut rafraichi = false;
        loop {
            let jeton = self.get_access_token().await?;
            let reponse = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {jeton}"))
                .header("Accept", "application/vnd.api+json")
                .query(&params)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("v2 : {e}"))?;
            let statut = reponse.status().as_u16();
            if statut == 401 && !rafraichi {
                rafraichi = true;
                match self.do_refresh_token().await {
                    Ok(true) => continue,
                    _ => return Err("v2 : 401, jeton non rafraîchi".into()),
                }
            }
            let corps = reponse
                .text()
                .await
                .map_err(|e| format!("v2 : lecture : {e}"))?;
            if statut != 200 {
                // Le corps d'erreur de TIDAL, tronqué : jamais l'en-tête ni
                // le jeton.
                let extrait: String = corps.chars().take(300).collect();
                return Err(format!("v2 : statut {statut} : {extrait}"));
            }
            let document: serde_json::Value =
                serde_json::from_str(&corps).map_err(|e| format!("v2 : JSON : {e}"))?;
            return interpreter_manifeste_v2(&document, format);
        }
    }

    /// La v2 pour une qualité Tune : les formats de [`formats_demandes`],
    /// dans l'ordre, le premier qui sert l'emporte. `Err` si aucun ne sert :
    /// l'appelant passe alors à la cascade v1.
    pub(super) async fn manifeste_v2(
        &self,
        track_id: &str,
        qualite: &str,
    ) -> Result<serde_json::Value, String> {
        let mut derniere = String::from("v2 : aucun format demandé");
        for &format in formats_demandes(qualite) {
            match self.manifeste_v2_pour(track_id, format).await {
                Ok(reponse) => {
                    info!(
                        track_id,
                        requested = qualite,
                        format_demande = format,
                        formats_recus = %reponse["formatsV2"],
                        returned = reponse["audioQuality"].as_str().unwrap_or("?"),
                        returned_codec = ?codec_annonce_par_le_manifeste(&reponse),
                        bit_depth = ?reponse["bitDepth"].as_u64(),
                        sample_rate = ?reponse["sampleRate"].as_u64(),
                        "tidal_manifeste_v2"
                    );
                    return Ok(reponse);
                }
                Err(e) => {
                    info!(
                        track_id,
                        requested = qualite,
                        format_demande = format,
                        error = %e,
                        "tidal_manifeste_v2_refuse"
                    );
                    derniere = e;
                }
            }
        }
        Err(derniere)
    }
}
