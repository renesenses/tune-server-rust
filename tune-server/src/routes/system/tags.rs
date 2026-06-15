use axum::Json;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize)]
struct TagDef {
    field: &'static str,
    id3v2: &'static str,
    vorbis: &'static str,
    mp4: &'static str,
    access: &'static str,
}

const TRACK_TAGS: &[TagDef] = &[
    TagDef {
        field: "title",
        id3v2: "TIT2",
        vorbis: "TITLE",
        mp4: "\u{00a9}nam",
        access: "read-write",
    },
    TagDef {
        field: "artist_name",
        id3v2: "TPE1",
        vorbis: "ARTIST",
        mp4: "\u{00a9}ART",
        access: "read-write",
    },
    TagDef {
        field: "album_artist",
        id3v2: "TPE2",
        vorbis: "ALBUMARTIST",
        mp4: "aART",
        access: "read-write",
    },
    TagDef {
        field: "album_artist_sort",
        id3v2: "TSO2",
        vorbis: "ALBUMARTISTSORT",
        mp4: "soaa",
        access: "read-write",
    },
    TagDef {
        field: "track_number",
        id3v2: "TRCK",
        vorbis: "TRACKNUMBER",
        mp4: "trkn",
        access: "read-write",
    },
    TagDef {
        field: "total_tracks",
        id3v2: "TRCK",
        vorbis: "TRACKTOTAL",
        mp4: "trkn",
        access: "read-write",
    },
    TagDef {
        field: "disc_number",
        id3v2: "TPOS",
        vorbis: "DISCNUMBER",
        mp4: "disk",
        access: "read-write",
    },
    TagDef {
        field: "total_discs",
        id3v2: "TPOS",
        vorbis: "DISCTOTAL",
        mp4: "disk",
        access: "read-write",
    },
    TagDef {
        field: "disc_subtitle",
        id3v2: "TSST",
        vorbis: "DISCSUBTITLE",
        mp4: "----:com.apple.iTunes:DISCSUBTITLE",
        access: "read-only",
    },
    TagDef {
        field: "year",
        id3v2: "TDRC",
        vorbis: "DATE",
        mp4: "\u{00a9}day",
        access: "read-write",
    },
    TagDef {
        field: "original_year",
        id3v2: "TDOR",
        vorbis: "ORIGINALDATE",
        mp4: "----:com.apple.iTunes:ORIGINALDATE",
        access: "read-write",
    },
    TagDef {
        field: "release_date",
        id3v2: "TDRL",
        vorbis: "DATE",
        mp4: "\u{00a9}day",
        access: "read-write",
    },
    TagDef {
        field: "original_date",
        id3v2: "TDOR",
        vorbis: "ORIGINALDATE",
        mp4: "----:com.apple.iTunes:ORIGINALDATE",
        access: "read-write",
    },
    TagDef {
        field: "genre",
        id3v2: "TCON",
        vorbis: "GENRE",
        mp4: "\u{00a9}gen",
        access: "read-write",
    },
    TagDef {
        field: "genres",
        id3v2: "TCON",
        vorbis: "GENRE",
        mp4: "\u{00a9}gen",
        access: "read-only",
    },
    TagDef {
        field: "composer",
        id3v2: "TCOM",
        vorbis: "COMPOSER",
        mp4: "\u{00a9}wrt",
        access: "read-write",
    },
    TagDef {
        field: "conductor",
        id3v2: "TPE3",
        vorbis: "CONDUCTOR",
        mp4: "----:com.apple.iTunes:CONDUCTOR",
        access: "read-write",
    },
    TagDef {
        field: "lyricist",
        id3v2: "TEXT",
        vorbis: "LYRICIST",
        mp4: "----:com.apple.iTunes:LYRICIST",
        access: "read-write",
    },
    TagDef {
        field: "performer",
        id3v2: "TMCL",
        vorbis: "PERFORMER",
        mp4: "----:com.apple.iTunes:PERFORMER",
        access: "read-write",
    },
    TagDef {
        field: "remixer",
        id3v2: "TPE4",
        vorbis: "REMIXER",
        mp4: "----:com.apple.iTunes:REMIXER",
        access: "read-write",
    },
    TagDef {
        field: "producer",
        id3v2: "TIPL",
        vorbis: "PRODUCER",
        mp4: "----:com.apple.iTunes:PRODUCER",
        access: "read-write",
    },
    TagDef {
        field: "bpm",
        id3v2: "TBPM",
        vorbis: "BPM",
        mp4: "tmpo",
        access: "read-write",
    },
    TagDef {
        field: "compilation",
        id3v2: "TCMP",
        vorbis: "COMPILATION",
        mp4: "cpil",
        access: "read-write",
    },
    TagDef {
        field: "comment",
        id3v2: "COMM",
        vorbis: "COMMENT",
        mp4: "\u{00a9}cmt",
        access: "read-write",
    },
    TagDef {
        field: "lyrics",
        id3v2: "USLT",
        vorbis: "LYRICS",
        mp4: "\u{00a9}lyr",
        access: "read-write",
    },
    TagDef {
        field: "mood",
        id3v2: "TMOO",
        vorbis: "MOOD",
        mp4: "----:com.apple.iTunes:MOOD",
        access: "read-write",
    },
    TagDef {
        field: "grouping",
        id3v2: "TIT1",
        vorbis: "GROUPING",
        mp4: "\u{00a9}grp",
        access: "read-write",
    },
    TagDef {
        field: "isrc",
        id3v2: "TSRC",
        vorbis: "ISRC",
        mp4: "----:com.apple.iTunes:ISRC",
        access: "read-write",
    },
    TagDef {
        field: "label",
        id3v2: "TPUB",
        vorbis: "LABEL",
        mp4: "----:com.apple.iTunes:LABEL",
        access: "read-write",
    },
    TagDef {
        field: "catalog_number",
        id3v2: "TXXX:CATALOGNUMBER",
        vorbis: "CATALOGNUMBER",
        mp4: "----:com.apple.iTunes:CATALOGNUMBER",
        access: "read-write",
    },
    TagDef {
        field: "barcode",
        id3v2: "TXXX:BARCODE",
        vorbis: "BARCODE",
        mp4: "----:com.apple.iTunes:BARCODE",
        access: "read-write",
    },
    TagDef {
        field: "copyright",
        id3v2: "TCOP",
        vorbis: "COPYRIGHT",
        mp4: "cprt",
        access: "read-write",
    },
    TagDef {
        field: "language",
        id3v2: "TLAN",
        vorbis: "LANGUAGE",
        mp4: "----:com.apple.iTunes:LANGUAGE",
        access: "read-write",
    },
    TagDef {
        field: "encoder",
        id3v2: "TENC",
        vorbis: "ENCODER",
        mp4: "\u{00a9}too",
        access: "read-write",
    },
    TagDef {
        field: "media_type",
        id3v2: "TMED",
        vorbis: "MEDIA",
        mp4: "----:com.apple.iTunes:MEDIA",
        access: "read-write",
    },
    TagDef {
        field: "cover_art",
        id3v2: "APIC",
        vorbis: "METADATA_BLOCK_PICTURE",
        mp4: "covr",
        access: "read-only",
    },
];

const MUSICBRAINZ_TAGS: &[TagDef] = &[
    TagDef {
        field: "musicbrainz_recording_id",
        id3v2: "TXXX:MusicBrainz Recording Id",
        vorbis: "MUSICBRAINZ_TRACKID",
        mp4: "----:com.apple.iTunes:MusicBrainz Track Id",
        access: "read-write",
    },
    TagDef {
        field: "musicbrainz_release_id",
        id3v2: "TXXX:MusicBrainz Album Id",
        vorbis: "MUSICBRAINZ_ALBUMID",
        mp4: "----:com.apple.iTunes:MusicBrainz Album Id",
        access: "read-write",
    },
    TagDef {
        field: "musicbrainz_artist_id",
        id3v2: "TXXX:MusicBrainz Artist Id",
        vorbis: "MUSICBRAINZ_ARTISTID",
        mp4: "----:com.apple.iTunes:MusicBrainz Artist Id",
        access: "read-write",
    },
    TagDef {
        field: "musicbrainz_album_artist_id",
        id3v2: "TXXX:MusicBrainz Album Artist Id",
        vorbis: "MUSICBRAINZ_ALBUMARTISTID",
        mp4: "----:com.apple.iTunes:MusicBrainz Album Artist Id",
        access: "read-write",
    },
    TagDef {
        field: "musicbrainz_release_group_id",
        id3v2: "TXXX:MusicBrainz Release Group Id",
        vorbis: "MUSICBRAINZ_RELEASEGROUPID",
        mp4: "----:com.apple.iTunes:MusicBrainz Release Group Id",
        access: "read-write",
    },
    TagDef {
        field: "musicbrainz_work_id",
        id3v2: "TXXX:MusicBrainz Work Id",
        vorbis: "MUSICBRAINZ_WORKID",
        mp4: "----:com.apple.iTunes:MusicBrainz Work Id",
        access: "read-write",
    },
];

const REPLAYGAIN_TAGS: &[TagDef] = &[
    TagDef {
        field: "rg_track_gain",
        id3v2: "TXXX:REPLAYGAIN_TRACK_GAIN",
        vorbis: "REPLAYGAIN_TRACK_GAIN",
        mp4: "----:com.apple.iTunes:REPLAYGAIN_TRACK_GAIN",
        access: "read-write",
    },
    TagDef {
        field: "rg_track_peak",
        id3v2: "TXXX:REPLAYGAIN_TRACK_PEAK",
        vorbis: "REPLAYGAIN_TRACK_PEAK",
        mp4: "----:com.apple.iTunes:REPLAYGAIN_TRACK_PEAK",
        access: "read-write",
    },
    TagDef {
        field: "rg_album_gain",
        id3v2: "TXXX:REPLAYGAIN_ALBUM_GAIN",
        vorbis: "REPLAYGAIN_ALBUM_GAIN",
        mp4: "----:com.apple.iTunes:REPLAYGAIN_ALBUM_GAIN",
        access: "read-write",
    },
    TagDef {
        field: "rg_album_peak",
        id3v2: "TXXX:REPLAYGAIN_ALBUM_PEAK",
        vorbis: "REPLAYGAIN_ALBUM_PEAK",
        mp4: "----:com.apple.iTunes:REPLAYGAIN_ALBUM_PEAK",
        access: "read-write",
    },
];

const SORT_ORDER_TAGS: &[TagDef] = &[
    TagDef {
        field: "sort_artist",
        id3v2: "TSOP",
        vorbis: "ARTISTSORT",
        mp4: "soar",
        access: "read-write",
    },
    TagDef {
        field: "sort_album",
        id3v2: "TSOA",
        vorbis: "ALBUMSORT",
        mp4: "soal",
        access: "read-write",
    },
    TagDef {
        field: "sort_album_artist",
        id3v2: "TSO2",
        vorbis: "ALBUMARTISTSORT",
        mp4: "soaa",
        access: "read-write",
    },
];

const AUDIO_PROPERTIES: &[TagDef] = &[
    TagDef {
        field: "duration",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
    TagDef {
        field: "sample_rate",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
    TagDef {
        field: "bit_depth",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
    TagDef {
        field: "channels",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
    TagDef {
        field: "format",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
    TagDef {
        field: "file_size",
        id3v2: "",
        vorbis: "",
        mp4: "",
        access: "read-only",
    },
];

const ALBUM_TAGS: &[TagDef] = &[
    TagDef {
        field: "title",
        id3v2: "TALB",
        vorbis: "ALBUM",
        mp4: "\u{00a9}alb",
        access: "read-write",
    },
    TagDef {
        field: "artist_name",
        id3v2: "TPE2",
        vorbis: "ALBUMARTIST",
        mp4: "aART",
        access: "read-write",
    },
    TagDef {
        field: "year",
        id3v2: "TDRC",
        vorbis: "DATE",
        mp4: "\u{00a9}day",
        access: "read-write",
    },
    TagDef {
        field: "original_year",
        id3v2: "TDOR",
        vorbis: "ORIGINALDATE",
        mp4: "----:com.apple.iTunes:ORIGINALDATE",
        access: "read-write",
    },
    TagDef {
        field: "genre",
        id3v2: "TCON",
        vorbis: "GENRE",
        mp4: "\u{00a9}gen",
        access: "read-write",
    },
    TagDef {
        field: "label",
        id3v2: "TPUB",
        vorbis: "LABEL",
        mp4: "----:com.apple.iTunes:LABEL",
        access: "read-write",
    },
    TagDef {
        field: "catalog_number",
        id3v2: "TXXX:CATALOGNUMBER",
        vorbis: "CATALOGNUMBER",
        mp4: "----:com.apple.iTunes:CATALOGNUMBER",
        access: "read-write",
    },
    TagDef {
        field: "barcode",
        id3v2: "TXXX:BARCODE",
        vorbis: "BARCODE",
        mp4: "----:com.apple.iTunes:BARCODE",
        access: "read-write",
    },
    TagDef {
        field: "compilation",
        id3v2: "TCMP",
        vorbis: "COMPILATION",
        mp4: "cpil",
        access: "read-write",
    },
    TagDef {
        field: "total_tracks",
        id3v2: "TRCK",
        vorbis: "TRACKTOTAL",
        mp4: "trkn",
        access: "read-only",
    },
    TagDef {
        field: "total_discs",
        id3v2: "TPOS",
        vorbis: "DISCTOTAL",
        mp4: "disk",
        access: "read-only",
    },
    TagDef {
        field: "cover_art",
        id3v2: "APIC",
        vorbis: "METADATA_BLOCK_PICTURE",
        mp4: "covr",
        access: "read-only",
    },
];

fn tags_to_json(tags: &[TagDef]) -> Vec<Value> {
    tags.iter()
        .map(|t| {
            json!({
                "field": t.field,
                "id3v2": t.id3v2,
                "vorbis": t.vorbis,
                "mp4": t.mp4,
                "access": t.access,
            })
        })
        .collect()
}

pub(super) async fn supported_tags() -> Json<Value> {
    Json(json!({
        "track": tags_to_json(TRACK_TAGS),
        "album": tags_to_json(ALBUM_TAGS),
        "musicbrainz": tags_to_json(MUSICBRAINZ_TAGS),
        "replaygain": tags_to_json(REPLAYGAIN_TAGS),
        "sort_order": tags_to_json(SORT_ORDER_TAGS),
        "audio_properties": tags_to_json(AUDIO_PROPERTIES),
    }))
}
