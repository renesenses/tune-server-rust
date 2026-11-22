//! #8487 (go de Bertrand du 18/11/2026, rc13) — les TITRES DIFFUSÉS par une
//! radio, notés par le serveur sous la ligne de leur station.
//!
//! # Pourquoi
//!
//! FabienM, fil 2723 : un titre entendu sur une radio lancée depuis Android
//! (« Rattlesnake », BBC Radio 6 Music) n'apparaît pas dans l'historique du
//! client web, qui ne montre que la station. Jusqu'ici, seuls les carnets de
//! chaque appareil (`localStorage` du navigateur, `shared_preferences` de
//! l'app) notaient ces titres : chacun ne voyait que ce qu'IL avait entendu.
//! Le serveur les note désormais, une fois pour tous les appareils.
//!
//! # Ce que #7965 avait écarté, et qui tient toujours
//!
//! #7965 (« jamais le titre ICY ») avait trois raisons valables, toutes
//! gardées ici :
//!
//! 1. **Les statistiques.** Un titre de radio n'est pas une écoute choisie :
//!    il ne doit compter ni dans « les plus écoutés », ni dans Tune Circle,
//!    ni dans les recommandations, ni partir en scrobble. Les titres
//!    diffusés vivent donc dans CETTE table, pas dans `listen_history` :
//!    aucune requête de statistiques ne la lit, et aucune ne peut l'oublier
//!    dans un filtre — l'exclusion tient par construction, pas par un
//!    `source != 'radio'` à recopier partout.
//! 2. **Les doublons.** Le sondeur relit le flux en boucle et republie le
//!    même titre ; certaines stations alternent titre et jingle. Un titre
//!    déjà noté parmi les [`TITRES_COMPARES`] derniers de la même écoute ne
//!    l'est pas une seconde fois ([`noter`]).
//! 3. **Le bruit.** Un titre vide, le nom de la station, une adresse web, une
//!    annonce de publicité ou de jingle ne sont pas des titres
//!    ([`titre_a_noter`]).
//!
//! La ligne de la station (`listen_history`, une par lancement, #7965) reste
//! ce qu'elle est : c'est elle qui compte une écoute de radio, et les titres
//! se rangent SOUS elle (`ecoute_id`).
//!
//! # Rattachement
//!
//! Un titre se rattache à la DERNIÈRE ligne d'historique du groupe de sa zone
//! (#8107 : une seule zone d'un groupe écrit la ligne de station ; seule la
//! plus petite zone du groupe note), et seulement si c'est la ligne de
//! station du même flux (`source = 'radio'`, `context_type = 'radio'`, même
//! `source_id`). Sur une zone navigateur, la ligne de station n'est écrite
//! qu'une fois l'écoute confirmée par le lecteur (#6066) : avant, rien n'est
//! noté ; le sondeur repasse au tour suivant. Un renderer arrêté ne note rien.
//!
//! Une ligne restée [`FRAICHEUR_MS`] sans lancement ni titre (radio reprise
//! le lendemain, relance par `play_without_history`) n'accueille plus de
//! titres : elle est recopiée à l'heure présente et une nouvelle session
//! commence — jamais de titre du jour sous la ligne de la veille.
//!
//! # Volume
//!
//! [`TITRES_GARDES`] titres au plus par ligne (les plus anciens sont effacés
//! à l'écriture) ; l'historique n'en relit que [`TITRES_PAR_STATION`], borne
//! posée en SQL.
//!
//! # Schéma
//!
//! Table sans numéro de migration, sur le modèle de
//! `alertes_douteuses_ignorees` (#8958) : les numéros SQLite 156 et PG 123
//! sont déjà demandés par d'autres PR du lot rc13. Passe finale de
//! `run_migrations` (SQLite), `ENSURE_TABLES` et `PG_FULL_SCHEMA`
//! (PostgreSQL). Copiée à la bascule, comme l'historique.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use super::backend::{DbBackend, ToSqlValue};

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/radio_titres_diffuses.sql");

/// Combien de titres d'une même écoute sont comparés au titre annoncé : le
/// dernier (le sondeur republie le même titre) et deux avant (une station
/// qui alterne titre, jingle et titre).
pub const TITRES_COMPARES: i64 = 3;

/// Combien de titres une ligne de station rend au plus dans
/// `GET /library/history` : une nuit d'écoute en compte des centaines.
pub const TITRES_PAR_STATION: usize = 100;

/// Combien de titres une ligne de station garde au plus : au-delà, les plus
/// anciens sont effacés à l'écriture ([`noter`]).
pub const TITRES_GARDES: i64 = 200;

/// Au-delà de ce silence (ni lancement ni titre noté), la ligne de station
/// n'accueille plus de titres : une nouvelle session commence
/// ([`station_en_cours`]).
pub const FRAICHEUR_MS: i64 = 3 * 3600 * 1000;

/// Un titre diffusé, tel que l'historique le rend sous sa station.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TitreDiffuse {
    /// La ligne de station (`listen_history.id`) sous laquelle il se range.
    #[serde(skip)]
    pub ecoute_id: i64,
    /// L'instant de diffusion, en millisecondes depuis l'époque Unix.
    #[serde(skip)]
    pub diffuse_le: i64,
    #[serde(rename = "zone_id")]
    pub zone_id: Option<i64>,
    #[serde(rename = "title")]
    pub titre: String,
    #[serde(rename = "artist_name")]
    pub artiste: Option<String>,
    #[serde(rename = "cover_url")]
    pub pochette: Option<String>,
    /// La même forme que `listened_at` (`AAAA-MM-JJTHH:MM:SSZ`).
    #[serde(rename = "played_at")]
    pub diffuse_a: String,
}

/// Pourquoi un titre annoncé n'a pas été noté (ou qu'il l'a été).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation {
    Ecrit,
    /// Vide, nom de la station, adresse web, publicité, jingle…
    PasUnTitre,
    /// La dernière ligne de la zone n'est pas la station de ce flux.
    PasDeStation,
    /// Déjà noté parmi les derniers titres de la même écoute.
    DejaNote,
    /// Zone compagne d'un groupe : la plus petite zone du groupe note.
    ZoneCompagne,
}

fn norme(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Les titres ou interprètes qui, SEULS, n'annoncent aucun morceau.
const NON_MUSICAUX: &[&str] = &[
    "unknown",
    "inconnu",
    "n/a",
    "na",
    "none",
    "null",
    "untitled",
    "sans titre",
    "no title",
    "live",
    "en direct",
    "direct",
    "on air",
    "radio",
    "stream",
    "news",
    "info",
    "infos",
    "flash info",
    "journal",
    "météo",
    "meteo",
    "pub",
    "pubs",
    "publicité",
    "publicite",
    "advert",
    "adverts",
    "advertisement",
    "advertising",
    "ad",
    "ads",
    "ad break",
    "commercial",
    "commercials",
    "commercial break",
    "jingle",
    "station id",
    "station jingle",
    "interlude",
];

/// Les mots qui, dans un titre SANS interprète (ou dont l'interprète est la
/// station), désignent un habillage d'antenne. Avec un vrai interprète, le
/// titre reste un morceau : « Jingle Bell Rock » de Bobby Helms.
const MOTS_D_HABILLAGE: &[&str] = &[
    "jingle",
    "publicit",
    "advert",
    "commercial break",
    "ad break",
    "station id",
    "spot pub",
];

fn est_une_adresse(s: &str) -> bool {
    s.contains("http://") || s.contains("https://") || s.starts_with("www.") || s.contains(" www.")
}

/// Le titre annoncé par le flux est-il un MORCEAU à noter ?
///
/// Écarte : un titre vide ou sans lettre ni chiffre, le nom de la station
/// (en titre, en interprète, ou en tête d'un titre sans interprète :
/// « FIP - en direct »), une adresse web, une annonce seule (« Unknown »,
/// « Pub », « Jingle »…), un habillage d'antenne sans interprète.
pub fn titre_a_noter(station: &str, artiste: Option<&str>, titre: &str) -> bool {
    let t = norme(titre);
    let a = norme(artiste.unwrap_or(""));
    let st = norme(station);
    if t.is_empty() || !t.chars().any(char::is_alphanumeric) {
        return false;
    }
    if est_une_adresse(&t) || est_une_adresse(&a) {
        return false;
    }
    if NON_MUSICAUX.contains(&t.as_str()) || NON_MUSICAUX.contains(&a.as_str()) {
        return false;
    }
    if !st.is_empty() {
        if t == st || a == st {
            return false;
        }
        if a.is_empty()
            && let Some(reste) = t.strip_prefix(&st)
            && !reste.starts_with(char::is_alphanumeric)
        {
            return false;
        }
    }
    let sans_interprete = a.is_empty() || a == st;
    if sans_interprete && MOTS_D_HABILLAGE.iter().any(|m| t.contains(m)) {
        return false;
    }
    true
}

/// Deux annonces désignent-elles le même morceau (casse et espaces près) ?
pub fn meme_titre(a1: Option<&str>, t1: &str, a2: Option<&str>, t2: &str) -> bool {
    norme(t1) == norme(t2) && norme(a1.unwrap_or("")) == norme(a2.unwrap_or(""))
}

/// L'instant `ms` au format de `listen_history.listened_at`.
pub fn date_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// `listened_at` (`AAAA-MM-JJTHH:MM:SSZ`, ou une forme voisine d'une base
/// ancienne) en millisecondes. `None` : illisible.
fn instant_ms(texte: &str) -> Option<i64> {
    let t = texte.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(d.timestamp_millis());
    }
    [
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .iter()
    .find_map(|f| chrono::NaiveDateTime::parse_from_str(t, f).ok())
    .map(|d| d.and_utc().timestamp_millis())
}

/// Les zones qui jouent ensemble : `zone_id` et ses compagnes de groupe
/// (#8107), triées. Une zone seule rend `[zone_id]`.
fn zones_du_groupe(db: &Arc<dyn DbBackend>, zone_id: i64) -> Vec<i64> {
    let groupes = crate::zones::groupes_enregistres::lire(
        &crate::db::settings_repo::SettingsRepo::with_backend(db.clone()),
    );
    let mut zones = crate::zones::groupes_enregistres::zones_compagnes(&groupes, zone_id);
    zones.push(zone_id);
    zones.sort_unstable();
    zones.dedup();
    zones
}

/// Les colonnes d'une ligne de station recopiées à la relance d'une session
/// (`listened_at` prend l'heure de la copie, `zone_id` celle qui note).
const COLONNES_RECOPIEES: &str = "track_id, title, artist_name, album_title, source, \
     source_id, album_id, duration_ms, cover_url, profile_id, context_type, context_id, \
     context_position, context_source, context_title, context_cover, album_ref";

/// La ligne de station sous laquelle un titre de `source_id`, sur `zone_id`,
/// se range, ou `None`.
///
/// * La DERNIÈRE ligne d'historique du groupe de la zone (#8107 : une seule
///   zone du groupe écrit la ligne de station), si c'est la station du même
///   flux. Sinon rien : la zone joue autre chose, ou son écoute n'est pas
///   encore confirmée.
/// * Si cette ligne est PÉRIMÉE — ni écrite ni enrichie d'un titre depuis
///   [`FRAICHEUR_MS`] (une radio reprise le lendemain sans nouveau lancement,
///   une relance par `play_without_history`) — une nouvelle session commence :
///   la ligne est recopiée à l'heure présente et les titres se rangent sous
///   la copie, jamais sous la veille.
pub fn station_en_cours(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    source_id: &str,
    maintenant_ms: i64,
) -> Result<Option<i64>, String> {
    let zones = zones_du_groupe(db, zone_id);
    let marques = vec!["?"; zones.len()].join(", ");
    let params: Vec<&dyn ToSqlValue> = zones.iter().map(|z| z as &dyn ToSqlValue).collect();
    let ligne = db.query_one(
        &format!(
            "SELECT id, source, source_id, context_type, listened_at FROM listen_history \
             WHERE zone_id IN ({marques}) ORDER BY id DESC LIMIT 1"
        ),
        &params,
    )?;
    let Some(ligne) = ligne else {
        return Ok(None);
    };
    let Some(id) = ligne.first().and_then(|v| v.as_i64()) else {
        return Ok(None);
    };
    let source = ligne.get(1).and_then(|v| v.as_string());
    let flux = ligne.get(2).and_then(|v| v.as_string());
    let nature = ligne.get(3).and_then(|v| v.as_string());
    if source.as_deref() != Some("radio")
        || nature.as_deref() != Some("radio")
        || flux.as_deref() != Some(source_id)
    {
        return Ok(None);
    }
    let ecrite = ligne
        .get(4)
        .and_then(|v| v.as_string())
        .and_then(|t| instant_ms(&t));
    let dernier_titre = db
        .query_one(
            "SELECT MAX(diffuse_le) FROM radio_titres_diffuses WHERE ecoute_id = ?",
            &[&id as &dyn ToSqlValue],
        )?
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    let activite = match (ecrite, dernier_titre) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    // Date illisible et aucun titre : on ne relance rien sur une supposition.
    let perimee = activite.is_some_and(|a| maintenant_ms - a > FRAICHEUR_MS);
    if !perimee {
        return Ok(Some(id));
    }
    let copie = db.execute_returning_id(
        &format!(
            "INSERT INTO listen_history ({COLONNES_RECOPIEES}, zone_id) \
             SELECT {COLONNES_RECOPIEES}, ? FROM listen_history WHERE id = ?"
        ),
        &[&zone_id as &dyn ToSqlValue, &id],
    )?;
    tracing::info!(
        zone_id,
        ancienne = id,
        nouvelle = copie,
        "radio_session_relancee"
    );
    Ok(Some(copie))
}

/// Note le titre que le flux de `source_id` annonce sur `zone_id`, sous la
/// ligne de sa station — ou dit pourquoi il ne l'est pas.
///
/// Appelée par le sondeur à chaque lecture des métadonnées d'une radio QUI
/// JOUE (`poller::radio`), que le titre ait changé ou non : un titre lu avant
/// que la station soit notée (zone navigateur pas encore confirmée) l'est au
/// tour suivant. Dans un groupe de zones, seule la plus petite zone note :
/// les autres liraient le même flux.
///
/// Au-delà de [`TITRES_GARDES`] titres sous une même ligne, les plus anciens
/// sont effacés : une radio qui joue jour et nuit ne fait pas grossir la
/// table sans fin.
// Un argument par colonne écrite, comme `record_listen`.
#[allow(clippy::too_many_arguments)]
pub fn noter(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    source_id: &str,
    station: &str,
    artiste: Option<&str>,
    titre: &str,
    pochette: Option<&str>,
    maintenant_ms: i64,
) -> Result<Notation, String> {
    if !titre_a_noter(station, artiste, titre) {
        return Ok(Notation::PasUnTitre);
    }
    if zones_du_groupe(db, zone_id).first() != Some(&zone_id) {
        return Ok(Notation::ZoneCompagne);
    }
    let Some(ecoute_id) = station_en_cours(db, zone_id, source_id, maintenant_ms)? else {
        return Ok(Notation::PasDeStation);
    };
    let derniers = db.query_many(
        "SELECT titre, artiste FROM radio_titres_diffuses \
         WHERE ecoute_id = ? ORDER BY diffuse_le DESC LIMIT ?",
        &[&ecoute_id as &dyn ToSqlValue, &TITRES_COMPARES],
    )?;
    let deja = derniers.iter().any(|r| {
        let t = r.first().and_then(|v| v.as_string()).unwrap_or_default();
        let a = r.get(1).and_then(|v| v.as_string());
        meme_titre(a.as_deref(), &t, artiste, titre)
    });
    if deja {
        return Ok(Notation::DejaNote);
    }
    let titre = titre.trim();
    let artiste = artiste.map(str::trim).filter(|a| !a.is_empty());
    db.execute(
        "INSERT INTO radio_titres_diffuses \
         (ecoute_id, diffuse_le, zone_id, flux, station, titre, artiste, pochette) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (ecoute_id, diffuse_le) DO NOTHING",
        &[
            &ecoute_id as &dyn ToSqlValue,
            &maintenant_ms,
            &zone_id,
            &source_id,
            &station,
            &titre,
            &artiste,
            &pochette,
        ],
    )?;
    let decalage = TITRES_GARDES - 1;
    db.execute(
        "DELETE FROM radio_titres_diffuses WHERE ecoute_id = ? AND diffuse_le < \
         (SELECT diffuse_le FROM radio_titres_diffuses WHERE ecoute_id = ? \
          ORDER BY diffuse_le DESC LIMIT 1 OFFSET ?)",
        &[&ecoute_id as &dyn ToSqlValue, &ecoute_id, &decalage],
    )?;
    Ok(Notation::Ecrit)
}

/// Les titres diffusés sous chacune des lignes de station `ecoutes`, le plus
/// récent d'abord, [`TITRES_PAR_STATION`] au plus par ligne — borne posée
/// EN SQL (`ROW_NUMBER`, SQLite ≥ 3.25 et PostgreSQL) : le client web relit
/// l'historique toutes les quelques secondes.
pub fn pour_ecoutes(
    db: &Arc<dyn DbBackend>,
    ecoutes: &[i64],
) -> Result<HashMap<i64, Vec<TitreDiffuse>>, String> {
    let mut par_ecoute: HashMap<i64, Vec<TitreDiffuse>> = HashMap::new();
    if ecoutes.is_empty() {
        return Ok(par_ecoute);
    }
    let marques = vec!["?"; ecoutes.len()].join(", ");
    let plafond = TITRES_PAR_STATION as i64;
    let mut params: Vec<&dyn ToSqlValue> = ecoutes.iter().map(|i| i as &dyn ToSqlValue).collect();
    params.push(&plafond);
    let lignes = db.query_many(
        &format!(
            "SELECT ecoute_id, diffuse_le, zone_id, titre, artiste, pochette FROM ( \
               SELECT ecoute_id, diffuse_le, zone_id, titre, artiste, pochette, \
                      ROW_NUMBER() OVER (PARTITION BY ecoute_id ORDER BY diffuse_le DESC) AS rang \
               FROM radio_titres_diffuses WHERE ecoute_id IN ({marques})) t \
             WHERE rang <= ? ORDER BY ecoute_id, diffuse_le DESC"
        ),
        &params,
    )?;
    for r in lignes {
        let (Some(ecoute_id), Some(diffuse_le)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        par_ecoute.entry(ecoute_id).or_default().push(TitreDiffuse {
            ecoute_id,
            diffuse_le,
            zone_id: r.get(2).and_then(|v| v.as_i64()),
            titre: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
            artiste: r.get(4).and_then(|v| v.as_string()),
            pochette: r.get(5).and_then(|v| v.as_string()),
            diffuse_a: date_iso(diffuse_le),
        });
    }
    Ok(par_ecoute)
}

/// Efface tous les titres diffusés — avec l'historique (`HistoryRepo::clear`).
pub fn effacer_tout(db: &Arc<dyn DbBackend>) -> Result<(), String> {
    db.execute("DELETE FROM radio_titres_diffuses", &[])?;
    Ok(())
}

#[cfg(test)]
#[path = "radio_titres_diffuses_tests_8487.rs"]
mod tests;
