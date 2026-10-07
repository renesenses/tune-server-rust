//! YouTube Music : accueil, tendances et ambiances (#1897, #5247).
//!
//! Les routes `/streaming/youtube/home`, `/charts` et `/moods` étaient des
//! talons qui rendaient des listes vides accompagnées d'un message « not yet
//! implemented ». L'écran web « Découvrir » parlait donc dans le vide.
//!
//! La source est la même que celle de la recherche et de la navigation :
//! l'API interne de YouTube Music (InnerTube, client `WEB_REMIX`), SANS jeton.
//! Le jeton OAuth du flux TV empoisonne ces appels (HTTP 400, voir
//! `YouTubeService::ytm_post`). Quatre pages servent :
//!
//! | Page                       | `browseId`                           |
//! |----------------------------|--------------------------------------|
//! | accueil                    | `FEmusic_home`                       |
//! | tendances d'un pays        | `FEmusic_charts` + `formData`        |
//! | liste des ambiances/genres | `FEmusic_moods_and_genres`           |
//! | contenu d'une ambiance     | `FEmusic_moods_and_genres_category` + `params` |
//!
//! Ce module ne fait QUE lire les réponses : aucune entrée/sortie. Les
//! analyseurs sont essayés sur des réponses réelles enregistrées
//! (`tests/fixtures/youtube/`), sans réseau.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ce qu'un élément de rayon ouvre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeElement {
    /// Une playlist : `id` est le `browseId` (`VL…`) que
    /// `/streaming/youtube/playlists/{id}` et sa liste de titres savent lire.
    Playlist,
    /// Un album : `id` est le `browseId` (`MPRE…`).
    Album,
    /// Un artiste ou une chaîne : `id` est le `browseId` (`UC…`).
    Artist,
    /// Un titre : `id` est l'identifiant de la vidéo.
    Track,
}

/// Un élément d'un rayon : pochette, titre, sous-titre et ce qu'il ouvre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementRayon {
    pub kind: TypeElement,
    pub id: String,
    pub title: String,
    /// Artistes d'un titre ou d'un album, description d'une playlist ; peut
    /// être vide.
    pub subtitle: String,
    pub cover_path: Option<String>,
}

/// Un rayon (« shelf ») tel que YouTube Music le présente.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rayon {
    pub title: String,
    pub items: Vec<ElementRayon>,
}

/// Une ambiance ou un genre : `params` ouvre son contenu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ambiance {
    pub title: String,
    pub params: String,
}

/// Un groupe d'ambiances (« Moods & moments », « Genres »).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategorieAmbiances {
    pub title: String,
    pub items: Vec<Ambiance>,
}

/// En-tête d'une page de playlist.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntetePlaylist {
    pub title: Option<String>,
    pub description: Option<String>,
    pub cover_path: Option<String>,
}

fn texte(runs: &Value) -> String {
    runs["runs"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect())
        .unwrap_or_default()
}

fn derniere_vignette(thumbnails: &Value) -> Option<String> {
    thumbnails
        .as_array()
        .and_then(|a| a.last())
        .and_then(|t| t["url"].as_str())
        .filter(|u| !u.is_empty())
        .map(String::from)
}

/// Les sections d'une page de navigation, quelle que soit sa disposition.
fn sections(data: &Value) -> Vec<&Value> {
    let une_colonne = data["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
        .as_array()
        .and_then(|t| t.first())
        .and_then(|t| t["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array());
    let deux_colonnes = data["contents"]["twoColumnBrowseResultsRenderer"]["tabs"]
        .as_array()
        .and_then(|t| t.first())
        .and_then(|t| t["tabRenderer"]["content"]["sectionListRenderer"]["contents"].as_array());
    une_colonne
        .or(deux_colonnes)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

fn type_de_page(endpoint: &Value) -> Option<TypeElement> {
    let page = endpoint["browseEndpointContextSupportedConfigs"]
        ["browseEndpointContextMusicConfig"]["pageType"]
        .as_str()?;
    match page {
        "MUSIC_PAGE_TYPE_PLAYLIST" => Some(TypeElement::Playlist),
        "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => Some(TypeElement::Album),
        "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => Some(TypeElement::Artist),
        _ => None,
    }
}

/// `musicTwoRowItemRenderer` : une carte (playlist, album, artiste, vidéo).
fn element_deux_lignes(item: &Value) -> Option<ElementRayon> {
    let nav = &item["navigationEndpoint"];
    let (kind, id) = if let Some(video) = nav["watchEndpoint"]["videoId"].as_str() {
        (TypeElement::Track, video)
    } else {
        let browse = &nav["browseEndpoint"];
        (type_de_page(browse)?, browse["browseId"].as_str()?)
    };
    let title = texte(&item["title"]);
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(ElementRayon {
        kind,
        id: id.into(),
        title,
        subtitle: texte(&item["subtitle"]),
        cover_path: derniere_vignette(
            &item["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
        ),
    })
}

/// `musicResponsiveListItemRenderer` : une ligne (titre, ou artiste des
/// tendances).
fn element_ligne(item: &Value) -> Option<ElementRayon> {
    let colonne = |i: usize| {
        texte(&item["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"])
    };
    let (kind, id) = if let Some(video) = item["playlistItemData"]["videoId"].as_str() {
        (TypeElement::Track, video)
    } else {
        let browse = &item["navigationEndpoint"]["browseEndpoint"];
        (type_de_page(browse)?, browse["browseId"].as_str()?)
    };
    let title = colonne(0);
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(ElementRayon {
        kind,
        id: id.into(),
        title,
        subtitle: colonne(1),
        cover_path: derniere_vignette(
            &item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
        ),
    })
}

fn element(wrapper: &Value) -> Option<ElementRayon> {
    let mut e = if !wrapper["musicTwoRowItemRenderer"].is_null() {
        element_deux_lignes(&wrapper["musicTwoRowItemRenderer"])
    } else if !wrapper["musicResponsiveListItemRenderer"].is_null() {
        element_ligne(&wrapper["musicResponsiveListItemRenderer"])
    } else {
        None
    }?;
    // Le sous-titre d'un TITRE est « Artiste • 19M plays » : le client en fait
    // un lien vers l'artiste, qui doit porter le seul nom.
    if e.kind == TypeElement::Track
        && let Some((artiste, _)) = e.subtitle.split_once(" \u{2022} ")
    {
        e.subtitle = artiste.to_string();
    }
    Some(e)
}

/// Les rayons d'une page (accueil, tendances, contenu d'une ambiance).
///
/// Trois dispositions : carrousel (`musicCarouselShelfRenderer`), grille
/// (`gridRenderer`) et liste (`musicShelfRenderer`). Un rayon sans titre ou
/// sans aucun élément lisible est écarté : on ne montre pas un titre au-dessus
/// du vide.
pub fn parser_rayons(data: &Value) -> Vec<Rayon> {
    let mut rayons = Vec::new();
    for section in sections(data) {
        let (titre, contenu) = if !section["musicCarouselShelfRenderer"].is_null() {
            let s = &section["musicCarouselShelfRenderer"];
            (
                texte(&s["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]),
                &s["contents"],
            )
        } else if !section["gridRenderer"].is_null() {
            let s = &section["gridRenderer"];
            (
                texte(&s["header"]["gridHeaderRenderer"]["title"]),
                &s["items"],
            )
        } else if !section["musicShelfRenderer"].is_null() {
            let s = &section["musicShelfRenderer"];
            (texte(&s["title"]), &s["contents"])
        } else {
            continue;
        };
        let items: Vec<ElementRayon> = contenu
            .as_array()
            .map(|c| c.iter().filter_map(element).collect())
            .unwrap_or_default();
        if !titre.is_empty() && !items.is_empty() {
            rayons.push(Rayon {
                title: titre,
                items,
            });
        }
    }
    rayons
}

/// Les groupes d'ambiances et de genres de `FEmusic_moods_and_genres`.
pub fn parser_ambiances(data: &Value) -> Vec<CategorieAmbiances> {
    let mut groupes = Vec::new();
    for section in sections(data) {
        let grille = &section["gridRenderer"];
        if grille.is_null() {
            continue;
        }
        let titre = texte(&grille["header"]["gridHeaderRenderer"]["title"]);
        let items: Vec<Ambiance> = grille["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|w| {
                        let b = &w["musicNavigationButtonRenderer"];
                        let title = texte(&b["buttonText"]);
                        let params = b["clickCommand"]["browseEndpoint"]["params"].as_str()?;
                        (!title.is_empty() && !params.is_empty()).then(|| Ambiance {
                            title,
                            params: params.into(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !titre.is_empty() && !items.is_empty() {
            groupes.push(CategorieAmbiances {
                title: titre,
                items,
            });
        }
    }
    groupes
}

/// L'en-tête d'une page de playlist.
///
/// YouTube Music l'a déplacé : l'ancien `header.musicDetailHeaderRenderer`
/// n'existe plus sur une page de playlist, remplacé par un
/// `musicResponsiveHeaderRenderer` posé dans la PREMIÈRE section de l'onglet
/// (disposition à deux colonnes). Sans lui, `get_playlist` titrait toutes les
/// playlists « Unknown ».
pub fn entete_playlist(data: &Value) -> EntetePlaylist {
    let ancien = &data["header"]["musicDetailHeaderRenderer"];
    let edite = &data["header"]["musicEditablePlaylistDetailHeaderRenderer"]["header"]["musicDetailHeaderRenderer"];
    let nouveau = sections(data)
        .into_iter()
        .map(|s| &s["musicResponsiveHeaderRenderer"])
        .find(|h| !h.is_null());

    let non_vide = |s: String| (!s.is_empty()).then_some(s);
    if let Some(h) = nouveau {
        return EntetePlaylist {
            title: non_vide(texte(&h["title"])),
            description: non_vide(texte(
                &h["description"]["musicDescriptionShelfRenderer"]["description"],
            )),
            cover_path: derniere_vignette(
                &h["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
            ),
        };
    }
    let h = if ancien.is_null() { edite } else { ancien };
    EntetePlaylist {
        title: non_vide(texte(&h["title"])),
        description: non_vide(texte(&h["description"])),
        cover_path: derniere_vignette(
            &h["thumbnail"]["croppedSquareThumbnailRenderer"]["thumbnail"]["thumbnails"],
        ),
    }
}

/// Un code pays des tendances : deux lettres ASCII, en majuscules, ou `ZZ`
/// (monde). Tout le reste est refusé AVANT d'aller chez YouTube.
pub fn code_pays(brut: &str) -> Option<String> {
    let c = brut.trim().to_ascii_uppercase();
    (c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase())).then_some(c)
}

#[cfg(test)]
#[path = "youtube_decouverte_tests.rs"]
mod tests;
