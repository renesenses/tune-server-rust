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
    fetch_radio_metadata_depuis(VoiesRadioFrance::REELLES, station_name, stream_url).await
}

/// Le même parcours, les racines Radio France en paramètre.
///
/// C'est ce que la BBC a déjà fait pour son propre distant (#3139) : la racine
/// est un paramètre pour que la contre-épreuve puisse dresser un faux service.
/// **Aucun test de ce dépôt n'appelle une vraie radio** — ni `livemeta`, ni un
/// flux icecast. Sans ce paramètre, la seule façon d'établir ce que la route
/// rend pour une station serait de l'interroger pour de vrai.
async fn fetch_radio_metadata_depuis(
    voies: VoiesRadioFrance<'_>,
    station_name: &str,
    stream_url: &str,
) -> Option<IcyMetadata> {
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
            return fetch_radiofrance_metadata(voies.pull, station_name, channel).await;
        }
        // `pull` ne les couvre pas, `live` si (#3149) : les trois webradios FIP
        // dont `pull/78`, `pull/95` et `pull/96` répondent 404 sont servies par
        // l'autre voie de la MÊME API. Rien d'autre ne passe par ici : le
        // balayage des identifiants `live` 1 à 300 du 01/09/2026 s'arrête à 100
        // et n'y trouve aucune webradio France Musique (#3142).
        if let Some(id) = radiofrance_live_id(station_name, stream_url) {
            return fetch_radiofrance_live_metadata(voies.live, station_name, id).await;
        }
    }

    // Radio Paradise
    if stream_url.contains("radioparadise")
        || station_name.to_lowercase().contains("radio paradise")
    {
        // Le nom compte autant que l'URL : la porte ci-dessus admet une station
        // reconnue par son SEUL nom, et le resolveur de canal ne lisait que
        // l'URL. Une entree nommee « Radio Paradise - Serenity » posee sur une
        // adresse que Tune ne sait pas decouper tombait donc sur le melange
        // principal (#3523, #3543).
        let chan = match radioparadise_channel(station_name, stream_url) {
            Some(c) => c,
            None => {
                // Aucun canal nomme : c'est le melange principal, dont l'URL ne
                // porte que le format (`flacm`, `aac-128`). On le journalise
                // pour qu'un canal NOUVEAU se lise a la ligne, au lieu
                // d'afficher en silence le morceau d'un autre canal.
                debug!(
                    station = %station_name,
                    url = %stream_url,
                    "radioparadise_canal_par_defaut"
                );
                CANAL_RADIO_PARADISE_PRINCIPAL
            }
        };
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

    // KEXP — quatrième famille (#2486). Mesuré le 19/09/2026 : le flux
    // `kexp-mp3-128.streamguys1.com/kexp128.mp3` annonce bien `icy-metaint`,
    // mais son bloc ICY ne porte qu'un « Artiste - Titre » sans pochette. L'API
    // publique de la station (`api.kexp.org/v2/plays/`) donne le morceau,
    // l'artiste, l'album et la miniature de la pochette — sans clef ni compte.
    if kexp_reconnue(station_name, stream_url) {
        return fetch_kexp_metadata(KEXP_API_BASE, station_name).await;
    }

    // AzuraCast — cinquième famille (#2486), reconnue à la FORME de l'URL et
    // non à une station : `https://<hôte>/listen/<code>/<montage>` est le
    // chemin canonique de ce logiciel de diffusion, et il publie son
    // now-playing sur `https://<hôte>/api/nowplaying/<code>`, sans clef.
    // Trois stations du catalogue livré (jamminvibezradio.com) en sont.
    //
    // La forme peut mentir : un `/listen/x/y` qui n'est PAS un AzuraCast
    // répond 404 à l'API. On retombe alors sur l'ICY, comme avant — jamais
    // sur une absence que ce correctif aurait fabriquée.
    if let Some(api) = azuracast_now_playing_url(stream_url)
        && let Some(meta) = fetch_azuracast_metadata(&api, station_name).await
    {
        return Some(meta);
    }

    // Icecast générique — sixième famille (#2486). Ce n'est pas un diffuseur
    // mais un LOGICIEL : `status-json.xsl` est la page d'état que tout serveur
    // Icecast publie, sans clef ni compte, sur le même hôte que le flux.
    //
    // Elle règle un manque précis, mesuré le 23/09/2026 : les montages
    // **Ogg/FLAC** n'annoncent AUCUN `icy-metaint` — Icecast ne transporte pas
    // de bloc ICY hors MP3/AAC — et `fetch_icy_metadata` s'arrête donc sur son
    // `?`. Neuf stations du catalogue passent par ici, dont cinq sur lesquelles
    // l'écran ne montre RIEN aujourd'hui.
    //
    // Muette ou absente, la page laisse la main au repli ICY, exactement comme
    // avant : aucune absence n'est fabriquée ici.
    if let Some(status) = icecast_status_url(stream_url)
        && let Some(montage) = montage_icecast(stream_url)
        && let Some(meta) = fetch_icecast_status_metadata(&status, station_name, montage).await
    {
        return Some(meta);
    }

    // Fallback: raw ICY metadata
    fetch_icy_metadata(stream_url).await
}

// ---------------------------------------------------------------------------
// Radio France
// ---------------------------------------------------------------------------

/// Racine du service `livemeta` de Radio France — public, sans clef ni compte.
const RADIOFRANCE_LIVEMETA_BASE: &str = "https://api.radiofrance.fr/livemeta/pull";

/// Racine de l'**autre** voie du même service `livemeta` (#3149).
///
/// `livemeta/live/{id}/{preset}` — publique elle aussi, sans clef ni compte :
/// un `400` sur un préréglage inconnu liste d'ailleurs les préréglages valides.
const RADIOFRANCE_LIVE_BASE: &str = "https://api.radiofrance.fr/livemeta/live";

/// Le préréglage du lecteur de webradios du site, mesuré le 01/09/2026.
///
/// C'est un identifiant **interne au lecteur**, pas un contrat : rien ne
/// garantit qu'il survive à une refonte du site. Quand il disparaîtra, la route
/// rendra `400` — `fetch_radiofrance_live_metadata` s'arrête alors sur son test
/// de statut et les trois stations retournent à l'absence propre d'où elles
/// viennent. Aucun écran ne montrera de titre faux pour autant.
const RADIOFRANCE_LIVE_PRESET: &str = "webrf_musique_inter_webradio_player";

/// Les racines des **deux** voies `livemeta` de Radio France.
///
/// Elles voyagent ensemble parce qu'elles se relaient sur la même famille :
/// une station passe par `pull` quand elle y a un canal, par `live` sinon. Les
/// tenir en un seul paramètre évite qu'une contre-épreuve dresse un faux
/// service pour l'une et appelle la **vraie** API pour l'autre.
#[derive(Clone, Copy)]
struct VoiesRadioFrance<'a> {
    /// `livemeta/pull/{canal}` : les stations principales et les huit
    /// webradios FIP qui y ont un canal.
    pull: &'a str,
    /// `livemeta/live/{id}/{preset}` : les trois webradios FIP que `pull`
    /// ignore (404).
    live: &'a str,
}

impl VoiesRadioFrance<'static> {
    /// Les vraies racines — celles qu'emprunte le serveur, jamais un test.
    const REELLES: Self = Self {
        pull: RADIOFRANCE_LIVEMETA_BASE,
        live: RADIOFRANCE_LIVE_BASE,
    };
}

/// Le qualificatif de webradio porté par un nom de station ou une URL de flux.
///
/// `Some("pop")` pour `FIP Pop` comme pour `.../fippop-hifi.aac`,
/// `Some("baroque")` pour `.../francemusiquebaroque-hifi.aac`, et
/// `Some("")` — la chaîne **vide** — pour la station principale de la famille.
///
/// Tout ce qui suit le dernier `prefixe` est retenu, moins le bruit de
/// conditionnement du flux (`-hifi.aac`, `midfi`, l'ancien hôte `fipradio.fr`),
/// pour que l'appelant distingue « c'est la station principale » de « c'est une
/// webradio, et laquelle ».
///
/// **`None` veut dire « ce n'est pas cette station du tout ».** Le préfixe est
/// bien là, mais collé derrière autre chose : il n'ouvre pas son jeton.
/// `monpetitfranceinter` contient `franceinter` sans être France Inter — c'est
/// **Mon Petit France Inter**, une station différente. Lire le mot au milieu
/// d'un autre lui donnait le canal 1, celui d'une station qu'elle n'est pas
/// (#3142).
fn qualificatif_webradio(hay: &str, prefixe: &str) -> Option<String> {
    let index = hay.rfind(prefixe)?;
    if hay[..index]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
    {
        return None;
    }

    Some(
        hay[index + prefixe.len()..]
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
            .join(" "),
    )
}

/// Le canal de la station **principale** d'une famille, et rien d'autre.
///
/// Un qualificatif — quel qu'il soit — veut dire « c'est une webradio ». Aucune
/// des familles servies par cette fonction n'a de webradio dotée d'un canal
/// `livemeta` : le balayage des canaux 1 à 120 du 01/09/2026 n'en a trouvé
/// aucune (les canaux 11 à 56, 68, 90 et 92 sont les stations locales « ici »).
/// Rendre le canal de la mère afficherait chez la fille le morceau de la mère.
/// FIP, la seule famille dont les webradios ont de vrais canaux, garde donc sa
/// propre table juste en dessous.
fn canal_principal_ou_rien(hay: &str, prefixe: &str, canal_principal: u32) -> Option<u32> {
    match qualificatif_webradio(hay, prefixe) {
        Some(qualificatif) if qualificatif.is_empty() => Some(canal_principal),
        _ => None,
    }
}

/// Le premier préfixe de la liste que `hay` porte, s'il y en a un.
fn prefixe_reconnu<'a>(hay: &str, prefixes: &[&'a str]) -> Option<&'a str> {
    prefixes.iter().copied().find(|p| hay.contains(p))
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
///
/// **La même règle vaut pour les autres familles, et elle n'y était pas
/// appliquée (#3142).** Onze stations livrées ou ajoutables recevaient le canal
/// de leur station mère : les neuf webradios de France Musique — Baroque,
/// Classique Easy, Classique Plus, Concerts, Contemporaine, Jazz, Musiques du
/// monde, Opéra, Piano Zen — partaient toutes sur le canal 4, celui de France
/// Musique **principale** ; Mouv Xtra sur le 6 ; Mon Petit France Inter sur le
/// 1. Elles affichaient donc, pendant des heures, le morceau d'une autre
/// station : plausible, au bon format, dans le bon genre, et faux.
///
/// Il n'y a **pas** de bon numéro à mettre à la place. Le balayage des canaux
/// `livemeta` 1 à 120 du 01/09/2026 n'a trouvé **aucun** canal de webradio
/// France Musique (les canaux 11 à 56, 68, 90 et 92 sont les stations locales
/// « ici »). La correspondance n'était pas fausse par étourderie, elle est
/// impossible avec cette source. Et le repli ICY ne les rattrape pas : mesuré
/// flux par flux le 01/09/2026, **aucun** flux `icecast.radiofrance.fr` ne rend
/// l'en-tête `icy-metaint`, même en réclamant `Icy-MetaData: 1` — pas plus
/// `francemusiquebaroque-hifi.aac` que `francemusique-hifi.aac` lui-même. Ces
/// onze stations n'affichent donc plus rien, et c'est le correctif : une
/// absence propre est honnête, un titre faux ne l'est pas.
fn radiofrance_channel_id(station_name: &str, stream_url: &str) -> Option<u32> {
    let hay = format!(
        "{} {}",
        station_name.to_lowercase(),
        stream_url.to_lowercase()
    );
    if let Some(prefixe) = prefixe_reconnu(&hay, &["franceinter"]) {
        canal_principal_ou_rien(&hay, prefixe, 1)
    } else if let Some(prefixe) = prefixe_reconnu(&hay, &["francemusique", "france-musique"]) {
        canal_principal_ou_rien(&hay, prefixe, 4)
    } else if let Some(prefixe) = prefixe_reconnu(&hay, &["mouv"]) {
        canal_principal_ou_rien(&hay, prefixe, 6)
    } else if let Some(prefixe) = prefixe_reconnu(&hay, &["franceculture", "france-culture"]) {
        canal_principal_ou_rien(&hay, prefixe, 2)
    } else if let Some(prefixe) = prefixe_reconnu(&hay, &["franceinfo"]) {
        canal_principal_ou_rien(&hay, prefixe, 3)
    } else if hay.contains("fip") {
        // FIP webradios: pick the specific substation, main FIP when there is
        // no qualifier at all, and nothing when the qualifier is unknown.
        let Some(qualifier) = qualificatif_webradio(&hay, "fip") else {
            return None;
        };
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

async fn fetch_radiofrance_metadata(
    base: &str,
    station_name: &str,
    channel: u32,
) -> Option<IcyMetadata> {
    let url = format!("{base}/{channel}");
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
// Radio France — la voie `live` (#3149)
// ---------------------------------------------------------------------------

/// L'identifiant `livemeta/live` des **trois** webradios FIP que `pull` ignore.
///
/// FIP Pop, FIP Hip-Hop et FIP Sacré français n'affichaient aucun morceau :
/// `pull/78`, `pull/95` et `pull/96` répondent **404**, et leurs flux AAC ne
/// portent pas d'`icy-metaint`. Elles étaient donc éteintes — à raison, tant
/// qu'aucune source ne les couvrait : un titre faux est pire qu'une absence.
///
/// Mesuré le 01/09/2026 : `livemeta/live/{id}/{preset}` — l'**autre voie de la
/// même API** — rend le morceau en cours des trois pour ces mêmes numéros.
///
/// **Trois, et pas une de plus.** L'espace d'identifiants de `live` a été
/// balayé de 1 à 300 : il s'arrête à 100, et aucune webradio France Musique n'y
/// figure. Les neuf de #3142 restent sans source, et restent éteintes. Les huit
/// webradios FIP qui ont un canal `pull` ne passent pas par ici non plus :
/// l'appelant a déjà rendu avant d'arriver à cette fonction.
fn radiofrance_live_id(station_name: &str, stream_url: &str) -> Option<u32> {
    let hay = format!(
        "{} {}",
        station_name.to_lowercase(),
        stream_url.to_lowercase()
    );
    // `None` ici veut dire « ce n'est pas FIP du tout » : `fip` collé derrière
    // autre chose n'ouvre pas son jeton (#3142). La chaîne vide veut dire « FIP
    // principale », qui a son canal `pull` et n'a rien à faire ici.
    let qualificatif = qualificatif_webradio(&hay, "fip")?;
    // Le nom livré et le radical du flux ne s'écrivent pas pareil : « FIP
    // Hip-Hop » rend `hip hop`, `fiphiphop-hifi.aac` rend `hiphop` ; « FIP
    // Sacré français » garde ses accents — `char::is_alphanumeric` est Unicode
    // — là où `fipsacrefrancais-hifi.aac` rend `sacrefrancais`. Les deux
    // écritures sont reconnues, et `hiphop` est cherché avant `pop` pour qu'un
    // ajout de motif ne puisse pas les intervertir.
    if qualificatif.contains("hiphop") || qualificatif.contains("hip hop") {
        Some(95)
    } else if qualificatif.contains("pop") {
        Some(78)
    } else if qualificatif.contains("sacre") || qualificatif.contains("sacré") {
        Some(96)
    } else {
        None
    }
}

/// La marge autour de la fenêtre du morceau, en secondes.
///
/// Elle sépare deux choses mesurées le 01/09/2026, et il y a deux ordres de
/// grandeur entre elles :
///
/// - le **relais normal** : `now` garde le morceau qui vient de finir jusqu'à
///   ce que le suivant soit annoncé — relevé à **21 s** au plus, en
///   échantillonnant les trois stations toutes les 20 s ;
/// - la **réponse périmée** : une même requête a rendu un `now` dont l'`endTime`
///   était **2 131 s** — trente-cinq minutes — dans le passé, avant de
///   redevenir fraîche au sondage suivant.
///
/// Une minute laisse passer le premier et arrête le second.
const RADIOFRANCE_LIVE_TOLERANCE_S: i64 = 60;

/// L'instant présent, en secondes depuis 1970.
fn maintenant_epoch_s() -> Option<i64> {
    Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64,
    )
}

/// Lire le morceau **en cours** dans la réponse de la voie `live`.
///
/// ## La forme, mesurée — elle n'a rien à voir avec celle de `pull`
///
/// `pull` rend `levels[0].items[position] → steps[uuid]`. `live` rend trois
/// cases côte à côte, `prev` / `now` / `next`, plus un `delayToRefresh` ; le
/// morceau est dans `now`, son titre en `firstLine`, son interprète en
/// `secondLine`, son genre en `thirdLine`.
///
/// ## Ce que l'API rend quand rien ne joue
///
/// Elle rend quand même un `now` **complet** — le piège exact que la BBC nous a
/// coûté (#2486). Mais elle le remplit avec la **carte de la station** au lieu
/// d'un morceau : `firstLine` devient `"Le direct"`, `secondLine` la phrase de
/// présentation de la webradio, et surtout `songUuid`, `startTime` et `endTime`
/// passent tous les trois à `null`. Mesuré le 01/09/2026 sur `live/7` (FIP
/// principale), qui rendait cette carte pendant que les webradios rendaient
/// leurs morceaux. Sans garde, l'écran aurait affiché « Le direct » de « La
/// radio la plus éclectique du monde » comme s'il s'agissait d'un titre.
///
/// ## Ce que l'API rend quand elle est en retard
///
/// La même réponse, complète et plausible, mais périmée : un `now` dont
/// l'`endTime` était **2 131 s** dans le passé a été servi, puis la requête
/// suivante est redevenue fraîche. Une charge utile pleine ne prouve donc pas
/// qu'elle décrit l'instant présent.
///
/// ## La garde
///
/// Trois conditions, et le morceau n'est rendu que si les trois tiennent :
/// `songUuid` est une chaîne non vide — c'est ce qui distingue un morceau d'une
/// carte de station —, `startTime` et `endTime` sont tous deux présents, et
/// **l'instant présent tombe dans la fenêtre `[startTime, endTime]`**, élargie
/// de [`RADIOFRANCE_LIVE_TOLERANCE_S`] de chaque côté. `maintenant` est un
/// paramètre pour que la contre-épreuve puisse placer l'horloge où elle veut.
fn lire_morceau_en_cours_radiofrance_live(
    body: &serde_json::Value,
    station_name: &str,
    maintenant: i64,
) -> Option<IcyMetadata> {
    let now = body.get("now")?;

    // Une carte de station n'a pas d'UUID de morceau : c'est le signe qu'il n'y
    // a rien à afficher, pas une métadonnée manquante.
    let uuid = now.get("songUuid").and_then(|v| v.as_str())?;
    if uuid.trim().is_empty() {
        return None;
    }

    let debut = now.get("startTime").and_then(serde_json::Value::as_i64)?;
    let fin = now.get("endTime").and_then(serde_json::Value::as_i64)?;
    if maintenant + RADIOFRANCE_LIVE_TOLERANCE_S < debut
        || maintenant - RADIOFRANCE_LIVE_TOLERANCE_S > fin
    {
        debug!(
            station = %station_name,
            retard_s = maintenant - fin,
            "radiofrance_live_hors_fenetre"
        );
        return None;
    }

    let title = now.get("firstLine").and_then(|v| v.as_str())?.trim();
    if title.is_empty() {
        return None;
    }
    let artist = now
        .get("secondLine")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some(IcyMetadata {
        title: title.to_string(),
        artist,
        station: Some(station_name.to_string()),
        // `cover` ne porte pas une adresse : c'est un UUID nu
        // (`095da3c1-5474-…`), et les racines candidates sondées le 01/09/2026
        // rendent toutes 401, 403 ou 404. `url_de_pochette` le rejette donc, et
        // l'écran garde le logo de la station — un logo juste vaut mieux qu'une
        // image cassée.
        cover_url: url_de_pochette(now.get("cover").and_then(|v| v.as_str())),
    })
}

/// `base` est un paramètre pour que la contre-épreuve puisse dresser un faux
/// service : aucun test de ce dépôt n'appelle une vraie radio.
async fn fetch_radiofrance_live_metadata(
    base: &str,
    station_name: &str,
    id: u32,
) -> Option<IcyMetadata> {
    let url = format!("{base}/{id}/{RADIOFRANCE_LIVE_PRESET}");
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    // C'est ici que se règle la disparition du préréglage : la route rendra
    // `400`, on s'arrête, et la station retourne à l'absence.
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "radiofrance_live_api_error");
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    lire_morceau_en_cours_radiofrance_live(&body, station_name, maintenant_epoch_s()?)
}

// ---------------------------------------------------------------------------
// Radio Paradise
// ---------------------------------------------------------------------------

/// Le melange principal de Radio Paradise : son URL de flux ne porte QUE le
/// format (`flacm`, `aac-128`, `aac-320`, `mp3-192`), aucun nom de canal.
/// C'est donc la valeur de repli, et elle est juste.
const CANAL_RADIO_PARADISE_PRINCIPAL: u32 = 0;

/// Les canaux de Radio Paradise, `stream_name` → numero `chan` de l'API.
///
/// **Mesure du 08/09/2026**, `https://api.radioparadise.com/api/list_chan` :
/// SEPT canaux, et leurs numeros NE SE SUIVENT PAS.
///
/// | `chan` | titre | `stream_name` |
/// |---|---|---|
/// | 0 | The Main Mix | `main-mix` |
/// | 1 | Mellow Mix | `mellow` |
/// | 2 | RockIt! | `rock` |
/// | 3 | The Globe | `global` |
/// | 5 | Beyond… | `beyond` |
/// | 42 | Serenity | `serenity` |
/// | 945 | KFAT | `kfat` |
///
/// Les sept repondent : `now_playing?chan={n}` rend un artiste, un titre et une
/// pochette pour chacun (mesure du 08/09/2026). Les numeros absents de cette
/// table — 4, 6, 7, 9 — rendent, eux, un titre VIDE : on ne les devine pas, et
/// c'est pourquoi cette table vient de `list_chan` et non d'un comptage.
///
/// Une URL de flux se lit `stream.radioparadise.com/{stream_name}-{format}`.
///
/// `world` et `rockit` sont des ALIAS : `world` est le nom que la table
/// d'origine de ce fichier donnait au canal 3, que Radio Paradise publie
/// aujourd'hui sous `global` ; `rockit` est le titre du canal 2. Les garder
/// n'ajoute aucun canal, cela evite seulement qu'une URL ou un nom deja saisi
/// cesse d'etre reconnu.
const CANAUX_RADIO_PARADISE: &[(&str, u32)] = &[
    ("mellow", 1),
    ("rock", 2),
    ("rockit", 2),
    ("global", 3),
    ("world", 3),
    ("beyond", 5),
    ("serenity", 42),
    ("kfat", 945),
];

/// Le canal de Radio Paradise designe par cette station, s'il est nomme.
///
/// **L'URL decide d'abord**, parce qu'elle dit ce qui est reellement diffuse —
/// c'est deja la doctrine de `bbc_service_id` : l'annuaire porte une entree
/// nommee « Radio Paradise » tout court sur `.../mellow-flacm`, et c'est le
/// Mellow Mix qui joue. **Le nom ne sert qu'en second**, pour les entrees dont
/// l'adresse ne porte pas le canal.
///
/// Le rapprochement se fait sur des JETONS ENTIERS, pas sur une inclusion :
/// `contains("rock")` mordait sur n'importe quel mot qui contient « rock ».
/// On decoupe donc sur tout ce qui n'est ni lettre ni chiffre, et l'on compare
/// le jeton en entier.
///
/// Rend `None` quand rien n'est nomme : le site d'appel en fait le melange
/// principal, et le journalise.
fn radioparadise_channel(station_name: &str, stream_url: &str) -> Option<u32> {
    // `chan=N` reste lu en premier : c'est le canal ecrit noir sur blanc.
    if let Some(chan) = canal_ecrit_dans_l_url(stream_url) {
        return Some(chan);
    }
    canal_par_jetons(stream_url).or_else(|| canal_par_jetons(station_name))
}

/// Le `chan=N` d'une URL, quand N est un canal que Radio Paradise publie.
///
/// Le nombre est lu ENTIER : `chan=1` ne doit pas mordre sur `chan=12`, qui
/// n'est pas le Mellow Mix mais un canal inexistant.
fn canal_ecrit_dans_l_url(stream_url: &str) -> Option<u32> {
    let apres = stream_url.split("chan=").nth(1)?;
    let nombre: String = apres.chars().take_while(|c| c.is_ascii_digit()).collect();
    let chan: u32 = nombre.parse().ok()?;
    (chan == CANAL_RADIO_PARADISE_PRINCIPAL
        || CANAUX_RADIO_PARADISE.iter().any(|(_, c)| *c == chan))
    .then_some(chan)
}

/// Chercher un `stream_name` de canal parmi les jetons d'une chaine.
fn canal_par_jetons(source: &str) -> Option<u32> {
    source
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .find_map(|jeton| {
            CANAUX_RADIO_PARADISE
                .iter()
                .find(|(nom, _)| *nom == jeton)
                .map(|(_, chan)| *chan)
        })
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
// KEXP (#2486)
// ---------------------------------------------------------------------------

/// Racine de l'API publique de KEXP (Seattle) — sans clef ni compte.
const KEXP_API_BASE: &str = "https://api.kexp.org";

/// L'hôte d'une URL, sans schéma, sans chemin, sans port.
fn hote_de_l_url(url: &str) -> Option<&str> {
    let sans_schema = url.split_once("://")?.1;
    let hote = sans_schema.split(['/', '?', '#']).next()?;
    let hote = hote.rsplit_once('@').map_or(hote, |(_, h)| h);
    let hote = hote.split(':').next()?;
    (!hote.is_empty()).then_some(hote)
}

/// KEXP se reconnaît à son hôte — `live-mp3-128.kexp.org` comme le relais
/// `kexp-mp3-128.streamguys1.com` du catalogue livré — ou, à défaut, au nom
/// de la station pris comme un JETON ENTIER : « KEXP » oui, « Kexpo » non.
fn kexp_reconnue(station_name: &str, stream_url: &str) -> bool {
    let url = stream_url.to_lowercase();
    if let Some(hote) = hote_de_l_url(&url) {
        if hote == "kexp.org" || hote.ends_with(".kexp.org") {
            return true;
        }
        if hote
            .split('.')
            .next()
            .is_some_and(|premier| premier == "kexp" || premier.starts_with("kexp-"))
        {
            return true;
        }
    }
    station_name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|mot| mot == "kexp")
}

/// Lire le morceau **en cours** dans la réponse `v2/plays/`.
///
/// `results[0]` est la dernière entrée d'antenne, pas forcément un morceau :
/// mesuré le 19/09/2026, une entrée sur huit est un `airbreak` — sans
/// `artist`, sans `song`, sans image. Rendre alors `results[1]` afficherait un
/// morceau terminé depuis quatre minutes, avec l'assurance d'un vrai
/// now-playing. On rend `None` : l'écran garde le logo de la station.
///
/// Le titre est `song`, l'artiste `artist`, la pochette `thumbnail_uri` — une
/// adresse `archive.org` en `.jpg` quand la station l'a, une chaîne VIDE sinon
/// (mesuré sur « Live on KEXP »). Le filtre de pochette du fichier écarte la
/// chaîne vide de lui-même.
fn lire_now_playing_kexp(body: &serde_json::Value, station_name: &str) -> Option<IcyMetadata> {
    let dernier = body.get("results")?.as_array()?.first()?;
    if dernier.get("play_type").and_then(|v| v.as_str()) != Some("trackplay") {
        return None;
    }

    let title = dernier.get("song").and_then(|v| v.as_str())?.trim();
    if title.is_empty() {
        return None;
    }

    let artist = dernier
        .get("artist")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(IcyMetadata {
        title: title.to_string(),
        artist,
        station: Some(station_name.to_string()),
        cover_url: pochette_de_stream_url(dernier.get("thumbnail_uri").and_then(|v| v.as_str())),
    })
}

/// `base` est un paramètre pour que la contre-épreuve puisse dresser un faux
/// service : aucun test de ce dépôt n'appelle une vraie radio.
async fn fetch_kexp_metadata(base: &str, station_name: &str) -> Option<IcyMetadata> {
    let url = format!("{base}/v2/plays/?limit=1");
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "kexp_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    lire_now_playing_kexp(&body, station_name)
}

// ---------------------------------------------------------------------------
// AzuraCast (#2486)
// ---------------------------------------------------------------------------

/// L'adresse du now-playing AzuraCast portée par la FORME d'une URL de flux.
///
/// `https://<hôte>/listen/<code>/<montage>` (ou `/hls/<code>/…`) est le chemin
/// canonique de ce logiciel : le `<code>` est l'identifiant court de la
/// station, celui que l'API attend dans `/api/nowplaying/<code>`. Il faut les
/// TROIS segments — un `/listen/flux.mp3` à deux segments n'a pas de code, et
/// n'est pas un AzuraCast. L'origine (schéma, hôte, port) est reprise telle
/// quelle : l'API vit sur le même hôte que le flux.
fn azuracast_now_playing_url(stream_url: &str) -> Option<String> {
    let (schema, reste) = stream_url.split_once("://")?;
    if schema != "http" && schema != "https" {
        return None;
    }
    let (hote, chemin) = reste.split_once('/')?;
    if hote.is_empty() {
        return None;
    }
    let mut segments = chemin.split(['?', '#']).next()?.split('/');
    let premier = segments.next()?;
    if premier != "listen" && premier != "hls" {
        return None;
    }
    let code = segments.next()?;
    let montage = segments.next()?;
    let code_valide = !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !code_valide || montage.is_empty() {
        return None;
    }
    Some(format!("{schema}://{hote}/api/nowplaying/{code}"))
}

/// La pochette qu'AzuraCast publie pour le morceau en cours — et seulement
/// celle-là.
///
/// `song.art` porte deux formes : l'image du morceau, servie par
/// `/api/station/<code>/art/<id>.jpg` (mesuré le 19/09/2026 sur
/// `online.jamminvibezradio.com`), et, quand la station n'en a pas, une image
/// GÉNÉRIQUE du logiciel (`/static/img/generic_song.jpg` ou une
/// `album_art.<n>.png` téléversée pour toute la station). Servir la seconde
/// remplacerait le logo de la station — juste — par un visuel qui ne dit rien
/// du morceau. On ne retient que le chemin `/art/`.
fn pochette_azuracast(art: Option<&str>) -> Option<String> {
    let url = pochette_de_stream_url(art)?;
    let chemin = url.split_once("://")?.1.split_once('/')?.1;
    let chemin = chemin.split(['?', '#']).next().unwrap_or(chemin);
    chemin.contains("/art/").then_some(url)
}

/// Lire `now_playing.song` — titre, artiste, pochette. Sans titre, `None` :
/// le champ `text` (« Artiste - Album - Titre ») n'est pas repris, il mêle
/// trois choses que les champs structurés donnent déjà séparées.
fn lire_now_playing_azuracast(body: &serde_json::Value, station_name: &str) -> Option<IcyMetadata> {
    let song = body.get("now_playing")?.get("song")?;
    let title = song.get("title").and_then(|v| v.as_str())?.trim();
    if title.is_empty() {
        return None;
    }

    let artist = song
        .get("artist")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(IcyMetadata {
        title: title.to_string(),
        artist,
        station: Some(station_name.to_string()),
        cover_url: pochette_azuracast(song.get("art").and_then(|v| v.as_str())),
    })
}

/// `api_url` sort de [`azuracast_now_playing_url`] : la contre-épreuve dresse
/// un faux hôte et passe une URL de flux qui pointe dessus.
async fn fetch_azuracast_metadata(api_url: &str, station_name: &str) -> Option<IcyMetadata> {
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(api_url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "azuracast_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    lire_now_playing_azuracast(&body, station_name)
}

// ---------------------------------------------------------------------------
// Icecast générique (#2486)
// ---------------------------------------------------------------------------

/// La page d'état d'un serveur Icecast, à l'adresse que le logiciel impose.
///
/// `https://<hôte>/status-json.xsl` — même schéma, même hôte, même port que le
/// flux. Aucune clef, aucun compte : c'est la page que l'administrateur
/// d'Icecast publie par défaut depuis la 2.4.
fn icecast_status_url(stream_url: &str) -> Option<String> {
    let (schema, reste) = stream_url.split_once("://")?;
    if schema != "http" && schema != "https" {
        return None;
    }
    let (hote, _) = reste.split_once('/')?;
    if hote.is_empty() {
        return None;
    }
    Some(format!("{schema}://{hote}/status-json.xsl"))
}

/// Le **montage** d'une URL de flux : son chemin, sans la barre de tête.
///
/// C'est la seule partie de `listenurl` sur laquelle on peut s'appuyer pour
/// retrouver son flux dans la page d'état. Mesuré le 23/09/2026 sur deux
/// stations du catalogue, l'hôte et le port du `listenurl` ne sont PAS ceux par
/// lesquels on écoute :
///
/// - Radio Calico : catalogue `https://stream.radio-calico.com/calico`,
///   `listenurl` `http://stream.radio-calico.com:8080/calico` (autre schéma,
///   autre port) ;
/// - Linn Jazz : catalogue `http://radio.linn.co.uk:8003/autodj`,
///   `listenurl` `http://radio.linnrecords.com:8003/autodj` (autre nom d'hôte).
///
/// Comparer les URL entières ne trouverait donc NI l'une NI l'autre.
fn montage_icecast(url: &str) -> Option<&str> {
    let (_, reste) = url.split_once("://")?;
    let (_, chemin) = reste.split_once('/')?;
    let chemin = chemin.split(['?', '#']).next()?.trim_end_matches('/');
    (!chemin.is_empty()).then_some(chemin)
}

/// Les montages déclarés par la page d'état.
///
/// `icestats.source` est **polymorphe** : Icecast écrit un tableau quand le
/// serveur porte plusieurs montages, et un objet nu quand il n'en porte qu'un.
/// Ne lire que le tableau raterait tout serveur mono-montage.
fn sources_icecast(body: &serde_json::Value) -> Vec<&serde_json::Value> {
    match body.get("icestats").and_then(|v| v.get("source")) {
        Some(serde_json::Value::Array(v)) => v.iter().collect(),
        Some(v @ serde_json::Value::Object(_)) => vec![v],
        _ => Vec::new(),
    }
}

/// Le nom du **programme** derrière un `server_name` de montage.
///
/// Un même programme est diffusé en plusieurs formats, et Icecast nomme chaque
/// montage en accolant sa qualité au nom de la station. Relevé le 23/09/2026 :
/// « Radio Calico - lossless » et « Radio Calico - 192kpbs », « Pure Classix
/// Radio » et « Pure Classix Radio MP3 ». On retire donc le qualificatif
/// introduit par « - », et l'on compare en minuscules.
fn nom_de_programme(server_name: &str) -> String {
    let base = server_name
        .split_once(" - ")
        .map_or(server_name, |(gauche, _)| gauche);
    base.trim().to_lowercase()
}

/// Deux montages diffusent-ils le même programme ?
///
/// Oui si l'un des noms normalisés est préfixe de l'autre **à la frontière d'un
/// mot** — « pure classix radio » couvre « pure classix radio mp3 », mais pas
/// « pure lounge radio » (programme différent sur le même serveur, mesuré sur
/// `mscp4.live-streams.nl:8140`) ni « jb radio2 » (autre programme de
/// `mediacp.jb-radio.net:8001`, que le seul préfixe littéral aurait admis).
fn meme_programme(a: &str, b: &str) -> bool {
    fn couvre(court: &str, long: &str) -> bool {
        !court.is_empty()
            && long.starts_with(court)
            && long[court.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == ' ' || c == '-')
    }
    couvre(a, b) || couvre(b, a)
}

/// Le morceau en cours d'un montage : titre, et artiste quand il est donné à
/// part.
///
/// Trois formes coexistent dans les relevés du 23/09/2026 :
/// - `artist` et `title` séparés (Radio Calico, montage MP3) ;
/// - `title` seul portant « Artiste - Titre » (BluesWave, Linn, Morow) ;
/// - `yp_currently_playing` seul, même forme (repli).
///
/// Sans titre — ou avec un titre vide une fois l'artiste retiré — on ne rend
/// rien : le montage `live` de JB Radio2 annonce `yp_currently_playing =
/// "JB RADIO2 - "`, qui est le nom de la station et non un morceau.
fn morceau_icecast(source: &serde_json::Value) -> Option<(String, Option<String>)> {
    let texte = |cle: &str| {
        source
            .get(cle)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    if let (Some(titre), Some(artiste)) = (texte("title"), texte("artist")) {
        return Some((titre.to_string(), Some(artiste.to_string())));
    }

    let brut = texte("title").or_else(|| texte("yp_currently_playing"))?;
    match brut.split_once(" - ") {
        Some((artiste, titre)) => {
            let (artiste, titre) = (artiste.trim(), titre.trim());
            if titre.is_empty() {
                return None;
            }
            Some((
                titre.to_string(),
                (!artiste.is_empty()).then(|| artiste.to_string()),
            ))
        }
        None => Some((brut.to_string(), None)),
    }
}

/// Lire le morceau en cours du montage écouté dans une page d'état Icecast.
///
/// Deux passes, et la seconde est le cœur du correctif : le montage **sans
/// perte** ne porte jamais de titre, parce qu'Icecast ne suit le titre en cours
/// que pour les montages MP3/AAC. Son jumeau compressé, lui, le porte. On
/// n'accepte ce jumeau que s'il diffuse le **même programme**
/// ([`meme_programme`]), et on le journalise — jamais en silence.
///
/// Aucune pochette : une page d'état Icecast n'en publie pas. `cover_url` reste
/// `None`, donc le logo de la station — juste — reste à l'écran.
fn lire_status_icecast(
    body: &serde_json::Value,
    station_name: &str,
    montage: &str,
) -> Option<IcyMetadata> {
    let sources = sources_icecast(body);
    let mien = sources.iter().find(|s| {
        s.get("listenurl")
            .and_then(|v| v.as_str())
            .and_then(montage_icecast)
            == Some(montage)
    })?;

    let rendre = |(title, artist): (String, Option<String>)| IcyMetadata {
        title,
        artist,
        station: Some(station_name.to_string()),
        cover_url: None,
    };

    if let Some(morceau) = morceau_icecast(mien) {
        return Some(rendre(morceau));
    }

    let mon_programme = nom_de_programme(mien.get("server_name")?.as_str()?);
    for voisin in &sources {
        if std::ptr::eq(*voisin, *mien) {
            continue;
        }
        let Some(nom) = voisin.get("server_name").and_then(|v| v.as_str()) else {
            continue;
        };
        if !meme_programme(&mon_programme, &nom_de_programme(nom)) {
            continue;
        }
        if let Some(morceau) = morceau_icecast(voisin) {
            debug!(
                station = %station_name,
                montage = %montage,
                voisin = %nom,
                "icecast_titre_pris_sur_un_montage_jumeau"
            );
            return Some(rendre(morceau));
        }
    }
    None
}

/// `status_url` sort de [`icecast_status_url`] : la contre-épreuve dresse un
/// faux hôte et passe une URL de flux qui pointe dessus.
async fn fetch_icecast_status_metadata(
    status_url: &str,
    station_name: &str,
    montage: &str,
) -> Option<IcyMetadata> {
    let client = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client.get(status_url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "icecast_status_absent");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    lire_status_icecast(&body, station_name, montage)
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
        let q = |hay: &str| qualificatif_webradio(hay, "fip");
        assert_eq!(
            q("fip https://icecast.radiofrance.fr/fip-hifi.aac").as_deref(),
            Some("")
        );
        assert_eq!(
            q(" https://icecast.radiofrance.fr/fip-midfi.mp3").as_deref(),
            Some("")
        );
        assert_eq!(
            q(" http://direct.fipradio.fr/live/fip-midfi.mp3").as_deref(),
            Some("")
        );
        assert_eq!(q("fip ").as_deref(), Some(""));

        // …while a substation keeps its qualifier, from the name or the URL.
        assert_eq!(q("fip pop ").as_deref(), Some("pop"));
        assert_eq!(
            q(" https://icecast.radiofrance.fr/fipsacrefrancais-hifi.aac").as_deref(),
            Some("sacrefrancais")
        );
        assert_eq!(
            q(" https://icecast.radiofrance.fr/fiprock-hifi.aac").as_deref(),
            Some("rock")
        );
    }

    /// Le même découpage, sur les autres familles : la station principale rend
    /// la chaîne vide, la webradio rend son qualificatif.
    #[test]
    fn le_qualificatif_se_lit_aussi_sur_les_autres_familles() {
        let url = |s: &str| format!("https://icecast.radiofrance.fr/{s}-hifi.aac");
        assert_eq!(
            qualificatif_webradio(&url("francemusique"), "francemusique").as_deref(),
            Some("")
        );
        assert_eq!(
            qualificatif_webradio(&url("francemusiquebaroque"), "francemusique").as_deref(),
            Some("baroque")
        );
        assert_eq!(
            qualificatif_webradio(&url("francemusiqueconcertsradiofrance"), "francemusique")
                .as_deref(),
            Some("concertsradiofrance")
        );
        assert_eq!(
            qualificatif_webradio(&url("mouv"), "mouv").as_deref(),
            Some("")
        );
        assert_eq!(
            qualificatif_webradio(&url("mouvxtra"), "mouv").as_deref(),
            Some("xtra")
        );
    }

    /// **La borne de #3142.** Un préfixe collé DERRIÈRE autre chose ne désigne
    /// pas la station : `monpetitfranceinter` porte bien `franceinter`, mais
    /// c'est Mon Petit France Inter, pas France Inter. Sans cette borne, la
    /// station recevait le canal 1 et affichait le morceau de France Inter.
    #[test]
    fn un_prefixe_colle_derriere_autre_chose_ne_designe_pas_la_station() {
        assert_eq!(
            qualificatif_webradio(
                "mon petit france inter https://icecast.radiofrance.fr/monpetitfranceinter-hifi.aac",
                "franceinter"
            ),
            None
        );
        // Le même mot, seul dans son jeton, reste France Inter.
        assert_eq!(
            qualificatif_webradio(
                "france inter https://icecast.radiofrance.fr/franceinter-hifi.aac",
                "franceinter"
            )
            .as_deref(),
            Some("")
        );
    }

    /// Le contrat d'origine, mot pour mot : rien de ce qui etait reconnu ne
    /// cesse de l'etre. `world-320` compris — Radio Paradise ne publie plus ce
    /// nom, l'alias le garde lisible.
    #[test]
    fn radioparadise_channel_detection() {
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://stream.radioparadise.com/aac-320"),
            None
        );
        assert_eq!(
            radioparadise_channel(
                "Radio Paradise",
                "http://stream.radioparadise.com/mellow-320"
            ),
            Some(1)
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://stream.radioparadise.com/rock-320"),
            Some(2)
        );
        assert_eq!(
            radioparadise_channel(
                "Radio Paradise",
                "http://stream.radioparadise.com/world-320"
            ),
            Some(3)
        );
    }

    /// Les SEPT canaux publies par Radio Paradise — table mesuree le
    /// 08/09/2026 sur `list_chan` — se resolvent par l'URL que l'annuaire
    /// mozaiklabs sert aujourd'hui.
    ///
    /// **C'est ici que le defaut se voit** : avant, `global-flacm` et
    /// `serenity-flac` ne portaient aucun des trois mots-clefs connus
    /// (`mellow`, `rock`, `world`) et tombaient sur le melange principal. Deux
    /// stations de l'annuaire (id 70 « Radio Paradise - Global Mix » et id 71
    /// « Radio Paradise - Serenity ») affichaient donc, en silence, le morceau
    /// d'un AUTRE canal (#3523, #3543).
    #[test]
    fn les_sept_canaux_de_radio_paradise_se_resolvent_par_leur_url() {
        for (url, attendu) in [
            // Melange principal : l'URL ne porte que le format.
            ("http://stream.radioparadise.com/flacm", None),
            ("http://stream.radioparadise.com/aac-128", None),
            ("http://stream.radioparadise.com/mellow-flacm", Some(1)),
            ("http://stream.radioparadise.com/rock-flacm", Some(2)),
            ("http://stream.radioparadise.com/rock-flac", Some(2)),
            ("http://stream.radioparadise.com/global-flacm", Some(3)),
            ("http://stream.radioparadise.com/beyond-flacm", Some(5)),
            ("http://stream.radioparadise.com/serenity-flac", Some(42)),
            ("http://stream.radioparadise.com/kfat-flacm", Some(945)),
        ] {
            assert_eq!(
                radioparadise_channel("Radio Paradise", url),
                attendu,
                "canal attendu pour {url}"
            );
        }
    }

    /// La porte d'entree (`fetch_radio_metadata_depuis`) admet une station
    /// reconnue par son SEUL nom. Le resolveur doit donc lire le nom lui aussi,
    /// sans quoi toute entree dont l'adresse ne porte pas le canal retombe sur
    /// le melange principal. Les noms sont ceux de l'annuaire mozaiklabs,
    /// releves le 08/09/2026.
    #[test]
    fn le_canal_se_resout_aussi_par_le_nom_quand_l_url_ne_le_porte_pas() {
        for (nom, attendu) in [
            ("Radio Paradise", None),
            ("Radio Paradise - Main Mix", None),
            ("Radio Paradise - Mellow Mix", Some(1)),
            ("Radio Paradise Rock Mix", Some(2)),
            ("Radio Paradise - Global Mix", Some(3)),
            ("Radio Paradise - Serenity", Some(42)),
        ] {
            assert_eq!(
                radioparadise_channel(nom, "http://exemple.invalid/paradise"),
                attendu,
                "canal attendu pour le nom {nom}"
            );
        }
    }

    /// L'URL passe AVANT le nom : l'annuaire porte une entree nommee « Radio
    /// Paradise » tout court (id 66) posee sur `.../mellow-flacm`, et c'est le
    /// Mellow Mix qui joue. Le nom ne doit pas ecraser ce que l'adresse dit.
    #[test]
    fn l_url_decide_avant_le_nom() {
        assert_eq!(
            radioparadise_channel(
                "Radio Paradise - Main Mix",
                "http://stream.radioparadise.com/rock-flacm"
            ),
            Some(2)
        );
        assert_eq!(
            radioparadise_channel(
                "Radio Paradise",
                "http://stream.radioparadise.com/global-flacm"
            ),
            Some(3)
        );
    }

    /// **Temoin.** Le rapprochement se fait sur un JETON ENTIER. Un mot qui
    /// contient seulement le nom d'un canal ne designe aucun canal — sinon
    /// « worldwide » vaudrait « world », et le `contains` d'origine le faisait.
    #[test]
    fn un_mot_qui_contient_le_nom_d_un_canal_ne_designe_aucun_canal() {
        assert_eq!(
            radioparadise_channel(
                "Radio Paradise Worldwide",
                "http://exemple.invalid/flux.mp3"
            ),
            None
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/rockabilly-320"),
            None
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise Beyonce", "http://exemple.invalid/flux.mp3"),
            None
        );
    }

    /// `chan=N` est lu comme un NOMBRE, pas comme un prefixe : `chan=12`
    /// n'existe pas chez Radio Paradise et ne vaut pas le Mellow Mix.
    #[test]
    fn le_parametre_chan_se_lit_en_entier() {
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/?chan=1"),
            Some(1)
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/?chan=42"),
            Some(42)
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/?chan=945"),
            Some(945)
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/?chan=0"),
            Some(0)
        );
        assert_eq!(
            radioparadise_channel("Radio Paradise", "http://exemple.invalid/?chan=12"),
            None
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
        faux_distant(routeur_faux_flux(avec_metaint)).await
    }

    /// Le même faux flux, en routeur : les témoins AzuraCast le montent SOUS
    /// une route d'API pour établir qui, de l'API ou de l'ICY, sert l'écran.
    fn routeur_faux_flux(avec_metaint: bool) -> axum::Router {
        axum::Router::new().fallback(move || async move {
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
        })
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

    // -----------------------------------------------------------------------
    // #3142 — onze stations affichaient le morceau d'une AUTRE station
    // -----------------------------------------------------------------------
    //
    // ## Le fait de base
    //
    // Les neuf webradios de France Musique partaient sur le canal 4, celui de
    // France Musique **principale** ; Mouv Xtra sur le 6, celui de Mouv' ; Mon
    // Petit France Inter sur le 1, celui de France Inter. Elles affichaient
    // donc le morceau d'une autre station : plausible, au bon format, dans le
    // bon genre, et faux, sans que rien ne le signale.
    //
    // ## Pourquoi ne rien afficher, plutôt que corriger le numéro
    //
    // Il n'y a pas de bon numéro. Balayage des canaux `livemeta` 1 à 120 le
    // 01/09/2026 : **aucun** canal de webradio France Musique n'existe.
    //
    // ## Pourquoi le repli ICY ne les rattrape pas
    //
    // Mesuré flux par flux le 01/09/2026, `Icy-MetaData: 1` réclamé :
    // `francemusique`, `francemusiquebaroque`, `francemusiqueeasyclassique`,
    // `francemusiqueclassiqueplus`, `francemusiqueconcertsradiofrance`,
    // `francemusiquelacontemporaine`, `francemusiquelajazz`,
    // `francemusiqueopera`, `francemusiquepianozen`, `monpetitfranceinter`,
    // `fiprock` répondent 200 **sans** en-tête `icy-metaint` et ne portent
    // aucun `StreamTitle` ; `francemusiqueocoramondial` et `mouvxtra`
    // répondent 404. Le repli ICY couvre donc **zéro** des onze : le correctif
    // est bien une absence, pas un basculement de source.
    //
    // Aucun test ci-dessous n'appelle une vraie radio : le `livemeta` comme le
    // flux sont des serveurs montés dans le test.

    /// Les onze du relevé : le nom livré, et le radical du flux Radio France.
    const ONZE_STATIONS_SANS_SOURCE: &[(&str, &str)] = &[
        ("France Musique Baroque", "francemusiquebaroque"),
        (
            "France Musique Classique Easy",
            "francemusiqueeasyclassique",
        ),
        (
            "France Musique Classique Plus",
            "francemusiqueclassiqueplus",
        ),
        (
            "France Musique Concerts",
            "francemusiqueconcertsradiofrance",
        ),
        (
            "France Musique Contemporaine",
            "francemusiquelacontemporaine",
        ),
        ("France Musique Jazz", "francemusiquelajazz"),
        (
            "France Musique Musiques du monde",
            "francemusiqueocoramondial",
        ),
        ("France Musique Opéra", "francemusiqueopera"),
        ("France Musique Piano Zen", "francemusiquepianozen"),
        ("Mouv Xtra", "mouvxtra"),
        ("Mon Petit France Inter", "monpetitfranceinter"),
    ];

    /// Faux `livemeta`, à la forme que lit `fetch_radiofrance_metadata`.
    ///
    /// Chaque canal rend un morceau **qui le nomme** : deux canaux différents
    /// ne peuvent donc pas rendre le même titre, et une station qui affiche le
    /// morceau d'une autre se voit au premier coup d'œil.
    async fn faux_livemeta() -> String {
        let app = axum::Router::new().fallback(|uri: axum::http::Uri| async move {
            let canal = uri.path().trim_start_matches('/').to_string();
            axum::Json(json!({
                "levels": [{ "position": 0, "items": ["pas-en-cours"] }],
                "steps": {
                    "pas-en-cours": {
                        "title": format!("Morceau du canal {canal}"),
                        "authors": format!("Interprète du canal {canal}"),
                    }
                },
            }))
        });
        faux_distant(app).await
    }

    /// L'adresse de flux d'une station Radio France, servie par le faux flux.
    ///
    /// Le radical est celui que Radio France emploie vraiment — c'est lui que
    /// lit `radiofrance_channel_id` — et l'hôte est local, pour qu'aucun test
    /// n'appelle une vraie radio.
    fn adresse_locale(flux: &str, radical: &str) -> String {
        format!("{flux}/radiofrance/{radical}-hifi.aac")
    }

    /// Les deux racines Radio France d'un test, toutes les deux locales.
    ///
    /// Passer les deux ensemble est ce qui garantit qu'aucun chemin de la
    /// famille — ni `pull`, ni `live` — ne puisse s'échapper vers la vraie API.
    fn voies<'a>(pull: &'a str, live: &'a str) -> VoiesRadioFrance<'a> {
        VoiesRadioFrance { pull, live }
    }

    /// Faux `livemeta/live`, **à la forme mesurée** le 01/09/2026 :
    /// `prev` / `now` / `next` côte à côte, le titre en `firstLine`,
    /// l'interprète en `secondLine`, le genre en `thirdLine`, un `songUuid` et
    /// une fenêtre `startTime` / `endTime` en secondes epoch.
    ///
    /// `decalage_s` déplace cette fenêtre par rapport à l'instant présent : `0`
    /// pour un morceau qui joue vraiment, une grande valeur négative pour la
    /// réponse périmée que la vraie API a servie.
    ///
    /// Comme la vraie route, il rend `400` sur un préréglage qu'il ne connaît
    /// pas, et chaque identifiant rend un morceau **qui le nomme** : deux
    /// stations ne peuvent pas rendre le même titre par accident.
    async fn faux_live(decalage_s: i64) -> String {
        faux_live_avec_preregage(decalage_s, RADIOFRANCE_LIVE_PRESET).await
    }

    /// Le même faux service, mais qui n'accepte que `preregage_accepte`.
    ///
    /// Servir un autre préréglage que celui du code, c'est jouer la refonte du
    /// lecteur du site : l'identifiant interne a changé, la route rend `400`.
    async fn faux_live_avec_preregage(decalage_s: i64, preregage_accepte: &str) -> String {
        let attendu = preregage_accepte.to_string();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let attendu = attendu.clone();
            async move {
                use axum::response::IntoResponse as _;
                let chemin = uri.path().trim_start_matches('/').to_string();
                let mut morceaux = chemin.split('/');
                let id = morceaux.next().unwrap_or_default().to_string();
                let preregage = morceaux.next().unwrap_or_default();
                if preregage != attendu {
                    // Le corps du vrai 400, à la forme mesurée le 01/09/2026 :
                    // un objet JSON qui liste les préréglages valides. Il ne
                    // porte donc **pas** de `now` — l'absence est tenue deux
                    // fois, par le statut puis par l'analyseur.
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({
                            "errCode": "e120",
                            "errMessage": "Expected rule to match 'apprf_bleu_display|…'",
                        })),
                    )
                        .into_response();
                }
                let t = maintenant_epoch_s().expect("horloge après 1970");
                axum::Json(json!({
                    "prev": [carte_de_station("Webradio")],
                    "now": {
                        "firstLine": format!("Morceau de la webradio {id}"),
                        "firstLineSongUuid": format!("uuid-{id}"),
                        "secondLine": format!("Interprète de la webradio {id}"),
                        "secondLineSongUuid": format!("uuid-{id}"),
                        "thirdLine": "Pop",
                        "songUuid": format!("uuid-{id}"),
                        // Un UUID nu, comme le vrai service : pas une adresse.
                        "cover": "00000000-0000-0000-0000-000000000000",
                        "startTime": t - 90 + decalage_s,
                        "endTime": t + 90 + decalage_s,
                    },
                    "next": [],
                    "delayToRefresh": 60000,
                }))
                .into_response()
            }
        });
        faux_distant(app).await
    }

    /// La **carte de la station** — ce que `live` met dans `now` quand aucun
    /// morceau ne joue. Relevé tel quel sur `live/7` le 01/09/2026 : les trois
    /// champs qui identifient un morceau sont `null`.
    fn carte_de_station(genre: &str) -> Value {
        json!({
            "firstLine": "Le direct",
            "secondLine": "La radio la plus éclectique du monde",
            "thirdLine": genre,
            "songUuid": Value::Null,
            "cover": "34e98566-0000-0000-0000-000000000000",
            "startTime": Value::Null,
            "endTime": Value::Null,
        })
    }

    /// **LE FAIT.** Ce que la route rend pour chacune des onze stations.
    ///
    /// Rouge avant le correctif : les onze rendaient `Morceau du canal 4`,
    /// `… 6` ou `… 1`, c'est-à-dire le morceau d'une autre station. Vert
    /// après : elles ne rendent plus rien.
    #[tokio::test]
    async fn aucune_des_onze_stations_ne_rend_le_morceau_d_une_autre() {
        let livemeta = faux_livemeta().await;
        let live = faux_live(0).await;
        let flux = faux_flux(false).await;
        let voies = voies(&livemeta, &live);

        // Ce que rendent, au même instant, les trois stations mères.
        let mut meres = Vec::new();
        for (nom, radical) in [
            ("France Musique", "francemusique"),
            ("Mouv'", "mouv"),
            ("France Inter", "franceinter"),
        ] {
            let titre = fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical))
                .await
                .unwrap_or_else(|| panic!("« {nom} » doit continuer de rendre son morceau"))
                .title;
            meres.push(titre);
        }

        // Toutes les onze sont interrogées avant de conclure : un `assert` par
        // tour s'arrêterait à la première et ne dirait rien des dix autres.
        let mut fautives = Vec::new();
        for &(nom, radical) in ONZE_STATIONS_SANS_SOURCE {
            if let Some(meta) =
                fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical)).await
            {
                fautives.push(format!("{nom} → « {} »", meta.title));
            }
        }
        assert!(
            fautives.is_empty(),
            "aucune des onze ne doit RIEN afficher — ni canal `pull`, ni identifiant `live`, et \
             leurs flux ne portent pas d'icy-metaint. {} station(s) affichent pourtant un \
             morceau : {fautives:?}, quand les stations mères rendent {meres:?} au même instant.",
            fautives.len()
        );
    }

    /// **Témoin.** France Musique principale continue de rendre SON morceau.
    /// Si le correctif éteignait la mère avec ses filles, il serait faux.
    #[tokio::test]
    async fn temoin_france_musique_principale_rend_toujours_son_morceau() {
        let livemeta = faux_livemeta().await;
        let live = faux_live(0).await;
        let flux = faux_flux(false).await;
        let meta = fetch_radio_metadata_depuis(
            voies(&livemeta, &live),
            "France Musique",
            &adresse_locale(&flux, "francemusique"),
        )
        .await
        .expect("France Musique principale doit rendre son morceau");
        assert_eq!(meta.title, "Morceau du canal 4");
        assert_eq!(meta.artist.as_deref(), Some("Interprète du canal 4"));
        assert_eq!(meta.station.as_deref(), Some("France Musique"));
    }

    /// **Témoin.** Une station couverte par le repli ICY rend bien son titre :
    /// le chemin qui sert toutes les stations non câblées est intact.
    #[tokio::test]
    async fn temoin_une_station_couverte_par_le_repli_icy_rend_son_titre() {
        let livemeta = faux_livemeta().await;
        let live = faux_live(0).await;
        let flux = faux_flux(true).await;
        let meta = fetch_radio_metadata_depuis(
            voies(&livemeta, &live),
            "Radio Machin",
            &format!("{flux}/machin.mp3"),
        )
        .await
        .expect("le repli ICY doit lire le bloc du flux");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist.as_deref(), Some("A"));
    }

    /// **Témoin.** Le catalogue Radio France livré, station par station : le
    /// correctif ne doit éteindre QUE les onze. Toute autre association reste
    /// exactement celle d'avant.
    #[test]
    fn temoin_le_catalogue_radio_france_livre_garde_ses_canaux() {
        let url = |s: &str| format!("https://icecast.radiofrance.fr/{s}-hifi.aac");
        for (nom, radical, attendu) in [
            // Les stations principales.
            ("FIP", "fip", Some(7)),
            ("France Musique", "francemusique", Some(4)),
            ("France Culture", "franceculture", Some(2)),
            ("France Inter", "franceinter", Some(1)),
            ("Mouv'", "mouv", Some(6)),
            // Les webradios FIP qui ont un vrai canal, mesuré.
            ("FIP Rock", "fiprock", Some(64)),
            ("FIP Jazz", "fipjazz", Some(65)),
            ("FIP Groove", "fipgroove", Some(66)),
            ("FIP Monde", "fipworld", Some(69)),
            ("FIP Nouveautés", "fipnouveautes", Some(70)),
            ("FIP Reggae", "fipreggae", Some(71)),
            ("FIP Electro", "fipelectro", Some(74)),
            ("FIP Metal", "fipmetal", Some(77)),
            // Les trois webradios FIP sans canal `pull`. Elles n'en gagnent
            // toujours pas : #3149 les sert par l'autre voie, `live`, et c'est
            // justement parce que cette correspondance-ci rend `None` que
            // l'appelant y arrive.
            ("FIP Pop", "fippop", None),
            ("FIP Hip-Hop", "fiphiphop", None),
            ("FIP Sacré français", "fipsacrefrancais", None),
        ] {
            assert_eq!(
                radiofrance_channel_id(nom, &url(radical)),
                attendu,
                "« {nom} » doit garder l'association qu'elle avait"
            );
        }
    }

    /// Le même verdict au niveau de la correspondance, station par station :
    /// aucune des onze ne reçoit de canal.
    #[test]
    fn les_onze_stations_ne_recoivent_aucun_canal() {
        let mut fautives = Vec::new();
        for &(nom, radical) in ONZE_STATIONS_SANS_SOURCE {
            let url = format!("https://icecast.radiofrance.fr/{radical}-hifi.aac");
            if let Some(canal) = radiofrance_channel_id(nom, &url) {
                fautives.push(format!("{nom} → canal {canal}"));
            }
            // Le nom seul suffit aussi — les stations ajoutées à la main ne
            // portent pas toujours une URL reconnaissable.
            if let Some(canal) = radiofrance_channel_id(nom, "") {
                fautives.push(format!("{nom} (nom seul) → canal {canal}"));
            }
        }
        assert!(
            fautives.is_empty(),
            "aucune des onze ne doit recevoir de canal, aucun ne les couvre : {fautives:?}"
        );
    }

    // -----------------------------------------------------------------------
    // #3149 — trois webradios FIP éteintes, qu'une AUTRE voie de la même API
    //         couvre
    // -----------------------------------------------------------------------
    //
    // ## Le fait de base
    //
    // FIP Pop, FIP Hip-Hop et FIP Sacré français n'affichaient aucun morceau :
    // `livemeta/pull/78`, `/95` et `/96` répondent **404** et leurs flux AAC ne
    // portent pas d'`icy-metaint`. Mesuré le 01/09/2026,
    // `livemeta/live/{id}/{preset}` — publique, sans clef — rend le morceau en
    // cours des trois pour ces mêmes numéros.
    //
    // ## La forme de `live`, qui n'est pas celle de `pull`
    //
    // `prev` / `now` / `next` côte à côte plus un `delayToRefresh`, le titre en
    // `firstLine`, l'interprète en `secondLine`, une fenêtre `startTime` /
    // `endTime` en secondes epoch, et une `cover` qui n'est **pas** une adresse
    // mais un UUID nu.
    //
    // ## Les deux façons dont elle ne décrit PAS l'instant présent
    //
    // 1. **Rien ne joue** : `now` reste complet mais porte la carte de la
    //    station — `firstLine` = `"Le direct"`, `songUuid`, `startTime` et
    //    `endTime` tous les trois `null`. Relevé sur `live/7`.
    // 2. **La réponse est périmée** : `now` est un vrai morceau, mais sa
    //    fenêtre est passée. Relevé à **2 131 s** — trente-cinq minutes — avant
    //    que la requête suivante redevienne fraîche.
    //
    // C'est le piège de la BBC (#2486), sous deux visages. La garde tient les
    // deux : un `songUuid`, et l'instant présent dans la fenêtre.
    //
    // Aucun test ci-dessous n'appelle une vraie radio, et aucun ne porte de
    // clef : la voie `live` est publique, et son distant est monté dans le test.

    /// Les trois du ticket : le nom livré, le radical du flux, l'identifiant.
    const TROIS_WEBRADIOS_FIP_PAR_LIVE: &[(&str, &str, u32)] = &[
        ("FIP Pop", "fippop", 78),
        ("FIP Hip-Hop", "fiphiphop", 95),
        ("FIP Sacré français", "fipsacrefrancais", 96),
    ];

    /// **LE FAIT.** Le titre et l'interprète rendus pour chacune des trois.
    ///
    /// Rouge avant le correctif : les trois ne rendaient **rien** — pas de
    /// canal `pull`, pas d'`icy-metaint` sur le flux. Vert après : chacune rend
    /// le morceau de SON identifiant, jamais celui d'une autre.
    #[tokio::test]
    async fn les_trois_webradios_fip_rendent_leur_morceau_en_cours() {
        let livemeta = faux_livemeta().await;
        let live = faux_live(0).await;
        let flux = faux_flux(false).await;
        let voies = voies(&livemeta, &live);

        // Les trois sont interrogées avant de conclure : un `assert` par tour
        // s'arrêterait à la première et ne dirait rien des deux autres.
        let mut rendus = Vec::new();
        for &(nom, radical, id) in TROIS_WEBRADIOS_FIP_PAR_LIVE {
            let meta = fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical))
                .await
                .unwrap_or_else(|| panic!("« {nom} » doit rendre le morceau de la voie `live`"));
            rendus.push((
                nom,
                meta.title.clone(),
                meta.artist.clone(),
                meta.cover_url.clone(),
            ));
            assert_eq!(
                meta.title,
                format!("Morceau de la webradio {id}"),
                "« {nom} » doit rendre le morceau de l'identifiant {id}, pas d'un autre"
            );
            assert_eq!(
                meta.artist.as_deref(),
                Some(format!("Interprète de la webradio {id}").as_str()),
                "« {nom} » doit rendre l'interprète de l'identifiant {id}"
            );
            assert_eq!(meta.station.as_deref(), Some(nom));
            // La `cover` de `live` est un UUID nu, pas une adresse : aucune
            // pochette ne part à l'écran, et c'est voulu.
            assert_eq!(meta.cover_url, None, "un UUID nu n'est pas une pochette");
        }

        // Trois titres distincts : aucune ne montre le morceau d'une autre.
        let titres: std::collections::BTreeSet<_> =
            rendus.iter().map(|(_, titre, _, _)| titre).collect();
        assert_eq!(titres.len(), 3, "les trois doivent différer : {rendus:?}");
    }

    /// **La garde, visage 1 — rien ne joue.** La carte de station relevée sur
    /// `live/7` rend une **absence**, pas « Le direct » dans la case du titre.
    #[test]
    fn une_reponse_sans_morceau_en_cours_rend_une_absence() {
        let corps = json!({
            "prev": [carte_de_station("FIP")],
            "now": carte_de_station("FIP"),
            "next": [],
            "delayToRefresh": 10000,
        });
        assert!(
            lire_morceau_en_cours_radiofrance_live(&corps, "FIP Pop", 1_788_290_585).is_none(),
            "sans `songUuid`, `now` porte la carte de la station et non un morceau"
        );
    }

    /// **`songUuid` n'est pas un ornement.** Sur la carte relevée, les trois
    /// champs sont `null` à la fois : le test ci-dessus tiendrait encore si
    /// seule la fenêtre était contrôlée. Celui-ci isole la condition — une
    /// fenêtre parfaitement valable, mais rien qui nomme un morceau — pour que
    /// la garde ne repose pas par accident sur une autre.
    #[test]
    fn un_now_sans_uuid_de_morceau_ne_rend_rien_meme_avec_une_fenetre_valable() {
        let t = 1_788_290_585;
        for sans_morceau in [Value::Null, json!(""), json!("   ")] {
            let corps = json!({
                "now": {
                    "firstLine": "Le direct",
                    "secondLine": "La radio la plus éclectique du monde",
                    "songUuid": sans_morceau,
                    "startTime": t - 90,
                    "endTime": t + 90,
                }
            });
            assert!(
                lire_morceau_en_cours_radiofrance_live(&corps, "FIP Pop", t).is_none(),
                "sans UUID de morceau, il n'y a pas de morceau à afficher : {corps}"
            );
        }
    }

    /// **La garde, visage 2 — la réponse est périmée.** Le même morceau, la
    /// même charge utile complète : affiché tant que sa fenêtre contient
    /// l'instant présent, tu dès qu'elle est derrière.
    ///
    /// Le retard éprouvé ici est celui **mesuré** le 01/09/2026 : 2 131 s.
    #[test]
    fn un_morceau_dont_la_fenetre_est_passee_ne_s_affiche_plus() {
        let t = 1_788_290_585;
        let morceau = |debut: i64, fin: i64| {
            json!({
                "now": {
                    "firstLine": "Rubber sky",
                    "secondLine": "Un interprète",
                    "songUuid": "55205f47-0000-0000-0000-000000000000",
                    "startTime": debut,
                    "endTime": fin,
                }
            })
        };

        // En cours : la fenêtre contient l'instant présent.
        let en_cours =
            lire_morceau_en_cours_radiofrance_live(&morceau(t - 90, t + 90), "FIP Pop", t)
                .expect("un morceau dont la fenêtre contient l'instant présent doit s'afficher");
        assert_eq!(en_cours.title, "Rubber sky");

        // Le relais normal entre deux morceaux : `now` garde le morceau qui
        // vient de finir jusqu'à ce que le suivant soit annoncé — relevé à
        // 21 s au plus. Il doit encore s'afficher.
        assert!(
            lire_morceau_en_cours_radiofrance_live(&morceau(t - 300, t - 21), "FIP Pop", t)
                .is_some(),
            "un relais de 21 s est le fonctionnement normal, pas une réponse périmée"
        );

        // La réponse périmée mesurée : 2 131 s de retard.
        assert!(
            lire_morceau_en_cours_radiofrance_live(&morceau(t - 2_400, t - 2_131), "FIP Pop", t)
                .is_none(),
            "un morceau terminé depuis 35 minutes ne doit plus s'afficher"
        );

        // Et l'autre bord : un morceau annoncé pour bien plus tard n'est pas
        // celui qui joue.
        assert!(
            lire_morceau_en_cours_radiofrance_live(&morceau(t + 600, t + 900), "FIP Pop", t)
                .is_none(),
            "un morceau qui n'a pas commencé ne joue pas"
        );
    }

    /// **La garde, bout à bout.** Le faux service rend une charge utile pleine
    /// et plausible, mais dont la fenêtre est passée de 2 131 s : la route
    /// entière rend une absence, pas le dernier morceau connu.
    #[tokio::test]
    async fn une_reponse_perimee_rend_une_absence_bout_a_bout() {
        let livemeta = faux_livemeta().await;
        // `-2_221` place `endTime` à `maintenant - 2_131`.
        let live = faux_live(-2_221).await;
        let flux = faux_flux(false).await;
        let voies = voies(&livemeta, &live);

        let mut fautives = Vec::new();
        for &(nom, radical, _) in TROIS_WEBRADIOS_FIP_PAR_LIVE {
            if let Some(meta) =
                fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical)).await
            {
                fautives.push(format!("{nom} → « {} »", meta.title));
            }
        }
        assert!(
            fautives.is_empty(),
            "une réponse périmée ne doit rien afficher : {fautives:?}"
        );
    }

    /// **Le préréglage n'est pas un contrat.** Quand le lecteur du site change
    /// son identifiant interne, la route rend `400` : les trois stations
    /// retournent à l'absence propre d'où elles viennent, sans titre faux ni
    /// panique.
    #[tokio::test]
    async fn un_preregage_devenu_inconnu_rend_une_absence() {
        let livemeta = faux_livemeta().await;
        let live = faux_live_avec_preregage(0, "webrf_un_autre_nom_apres_refonte").await;
        let flux = faux_flux(false).await;
        let voies = voies(&livemeta, &live);

        let mut fautives = Vec::new();
        for &(nom, radical, _) in TROIS_WEBRADIOS_FIP_PAR_LIVE {
            if let Some(meta) =
                fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical)).await
            {
                fautives.push(format!("{nom} → « {} »", meta.title));
            }
        }
        assert!(
            fautives.is_empty(),
            "un 400 sur le préréglage doit rendre une absence : {fautives:?}"
        );
    }

    /// **Témoin.** Seules les trois ont un identifiant `live` — par le nom seul
    /// comme par l'URL du flux, car les stations ajoutées à la main ne portent
    /// pas toujours une URL reconnaissable.
    #[test]
    fn temoin_seules_les_trois_webradios_fip_ont_un_identifiant_live() {
        let url = |s: &str| format!("https://icecast.radiofrance.fr/{s}-hifi.aac");

        for &(nom, radical, id) in TROIS_WEBRADIOS_FIP_PAR_LIVE {
            assert_eq!(
                radiofrance_live_id(nom, &url(radical)),
                Some(id),
                "« {nom} » doit recevoir l'identifiant {id}"
            );
            assert_eq!(
                radiofrance_live_id(nom, ""),
                Some(id),
                "« {nom} » (nom seul) doit recevoir l'identifiant {id}"
            );
            assert_eq!(
                radiofrance_live_id("", &url(radical)),
                Some(id),
                "« {radical} » (URL seule) doit recevoir l'identifiant {id}"
            );
        }

        // Personne d'autre. Les huit webradios FIP à canal `pull`, FIP
        // principale, les onze de #3142, et une station qui n'est pas FIP du
        // tout : aucune ne doit passer par la voie `live`.
        let mut fautives = Vec::new();
        let autres = [
            ("FIP", "fip"),
            ("FIP Rock", "fiprock"),
            ("FIP Jazz", "fipjazz"),
            ("FIP Groove", "fipgroove"),
            ("FIP Monde", "fipworld"),
            ("FIP Nouveautés", "fipnouveautes"),
            ("FIP Reggae", "fipreggae"),
            ("FIP Electro", "fipelectro"),
            ("FIP Metal", "fipmetal"),
            ("France Musique", "francemusique"),
            ("France Inter", "franceinter"),
            ("France Culture", "franceculture"),
            ("Mouv'", "mouv"),
        ];
        for (nom, radical) in autres
            .iter()
            .copied()
            .chain(ONZE_STATIONS_SANS_SOURCE.iter().copied())
        {
            if let Some(id) = radiofrance_live_id(nom, &url(radical)) {
                fautives.push(format!("{nom} → live/{id}"));
            }
            if let Some(id) = radiofrance_live_id(nom, "") {
                fautives.push(format!("{nom} (nom seul) → live/{id}"));
            }
        }
        assert!(
            fautives.is_empty(),
            "seules les trois webradios du ticket ont un identifiant `live` : {fautives:?}"
        );
    }

    /// **Témoin.** FIP principale et les webradios FIP qui ont un canal
    /// continuent de passer par `pull`, et rendent le morceau de LEUR canal.
    /// Le faux `live` est monté au même instant : s'il les captait, le titre
    /// rendu le dirait.
    #[tokio::test]
    async fn temoin_les_webradios_fip_a_canal_passent_toujours_par_pull() {
        let livemeta = faux_livemeta().await;
        let live = faux_live(0).await;
        let flux = faux_flux(false).await;
        let voies = voies(&livemeta, &live);

        for (nom, radical, canal) in [
            ("FIP", "fip", 7),
            ("FIP Rock", "fiprock", 64),
            ("FIP Jazz", "fipjazz", 65),
            ("FIP Metal", "fipmetal", 77),
        ] {
            let meta = fetch_radio_metadata_depuis(voies, nom, &adresse_locale(&flux, radical))
                .await
                .unwrap_or_else(|| panic!("« {nom} » doit continuer de rendre son morceau"));
            assert_eq!(
                meta.title,
                format!("Morceau du canal {canal}"),
                "« {nom} » doit rester sur son canal `pull` {canal}"
            );
        }
    }

    // --- KEXP (#2486) ---

    /// Faux service KEXP qui rend `corps` sur la route des passages.
    async fn faux_kexp(corps: Value) -> String {
        let app = axum::Router::new().route(
            "/v2/plays/",
            axum::routing::get(move || {
                let c = corps.clone();
                async move { axum::Json(c) }
            }),
        );
        faux_distant(app).await
    }

    /// Un passage d'antenne KEXP, à la forme mesurée le 19/09/2026.
    fn passage_kexp(
        play_type: &str,
        artist: Option<&str>,
        song: Option<&str>,
        thumb: &str,
    ) -> Value {
        json!({
            "id": 3709218,
            "airdate": "2026-09-19T03:49:38-07:00",
            "show": 67834,
            "image_uri": thumb.replace("_thumb250", "_thumb500"),
            "thumbnail_uri": thumb,
            "song": song,
            "artist": artist,
            "album": "Premiers Symptômes",
            "release_date": "1997-07-29",
            "play_type": play_type,
        })
    }

    const MINIATURE_KEXP: &str = "https://dn711001.ca.archive.org/0/items/mbid-a6e48744-b193-4d84-b8a5-4f5d7627e4cf/mbid-a6e48744-b193-4d84-b8a5-4f5d7627e4cf-22471663759_thumb250.jpg";

    /// **Le vert.** La charge utile relevée le 19/09/2026 à 10:53 UTC sur
    /// `api.kexp.org/v2/plays/?limit=1` : Air, « Modular Mix », miniature
    /// `archive.org`. Titre, artiste et pochette sortent tels quels.
    #[tokio::test]
    async fn une_station_kexp_rend_le_morceau_en_cours_et_sa_pochette() {
        let corps = json!({ "next": null, "previous": null, "results": [
            passage_kexp("trackplay", Some("Air"), Some("Modular Mix"), MINIATURE_KEXP),
        ]});
        let base = faux_kexp(corps).await;
        let meta = fetch_kexp_metadata(&base, "KEXP")
            .await
            .expect("le morceau en cours doit sortir du service");

        assert_eq!(meta.title, "Modular Mix");
        assert_eq!(meta.artist.as_deref(), Some("Air"));
        assert_eq!(meta.station.as_deref(), Some("KEXP"));
        assert_eq!(meta.cover_url.as_deref(), Some(MINIATURE_KEXP));
    }

    /// **L'absence propre.** Mesuré le 19/09/2026 : sur huit passages, un
    /// `airbreak` — `song`, `artist` et `thumbnail_uri` absents ou vides. Le
    /// morceau d'AVANT la pause ne doit pas ressusciter à l'écran.
    #[tokio::test]
    async fn une_pause_antenne_kexp_ne_ressuscite_pas_le_morceau_precedent() {
        let corps = json!({ "next": null, "previous": null, "results": [
            { "id": 3709215, "airdate": "2026-09-19T03:37:19-07:00", "show": 67834,
              "image_uri": "", "thumbnail_uri": "", "comment": "", "play_type": "airbreak" },
            passage_kexp("trackplay", Some("Craft Spells"), Some("Breaking the Angle Against the Tide"), MINIATURE_KEXP),
        ]});
        let base = faux_kexp(corps).await;
        assert!(
            fetch_kexp_metadata(&base, "KEXP").await.is_none(),
            "pendant une pause antenne, rien ne joue : rien ne sort"
        );
    }

    /// Une miniature VIDE — « Live on KEXP », mesuré — ne devient pas une
    /// pochette vide ; le titre et l'artiste sortent quand même.
    #[test]
    fn une_miniature_kexp_vide_laisse_le_logo_de_la_station() {
        let corps = json!({ "results": [
            passage_kexp("trackplay", Some("Ladytron"), Some("Destroy Everything You Touch"), ""),
        ]});
        let meta = lire_now_playing_kexp(&corps, "KEXP").expect("un morceau joue");
        assert_eq!(meta.title, "Destroy Everything You Touch");
        assert_eq!(meta.artist.as_deref(), Some("Ladytron"));
        assert!(meta.cover_url.is_none());
    }

    /// KEXP se reconnaît à l'hôte — le relais `streamguys1.com` du catalogue
    /// comme le domaine propre — ou au nom pris en jeton entier. Rien d'autre.
    #[test]
    fn kexp_se_reconnait_a_l_hote_ou_au_nom_et_a_rien_d_autre() {
        assert!(kexp_reconnue(
            "KEXP",
            "https://kexp-mp3-128.streamguys1.com/kexp128.mp3"
        ));
        assert!(kexp_reconnue(
            "Radio",
            "https://live-mp3-128.kexp.org/kexp128.mp3"
        ));
        assert!(kexp_reconnue(
            "KEXP 90.3 FM",
            "http://exemple.invalid/flux.mp3"
        ));
        assert!(kexp_reconnue(
            "kexp seattle",
            "http://exemple.invalid/flux.mp3"
        ));

        assert!(!kexp_reconnue(
            "Kexpo Radio",
            "http://exemple.invalid/flux.mp3"
        ));
        assert!(!kexp_reconnue(
            "Radio",
            "http://exemple.invalid/kexp128.mp3"
        ));
        assert!(!kexp_reconnue(
            "Radio Paradise",
            "http://stream.radioparadise.com/aac-128"
        ));
        assert!(!kexp_reconnue(
            "FIP",
            "https://icecast.radiofrance.fr/fip-hifi.aac"
        ));
        assert!(!kexp_reconnue(
            "BBC Radio 3",
            "http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three"
        ));
    }

    // --- AzuraCast (#2486) ---

    /// La charge utile relevée le 19/09/2026 à 10:56 UTC sur
    /// `online.jamminvibezradio.com/api/nowplaying/reggae`, réduite aux champs
    /// que Tune lit — et à `text`, pour établir qu'il n'est PAS repris.
    fn charge_azuracast(art: &str) -> Value {
        json!({
            "station": { "id": 1, "name": "Reggae Classics Mix", "shortcode": "reggae" },
            "live": { "is_live": false, "streamer_name": "", "art": null },
            "now_playing": {
                "sh_id": 49352, "played_at": 1789815337, "duration": 281, "elapsed": 13,
                "song": {
                    "id": "bbf3f53c35284ab0c3abc1072167753a",
                    "art": art,
                    "text": "Joshua Kesler - Rock I Stand - Rock I Stand",
                    "artist": "Joshua Kesler",
                    "title": "Rock I Stand",
                    "album": "Rock I Stand",
                }
            }
        })
    }

    const POCHETTE_AZURACAST: &str = "https://online.jamminvibezradio.com/api/station/reggae/art/572f8e5324df3d43645624e4-1786762267.jpg";

    /// Un faux hôte AzuraCast : l'API sur `/api/nowplaying/<code>`, et un flux
    /// ICY partout ailleurs, qui dit « A - B ».
    async fn faux_azuracast(code: &str, corps: Value) -> String {
        let app = routeur_faux_flux(true).route(
            &format!("/api/nowplaying/{code}"),
            axum::routing::get(move || {
                let c = corps.clone();
                async move { axum::Json(c) }
            }),
        );
        faux_distant(app).await
    }

    /// **Le témoin, par la porte publique.** Une station du catalogue —
    /// `…/listen/reggae/live.flac` — passe par l'API de son hôte, pas par le
    /// bloc ICY du flux : le titre est celui de l'API, pas « B ».
    ///
    /// Rouge avant ce correctif : rien ne reconnaissait la forme `/listen/`,
    /// l'écran affichait le « A - B » de l'ICY, sans pochette.
    #[tokio::test]
    async fn une_station_azuracast_est_servie_par_l_api_de_son_hote() {
        let base = faux_azuracast("reggae", charge_azuracast(POCHETTE_AZURACAST)).await;
        let meta = fetch_radio_metadata(
            "Reggae Classic Mix",
            &format!("{base}/listen/reggae/live.flac"),
        )
        .await
        .expect("le morceau en cours doit sortir de l'API");

        assert_eq!(meta.title, "Rock I Stand");
        assert_eq!(meta.artist.as_deref(), Some("Joshua Kesler"));
        assert_eq!(meta.station.as_deref(), Some("Reggae Classic Mix"));
        assert_eq!(meta.cover_url.as_deref(), Some(POCHETTE_AZURACAST));
    }

    /// **La forme peut mentir.** Un `/listen/x/y` dont l'hôte répond 404 à
    /// l'API n'est pas un AzuraCast : on lit l'ICY du flux, exactement comme
    /// avant. Aucune absence n'est fabriquée.
    #[tokio::test]
    async fn un_listen_qui_n_est_pas_un_azuracast_laisse_la_main_au_repli_icy() {
        let base = faux_distant(routeur_faux_flux(true)).await; // pas d'API
        let meta = fetch_radio_metadata("Faux Flux", &format!("{base}/listen/foo/bar.mp3"))
            .await
            .expect("le repli ICY sert toujours");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist.as_deref(), Some("A"));
    }

    /// L'adresse de l'API se déduit de l'URL de flux — les trois stations du
    /// catalogue livré (19/09/2026) — et de rien d'autre.
    #[test]
    fn l_url_de_flux_azuracast_donne_l_adresse_du_now_playing() {
        for (flux, api) in [
            (
                "https://online.jamminvibezradio.com/listen/reggae/live.flac",
                "https://online.jamminvibezradio.com/api/nowplaying/reggae",
            ),
            (
                "https://online.jamminvibezradio.com/listen/oldies/live.flac",
                "https://online.jamminvibezradio.com/api/nowplaying/oldies",
            ),
            (
                "https://online.jamminvibezradio.com/listen/caribbean/live.flac",
                "https://online.jamminvibezradio.com/api/nowplaying/caribbean",
            ),
            (
                "https://online.jamminvibezradio.com/hls/reggae/live.m3u8",
                "https://online.jamminvibezradio.com/api/nowplaying/reggae",
            ),
            (
                "http://radio.exemple.invalid:8000/listen/ma_radio/stream.aac?x=1",
                "http://radio.exemple.invalid:8000/api/nowplaying/ma_radio",
            ),
        ] {
            assert_eq!(
                azuracast_now_playing_url(flux).as_deref(),
                Some(api),
                "{flux}"
            );
        }

        for flux in [
            "https://icecast.radiofrance.fr/fip-hifi.aac",
            "http://stream.radioparadise.com/aac-128",
            "https://kexp-mp3-128.streamguys1.com/kexp128.mp3",
            "http://exemple.invalid/listen/flux.mp3",
            "http://exemple.invalid/listen//flux.mp3",
            "http://exemple.invalid/listen/a b/flux.mp3",
            "http://exemple.invalid/listen/reggae/",
            "rtsp://exemple.invalid/listen/reggae/live",
            "https://mscp3.live-streams.nl:8340/jazz-high.aac",
        ] {
            assert_eq!(azuracast_now_playing_url(flux), None, "{flux}");
        }
    }

    /// L'image GÉNÉRIQUE du logiciel n'est pas une pochette : le logo de la
    /// station — juste — reste à l'écran.
    #[test]
    fn une_image_generique_azuracast_n_est_pas_une_pochette() {
        let sans = lire_now_playing_azuracast(
            &charge_azuracast("https://online.jamminvibezradio.com/static/img/generic_song.jpg"),
            "Reggae Classic Mix",
        )
        .expect("le titre sort quand même");
        assert_eq!(sans.title, "Rock I Stand");
        assert!(sans.cover_url.is_none());

        let televersee = lire_now_playing_azuracast(
            &charge_azuracast(
                "https://online.jamminvibezradio.com/static/uploads/album_art.1610425816.png",
            ),
            "Reggae Classic Mix",
        )
        .expect("le titre sort quand même");
        assert!(televersee.cover_url.is_none());

        let avec =
            lire_now_playing_azuracast(&charge_azuracast(POCHETTE_AZURACAST), "Reggae Classic Mix")
                .expect("le titre sort");
        assert_eq!(avec.cover_url.as_deref(), Some(POCHETTE_AZURACAST));
    }

    /// Sans `title`, rien ne sort — `text` n'est pas un titre de rechange.
    #[test]
    fn un_azuracast_sans_titre_ne_promeut_pas_le_champ_text() {
        let mut corps = charge_azuracast(POCHETTE_AZURACAST);
        corps["now_playing"]["song"]["title"] = json!("");
        assert!(lire_now_playing_azuracast(&corps, "Reggae Classic Mix").is_none());
    }

    // -----------------------------------------------------------------------
    // La sixième famille : Icecast générique (#2486)
    // -----------------------------------------------------------------------
    //
    // ## Le fait de base, mesuré le 23/09/2026
    //
    // Neuf stations du catalogue livré (`GET https://mozaiklabs.fr/api/v1/radios`,
    // 59 stations) sont servies par un Icecast que Tune ne reconnaissait pas.
    // Une réclamation `Icy-MetaData: 1` sur leur flux donne ceci :
    //
    // | station (montage du catalogue)         | type          | `icy-metaint` |
    // |----------------------------------------|---------------|---------------|
    // | naim JAZZ `:8340/jazz-high.aac`        | audio/aac     | 16000         |
    // | Linn Jazz `:8003/autodj`               | audio/mpeg    | 16000         |
    // | Linn Classical `:8004/autodj`          | audio/mpeg    | 16000         |
    // | Morow `:8080/morow_hi.aacp`            | audio/aacp    | 16000         |
    // | BluesWave `:8050/FlacBlues`            | audio/ogg     | **absent**    |
    // | PureClassic `:8140/flac.ogg`           | audio/ogg     | **absent**    |
    // | Radio Calico `/calico`                 | application/ogg | **absent**  |
    // | JB Radio2 `:8001/flac`                 | application/ogg | **absent**  |
    // | Naim Classical `:8252/class-flac.flac` | (302)         | **absent**    |
    //
    // Icecast ne transporte de bloc ICY que sur les montages MP3/AAC.
    // `fetch_icy_metadata` s'arrête donc sur son `?` pour les CINQ montages sans
    // perte — l'écran ne montre RIEN. Et ce sont précisément ceux que le public
    // de Tune écoute.
    //
    // `status-json.xsl`, la page d'état du même serveur, porte le morceau — sur
    // le montage compressé jumeau. D'où les deux passes de `lire_status_icecast`.
    //
    // Aucun test ci-dessous n'appelle une vraie radio.

    /// La page d'état relevée le 23/09/2026 à 11:57 UTC sur
    /// `https://stream.radio-calico.com/status-json.xsl`, réduite aux champs
    /// que Tune lit. Deux montages : le sans-perte écouté, sans titre, et son
    /// jumeau MP3 qui le porte.
    ///
    /// Noter le `listenurl` : `http://…:8080/calico` quand le catalogue écoute
    /// `https://…/calico`. Autre schéma, autre port.
    fn charge_calico() -> Value {
        json!({ "icestats": { "source": [
            {
                "listenurl": "http://stream.radio-calico.com:8080/calico",
                "server_name": "Radio Calico - lossless",
                "server_type": "application/ogg",
            },
            {
                "listenurl": "http://stream.radio-calico.com:8080/calico.mp3",
                "server_name": "Radio Calico - 192kpbs",
                "server_type": "audio/mpeg",
                "artist": "Glass Animals",
                "title": "Heat Waves",
                "yp_currently_playing": "Glass Animals - Heat Waves",
            },
        ]}})
    }

    /// Un faux Icecast : la page d'état, et un flux SANS `icy-metaint` partout
    /// ailleurs — ce que rend un montage Ogg/FLAC réel.
    async fn faux_icecast(corps: Value) -> String {
        let app = routeur_faux_flux(false).route(
            "/status-json.xsl",
            axum::routing::get(move || {
                let c = corps.clone();
                async move { axum::Json(c) }
            }),
        );
        faux_distant(app).await
    }

    /// **Le témoin, par la porte publique.** Radio Calico, écoutée sur son
    /// montage sans perte, rend le morceau en cours.
    ///
    /// Rouge avant ce correctif : le montage `application/ogg` n'annonce aucun
    /// `icy-metaint`, `fetch_icy_metadata` rend `None`, et
    /// `fetch_radio_metadata` aussi — l'écran ne montre rien.
    #[tokio::test]
    async fn un_montage_sans_perte_rend_le_morceau_du_montage_jumeau() {
        let base = faux_icecast(charge_calico()).await;
        let meta = fetch_radio_metadata("Radio Calico", &format!("{base}/calico"))
            .await
            .expect("le morceau en cours doit sortir de la page d'etat Icecast");

        assert_eq!(meta.title, "Heat Waves");
        assert_eq!(meta.artist.as_deref(), Some("Glass Animals"));
        assert_eq!(meta.station.as_deref(), Some("Radio Calico"));
        // Une page d'etat Icecast ne publie aucune pochette : le logo de la
        // station reste a l'ecran.
        assert!(meta.cover_url.is_none());
    }

    /// **Le jumeau n'est pas n'importe quel voisin.** Sur
    /// `mscp4.live-streams.nl:8140`, le serveur porte DEUX programmes : Pure
    /// Classix (écouté en FLAC) et Pure Lounge. Prendre le titre du second
    /// afficherait un morceau qui ne passe pas à l'antenne.
    #[tokio::test]
    async fn un_autre_programme_du_meme_serveur_n_est_pas_un_jumeau() {
        // Relevé du 23/09/2026 : `flac.ogg` sans titre, `live.mp3` du même
        // programme, `lounge.mp3` d'un autre.
        let corps = json!({ "icestats": { "source": [
            { "listenurl": "http://mscp4.live-streams.nl:8140/flac.ogg",
              "server_name": "Pure Classix Radio" },
            { "listenurl": "http://mscp4.live-streams.nl:8140/lounge.mp3",
              "server_name": "Pure Lounge Radio",
              "title": "Monkey Safari - Gravity" },
            { "listenurl": "http://mscp4.live-streams.nl:8140/live.mp3",
              "server_name": "Pure Classix Radio MP3",
              "title": "Peter Gabriel - In Your Eyes" },
        ]}});
        let meta = lire_status_icecast(&corps, "PureClassic Radio", "flac.ogg")
            .expect("le jumeau du bon programme donne le titre");
        assert_eq!(meta.title, "In Your Eyes");
        assert_eq!(meta.artist.as_deref(), Some("Peter Gabriel"));

        // Sans le jumeau du bon programme, rien — surtout pas Pure Lounge.
        let sans_jumeau = json!({ "icestats": { "source": [
            { "listenurl": "http://mscp4.live-streams.nl:8140/flac.ogg",
              "server_name": "Pure Classix Radio" },
            { "listenurl": "http://mscp4.live-streams.nl:8140/lounge.mp3",
              "server_name": "Pure Lounge Radio",
              "title": "Monkey Safari - Gravity" },
        ]}});
        assert!(lire_status_icecast(&sans_jumeau, "PureClassic Radio", "flac.ogg").is_none());
    }

    /// `JB Radio2` : le montage `live` annonce `yp_currently_playing =
    /// "JB RADIO2 - "`. C'est le nom de la station, pas un morceau — et le
    /// montage `aac` du MÊME programme porte le vrai titre.
    #[test]
    fn un_nom_de_station_deguise_en_morceau_n_est_pas_un_titre() {
        let corps = json!({ "icestats": { "source": [
            { "listenurl": "http://mediacp.jb-radio.net:8001/flac",
              "server_name": "JB Radio" },
            { "listenurl": "http://mediacp.jb-radio.net:8001/live",
              "server_name": "JB RADIO2",
              "artist": "JB RADIO2",
              "yp_currently_playing": "JB RADIO2 - " },
            { "listenurl": "http://mediacp.jb-radio.net:8001/aac",
              "server_name": "JB RADIO",
              "title": "Zero 7 - In The Waiting Line" },
        ]}});
        let meta = lire_status_icecast(&corps, "JB Radio2", "flac").expect("le montage aac sert");
        assert_eq!(meta.title, "In The Waiting Line");
        assert_eq!(meta.artist.as_deref(), Some("Zero 7"));

        // `JB RADIO2` n'est pas le programme `JB RADIO` : le seul préfixe
        // littéral l'aurait admis, la frontière de mot l'écarte.
        assert!(!meme_programme("jb radio", "jb radio2"));
        assert!(meme_programme(
            "pure classix radio",
            "pure classix radio mp3"
        ));
        assert!(!meme_programme("pure classix radio", "pure lounge radio"));
    }

    /// Le montage écouté porte son propre titre : on ne va pas chercher de
    /// voisin, et « Artiste - Titre » se découpe.
    #[test]
    fn un_montage_qui_porte_son_titre_se_lit_seul() {
        // Linn Jazz, relevé du 23/09/2026 — `listenurl` sur
        // `radio.linnrecords.com`, quand le catalogue écoute `radio.linn.co.uk`.
        let corps = json!({ "icestats": { "source": [
            { "listenurl": "http://radio.linnrecords.com:8003/autodj",
              "server_name": "Linn Radio",
              "title": "Barb Jungr - Like A Rolling Stone" },
            { "listenurl": "http://radio.linnrecords.com:8003/live" },
        ]}});
        let meta = lire_status_icecast(&corps, "Linn Jazz", "autodj").expect("le titre est la");
        assert_eq!(meta.title, "Like A Rolling Stone");
        assert_eq!(meta.artist.as_deref(), Some("Barb Jungr"));
    }

    /// `icestats.source` est un OBJET NU quand le serveur ne porte qu'un
    /// montage. Ne lire que le tableau raterait tout serveur mono-montage.
    #[test]
    fn une_page_d_etat_a_un_seul_montage_se_lit_aussi() {
        let corps = json!({ "icestats": { "source":
            { "listenurl": "http://exemple.invalid:8000/flux",
              "server_name": "Solo",
              "title": "A - B" }
        }});
        let meta = lire_status_icecast(&corps, "Solo", "flux").expect("l'objet nu se lit");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist.as_deref(), Some("A"));
        assert_eq!(sources_icecast(&corps).len(), 1);
    }

    /// **La source muette ne fabrique aucune absence.** Un hôte sans page
    /// d'état laisse la main au repli ICY, exactement comme avant.
    #[tokio::test]
    async fn un_hote_sans_page_d_etat_laisse_la_main_au_repli_icy() {
        let base = faux_distant(routeur_faux_flux(true)).await; // pas de status-json.xsl
        let meta = fetch_radio_metadata("Faux Flux", &format!("{base}/flux.mp3"))
            .await
            .expect("le repli ICY sert toujours");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist.as_deref(), Some("A"));
    }

    /// Un montage absent de la page d'état ne prend le titre de personne.
    #[test]
    fn un_montage_absent_de_la_page_ne_prend_le_titre_de_personne() {
        assert!(lire_status_icecast(&charge_calico(), "Radio Calico", "autre").is_none());
        assert!(lire_status_icecast(&json!({}), "Radio Calico", "calico").is_none());
    }

    /// L'adresse de la page d'état et le montage se déduisent de l'URL du flux
    /// — les neuf stations du catalogue (23/09/2026) — et de rien d'autre.
    #[test]
    fn l_url_de_flux_icecast_donne_la_page_d_etat_et_le_montage() {
        for (flux, status, montage) in [
            (
                "https://stream.radio-calico.com/calico",
                "https://stream.radio-calico.com/status-json.xsl",
                "calico",
            ),
            (
                "http://radio.linn.co.uk:8003/autodj",
                "http://radio.linn.co.uk:8003/status-json.xsl",
                "autodj",
            ),
            (
                "http://radio.linn.co.uk:8004/autodj",
                "http://radio.linn.co.uk:8004/status-json.xsl",
                "autodj",
            ),
            (
                "http://blueswave.radio:8050/FlacBlues",
                "http://blueswave.radio:8050/status-json.xsl",
                "FlacBlues",
            ),
            (
                "https://mediacp.jb-radio.net:8001/flac",
                "https://mediacp.jb-radio.net:8001/status-json.xsl",
                "flac",
            ),
            (
                "http://mscp4.live-streams.nl:8140/flac.ogg",
                "http://mscp4.live-streams.nl:8140/status-json.xsl",
                "flac.ogg",
            ),
            (
                "https://mscp3.live-streams.nl:8252/class-flac.flac",
                "https://mscp3.live-streams.nl:8252/status-json.xsl",
                "class-flac.flac",
            ),
            (
                "http://mscp3.live-streams.nl:8340/jazz-high.aac",
                "http://mscp3.live-streams.nl:8340/status-json.xsl",
                "jazz-high.aac",
            ),
            (
                "http://stream.fr.morow.com:8080/morow_hi.aacp?x=1",
                "http://stream.fr.morow.com:8080/status-json.xsl",
                "morow_hi.aacp",
            ),
        ] {
            assert_eq!(icecast_status_url(flux).as_deref(), Some(status), "{flux}");
            assert_eq!(montage_icecast(flux), Some(montage), "{flux}");
        }

        for flux in [
            "rtsp://exemple.invalid/flux",
            "http:///flux",
            "http://exemple.invalid",
            "exemple.invalid/flux",
        ] {
            assert!(
                icecast_status_url(flux).is_none() || montage_icecast(flux).is_none(),
                "{flux}"
            );
        }
    }
}
