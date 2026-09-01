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
/// API (Radio France / FIP, Radio Paradise, BBC) and uses those richer
/// endpoints. As a fallback it attempts to read raw ICY metadata from the audio
/// stream.
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

    // BBC — troisième famille (#2486). Mesuré le 01/09/2026 :
    // `http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three` répond 200 SANS
    // en-tête `icy-metaint`, même en réclamant `Icy-MetaData: 1`. Le repli ICY
    // s'arrête donc sur son `?` et l'écran ne montre RIEN. Le service RMS de la
    // BBC, lui, publie le morceau en cours.
    if let Some(service_id) = bbc_service_id(station_name, stream_url) {
        return fetch_bbc_metadata(BBC_RMS_BASE, station_name, &service_id).await;
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
// BBC (#2486)
// ---------------------------------------------------------------------------

/// Racine du service *RMS* de la BBC — public, sans clef ni compte.
const BBC_RMS_BASE: &str = "https://rms.api.bbc.co.uk";

/// L'adresse d'illustration rendue par la BBC porte un gabarit `{recipe}` :
/// `https://ichef.bbci.co.uk/images/ic/{recipe}/p03mpgxn.jpg`. Servie telle
/// quelle elle répond **403** (mesuré le 01/09/2026) — une image cassée là où
/// le logo de la station faisait l'affaire. Substituée, elle répond 200
/// `image/jpeg`. C'est un gabarit à remplir, pas une URL à recopier.
const BBC_RECETTE_POCHETTE: &str = "320x320";

/// Longueur au-delà de laquelle un jeton `bbc_…` n'est plus un identifiant de
/// service plausible : on ne l'envoie pas au distant.
const BBC_SERVICE_ID_MAX: usize = 48;

/// L'identifiant de service BBC porté par l'URL de flux, ou déduit du nom.
///
/// **L'URL décide d'abord**, parce qu'elle dit ce qui est réellement diffusé :
/// les deux formes livrées par la BBC portent l'identifiant en clair —
/// `…bbcmedia.co.uk/bbc_radio_three` et le HLS
/// `…/live/ww/bbc_radio_three/bbc_radio_three.isml/bbc_radio_three-audio%3d320000…`.
/// On retient le premier jeton qui commence par `bbc_` et l'on s'arrête au
/// premier caractère qui n'est ni lettre, ni chiffre, ni `_` : le `-audio…` et
/// le `.isml` tombent d'eux-mêmes.
///
/// **Le nom ne sert qu'en second**, pour les stations ajoutées à la main qui ne
/// portent pas d'URL reconnaissable. La table est courte **et mesurée** : les
/// six identifiants ci-dessous ont été interrogés le 01/09/2026 et répondent
/// tous 200 (un identifiant inconnu, lui, rend 400 — vérifié). On n'en devine
/// aucun autre : rendre `None` renvoie au repli ICY, ce qui est exactement ce
/// qui se passait avant ce correctif.
fn bbc_service_id(station_name: &str, stream_url: &str) -> Option<String> {
    if let Some(depuis_url) = bbc_id_dans_url(&stream_url.to_lowercase()) {
        return Some(depuis_url);
    }

    let nom: String = station_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if !nom.split(' ').any(|mot| mot == "bbc") {
        return None;
    }

    let id = if nom.contains("6 music") || nom.contains("6music") {
        "bbc_6music"
    } else if nom.contains("world service") {
        "bbc_world_service"
    } else if nom.contains("radio 1") || nom.contains("radio one") {
        "bbc_radio_one"
    } else if nom.contains("radio 2") || nom.contains("radio two") {
        "bbc_radio_two"
    } else if nom.contains("radio 3") || nom.contains("radio three") {
        "bbc_radio_three"
    } else if nom.contains("radio 4") || nom.contains("radio four") {
        // L'identifiant du 4 est `…fourfm` : c'est le flux FM que diffuse la BBC.
        "bbc_radio_fourfm"
    } else {
        return None;
    };

    Some(id.to_string())
}

/// Le jeton `bbc_…` d'une URL déjà passée en minuscules.
fn bbc_id_dans_url(url: &str) -> Option<String> {
    let debut = url.find("bbc_")?;
    let jeton: String = url[debut..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();

    // « bbc_ » tout seul, ou un jeton à rallonge, ne sont pas des identifiants.
    if jeton.len() <= "bbc_".len() || jeton.len() > BBC_SERVICE_ID_MAX {
        return None;
    }
    Some(jeton)
}

/// L'illustration du morceau, gabarit rempli.
///
/// Si un autre gabarit que `{recipe}` apparaissait un jour, l'accolade
/// survivrait à la substitution : on rend alors `None` plutôt qu'une adresse
/// qui répondrait 403.
///
/// Le reste des règles est celui de [`pochette_de_stream_url`], le plus strict
/// des deux filtres du fichier : il exige que le **chemin** se termine par une
/// extension d'image. `url_de_pochette` seul laisserait passer une page — et
/// une image cassée est pire que l'absence, parce qu'elle remplace un repli
/// qui, lui, fonctionnait.
fn pochette_bbc(image_url: Option<&str>) -> Option<String> {
    let brut = image_url?.trim();
    let rempli = brut.replace("{recipe}", BBC_RECETTE_POCHETTE);
    if rempli.contains('{') || rempli.contains('}') {
        return None;
    }
    pochette_de_stream_url(Some(&rempli))
}

/// Lire le morceau **en cours** dans la réponse RMS.
///
/// Le piège que la lecture d'un seul champ ne montre pas : `data` porte les
/// quatre derniers segments, et la BBC en diffuse toujours quatre — même
/// quand plus rien ne joue. Le morceau en cours est celui, et seulement celui,
/// que l'API marque `offset.now_playing = true`. Mesuré le 01/09/2026 :
/// `bbc_radio_one`, `bbc_radio_fourfm`, `bbc_world_service` et
/// `bbc_radio_scotland_fm` rendaient quatre segments et **aucun** marqué en
/// cours. Prendre `data[0]` y aurait affiché un morceau terminé depuis vingt
/// minutes — une réponse assurée et fausse, ce que ce fichier refuse déjà pour
/// FIP Pop.
///
/// Le titre est `titles.secondary` (l'œuvre) et l'artiste `titles.primary`.
/// Sans `secondary`, on rend `None` : promouvoir l'artiste — ou le nom de la
/// station — dans la case du titre fabriquerait un morceau qui n'existe pas.
fn lire_now_playing_bbc(body: &serde_json::Value, station_name: &str) -> Option<IcyMetadata> {
    let segments = body.get("data")?.as_array()?;

    let en_cours = segments.iter().find(|segment| {
        segment
            .get("offset")
            .and_then(|o| o.get("now_playing"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && segment.get("segment_type").and_then(|v| v.as_str()) == Some("music")
    })?;

    let titres = en_cours.get("titles")?;
    let title = titres.get("secondary").and_then(|v| v.as_str())?.trim();
    if title.is_empty() {
        return None;
    }

    let artist = titres
        .get("primary")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(IcyMetadata {
        title: title.to_string(),
        artist,
        station: Some(station_name.to_string()),
        cover_url: pochette_bbc(en_cours.get("image_url").and_then(|v| v.as_str())),
    })
}

/// `base` est un paramètre pour que la contre-épreuve puisse dresser un faux
/// service : aucun test de ce dépôt n'appelle une vraie radio.
async fn fetch_bbc_metadata(
    base: &str,
    station_name: &str,
    service_id: &str,
) -> Option<IcyMetadata> {
    let url = format!("{base}/v2/services/{service_id}/segments/latest?experience=domestic");
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "bbc_rms_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    lire_now_playing_bbc(&body, station_name)
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

    // -----------------------------------------------------------------------
    // La troisième famille : BBC (#2486)
    // -----------------------------------------------------------------------
    //
    // ## Le fait de base, mesuré le 01/09/2026
    //
    // `http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three` (BBC Radio 3, du
    // catalogue livré) répond 200 **sans** en-tête `icy-metaint`, même en
    // réclamant `Icy-MetaData: 1`. `fetch_icy_metadata` s'arrête donc sur son
    // `?` et l'écran ne montre **rien** : ni titre, ni artiste, ni pochette.
    //
    // Le service RMS de la BBC — public, sans clef — rendait au même instant
    // `titles.primary = "Lennox Berkeley"`,
    // `titles.secondary = "Divertimento, Op.18: I. Prelude. Moderato"`.
    //
    // Aucun test ci-dessous n'appelle une vraie radio : les distants sont des
    // serveurs montés dans le test, qui répondent puis se ferment proprement.

    use serde_json::{Value, json};

    /// Monte un distant local et rend sa racine.
    async fn faux_distant(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("port local");
        let port = listener.local_addr().expect("adresse locale").port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Faux service RMS qui rend `corps` sur la route des segments.
    async fn faux_rms(service_id: &str, corps: serde_json::Value) -> String {
        let chemin = format!("/v2/services/{service_id}/segments/latest");
        let app = axum::Router::new().route(
            &chemin,
            axum::routing::get(move || {
                let c = corps.clone();
                async move { axum::Json(c) }
            }),
        );
        faux_distant(app).await
    }

    /// Un segment RMS, à la forme mesurée.
    fn segment(kind: &str, primary: Option<&str>, secondary: Option<&str>, now: bool) -> Value {
        json!({
            "type": "segment_item",
            "segment_type": kind,
            "titles": { "primary": primary, "secondary": secondary, "tertiary": null },
            "image_url": "https://ichef.bbci.co.uk/images/ic/{recipe}/p03mpgxn.jpg",
            "offset": { "start": 5830, "end": 6053, "now_playing": now },
        })
    }

    /// La charge utile mesurée sur `bbc_radio_three` le 01/09/2026 : quatre
    /// segments, le premier marqué en cours.
    fn charge_radio_three() -> Value {
        json!({ "total": 4, "data": [
            segment("music", Some("Lennox Berkeley"), Some("Divertimento, Op.18: I. Prelude. Moderato"), true),
            segment("music", Some("Cécile Chaminade"), Some("Concertino, Op.107"), false),
            segment("music", Some("Robert Kahn"), Some("Es war der Tag der weißen Chrysanthemen, Op. 61 No. 10"), false),
            segment("music", Some("Antonín Dvořák"), Some("Slavonic Dance No 9 in B major, Op 72"), false),
        ]})
    }

    /// **Le vert.** Une station de la nouvelle famille rend le titre et
    /// l'artiste en cours — et ils sont justes, au caractère près.
    #[tokio::test]
    async fn une_station_bbc_rend_le_titre_et_l_artiste_en_cours() {
        let base = faux_rms("bbc_radio_three", charge_radio_three()).await;
        let meta = fetch_bbc_metadata(&base, "BBC Radio 3", "bbc_radio_three")
            .await
            .expect("le morceau en cours doit sortir du service");

        assert_eq!(meta.title, "Divertimento, Op.18: I. Prelude. Moderato");
        assert_eq!(meta.artist.as_deref(), Some("Lennox Berkeley"));
        assert_eq!(meta.station.as_deref(), Some("BBC Radio 3"));
        assert_eq!(
            meta.cover_url.as_deref(),
            Some("https://ichef.bbci.co.uk/images/ic/320x320/p03mpgxn.jpg"),
            "le gabarit {{recipe}} doit être rempli : servi nu, il répond 403"
        );
    }

    /// **L'absence propre.** Mesuré le 01/09/2026 : `bbc_radio_one`,
    /// `bbc_radio_fourfm`, `bbc_world_service` et `bbc_radio_scotland_fm`
    /// rendaient quatre segments et **aucun** marqué en cours. Rien ne doit
    /// sortir — ni chaîne vide, ni morceau terminé, ni nom de station.
    #[tokio::test]
    async fn une_station_bbc_qui_ne_joue_rien_rend_une_absence_propre() {
        let corps = json!({ "total": 2, "data": [
            segment("music", Some("Billy Ocean"), Some("Caribbean Queen"), false),
            segment("music", Some("DIIV"), Some("The Fountain"), false),
        ]});
        let base = faux_rms("bbc_radio_fourfm", corps).await;
        assert!(
            fetch_bbc_metadata(&base, "BBC Radio 4", "bbc_radio_fourfm")
                .await
                .is_none(),
            "aucun segment n'est en cours : l'écran doit rester au logo"
        );
    }

    /// Un identifiant inconnu rend **400** chez la BBC (mesuré). Un refus ne
    /// doit pas devenir un titre.
    #[tokio::test]
    async fn un_refus_du_service_bbc_ne_fabrique_aucun_titre() {
        let base = faux_distant(axum::Router::new()).await; // tout est 404
        assert!(
            fetch_bbc_metadata(&base, "BBC Machin", "bbc_machin")
                .await
                .is_none()
        );
    }

    /// Le piège que `data[0]` ne montre pas : le morceau en cours n'est pas
    /// toujours le premier de la liste. C'est le drapeau qui décide.
    #[test]
    fn seul_le_segment_marque_en_cours_est_retenu() {
        let corps = json!({ "data": [
            segment("music", Some("Terminé"), Some("Il y a vingt minutes"), false),
            segment("music", Some("DIIV"), Some("The Fountain"), true),
        ]});
        let meta = lire_now_playing_bbc(&corps, "BBC 6 Music").expect("un segment est en cours");
        assert_eq!(meta.title, "The Fountain");
        assert_eq!(meta.artist.as_deref(), Some("DIIV"));
    }

    /// Un segment de parole marqué en cours n'est pas un morceau. Le rendre
    /// mettrait le nom d'une émission dans la case du titre.
    #[test]
    fn un_segment_de_parole_nest_pas_un_morceau() {
        let corps = json!({ "data": [
            segment("speech", Some("Petroc Trelawny"), Some("Breakfast"), true),
        ]});
        assert!(lire_now_playing_bbc(&corps, "BBC Radio 3").is_none());
    }

    /// Sans titre d'œuvre, aucune métadonnée : promouvoir l'artiste — ou le
    /// nom de la station — dans la case du titre inventerait un morceau.
    #[test]
    fn sans_titre_d_oeuvre_rien_ne_prend_sa_place() {
        for corps in [
            json!({ "data": [segment("music", Some("Lennox Berkeley"), None, true)] }),
            json!({ "data": [segment("music", Some("Lennox Berkeley"), Some(""), true)] }),
            json!({ "data": [segment("music", Some("Lennox Berkeley"), Some("   "), true)] }),
            json!({ "data": [] }),
            json!({ "total": 0 }),
        ] {
            assert_eq!(
                lire_now_playing_bbc(&corps, "BBC Radio 3").map(|m| m.title),
                None,
                "aucun titre ne doit être fabriqué à partir de {corps}"
            );
        }
    }

    /// L'identifiant vient de l'URL en priorité — elle dit ce qui est
    /// réellement diffusé. Les deux formes livrées par la BBC le portent en
    /// clair, l'une nue, l'autre noyée dans un HLS.
    #[test]
    fn l_identifiant_bbc_se_lit_dans_l_url() {
        assert_eq!(
            bbc_service_id(
                "BBC Radio 3",
                "http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three"
            )
            .as_deref(),
            Some("bbc_radio_three")
        );
        assert_eq!(
            bbc_service_id(
                "",
                "https://as-hls-ww.live.cf.md.bbci.co.uk/pool/x/live/ww/bbc_6music/bbc_6music.isml/bbc_6music-audio%3d320000.norewind.m3u8"
            )
            .as_deref(),
            Some("bbc_6music"),
            "le .isml et le -audio doivent tomber"
        );
        // L'URL prime sur le nom : c'est elle qui dit ce qui joue.
        assert_eq!(
            bbc_service_id("BBC Radio 3", "http://x/bbc_radio_two").as_deref(),
            Some("bbc_radio_two")
        );
    }

    /// Le nom ne sert qu'en second, et seulement pour les six identifiants
    /// interrogés le 01/09/2026 (tous 200). Rien d'autre n'est deviné.
    #[test]
    fn le_nom_ne_sert_que_pour_les_services_mesures() {
        for (nom, attendu) in [
            ("BBC Radio 3", "bbc_radio_three"),
            ("BBC Radio 1", "bbc_radio_one"),
            ("BBC Radio 2", "bbc_radio_two"),
            ("BBC Radio 4", "bbc_radio_fourfm"),
            ("BBC Radio 6 Music", "bbc_6music"),
            ("BBC World Service", "bbc_world_service"),
        ] {
            assert_eq!(
                bbc_service_id(nom, "http://exemple.invalid/flux.mp3").as_deref(),
                Some(attendu),
                "« {nom} »"
            );
        }
    }

    /// **Témoin.** Aucune station des deux familles déjà couvertes, ni aucune
    /// station quelconque, ne doit être détournée vers la BBC : elles doivent
    /// continuer d'emprunter exactement le chemin qu'elles empruntaient.
    #[test]
    fn aucune_autre_station_n_est_detournee_vers_la_bbc() {
        for (nom, url) in [
            ("FIP", "https://icecast.radiofrance.fr/fip-hifi.aac"),
            ("Radio Paradise", "http://stream.radioparadise.com/aac-320"),
            (
                "TSF Jazz",
                "https://tsfjazz.ice.infomaniak.ch/tsfjazz-high.mp3",
            ),
            ("KEXP", "https://kexp-mp3-128.streamguys1.com/kexp128.mp3"),
            // « bbcmedia » sans souligné n'est pas un identifiant de service,
            // et un nom sans « bbc » isolé non plus.
            (
                "Radio 3",
                "http://stream.live.vc.bbcmedia.co.uk/some_stream",
            ),
            ("Abbey Road Radio", "http://exemple.invalid/flux.mp3"),
            // Un « bbc_ » nu ne doit pas partir chez le distant.
            ("", "http://exemple.invalid/bbc_"),
        ] {
            assert_eq!(
                bbc_service_id(nom, url),
                None,
                "« {nom} » / « {url} » ne doit pas être routé vers la BBC"
            );
        }
    }

    /// Le gabarit `{recipe}` doit être rempli, et rien d'autre ne doit passer :
    /// une accolade survivante répondrait 403, c'est-à-dire une image cassée là
    /// où le logo de la station faisait l'affaire.
    #[test]
    fn la_pochette_bbc_remplit_le_gabarit_et_refuse_le_reste() {
        assert_eq!(
            pochette_bbc(Some(
                "https://ichef.bbci.co.uk/images/ic/{recipe}/p026ktjp.jpg"
            )),
            Some("https://ichef.bbci.co.uk/images/ic/320x320/p026ktjp.jpg".to_string())
        );
        // Déjà remplie, elle passe telle quelle.
        assert_eq!(
            pochette_bbc(Some(
                "https://ichef.bbci.co.uk/images/ic/480x480/p026ktjp.jpg"
            )),
            Some("https://ichef.bbci.co.uk/images/ic/480x480/p026ktjp.jpg".to_string())
        );
        // Un gabarit inconnu, une page, un chemin relatif : rien.
        assert_eq!(
            pochette_bbc(Some("https://ichef.bbci.co.uk/images/ic/{size}/p0.jpg")),
            None
        );
        assert_eq!(
            pochette_bbc(Some("https://www.bbc.co.uk/programmes/b006tp52")),
            None
        );
        assert_eq!(pochette_bbc(Some("/images/ic/{recipe}/p0.jpg")), None);
        assert_eq!(pochette_bbc(Some("   ")), None);
        assert_eq!(pochette_bbc(None), None);
    }

    // --- Témoins sur le repli ICY, des deux côtés du correctif --------------

    /// Un faux flux : `avec_metaint` décide s'il annonce ses métadonnées.
    /// Sans l'en-tête, c'est ce que sert BBC Radio 3 (mesuré).
    async fn faux_flux(avec_metaint: bool) -> String {
        let app = axum::Router::new().fallback(move || async move {
            let mut corps: Vec<u8> = vec![0xAA; 16];
            corps.push(2); // 2 × 16 = 32 octets de bloc
            let mut bloc = b"StreamTitle='A - B';".to_vec();
            bloc.resize(32, 0);
            corps.extend_from_slice(&bloc);

            let mut reponse = axum::response::Response::new(axum::body::Body::from(corps));
            reponse.headers_mut().insert(
                "icy-name",
                axum::http::HeaderValue::from_static("Faux Flux"),
            );
            if avec_metaint {
                reponse
                    .headers_mut()
                    .insert("icy-metaint", axum::http::HeaderValue::from_static("16"));
            }
            reponse
        });
        faux_distant(app).await
    }

    /// **Le rouge, tel qu'il se mesure aujourd'hui.** Un flux qui n'annonce
    /// pas `icy-metaint` — ce que sert BBC Radio 3 — ne rend rien du tout.
    /// C'est l'état d'où part ce correctif, et il ne change pas : la BBC est
    /// désormais servie AVANT ce repli, pas à travers lui.
    #[tokio::test]
    async fn temoin_un_flux_sans_icy_metaint_ne_rend_rien() {
        let url = faux_flux(false).await;
        assert!(
            fetch_icy_metadata(&url).await.is_none(),
            "sans icy-metaint le repli n'a rien à lire"
        );
    }

    /// **Témoin.** Le repli ICY — celui qui sert toutes les stations non
    /// câblées — lit exactement ce qu'il lisait avant.
    #[tokio::test]
    async fn temoin_un_flux_icy_complet_se_lit_comme_avant() {
        let url = faux_flux(true).await;
        let meta = fetch_icy_metadata(&url).await.expect("le bloc ICY se lit");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist.as_deref(), Some("A"));
        assert_eq!(meta.station.as_deref(), Some("Faux Flux"));
        assert_eq!(meta.cover_url, None);
    }
}
