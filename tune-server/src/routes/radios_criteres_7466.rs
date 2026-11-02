//! #7466 — la recherche de radios par critères.
//!
//! Les mêmes critères servent `GET /radios/search` (« Mes radios » et les
//! suggestions de l'annuaire) et `GET /radios/directory` (l'annuaire). Tous
//! sont facultatifs et se combinent en ET ; plusieurs genres se combinent en OU.
//!
//! Le filtrage se fait ici, en Rust, et non en SQL : le pays et le genre d'une
//! station locale sont stockés EN CLAIR (« Royaume-Uni », « Classique ») et ne
//! se comparent à un code ISO ou à une clé d'annuaire qu'à travers
//! [`super::radios_libelles`] ; la fréquence d'échantillonnage et la note
//! n'existent que dans la fiche d'annuaire. Aucune requête SQL n'est donc
//! ajoutée ni modifiée, ce qui vaut pour SQLite comme pour PostgreSQL, et
//! aucun index n'est nécessaire (le catalogue local compte quelques centaines
//! de stations, l'annuaire est en mémoire).
//!
//! Critères livrés : `genre` (liste séparée par des virgules), `country` (code
//! ISO alpha-2), `codec`, `bitrate_min`, `lossless`, `hires` (un flux au-delà
//! de 48 kHz — la profondeur en bits n'est publiée nulle part), `favorites_only`,
//! `rated_min` (la note moyenne radio-metadata.fr, `avg_rating`) et
//! `metadata_plus` (la fiche a une clé radio-metadata.fr : le badge Metadata+).
//! `language` n'est PAS proposé : ni l'annuaire ni le catalogue livré ne
//! portent cette donnée, et un filtre sans donnée écarterait tout.

use std::collections::HashMap;

use serde_json::Value;
use tune_core::db::radio_repo::RadioStation;

use super::radios::StationAnnuaire;
use super::radios_annuaire::{IndexAnnuaire, flux_de_la_fiche};
use super::radios_libelles::{cle_genre, code_pays};

/// Plafond des suggestions d'annuaire d'une recherche faite de critères seuls
/// (sans texte) : l'annuaire entier se parcourt par `GET /radios/directory`.
pub const SUGGESTIONS_MAX: usize = 200;

/// Au-delà, c'est de la haute résolution.
const FREQUENCE_HIRES: i32 = 48_000;

/// Un nombre JSON, écrit en nombre ou en texte (`4.7` ou `"4.7"`).
pub fn nombre(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n| n.is_finite())
}

/// La note moyenne d'une fiche v2 en centièmes, ou `None` si personne ne l'a
/// notée (`rating_count` à 0 ou absent, ou `avg_rating` absent ou nul).
pub fn note_centiemes(item: &Value) -> Option<i32> {
    let votes = item.get("rating_count").and_then(nombre).unwrap_or(0.0);
    if votes <= 0.0 {
        return None;
    }
    let moyenne = item.get("avg_rating").and_then(nombre)?;
    (moyenne > 0.0).then(|| (moyenne * 100.0).round() as i32)
}

/// Un format de flux ramené à sa famille : `AAC+`, `HE-AAC` → `aac`.
pub fn famille_codec(codec: &str) -> String {
    let c = codec.trim().to_ascii_lowercase();
    match c.as_str() {
        "aac+" | "aacp" | "he-aac" | "he-aac v2" | "heaac" | "aac-lc" | "aac lc" | "m4a" => {
            "aac".into()
        }
        "mpeg" | "mpga" | "audio/mpeg" => "mp3".into(),
        "ogg" | "ogg vorbis" => "vorbis".into(),
        "ogg opus" => "opus".into(),
        "ogg flac" => "flac".into(),
        _ => c,
    }
}

/// Les formats sans perte.
pub fn est_sans_perte(codec: &str) -> bool {
    matches!(
        famille_codec(codec).as_str(),
        "flac" | "alac" | "wav" | "pcm" | "lpcm" | "aiff" | "wavpack" | "wv" | "ape"
    )
}

/// Le code ISO alpha-2 d'un pays écrit en code (`gb`) ou en clair (« France »).
fn code_iso(pays: &str) -> Option<String> {
    let p = pays.trim();
    if p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(p.to_ascii_uppercase());
    }
    code_pays(p).map(str::to_string)
}

/// Deux genres sont-ils le même ? À la casse près ; pour une station de
/// « Mes radios » (`en_clair`, genre écrit en français par le semis), aussi
/// par leur clé de traduction (`classical` = « Classique ») ou la clé
/// elle-même (`radio.genre.jazz`). Une fiche d'annuaire garde la comparaison
/// exacte d'avant #7466, celle de ses facettes.
fn meme_genre(demande: &str, genre: &str, en_clair: bool) -> bool {
    let d = demande.trim();
    let g = genre.trim();
    if d.eq_ignore_ascii_case(g) {
        return true;
    }
    if !en_clair {
        return false;
    }
    let cle_station = cle_genre(g);
    cle_station.is_some() && (cle_genre(d) == cle_station || cle_station == Some(d))
}

/// Ce qu'on sait d'une station pour la juger — fiche d'annuaire ou station de
/// « Mes radios » éclairée par sa fiche.
#[derive(Debug, Default)]
struct Profil<'a> {
    genres: Vec<&'a str>,
    /// Genres écrits en clair (station locale) : voir [`meme_genre`].
    genres_en_clair: bool,
    pays: Option<String>,
    codecs: Vec<String>,
    debit_max: Option<i32>,
    frequence_max: Option<i32>,
    note_centiemes: Option<i32>,
    favori: bool,
    metadata_plus: bool,
}

/// Les critères d'une requête, normalisés.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Criteres {
    pub genres: Vec<String>,
    pub pays: Option<String>,
    pub codec: Option<String>,
    pub debit_min: Option<i32>,
    pub sans_perte: bool,
    pub hires: bool,
    pub favoris: bool,
    pub note_min: Option<f64>,
    pub metadata_plus: bool,
}

impl Criteres {
    #[allow(clippy::too_many_arguments)]
    pub fn depuis(
        genre: Option<&str>,
        country: Option<&str>,
        codec: Option<&str>,
        bitrate_min: Option<i32>,
        lossless: Option<bool>,
        hires: Option<bool>,
        favorites_only: Option<bool>,
        rated_min: Option<f64>,
        metadata_plus: Option<bool>,
    ) -> Self {
        let mut genres: Vec<String> = Vec::new();
        for g in genre
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            if !genres.iter().any(|x| x.eq_ignore_ascii_case(g)) {
                genres.push(g.to_string());
            }
        }
        fn texte(v: Option<&str>) -> Option<&str> {
            v.map(str::trim).filter(|s| !s.is_empty())
        }
        Self {
            genres,
            pays: texte(country).map(|p| code_iso(p).unwrap_or_else(|| p.to_ascii_uppercase())),
            codec: texte(codec).map(famille_codec),
            debit_min: bitrate_min.filter(|b| *b > 0),
            sans_perte: lossless == Some(true),
            hires: hires == Some(true),
            favoris: favorites_only == Some(true),
            note_min: rated_min.filter(|n| n.is_finite() && *n > 0.0),
            metadata_plus: metadata_plus == Some(true),
        }
    }

    /// Aucun critère : la requête est celle d'avant #7466.
    pub fn est_vide(&self) -> bool {
        *self == Self::default()
    }

    /// Le genre d'une fiche : l'un des genres demandés est l'un des siens.
    pub fn genre_ok(&self, genres: &[&str]) -> bool {
        self.genre_ok_selon(genres, false)
    }

    fn genre_ok_selon(&self, genres: &[&str], en_clair: bool) -> bool {
        self.genres.is_empty()
            || self
                .genres
                .iter()
                .any(|d| genres.iter().any(|g| meme_genre(d, g, en_clair)))
    }

    /// Le pays (code ISO), quelle que soit l'écriture de la station.
    pub fn pays_ok(&self, pays: Option<&str>) -> bool {
        self.pays
            .as_deref()
            .is_none_or(|p| pays.and_then(code_iso).as_deref() == Some(p))
    }

    /// Tous les critères autres que le genre et le pays — ceux dont
    /// l'annuaire ne compte pas de facette.
    fn autres_ok(&self, p: &Profil) -> bool {
        self.codec
            .as_deref()
            .is_none_or(|c| p.codecs.iter().any(|x| x == c))
            && self
                .debit_min
                .is_none_or(|min| p.debit_max.is_some_and(|d| d >= min))
            && (!self.sans_perte || p.codecs.iter().any(|c| est_sans_perte(c)))
            && (!self.hires || p.frequence_max.is_some_and(|f| f > FREQUENCE_HIRES))
            && (!self.favoris || p.favori)
            && self.note_min.is_none_or(|min| {
                p.note_centiemes
                    .is_some_and(|n| f64::from(n) + 0.5 >= min * 100.0)
            })
            && (!self.metadata_plus || p.metadata_plus)
    }

    fn retient(&self, p: &Profil) -> bool {
        self.genre_ok_selon(&p.genres, p.genres_en_clair)
            && self.pays_ok(p.pays.as_deref())
            && self.autres_ok(p)
    }

    /// La fiche passe-t-elle les critères autres que le genre et le pays ?
    /// `favori` : la station de « Mes radios » rapprochée de la fiche est en
    /// favori (`None` : la fiche n'est pas dans « Mes radios »).
    pub fn autres_ok_pour_la_fiche(&self, f: &StationAnnuaire, favori: Option<bool>) -> bool {
        self.autres_ok(&profil_de_fiche(f, favori))
    }

    /// La fiche passe-t-elle TOUS les critères ?
    pub fn retient_la_fiche(&self, f: &StationAnnuaire, favori: Option<bool>) -> bool {
        self.retient(&profil_de_fiche(f, favori))
    }

    /// Les stations de « Mes radios » qui passent les critères. Ce que la
    /// station ne dit pas (fréquence, note, Metadata+, et format ou débit
    /// quand ils sont vides) se lit dans sa fiche d'annuaire, retrouvée comme
    /// pour l'affichage (lien enregistré, flux, nom).
    pub fn filtrer_les_stations(
        &self,
        stations: Vec<RadioStation>,
        annuaire: &[StationAnnuaire],
        liens: &HashMap<i64, String>,
    ) -> Vec<RadioStation> {
        let index = IndexAnnuaire::new(annuaire);
        stations
            .into_iter()
            .filter(|st| {
                let ext = st.id.and_then(|id| liens.get(&id)).map(String::as_str);
                let fiche = index
                    .lien(ext, &st.name, &st.url)
                    .and_then(|(i, _)| index.fiche(i));
                self.retient(&profil_de_station(st, fiche))
            })
            .collect()
    }
}

fn profil_de_fiche(f: &StationAnnuaire, favori: Option<bool>) -> Profil<'_> {
    let mut codecs: Vec<String> = Vec::new();
    let mut ajouter = |c: &str| {
        let c = famille_codec(c);
        if !c.is_empty() && !codecs.contains(&c) {
            codecs.push(c);
        }
    };
    if let Some(c) = f.codec.as_deref() {
        ajouter(c);
    }
    for x in &f.flux {
        if let Some(c) = x.codec.as_deref() {
            ajouter(c);
        }
    }
    // Une fiche v1 sans flux détaillé ne dit son format que par `quality`.
    if f.quality
        .as_deref()
        .is_some_and(|q| q.eq_ignore_ascii_case("flac"))
    {
        ajouter("flac");
    }
    Profil {
        genres: f.genres_ou_genre(),
        genres_en_clair: false,
        pays: f.country.clone(),
        codecs,
        debit_max: f
            .bitrate
            .into_iter()
            .chain(f.flux.iter().filter_map(|x| x.bitrate))
            .max(),
        frequence_max: f.flux.iter().filter_map(|x| x.sample_rate).max(),
        note_centiemes: f.note_centiemes,
        favori: favori == Some(true),
        metadata_plus: f.radio_metadata_key.is_some(),
    }
}

fn profil_de_station<'a>(st: &'a RadioStation, fiche: Option<&'a StationAnnuaire>) -> Profil<'a> {
    // Le flux de la fiche que la station écoute vraiment.
    let flux = fiche.and_then(|f| {
        f.flux.iter().find(|x| {
            super::radios::flux_normalise(&x.url) == super::radios::flux_normalise(&st.url)
        })
    });
    // L'adresse principale de la fiche, sans détail de flux (fiche v1) : le
    // format et le débit de la fiche sont ceux de ce flux.
    let principale =
        fiche.filter(|f| flux.is_none() && flux_de_la_fiche(f, &st.url) && f.flux.is_empty());
    let codec = st
        .codec
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .or_else(|| flux.and_then(|x| x.codec.clone()))
        .or_else(|| principale.and_then(|f| f.codec.clone()));
    Profil {
        genres: st.genre.as_deref().into_iter().collect(),
        genres_en_clair: true,
        pays: st.country.clone(),
        codecs: codec.map(|c| famille_codec(&c)).into_iter().collect(),
        debit_max: st
            .bitrate
            .filter(|b| *b > 0)
            .or_else(|| flux.and_then(|x| x.bitrate))
            .or_else(|| principale.and_then(|f| f.bitrate)),
        frequence_max: flux.and_then(|x| x.sample_rate),
        note_centiemes: fiche.and_then(|f| f.note_centiemes),
        favori: st.is_favorite,
        metadata_plus: fiche.is_some_and(|f| f.radio_metadata_key.is_some()),
    }
}

#[cfg(test)]
#[path = "radios_criteres_7466_tests.rs"]
mod tests;
