//! #5283 — une qualité de service annoncée NULLE ne part jamais au décodeur.
//!
//! ## Le fait
//!
//! Fil forum 2000 (Didier, 27/09/2026) : sur une sortie locale, des pistes
//! Qobuz 24/192 de *Freak Out!* ne jouaient pas. Le journal :
//!
//! ```text
//! streaming_quality_resolved service="qobuz" ... delivered_sample_rate=0 delivered_bit_depth=0
//! streaming_transcode_to_wav_for_local_output service="qobuz" codec=flac sample_rate=0 bit_depth=32
//! streaming_transcode_decode_failed error=stream target sample rate must be greater than zero
//! ```
//!
//! `getFileUrl` rendait `sampling_rate: 0, bit_depth: 0` (le repli 44,1/16 de
//! `qobuz.rs` ne vaut que pour un champ ABSENT). La cadence cible du WAV était
//! reprise telle quelle, le décodeur refusait avant le premier octet, la
//! session restait vide et la sortie locale concluait à un « conteneur non
//! reconnu » — l'écran accusait le fichier.
//!
//! ## La règle
//!
//! Quand le service annonce une cadence (ou une profondeur) nulle, on la
//! remplace, dans cet ordre :
//! 1. par ce qu'énonce le STREAMINFO du flux lui-même (les premiers octets,
//!    une seule requête `Range`) — la source qui fait foi ;
//! 2. à défaut, par la qualité du catalogue du service (`get_track`).
//!
//! Si elle reste inconnue, la cadence reste à 0 et le bras local/OAAT refuse
//! la piste avec une erreur NOMMÉE ([`CODE_CADENCE_INCONNUE`]), sans ouvrir de
//! session ni lancer de transcodage à 0.

use tracing::{info, warn};

use super::{StreamQuality, StreamUrl, StreamingService};

/// Le code de l'erreur rendue quand aucune source ne donne la cadence.
pub const CODE_CADENCE_INCONNUE: &str = "streaming_sample_rate_unknown";

/// Octets lus en tête du flux : STREAMINFO tient dans les 42 premiers, la
/// marge couvre un serveur qui ignorerait `Range` sans rien coûter de plus.
const OCTETS_D_EN_TETE: usize = 8 * 1024;

/// L'erreur nommée d'une cadence restée inconnue.
pub fn erreur_cadence_inconnue(service: &str) -> String {
    format!(
        "{CODE_CADENCE_INCONNUE}: {service} n'a annoncé aucune fréquence d'échantillonnage \
         pour cette piste, et ni l'en-tête du flux ni le catalogue ne la donnent"
    )
}

/// La qualité annoncée est-elle incomplète (cadence ou profondeur nulle) ?
pub fn qualite_incomplete(q: &StreamQuality) -> bool {
    q.sample_rate == 0 || q.bit_depth == 0
}

/// D'où vient la cadence retenue — pour le journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrigineDeLaCadence {
    /// Le service l'avait annoncée : rien n'a été lu.
    Annoncee,
    EnTeteDuFlux,
    Catalogue,
    Inconnue,
}

/// Le cœur pur : complète une qualité nulle par l'en-tête lu, puis par le
/// catalogue. Ne touche à rien de ce que le service a annoncé non nul.
pub fn completer_la_qualite(
    qualite: &mut StreamQuality,
    en_tete: Option<&[u8]>,
    catalogue: Option<&StreamQuality>,
) -> OrigineDeLaCadence {
    if let Some((cadence, profondeur)) =
        en_tete.and_then(crate::audio::flac_vendeur::cadence_streaminfo)
    {
        if qualite.sample_rate == 0 {
            qualite.sample_rate = cadence;
        }
        if qualite.bit_depth == 0 {
            qualite.bit_depth = profondeur;
        }
        return OrigineDeLaCadence::EnTeteDuFlux;
    }
    if let Some(cat) = catalogue.filter(|c| c.sample_rate > 0) {
        if qualite.sample_rate == 0 {
            qualite.sample_rate = cat.sample_rate;
        }
        if qualite.bit_depth == 0 && cat.bit_depth > 0 {
            qualite.bit_depth = cat.bit_depth;
        }
        return OrigineDeLaCadence::Catalogue;
    }
    OrigineDeLaCadence::Inconnue
}

/// Les premiers octets du flux : fichier local (`file://`, DASH assemblé) ou
/// requête `Range` HTTP qui rejoue les en-têtes du résolveur. `None` sur
/// toute erreur : la décision retombe alors sur le catalogue.
pub async fn lire_le_debut_du_flux(url: &str, entetes: &[(String, String)]) -> Option<Vec<u8>> {
    if let Some(chemin) = url.strip_prefix("file://") {
        use std::io::Read;
        let mut f = std::fs::File::open(chemin).ok()?;
        let mut octets = vec![0u8; OCTETS_D_EN_TETE];
        let mut lus = 0;
        while lus < octets.len() {
            match f.read(&mut octets[lus..]) {
                Ok(0) => break,
                Ok(n) => lus += n,
                Err(_) => return None,
            }
        }
        octets.truncate(lus);
        return Some(octets);
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    let client = crate::http::client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let mut requete = client.get(url);
    for (nom, valeur) in entetes {
        if nom.eq_ignore_ascii_case("range") || nom.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        requete = requete.header(nom, valeur);
    }
    let mut reponse = requete
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(
            reqwest::header::RANGE,
            format!("bytes=0-{}", OCTETS_D_EN_TETE - 1),
        )
        .send()
        .await
        .ok()?;
    if !reponse.status().is_success() {
        return None;
    }
    // Un serveur qui ignore `Range` rend le fichier entier : on s'arrête à la
    // borne, la réponse est abandonnée.
    let mut octets = Vec::with_capacity(OCTETS_D_EN_TETE);
    while octets.len() < OCTETS_D_EN_TETE {
        match reponse.chunk().await {
            Ok(Some(c)) => octets.extend_from_slice(&c),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    octets.truncate(OCTETS_D_EN_TETE);
    Some(octets)
}

/// Complète la qualité d'un flux dont le service annonce une cadence ou une
/// profondeur nulle (voir l'en-tête du module). Sans effet — et sans aucune
/// requête — quand la qualité annoncée est complète.
pub async fn completer_la_cadence(
    flux: &mut StreamUrl,
    service: &dyn StreamingService,
    service_name: &str,
    source_id: &str,
) -> OrigineDeLaCadence {
    if !qualite_incomplete(&flux.quality) {
        return OrigineDeLaCadence::Annoncee;
    }
    let annonce = (flux.quality.sample_rate, flux.quality.bit_depth);
    let en_tete = lire_le_debut_du_flux(&flux.url, &flux.headers).await;
    let catalogue = if en_tete
        .as_deref()
        .and_then(crate::audio::flac_vendeur::cadence_streaminfo)
        .is_none()
    {
        service
            .get_track(source_id)
            .await
            .ok()
            .and_then(|t| t.quality)
    } else {
        None
    };
    let origine = completer_la_qualite(&mut flux.quality, en_tete.as_deref(), catalogue.as_ref());
    if origine == OrigineDeLaCadence::Inconnue {
        warn!(
            service = service_name,
            source_id, "streaming_sample_rate_unknown"
        );
    } else {
        info!(
            service = service_name,
            source_id,
            announced_sample_rate = annonce.0,
            announced_bit_depth = annonce.1,
            sample_rate = flux.quality.sample_rate,
            bit_depth = flux.quality.bit_depth,
            origine = ?origine,
            "streaming_null_quality_completed"
        );
    }
    origine
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nulle() -> StreamQuality {
        StreamQuality {
            codec: "FLAC".into(),
            sample_rate: 0,
            bit_depth: 0,
            bitrate: None,
            channels: 2,
        }
    }

    fn flac_96_24() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/flac/ref_24_96000_stereo.flac"
        ))
        .unwrap()
    }

    #[test]
    fn l_en_tete_du_flux_fait_foi_5283() {
        let mut q = nulle();
        let cat = StreamQuality {
            sample_rate: 192_000,
            bit_depth: 24,
            ..nulle()
        };
        let o = completer_la_qualite(&mut q, Some(&flac_96_24()), Some(&cat));
        assert_eq!(o, OrigineDeLaCadence::EnTeteDuFlux);
        assert_eq!((q.sample_rate, q.bit_depth), (96_000, 24));
    }

    #[test]
    fn sans_en_tete_lisible_le_catalogue_donne_la_cadence_5283() {
        let mut q = nulle();
        let cat = StreamQuality {
            sample_rate: 192_000,
            bit_depth: 24,
            ..nulle()
        };
        let o = completer_la_qualite(&mut q, Some(b"pas un flac du tout, ni fLaC"), Some(&cat));
        assert_eq!(o, OrigineDeLaCadence::Catalogue);
        assert_eq!((q.sample_rate, q.bit_depth), (192_000, 24));
    }

    #[test]
    fn rien_ne_la_donne_la_cadence_reste_nulle_5283() {
        let mut q = nulle();
        let o = completer_la_qualite(&mut q, None, Some(&nulle()));
        assert_eq!(o, OrigineDeLaCadence::Inconnue);
        assert_eq!(q.sample_rate, 0);
    }

    #[test]
    fn une_qualite_annoncee_non_nulle_n_est_pas_touchee_5283() {
        let mut q = StreamQuality {
            sample_rate: 44_100,
            bit_depth: 0,
            ..nulle()
        };
        completer_la_qualite(&mut q, Some(&flac_96_24()), None);
        assert_eq!((q.sample_rate, q.bit_depth), (44_100, 24));
    }
}
