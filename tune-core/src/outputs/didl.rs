//! Shared DIDL-Lite XML builder for DLNA/UPnP/OpenHome outputs.
//!
//! DIDL-Lite (Digital Item Declaration Language) is the XML format used by
//! DLNA/UPnP to describe media items. This module provides a single reusable
//! builder so that all output modules produce consistent, valid DIDL-Lite.

use quick_xml::escape::{escape, partial_escape};

/// DLNA flags string for a given MIME type.
///
/// Returns `protocolInfo` 4th-field with DLNA profile name, operation flags,
/// transcoding indicator and streaming flags.
pub fn dlna_flags_for_mime(mime: &str) -> &'static str {
    // DLNA.ORG_OP=01 : byte-range seek supported
    // DLNA.ORG_CI=0  : no transcoding
    // DLNA.ORG_FLAGS : streaming + interactive + background + v1.5
    match mime {
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
        _ => "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
    }
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
        }
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
        let title = escape(&self.title);
        let escaped_url = escape(&self.resource_url);
        let escaped_id = escape(&self.item_id);
        let escaped_pid = escape(&self.parent_id);

        // Artist tags
        let artist_tags = if is_valid_meta(self.artist.as_deref()) {
            let a = escape(self.artist.as_deref().unwrap());
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
            .map(|a| format!("<upnp:album>{}</upnp:album>", escape(a)))
            .unwrap_or_default();

        // Album art URI
        let art_tag = self
            .album_art_uri
            .as_deref()
            .filter(|c| is_valid_meta(Some(c)))
            .map(|c| {
                let c = escape(c);
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
                let flags = dlna_flags_for_mime(&self.mime_type);
                format!("http-get:*:{}:{}", self.mime_type, flags)
            }
            ProtocolStyle::Simple => format!("http-get:*:{}:*", self.mime_type),
        };

        // Res attributes
        let dur_attr = self
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| format!(" duration=\"{}\"", format_duration_didl(d)))
            .unwrap_or_default();

        let size_attr = self
            .file_size
            .map(|s| format!(" size=\"{s}\""))
            .unwrap_or_default();

        let sr_attr = self
            .sample_rate
            .map(|sr| format!(" sampleFrequency=\"{sr}\""))
            .unwrap_or_default();

        let bd_attr = self
            .bit_depth
            .map(|bd| format!(" bitsPerSample=\"{bd}\""))
            .unwrap_or_default();

        let ch_attr = self
            .channels
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
    fn dlna_flags_mp3() {
        assert!(dlna_flags_for_mime("audio/mpeg").contains("DLNA.ORG_PN=MP3"));
    }

    #[test]
    fn dlna_flags_aac() {
        assert!(dlna_flags_for_mime("audio/mp4").contains("DLNA.ORG_PN=AAC_ISO"));
        assert!(dlna_flags_for_mime("audio/aac").contains("DLNA.ORG_PN=AAC_ISO"));
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
}
