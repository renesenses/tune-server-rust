//! Go du 13/11 (rc12) — l'audio d'une piste TIDAL pour une ANALYSE (forme
//! d'onde, spectrogramme), sans rien télécharger.
//!
//! `get_track_url` assemble le fMP4 DASH entier sous le garde du service ;
//! or la lecture prend le service en écriture : une analyse qui passait par
//! là faisait attendre toute lecture TIDAL jusqu'à la fin de son
//! téléchargement (relecture de #8739). Ici, seules les ADRESSES sont
//! résolues — manifeste v2, puis v1, comme la lecture — et l'appelant
//! télécharge hors du garde, avec son plafond et son annulation.
//!
//! Rien n'est écrit dans le cache d'adresses du service : une lecture ne
//! retrouve jamais un fichier d'analyse, et l'analyse ne prend jamais le
//! fichier (peut-être encore en croissance) d'une lecture. Aucun compte
//! rendu de lecture n'est envoyé.

use super::*;

/// Où lire l'audio d'une piste pour l'analyser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FluxPourAnalyse {
    /// Un seul fichier.
    Adresse { url: String, mime_type: String },
    /// Un DASH à plusieurs segments : l'initialisation, puis les segments
    /// dans l'ordre ; leur réunion est le fichier.
    Segments {
        init: String,
        segments: Vec<String>,
        mime_type: String,
    },
}

impl TidalService {
    /// Les adresses de l'audio de `track_id` à la qualité `quality` (celle
    /// du service sinon) : la v2 `trackManifests` d'abord, puis la cascade
    /// v1, jusqu'à la première réponse qui porte un manifeste.
    pub async fn flux_pour_analyse(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<FluxPourAnalyse, String> {
        let demandee = quality.unwrap_or(self.quality.as_str());
        let mut derniere = String::from("tidal: no usable stream found");
        let mut donnees = match self.manifeste_v2(track_id, demandee).await {
            Ok(d) if d["manifest"].as_str().is_some() => Some(d),
            Ok(_) => None,
            Err(e) => {
                derniere = e;
                None
            }
        };
        let cascade: &[&str] = match demandee {
            "HI_RES_LOSSLESS" => &["HI_RES_LOSSLESS", "LOSSLESS", "HIGH"],
            "HI_RES" => &["HI_RES", "LOSSLESS", "HIGH"],
            "LOSSLESS" => &["LOSSLESS", "HIGH"],
            _ => &["HIGH"],
        };
        for &q in cascade {
            if donnees.is_some() {
                break;
            }
            match self.fetch_playback_info(track_id, q).await {
                Ok(d) if d["manifest"].as_str().is_some() => donnees = Some(d),
                Ok(_) => {}
                Err(e) => derniere = e,
            }
        }
        let donnees = donnees.ok_or_else(|| {
            format!("tidal: no usable stream found for track {track_id} ({derniere})")
        })?;
        flux_du_manifeste(&donnees)
    }
}

/// Une réponse `playbackinfo` (ou v2 ramenée à sa forme) lue en adresses.
fn flux_du_manifeste(donnees: &serde_json::Value) -> Result<FluxPourAnalyse, String> {
    let manifeste = donnees["manifest"].as_str().ok_or("no manifest")?;
    let type_du_manifeste = donnees["manifestMimeType"].as_str().unwrap_or("");
    let texte = String::from_utf8(base64_decode(manifeste)?).map_err(|e| format!("utf8: {e}"))?;
    if type_du_manifeste == "application/dash+xml" {
        let dash = parse_dash_manifest(&texte).ok_or("tidal: could not parse DASH manifest")?;
        let mime_type = dash.mime_type.clone().unwrap_or_else(|| "audio/mp4".into());
        return Ok(match dash.segments {
            Some(ref s) => FluxPourAnalyse::Segments {
                init: s.init_url.clone(),
                segments: (0..s.segment_count)
                    .map(|i| url_du_segment(s, s.start_number + i))
                    .collect(),
                mime_type,
            },
            None => FluxPourAnalyse::Adresse {
                url: dash.url,
                mime_type,
            },
        });
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&texte) {
        let url = json["urls"]
            .as_array()
            .and_then(|u| u.first())
            .and_then(|u| u.as_str())
            .ok_or("no url in manifest")?
            .to_string();
        let (_, mime) = codec_et_mime_du_manifeste_bts(&json);
        return Ok(FluxPourAnalyse::Adresse {
            url,
            mime_type: mime.unwrap_or_else(|| "audio/flac".into()),
        });
    }
    Ok(FluxPourAnalyse::Adresse {
        url: texte,
        mime_type: "audio/flac".into(),
    })
}
