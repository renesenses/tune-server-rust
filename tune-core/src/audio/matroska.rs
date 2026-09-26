//! Matroska (`.mkv`, `.mka`, `.webm`) : la sonde de la piste audio, et ce que
//! le conteneur dit de lui-même (#3633, point 2 de l'arbitrage du 16/09).
//!
//! # Ce que ce module décide, et ce qu'il ne décide pas
//!
//! symphonia démuxe le Matroska depuis toujours (feature `mkv` de
//! `Cargo.toml`, en service pour l'Opus-in-WebM de YouTube). Ce qui manquait
//! n'était pas le démuxeur : c'était la QUESTION posée au fichier. Un `.mkv`
//! de concert porte tantôt du FLAC ou du PCM — que symphonia décode —, tantôt
//! de l'AC-3, de l'E-AC-3 ou du TrueHD, qu'aucun décodeur livré ne lit. Le
//! catalogue ne peut pas trancher à l'extension : il faut ouvrir le fichier
//! et lire le `CodecID` de sa première piste audio. C'est [`sonder`].
//!
//! La sonde ne décode AUCUNE trame : elle lit l'en-tête EBML, l'élément
//! `Tracks`, les balises et la durée, puis rend. Sur un NAS, c'est quelques
//! kilo-octets ; le scanner ne l'appelle que dans la phase de métadonnées,
//! derrière son délai maximal, jamais pendant l'énumération des dossiers.
//!
//! Le verdict « décodable » est celui du REGISTRE de symphonia
//! (`get_codecs().get_audio_decoder(id)`), plus libopus pour l'Opus : la
//! liste n'est pas recopiée ici, elle est LUE. Un codec ajouté à symphonia
//! demain sera admis sans qu'une ligne change.
//!
//! Le point 3 de l'arbitrage — décoder AC-3/E-AC-3/TrueHD — est hors feuille
//! de route et hors de ce module : ces fichiers restent comptés-nommés, le
//! codec dans le motif.

use std::path::Path;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioCodecId;
use symphonia::core::codecs::audio::well_known::*;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};

/// Les extensions que ce module reconnaît comme un conteneur Matroska.
///
/// `webm`/`weba` sont du Matroska restreint (Opus/Vorbis) : même démuxeur,
/// même sonde. Ils étaient reconnus par le DÉCODEUR (`decode.rs`) mais par
/// aucune liste du scanner — un `.webm` local n'entrait pas en bibliothèque
/// alors que Tune savait le lire (#3633, note de PR #3932).
pub const EXTENSIONS: &[&str] = &["mkv", "mka", "webm", "weba"];

/// L'extension, en minuscules, désigne-t-elle un conteneur Matroska ?
pub fn est_extension_matroska(ext: &str) -> bool {
    EXTENSIONS.contains(&ext)
}

/// Le chemin porte-t-il une extension Matroska ?
pub fn est_chemin_matroska(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| est_extension_matroska(&e.to_ascii_lowercase()))
}

/// Ce que la première piste audio du conteneur permet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PisteAudio {
    /// Un décodeur livré la lit : symphonia (FLAC, PCM, Vorbis, AAC, ALAC,
    /// MP3…) ou libopus (`opus == true`).
    Decodable { codec: String, opus: bool },
    /// Le conteneur la nomme, mais aucun décodeur livré ne la lit — AC-3,
    /// E-AC-3, TrueHD, DTS… Le nom est celui qui part dans le rapport.
    NonDecodable { codec: String },
    /// Aucune piste audio que symphonia sache nommer : MKV vidéo seul, ou
    /// `CodecID` inconnu du démuxeur (`A_MS/ACM`, `A_DTS/LOSSLESS`…).
    Aucune,
}

/// Les balises Matroska que la bibliothèque sait ranger.
///
/// Le Matroska porte ses balises par CIBLE (`TargetTypeValue` 50 = album,
/// 30 = piste) ; symphonia les a déjà résolues en [`StandardTag`] — un
/// `TITLE` de cible 50 est un album, de cible 30 un titre de piste.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BalisesMatroska {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    /// `DATE_RELEASED`, tel quel — l'année s'en extrait.
    pub release_date: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
}

/// Ce que la sonde rend d'un Matroska, sans avoir décodé une trame.
#[derive(Debug, Clone)]
pub struct SondeMatroska {
    pub piste: PisteAudio,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub bit_depth: Option<u16>,
    /// La durée annoncée par `Info/Duration`, en millisecondes.
    pub duration_ms: Option<u64>,
    pub balises: BalisesMatroska,
}

impl SondeMatroska {
    /// La première piste audio est-elle lisible par un décodeur livré ?
    pub fn est_decodable(&self) -> bool {
        matches!(self.piste, PisteAudio::Decodable { .. })
    }

    /// La première piste audio est-elle de l'Opus — donc à décoder par
    /// libopus, que symphonia n'a pas ?
    pub fn est_opus(&self) -> bool {
        matches!(self.piste, PisteAudio::Decodable { opus: true, .. })
    }
}

/// Le nom court d'un codec que symphonia sait NOMMER sans savoir le décoder.
///
/// Pour un codec décodable, le nom vient du registre (`CodecInfo::short_name`)
/// et cette table n'est pas consultée. Elle ne sert qu'au motif de refus :
/// « mkv-codec-non-decodable-ac3 » doit dire « ac3 », pas « 0x1008 ».
fn nom_du_codec(id: AudioCodecId) -> String {
    let nom = match id {
        CODEC_ID_AC3 => "ac3",
        CODEC_ID_EAC3 => "eac3",
        CODEC_ID_AC4 => "ac4",
        CODEC_ID_TRUEHD => "truehd",
        CODEC_ID_DCA => "dts",
        CODEC_ID_MP1 => "mp1",
        CODEC_ID_MP2 => "mp2",
        CODEC_ID_MP3 => "mp3",
        CODEC_ID_AAC => "aac",
        CODEC_ID_FLAC => "flac",
        CODEC_ID_VORBIS => "vorbis",
        CODEC_ID_OPUS => "opus",
        CODEC_ID_ALAC => "alac",
        CODEC_ID_WAVPACK => "wavpack",
        CODEC_ID_TTA => "tta",
        CODEC_ID_MUSEPACK => "musepack",
        CODEC_ID_WMA => "wma",
        CODEC_ID_ATRAC1 => "atrac1",
        CODEC_ID_ATRAC3 => "atrac3",
        CODEC_ID_RA10 | CODEC_ID_RA20 | CODEC_ID_COOK | CODEC_ID_SIPR | CODEC_ID_RALF => {
            "realaudio"
        }
        // Les PCM sont tous au registre : ils ne passent jamais ici.
        autre => return format!("{autre}"),
    };
    nom.to_string()
}

/// Classe la piste audio par défaut du conteneur : décodable, nommée mais non
/// décodable, ou absente.
fn classer_la_piste(format: &dyn FormatReader) -> (PisteAudio, Option<&CodecParameters>) {
    let Some(track) = format.default_track(TrackType::Audio) else {
        return (PisteAudio::Aucune, None);
    };
    let Some(params @ CodecParameters::Audio(audio)) = track.codec_params.as_ref() else {
        return (PisteAudio::Aucune, None);
    };
    let id = audio.codec;
    let piste = if id == CODEC_ID_OPUS {
        // symphonia n'a pas de codec Opus : c'est libopus qui décode, comme
        // pour `.opus` et l'Opus-in-WebM de YouTube (`decode_opus_to_pcm`).
        PisteAudio::Decodable {
            codec: "opus".to_string(),
            opus: true,
        }
    } else if let Some(decodeur) = symphonia::default::get_codecs().get_audio_decoder(id) {
        PisteAudio::Decodable {
            codec: decodeur.codec.info.short_name.to_ascii_lowercase(),
            opus: false,
        }
    } else {
        PisteAudio::NonDecodable {
            codec: nom_du_codec(id),
        }
    };
    (piste, Some(params))
}

/// Ramasse, dans une révision de balises, celles que la bibliothèque range.
///
/// Première valeur rencontrée gardée : les balises du média d'abord, puis
/// celles rattachées aux pistes. Sur un fichier à une seule piste audio, les
/// deux disent la même chose.
fn ramasser_les_balises(revision: &MetadataRevision, dans: &mut BalisesMatroska) {
    let media = revision.media.tags.iter();
    let pistes = revision
        .per_track
        .iter()
        .flat_map(|piste| piste.metadata.tags.iter());
    for tag in media.chain(pistes) {
        let Some(std) = tag.std.as_ref() else {
            continue;
        };
        let texte = |s: &std::sync::Arc<String>| Some(s.as_str().trim().to_string());
        match std {
            StandardTag::TrackTitle(v) => dans.title = dans.title.take().or_else(|| texte(v)),
            StandardTag::Artist(v) => dans.artist = dans.artist.take().or_else(|| texte(v)),
            StandardTag::Album(v) => dans.album = dans.album.take().or_else(|| texte(v)),
            StandardTag::AlbumArtist(v) => {
                dans.album_artist = dans.album_artist.take().or_else(|| texte(v))
            }
            StandardTag::TrackNumber(n) => {
                dans.track_number = dans.track_number.or(u32::try_from(*n).ok())
            }
            StandardTag::TrackTotal(n) => {
                dans.track_total = dans.track_total.or(u32::try_from(*n).ok())
            }
            StandardTag::DiscNumber(n) => {
                dans.disc_number = dans.disc_number.or(u32::try_from(*n).ok())
            }
            StandardTag::DiscTotal(n) => {
                dans.disc_total = dans.disc_total.or(u32::try_from(*n).ok())
            }
            StandardTag::ReleaseDate(v) | StandardTag::RecordingDate(v) => {
                dans.release_date = dans.release_date.take().or_else(|| texte(v))
            }
            StandardTag::Genre(v) => dans.genre = dans.genre.take().or_else(|| texte(v)),
            StandardTag::Comment(v) => dans.comment = dans.comment.take().or_else(|| texte(v)),
            _ => {}
        }
    }
    // Une balise vide n'est pas une balise.
    for champ in [
        &mut dans.title,
        &mut dans.artist,
        &mut dans.album,
        &mut dans.album_artist,
        &mut dans.release_date,
        &mut dans.genre,
        &mut dans.comment,
    ] {
        if champ.as_deref().is_some_and(str::is_empty) {
            *champ = None;
        }
    }
}

/// Sonde un conteneur Matroska : la première piste audio, ses paramètres, la
/// durée et les balises. Ne décode aucune trame.
///
/// `Err` quand le fichier ne s'ouvre pas ou n'est pas un Matroska que
/// symphonia sache lire — c'est un fichier ILLISIBLE, pas un format non pris
/// en charge, et l'appelant doit le compter comme tel.
pub fn sonder(path: &Path) -> Result<SondeMatroska, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("mkv open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format: Box<dyn FormatReader> = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("mkv probe: {e}"))?;

    let (piste, params) = classer_la_piste(format.as_ref());
    let (sample_rate, channels, bit_depth) = match params {
        Some(CodecParameters::Audio(audio)) => (
            audio.sample_rate,
            audio
                .channels
                .as_ref()
                .and_then(|c| u16::try_from(c.count()).ok()),
            audio.bits_per_sample.and_then(|b| u16::try_from(b).ok()),
        ),
        _ => (None, None, None),
    };

    // `Info/Duration`, en unités de la base de temps du média.
    let info = format.media_info();
    let duration_ms = match (info.time_base, info.duration) {
        (Some(tb), Some(dur)) => {
            let ms = dur.get() as u128 * tb.numer.get() as u128 * 1000 / tb.denom.get() as u128;
            u64::try_from(ms).ok().filter(|ms| *ms > 0)
        }
        _ => None,
    };

    let mut balises = BalisesMatroska::default();
    if let Some(revision) = format.metadata().skip_to_latest() {
        ramasser_les_balises(revision, &mut balises);
    }

    Ok(SondeMatroska {
        piste,
        sample_rate,
        channels,
        bit_depth,
        duration_ms,
        balises,
    })
}

/// La première piste audio de ce Matroska est-elle de l'Opus ?
///
/// C'est la question que le routage de `decode.rs` pose avant de choisir
/// entre libopus et symphonia. `false` quand la sonde échoue : symphonia
/// rendra alors son propre motif, le même que sur n'importe quel fichier
/// abîmé.
pub fn piste_audio_est_opus(file_path: &str) -> bool {
    sonder(Path::new(file_path)).is_ok_and(|s| s.est_opus())
}

/// Un mini-muxer EBML, pour les témoins seulement.
///
/// ffmpeg est banni du dépôt, Shrek n'a ni `mkvmerge` ni `flac` : les fixtures
/// Matroska se FABRIQUENT ici, en Rust, à partir des `.flac` de référence déjà
/// gardés bit pour bit par `tests/flac_empreintes_reference.rs`. Un segment,
/// une piste, un `SimpleBlock` par trame FLAC — exactement ce que produirait
/// `mkvmerge -o x.mka x.flac`, sans les éléments facultatifs.
#[cfg(test)]
pub(crate) mod muxer_de_test {
    use std::path::Path;

    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    // Identifiants EBML/Matroska, tels que dans `symphonia-format-mkv/schema.rs`.
    const EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
    const EBML_VERSION: &[u8] = &[0x42, 0x86];
    const EBML_READ_VERSION: &[u8] = &[0x42, 0xF7];
    const EBML_MAX_ID_LENGTH: &[u8] = &[0x42, 0xF2];
    const EBML_MAX_SIZE_LENGTH: &[u8] = &[0x42, 0xF3];
    const DOC_TYPE: &[u8] = &[0x42, 0x82];
    const DOC_TYPE_VERSION: &[u8] = &[0x42, 0x87];
    const DOC_TYPE_READ_VERSION: &[u8] = &[0x42, 0x85];
    const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
    const INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
    const TIMESTAMP_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
    const DURATION: &[u8] = &[0x44, 0x89];
    const MUXING_APP: &[u8] = &[0x4D, 0x80];
    const WRITING_APP: &[u8] = &[0x57, 0x41];
    const TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
    const TRACK_ENTRY: &[u8] = &[0xAE];
    const TRACK_NUMBER: &[u8] = &[0xD7];
    const TRACK_UID: &[u8] = &[0x73, 0xC5];
    const TRACK_TYPE: &[u8] = &[0x83];
    const FLAG_DEFAULT: &[u8] = &[0x88];
    const DEFAULT_DURATION: &[u8] = &[0x23, 0xE3, 0x83];
    const CODEC_ID: &[u8] = &[0x86];
    const CODEC_PRIVATE: &[u8] = &[0x63, 0xA2];
    const AUDIO: &[u8] = &[0xE1];
    const SAMPLING_FREQUENCY: &[u8] = &[0xB5];
    const CHANNELS: &[u8] = &[0x9F];
    const BIT_DEPTH: &[u8] = &[0x62, 0x64];
    const CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
    const TIMESTAMP: &[u8] = &[0xE7];
    const SIMPLE_BLOCK: &[u8] = &[0xA3];
    const CUES: &[u8] = &[0x1C, 0x53, 0xBB, 0x6B];
    const CUE_POINT: &[u8] = &[0xBB];
    const CUE_TIME: &[u8] = &[0xB3];
    const CUE_TRACK_POSITIONS: &[u8] = &[0xB7];
    const CUE_TRACK: &[u8] = &[0xF7];
    const CUE_CLUSTER_POSITION: &[u8] = &[0xF1];
    const TAGS: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
    const TAG: &[u8] = &[0x73, 0x73];
    const TARGETS: &[u8] = &[0x63, 0xC0];
    const TARGET_TYPE_VALUE: &[u8] = &[0x68, 0xCA];
    const SIMPLE_TAG: &[u8] = &[0x67, 0xC8];
    const TAG_NAME: &[u8] = &[0x45, 0xA3];
    const TAG_STRING: &[u8] = &[0x44, 0x87];

    /// Taille EBML sur 8 octets, toujours : `0x01` puis 7 octets gros-boutistes.
    /// Valide (`EBMLMaxSizeLength = 8`) et sans cas particulier.
    fn taille(n: usize) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = 0x01;
        out[1..].copy_from_slice(&(n as u64).to_be_bytes()[1..]);
        out
    }

    fn element(id: &[u8], charge: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(id.len() + 8 + charge.len());
        out.extend_from_slice(id);
        out.extend_from_slice(&taille(charge.len()));
        out.extend_from_slice(charge);
        out
    }

    fn entier(id: &[u8], v: u64) -> Vec<u8> {
        let octets = v.to_be_bytes();
        let debut = octets.iter().position(|b| *b != 0).unwrap_or(7);
        element(id, &octets[debut..])
    }

    fn flottant(id: &[u8], v: f64) -> Vec<u8> {
        element(id, &v.to_be_bytes())
    }

    fn chaine(id: &[u8], s: &str) -> Vec<u8> {
        element(id, s.as_bytes())
    }

    /// Un `SimpleBlock` sans lacing : piste 1, horodatage relatif signé sur
    /// 16 bits, drapeau « image clé ».
    fn bloc_simple(ts_relatif_ms: i16, trame: &[u8]) -> Vec<u8> {
        let mut charge = Vec::with_capacity(4 + trame.len());
        charge.push(0x81); // numéro de piste 1, en varint EBML
        charge.extend_from_slice(&ts_relatif_ms.to_be_bytes());
        charge.push(0x80);
        charge.extend_from_slice(trame);
        element(SIMPLE_BLOCK, &charge)
    }

    fn en_tete_ebml(doc_type: &str) -> Vec<u8> {
        let mut charge = Vec::new();
        charge.extend(entier(EBML_VERSION, 1));
        charge.extend(entier(EBML_READ_VERSION, 1));
        charge.extend(entier(EBML_MAX_ID_LENGTH, 4));
        charge.extend(entier(EBML_MAX_SIZE_LENGTH, 8));
        charge.extend(chaine(DOC_TYPE, doc_type));
        charge.extend(entier(DOC_TYPE_VERSION, 4));
        charge.extend(entier(DOC_TYPE_READ_VERSION, 2));
        element(EBML, &charge)
    }

    /// `Tags` : les balises d'album (cible 50) et de piste (cible 30), sous la
    /// forme que symphonia résout en `StandardTag`.
    fn balises(album: &[(&str, &str)], piste: &[(&str, &str)]) -> Vec<u8> {
        let mut charge = Vec::new();
        for (cible, paires) in [(50u64, album), (30u64, piste)] {
            if paires.is_empty() {
                continue;
            }
            let mut tag = element(TARGETS, &entier(TARGET_TYPE_VALUE, cible));
            for (nom, valeur) in paires {
                let mut simple = chaine(TAG_NAME, nom);
                simple.extend(chaine(TAG_STRING, valeur));
                tag.extend(element(SIMPLE_TAG, &simple));
            }
            charge.extend(element(TAG, &tag));
        }
        element(TAGS, &charge)
    }

    /// Une trame et son horodatage en millisecondes.
    pub(crate) struct Trame {
        pub ts_ms: u64,
        pub octets: Vec<u8>,
    }

    /// Les paramètres de la piste unique du conteneur fabriqué.
    pub(crate) struct Piste<'a> {
        pub codec_id: &'a str,
        pub codec_private: Option<&'a [u8]>,
        pub sample_rate: u32,
        pub channels: u16,
        pub bit_depth: Option<u16>,
        /// Durée d'une trame en nanosecondes (`DefaultDuration`).
        pub duree_trame_ns: Option<u64>,
    }

    /// Fabrique un Matroska complet : un segment, une piste, les trames
    /// réparties en `clusters` clusters, et — si demandé — un `Cues` qui pointe
    /// chaque cluster.
    // Fabrique d'épreuve, un argument par champ du banc (clippy 1.98).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn matroska(
        doc_type: &str,
        piste: &Piste<'_>,
        duree_ms: f64,
        trames: &[Trame],
        clusters: usize,
        avec_cues: bool,
        balises_album: &[(&str, &str)],
        balises_piste: &[(&str, &str)],
    ) -> Vec<u8> {
        let mut info = entier(TIMESTAMP_SCALE, 1_000_000);
        info.extend(flottant(DURATION, duree_ms));
        info.extend(chaine(MUXING_APP, "tune-core muxer_de_test"));
        info.extend(chaine(WRITING_APP, "tune-core muxer_de_test (#3633)"));
        let info = element(INFO, &info);

        let mut entree = entier(TRACK_NUMBER, 1);
        entree.extend(entier(TRACK_UID, 1));
        entree.extend(entier(TRACK_TYPE, 2));
        entree.extend(entier(FLAG_DEFAULT, 1));
        if let Some(ns) = piste.duree_trame_ns {
            entree.extend(entier(DEFAULT_DURATION, ns));
        }
        entree.extend(chaine(CODEC_ID, piste.codec_id));
        if let Some(prive) = piste.codec_private {
            entree.extend(element(CODEC_PRIVATE, prive));
        }
        let mut audio = flottant(SAMPLING_FREQUENCY, piste.sample_rate as f64);
        audio.extend(entier(CHANNELS, piste.channels as u64));
        if let Some(bd) = piste.bit_depth {
            audio.extend(entier(BIT_DEPTH, bd as u64));
        }
        entree.extend(element(AUDIO, &audio));
        let tracks = element(TRACKS, &element(TRACK_ENTRY, &entree));

        let tags = balises(balises_album, balises_piste);

        // Les clusters, et la position de chacun RELATIVE au début des données
        // du segment — c'est ce que `CueClusterPosition` désigne.
        let mut corps = Vec::new();
        corps.extend(&info);
        corps.extend(&tracks);
        corps.extend(&tags);
        let par_cluster = trames.len().div_ceil(clusters.max(1)).max(1);
        let mut positions = Vec::new();
        for lot in trames.chunks(par_cluster) {
            let base_ms = lot[0].ts_ms;
            let mut charge = entier(TIMESTAMP, base_ms);
            for trame in lot {
                let relatif = i16::try_from(trame.ts_ms - base_ms).expect("cluster trop long");
                charge.extend(bloc_simple(relatif, &trame.octets));
            }
            positions.push((base_ms, corps.len() as u64));
            corps.extend(element(CLUSTER, &charge));
        }
        if avec_cues {
            let mut cues = Vec::new();
            for (temps_ms, position) in positions {
                let mut pos = entier(CUE_TRACK, 1);
                pos.extend(entier(CUE_CLUSTER_POSITION, position));
                let mut point = entier(CUE_TIME, temps_ms);
                point.extend(element(CUE_TRACK_POSITIONS, &pos));
                cues.extend(element(CUE_POINT, &point));
            }
            corps.extend(element(CUES, &cues));
        }

        let mut fichier = en_tete_ebml(doc_type);
        fichier.extend(element(SEGMENT, &corps));
        fichier
    }

    /// Ré-emballe un `.flac` dans un Matroska : `CodecPrivate` = en-tête
    /// `fLaC` + `STREAMINFO`, un `SimpleBlock` par trame, telle que le
    /// démuxeur FLAC de symphonia la découpe (recherche de sync + CRC-16 —
    /// c'est lui qui connaît les frontières de trame, pas ce module).
    pub(crate) fn mka_depuis_flac(
        flac: &Path,
        clusters: usize,
        avec_cues: bool,
        balises_album: &[(&str, &str)],
        balises_piste: &[(&str, &str)],
    ) -> Vec<u8> {
        let file = std::fs::File::open(flac).expect("ouvrir la fixture FLAC");
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("flac");
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .expect("sonder la fixture FLAC");
        let track = format
            .default_track(TrackType::Audio)
            .expect("piste FLAC")
            .clone();
        let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
            panic!("la fixture FLAC n'a pas de paramètres audio");
        };
        let sample_rate = params.sample_rate.expect("cadence FLAC");
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .expect("canaux FLAC");
        let bit_depth = params.bits_per_sample.map(|b| b as u16);
        let streaminfo = params.extra_data.as_ref().expect("STREAMINFO");

        // `fLaC`, puis l'en-tête de bloc STREAMINFO (dernier bloc, type 0,
        // longueur sur 24 bits), puis le bloc lui-même.
        let mut codec_private = b"fLaC".to_vec();
        codec_private.push(0x80);
        codec_private.extend_from_slice(&(streaminfo.len() as u32).to_be_bytes()[1..]);
        codec_private.extend_from_slice(streaminfo);

        let mut trames = Vec::new();
        let mut duree_trame_ns = None;
        let mut total_frames: u64 = 0;
        while let Ok(Some(paquet)) = format.next_packet() {
            if paquet.track_id != track.id {
                continue;
            }
            let ts = paquet.pts.get().max(0) as u64;
            let dur = paquet.dur.get();
            if duree_trame_ns.is_none() && dur > 0 {
                duree_trame_ns = Some(dur * 1_000_000_000 / sample_rate as u64);
            }
            total_frames = total_frames.max(ts + dur);
            trames.push(Trame {
                ts_ms: ts * 1000 / sample_rate as u64,
                octets: paquet.data.to_vec(),
            });
        }
        assert!(
            trames.len() >= 2,
            "la fixture FLAC doit porter plusieurs trames pour éprouver le seek"
        );
        let duree_ms = total_frames as f64 * 1000.0 / sample_rate as f64;

        matroska(
            "matroska",
            &Piste {
                codec_id: "A_FLAC",
                codec_private: Some(&codec_private),
                sample_rate,
                channels,
                bit_depth,
                duree_trame_ns,
            },
            duree_ms,
            &trames,
            clusters,
            avec_cues,
            balises_album,
            balises_piste,
        )
    }

    /// Un Matroska structurellement valide dont la piste audio est annoncée
    /// `codec_id` (par exemple `A_AC3`), avec des blocs opaques. Sert à
    /// prouver le REFUS nommé : ce que le conteneur dit, pas ce qu'il contient.
    pub(crate) fn mkv_a_codec(codec_id: &str, channels: u16) -> Vec<u8> {
        let trames: Vec<Trame> = (0..4)
            .map(|i| Trame {
                ts_ms: i * 32,
                octets: vec![0x0B, 0x77, 0xAA, 0x55, 0x00, 0x00, 0x00, 0x00],
            })
            .collect();
        matroska(
            "matroska",
            &Piste {
                codec_id,
                codec_private: None,
                sample_rate: 48_000,
                channels,
                bit_depth: None,
                duree_trame_ns: Some(32_000_000),
            },
            128.0,
            &trames,
            1,
            false,
            &[("TITLE", "Concert factice"), ("ARTIST", "Témoin")],
            &[("TITLE", "Ouverture"), ("PART_NUMBER", "1")],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::muxer_de_test::mka_depuis_flac;
    use super::*;

    fn fixture_flac() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/flac/ref_16_44100_stereo.flac")
    }

    /// La sonde admet un FLAC dans Matroska, et lit ce que le conteneur dit :
    /// cadence, canaux, profondeur, durée, balises d'album ET de piste.
    #[test]
    fn un_mka_flac_est_decodable_et_ses_balises_se_lisent() {
        let dossier = crate::test_scratch::scratch_dir("3633-sonde-mka");
        let chemin = dossier.path().join("concert.mka");
        std::fs::write(
            &chemin,
            mka_depuis_flac(
                &fixture_flac(),
                2,
                true,
                &[
                    ("TITLE", "Live au Rocher"),
                    ("ARTIST", "Les Témoins"),
                    ("DATE_RELEASED", "2026-09-16"),
                    ("GENRE", "Jazz"),
                ],
                &[
                    ("TITLE", "Ouverture"),
                    ("ARTIST", "Le Soliste"),
                    ("PART_NUMBER", "3"),
                ],
            ),
        )
        .unwrap();

        let sonde = sonder(&chemin).expect("la sonde doit lire un Matroska valide");
        assert_eq!(
            sonde.piste,
            PisteAudio::Decodable {
                codec: "flac".into(),
                opus: false
            },
            "FLAC dans Matroska : symphonia le démuxe ET le décode (#3633)"
        );
        assert!(sonde.est_decodable());
        assert!(!sonde.est_opus());
        assert_eq!(sonde.sample_rate, Some(44_100));
        assert_eq!(sonde.channels, Some(2));
        assert_eq!(sonde.bit_depth, Some(16));
        // 17 640 trames à 44,1 kHz = 400 ms (la fixture dure 0,4 s).
        assert_eq!(sonde.duration_ms, Some(400));
        assert_eq!(sonde.balises.title.as_deref(), Some("Ouverture"));
        assert_eq!(sonde.balises.album.as_deref(), Some("Live au Rocher"));
        // `ARTIST` sous la cible 50 (album) est l'ARTISTE D'ALBUM pour
        // symphonia (`ALBUM@ARTIST`) ; sous la cible 30, l'artiste de piste.
        assert_eq!(sonde.balises.artist.as_deref(), Some("Le Soliste"));
        assert_eq!(sonde.balises.album_artist.as_deref(), Some("Les Témoins"));
        assert_eq!(sonde.balises.track_number, Some(3));
        assert_eq!(sonde.balises.release_date.as_deref(), Some("2026-09-16"));
        assert_eq!(sonde.balises.genre.as_deref(), Some("Jazz"));
    }

    /// Le REFUS est nommé : un MKV dont la piste est annoncée AC-3 (puis
    /// E-AC-3, TrueHD, DTS) rend le codec, pas un identifiant hexadécimal.
    /// Le point 3 de l'arbitrage — les décoder — reste hors feuille de route.
    #[test]
    fn un_mkv_ac3_est_nomme_non_decodable() {
        let dossier = crate::test_scratch::scratch_dir("3633-sonde-ac3");
        for (codec_id, attendu) in [
            ("A_AC3", "ac3"),
            ("A_EAC3", "eac3"),
            ("A_TRUEHD", "truehd"),
            ("A_DTS", "dts"),
        ] {
            let chemin = dossier.path().join(format!("{attendu}.mkv"));
            std::fs::write(&chemin, super::muxer_de_test::mkv_a_codec(codec_id, 6)).unwrap();
            let sonde = sonder(&chemin).expect("la sonde doit lire un Matroska valide");
            assert_eq!(
                sonde.piste,
                PisteAudio::NonDecodable {
                    codec: attendu.into()
                },
                "{codec_id} : aucun décodeur livré, le refus doit NOMMER le codec (#3633)"
            );
            assert!(!sonde.est_decodable());
            // Le conteneur dit quand même ce qu'il sait.
            assert_eq!(sonde.channels, Some(6));
            assert_eq!(sonde.sample_rate, Some(48_000));
            assert_eq!(sonde.balises.title.as_deref(), Some("Ouverture"));
        }
    }

    /// Un `CodecID` que le démuxeur ne sait pas nommer, ou un MKV sans piste
    /// audio, n'est ni admis ni « non décodable » : il est SANS piste.
    #[test]
    fn un_codec_inconnu_du_demuxeur_est_sans_piste() {
        let dossier = crate::test_scratch::scratch_dir("3633-sonde-inconnu");
        let chemin = dossier.path().join("acm.mkv");
        std::fs::write(&chemin, super::muxer_de_test::mkv_a_codec("A_MS/ACM", 2)).unwrap();
        let sonde = sonder(&chemin).unwrap();
        assert_eq!(sonde.piste, PisteAudio::Aucune);
    }

    /// Ce qui n'est pas un Matroska est ILLISIBLE, pas « non pris en charge ».
    #[test]
    fn un_fichier_qui_nest_pas_un_matroska_est_une_erreur() {
        let dossier = crate::test_scratch::scratch_dir("3633-sonde-faux");
        let chemin = dossier.path().join("faux.mkv");
        std::fs::write(&chemin, b"ceci n'est pas un conteneur").unwrap();
        let err = sonder(&chemin).expect_err("un faux Matroska ne se sonde pas");
        assert!(
            err.contains("mkv"),
            "le motif doit dire d'où il vient : {err}"
        );
        assert!(!piste_audio_est_opus(chemin.to_str().unwrap()));
    }

    // -----------------------------------------------------------------
    // La LECTURE : `decode_to_pcm` et le chemin streaming lisent la piste
    // FLAC d'un Matroska, seek compris.
    // -----------------------------------------------------------------

    fn ecrire_mka(dossier: &Path, nom: &str, clusters: usize, avec_cues: bool) -> String {
        let chemin = dossier.join(nom);
        std::fs::write(
            &chemin,
            mka_depuis_flac(&fixture_flac(), clusters, avec_cues, &[], &[]),
        )
        .unwrap();
        chemin.to_str().unwrap().to_owned()
    }

    /// Le PCM d'un FLAC dans Matroska est CELUI du FLAC nu — même fixture,
    /// mêmes trames, seul le conteneur change. Le `.flac` est lui-même gardé
    /// bit pour bit contre `flac -d` (`tests/flac_empreintes_reference.rs`) :
    /// l'égalité ici transporte cette preuve au Matroska.
    ///
    /// Puis le SEEK : décodé depuis 0,4 s, le Matroska rend un SUFFIXE du
    /// décodage complet, plus court d'au moins 0,4 s — avec `Cues` (le
    /// démuxeur saute au cluster) comme sans (il avance dans les blocs).
    #[test]
    fn un_mka_flac_se_decode_bit_pour_bit_comme_son_flac_et_se_seeke() {
        use crate::audio::decode::decode_to_pcm;
        let dossier = crate::test_scratch::scratch_dir("3633-decode-mka");
        let flac = fixture_flac();
        let reference = decode_to_pcm(flac.to_str().unwrap(), None, None, 0.0, 0.0)
            .expect("le FLAC de référence se décode");
        // 17 640 trames stéréo = 35 280 échantillons entrelacés (0,4 s).
        assert_eq!(reference.samples_i32.len(), 35_280);

        for (nom, clusters, avec_cues) in [
            ("avec_cues.mka", 2, true),
            ("sans_cues.mka", 3, false),
            ("un_cluster.mkv", 1, false),
        ] {
            let chemin = ecrire_mka(dossier.path(), nom, clusters, avec_cues);
            let mka = decode_to_pcm(&chemin, None, None, 0.0, 0.0).unwrap_or_else(|e| {
                panic!("{nom} : un FLAC dans Matroska doit se lire (#3633) : {e}")
            });
            assert_eq!(mka.sample_rate, reference.sample_rate, "{nom} : cadence");
            assert_eq!(mka.channels, reference.channels, "{nom} : canaux");
            assert_eq!(mka.bit_depth, reference.bit_depth, "{nom} : profondeur");
            assert_eq!(
                mka.samples_i32, reference.samples_i32,
                "{nom} : le PCM sorti du Matroska doit être CELUI du FLAC nu"
            );

            let seeke = decode_to_pcm(&chemin, None, None, 0.2, 0.0)
                .unwrap_or_else(|e| panic!("{nom} : seek à 0,2 s refusé : {e}"));
            assert!(
                !seeke.samples_i32.is_empty(),
                "{nom} : un seek qui rend VIDE est un seek refusé par le démuxeur"
            );
            assert!(
                reference.samples_i32.ends_with(&seeke.samples_i32),
                "{nom} : décodé depuis 0,2 s, le Matroska doit rendre un suffixe \
                 du décodage complet"
            );
            let sautees = (reference.samples_i32.len() - seeke.samples_i32.len()) / 2;
            // Au moins 0,2 s (8 820 trames) sautées, à un bloc FLAC près
            // (4 096 trames = 93 ms) : le seek se pose sur une frontière de
            // trame, jamais après la cible.
            assert!(
                (8_820 - 4_096..17_640).contains(&sautees),
                "{nom} : {sautees} trames sautées pour un seek à 0,2 s"
            );
        }
    }

    /// Le refus à la LECTURE nomme le codec, comme au scan : le même
    /// `decoder_rejection` garde les deux frontières.
    #[test]
    fn un_mkv_ac3_est_refuse_a_la_lecture_en_nommant_le_codec() {
        let dossier = crate::test_scratch::scratch_dir("3633-decode-ac3");
        let chemin = dossier.path().join("concert.mkv");
        std::fs::write(&chemin, super::muxer_de_test::mkv_a_codec("A_AC3", 6)).unwrap();
        let err = match crate::audio::decode::decode_to_pcm(
            chemin.to_str().unwrap(),
            None,
            None,
            0.0,
            0.0,
        ) {
            Err(e) => e,
            Ok(_) => panic!("un MKV en AC-3 ne doit pas se décoder : aucun décodeur livré"),
        };
        assert!(
            err.contains("ac3"),
            "le refus de lecture doit NOMMER le codec (#3633) : {err}"
        );
    }

    /// Le chemin STREAMING — celui de la lecture réelle — sert les mêmes
    /// octets pour le Matroska que pour le FLAC nu, depuis le début comme
    /// depuis un seek.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn le_chemin_streaming_sert_la_piste_flac_dun_mka_comme_le_flac_nu() {
        use crate::audio::decode::decode_to_pcm_streaming_tranche;

        async fn servir(chemin: String, seek_s: f64) -> Vec<u8> {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            let pret = std::sync::Arc::new(tokio::sync::Notify::new());
            let (niveaux, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
            let tache = tokio::task::spawn_blocking(move || {
                decode_to_pcm_streaming_tranche(
                    &chemin,
                    Some(44_100),
                    Some(2),
                    Some(16),
                    tx,
                    32_768,
                    pret,
                    niveaux,
                    seek_s,
                    None,
                )
            });
            let mut octets = Vec::new();
            while let Some(bloc) = rx.recv().await {
                octets.extend(bloc);
            }
            tache.await.unwrap().expect("le flux doit se décoder");
            octets
        }

        let dossier = crate::test_scratch::scratch_dir("3633-stream-mka");
        let flac = fixture_flac().to_str().unwrap().to_owned();
        let mka = ecrire_mka(dossier.path(), "concert.mka", 2, true);

        let flac_entier = servir(flac.clone(), 0.0).await;
        let mka_entier = servir(mka.clone(), 0.0).await;
        // 44 octets d'en-tête WAV + 17 640 trames × 2 canaux × 2 octets.
        assert_eq!(
            flac_entier.len(),
            44 + 35_280 * 2,
            "le FLAC nu sert tout : repère cassé, le témoin ne mesurerait rien"
        );
        assert_eq!(
            mka_entier, flac_entier,
            "le chemin streaming doit servir, pour le Matroska, les octets du FLAC nu"
        );

        // Le SEEK sur le chemin streaming. Les deux démuxeurs ne se posent pas
        // sur la même frontière — le FLAC nu a sa table de recherche et ses
        // propres règles, le Matroska avance de bloc en bloc jusqu'à celui
        // qui contient 0,2 s — donc on ne compare pas octet à octet les deux
        // seeks. Ce qui est garanti, et mesuré : le flux seeké du Matroska
        // est un SUFFIXE de son flux entier, et il a sauté au moins 0,2 s à
        // un bloc FLAC près (4 096 trames).
        let flac_seeke = servir(flac, 0.2).await;
        let mka_seeke = servir(mka, 0.2).await;
        assert!(
            mka_seeke.len() < mka_entier.len() && flac_seeke.len() < flac_entier.len(),
            "le seek doit sauter de la matière"
        );
        let (en_tete, pcm_seeke) = mka_seeke.split_at(44);
        assert_eq!(en_tete, &mka_entier[..44], "même en-tête WAV");
        assert!(
            mka_entier[44..].ends_with(pcm_seeke),
            "depuis 0,2 s, le Matroska doit servir un suffixe de son flux entier"
        );
        let trames_sautees = (mka_entier.len() - mka_seeke.len()) / 4;
        assert!(
            (8_820 - 4_096..17_640).contains(&trames_sautees),
            "{trames_sautees} trames sautées pour un seek à 0,2 s (FLAC nu : {})",
            (flac_entier.len() - flac_seeke.len()) / 4
        );
    }

    #[test]
    fn les_extensions_matroska_sont_reconnues_en_toute_casse() {
        for nom in ["a.mkv", "b.MKA", "c.webm", "d.Weba"] {
            assert!(est_chemin_matroska(Path::new(nom)), "{nom}");
        }
        for nom in ["a.flac", "b.mp4", "c", "d.mk"] {
            assert!(!est_chemin_matroska(Path::new(nom)), "{nom}");
        }
    }
}
