//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! DISCOGS par code-barres ou numéro de catalogue, quand MusicBrainz ne
//! trouve rien (#4805, idée 10 de l'analyse de MetaRust).
//!
//! # Des PROPOSITIONS, jamais une écriture d'office
//!
//! MetaRust fusionnait les crédits et l'année de Discogs dans la release
//! (`merge_into_release`), sans écraser MusicBrainz. Tune va plus loin dans la
//! prudence : une recherche Discogs n'écrit RIEN sur l'album. Elle range une
//! proposition dans `propositions_discogs` ; l'utilisateur l'accepte ou la
//! rejette sur la fiche de l'album. Accepter ne remplit que les champs VIDES
//! (année, label, numéro de catalogue) et jamais un champ tenu à la main
//! (`album_repo::sql_champ_tenu_a_la_main`) ; les crédits Discogs ne vont que
//! sur les pistes qui n'en ont aucun, et seulement si la tracklist s'apparie
//! sans doute (même nombre de pistes, même place, titres concordants — la
//! règle de `credits_release::apparier`).
//!
//! Un album qui porte un pressage MusicBrainz n'est pas proposé : ses données
//! viennent de MusicBrainz, et Discogs ne les remplace pas.
//!
//! # Repris, corrigé
//!
//! * **Repris** : `pick_discogs_hit` (un code sûr = un seul résultat, ou un
//!   seul CD), `parse_search`, `parse_discogs_release`, la traduction des
//!   rôles (`map_discogs_roles`, `split_role_list`, `skip_role`…) et
//!   `parse_tracks_field`.
//! * **Corrigé** : MetaRust appariait les pistes par numéro SANS le disque —
//!   faux sur un album multi-disque (relevé de l'analyse). Ici la position
//!   Discogs `2-3` / `CD2-3` / `2.3` donne (disque 2, piste 3).
//! * **Réécrit** : le client. Le transport est celui de Tune — le client HTTP
//!   partagé que les portraits d'artistes utilisent déjà, sous le limiteur
//!   PARTAGÉ [`crate::http::fetch::DISCOGS`], avec le jeton résolu par
//!   [`crate::cles_api`] (personnel d'abord).
//!
//! Les conditions de Discogs demandent l'attribution : chaque proposition
//! porte le lien de la fiche Discogs, que l'interface affiche.

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use super::credits_mb::LigneCredit;
use super::credits_release::{PisteMb, apparier, ecrire_credits_piste, pistes_locales};
use crate::db::backend::{DbBackend, ToSqlValue};

pub const BASE_DISCOGS: &str = "https://api.discogs.com";

// ── Recherche (repris) ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub id: u64,
    pub title: String,
    pub formats: Vec<String>,
}

pub fn parse_search(json: &Value) -> Vec<SearchHit> {
    let Some(results) = json.get("results").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_u64()?;
            let title = r.get("title")?.as_str()?.to_string();
            let formats = r
                .get("format")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Some(SearchHit { id, title, formats })
        })
        .collect()
}

/// Un résultat SÛR pour un code-barres ou un numéro de catalogue : un seul
/// résultat, ou un seul CD parmi plusieurs (MetaRust : `pick_discogs_hit`,
/// branche `barcode_or_catno`). Plusieurs candidats : on ne choisit pas.
pub fn pick_discogs_hit(hits: &[SearchHit]) -> Option<&SearchHit> {
    if hits.len() == 1 {
        return Some(&hits[0]);
    }
    let cds: Vec<_> = hits
        .iter()
        .filter(|h| {
            h.formats
                .iter()
                .any(|f| f.to_ascii_lowercase().contains("cd"))
        })
        .collect();
    if cds.len() == 1 {
        return Some(cds[0]);
    }
    None
}

// ── Rôles (repris) ──────────────────────────────────────────────────────────

/// Un crédit Discogs traduit dans le vocabulaire de `track_credits`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditDiscogs {
    pub nom: String,
    /// `composer`, `writer`, `arranger`, `conductor`, `producer`, `engineer`,
    /// `mixer`, `mastering`, `remixer`, `vocal`, `performer`.
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<String>,
}

fn split_role_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                cur.push(c);
            }
            ']' => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            ',' if depth == 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

fn split_brackets(chunk: &str) -> (String, Vec<String>) {
    if let (Some(start), Some(end)) = (chunk.find('['), chunk.rfind(']'))
        && end > start
    {
        let role = chunk[..start].trim().to_string();
        let attrs = chunk[start + 1..end]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return (role, attrs);
    }
    (chunk.trim().to_string(), Vec::new())
}

fn skip_role(key: &str) -> bool {
    matches!(
        key,
        "photography by"
            | "photography"
            | "artwork"
            | "artwork by"
            | "design"
            | "design by"
            | "illustration"
            | "cover"
            | "cover design"
            | "layout"
            | "liner notes"
            | "liner notes by"
            | "legal"
            | "a&r"
            | "management"
            | "booking"
            | "painting"
            | "graphics"
            | "translated by"
            | "copyright"
            | "phonographic copyright"
            | "lacquer cut by"
    )
}

/// Un rôle Discogs : `(rôle Tune, instrument)`. `None` pour les rôles hors
/// musique (photographie, pochette, juridique).
fn map_one_role(chunk: &str) -> Option<(&'static str, Option<String>)> {
    let (role, attrs) = split_brackets(chunk);
    let key = role.to_ascii_lowercase();
    let key = key.trim().trim_end_matches('.');
    if key.is_empty() || skip_role(key) {
        return None;
    }
    let mapped = match key {
        "music by" | "composed by" | "composer" => "composer",
        "written-by" | "written by" | "lyrics by" | "words by" | "lyricist" | "songwriter" => {
            "writer"
        }
        "arranged by" | "arranger" | "orchestrated by" => "arranger",
        "conductor" | "directed by" | "director" | "musical director" | "music director" => {
            "conductor"
        }
        "producer" | "co-producer" | "executive-producer" | "executive producer" => "producer",
        "engineer" | "recording engineer" | "recorded by" => "engineer",
        "mixed by" | "mix" | "mixing" => "mixer",
        "mastered by" | "mastering" => "mastering",
        "remix" | "remixer" | "remixes" => "remixer",
        "vocals" | "lead vocals" | "backing vocals" | "featuring" | "feat" | "guest" | "chorus"
        | "voice" => "vocal",
        "performer" | "performed by" => "performer",
        // Tout autre libellé de musicien est un INSTRUMENT (« Alto Saxophone »,
        // « Double Bass ») : MetaRust le range sous `Instrument(libellé)`.
        other => {
            return Some((
                "performer",
                Some(super::instruments::canoniser_instrument(other)),
            ));
        }
    };
    // Un attribut entre crochets (« [Soloist] ») qualifie, il ne nomme pas
    // d'instrument : il n'est pas repris.
    let _ = attrs;
    Some((mapped, None))
}

/// Les rôles d'un champ `role` Discogs : « Written-By, Alto Saxophone
/// [Soloist], Photography By » → auteur + saxophone alto.
pub fn map_discogs_roles(raw: &str) -> Vec<(&'static str, Option<String>)> {
    split_role_list(raw)
        .into_iter()
        .filter_map(|chunk| map_one_role(&chunk))
        .collect()
}

/// `None` = toutes les pistes (champ vide). `Some` = positions 1-based.
pub fn parse_tracks_field(raw: &str) -> Option<Vec<u32>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.chars().any(|c| c.is_ascii_alphabetic()) {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in raw.split([',', ';', '&']) {
        let part = part.trim().to_ascii_lowercase().replace(" to ", "-");
        if let Some((a, b)) = part.split_once('-') {
            let (Ok(start), Ok(end)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) else {
                continue;
            };
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            out.extend(lo..=hi);
        } else if let Ok(n) = part.parse::<u32>() {
            out.push(n);
        }
    }
    Some(out)
}

// ── Release (repris, avec le disque) ────────────────────────────────────────

/// Une piste Discogs, avec SON disque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PisteDiscogs {
    pub disque: u32,
    pub numero: u32,
    /// La position telle que Discogs l'écrit (`2-3`, `B1`).
    pub position: String,
    pub titre: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credits: Vec<CreditDiscogs>,
}

/// Ce que Discogs propose pour un album.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposition {
    pub discogs_id: u64,
    /// `barcode` ou `catno`.
    pub methode: String,
    /// Le code qui a trouvé la fiche.
    pub code: String,
    pub titre: Option<String>,
    pub annee: Option<i64>,
    pub date: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub credits_album: Vec<CreditDiscogs>,
    pub pistes: Vec<PisteDiscogs>,
    /// La fiche Discogs (attribution).
    pub url: String,
}

/// La position Discogs en (disque, piste). MetaRust ne gardait que le
/// numéro ; c'est ce qui trompait sur un album multi-disque.
pub fn parse_position(raw: &str, rang: u32) -> (u32, u32) {
    let r = raw.trim();
    if let Ok(n) = r.parse::<u32>() {
        return (1, n);
    }
    // « CD2-3 », « 2-3 », « 2.3 », « 2/3 ».
    let sans_prefixe = r
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim();
    for sep in ['-', '.', '/'] {
        if let Some((d, n)) = sans_prefixe.split_once(sep)
            && let (Ok(d), Ok(n)) = (d.trim().parse::<u32>(), n.trim().parse::<u32>())
        {
            return (d.max(1), n);
        }
    }
    // Vinyle (« A1 », « B3 ») : une face n'est pas un disque ; la piste prend
    // son rang dans la tracklist.
    (1, rang)
}

fn credits_de(node: Option<&Value>) -> Vec<CreditDiscogs> {
    let mut out: Vec<CreditDiscogs> = Vec::new();
    for item in node.and_then(Value::as_array).into_iter().flatten() {
        let nom = item
            .get("anv")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| item.get("name").and_then(Value::as_str))
            .unwrap_or("")
            .trim();
        if nom.is_empty() {
            continue;
        }
        // « Miles Davis (2) » : Discogs numérote les homonymes.
        let nom = nom
            .rsplit_once(" (")
            .filter(|(_, n)| n.trim_end_matches(')').chars().all(|c| c.is_ascii_digit()))
            .map(|(a, _)| a)
            .unwrap_or(nom)
            .to_string();
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        for (r, instrument) in map_discogs_roles(role) {
            let c = CreditDiscogs {
                nom: nom.clone(),
                role: r.to_string(),
                instrument,
            };
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

fn annee_de(json: &Value) -> (Option<i64>, Option<String>) {
    let date = json
        .get("released")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| t.len() >= 4 && t.as_bytes()[..4].iter().all(u8::is_ascii_digit))
        .map(|t| t.replace("-00", "").to_string());
    let annee = date
        .as_deref()
        .and_then(|d| d[..4].parse::<i64>().ok())
        .or_else(|| json.get("year").and_then(Value::as_i64))
        .filter(|y| (1900..2100).contains(y));
    (annee, date)
}

/// Une réponse `GET /releases/{id}` en proposition.
pub fn parse_discogs_release(json: &Value, methode: &str, code: &str) -> Option<Proposition> {
    let discogs_id = json.get("id").and_then(Value::as_u64)?;
    let premier_label = json
        .get("labels")
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    let (annee, date) = annee_de(json);
    let mut p = Proposition {
        discogs_id,
        methode: methode.to_string(),
        code: code.to_string(),
        titre: json
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        annee,
        date,
        label: premier_label
            .and_then(|l| l.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        catalog_number: premier_label
            .and_then(|l| l.get("catno"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
            .map(str::to_string),
        credits_album: Vec::new(),
        pistes: Vec::new(),
        url: json
            .get("uri")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://www.discogs.com/release/{discogs_id}")),
    };
    let mut rang = 0u32;
    for item in json
        .get("tracklist")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = item.get("type_").and_then(Value::as_str).unwrap_or("track");
        if kind != "track" {
            continue;
        }
        rang += 1;
        let position = item
            .get("position")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (disque, numero) = parse_position(&position, rang);
        p.pistes.push(PisteDiscogs {
            disque,
            numero,
            position,
            titre: item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            credits: credits_de(item.get("extraartists")),
        });
    }
    // Les crédits de la release : « tracks » vide = tout l'album ; sinon les
    // positions listées (sur la numérotation Discogs, disque 1).
    for item in json
        .get("extraartists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let credits = credits_de(Some(&Value::Array(vec![item.clone()])));
        let champ = item.get("tracks").and_then(Value::as_str).unwrap_or("");
        match parse_tracks_field(champ) {
            None => {
                for c in credits {
                    if !p.credits_album.contains(&c) {
                        p.credits_album.push(c);
                    }
                }
            }
            Some(positions) => {
                for pos in positions {
                    // Le champ « tracks » compte dans la tracklist entière :
                    // la n-ième piste, quel que soit son disque.
                    if let Some(piste) = p.pistes.get_mut(pos.saturating_sub(1) as usize) {
                        for c in &credits {
                            if !piste.credits.contains(c) {
                                piste.credits.push(c.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    Some(p)
}

// ── La recherche, transport en paramètre ────────────────────────────────────

/// Pourquoi aucune proposition n'a été cherchée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Empechement {
    AlbumInconnu,
    /// L'album a un pressage MusicBrainz : Discogs ne le remplace pas.
    DejaIdentifie,
    /// Ni code-barres ni numéro de catalogue.
    SansCode,
    /// Discogs n'a pas répondu.
    Panne,
}

/// L'album tel que la recherche le lit.
#[derive(Debug, Clone, Default)]
struct AlbumLu {
    mbid: Option<String>,
    barcode: Option<String>,
    catalog_number: Option<String>,
}

fn lire_album(backend: &Arc<dyn DbBackend>, album_id: i64) -> Option<AlbumLu> {
    let r = backend
        .query_one(
            "SELECT musicbrainz_release_id, barcode, catalog_number FROM albums WHERE id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    let txt = |i: usize| {
        r.get(i)
            .and_then(|v| v.as_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some(AlbumLu {
        mbid: txt(0),
        barcode: txt(1),
        catalog_number: txt(2),
    })
}

/// Un code-barres réduit à ses chiffres (« 0 77774-6001-2 7 » → `077774600127`).
pub fn chiffres_du_code_barres(code: &str) -> Option<String> {
    let c: String = code.chars().filter(char::is_ascii_digit).collect();
    (c.len() >= 8).then_some(c)
}

pub fn url_recherche(champ: &str, valeur: &str) -> String {
    format!(
        "{BASE_DISCOGS}/database/search?type=release&{champ}={}",
        urlencoding::encode(valeur)
    )
}

pub fn url_release(id: u64) -> String {
    format!("{BASE_DISCOGS}/releases/{id}")
}

/// Cherche sur Discogs, par code-barres puis par numéro de catalogue, et
/// GARDE la proposition trouvée. N'écrit rien sur l'album.
///
/// `get(url)` est le transport : `Err` = panne, `Ok(None)` = 404.
pub async fn chercher_par<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    get: F,
) -> Result<Option<Proposition>, Empechement>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Option<Value>, String>>,
{
    let album = lire_album(backend, album_id).ok_or(Empechement::AlbumInconnu)?;
    if album.mbid.is_some() {
        return Err(Empechement::DejaIdentifie);
    }
    let mut essais: Vec<(&str, String)> = Vec::new();
    if let Some(b) = album.barcode.as_deref().and_then(chiffres_du_code_barres) {
        essais.push(("barcode", b));
    }
    if let Some(c) = album.catalog_number.clone() {
        essais.push(("catno", c));
    }
    if essais.is_empty() {
        return Err(Empechement::SansCode);
    }
    for (methode, code) in essais {
        let recherche = get(url_recherche(methode, &code))
            .await
            .map_err(|_| Empechement::Panne)?;
        let hits = recherche.as_ref().map(parse_search).unwrap_or_default();
        let Some(hit) = pick_discogs_hit(&hits) else {
            continue;
        };
        let Some(fiche) = get(url_release(hit.id))
            .await
            .map_err(|_| Empechement::Panne)?
        else {
            continue;
        };
        if let Some(p) = parse_discogs_release(&fiche, methode, &code) {
            garder(backend, album_id, &p);
            info!(
                album_id,
                discogs_id = p.discogs_id,
                methode,
                "discogs_proposition"
            );
            return Ok(Some(p));
        }
    }
    Ok(None)
}

// ── En base ─────────────────────────────────────────────────────────────────

/// Le format des dates de `propositions_discogs` : il se compare en texte.
const FORMAT_DATE: &str = "%Y-%m-%dT%H:%M:%SZ";

fn maintenant() -> String {
    chrono::Utc::now().format(FORMAT_DATE).to_string()
}

/// Au-delà, une proposition en attente est périmée (conditions de Discogs).
pub const FRAICHEUR_HEURES: i64 = 6;

/// La date avant laquelle une proposition en attente est périmée, au format
/// de la table (comparable en texte).
pub fn seuil_de_fraicheur(maintenant: chrono::DateTime<chrono::Utc>) -> String {
    (maintenant - chrono::Duration::hours(FRAICHEUR_HEURES))
        .format(FORMAT_DATE)
        .to_string()
}

/// Garde une proposition. Une proposition déjà TRANCHÉE (acceptée, rejetée)
/// n'est pas remise en attente : une nouvelle recherche ne la ressuscite pas.
pub fn garder(backend: &Arc<dyn DbBackend>, album_id: i64, p: &Proposition) {
    let id = p.discogs_id as i64;
    let corps = serde_json::to_string(p).unwrap_or_default();
    let quand = maintenant();
    if let Err(e) = backend.execute(
        "INSERT INTO propositions_discogs \
         (album_id, discogs_release_id, methode, proposition, statut, cree_le) \
         VALUES (?, ?, ?, ?, 'proposee', ?) \
         ON CONFLICT (album_id, discogs_release_id) DO UPDATE SET \
         proposition = excluded.proposition, methode = excluded.methode, \
         cree_le = excluded.cree_le \
         WHERE propositions_discogs.statut = 'proposee'",
        &[
            &album_id as &dyn ToSqlValue,
            &id as &dyn ToSqlValue,
            &p.methode as &dyn ToSqlValue,
            &corps as &dyn ToSqlValue,
            &quand as &dyn ToSqlValue,
        ],
    ) {
        warn!(album_id, erreur = %e, "discogs_proposition_ecriture_impossible");
    }
}

/// Une proposition et son sort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PropositionGardee {
    pub statut: String,
    pub cree_le: String,
    pub decide_le: Option<String>,
    /// En attente et lue il y a plus de [`FRAICHEUR_HEURES`] heures : son
    /// contenu ne s'affiche plus et elle ne s'accepte plus (voir l'en-tête).
    pub perimee: bool,
    pub proposition: Proposition,
}

pub fn lister(backend: &Arc<dyn DbBackend>, album_id: i64) -> Vec<PropositionGardee> {
    lister_au(backend, album_id, chrono::Utc::now())
}

/// [`lister`] à une date donnée (les témoins avancent l'horloge).
pub fn lister_au(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    maintenant: chrono::DateTime<chrono::Utc>,
) -> Vec<PropositionGardee> {
    let seuil = seuil_de_fraicheur(maintenant);
    backend
        .query_many(
            "SELECT proposition, statut, cree_le, decide_le FROM propositions_discogs \
             WHERE album_id = ? ORDER BY cree_le DESC",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let statut = r.get(1)?.as_string()?;
            let cree_le = r.get(2)?.as_string()?;
            Some(PropositionGardee {
                proposition: serde_json::from_str(&r.first()?.as_string()?).ok()?,
                perimee: statut == "proposee" && cree_le < seuil,
                statut,
                cree_le,
                decide_le: r.get(3).and_then(|v| v.as_string()),
            })
        })
        .collect()
}

fn lire_une(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    discogs_id: u64,
) -> Option<PropositionGardee> {
    lister(backend, album_id)
        .into_iter()
        .find(|p| p.proposition.discogs_id == discogs_id)
}

fn decider(backend: &Arc<dyn DbBackend>, album_id: i64, discogs_id: u64, statut: &str) {
    let id = discogs_id as i64;
    let quand = maintenant();
    backend
        .execute(
            "UPDATE propositions_discogs SET statut = ?, decide_le = ? \
             WHERE album_id = ? AND discogs_release_id = ?",
            &[
                &statut as &dyn ToSqlValue,
                &quand as &dyn ToSqlValue,
                &album_id as &dyn ToSqlValue,
                &id as &dyn ToSqlValue,
            ],
        )
        .ok();
}

/// Ce que l'acceptation a posé, et ce qu'elle a laissé.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BilanAcceptation {
    /// Champs d'album remplis (ils étaient vides et non tenus).
    pub champs_poses: Vec<String>,
    /// Champs déjà renseignés ou tenus à la main : laissés tels quels.
    pub champs_gardes: Vec<String>,
    /// Pistes qui ont reçu les crédits Discogs (elles n'en avaient aucun).
    pub pistes_creditees: usize,
}

/// Remplit UN champ d'album s'il est vide et non tenu. Rend `true` s'il l'a
/// posé.
fn poser_si_vide(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    champ: &str,
    vide: &str,
    valeur: &dyn ToSqlValue,
) -> bool {
    let tenu = crate::db::album_repo::sql_champ_tenu_a_la_main(champ);
    let sql = format!("UPDATE albums SET {champ} = ? WHERE id = ? AND ({vide}) AND NOT {tenu}");
    backend
        .execute(&sql, &[valeur, &album_id as &dyn ToSqlValue])
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Accepte une proposition : remplit les champs vides, et — si demandé — les
/// crédits des pistes qui n'en ont aucun. Jamais par-dessus une valeur.
pub fn accepter(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    discogs_id: u64,
    avec_credits: bool,
) -> Result<BilanAcceptation, String> {
    let gardee = lire_une(backend, album_id, discogs_id).ok_or("unknown_proposal")?;
    if gardee.statut != "proposee" {
        return Err("already_decided".into());
    }
    // Conditions de Discogs : une donnée de plus de six heures ne s'affiche
    // plus, elle ne s'accepte donc pas non plus. Une nouvelle recherche la
    // rafraîchit.
    if gardee.perimee {
        return Err("proposition_perimee".into());
    }
    let p = gardee.proposition;
    let mut bilan = BilanAcceptation::default();
    let mut champ = |nom: &str, vide: &str, valeur: Option<&dyn ToSqlValue>| {
        if let Some(v) = valeur {
            if poser_si_vide(backend, album_id, nom, vide, v) {
                bilan.champs_poses.push(nom.to_string());
            } else {
                bilan.champs_gardes.push(nom.to_string());
            }
        }
    };
    champ(
        "year",
        "year IS NULL OR year = 0",
        p.annee.as_ref().map(|a| a as &dyn ToSqlValue),
    );
    champ(
        "label",
        "label IS NULL OR label = ''",
        p.label.as_ref().map(|l| l as &dyn ToSqlValue),
    );
    champ(
        "catalog_number",
        "catalog_number IS NULL OR catalog_number = ''",
        p.catalog_number.as_ref().map(|c| c as &dyn ToSqlValue),
    );
    if avec_credits {
        bilan.pistes_creditees = crediter_les_pistes_sans_credit(backend, album_id, &p);
    }
    decider(backend, album_id, discogs_id, "acceptee");
    info!(album_id, discogs_id, ?bilan, "discogs_proposition_acceptee");
    Ok(bilan)
}

pub fn rejeter(backend: &Arc<dyn DbBackend>, album_id: i64, discogs_id: u64) -> Result<(), String> {
    let gardee = lire_une(backend, album_id, discogs_id).ok_or("unknown_proposal")?;
    if gardee.statut != "proposee" {
        return Err("already_decided".into());
    }
    decider(backend, album_id, discogs_id, "rejetee");
    Ok(())
}

fn nombre_de_credits(backend: &Arc<dyn DbBackend>, track_id: i64) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM track_credits WHERE track_id = ?",
            &[&track_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Les crédits Discogs sur les pistes qui n'en ont AUCUN, appariées par la
/// règle stricte de `credits_release::apparier` (même nombre de pistes, même
/// place, titres concordants).
fn crediter_les_pistes_sans_credit(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    p: &Proposition,
) -> usize {
    let lignes = |piste: &PisteDiscogs| -> Vec<LigneCredit> {
        p.credits_album
            .iter()
            .chain(piste.credits.iter())
            .map(|c| LigneCredit {
                artist_name: c.nom.clone(),
                role: c.role.clone(),
                instrument: c.instrument.clone(),
                artist_mbid: None,
            })
            .collect()
    };
    let mb: Vec<PisteMb> = p
        .pistes
        .iter()
        .map(|t| PisteMb {
            disque: t.disque as i64,
            numero: t.numero as i64,
            titre: t.titre.clone(),
            recording_mbid: String::new(),
            lignes: lignes(t),
        })
        .collect();
    let locales: Vec<_> = pistes_locales(backend, album_id)
        .into_iter()
        .map(|mut l| {
            // Pas de MBID côté Discogs : l'appariement se fait par la place.
            l.recording_mbid = None;
            l
        })
        .collect();
    let mut n = 0;
    for (track_id, i) in apparier(&mb, &locales) {
        if mb[i].lignes.is_empty() || nombre_de_credits(backend, track_id) > 0 {
            continue;
        }
        if ecrire_credits_piste(backend, track_id, &mb[i].lignes) > 0 {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_ecrits_et_cuivres() {
        let roles = map_discogs_roles("Written-By, Alto Saxophone [Soloist], Photography By");
        assert_eq!(roles.len(), 2);
        assert!(roles.iter().any(|(r, _)| *r == "writer"));
        assert!(roles.iter().any(
            |(r, i)| *r == "performer" && i.as_deref().is_some_and(|x| x.contains("saxophone"))
        ));
    }

    #[test]
    fn tracks_intervalle() {
        assert_eq!(parse_tracks_field(""), None);
        assert_eq!(parse_tracks_field("4").unwrap(), vec![4]);
        assert_eq!(parse_tracks_field("3-5").unwrap(), vec![3, 4, 5]);
        assert!(parse_tracks_field("A4").unwrap().is_empty());
    }

    /// Le défaut de MetaRust corrigé : la position porte le disque.
    #[test]
    fn la_position_discogs_porte_le_disque() {
        assert_eq!(parse_position("3", 3), (1, 3));
        assert_eq!(parse_position("2-3", 9), (2, 3));
        assert_eq!(parse_position("CD2-3", 9), (2, 3));
        assert_eq!(parse_position("2.4", 9), (2, 4));
        assert_eq!(parse_position("B1", 5), (1, 5));
    }

    #[test]
    fn un_code_sur_ne_donne_qu_un_resultat() {
        let un = vec![SearchHit {
            id: 9,
            title: "X - Y".into(),
            formats: vec!["Vinyl".into()],
        }];
        assert_eq!(pick_discogs_hit(&un).unwrap().id, 9);
        let deux_cd = vec![
            SearchHit {
                id: 1,
                title: "A".into(),
                formats: vec!["CD".into()],
            },
            SearchHit {
                id: 2,
                title: "A".into(),
                formats: vec!["CD".into()],
            },
        ];
        assert!(pick_discogs_hit(&deux_cd).is_none());
        let un_cd = vec![
            SearchHit {
                id: 1,
                title: "A".into(),
                formats: vec!["Vinyl".into()],
            },
            SearchHit {
                id: 2,
                title: "A".into(),
                formats: vec!["CD".into(), "Album".into()],
            },
        ];
        assert_eq!(pick_discogs_hit(&un_cd).unwrap().id, 2);
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn fixture(nom: &str) -> Value {
        let chemin = format!(
            "{}/tests/fixtures/discogs/{nom}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(chemin).unwrap()).unwrap()
    }

    /// Le transport rejoue une fiche Discogs RÉELLE, enregistrée le
    /// 12/10/2026 (`GET /releases/249504`, l'exemple de la documentation de
    /// l'API : Rick Astley, « Never Gonna Give You Up », RCA PB 41447, 45 t).
    /// La réponse de recherche suit la forme documentée de
    /// `/database/search` (elle exige un jeton : elle n'a pas été rejouée).
    /// Toute autre URL est une panne, donc un échec visible.
    fn transport_enregistre(url: String) -> std::future::Ready<Result<Option<Value>, String>> {
        let v = if url.contains("/database/search") && url.contains("barcode=5012394144777") {
            json!({"pagination": {"page": 1, "pages": 1, "per_page": 50, "items": 1}, "results": [
                {"id": 249504, "type": "release", "title": "Rick Astley - Never Gonna Give You Up",
                 "format": ["Vinyl", "7\"", "45 RPM", "Single"], "catno": "PB 41447"}
            ]})
        } else if url.ends_with("/releases/249504") {
            fixture("release_249504.json")
        } else {
            return std::future::ready(Err(format!("URL inattendue : {url}")));
        };
        std::future::ready(Ok(Some(v)))
    }

    fn album_sans_mbid(b: &Arc<dyn DbBackend>) {
        b.execute(
            "INSERT INTO albums (id, title, barcode, year, label) \
             VALUES (5, 'Never Gonna Give You Up', '5 012394 144777', NULL, 'Label saisi')",
            &[],
        )
        .unwrap();
        for (id, n, t) in [
            (50, 1, "Never Gonna Give You Up"),
            (51, 2, "Never Gonna Give You Up (Instrumental)"),
        ] {
            b.execute(
                "INSERT INTO tracks (id, title, album_id, disc_number, track_number, file_path) \
                 VALUES (?, ?, 5, 1, ?, ?)",
                &[
                    &(id as i64) as &dyn ToSqlValue,
                    &t as &dyn ToSqlValue,
                    &(n as i64) as &dyn ToSqlValue,
                    &format!("/m/{id}.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn la_fiche_discogs_reelle_se_lit() {
        let p = parse_discogs_release(&fixture("release_249504.json"), "barcode", "5012394144777")
            .unwrap();
        assert_eq!(p.annee, Some(1987));
        assert_eq!(p.label.as_deref(), Some("RCA"));
        assert_eq!(p.catalog_number.as_deref(), Some("PB 41447"));
        // Faces A et B d'un 45 t : un seul disque, pistes 1 et 2.
        assert_eq!(
            p.pistes
                .iter()
                .map(|t| (t.disque, t.numero))
                .collect::<Vec<_>>(),
            vec![(1, 1), (1, 2)]
        );
        // Le nom CRÉDITÉ (`anv`) passe devant le nom de la fiche, comme chez
        // MetaRust : « Stock / Aitken / Waterman » sur cette pochette.
        let roles: Vec<(&str, &str)> = p
            .credits_album
            .iter()
            .map(|c| (c.nom.as_str(), c.role.as_str()))
            .collect();
        assert!(
            roles.contains(&("Stock / Aitken / Waterman", "producer")),
            "{roles:?}"
        );
        assert!(
            roles.contains(&("Stock / Aitken / Waterman", "writer")),
            "{roles:?}"
        );
        assert!(
            roles.contains(&("Mixmaster Pete Hammond", "mixer")),
            "{roles:?}"
        );
        // Pochette et gravure ne sont pas des crédits musicaux.
        assert!(
            !roles.iter().any(|(n, _)| *n == "Me Co" || *n == "MS"),
            "{roles:?}"
        );
    }

    fn album(b: &Arc<dyn DbBackend>) -> (Option<i64>, Option<String>, Option<String>) {
        let r = b
            .query_one(
                "SELECT year, label, catalog_number FROM albums WHERE id = 5",
                &[],
            )
            .unwrap()
            .unwrap();
        (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_string()),
            r.get(2).and_then(|v| v.as_string()),
        )
    }

    /// Discogs ne fait que PROPOSER : la recherche n'écrit rien sur l'album
    /// ni sur ses pistes.
    #[tokio::test]
    async fn discogs_ne_fait_que_proposer_il_n_ecrit_pas() {
        let b = base();
        album_sans_mbid(&b);
        let avant = album(&b);
        let p = chercher_par(&b, 5, transport_enregistre)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.methode, "barcode");
        assert_eq!(
            p.code, "5012394144777",
            "le code-barres réduit à ses chiffres"
        );
        assert_eq!(p.annee, Some(1987));
        assert_eq!(
            album(&b),
            avant,
            "la recherche ne doit rien écrire sur l'album"
        );
        let credits = b
            .query_one("SELECT COUNT(*) FROM track_credits", &[])
            .unwrap()
            .unwrap();
        assert_eq!(credits.first().and_then(|v| v.as_i64()), Some(0));
        let liste = lister(&b, 5);
        assert_eq!(liste.len(), 1);
        assert_eq!(liste[0].statut, "proposee");
    }

    /// Accepter ne remplit que le VIDE : le label saisi reste.
    #[tokio::test]
    async fn accepter_ne_remplit_que_les_champs_vides() {
        let b = base();
        album_sans_mbid(&b);
        chercher_par(&b, 5, transport_enregistre).await.unwrap();
        let bilan = accepter(&b, 5, 249504, true).unwrap();
        assert_eq!(bilan.champs_poses, vec!["year", "catalog_number"]);
        assert_eq!(bilan.champs_gardes, vec!["label"]);
        assert_eq!(bilan.pistes_creditees, 2);
        assert_eq!(
            album(&b),
            (
                Some(1987),
                Some("Label saisi".into()),
                Some("PB 41447".into())
            )
        );
        // Tranchée : ni seconde acceptation, ni résurrection par une recherche.
        assert_eq!(
            accepter(&b, 5, 249504, true).unwrap_err(),
            "already_decided"
        );
        chercher_par(&b, 5, transport_enregistre).await.unwrap();
        assert_eq!(lister(&b, 5)[0].statut, "acceptee");
    }

    /// Conditions de Discogs : une proposition en attente de plus de six
    /// heures est périmée — marquée, refusée à l'acceptation — et une
    /// nouvelle recherche la rafraîchit.
    #[tokio::test]
    async fn une_proposition_de_plus_de_six_heures_est_perimee() {
        let b = base();
        album_sans_mbid(&b);
        chercher_par(&b, 5, transport_enregistre).await.unwrap();
        let tout_de_suite = lister(&b, 5);
        assert!(
            !tout_de_suite[0].perimee,
            "une proposition fraîche n'est pas périmée"
        );
        // On recule sa lecture de sept heures.
        let il_y_a_7_h = (chrono::Utc::now() - chrono::Duration::hours(7))
            .format(FORMAT_DATE)
            .to_string();
        b.execute(
            "UPDATE propositions_discogs SET cree_le = ? WHERE album_id = 5",
            &[&il_y_a_7_h as &dyn ToSqlValue],
        )
        .unwrap();
        assert!(lister(&b, 5)[0].perimee, "plus de six heures : périmée");
        assert_eq!(
            accepter(&b, 5, 249504, false).unwrap_err(),
            "proposition_perimee"
        );
        // Une nouvelle recherche la relit : fraîche de nouveau, acceptable.
        chercher_par(&b, 5, transport_enregistre).await.unwrap();
        assert!(
            !lister(&b, 5)[0].perimee,
            "la recherche rafraîchit la proposition"
        );
        assert!(accepter(&b, 5, 249504, false).is_ok());
    }

    #[tokio::test]
    async fn un_album_identifie_par_musicbrainz_n_est_pas_propose() {
        let b = base();
        b.execute(
            "INSERT INTO albums (id, title, barcode, musicbrainz_release_id) \
             VALUES (5, 'X', '5012394144777', 'mb-1')",
            &[],
        )
        .unwrap();
        assert_eq!(
            chercher_par(&b, 5, transport_enregistre).await.unwrap_err(),
            Empechement::DejaIdentifie
        );
    }
}
