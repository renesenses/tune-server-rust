//! Fil 2062 / #5550 — la durée d'une piste de serveur UPnP qui n'en annonce
//! aucune, lue dans les en-têtes du flux.
//!
//! La Freebox sert ses pistes sans `res@duration` : Tune les jouait avec une
//! durée de 0, la barre de lecture restait à 0:00 pendant que le temps écoulé
//! avançait, et toute la mécanique qui s'appuie sur la durée (fin de piste à
//! l'horloge, armement gapless, préchargement) était aveugle.
//!
//! On ne DEVINE rien : quelques requêtes `Range` bornées (64 Kio, au plus trois
//! lectures, quatre secondes en tout) et la lecture des en-têtes par
//! [`crate::audio::duree_des_entetes`]. Une durée connue n'est jamais
//! contredite : la sonde n'est appelée qu'à durée absente ou nulle.

use super::*;
use crate::audio::duree_des_entetes::{Lecture, analyser_la_suite, analyser_la_tete};

/// Octets lus en tête de flux.
const TETE: u64 = 64 * 1024;
/// Lectures au plus (tête, puis reprises après ID3 ou vers `moov`).
const LECTURES_MAX: usize = 3;
/// Borne de la sonde entière : au-delà, la piste part sans durée, comme avant.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// Lit `longueur` octets à partir de `debut`, et la taille totale du fichier
/// quand le serveur la dit. Un serveur qui ignore `Range` (réponse `200`) ne
/// sert que pour la tête : on lit alors les premiers octets et on coupe.
async fn lire_une_plage(
    client: &reqwest::Client,
    url: &str,
    debut: u64,
    longueur: u64,
) -> Option<(Vec<u8>, Option<u64>)> {
    let mut rep = client
        .get(url)
        .header(
            reqwest::header::RANGE,
            format!("bytes={debut}-{}", debut + longueur - 1),
        )
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .ok()?;
    let statut = rep.status();
    let total = if statut == reqwest::StatusCode::PARTIAL_CONTENT {
        rep.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|t| t.trim().parse::<u64>().ok())
    } else if statut.is_success() {
        if debut != 0 {
            return None;
        }
        rep.content_length()
    } else {
        return None;
    };
    let mut octets = Vec::with_capacity(longueur as usize);
    while (octets.len() as u64) < longueur {
        match rep.chunk().await.ok()? {
            Some(c) => octets.extend_from_slice(&c),
            None => break,
        }
    }
    octets.truncate(longueur as usize);
    Some((octets, total))
}

/// La durée (ms) d'un flux distant, lue dans ses en-têtes. `None` quand le
/// serveur ne répond pas, que le format n'est pas reconnu, ou que le budget
/// est dépassé.
pub(crate) async fn deduire_la_duree_du_flux(url: &str) -> Option<i64> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let sonde = async {
        let client = crate::http::client::shared();
        let (tete, total) = lire_une_plage(client, url, 0, TETE).await?;
        let mut lecture = analyser_la_tete(&tete, total);
        for _ in 1..LECTURES_MAX {
            let Lecture::Lire {
                debut,
                longueur,
                suite,
            } = lecture
            else {
                break;
            };
            let longueur = total.map_or(longueur, |t| longueur.min(t.saturating_sub(debut)));
            if longueur == 0 {
                return None;
            }
            let (octets, _) = lire_une_plage(client, url, debut, longueur).await?;
            lecture = analyser_la_suite(&octets, debut, total, suite);
        }
        match lecture {
            Lecture::Duree(ms) => i64::try_from(ms).ok(),
            _ => None,
        }
    };
    tokio::time::timeout(BUDGET, sonde).await.ok().flatten()
}

impl PlaybackOrchestrator {
    /// La durée à retenir pour une piste UPnP : celle de la demande quand elle
    /// est connue, sinon celle des en-têtes du flux — rangée alors dans la
    /// file de la zone et, pour une piste indexée, dans la bibliothèque.
    pub(super) async fn duree_d_une_piste_upnp(
        &self,
        req: &PlayRequest,
        audio_url: &str,
    ) -> Option<i64> {
        if req.duration_ms.is_some_and(|d| d > 0) {
            return req.duration_ms;
        }
        let Some(ms) = deduire_la_duree_du_flux(audio_url).await else {
            info!(
                zone_id = req.zone_id,
                url = %audio_url,
                "upnp_duree_du_flux_introuvable"
            );
            return req.duration_ms;
        };
        info!(
            zone_id = req.zone_id,
            track_id = ?req.track_id,
            duration_ms = ms,
            url = %audio_url,
            "upnp_duree_deduite_du_flux"
        );
        self.ranger_la_duree_deduite(req, ms);
        Some(ms)
    }

    /// Range une durée déduite là où l'interface la relit : les lignes de file
    /// de la zone qui désignent ce flux sans durée, et la ligne de
    /// bibliothèque d'une piste indexée sans durée. Jamais par-dessus une
    /// durée connue.
    fn ranger_la_duree_deduite(&self, req: &PlayRequest, ms: i64) {
        use crate::db::backend::ToSqlValue;
        if let Some(source_id) = req.source_id.as_deref()
            && let Err(e) = self.db.execute(
                "UPDATE queue_items SET duration_ms = ? WHERE zone_id = ? AND source_id = ? \
                 AND (duration_ms IS NULL OR duration_ms <= 0)",
                &[
                    &ms as &dyn ToSqlValue,
                    &req.zone_id as &dyn ToSqlValue,
                    &source_id as &dyn ToSqlValue,
                ],
            )
        {
            warn!(zone_id = req.zone_id, error = %e, "upnp_duree_file_non_rangee");
        }
        if let Some(track_id) = req.track_id
            && let Err(e) = self.db.execute(
                "UPDATE tracks SET duration_ms = ? WHERE id = ? \
                 AND (duration_ms IS NULL OR duration_ms <= 0)",
                &[&ms as &dyn ToSqlValue, &track_id as &dyn ToSqlValue],
            )
        {
            warn!(track_id, error = %e, "upnp_duree_piste_non_rangee");
        }
    }
}
