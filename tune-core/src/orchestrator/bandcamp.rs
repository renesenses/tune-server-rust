/// Les encodages que Bandcamp nomme dans le CHEMIN d'une URL de flux.
///
/// Liste fermée et non heuristique : un segment de chemin Bandcamp est le plus
/// souvent un hachage hexadécimal, et prendre n'importe quel segment pour un
/// encodage inventerait une qualité.
pub(super) const BC_KNOWN_ENCODINGS: &[&str] = &[
    "mp3-128",
    "mp3-320",
    "mp3-v0",
    "flac",
    "alac",
    "aac-hi",
    "aiff-lossless",
    "vorbis",
    "wav",
];

/// L'encodage nommé par une URL Bandcamp, en minuscules.
///
/// Bandcamp écrit le nom de l'encodage dans l'URL elle-même, sous deux formes :
///
/// * segment de chemin — `https://t4.bcbits.com/stream/<hash>/mp3-128/<id>?…` ;
/// * paramètre de requête — `https://bandcamp.com/stream_redirect?enc=mp3-128&…`.
///
/// Un fichier d'album **acheté** emprunte la seconde forme avec une autre
/// valeur (`enc=flac`, `enc=mp3-320`, `enc=alac`…). C'est toute la raison
/// d'être de cette fonction : la règle écrite dans le plugin — « un flux à
/// 128 kbit/s doit être annoncé comme tel partout où il apparaît »,
/// `plugins/tune-bandcamp/src/lib.rs` — porte sur la QUALITÉ du flux, jamais
/// sur le nom du service. Décider depuis `source == "bandcamp"` collerait
/// « 128 kbit/s » sur un FLAC acheté (#2074).
///
/// Rend `None` quand l'URL ne nomme aucun encodage connu : on n'annonce alors
/// rien plutôt que de deviner un chiffre.
pub fn bandcamp_encoding(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    let (path, query) = lower.split_once('?').unwrap_or((lower.as_str(), ""));
    if let Some(enc) = query
        .split('&')
        .find_map(|p| p.strip_prefix("enc="))
        .filter(|v| !v.is_empty())
    {
        return Some(enc.to_string());
    }
    path.split('/')
        .find(|seg| BC_KNOWN_ENCODINGS.contains(seg))
        .map(|seg| seg.to_string())
}

/// Ce qu'un encodage Bandcamp vaut réellement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandcampQuality {
    /// Codec, sous la forme d'extension que lit le chemin du signal.
    pub codec: &'static str,
    /// Type MIME à annoncer à la zone.
    pub mime_type: &'static str,
    /// Débit CONSTANT en kbit/s. `None` pour un débit variable ou un encodage
    /// sans perte : un débit inventé serait pire que pas de débit du tout.
    pub bitrate_kbps: Option<u32>,
}

/// Traduire un encodage Bandcamp en codec, MIME et débit annonçables.
pub fn bandcamp_quality(enc: &str) -> Option<BandcampQuality> {
    let q = |codec, mime_type, bitrate_kbps| {
        Some(BandcampQuality {
            codec,
            mime_type,
            bitrate_kbps,
        })
    };
    match enc {
        "flac" => q("flac", "audio/flac", None),
        "alac" => q("alac", "audio/mp4", None),
        "aiff-lossless" => q("aiff", "audio/aiff", None),
        "wav" => q("wav", "audio/wav", None),
        "vorbis" => q("ogg", "audio/ogg", None),
        "aac-hi" => q("aac", "audio/mp4", None),
        // Débit variable : le nom ne porte aucun chiffre, et en inventer un
        // serait exactement le mensonge que ce correctif supprime.
        "mp3-v0" => q("mp3", "audio/mpeg", None),
        // `mp3-128` (écoute libre) et `mp3-320` (achat) partagent la même
        // forme : on lit le nombre plutôt que d'énumérer, sinon un futur
        // `mp3-256` retomberait silencieusement sans débit.
        other => other
            .strip_prefix("mp3-")
            .and_then(|n| n.parse::<u32>().ok())
            .and_then(|kbps| q("mp3", "audio/mpeg", Some(kbps))),
    }
}

/// Le MIME de DERNIER RECOURS, quand rien — ni l'URL, ni la ligne, ni
/// l'appelant — ne nomme le format.
///
/// Il n'affirme rien : c'est le choix le moins coûteux pour une URL de podcast
/// ou de radio, qui est très majoritairement du MP3. Sur un serveur média, ce
/// même défaut MENT (voir [`super::mime_upnp`]), d'où la séparation ci-dessous
/// entre « l'extension nomme un format » et « on retombe ».
pub(super) const MIME_PAR_DEFAUT: &str = "audio/mpeg";

/// Le MIME que l'EXTENSION de l'URL nomme — `None` quand elle n'en nomme
/// aucun.
///
/// Cette fonction ne retombe sur rien : c'est précisément la distinction qui
/// manquait. [`guess_mime_from_url`] confondait « l'URL dit MP3 » et « l'URL
/// ne dit rien », les deux rendant `"audio/mpeg"` ; un appelant qui sait autre
/// chose sur la piste ne pouvait donc pas savoir s'il avait le droit de parler.
pub(super) fn mime_depuis_l_extension(url: &str) -> Option<&'static str> {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") {
        Some("audio/mpeg")
    } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") {
        Some("audio/mp4")
    } else if path.ends_with(".ogg") || path.ends_with(".opus") {
        Some("audio/ogg")
    } else if path.ends_with(".flac") || path.ends_with(".flc") {
        // ".flc" is the extension Lyrion/LMS uses for FLAC in its stream URLs
        // (…/music/<id>/download.flc); it fell through to the "audio/mpeg"
        // default, so the DLNA renderer got FLAC bytes labelled as MP3.
        Some("audio/flac")
    } else if path.ends_with(".wav") {
        Some("audio/wav")
    } else if path.ends_with(".aif") || path.ends_with(".aiff") {
        Some("audio/aiff")
    } else {
        None
    }
}

pub(super) fn guess_mime_from_url(url: &str) -> &'static str {
    mime_depuis_l_extension(url).unwrap_or(MIME_PAR_DEFAUT)
}
