//! Catalogue imprimable de la bibliothèque (#7671).
//!
//! `GET /api/v1/export/catalogue.html` rend UN fichier HTML autonome : la liste
//! des albums avec pochette, titre, artiste, année et format, sa feuille de
//! style et ses images DANS le fichier. Il s'ouvre hors ligne, loin du serveur,
//! et s'imprime tel quel.
//!
//! PDF : laissé au navigateur (« Imprimer → Enregistrer en PDF »). La feuille
//! porte ses règles `@media print` ; produire le PDF ici demanderait un moteur
//! de rendu, une dépendance lourde pour un résultat moins sûr que celui du
//! navigateur.
//!
//! Aucun chemin de fichier ne sort : `cover_path` est un chemin du disque de
//! l'utilisateur (ou un condensat du cache), il sert à LIRE l'image, jamais à
//! la désigner. La pochette part en `data:image/…;base64,…`, et une pochette
//! distante (`http…`) n'est pas récupérée : sa case reste vide.
//!
//! Paramètres, tous facultatifs et cumulables (ET entre eux) :
//! - `genre` : le genre principal OU l'un des genres de l'album, sans casse ;
//! - `artist` (nom, sans casse) ou `artist_id` ;
//! - `quality` : `dsd`, `hires`, `cd`, `lossy`, plusieurs séparés par des
//!   virgules (OU entre eux, comme les pastilles de la Bibliothèque) ;
//! - `format` : `FLAC`, `WAV`…, plusieurs séparés par des virgules, sans casse ;
//! - `sort` : `title` (défaut), `artist`, `year` ; `order` : `asc` ou `desc` ;
//! - `covers=false` : sans pochettes, pour une très grande bibliothèque ;
//! - `lang` : `fr` ou `en` (à défaut, l'en-tête `Accept-Language`).

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::Album;

use crate::state::AppState;

/// Case de vignette des pochettes intégrées : assez pour l'impression d'une
/// grille, assez petite pour qu'un catalogue de quelques milliers d'albums
/// reste un fichier raisonnable.
const CASE_POCHETTE: u32 = 200;

/// Les filtres et le tri demandés, déjà lus et normalisés.
#[derive(Debug, Default, Clone)]
pub struct FiltresCatalogue {
    pub genre: Option<String>,
    pub artist: Option<String>,
    pub artist_id: Option<i64>,
    pub qualities: Vec<String>,
    pub formats: Vec<String>,
    pub sort: String,
    pub desc: bool,
    pub covers: bool,
    pub lang: Langue,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Langue {
    Fr,
    #[default]
    En,
}

impl FiltresCatalogue {
    pub fn depuis_requete(q: &HashMap<String, String>, accept_language: Option<&str>) -> Self {
        let texte = |k: &str| {
            q.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let liste = |k: &str| -> Vec<String> {
            q.get(k)
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        let sort = match texte("sort").as_deref() {
            Some("artist") => "artist",
            Some("year") | Some("original_year") => "year",
            _ => "title",
        }
        .to_string();
        let lang_demandee = texte("lang").or_else(|| accept_language.map(|s| s.to_string()));
        let lang = match lang_demandee {
            Some(l) if l.to_ascii_lowercase().starts_with("fr") => Langue::Fr,
            _ => Langue::En,
        };
        Self {
            genre: texte("genre"),
            artist: texte("artist"),
            artist_id: texte("artist_id").and_then(|v| v.parse().ok()),
            qualities: liste("quality"),
            formats: liste("format"),
            sort,
            desc: texte("order").is_some_and(|o| o.eq_ignore_ascii_case("desc")),
            covers: texte("covers").is_none_or(|v| v != "false" && v != "0"),
            lang,
        }
    }

    /// Le filtre que la base ne sait pas faire seule : genre, artiste, et les
    /// listes de qualités et de formats (la base n'en prend qu'une valeur).
    pub fn garde(&self, a: &Album) -> bool {
        if let Some(id) = self.artist_id
            && a.artist_id != Some(id)
        {
            return false;
        }
        if let Some(nom) = &self.artist {
            let artiste = a.artist_name.as_deref().unwrap_or("").trim();
            if artiste.to_lowercase() != nom.to_lowercase() {
                return false;
            }
        }
        if let Some(g) = &self.genre
            && !genres_de(a).iter().any(|x| meme_genre(x, g))
        {
            return false;
        }
        if !self.qualities.is_empty() {
            // `hi-res` est le badge, `hires` la clé de la route : les deux.
            let badge = a.quality().map(|b| b.replace('-', ""));
            if !self
                .qualities
                .iter()
                .any(|q| badge.as_deref() == Some(q.replace('-', "").as_str()))
            {
                return false;
            }
        }
        if !self.formats.is_empty() {
            let f = a
                .format
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !self.formats.contains(&f) {
                return false;
            }
        }
        true
    }
}

/// Le genre principal et le tableau JSON `genres`, sans doublon.
fn genres_de(a: &Album) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Some(g) = a.genre.as_deref() {
        v.push(g.to_string());
    }
    if let Some(json) = a.genres.as_deref()
        && let Ok(liste) = serde_json::from_str::<Vec<String>>(json)
    {
        v.extend(liste);
    }
    v
}

fn meme_genre(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Une ligne du catalogue : ce qui s'imprime, rien d'autre. La pochette est
/// déjà une URI `data:` — le type ne peut pas porter de chemin.
#[derive(Debug, Clone)]
pub struct LigneCatalogue {
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub format: String,
    pub cover_data_uri: Option<String>,
}

impl LigneCatalogue {
    pub fn depuis_album(a: &Album) -> Self {
        Self {
            title: a.title.clone(),
            artist: a.artist_name.clone().unwrap_or_default(),
            year: a.year.or(a.original_year).filter(|y| *y > 0),
            format: libelle_format(a),
            cover_data_uri: None,
        }
    }
}

/// « FLAC 24/96 », « DSF », « MP3 » : le format, plus résolution et cadence
/// quand elles sont connues et que le format n'est pas du DSD.
fn libelle_format(a: &Album) -> String {
    let fmt = a.format.as_deref().unwrap_or("").trim().to_uppercase();
    let dsd = a.quality().as_deref() == Some("dsd");
    match (a.bit_depth, a.sample_rate) {
        (Some(bd), Some(sr)) if bd > 0 && sr > 0 && !dsd => {
            let khz = sr as f64 / 1000.0;
            let khz = if (khz - khz.round()).abs() < f64::EPSILON {
                format!("{}", khz as i64)
            } else {
                format!("{khz:.1}")
            };
            format!("{fmt} {bd}/{khz}").trim().to_string()
        }
        _ => fmt,
    }
}

pub fn echapper(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

struct Libelles {
    titre: &'static str,
    albums: &'static str,
    album: &'static str,
    artiste: &'static str,
    annee: &'static str,
    format: &'static str,
    genre: &'static str,
    qualite: &'static str,
    edite_le: &'static str,
    imprimer: &'static str,
    vide: &'static str,
}

fn libelles(l: Langue) -> Libelles {
    match l {
        Langue::Fr => Libelles {
            titre: "Catalogue de la bibliothèque",
            albums: "albums",
            album: "Album",
            artiste: "Artiste",
            annee: "Année",
            format: "Format",
            genre: "Genre",
            qualite: "Qualité",
            edite_le: "Édité le",
            imprimer: "Pour un PDF : Imprimer, puis Enregistrer en PDF.",
            vide: "Aucun album ne correspond à ces filtres.",
        },
        Langue::En => Libelles {
            titre: "Library catalogue",
            albums: "albums",
            album: "Album",
            artiste: "Artist",
            annee: "Year",
            format: "Format",
            genre: "Genre",
            qualite: "Quality",
            edite_le: "Generated on",
            imprimer: "For a PDF: Print, then Save as PDF.",
            vide: "No album matches these filters.",
        },
    }
}

const STYLE: &str = "\
*{box-sizing:border-box}\
body{font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;margin:24px;color:#111;background:#fff}\
header h1{margin:0 0 4px;font-size:22px}\
header p{margin:2px 0;color:#555;font-size:13px}\
.astuce{font-style:italic}\
table{width:100%;border-collapse:collapse;margin-top:16px;font-size:13px}\
th{text-align:left;border-bottom:2px solid #333;padding:6px 8px}\
td{border-bottom:1px solid #ddd;padding:6px 8px;vertical-align:middle}\
td.pochette{width:72px}\
td.pochette img,td.pochette span{display:block;width:64px;height:64px;object-fit:cover;border-radius:3px;background:#eee}\
td.annee,td.format{white-space:nowrap}\
tr{break-inside:avoid;page-break-inside:avoid}\
thead{display:table-header-group}\
@media print{body{margin:0}.astuce{display:none}@page{margin:12mm}}";

/// Le document entier. Fonction pure : la route lui passe des lignes déjà
/// filtrées, triées et munies de leurs pochettes.
pub fn rendre_html(lignes: &[LigneCatalogue], f: &FiltresCatalogue, date: &str) -> String {
    let l = libelles(f.lang);
    let mut filtres: Vec<String> = Vec::new();
    if let Some(g) = &f.genre {
        filtres.push(format!("{} : {}", l.genre, echapper(g)));
    }
    if let Some(a) = &f.artist {
        filtres.push(format!("{} : {}", l.artiste, echapper(a)));
    }
    if !f.qualities.is_empty() {
        let q: Vec<String> = f
            .qualities
            .iter()
            .map(|q| match q.as_str() {
                "hires" | "hi-res" => "Hi-Res".to_string(),
                "dsd" | "cd" => q.to_uppercase(),
                autre => echapper(autre),
            })
            .collect();
        filtres.push(format!("{} : {}", l.qualite, q.join(", ")));
    }
    if !f.formats.is_empty() {
        let fm: Vec<String> = f
            .formats
            .iter()
            .map(|x| echapper(&x.to_uppercase()))
            .collect();
        filtres.push(format!("{} : {}", l.format, fm.join(", ")));
    }

    let lang = if f.lang == Langue::Fr { "fr" } else { "en" };
    let mut html = String::with_capacity(4096 + lignes.len() * 512);
    html.push_str(&format!(
        "<!DOCTYPE html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>{titre}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<header>\n\
         <h1>{titre}</h1>\n<p>{n} {albums} · {edite} {date}</p>\n",
        titre = l.titre,
        n = lignes.len(),
        albums = l.albums,
        edite = l.edite_le,
        date = echapper(date),
    ));
    if !filtres.is_empty() {
        html.push_str(&format!("<p>{}</p>\n", filtres.join(" · ")));
    }
    html.push_str(&format!(
        "<p class=\"astuce\">{}</p>\n</header>\n",
        l.imprimer
    ));

    if lignes.is_empty() {
        html.push_str(&format!("<p>{}</p>\n</body>\n</html>\n", l.vide));
        return html;
    }

    let colonne_pochette = if f.covers { "<th></th>" } else { "" };
    html.push_str(&format!(
        "<table>\n<thead><tr>{colonne_pochette}<th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead>\n<tbody>\n",
        l.album, l.artiste, l.annee, l.format
    ));
    for ligne in lignes {
        html.push_str("<tr>");
        if f.covers {
            match &ligne.cover_data_uri {
                Some(uri) => html.push_str(&format!(
                    "<td class=\"pochette\"><img src=\"{uri}\" alt=\"\"></td>"
                )),
                None => html.push_str("<td class=\"pochette\"><span></span></td>"),
            }
        }
        html.push_str(&format!(
            "<td class=\"titre\">{}</td><td class=\"artiste\">{}</td><td class=\"annee\">{}</td><td class=\"format\">{}</td></tr>\n",
            echapper(&ligne.title),
            echapper(&ligne.artist),
            ligne.year.map(|y| y.to_string()).unwrap_or_default(),
            echapper(&ligne.format),
        ));
    }
    html.push_str("</tbody>\n</table>\n</body>\n</html>\n");
    html
}

/// La pochette d'un album en URI `data:`, ou `None`.
///
/// `cover_path` est soit un condensat du cache, soit le chemin d'une image du
/// disque (cherchée d'abord dans le cache sous son condensat), soit une
/// adresse distante — jamais récupérée ici. L'image est réduite à
/// [`CASE_POCHETTE`] ; plus petite, elle part telle quelle.
pub fn pochette_data_uri(cover_path: &str, cache_dir: &std::path::Path) -> Option<String> {
    use tune_core::library::artwork;

    if cover_path.starts_with("http") {
        return None;
    }
    let est_condensat = (cover_path.len() == 32 || cover_path.len() == 64)
        && cover_path.chars().all(|c| c.is_ascii_hexdigit());
    let condensat = if est_condensat {
        cover_path.to_string()
    } else {
        artwork::artwork_hash(cover_path)
    };
    let octets = match artwork::find_cached(cache_dir, &condensat) {
        Some((chemin, _)) => std::fs::read(chemin).ok()?,
        None if !est_condensat => std::fs::read(cover_path).ok()?,
        None => return None,
    };
    if let Some(vignette) = artwork::make_thumbnail(&octets, CASE_POCHETTE) {
        return Some(data_uri("image/jpeg", &vignette));
    }
    // Déjà plus petite que la case, ou format non décodable : l'original, à
    // condition d'en reconnaître le type — sinon le navigateur n'en ferait rien.
    Some(data_uri(type_image(&octets)?, &octets))
}

/// Le type d'une image d'après ses premiers octets.
fn type_image(o: &[u8]) -> Option<&'static str> {
    if o.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if o.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if o.len() >= 12 && &o[..4] == b"RIFF" && &o[8..12] == b"WEBP" {
        Some("image/webp")
    } else if o.starts_with(b"GIF8") {
        Some("image/gif")
    } else if o.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

fn data_uri(mime: &str, octets: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64(octets))
}

/// Base64 standard, avec bourrage. Quinze lignes plutôt qu'une dépendance de
/// plus à la caisse serveur, qui n'en a pas à l'exécution.
fn base64(octets: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(octets.len().div_ceil(3) * 4);
    for bloc in octets.chunks(3) {
        let n = (u32::from(bloc[0]) << 16)
            | (u32::from(*bloc.get(1).unwrap_or(&0)) << 8)
            | u32::from(*bloc.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= bloc.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// GET /export/catalogue.html
pub(super) async fn export_catalogue_html(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let accept = headers.get("accept-language").and_then(|v| v.to_str().ok());
    let filtres = FiltresCatalogue::depuis_requete(&q, accept);

    let repo = AlbumRepo::with_backend(state.backend.clone());
    let order = if filtres.desc { "desc" } else { "asc" };
    // Albums masqués exclus, comme dans la Bibliothèque. Le tri vient de la
    // base ; une seule qualité passe aussi par elle, le reste est filtré ici.
    let qualite_sql = match filtres.qualities.as_slice() {
        [une] => Some(une.as_str()),
        _ => None,
    };
    let albums = match repo.list_filtered(
        999_999,
        0,
        &filtres.sort,
        order,
        None,
        qualite_sql,
        None,
        false,
        None,
    ) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("catalogue export: album listing failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let filtres_tache = filtres.clone();
    let cache_dir = super::super::library::artwork_cache_dir();
    let lignes = tokio::task::spawn_blocking(move || {
        albums
            .iter()
            .filter(|a| filtres_tache.garde(a))
            .map(|a| {
                let mut ligne = LigneCatalogue::depuis_album(a);
                if filtres_tache.covers {
                    ligne.cover_data_uri = a
                        .cover_path
                        .as_deref()
                        .and_then(|c| pochette_data_uri(c, &cache_dir));
                }
                ligne
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let html = rendre_html(&lignes, &filtres, &date);

    let mut h = HeaderMap::new();
    h.insert(
        "Content-Type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        "Content-Disposition",
        HeaderValue::from_static("attachment; filename=\"catalogue.html\""),
    );
    (StatusCode::OK, h, html).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(titre: &str, artiste: &str, genre: &str, fmt: &str, sr: i32, bd: i32) -> Album {
        let mut a = Album::new(titre.to_string());
        a.artist_name = Some(artiste.to_string());
        a.genre = Some(genre.to_string());
        a.format = Some(fmt.to_string());
        a.sample_rate = Some(sr);
        a.bit_depth = Some(bd);
        a
    }

    #[test]
    fn le_texte_est_echappe() {
        let f = FiltresCatalogue {
            covers: false,
            ..Default::default()
        };
        let mut l = LigneCatalogue::depuis_album(&album("<b>&", "a\"b", "Jazz", "FLAC", 0, 0));
        l.year = Some(1999);
        let html = rendre_html(&[l], &f, "2026-11-03");
        assert!(html.contains("&lt;b&gt;&amp;"));
        assert!(html.contains("a&quot;b"));
        assert!(!html.contains("<b>&"));
    }

    #[test]
    fn base64_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xFF, 0xFE]), "//4=");
    }

    #[test]
    fn libelle_de_format() {
        assert_eq!(
            libelle_format(&album("x", "y", "", "flac", 96000, 24)),
            "FLAC 24/96"
        );
        assert_eq!(
            libelle_format(&album("x", "y", "", "FLAC", 44100, 16)),
            "FLAC 16/44.1"
        );
        assert_eq!(libelle_format(&album("x", "y", "", "MP3", 0, 0)), "MP3");
    }

    #[test]
    fn genre_dans_le_tableau_des_genres() {
        let mut a = album("x", "y", "Rock", "FLAC", 44100, 16);
        a.genres = Some(r#"["Rock","Jazz Fusion"]"#.to_string());
        let f = FiltresCatalogue {
            genre: Some("jazz fusion".into()),
            ..Default::default()
        };
        assert!(f.garde(&a));
        let f = FiltresCatalogue {
            genre: Some("Classique".into()),
            ..Default::default()
        };
        assert!(!f.garde(&a));
    }
}
