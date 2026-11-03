//! Les versions d'un même album regroupées sous UNE fiche — l'« œuvre »
//! (#7617).
//!
//! # Les routes
//!
//! | méthode | chemin | rôle |
//! |---|---|---|
//! | `GET` | `/library/albums/works` | la bibliothèque en œuvres, paginée (`limit`, `offset`) |
//! | `GET` | `/library/albums/{id}/versions` | l'œuvre de l'album `id`, avec toutes ses versions |
//! | `GET` | `/library/albums/works/settings` | le réglage : `{"enabled": bool}` |
//! | `PUT` | `/library/albums/works/settings` | l'activer ou le désactiver (`{"enabled": false}`) |
//! | `PUT` | `/library/albums/works/preferred` | la version préférée d'une œuvre (`{"work_id": "…", "album_id": 12}`, `null` pour revenir au défaut) |
//!
//! # Les clés, dans l'ordre
//!
//! 1. le groupe de sortie MusicBrainz (`albums.musicbrainz_release_group_id`) ;
//! 2. à défaut, l'ARTISTE de l'album et le titre normalisé, sans ses mentions
//!    d'édition (« (2011 Remaster) », « [Deluxe Edition] », « - Remastered »).
//!    Le titre seul ne suffit jamais : deux « Greatest Hits » d'artistes
//!    différents restent deux œuvres ;
//! 3. l'étiquette `VERSION` des pistes (`track_metadata['version']`), sinon la
//!    mention d'édition retirée du titre, NOMME chaque version ; elle ne
//!    regroupe rien.
//!
//! Un album sans identifiant de groupe rejoint le groupe MusicBrainz de même
//! artiste et de même titre normalisé quand il n'y en a qu'UN ; s'il y en a
//! plusieurs, il reste avec ses pareils sans identifiant.
//!
//! Les paires déclarées « pas des doublons » (#1276,
//! `/albums/{id}/distinct/{other_id}`) ne sont jamais dans la même œuvre :
//! c'est le geste qui sépare deux versions regroupées à tort.
//!
//! # Ce que le module ne fait PAS
//!
//! Il n'écrit rien en base hors de deux réglages (`settings`) : aucun album
//! n'est fusionné, aucune colonne n'est ajoutée. `GET /library/albums` est
//! inchangé ; seul un client qui demande les œuvres les reçoit.

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::Album;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::groupes_versions::noyau_de_titre;
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

/// Le réglage qui active le regroupement (`settings`). Absent : activé.
pub(crate) const REGLAGE_ACTIF: &str = "library_group_versions";

/// Les versions préférées choisies par l'utilisateur : un objet JSON
/// `{work_id: album_id}` rangé sous une seule clé de `settings`.
pub(crate) const REGLAGE_PREFEREES: &str = "library_works_preferred";

/// Les mots qui font d'un suffixe une mention d'ÉDITION, et non une partie du
/// titre : « Rock And Roll (Part 2) » ou « Running Up That Hill (A Deal With
/// God) » gardent leur parenthèse.
const MOTS_D_EDITION: &[&str] = &[
    "remaster",
    "deluxe",
    "edition",
    "édition",
    "anniversary",
    "expanded",
    "bonus",
    "collector",
    "reissue",
    "re-issue",
    "hi-res",
    "hires",
    "24-bit",
    "24 bit",
    "24bit",
    "mono",
    "stereo",
    "legacy",
    "sacd",
    "mfsl",
];

/// Les délimiteurs d'un suffixe d'édition, les mêmes que les versions de
/// piste (#2372).
const DELIMITEURS: [&str; 3] = crate::routes::versions::DELIMITEURS_D_EDITION;

/// Un album vu par le regroupement.
#[derive(Debug, Clone, Default)]
pub(crate) struct Candidat {
    pub id: i64,
    pub titre: String,
    pub artist_id: Option<i64>,
    pub groupe_mb: Option<String>,
    pub source: String,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    /// La valeur `VERSION` commune aux pistes de l'album.
    pub version: Option<String>,
}

/// Comment une œuvre a été formée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Methode {
    MusicBrainz,
    ArtisteTitre,
    /// Une seule version : rien n'a été regroupé.
    Seule,
}

impl Methode {
    pub(crate) fn nom(self) -> &'static str {
        match self {
            Methode::MusicBrainz => "musicbrainz",
            Methode::ArtisteTitre => "artist_title",
            Methode::Seule => "single",
        }
    }
}

/// Une version de l'œuvre : l'album et le nom qui la distingue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub id: i64,
    pub nom: Option<String>,
}

/// Une œuvre : ses versions, dans l'ordre de préférence, et celle jouée par
/// défaut.
#[derive(Debug, Clone)]
pub(crate) struct Oeuvre {
    pub work_id: String,
    pub methode: Methode,
    pub versions: Vec<Version>,
    pub preferee: i64,
}

/// Un suffixe est-il une mention d'édition ? Un mot de [`MOTS_D_EDITION`], ou
/// une année seule (« Abbey Road (2019) » n'est pas un autre album).
fn est_mention_d_edition(suffixe: &str) -> bool {
    let s = suffixe.trim().to_lowercase();
    if s.is_empty() {
        return false;
    }
    let annee_seule = s.len() == 4 && s.chars().all(|c| c.is_ascii_digit());
    annee_seule || MOTS_D_EDITION.iter().any(|m| s.contains(m))
}

/// Le titre sans ses mentions d'édition finales, et ces mentions, de la
/// première à la dernière.
///
/// On retire, en partant de la fin, chaque suffixe ouvert par un délimiteur
/// d'édition dont le contenu est une mention d'édition. « Abbey Road (2019
/// Remaster) » → (« Abbey Road », [« 2019 Remaster »]) ; « Wish You Were Here
/// (Live) » reste entier.
pub(crate) fn sans_mentions_d_edition(titre: &str) -> (String, Vec<String>) {
    let mut reste = titre.trim().to_string();
    let mut mentions = Vec::new();
    loop {
        let coupe = DELIMITEURS
            .iter()
            .filter_map(|d| reste.rfind(d).map(|pos| (pos, *d)))
            .max_by_key(|(pos, _)| *pos);
        let Some((pos, d)) = coupe else { break };
        let corps = reste[pos + d.len()..].trim();
        let corps = match d {
            " (" => corps.strip_suffix(')').unwrap_or(corps),
            " [" => corps.strip_suffix(']').unwrap_or(corps),
            _ => corps,
        }
        .trim();
        let base = reste[..pos].trim_end();
        if base.is_empty() || !est_mention_d_edition(corps) {
            break;
        }
        mentions.insert(0, corps.to_string());
        reste = base.to_string();
    }
    (reste, mentions)
}

/// La clé de titre de la clé 2 : le noyau du titre sans mentions d'édition.
pub(crate) fn titre_normalise(titre: &str) -> String {
    noyau_de_titre(&sans_mentions_d_edition(titre).0)
}

/// Le nom d'une version (clé 3) : `VERSION`, sinon les mentions d'édition du
/// titre, sinon rien.
fn nom_de_version(c: &Candidat) -> Option<String> {
    if let Some(v) = c
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(v.to_string());
    }
    let (_, mentions) = sans_mentions_d_edition(&c.titre);
    (!mentions.is_empty()).then(|| mentions.join(", "))
}

/// L'ordre de préférence par défaut : la version locale avant celle d'un
/// service, puis la meilleure résolution (fréquence, puis profondeur), puis
/// le plus ancien album.
fn ordre_par_defaut(a: &Candidat, b: &Candidat) -> std::cmp::Ordering {
    let locale = |c: &Candidat| c.source == "local";
    locale(b)
        .cmp(&locale(a))
        .then(b.sample_rate.unwrap_or(0).cmp(&a.sample_rate.unwrap_or(0)))
        .then(b.bit_depth.unwrap_or(0).cmp(&a.bit_depth.unwrap_or(0)))
        .then(a.id.cmp(&b.id))
}

/// Sépare un groupe en sous-groupes sans paire déclarée distincte : chaque
/// membre rejoint le premier sous-groupe où il n'a aucun « pas un doublon ».
fn separer<'a>(membres: Vec<&'a Candidat>, distinctes: &DistinctPairSet) -> Vec<Vec<&'a Candidat>> {
    if distinctes.is_empty() {
        return vec![membres];
    }
    let mut sous: Vec<Vec<&Candidat>> = Vec::new();
    for m in membres {
        match sous
            .iter_mut()
            .find(|g| g.iter().all(|o| !distinctes.contains(o.id, m.id)))
        {
            Some(g) => g.push(m),
            None => sous.push(vec![m]),
        }
    }
    sous
}

/// Regroupe des albums en œuvres. `actif` à faux : chaque album est sa propre
/// œuvre. `preferees` : les choix de l'utilisateur, par `work_id`, ignorés
/// quand l'album n'est plus dans l'œuvre.
///
/// Les œuvres sortent dans l'ordre du premier album de chacune dans
/// `albums` : l'appelant garde ainsi son tri.
pub(crate) fn regrouper(
    albums: &[Candidat],
    actif: bool,
    distinctes: &DistinctPairSet,
    preferees: &HashMap<String, i64>,
) -> Vec<Oeuvre> {
    // (clé de l'œuvre, méthode) → membres, dans l'ordre d'arrivée.
    let mut groupes: Vec<(String, Methode, Vec<&Candidat>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    if !actif {
        return albums
            .iter()
            .map(|c| Oeuvre {
                work_id: format!("album:{}", c.id),
                methode: Methode::Seule,
                versions: vec![Version {
                    id: c.id,
                    nom: nom_de_version(c),
                }],
                preferee: c.id,
            })
            .collect();
    }

    // Les groupes MusicBrainz joignables par la clé 2 : (artiste, titre) →
    // identifiants de groupe distincts.
    let mut mb_par_titre: HashMap<(i64, String), Vec<String>> = HashMap::new();
    for c in albums {
        if let (Some(g), Some(a)) = (
            c.groupe_mb.as_deref().filter(|g| !g.is_empty()),
            c.artist_id,
        ) {
            let liste = mb_par_titre
                .entry((a, titre_normalise(&c.titre)))
                .or_default();
            if !liste.iter().any(|x| x == g) {
                liste.push(g.to_string());
            }
        }
    }

    for c in albums {
        let (cle, methode) = match (
            c.groupe_mb.as_deref().filter(|g| !g.is_empty()),
            c.artist_id,
        ) {
            (Some(g), _) => (format!("mb:{g}"), Methode::MusicBrainz),
            (None, Some(a)) => {
                let titre = titre_normalise(&c.titre);
                match mb_par_titre.get(&(a, titre.clone())).map(Vec::as_slice) {
                    Some([g]) => (format!("mb:{g}"), Methode::MusicBrainz),
                    _ if titre.is_empty() => (format!("album:{}", c.id), Methode::Seule),
                    _ => (format!("at:{a}:{titre}"), Methode::ArtisteTitre),
                }
            }
            (None, None) => (format!("album:{}", c.id), Methode::Seule),
        };
        match index.get(&cle) {
            Some(&i) => groupes[i].2.push(c),
            None => {
                index.insert(cle.clone(), groupes.len());
                groupes.push((cle, methode, vec![c]));
            }
        }
    }

    let mut oeuvres = Vec::new();
    for (cle, methode, membres) in groupes {
        let sous = separer(membres, distinctes);
        let plusieurs = sous.len() > 1;
        for (rang, mut membres) in sous.into_iter().enumerate() {
            // Un sous-groupe séparé à la main garde une clé stable : celle
            // du groupe, suffixée par son plus petit album.
            let work_id = if plusieurs && rang > 0 {
                let min = membres.iter().map(|m| m.id).min().unwrap_or(0);
                format!("{cle}#{min}")
            } else {
                cle.clone()
            };
            membres.sort_by(|a, b| ordre_par_defaut(a, b));
            let choisie = preferees
                .get(&work_id)
                .copied()
                .filter(|id| membres.iter().any(|m| m.id == *id));
            let preferee = choisie.unwrap_or(membres[0].id);
            let methode = if membres.len() == 1 {
                Methode::Seule
            } else {
                methode
            };
            oeuvres.push(Oeuvre {
                work_id,
                methode,
                versions: membres
                    .iter()
                    .map(|m| Version {
                        id: m.id,
                        nom: nom_de_version(m),
                    })
                    .collect(),
                preferee,
            });
        }
    }
    oeuvres
}

// ── Lecture en base ─────────────────────────────────────────────────────────

/// Le regroupement est-il activé ? Absent ou illisible : oui.
pub(crate) fn regroupement_actif(state: &AppState) -> bool {
    SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_ACTIF)
        .ok()
        .flatten()
        .is_none_or(|v| v.trim() != "false")
}

fn preferees(state: &AppState) -> HashMap<String, i64> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_PREFEREES)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn distinctes(state: &AppState) -> DistinctPairSet {
    AlbumDistinctRepo::with_backend(state.backend.clone())
        .charger_ensemble()
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "album_distinct_pairs_load_failed");
            DistinctPairSet::default()
        })
}

/// La `VERSION` de chaque album dont les pistes n'en portent qu'UNE valeur.
/// Une requête groupée pour toute la bibliothèque.
fn versions_des_albums(state: &AppState) -> HashMap<i64, String> {
    let lignes = state
        .backend
        .query_many(
            "SELECT t.album_id, m.value FROM track_metadata m \
             JOIN tracks t ON t.id = m.track_id \
             WHERE m.key = 'version' AND m.value <> '' AND t.album_id IS NOT NULL \
             GROUP BY t.album_id, m.value",
            &[],
        )
        .ou_defaut_journalise();
    let mut vues: HashMap<i64, Option<String>> = HashMap::new();
    for l in &lignes {
        let (Some(id), Some(v)) = (
            l.first().and_then(|x| x.as_i64()),
            l.get(1).and_then(|x| x.as_string()),
        ) else {
            continue;
        };
        vues.entry(id).and_modify(|e| *e = None).or_insert(Some(v));
    }
    vues.into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect()
}

fn candidat(a: &Album, versions: &HashMap<i64, String>) -> Option<Candidat> {
    let id = a.id?;
    Some(Candidat {
        id,
        titre: a.title.clone(),
        artist_id: a.artist_id,
        groupe_mb: a.musicbrainz_release_group_id.clone(),
        source: a.source.clone(),
        sample_rate: a.sample_rate,
        bit_depth: a.bit_depth,
        version: versions.get(&id).cloned(),
    })
}

/// Une version, telle que le sélecteur la montre.
fn version_json(a: &Album, nom: &Option<String>) -> Value {
    json!({
        "id": a.id,
        "title": a.title,
        "version_name": nom,
        "year": a.year,
        "original_year": a.original_year,
        "label": a.label,
        "format": a.format,
        "sample_rate": a.sample_rate,
        "bit_depth": a.bit_depth,
        "source": a.source,
        "cover_path": a.cover_path,
        "track_count": a.track_count,
        "musicbrainz_release_id": a.musicbrainz_release_id,
    })
}

fn oeuvre_json(o: &Oeuvre, albums: &HashMap<i64, &Album>) -> Value {
    let preferee = albums.get(&o.preferee).map(|a| a.to_json());
    json!({
        "work_id": o.work_id,
        "method": o.methode.nom(),
        "count": o.versions.len(),
        "preferred_id": o.preferee,
        "preferred": preferee,
        "versions": o
            .versions
            .iter()
            .filter_map(|v| albums.get(&v.id).map(|a| version_json(a, &v.nom)))
            .collect::<Vec<_>>(),
    })
}

/// Toute la bibliothèque visible (masqués exclus), triée par titre.
fn albums_visibles(state: &AppState) -> Vec<Album> {
    AlbumRepo::with_backend(state.backend.clone())
        .list_filtered(i64::MAX, 0, "title", "asc", None, None, None, false, None)
        .ou_defaut_journalise()
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageOeuvres {
    #[serde(default = "limite_par_defaut")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn limite_par_defaut() -> i64 {
    100
}

/// `GET /library/albums/works` — la bibliothèque en œuvres.
pub(crate) async fn lister_oeuvres(
    State(state): State<AppState>,
    Query(page): Query<PageOeuvres>,
) -> Result<Json<Value>, AppError> {
    let actif = regroupement_actif(&state);
    let albums = albums_visibles(&state);
    let versions = versions_des_albums(&state);
    let candidats: Vec<Candidat> = albums
        .iter()
        .filter_map(|a| candidat(a, &versions))
        .collect();
    let oeuvres = regrouper(&candidats, actif, &distinctes(&state), &preferees(&state));
    let par_id: HashMap<i64, &Album> = albums.iter().filter_map(|a| Some((a.id?, a))).collect();
    let limite = page.limit.clamp(1, 1000) as usize;
    let debut = page.offset.max(0) as usize;
    let items: Vec<Value> = oeuvres
        .iter()
        .skip(debut)
        .take(limite)
        .map(|o| oeuvre_json(o, &par_id))
        .collect();
    Ok(Json(json!({
        "enabled": actif,
        "total": oeuvres.len(),
        "albums_total": candidats.len(),
        "limit": limite,
        "offset": debut,
        "items": items,
    })))
}

/// `GET /library/albums/{id}/versions` — l'œuvre d'un album.
///
/// Les candidats sont les albums du même artiste et ceux du même groupe
/// MusicBrainz : ce sont les seuls que les clés 1 et 2 peuvent rapprocher.
pub(crate) async fn versions_de_l_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("album not found"))?;
    let mut albums: BTreeMap<i64, Album> = BTreeMap::new();
    albums.insert(id, album.clone());
    if let Some(a) = album.artist_id {
        for x in repo.list_by_artist(a).ou_defaut_journalise() {
            if let Some(i) = x.id {
                albums.entry(i).or_insert(x);
            }
        }
    }
    if let Some(g) = album
        .musicbrainz_release_group_id
        .as_deref()
        .filter(|g| !g.is_empty())
    {
        for x in repo.list_by_release_group(g).ou_defaut_journalise() {
            if let Some(i) = x.id {
                albums.entry(i).or_insert(x);
            }
        }
    }
    let versions = versions_des_albums(&state);
    let candidats: Vec<Candidat> = albums
        .values()
        .filter_map(|a| candidat(a, &versions))
        .collect();
    let actif = regroupement_actif(&state);
    let oeuvres = regrouper(&candidats, actif, &distinctes(&state), &preferees(&state));
    let par_id: HashMap<i64, &Album> = albums.iter().map(|(k, v)| (*k, v)).collect();
    let oeuvre = oeuvres
        .iter()
        .find(|o| o.versions.iter().any(|v| v.id == id))
        .ok_or(AppError::not_found("album not found"))?;
    let mut corps = oeuvre_json(oeuvre, &par_id);
    if let Some(obj) = corps.as_object_mut() {
        obj.insert("enabled".into(), json!(actif));
        obj.insert("album_id".into(), json!(id));
    }
    Ok(Json(corps))
}

/// `GET /library/albums/works/settings`.
pub(crate) async fn lire_reglage(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "enabled": regroupement_actif(&state) }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CorpsReglage {
    enabled: bool,
}

/// `PUT /library/albums/works/settings`.
pub(crate) async fn ecrire_reglage(
    State(state): State<AppState>,
    Json(corps): Json<CorpsReglage>,
) -> Result<Json<Value>, AppError> {
    SettingsRepo::with_backend(state.backend.clone())
        .set(REGLAGE_ACTIF, if corps.enabled { "true" } else { "false" })
        .map_err(AppError::internal)?;
    Ok(Json(json!({ "enabled": corps.enabled })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CorpsPreferee {
    work_id: String,
    album_id: Option<i64>,
}

/// `PUT /library/albums/works/preferred` — la version préférée d'une œuvre,
/// ou `album_id: null` pour revenir au choix par défaut.
pub(crate) async fn ecrire_preferee(
    State(state): State<AppState>,
    Json(corps): Json<CorpsPreferee>,
) -> Result<Json<Value>, AppError> {
    let work_id = corps.work_id.trim().to_string();
    if work_id.is_empty() {
        return Err(AppError::bad_request("work_id is required"));
    }
    let mut choix = preferees(&state);
    match corps.album_id {
        Some(a) => {
            choix.insert(work_id.clone(), a);
        }
        None => {
            choix.remove(&work_id);
        }
    }
    let valeur = serde_json::to_string(&choix).map_err(|e| AppError::internal(e.to_string()))?;
    SettingsRepo::with_backend(state.backend.clone())
        .set(REGLAGE_PREFEREES, &valeur)
        .map_err(AppError::internal)?;
    Ok(Json(
        json!({ "work_id": work_id, "album_id": corps.album_id }),
    ))
}

#[cfg(test)]
#[path = "oeuvres_tests.rs"]
mod tests;
