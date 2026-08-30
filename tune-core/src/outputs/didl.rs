//! Shared DIDL-Lite XML builder for DLNA/UPnP/OpenHome outputs.
//!
//! DIDL-Lite (Digital Item Declaration Language) is the XML format used by
//! DLNA/UPnP to describe media items. This module provides a single reusable
//! builder so that all output modules produce consistent, valid DIDL-Lite.

use quick_xml::escape::{escape, partial_escape};

/// XML 1.0 interdit les caractères de contrôle hors tabulation et fins de
/// ligne — et `escape` n'y touche pas : un séparateur NUL d'ID3v2.4/Picard
/// traversait tel quel et rendait l'enveloppe SOAP entière illégale. npupnp
/// (upmpdcli — HiFiMAN Serenade de Tades) répond alors 401 « Invalid
/// Action » : son parseur échoue sur le corps, pas sur l'action. On remplace
/// par une espace : les multi-valeurs restent lisibles (« Lisa The String
/// Soloists »), rien n'est perdu.
fn texte_xml_sain(s: &str) -> std::borrow::Cow<'_, str> {
    let illegal = |c: char| !matches!(c, '\t' | '\n' | '\r') && c < '\u{20}';
    if s.chars().any(illegal) {
        std::borrow::Cow::Owned(
            s.chars()
                .map(|c| if illegal(c) { ' ' } else { c })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Échappement XML précédé de l'assainissement ci-dessus — l'unique porte
/// par laquelle du texte libre (tags) entre dans un document DIDL.
fn escape_sain(s: &str) -> String {
    let sain = texte_xml_sain(s);
    escape(sain.as_ref()).into_owned()
}

/// DLNA flags string for a given MIME type.
///
/// Returns `protocolInfo` 4th-field with DLNA profile name, operation flags,
/// transcoding indicator and streaming flags.
pub fn dlna_flags_for_mime(mime: &str) -> &'static str {
    dlna_flags_for_mime_bd(mime, None)
}

/// Like [`dlna_flags_for_mime`] but bit-depth aware for LPCM/WAV.
///
/// The standard DLNA `LPCM` profile (`DLNA.ORG_PN=LPCM`) is defined for 16-bit
/// only. Advertising it on a genuine 24-bit WAV makes strict renderers map the
/// stream back to 16-bit and read misaligned samples → SILENCE (#1137). So for
/// a WAV/LPCM MIME served at >16-bit we emit NO `PN` (just OP/CI/FLAGS): the
/// renderer parses the real WAV header instead of a false profile claim. This
/// path is only ever reached for the opt-in `dlna_wav24` zones; every existing
/// LPCM fallback stays 16-bit and keeps the `PN=LPCM` profile unchanged.
pub fn dlna_flags_for_mime_bd(mime: &str, bit_depth: Option<u32>) -> &'static str {
    dlna_flags_for_mime_bd_sr(mime, bit_depth, None)
}

/// Fréquence au-delà de laquelle le profil `LPCM` ne s'applique plus.
///
/// Le profil DLNA `LPCM` est défini pour 44,1 et 48 kHz. Au-dessus, la
/// déclaration est fausse, quelle que soit la profondeur.
const LPCM_PROFILE_MAX_RATE: u32 = 48_000;

/// Comme [`dlna_flags_for_mime_bd`], mais en tenant compte AUSSI de la
/// fréquence d'échantillonnage.
///
/// Le correctif #1137 a retiré le `PN=LPCM` au-delà de 16 bits, parce qu'un
/// renderer strict rabat le flux sur le profil annoncé et lit des échantillons
/// désalignés — donc du SILENCE. La même règle vaut pour la fréquence, et elle
/// n'avait jamais été appliquée : un WAV 16 bits à 192 kHz sortait annoncé
/// `PN=LPCM`, un profil auquel il ne se conforme pas.
///
/// Le chemin de repli WAV plafonne la PROFONDEUR à 16 bits mais ne touche pas
/// à la fréquence : une source 192/24 finit donc en 16 bits **à 192 kHz**, avec
/// une déclaration `LPCM` mensongère. C'est la configuration de Yves — ALAC
/// 192/24, « Forcer le WAV », aucune fréquence max — et le symptôme rapporté
/// est exactement celui de la famille #1137 : la lecture démarre, la position
/// avance, aucun son ne sort (forum #1437, darTZeel LHC).
///
/// Retirer le `PN` ne dégrade rien : le renderer lit alors l'en-tête WAV réel
/// au lieu d'une promesse de profil. C'est une correction de justesse, vraie
/// indépendamment de ce cas précis.
pub fn dlna_flags_for_mime_bd_sr(
    mime: &str,
    bit_depth: Option<u32>,
    sample_rate: Option<u32>,
) -> &'static str {
    // DLNA.ORG_OP=01 : byte-range seek supported
    // DLNA.ORG_CI=0  : no transcoding
    // DLNA.ORG_FLAGS : streaming + interactive + background + v1.5
    const NO_PN: &str =
        "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
    let out_of_lpcm_profile = bit_depth.is_some_and(|bd| bd > 16)
        || sample_rate.is_some_and(|sr| sr > LPCM_PROFILE_MAX_RATE);
    match mime {
        "audio/L16" | "audio/wav" | "audio/x-wav" if out_of_lpcm_profile => NO_PN,
        "audio/L16" | "audio/wav" | "audio/x-wav" => {
            "DLNA.ORG_PN=LPCM;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        }
        "audio/flac" | "audio/x-flac" => {
            "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        }
        "audio/mpeg" => {
            "DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        }
        "audio/mp4" | "audio/aac" => {
            "DLNA.ORG_PN=AAC_ISO;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        }
        "application/x-dsd" | "audio/x-dsd" | "audio/dsf" | "audio/dff" | "audio/x-dff" => {
            "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        }
        _ => "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
    }
}

/// DLNA flags string for a *live* (infinite) stream such as internet radio.
///
/// Differs from [`dlna_flags_for_mime`] in two important ways:
/// - `DLNA.ORG_OP=00` : no byte-range and no time seek (a live stream is not
///   seekable). Advertising `OP=01` on a source with no size makes some
///   renderers reject or silently drop the stream.
/// - `DLNA.ORG_FLAGS=8D500000…` : senderPaced (`sn-increase`, bit 31) +
///   streaming-transfer-mode (bit 24) + background (bit 22) + DLNA v1.5
///   (bit 20). This is the widely-used flag set for live/senderPaced sources.
///
/// The `DLNA.ORG_PN` profile name is intentionally omitted: renderers are
/// stricter about matching a declared PN against an exact bitrate/profile for
/// live streams, and an absent PN is treated as "unspecified" (accepted).
pub fn dlna_flags_for_mime_live(_mime: &str) -> &'static str {
    "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=8D500000000000000000000000000000"
}

/// Format a duration in milliseconds to DIDL `HH:MM:SS.mmm` format.
pub fn format_duration_didl(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let frac = ms % 1000;
    format!("{h}:{m:02}:{s:02}.{frac:03}")
}

/// Format a duration in milliseconds to `H:MM:SS` format (no fractional part).
pub fn format_duration_hms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

/// Return true when the value is a usable metadata string (not empty,
/// not the literal `"null"` that JavaScript clients sometimes send).
fn is_valid_meta(v: Option<&str>) -> bool {
    matches!(v, Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("null"))
}

/// Which protocol-info style to use in the `<res>` element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolStyle {
    /// Full DLNA flags: `http-get:*:{mime}:{dlna_flags}`.
    /// Used by DLNA renderers (Sonos, DMP-A8, darTZeel, etc.)
    Dlna,
    /// Simple wildcard: `http-get:*:{mime}:*`.
    /// Used by OpenHome and UPnP ContentDirectory.
    Simple,
}

/// Builder for a single DIDL-Lite `<item>` element.
///
/// Produces raw XML (not HTML-escaped). Callers that embed the result inside
/// SOAP body text must escape it themselves (use `build_escaped()` or
/// `quick_xml::escape::partial_escape` — NOT `escape`, which also escapes
/// `"` and breaks some DLNA renderers).
pub struct DidlBuilder {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_art_uri: Option<String>,
    /// If true, add `dlna:profileID="JPEG_TN"` attribute on albumArtURI
    /// and declare `xmlns:dlna` on the root element.
    dlna_art_profile: bool,
    duration_ms: Option<u64>,
    resource_url: String,
    mime_type: String,
    protocol_style: ProtocolStyle,
    file_size: Option<u64>,
    sample_rate: Option<u32>,
    bit_depth: Option<u32>,
    channels: Option<u32>,
    track_number: Option<u32>,
    /// Include `<upnp:artist>` in addition to `<dc:creator>`.
    include_upnp_artist: bool,
    /// Item id attribute value.
    item_id: String,
    /// Parent id attribute value.
    parent_id: String,
    /// True for infinite live streams (internet radio): emit live/senderPaced
    /// protocolInfo flags and never emit `size=`/`duration=` in `<res>`.
    live_stream: bool,
    byte_seekable: bool,
    /// Émettre `sampleFrequency` / `bitsPerSample` / `nrAudioChannels` dans
    /// `<res>`. Faux pour le DIDL réduit, qui doit tenir sous un segment TCP
    /// mais a QUAND MÊME besoin de la fréquence et de la profondeur pour
    /// choisir le bon profil DLNA (voir [`Self::sans_attributs_audio`]).
    emettre_attributs_audio: bool,
}

impl DidlBuilder {
    /// Create a new builder with required fields.
    pub fn new(title: &str, resource_url: &str, mime_type: &str) -> Self {
        Self {
            title: title.to_string(),
            artist: None,
            album: None,
            album_art_uri: None,
            dlna_art_profile: false,
            duration_ms: None,
            resource_url: resource_url.to_string(),
            mime_type: mime_type.to_string(),
            protocol_style: ProtocolStyle::Simple,
            file_size: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            track_number: None,
            include_upnp_artist: false,
            item_id: "0".to_string(),
            parent_id: "0".to_string(),
            live_stream: false,
            byte_seekable: true,
            emettre_attributs_audio: true,
        }
    }

    /// Renseigner la fréquence et la profondeur SANS les écrire dans `<res>`.
    ///
    /// Le profil DLNA annoncé dans le `protocolInfo` dépend des deux
    /// (`dlna_flags_for_mime_bd_sr` : au-delà de 16 bits ou de 48 kHz, un WAV
    /// n'est plus du `PN=LPCM`, et l'annoncer quand même fait jouer du SILENCE
    /// — #1137, #1458). Les ATTRIBUTS, eux, coûtent des octets, et le DIDL
    /// réduit existe précisément pour tenir sous un segment TCP.
    ///
    /// Les deux besoins ne sont pas contradictoires : on garde les valeurs pour
    /// décider du profil, on n'écrit pas les attributs. Le `protocolInfo` ne
    /// grossit pas non plus — la variante sans `PN` est plus COURTE.
    pub fn sans_attributs_audio(mut self) -> Self {
        self.emettre_attributs_audio = false;
        self
    }

    /// Mark this item as an infinite live stream (internet radio).
    ///
    /// Switches the `<res>` protocolInfo to live/senderPaced flags
    /// (`DLNA.ORG_OP=00`, `DLNA.ORG_FLAGS=8D50…`) and suppresses the
    /// `size=`/`duration=` attributes regardless of any values set.
    pub fn live_stream(mut self, yes: bool) -> Self {
        self.live_stream = yes;
        self
    }

    /// Une source FINIE mais non-seekable : la conversion à la volée
    /// (DSD→WAV). La durée et la taille restent annoncées, mais le
    /// protocolInfo passe à `DLNA.ORG_OP=00` — sans quoi le renderer
    /// seeke par tranches un tuyau qui ne sait pas rejouer un octet passé
    /// (Eversolo DMP-A8 : gel à 0:00 en boucle, .42, 24/08).
    pub fn byte_seekable(mut self, yes: bool) -> Self {
        self.byte_seekable = yes;
        self
    }

    pub fn artist(mut self, artist: &str) -> Self {
        self.artist = Some(artist.to_string());
        self
    }

    pub fn artist_opt(mut self, artist: Option<&str>) -> Self {
        self.artist = artist.map(|s| s.to_string());
        self
    }

    pub fn album(mut self, album: &str) -> Self {
        self.album = Some(album.to_string());
        self
    }

    pub fn album_opt(mut self, album: Option<&str>) -> Self {
        self.album = album.map(|s| s.to_string());
        self
    }

    pub fn album_art(mut self, uri: &str) -> Self {
        self.album_art_uri = Some(uri.to_string());
        self
    }

    pub fn album_art_opt(mut self, uri: Option<&str>) -> Self {
        self.album_art_uri = uri.map(|s| s.to_string());
        self
    }

    /// Add `dlna:profileID="JPEG_TN"` to albumArtURI and declare `xmlns:dlna`.
    pub fn dlna_art_profile(mut self, yes: bool) -> Self {
        self.dlna_art_profile = yes;
        self
    }

    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn duration_ms_opt(mut self, ms: Option<u64>) -> Self {
        self.duration_ms = ms;
        self
    }

    pub fn file_size(mut self, size: u64) -> Self {
        self.file_size = Some(size);
        self
    }

    pub fn file_size_opt(mut self, size: Option<u64>) -> Self {
        self.file_size = size;
        self
    }

    pub fn audio_info(mut self, rate: u32, depth: u32, channels: u32) -> Self {
        self.sample_rate = Some(rate);
        self.bit_depth = Some(depth);
        self.channels = Some(channels);
        self
    }

    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.sample_rate = Some(rate);
        self
    }

    pub fn sample_rate_opt(mut self, rate: Option<u32>) -> Self {
        self.sample_rate = rate;
        self
    }

    pub fn bit_depth(mut self, depth: u32) -> Self {
        self.bit_depth = Some(depth);
        self
    }

    pub fn bit_depth_opt(mut self, depth: Option<u32>) -> Self {
        self.bit_depth = depth;
        self
    }

    pub fn channels(mut self, ch: u32) -> Self {
        self.channels = Some(ch);
        self
    }

    pub fn channels_opt(mut self, ch: Option<u32>) -> Self {
        self.channels = ch;
        self
    }

    pub fn track_number(mut self, num: u32) -> Self {
        self.track_number = Some(num);
        self
    }

    /// Set the protocol-info style (DLNA flags or simple wildcard).
    pub fn protocol_style(mut self, style: ProtocolStyle) -> Self {
        self.protocol_style = style;
        self
    }

    /// Include `<upnp:artist>` tag in addition to `<dc:creator>`.
    pub fn include_upnp_artist(mut self, yes: bool) -> Self {
        self.include_upnp_artist = yes;
        self
    }

    /// Set the item id attribute (default "0").
    pub fn item_id(mut self, id: &str) -> Self {
        self.item_id = id.to_string();
        self
    }

    /// Set the parent id attribute (default "0").
    pub fn parent_id(mut self, id: &str) -> Self {
        self.parent_id = id.to_string();
        self
    }

    /// Build just the `<item>` element (without the `<DIDL-Lite>` envelope).
    ///
    /// Use this when multiple items are combined inside a single `<DIDL-Lite>`
    /// wrapper (e.g. UPnP ContentDirectory Browse responses).
    pub fn build_item(&self) -> String {
        let title = escape_sain(&self.title);
        let escaped_url = escape_sain(&self.resource_url);
        let escaped_id = escape_sain(&self.item_id);
        let escaped_pid = escape_sain(&self.parent_id);

        // Artist tags
        let artist_tags = if is_valid_meta(self.artist.as_deref()) {
            let a = escape_sain(self.artist.as_deref().unwrap());
            if self.include_upnp_artist {
                format!("<dc:creator>{a}</dc:creator><upnp:artist>{a}</upnp:artist>")
            } else {
                format!("<dc:creator>{a}</dc:creator>")
            }
        } else {
            String::new()
        };

        // Album tag
        let album_tag = self
            .album
            .as_deref()
            .filter(|a| is_valid_meta(Some(a)))
            .map(|a| format!("<upnp:album>{}</upnp:album>", escape_sain(a)))
            .unwrap_or_default();

        // Album art URI
        let art_tag = self
            .album_art_uri
            .as_deref()
            .filter(|c| is_valid_meta(Some(c)))
            .map(|c| {
                let c = escape_sain(c);
                if self.dlna_art_profile {
                    format!("<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">{c}</upnp:albumArtURI>")
                } else {
                    format!("<upnp:albumArtURI>{c}</upnp:albumArtURI>")
                }
            })
            .unwrap_or_default();

        // Track number
        let track_num_tag = self
            .track_number
            .filter(|n| *n > 0)
            .map(|n| format!("<upnp:originalTrackNumber>{n}</upnp:originalTrackNumber>"))
            .unwrap_or_default();

        // Protocol info
        let protocol_info = match self.protocol_style {
            ProtocolStyle::Dlna => {
                let flags = if self.live_stream {
                    dlna_flags_for_mime_live(&self.mime_type).to_string()
                } else {
                    let f = dlna_flags_for_mime_bd_sr(
                        &self.mime_type,
                        self.bit_depth,
                        self.sample_rate,
                    );
                    if self.byte_seekable {
                        f.to_string()
                    } else {
                        // Conversion à la volée : mêmes profils, mais on dit la
                        // vérité sur la seekabilité.
                        f.replace("DLNA.ORG_OP=01", "DLNA.ORG_OP=00")
                    }
                };
                format!("http-get:*:{}:{}", self.mime_type, flags)
            }
            ProtocolStyle::Simple => format!("http-get:*:{}:*", self.mime_type),
        };

        // Res attributes. Live streams (internet radio) are infinite and not
        // seekable — never advertise a duration or size for them, otherwise
        // renderers try to treat the source as a fixed-length file.
        let dur_attr = if self.live_stream {
            String::new()
        } else {
            self.duration_ms
                .filter(|d| *d > 0)
                .map(|d| format!(" duration=\"{}\"", format_duration_didl(d)))
                .unwrap_or_default()
        };

        let size_attr = if self.live_stream {
            String::new()
        } else {
            self.file_size
                .map(|s| format!(" size=\"{s}\""))
                .unwrap_or_default()
        };

        let sr_attr = self
            .sample_rate
            .filter(|_| self.emettre_attributs_audio)
            .map(|sr| format!(" sampleFrequency=\"{sr}\""))
            .unwrap_or_default();

        let bd_attr = self
            .bit_depth
            .filter(|_| self.emettre_attributs_audio)
            .map(|bd| format!(" bitsPerSample=\"{bd}\""))
            .unwrap_or_default();

        let ch_attr = self
            .channels
            .filter(|_| self.emettre_attributs_audio)
            .map(|ch| format!(" nrAudioChannels=\"{ch}\""))
            .unwrap_or_default();

        format!(
            "<item id=\"{escaped_id}\" parentID=\"{escaped_pid}\" restricted=\"1\">\
             <dc:title>{title}</dc:title>\
             {artist_tags}\
             <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
             {album_tag}\
             {art_tag}\
             {track_num_tag}\
             <res protocolInfo=\"{protocol_info}\"{dur_attr}{sr_attr}{bd_attr}{ch_attr}{size_attr}>{escaped_url}</res>\
             </item>"
        )
    }

    /// Build the complete DIDL-Lite XML document (raw, not HTML-escaped).
    ///
    /// Wraps a single `<item>` inside the `<DIDL-Lite>` envelope with all
    /// required namespace declarations.
    pub fn build(&self) -> String {
        // xmlns:dlna only when needed
        let dlna_ns = if self.dlna_art_profile && self.album_art_uri.is_some() {
            " xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\""
        } else {
            ""
        };

        let item = self.build_item();

        format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"{dlna_ns}>\
             {item}\
             </DIDL-Lite>"
        )
    }

    /// Build the DIDL-Lite XML and then XML-escape it for embedding in SOAP body text.
    ///
    /// This is the format expected by DLNA renderers when DIDL is passed as the
    /// value of `CurrentURIMetaData` in a `SetAVTransportURI` SOAP call.
    ///
    /// Uses `partial_escape` (only `<`, `>`, `&`) instead of full `escape`
    /// (which also escapes `"` and `'`).  Double-quotes do NOT require escaping
    /// in XML text content — only in attribute values — and some DLNA renderers
    /// (Denon, Marantz) have buggy XML parsers that fail to unescape `&quot;`
    /// in text content, causing them to reject the metadata entirely.
    pub fn build_escaped(&self) -> String {
        partial_escape(&self.build()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_build() {
        let xml = DidlBuilder::new("Test", "http://example.com/stream", "audio/flac").build();
        assert!(xml.contains("DIDL-Lite"));
        assert!(xml.contains("Test"));
        assert!(xml.contains("http://example.com/stream"));
        assert!(xml.contains("audio/flac"));
        assert!(xml.contains("object.item.audioItem.musicTrack"));
    }

    #[test]
    fn with_all_fields() {
        let xml = DidlBuilder::new(
            "So What",
            "http://192.168.1.18:8085/stream/42.flac",
            "audio/flac",
        )
        .artist("Miles Davis")
        .album("Kind of Blue")
        .album_art("http://192.168.1.18:8085/artwork/abc123")
        .duration_ms(562_000)
        .file_size(50_000_000)
        .audio_info(96000, 24, 2)
        .track_number(1)
        .include_upnp_artist(true)
        .item_id("track/42")
        .parent_id("album/10")
        .build();

        assert!(xml.contains("So What"));
        assert!(xml.contains("Miles Davis"));
        assert!(xml.contains("Kind of Blue"));
        assert!(xml.contains("albumArtURI"));
        assert!(xml.contains("abc123"));
        assert!(xml.contains("duration=\"0:09:22.000\""));
        assert!(xml.contains("size=\"50000000\""));
        assert!(xml.contains("sampleFrequency=\"96000\""));
        assert!(xml.contains("bitsPerSample=\"24\""));
        assert!(xml.contains("nrAudioChannels=\"2\""));
        assert!(xml.contains("originalTrackNumber"));
        assert!(xml.contains("<upnp:artist>Miles Davis</upnp:artist>"));
        assert!(xml.contains("<dc:creator>Miles Davis</dc:creator>"));
        assert!(xml.contains("id=\"track/42\""));
        assert!(xml.contains("parentID=\"album/10\""));
    }

    #[test]
    fn dlna_style_protocol_info() {
        let xml = DidlBuilder::new("T", "http://x/s", "audio/flac")
            .protocol_style(ProtocolStyle::Dlna)
            .build();
        assert!(xml.contains("DLNA.ORG_OP=01"));
        assert!(xml.contains("DLNA.ORG_FLAGS="));
    }

    #[test]
    fn simple_style_protocol_info() {
        let xml = DidlBuilder::new("T", "http://x/s", "audio/flac")
            .protocol_style(ProtocolStyle::Simple)
            .build();
        assert!(xml.contains("protocolInfo=\"http-get:*:audio/flac:*\""));
    }

    #[test]
    fn dlna_art_profile() {
        let xml = DidlBuilder::new("T", "http://x/s", "audio/flac")
            .album_art("http://x/cover.jpg")
            .dlna_art_profile(true)
            .build();
        assert!(xml.contains("dlna:profileID=\"JPEG_TN\""));
        assert!(xml.contains("xmlns:dlna"));
    }

    #[test]
    fn no_dlna_ns_without_art() {
        let xml = DidlBuilder::new("T", "http://x/s", "audio/flac")
            .dlna_art_profile(true)
            .build();
        assert!(
            !xml.contains("xmlns:dlna"),
            "no xmlns:dlna without album art"
        );
    }

    #[test]
    fn escapes_special_chars() {
        let xml = DidlBuilder::new(
            "Rock & Roll",
            "http://example.com/stream?a=1&b=2",
            "audio/flac",
        )
        .artist("AC/DC")
        .build();
        assert!(xml.contains("Rock &amp; Roll"));
        assert!(xml.contains("a=1&amp;b=2"));
        assert!(xml.contains("AC/DC"));
    }

    #[test]
    fn null_artist_omitted() {
        let xml = DidlBuilder::new("Title", "http://x/s", "audio/flac")
            .artist("null")
            .build();
        assert!(
            !xml.contains("dc:creator"),
            "literal 'null' artist must be omitted"
        );
    }

    #[test]
    fn empty_artist_omitted() {
        let xml = DidlBuilder::new("Title", "http://x/s", "audio/flac")
            .artist("")
            .build();
        assert!(!xml.contains("dc:creator"), "empty artist must be omitted");
    }

    #[test]
    fn without_optional_fields() {
        let xml = DidlBuilder::new("Title", "http://x/s", "audio/flac").build();
        assert!(!xml.contains("albumArtURI"));
        assert!(!xml.contains("upnp:album"));
        assert!(!xml.contains("dc:creator"));
        assert!(!xml.contains("size="));
        assert!(!xml.contains("duration="));
        assert!(!xml.contains("sampleFrequency"));
        assert!(!xml.contains("bitsPerSample"));
        assert!(!xml.contains("nrAudioChannels"));
        assert!(!xml.contains("originalTrackNumber"));
    }

    #[test]
    fn build_escaped_wraps_in_entities() {
        let xml = DidlBuilder::new("Test", "http://x/s", "audio/flac").build_escaped();
        assert!(xml.contains("&lt;DIDL-Lite"));
        assert!(xml.contains("&lt;/DIDL-Lite&gt;"));
    }

    #[test]
    fn build_escaped_does_not_escape_quotes() {
        // DLNA renderers (Denon, Marantz) have buggy XML parsers that fail
        // to unescape &quot; in SOAP text content.  Quotes must remain as
        // raw " in the escaped DIDL, not &quot;.
        let xml = DidlBuilder::new("Test", "http://x/s", "audio/flac")
            .protocol_style(ProtocolStyle::Dlna)
            .item_id("1")
            .build_escaped();
        assert!(
            !xml.contains("&quot;"),
            "escaped DIDL must not contain &quot; — breaks Denon/Marantz"
        );
        // Namespace declarations and attribute values should use raw quotes
        assert!(xml.contains("xmlns=\""));
        assert!(xml.contains("id=\"1\""));
    }

    #[test]
    fn dlna_flags_wav() {
        assert!(dlna_flags_for_mime("audio/wav").contains("DLNA.ORG_PN=LPCM"));
        assert!(dlna_flags_for_mime("audio/x-wav").contains("DLNA.ORG_PN=LPCM"));
        assert!(dlna_flags_for_mime("audio/L16").contains("DLNA.ORG_PN=LPCM"));
    }

    #[test]
    fn dlna_flags_wav_16bit_keeps_lpcm_profile() {
        // 16-bit WAV is genuine LPCM — keep the PN so lax renderers accept it.
        assert!(dlna_flags_for_mime_bd("audio/wav", Some(16)).contains("DLNA.ORG_PN=LPCM"));
        assert!(dlna_flags_for_mime_bd("audio/L16", None).contains("DLNA.ORG_PN=LPCM"));
    }

    #[test]
    fn dlna_flags_wav_24bit_drops_lpcm_profile() {
        // 24-bit WAV must NOT claim the 16-bit-only LPCM profile (#1137 silence).
        let f = dlna_flags_for_mime_bd("audio/wav", Some(24));
        assert!(
            !f.contains("DLNA.ORG_PN"),
            "24-bit WAV must not advertise a PN: {f}"
        );
        assert!(f.contains("DLNA.ORG_OP=01"), "still seekable: {f}");
        assert!(!dlna_flags_for_mime_bd("audio/L16", Some(24)).contains("DLNA.ORG_PN"));
    }

    /// Le pendant de #1137 sur l'axe de la FRÉQUENCE, jamais couvert jusqu'ici.
    /// Le repli WAV plafonne la profondeur à 16 bits mais laisse la fréquence
    /// intacte : une source 192/24 sortait donc en 16 bits à 192 kHz, annoncée
    /// `PN=LPCM` — un profil auquel elle ne se conforme pas (forum #1437).
    #[test]
    fn dlna_flags_wav_16bit_above_48k_drops_lpcm_profile() {
        let f = dlna_flags_for_mime_bd_sr("audio/wav", Some(16), Some(192_000));
        assert!(
            !f.contains("DLNA.ORG_PN"),
            "192 kHz n'est pas du profil LPCM : {f}"
        );
        assert!(f.contains("DLNA.ORG_OP=01"), "toujours seekable : {f}");
        assert!(
            !dlna_flags_for_mime_bd_sr("audio/L16", Some(16), Some(96_000)).contains("DLNA.ORG_PN")
        );
    }

    /// Les fréquences DU profil restent annoncées : ne rien casser chez les
    /// renderers laxistes qui exigent un `PN` pour accepter le flux.
    #[test]
    fn dlna_flags_wav_16bit_within_profile_keeps_lpcm() {
        for sr in [44_100_u32, 48_000] {
            let f = dlna_flags_for_mime_bd_sr("audio/wav", Some(16), Some(sr));
            assert!(
                f.contains("DLNA.ORG_PN=LPCM"),
                "{sr} Hz reste du LPCM : {f}"
            );
        }
        // Fréquence inconnue : comportement d'avant, on garde le profil.
        assert!(
            dlna_flags_for_mime_bd_sr("audio/wav", Some(16), None).contains("DLNA.ORG_PN=LPCM")
        );
    }

    #[test]
    fn dlna_flags_mp3() {
        assert!(dlna_flags_for_mime("audio/mpeg").contains("DLNA.ORG_PN=MP3"));
    }

    #[test]
    fn dlna_flags_aac() {
        assert!(dlna_flags_for_mime("audio/mp4").contains("DLNA.ORG_PN=AAC_ISO"));
        assert!(dlna_flags_for_mime("audio/aac").contains("DLNA.ORG_PN=AAC_ISO"));
    }

    #[test]
    fn dlna_flags_live_no_seek() {
        let f = dlna_flags_for_mime_live("audio/mpeg");
        assert!(f.contains("DLNA.ORG_OP=00"), "live stream must not seek");
        assert!(f.contains("8D50"), "live stream uses senderPaced flags");
        assert!(!f.contains("DLNA.ORG_OP=01"));
    }

    #[test]
    fn live_stream_didl_omits_size_and_duration() {
        // Even when a size/duration are set, a live stream must not advertise
        // them — some renderers (Yamaha R-N2000A) stay silent otherwise.
        let xml = DidlBuilder::new("France Inter", "http://icecast/fip.mp3", "audio/mpeg")
            .protocol_style(ProtocolStyle::Dlna)
            .live_stream(true)
            .duration_ms(999_000)
            .file_size(123_456)
            .build();
        assert!(
            xml.contains("DLNA.ORG_OP=00"),
            "live protocolInfo, got: {xml}"
        );
        assert!(!xml.contains("size="), "no size on live stream");
        assert!(!xml.contains("duration="), "no duration on live stream");
    }

    /// Une conversion à la volée (DSD→WAV) est FINIE mais non-seekable : le
    /// protocolInfo doit dire OP=00 (sinon l'Eversolo seeke le tuyau et gèle
    /// à 0:00), mais la durée et la taille restent annoncées — le renderer
    /// affiche une piste normale, il ne doit juste pas chercher dedans.
    #[test]
    fn conversion_didl_dit_op00_mais_garde_la_duree() {
        let xml = DidlBuilder::new("Abacab", "http://s/stream/x.wav", "audio/wav")
            .protocol_style(ProtocolStyle::Dlna)
            .byte_seekable(false)
            .duration_ms(390_000)
            .file_size(1_300_000_000)
            .bit_depth(24)
            .sample_rate(176_400)
            .build();
        assert!(
            xml.contains("DLNA.ORG_OP=00"),
            "conversion non-seekable, got: {xml}"
        );
        assert!(
            !xml.contains("DLNA.ORG_OP=01"),
            "plus aucune annonce de seek, got: {xml}"
        );
        assert!(xml.contains("duration="), "la durée reste annoncée");
        assert!(xml.contains("size="), "la taille reste annoncée");
        // Pas les drapeaux senderPaced d'un direct : c'est une piste finie.
        assert!(
            xml.contains("DLNA.ORG_FLAGS=01700000"),
            "drapeaux interactifs d'une piste finie, got: {xml}"
        );
    }

    #[test]
    fn non_live_stream_keeps_file_semantics() {
        let xml = DidlBuilder::new("Track", "http://x/s.flac", "audio/flac")
            .protocol_style(ProtocolStyle::Dlna)
            .duration_ms(200_000)
            .file_size(5_000_000)
            .build();
        assert!(xml.contains("DLNA.ORG_OP=01"));
        assert!(xml.contains("size=\"5000000\""));
        assert!(xml.contains("duration="));
    }

    #[test]
    fn format_duration_didl_works() {
        assert_eq!(format_duration_didl(0), "0:00:00.000");
        assert_eq!(format_duration_didl(256_487), "0:04:16.487");
        assert_eq!(format_duration_didl(3_600_000), "1:00:00.000");
        assert_eq!(format_duration_didl(562_000), "0:09:22.000");
    }

    #[test]
    fn format_duration_hms_works() {
        assert_eq!(format_duration_hms(0), "0:00:00");
        assert_eq!(format_duration_hms(225_000), "0:03:45");
        assert_eq!(format_duration_hms(3_600_000), "1:00:00");
    }

    // Tades, 25/08/2026 : l'artiste multi-valeurs d'un rip Picard/ID3v2.4
    // arrive avec son séparateur NUL (« Jacobs, Lisa\0The String Soloists »).
    // `escape` n'échappe que &<>"' : l'octet 0x00 traversait tel quel, toute
    // l'enveloppe SOAP devenait du XML illégal, et npupnp (upmpdcli — la
    // HiFiMAN Serenade) répondait 401 « Invalid Action » — son code renvoie
    // ce fault dès que le corps ne parse pas.
    #[test]
    fn un_separateur_nul_id3_ne_rend_pas_l_enveloppe_illegale() {
        let didl = DidlBuilder::new(
            "1. Andante: Locatelli Violin Concerto No. 2 in C Minor, Op. 3, No. 2",
            "http://192.168.0.10:8888/stream/x.wav",
            "audio/wav",
        )
        .artist("Jacobs, Lisa\u{0}The String Soloists")
        .album("L'Arte del Violino\u{1}")
        .build_escaped();
        let interdit = didl
            .chars()
            .find(|c| !matches!(c, '\t' | '\n' | '\r') && *c < '\u{20}');
        assert!(
            interdit.is_none(),
            "caractère XML-illégal {:?} dans le DIDL échappé",
            interdit
        );
        // Le séparateur devient un espace lisible, pas une fusion des noms.
        assert!(
            didl.contains("Jacobs, Lisa The String Soloists"),
            "artiste attendu avec séparateur remplacé, didl: {didl}"
        );
    }

    // Les caractères légaux, eux, ne bougent pas : échappement intact, BOM
    // conservé (il est licite en XML 1.0 — dossier DMP-A8).
    #[test]
    fn l_assainissement_ne_touche_pas_aux_caracteres_legaux() {
        let didl = DidlBuilder::new("Rock & Roll", "http://x/s?a=1&b=2", "audio/flac")
            .artist("\u{feff}Jacobs, Lisa")
            .build();
        assert!(didl.contains("Rock &amp; Roll"));
        assert!(didl.contains("a=1&amp;b=2"));
        assert!(didl.contains("\u{feff}Jacobs, Lisa"));
    }
}
