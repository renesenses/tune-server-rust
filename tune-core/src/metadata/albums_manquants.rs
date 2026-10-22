//! MetaRust — « Albums manquants » (#6288) : la discographie MusicBrainz d'un
//! artiste identifié, moins ce que la bibliothèque possède.
//!
//! # La décision
//!
//! Bertrand, 13/10/2026, rattaché au chantier MetaRust (#4805), idée reprise
//! d'Aède : sur la fiche d'un artiste identifié (MBID), sa discographie
//! (MusicBrainz, groupes de parutions de type Album et EP, rééditions
//! regroupées) moins ce qui est dans la bibliothèque.
//!
//! « Rééditions regroupées » : c'est ce que fait le GROUPE de parutions
//! (release-group) — toutes les éditions d'un album en sont une seule.
//!
//! # Ce que l'issue laisse ouvert, et le choix prudent retenu
//!
//! * **Compilations, live, etc.** : EXCLUS par défaut — seuls les groupes
//!   Album ou EP sans type secondaire sont proposés. `avec_secondaires`
//!   (`?secondaires=1` sur la route) les rend tous.
//! * **Artistes très prolifiques** : au plus [`PAGES_MAX`] pages de
//!   [`PAGE`] groupes. Au-delà, la réponse le dit (`tronquee`) ; une liste
//!   tronquée peut OMETTRE un manque, jamais en inventer un.
//! * **Ce que la bibliothèque possède** : un album dont le groupe de
//!   parutions est connu, OU — l'identification ne couvre qu'environ la
//!   moitié des albums (mesure A, #6265) — dont le titre normalisé est le
//!   même. Mieux vaut taire un manque que dire « manquant » un disque qui est
//!   sur l'étagère.
//!
//! # Débit et mémoire
//!
//! Chaque page attend le créneau du limiteur MusicBrainz PARTAGÉ (une requête
//! par seconde pour tout le serveur). La discographie lue est gardée
//! [`VALIDITE_JOURS`] jours par artiste, AVANT soustraction : un album ajouté
//! à la bibliothèque sort de la liste tout de suite, sans relire MusicBrainz.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Groupes par page (le plafond de MusicBrainz).
pub const PAGE: u32 = 100;
/// Au plus 1 500 groupes — un quart de minute au premier passage.
pub const PAGES_MAX: u32 = 15;
pub const VALIDITE_JOURS: i64 = 7;

/// Un groupe de parutions MusicBrainz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Groupe {
    pub mbid: String,
    pub titre: String,
    /// `Album`, `EP`.
    pub type_primaire: Option<String>,
    /// `Compilation`, `Live`, …
    pub types_secondaires: Vec<String>,
    /// `first-release-date` (`AAAA`, `AAAA-MM` ou `AAAA-MM-JJ`).
    pub premiere_sortie: Option<String>,
}

/// La discographie lue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discographie {
    pub groupes: Vec<Groupe>,
    pub tronquee: bool,
    /// Secondes depuis l'époque Unix.
    pub lu_le: i64,
}

/// Les groupes d'une page `release-group?artist=…`, et le total annoncé.
pub fn groupes_de_la_page(page: &Value) -> (Vec<Groupe>, Option<u64>) {
    let groupes = page["release-groups"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|g| {
                    let mbid = g["id"].as_str()?.trim();
                    let titre = g["title"].as_str()?.trim();
                    if titre.is_empty() || !mbid_plausible(mbid) {
                        return None;
                    }
                    let texte = |v: &Value| {
                        v.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                    };
                    Some(Groupe {
                        mbid: mbid.to_string(),
                        titre: titre.to_string(),
                        type_primaire: texte(&g["primary-type"]),
                        types_secondaires: g["secondary-types"]
                            .as_array()
                            .map(|a| a.iter().filter_map(texte).collect())
                            .unwrap_or_default(),
                        premiere_sortie: texte(&g["first-release-date"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (groupes, page["release-group-count"].as_u64())
}

/// Ce que la lecture a donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lecture {
    Lue(Discographie),
    /// MusicBrainz ne connaît pas cet artiste (ou l'identifiant est mal formé).
    Inconnu,
    /// MusicBrainz n'a pas répondu : rien à conclure, rien à garder.
    Indisponible,
}

/// Une page brute : `Ok(Some)`, `Ok(None)` (artiste inconnu), `Err` (pas de
/// réponse).
pub type Page = Result<Option<Value>, ()>;

/// La discographie Album + EP d'un artiste, page par page, par le transport
/// injecté `lire_page(mbid, offset)`. Une page manquante en cours de route
/// rend `Indisponible` : la liste partielle serait sinon gardée sept jours
/// comme si elle était complète.
pub async fn lire_discographie_par<F, Fut>(mbid: &str, lire_page: &F, maintenant: i64) -> Lecture
where
    F: Fn(String, u32) -> Fut,
    Fut: Future<Output = Page>,
{
    let mbid = mbid.trim();
    if !mbid_plausible(mbid) {
        return Lecture::Inconnu;
    }
    let mut d = Discographie {
        lu_le: maintenant,
        ..Discographie::default()
    };
    for n in 0..PAGES_MAX {
        let page = match lire_page(mbid.to_string(), n * PAGE).await {
            Ok(Some(p)) => p,
            Ok(None) if n == 0 => return Lecture::Inconnu,
            Ok(None) | Err(()) => return Lecture::Indisponible,
        };
        let (groupes, total) = groupes_de_la_page(&page);
        let vide = groupes.is_empty();
        d.groupes.extend(groupes);
        let total = total.unwrap_or(0);
        if vide || d.groupes.len() as u64 >= total {
            return Lecture::Lue(d);
        }
        if n + 1 == PAGES_MAX {
            d.tronquee = true;
        }
    }
    Lecture::Lue(d)
}

/// Transport de production d'une page, sous le limiteur MusicBrainz partagé.
pub async fn lire_page_de_discographie(mbid: String, offset: u32) -> Page {
    use super::musicbrainz_release::{
        RefusMusicBrainz, parcourir_sur_musicbrainz, rate_limit_delay,
    };
    rate_limit_delay().await;
    match parcourir_sur_musicbrainz(
        "release-group",
        &[
            ("artist", mbid),
            ("type", "album|ep".to_string()),
            ("limit", PAGE.to_string()),
            ("offset", offset.to_string()),
        ],
    )
    .await
    {
        Ok(v) => Ok(Some(v)),
        Err(RefusMusicBrainz::Statut(400 | 404)) => Ok(None),
        Err(_) => Err(()),
    }
}

/// Un identifiant MusicBrainz plausible : 36 caractères, hexadécimal et
/// tirets. Garde le chemin de la requête à l'abri d'une valeur de la base
/// qui contiendrait `&` ou `?`.
pub fn mbid_plausible(mbid: &str) -> bool {
    mbid.len() == 36 && mbid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

// ── Soustraction ────────────────────────────────────────────────────────────

/// Un album de la bibliothèque, tel que la comparaison le lit.
#[derive(Debug, Clone, Default)]
pub struct AlbumPossede {
    pub titre: String,
    pub groupe_mbid: Option<String>,
}

/// Titre comparable : minuscules, sans accents ni ponctuation, sans les
/// mentions entre parenthèses ou crochets (« (Remastered) », « [Deluxe] »).
pub fn titre_normalise(titre: &str) -> String {
    let mut sans_mentions = String::with_capacity(titre.len());
    let mut profondeur = 0u32;
    for c in titre.chars() {
        match c {
            '(' | '[' => profondeur += 1,
            ')' | ']' => profondeur = profondeur.saturating_sub(1),
            _ if profondeur == 0 => sans_mentions.push(c),
            _ => {}
        }
    }
    let base = if sans_mentions.trim().is_empty() {
        titre
    } else {
        &sans_mentions
    };
    use unicode_normalization::UnicodeNormalization;
    base.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le groupe est-il proposé ? Album ou EP ; sans type secondaire, sauf
/// `avec_secondaires`.
pub fn retenu(g: &Groupe, avec_secondaires: bool) -> bool {
    let primaire = g
        .type_primaire
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("album") || t.eq_ignore_ascii_case("ep"));
    primaire && (avec_secondaires || g.types_secondaires.is_empty())
}

/// Le résultat de la comparaison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bilan {
    /// Les groupes retenus que la bibliothèque n'a pas, du plus ancien au plus
    /// récent (sans date : en dernier).
    pub manquants: Vec<Groupe>,
    /// Groupes retenus présents dans la bibliothèque.
    pub possedes: usize,
    /// Groupes retenus en tout.
    pub retenus: usize,
    pub tronquee: bool,
}

pub fn comparer(d: &Discographie, bibliotheque: &[AlbumPossede], avec_secondaires: bool) -> Bilan {
    let ids: HashSet<String> = bibliotheque
        .iter()
        .filter_map(|a| a.groupe_mbid.as_deref())
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .collect();
    let titres: HashSet<String> = bibliotheque
        .iter()
        .map(|a| titre_normalise(&a.titre))
        .filter(|t| !t.is_empty())
        .collect();
    let mut manquants = Vec::new();
    let mut possedes = 0;
    let mut retenus = 0;
    for g in d.groupes.iter().filter(|g| retenu(g, avec_secondaires)) {
        retenus += 1;
        if ids.contains(&g.mbid.to_ascii_lowercase()) || titres.contains(&titre_normalise(&g.titre))
        {
            possedes += 1;
        } else {
            manquants.push(g.clone());
        }
    }
    manquants.sort_by(|a, b| {
        (a.premiere_sortie.is_none(), &a.premiere_sortie, &a.titre).cmp(&(
            b.premiere_sortie.is_none(),
            &b.premiere_sortie,
            &b.titre,
        ))
    });
    Bilan {
        manquants,
        possedes,
        retenus,
        tronquee: d.tronquee,
    }
}

// ── Mémoire (table `settings`) ──────────────────────────────────────────────

fn cle(mbid: &str) -> String {
    format!("cache:mb:discographie:{}", mbid.trim().to_ascii_lowercase())
}

/// La discographie gardée, si elle a moins de [`VALIDITE_JOURS`] jours.
pub fn gardee(backend: &Arc<dyn DbBackend>, mbid: &str, maintenant: i64) -> Option<Discographie> {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(&cle(mbid))
        .ok()
        .flatten()?;
    let d: Discographie = serde_json::from_str(&brut).ok()?;
    (maintenant - d.lu_le <= VALIDITE_JOURS * 86_400).then_some(d)
}

pub fn garder(backend: &Arc<dyn DbBackend>, mbid: &str, d: &Discographie) {
    let _ = SettingsRepo::with_backend(backend.clone()).set(&cle(mbid), &json!(d).to_string());
}

/// La discographie : gardée si fraîche, sinon lue puis gardée.
pub async fn discographie_par<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    mbid: &str,
    lire_page: &F,
    maintenant: i64,
) -> Lecture
where
    F: Fn(String, u32) -> Fut,
    Fut: Future<Output = Page>,
{
    if let Some(d) = gardee(backend, mbid, maintenant) {
        return Lecture::Lue(d);
    }
    let lecture = lire_discographie_par(mbid, lire_page, maintenant).await;
    if let Lecture::Lue(ref d) = lecture {
        garder(backend, mbid, d);
    }
    lecture
}

#[cfg(test)]
#[path = "albums_manquants_tests.rs"]
mod tests;
