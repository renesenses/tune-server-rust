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

/// #6334 — le codec que le service a LIVRÉ, en minuscules (`aac`, `flac`),
/// tel que `StreamQuality::codec` le nomme. C'est ce qu'il faut annoncer, et
/// non le conteneur servi : un AAC décodé en WAV pour la sortie restait sinon
/// annoncé « WAV · 16 bits », c'est-à-dire comme un CD.
pub(crate) fn format_livre_par_le_service(
    qualite: &crate::streaming::StreamQuality,
) -> Option<String> {
    let codec = qualite.codec.trim().to_lowercase();
    (!codec.is_empty()).then_some(codec)
}

/// #6334 — la profondeur à ANNONCER pour un flux de service : aucune pour un
/// codec avec perte (un AAC n'a pas de « 16 bits »), aucune quand le service
/// n'en a pas donné (0). Jamais la profondeur du conteneur de sortie.
pub(crate) fn profondeur_annoncee_par_le_service(
    qualite: &crate::streaming::StreamQuality,
    profondeur: u16,
) -> Option<u32> {
    (!qualite.avec_perte() && profondeur > 0).then_some(u32::from(profondeur))
}

/// #6334 — la profondeur qui part SUR LE FIL, pour `PlayMedia` : le DIDL
/// (`<res bitsPerSample>`) et l'annonce SlimProto décrivent le flux servi, pas
/// la source. `ResolvedStream.bit_depth` ne porte rien pour un codec avec
/// perte (l'annonce de la SOURCE) ; décodé pour la sortie en WAV ou FLAC, ce
/// flux a pourtant une profondeur : celle que les bras réseau (canal AAC,
/// OAAT, DASH) tirent d'une profondeur nulle, `cap_output_bit_depth(0)`, soit
/// 16 bits. La sortie locale reçoit, elle, un WAV 32 bits dont elle lit
/// l'en-tête : comme pour une source sans perte, l'annonce n'y décide de rien.
/// Relayé tel quel (`audio/mp4`), le flux n'a toujours aucune profondeur.
pub(crate) fn profondeur_du_fil(resolved: &ResolvedStream) -> Option<u32> {
    use crate::audio::formats::AudioFormat;
    if resolved.bit_depth.is_some() {
        return resolved.bit_depth;
    }
    let source_avec_perte = resolved.format_source.as_deref().is_some_and(|codec| {
        matches!(
            AudioFormat::from_extension(codec),
            Some(AudioFormat::Aac | AudioFormat::Mp3 | AudioFormat::Ogg | AudioFormat::Opus)
        )
    });
    let mime = resolved
        .mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let decode_pour_la_sortie = matches!(
        mime.as_str(),
        "audio/wav"
            | "audio/x-wav"
            | "audio/wave"
            | "audio/flac"
            | "audio/x-flac"
            | "audio/aiff"
            | "audio/x-aiff"
    ) || mime.starts_with("audio/l16")
        || mime.starts_with("audio/l24");
    (source_avec_perte && decode_pour_la_sortie)
        .then(|| u32::from(super::regles::cap_output_bit_depth(0)))
}

/// #6334 — le débit à annoncer, en kbit/s : celui que le service nomme pour
/// un codec avec perte (l'AAC 320 de Tidal `HIGH`). Rien pour un sans-perte :
/// le chemin du signal ne l'afficherait pas, et un chiffre calculé n'est pas
/// un débit nommé.
pub(crate) fn debit_annonce_par_le_service(
    qualite: &crate::streaming::StreamQuality,
) -> Option<u32> {
    qualite
        .bitrate
        .filter(|b| *b > 0 && qualite.avec_perte())
        .map(|b| b / 1000)
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
                .or_else(|| resolved.format_source.clone())
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
