//! #3365 — la qualité d'une piste de SERVICE survit à l'enchaînement.
//!
//! Serge Asselin (fil 1670, Hifi Rose RS250A en DLNA, Qobuz) : la première
//! piste s'affiche en 192 kHz / 24 bits, la suivante retombe en 44,1 / 16
//! alors que le Rose, lui, reste à 192 / 24. Le flux est bon, l'annonce est
//! fausse.
//!
//! La première piste passe par `play_inner` → `composer_le_now_playing`, qui
//! reporte `format` / `sample_rate` / `bit_depth` depuis le `ResolvedStream`
//! de `resolve_stream`. La piste suivante passe par l'enchaînement gapless :
//! `resolve_queue_item_url` la résout (le MÊME `resolve_stream`) une
//! trentaine de secondes à l'avance, n'en gardait que le `stream_id`
//! (`gapless_sessions`), et `advance_queue_metadata` construisait ensuite un
//! `NowPlaying` de service par `..Default::default()` — qualité à `None`.
//!
//! Ce module porte la qualité ANNONCÉE de la piste pré-armée, de l'armement
//! jusqu'à la transition. Elle est indexée par zone et datée par son
//! `stream_id` : à l'avance, elle n'est reprise que si la zone adopte
//! exactement ce flux-là. Aucun chiffre n'est fabriqué : ce que
//! `resolve_stream` ne sait pas reste `None`.

use super::*;

/// Ce que la première piste aurait annoncé pour ce flux, calculé à
/// l'armement par les mêmes règles que `composer_le_now_playing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualitePreArmee {
    /// Le flux auquel cette qualité appartient. Une qualité dont le flux
    /// n'est pas celui que la zone adopte n'est jamais reprise.
    pub stream_id: String,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub bitrate_kbps: Option<u32>,
}

/// Le format que la SOURCE nomme elle-même, sans regarder le flux servi.
///
/// Extrait de `composer_le_now_playing` pour que l'armement gapless dise la
/// même chose que le démarrage : Qobuz ne sert que du FLAC ; Bandcamp tire
/// son codec de l'URL (`mp3` quand elle ne nomme rien, #2074).
pub(crate) fn format_nomme_par_la_source(source: &str, source_id: Option<&str>) -> Option<String> {
    match source {
        "qobuz" => Some("flac".to_string()),
        // Bandcamp : le dire ici — et non depuis le client — pour deux
        // raisons : le repli sur le type MIME afficherait « MPEG » au lieu de
        // « MP3 », et surtout l'avance de file re-résout la piste SANS
        // qu'aucun client repasse un format.
        //
        // Le codec vient de l'URL, pas du nom du service : sans achat
        // Bandcamp ne sert que du `mp3-128`, mais un fichier acheté descend en
        // `flac`/`alac` par la même porte, et l'annoncer « MP3 » serait faux
        // (#2074). Repli sur `mp3` quand l'URL ne nomme rien : c'est l'écoute
        // libre.
        "bandcamp" => Some(
            source_id
                .and_then(bandcamp_encoding)
                .and_then(|enc| bandcamp_quality(&enc))
                .map(|q| q.codec.to_string())
                .unwrap_or_else(|| "mp3".to_string()),
        ),
        _ => None,
    }
}

/// Le dernier repli du format : le type MIME du flux, `audio/` et `x-` ôtés.
pub(crate) fn format_du_mime(mime: &str) -> String {
    mime.strip_prefix("audio/")
        .unwrap_or(mime)
        .replace("x-", "")
        .to_string()
}

impl QualitePreArmee {
    /// La qualité qu'annoncerait `composer_le_now_playing` pour ce flux de
    /// service : pas de ligne de bibliothèque, pas de `media_format` client.
    /// `None` quand le flux n'a pas d'identifiant — rien à quoi l'attacher.
    pub(crate) fn d_un_flux_de_service(
        source: &str,
        source_id: Option<&str>,
        resolved: &ResolvedStream,
    ) -> Option<Self> {
        let stream_id = resolved.stream_id.clone()?;
        Some(Self {
            stream_id,
            format: format_nomme_par_la_source(source, source_id)
                .or_else(|| Some(format_du_mime(&resolved.mime_type))),
            // `resolution_annoncee(None, resolu, false)` = `resolu` : une
            // source non locale n'a pas de ligne, la valeur résolue fait foi.
            sample_rate: resolution_annoncee(None, resolved.sample_rate, false),
            bit_depth: resolution_annoncee(None, resolved.bit_depth, false),
            bitrate_kbps: resolved.bitrate_kbps,
        })
    }
}

impl PlaybackOrchestrator {
    /// Range la qualité de la piste que l'on vient d'armer pour cette zone.
    /// Remplace celle d'un armement précédent.
    pub(crate) async fn ranger_la_qualite_pre_armee(
        &self,
        zone_id: i64,
        qualite: Option<QualitePreArmee>,
    ) {
        let mut rangees = self.qualites_pre_armees.lock().await;
        match qualite {
            Some(q) => {
                rangees.insert(zone_id, q);
            }
            None => {
                rangees.remove(&zone_id);
            }
        }
    }

    /// Reprend (et retire) la qualité pré-armée de la zone, UNIQUEMENT si
    /// elle appartient au flux que la zone adopte. Sinon `None` : mieux vaut
    /// ne rien annoncer qu'annoncer la qualité d'un autre flux.
    pub(crate) async fn reprendre_la_qualite_pre_armee(
        &self,
        zone_id: i64,
        flux_adopte: Option<&str>,
    ) -> Option<QualitePreArmee> {
        let q = self.qualites_pre_armees.lock().await.remove(&zone_id)?;
        (Some(q.stream_id.as_str()) == flux_adopte).then_some(q)
    }
}
