use serde::{Deserialize, Serialize};
use tracing::debug;

/// Metadata extracted from a radio stream (ICY or external API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IcyMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub station: Option<String>,
    /// La pochette du titre en cours, quand la station la donne elle-même.
    ///
    /// Aucune recherche, aucune supposition : c'est l'URL que l'API de la
    /// station publie avec son now-playing. Absente, l'écran garde le logo de
    /// la radio — mieux vaut un logo juste qu'une pochette devinée.
    #[serde(default)]
    pub cover_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Fetch metadata for the given radio station.
///
/// The function first checks whether the station URL matches a known metadata
/// API (Radio France / FIP, Radio Paradise) and uses those richer endpoints.
/// As a fallback it attempts to read raw ICY metadata from the audio stream.
pub async fn fetch_radio_metadata(station_name: &str, stream_url: &str) -> Option<IcyMetadata> {
    // Radio France family (FIP, France Inter, France Musique, ...)
    if stream_url.contains("fipradio")
        || stream_url.contains("radiofrance")
        || stream_url.contains("fip-")
        || station_name.to_lowercase().contains("fip")
        || station_name.to_lowercase().contains("france musique")
        || station_name.to_lowercase().contains("france inter")
    {
        // No channel means the live-meta API has nothing for this station.
        // Fall through to the raw ICY reader rather than querying a channel
        // that belongs to a different station.
        if let Some(channel) = radiofrance_channel_id(station_name, stream_url) {
            return fetch_radiofrance_metadata(station_name, channel).await;
        }
    }

    // Radio Paradise
    if stream_url.contains("radioparadise")
        || station_name.to_lowercase().contains("radio paradise")
    {
        let chan = radioparadise_channel(stream_url);
        return fetch_radio_paradise_metadata(station_name, chan).await;
    }

    // Fallback: raw ICY metadata
    fetch_icy_metadata(stream_url).await
}

// ---------------------------------------------------------------------------
// Radio France
// ---------------------------------------------------------------------------

/// Extract the FIP sub-station qualifier from a station name or stream URL:
/// `"pop"` for both `FIP Pop` and `.../fippop-hifi.aac`, and an empty string
/// for the main station.
///
/// Everything after the last `fip` is kept, minus the stream-flavour noise
/// (`-hifi.aac`, `midfi`, the legacy `fipradio.fr` host), so the caller can
/// tell "this is FIP" from "this is some FIP webradio we do not know".
fn fip_qualifier(hay: &str) -> String {
    let Some(index) = hay.rfind("fip") else {
        return String::new();
    };

    hay[index + 3..]
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|token| {
            !matches!(
                *token,
                "hifi"
                    | "midfi"
                    | "lofi"
                    | "aac"
                    | "mp3"
                    | "radio"
                    | "fr"
                    | "com"
                    | "http"
                    | "https"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map a station name / stream URL to the Radio France *station id* used by
/// their live-meta API, or `None` when that API has nothing for it.
///
/// The FIP *webradios* (Rock, Jazz, Groove, …) each have their OWN livemeta
/// channel — matching only `fip` mapped them all to 7, so FIP, FIP Rock and
/// FIP Cultes all showed FIP's now-playing (forum: identical titles). The
/// substation ids below were verified live against api.radiofrance.fr/livemeta.
/// Match on the station name too (user-added stations often carry the substation
/// in the name, e.g. "FIP Rock", not the URL).
///
/// Returning `None` matters as much as returning an id. FIP Pop, FIP Hip-Hop,
/// FIP Sacré français and FIP Cultes stream fine but have **no** livemeta
/// channel — the whole `1..=260` range was swept on 2026-08-10 and nothing
/// answers for them — and their AAC streams carry no in-band ICY metadata
/// either. Falling back to 7 made them display *main FIP's* now-playing:
/// a confident, wrong answer, which is worse than an empty one (forum: Jean
/// Valjean, « Mauvaises Métadonnées sur Fip Cultes, FIP Pop, FIP Sacré
/// français, FIP Hip-Hop »). The same reasoning applies to any Radio France
/// station we do not recognise at all: guessing FIP would be inventing.
fn radiofrance_channel_id(station_name: &str, stream_url: &str) -> Option<u32> {
    let hay = format!(
        "{} {}",
        station_name.to_lowercase(),
        stream_url.to_lowercase()
    );
    if hay.contains("franceinter") {
        Some(1)
    } else if hay.contains("francemusique") || hay.contains("france-musique") {
        Some(4)
    } else if hay.contains("mouv") {
        Some(6)
    } else if hay.contains("franceculture") || hay.contains("france-culture") {
        Some(2)
    } else if hay.contains("franceinfo") {
        Some(3)
    } else if hay.contains("fip") {
        // FIP webradios: pick the specific substation, main FIP when there is
        // no qualifier at all, and nothing when the qualifier is unknown.
        let qualifier = fip_qualifier(&hay);
        if qualifier.is_empty() {
            Some(7)
        } else if qualifier.contains("rock") {
            Some(64)
        } else if qualifier.contains("jazz") {
            Some(65)
        } else if qualifier.contains("groove") {
            Some(66)
        } else if qualifier.contains("monde") || qualifier.contains("world") {
            Some(69)
        } else if qualifier.contains("nouveau") {
            Some(70)
        } else if qualifier.contains("reggae") {
            Some(71)
        } else if qualifier.contains("electro") {
            Some(74)
        } else if qualifier.contains("metal") {
            Some(77)
        } else {
            None
        }
    } else {
        None
    }
}

/// Ne retenir qu'une **adresse** de pochette.
///
/// Le champ `visual` de Radio France est polymorphe, et un seul sondage suffit
/// à s'y tromper : sur un pas de type *chanson* il porte une vraie URL
/// (`https://www.radiofrance.fr/s3/…/400x400_….jpg`), sur un pas d'**émission**
/// il ne porte qu'un UUID nu (`1059fabb-9a51-…`). Servir cet UUID à l'écran
/// donnerait une image cassée là où le logo de la station faisait l'affaire.
fn url_de_pochette(v: Option<&str>) -> Option<String> {
    let v = v?.trim();
    if v.starts_with("http://") || v.starts_with("https://") {
        Some(v.to_string())
    } else {
        None
    }
}

async fn fetch_radiofrance_metadata(station_name: &str, channel: u32) -> Option<IcyMetadata> {
    let url = format!("https://api.radiofrance.fr/livemeta/pull/{channel}");
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "radiofrance_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;

    // New API format: levels[0].items[position] → step UUID → steps[uuid]
    let levels = body.get("levels")?.as_array()?;
    let level = levels.first()?;
    let position = level.get("position")?.as_u64()? as usize;
    let items = level.get("items")?.as_array()?;
    let current_id = items.get(position)?.as_str()?;
    let steps = body.get("steps")?.as_object()?;
    let now = steps.get(current_id)?;

    let title = now
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if title.is_empty() {
        return None;
    }

    let artist = now
        .get("authors")
        .and_then(|v| v.as_str())
        .or_else(|| {
            now.get("song")
                .and_then(|s| s.get("interpreters"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string());

    // `visual` porte la pochette du titre sur un pas « chanson ». On la prend
    // telle qu'elle vient — c'est la station qui la publie, il n'y a rien à
    // deviner ni à chercher ailleurs.
    let cover_url = url_de_pochette(now.get("visual").and_then(|v| v.as_str()));

    Some(IcyMetadata {
        title,
        artist,
        station: Some(station_name.to_string()),
        cover_url,
    })
}

// ---------------------------------------------------------------------------
// Radio Paradise
// ---------------------------------------------------------------------------

fn radioparadise_channel(stream_url: &str) -> u32 {
    if stream_url.contains("chan=1") || stream_url.contains("mellow") {
        1
    } else if stream_url.contains("chan=2") || stream_url.contains("rock") {
        2
    } else if stream_url.contains("chan=3") || stream_url.contains("world") {
        3
    } else {
        0 // main mix
    }
}

async fn fetch_radio_paradise_metadata(station_name: &str, chan: u32) -> Option<IcyMetadata> {
    let url = format!("https://api.radioparadise.com/api/now_playing?chan={chan}");
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "radioparadise_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let artist = body
        .get("artist")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if title.is_empty() {
        return None;
    }

    // Radio Paradise publie trois tailles ; `cover` est la grande. Les deux
    // autres restent disponibles si l'affichage venait à en vouloir de plus
    // légères.
    let cover_url = url_de_pochette(body.get("cover").and_then(|v| v.as_str()))
        .or_else(|| url_de_pochette(body.get("cover_med").and_then(|v| v.as_str())));

    Some(IcyMetadata {
        title,
        artist,
        station: Some(station_name.to_string()),
        cover_url,
    })
}

// ---------------------------------------------------------------------------
// Raw ICY metadata (fallback)
// ---------------------------------------------------------------------------

async fn fetch_icy_metadata(stream_url: &str) -> Option<IcyMetadata> {
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get(stream_url)
        .header("Icy-MetaData", "1")
        .send()
        .await
        .ok()?;

    let icy_metaint: usize = resp
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())?;

    let station = resp
        .headers()
        .get("icy-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // We need to read `icy_metaint` bytes of audio, then one length byte,
    // then `length * 16` bytes of metadata text.
    let bytes = resp.bytes().await.ok()?;

    if bytes.len() <= icy_metaint {
        return None;
    }

    let meta_length_byte = bytes[icy_metaint] as usize;
    if meta_length_byte == 0 {
        return None;
    }

    let meta_len = meta_length_byte * 16;
    let meta_start = icy_metaint + 1;
    let meta_end = meta_start + meta_len;
    if bytes.len() < meta_end {
        return None;
    }

    let raw = String::from_utf8_lossy(&bytes[meta_start..meta_end]);
    parse_icy_string(&raw, station)
}

/// Lire un champ `Nom='valeur'` d'un bloc ICY.
///
/// Le bloc est une suite de `Cle='valeur';`. On cherche la clé suivie de `='`
/// et l'on s'arrête au premier `';` — c'est déjà ce que faisait l'extraction du
/// titre, extraite ici pour servir les DEUX champs plutôt qu'un seul.
fn champ_icy<'a>(bloc: &'a str, cle: &str) -> Option<&'a str> {
    let ouvrant = format!("{cle}='");
    let debut = bloc.find(&ouvrant)?;
    let apres = &bloc[debut + ouvrant.len()..];
    let fin = apres.find("';")?;
    Some(&apres[..fin])
}

/// La pochette qu'une station publie dans `StreamUrl`, quand c'en est une.
///
/// `StreamUrl` est le champ « adresse liée au flux » du protocole ICY, et il
/// est **polymorphe** — exactement comme `visual` chez Radio France : certaines
/// stations y mettent l'accueil de leur site, d'autres l'illustration du titre
/// en cours. Servir une page HTML à une balise `<img>` afficherait une image
/// cassée là où le logo de la station faisait l'affaire. On ne retient donc
/// qu'une adresse absolue dont le **chemin** se termine par une extension
/// d'image ; tout le reste retombe sur le repli déjà en place.
///
/// Ce n'est pas une supposition sur le protocole, c'est une mesure — et Tune
/// écrit lui-même la pochette dans ce champ depuis #2161
/// (`tune-stream-http`, `StreamUrl='…'`) pour l'écran des lecteurs réseau.
/// Il l'écrivait sans jamais la relire.
fn pochette_de_stream_url(v: Option<&str>) -> Option<String> {
    let brut = v?.trim();
    let url = url_de_pochette(Some(brut))?;

    // Le chemin seul décide : `?size=512` ou `#x` ne changent pas la nature du
    // fichier, et les laisser dans la comparaison ferait rater les stations qui
    // versionnent leur illustration par une chaîne de requête.
    let chemin = url
        .split(['?', '#'])
        .next()
        .unwrap_or(&url)
        .to_ascii_lowercase();

    const EXTENSIONS: [&str; 7] = [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif", ".bmp"];
    if EXTENSIONS.iter().any(|ext| chemin.ends_with(ext)) {
        Some(url)
    } else {
        None
    }
}

/// Parse the ICY metadata string.
///
/// Typical format: `StreamTitle='Artist - Title';StreamUrl='';`
fn parse_icy_string(raw: &str, station: Option<String>) -> Option<IcyMetadata> {
    let trimmed = raw.trim_end_matches('\0');

    // Extract StreamTitle value
    let stream_title = champ_icy(trimmed, "StreamTitle")?;

    if stream_title.is_empty() {
        return None;
    }

    // Try to split "Artist - Title"
    let (artist, title) = if let Some(sep) = stream_title.find(" - ") {
        (
            Some(stream_title[..sep].to_string()),
            stream_title[sep + 3..].to_string(),
        )
    } else {
        (None, stream_title.to_string())
    };

    Some(IcyMetadata {
        title,
        artist,
        station,
        // La troisième sœur du `match` d'aiguillage : Radio France et Radio
        // Paradise rendaient une pochette, ce repli — celui qui sert TOUTES les
        // autres stations — rendait `None` en affirmant qu'ICY n'en transporte
        // pas. Mesuré le 30/08/2026, premier bloc du flux :
        //
        //   stream.radioparadise.com/mp3-192  → StreamUrl='http://img.radioparadise.com/covers/l/10580.jpg'
        //   ice1.somafm.com/groovesalad-128-mp3 → StreamUrl='https://somafm.com/logos/512/groovesalad512.jpg'
        //   ice2.somafm.com/dronezone-128-mp3   → StreamUrl='https://somafm.com/logos/512/dronezone512.png'
        //
        // On ne CHERCHE toujours rien : c'est la station qui publie l'adresse,
        // dans le champ où Tune lui-même écrit la sienne (#2161).
        cover_url: pochette_de_stream_url(champ_icy(trimmed, "StreamUrl")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_icy_artist_title() {
        let raw = "StreamTitle='Miles Davis - So What';StreamUrl='';";
        let meta = parse_icy_string(raw, Some("Jazz FM".into())).unwrap();
        assert_eq!(meta.title, "So What");
        assert_eq!(meta.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(meta.station.as_deref(), Some("Jazz FM"));
    }

    #[test]
    fn parse_icy_title_only() {
        let raw = "StreamTitle='Unknown Show';StreamUrl='';";
        let meta = parse_icy_string(raw, None).unwrap();
        assert_eq!(meta.title, "Unknown Show");
        assert!(meta.artist.is_none());
    }

    #[test]
    fn parse_icy_empty() {
        let raw = "StreamTitle='';StreamUrl='';";
        assert!(parse_icy_string(raw, None).is_none());
    }

    /// Regression #2065 — la pochette d'un flux ICY brut.
    ///
    /// `fetch_radio_metadata` aiguille vers trois analyseurs. Radio France et
    /// Radio Paradise ont reçu `cover_url` en v0.9.97 ; le repli ICY — celui
    /// qui sert **toutes les autres stations** — est resté à `None`, sur
    /// l'affirmation qu'ICY ne transporte pas d'image. Mesuré le 30/08/2026,
    /// premier bloc de métadonnées du flux : Radio Paradise y publie la
    /// pochette du morceau EN COURS, à l'adresse même que rend son API.
    #[test]
    fn le_bloc_icy_livre_la_pochette_quand_la_station_la_publie() {
        let raw = "StreamTitle='Amadou & Mariam - Ce N'est Pas Bon';\
                   StreamUrl='http://img.radioparadise.com/covers/l/10580.jpg';";
        let meta = parse_icy_string(raw, Some("Radio Paradise".into())).unwrap();
        assert_eq!(
            meta.cover_url.as_deref(),
            Some("http://img.radioparadise.com/covers/l/10580.jpg"),
            "la pochette publiée par la station doit sortir de l'analyseur ICY"
        );
    }

    /// SomaFM (mesuré le 30/08/2026) : `.png`, et une URL en `https`. Deux
    /// stations, deux extensions — l'analyseur ne doit privilégier ni l'une ni
    /// l'autre. La chaîne de requête ne décide pas non plus : c'est le CHEMIN
    /// qui dit si c'est une image.
    #[test]
    fn la_pochette_icy_accepte_les_extensions_et_ignore_la_chaine_de_requete() {
        assert_eq!(
            pochette_de_stream_url(Some("https://somafm.com/logos/512/dronezone512.png")),
            Some("https://somafm.com/logos/512/dronezone512.png".to_string())
        );
        assert_eq!(
            pochette_de_stream_url(Some("https://cdn.exemple.fr/art/1234.JPEG?size=512")),
            Some("https://cdn.exemple.fr/art/1234.JPEG?size=512".to_string())
        );
        assert_eq!(
            pochette_de_stream_url(Some("  https://cdn.exemple.fr/a.webp  ")),
            Some("https://cdn.exemple.fr/a.webp".to_string())
        );
    }

    /// `StreamUrl` est polymorphe, comme `visual` chez Radio France : beaucoup
    /// de stations y mettent l'accueil de leur site. Le servir à une balise
    /// `<img>` afficherait une image cassée là où le logo de la station faisait
    /// l'affaire — et une image cassée est PIRE que l'absence, parce qu'elle
    /// remplace un repli qui, lui, fonctionnait.
    #[test]
    fn une_adresse_qui_n_est_pas_une_image_ne_devient_pas_une_pochette() {
        for adresse in [
            "https://www.walmradio.com",
            "http://somafm.com/groovesalad/",
            "https://exemple.fr/page.html",
            "1059fabb-9a51-4f2b-8f3d-2c7c1a0b9e44",
            "",
            "   ",
        ] {
            assert_eq!(
                pochette_de_stream_url(Some(adresse)),
                None,
                "« {adresse} » n'est pas une image et ne doit pas devenir une pochette"
            );
        }
        assert_eq!(pochette_de_stream_url(None), None);
    }

    /// Le cas majoritaire reste `StreamUrl=''` : rien ne doit changer pour ces
    /// stations-là, le repli sur le logo doit continuer de jouer.
    #[test]
    fn un_streamurl_vide_ou_absent_laisse_la_pochette_au_repli() {
        let vide = parse_icy_string("StreamTitle='Miles Davis - So What';StreamUrl='';", None)
            .expect("titre lisible");
        assert_eq!(vide.cover_url, None);

        let absent =
            parse_icy_string("StreamTitle='Miles Davis - So What';", None).expect("titre lisible");
        assert_eq!(absent.cover_url, None);
    }

    #[test]
    fn parse_icy_with_null_padding() {
        let mut raw = String::from("StreamTitle='FIP - Jazz';StreamUrl='';");
        raw.push_str("\0\0\0\0\0");
        let meta = parse_icy_string(&raw, None).unwrap();
        assert_eq!(meta.title, "Jazz");
        assert_eq!(meta.artist.as_deref(), Some("FIP"));
    }

    #[test]
    fn radiofrance_channel_detection() {
        assert_eq!(
            radiofrance_channel_id("FIP", "https://icecast.radiofrance.fr/fip-hifi.aac"),
            Some(7)
        );
        assert_eq!(
            radiofrance_channel_id(
                "Inter",
                "https://icecast.radiofrance.fr/franceinter-hifi.aac"
            ),
            Some(1)
        );
        assert_eq!(
            radiofrance_channel_id(
                "Musique",
                "https://icecast.radiofrance.fr/francemusique-hifi.aac"
            ),
            Some(4)
        );
        assert_eq!(
            radiofrance_channel_id("Mouv", "https://icecast.radiofrance.fr/mouv-hifi.aac"),
            Some(6)
        );
    }

    #[test]
    fn fip_webradios_map_to_distinct_channels() {
        // Regression: FIP substations all mapped to 7 → identical now-playing
        // for FIP / FIP Rock / FIP Cultes (forum). Each must resolve its own
        // livemeta channel — matched via the stream URL...
        let url = |s: &str| format!("https://icecast.radiofrance.fr/{s}-hifi.aac");
        assert_eq!(radiofrance_channel_id("FIP", &url("fip")), Some(7));
        assert_eq!(radiofrance_channel_id("", &url("fiprock")), Some(64));
        assert_eq!(radiofrance_channel_id("", &url("fipjazz")), Some(65));
        assert_eq!(radiofrance_channel_id("", &url("fipgroove")), Some(66));
        assert_eq!(radiofrance_channel_id("", &url("fipworld")), Some(69));
        assert_eq!(radiofrance_channel_id("", &url("fipnouveautes")), Some(70));
        assert_eq!(radiofrance_channel_id("", &url("fipreggae")), Some(71));
        assert_eq!(radiofrance_channel_id("", &url("fipelectro")), Some(74));
        assert_eq!(radiofrance_channel_id("", &url("fipmetal")), Some(77));
        // ...or via the station name alone (user-added radios).
        assert_eq!(radiofrance_channel_id("FIP Rock", ""), Some(64));
        assert_eq!(radiofrance_channel_id("FIP Groove", ""), Some(66));
        assert_eq!(radiofrance_channel_id("FIP", ""), Some(7));
    }

    #[test]
    fn fip_webradios_without_livemeta_return_no_channel() {
        // Regression (forum, Jean Valjean): FIP Pop, FIP Hip-Hop, FIP Sacré
        // français and FIP Cultes stream fine but Radio France exposes no
        // livemeta channel for them. Defaulting to 7 displayed *main FIP's*
        // now-playing on those stations — a confident wrong answer. They must
        // resolve to nothing so the caller falls back to the raw ICY reader.
        let url = |s: &str| format!("https://icecast.radiofrance.fr/{s}-hifi.aac");
        assert_eq!(radiofrance_channel_id("FIP Pop", &url("fippop")), None);
        assert_eq!(
            radiofrance_channel_id("FIP Hip-Hop", &url("fiphiphop")),
            None
        );
        assert_eq!(
            radiofrance_channel_id("FIP Sacré français", &url("fipsacrefrancais")),
            None
        );
        assert_eq!(
            radiofrance_channel_id("FIP Cultes", &url("fipcultes")),
            None
        );

        // A station name alone is enough — user-added radios often carry no
        // recognisable URL.
        assert_eq!(radiofrance_channel_id("FIP Pop", ""), None);

        // And a Radio France station we know nothing about is not FIP either.
        assert_eq!(
            radiofrance_channel_id("Radio Machin", "https://icecast.radiofrance.fr/machin.aac"),
            None
        );
    }

    #[test]
    fn fip_qualifier_ignores_stream_flavour() {
        // The main station must stay recognisable through every URL shape it
        // ships under, otherwise it would lose its metadata too.
        assert_eq!(
            fip_qualifier("fip https://icecast.radiofrance.fr/fip-hifi.aac"),
            ""
        );
        assert_eq!(
            fip_qualifier(" https://icecast.radiofrance.fr/fip-midfi.mp3"),
            ""
        );
        assert_eq!(
            fip_qualifier(" http://direct.fipradio.fr/live/fip-midfi.mp3"),
            ""
        );
        assert_eq!(fip_qualifier("fip "), "");

        // …while a substation keeps its qualifier, from the name or the URL.
        assert_eq!(fip_qualifier("fip pop "), "pop");
        assert_eq!(
            fip_qualifier(" https://icecast.radiofrance.fr/fipsacrefrancais-hifi.aac"),
            "sacrefrancais"
        );
        assert_eq!(
            fip_qualifier(" https://icecast.radiofrance.fr/fiprock-hifi.aac"),
            "rock"
        );
    }

    #[test]
    fn radioparadise_channel_detection() {
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/aac-320"),
            0
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/mellow-320"),
            1
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/rock-320"),
            2
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/world-320"),
            3
        );
    }

    // --- La pochette du titre, quand la station la donne ---

    #[test]
    fn seule_une_adresse_est_retenue_comme_pochette() {
        assert_eq!(
            url_de_pochette(Some("https://www.radiofrance.fr/s3/x/400x400_y.jpg")),
            Some("https://www.radiofrance.fr/s3/x/400x400_y.jpg".into())
        );
        assert_eq!(
            url_de_pochette(Some("http://img.radioparadise.com/covers/l/6940.jpg")),
            Some("http://img.radioparadise.com/covers/l/6940.jpg".into())
        );
    }

    /// Le piège qu'un seul sondage de l'API ne montre pas : le champ `visual`
    /// de Radio France porte une vraie URL sur un pas « chanson », et un UUID
    /// NU sur un pas d'émission. Servir l'UUID donnerait une image cassée là
    /// où le logo de la station faisait l'affaire.
    #[test]
    fn un_uuid_nu_nest_pas_une_pochette() {
        assert_eq!(
            url_de_pochette(Some("1059fabb-9a51-4d4d-8abe-ecde69d0a3e6")),
            None
        );
        assert_eq!(
            url_de_pochette(Some("405a846f-bc82-41b9-89b1-135b2430fe5c")),
            None
        );
    }

    #[test]
    fn labsence_et_le_vide_ne_donnent_rien() {
        assert_eq!(url_de_pochette(None), None);
        assert_eq!(url_de_pochette(Some("")), None);
        assert_eq!(url_de_pochette(Some("   ")), None);
        // Un chemin relatif n'est pas exploitable tel quel côté client.
        assert_eq!(url_de_pochette(Some("/s3/x/400x400_y.jpg")), None);
    }

    #[test]
    fn les_espaces_autour_de_l_adresse_sont_retires() {
        assert_eq!(
            url_de_pochette(Some("  https://x/y.jpg  ")),
            Some("https://x/y.jpg".into())
        );
    }

    /// Sans `StreamUrl`, aucune pochette : on n'en cherche pas par
    /// ressemblance, et le repli sur le logo de la station reste seul en jeu.
    ///
    /// L'intitulé d'origine disait « un flux ICY brut ne transporte aucune
    /// pochette ». C'est faux, et mesuré comme tel le 30/08/2026 (#2065) :
    /// Radio Paradise et SomaFM publient une image dans `StreamUrl`. Ce que
    /// cette épreuve garde, c'est le cas — majoritaire — où le champ est
    /// absent.
    #[test]
    fn licy_brut_ne_promet_aucune_pochette() {
        let m = parse_icy_string("StreamTitle='Kings Of Leon - Pistol of fire';", None)
            .expect("l'ICY se lit");
        assert_eq!(m.cover_url, None);
    }
}
