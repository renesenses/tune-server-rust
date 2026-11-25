//! Les versions d'un même album regroupées sous UNE fiche — l'« œuvre »
//! (#7617).
//!
//! # Les routes
//!
//! | méthode | chemin | rôle |
//! |---|---|---|
//! | `GET` | `/library/albums/works` | la bibliothèque en œuvres, paginée (`limit`, `offset`) |
//! | `GET` | `/library/albums/works/members` | les œuvres de plusieurs versions, compactes (`work_id`, `preferred_id`, `ids`) : pour la grille |
//! | `GET` | `/library/albums/{id}/versions` | l'œuvre de l'album `id`, avec toutes ses versions |
//! | `GET` | `/library/albums/works/settings` | le réglage : `{"enabled": bool}` |
//! | `PUT` | `/library/albums/works/settings` | l'activer ou le désactiver (`{"enabled": false}`) |
//! | `PUT` | `/library/albums/works/preferred` | la version préférée d'une œuvre (`{"work_id": "…", "album_id": 12}`, `null` pour revenir au défaut) |
//!
//! # Les clés, dans l'ordre
//!
//! 1. le groupe de sortie MusicBrainz (`albums.musicbrainz_release_group_id`) ;
//! 2. le MASTER Discogs (fil 3012, [`tune_core::metadata::discogs_master`]) :
//!    `album_metadata['discogs_master_id']`, sinon la valeur commune de
//!    `track_metadata['discogs_master_id']` (balise `DISCOGS_MASTER_ID`) ;
//! 3. à défaut des deux, l'ARTISTE de l'album et le titre normalisé, sans ses
//!    mentions d'édition (« (2011 Remaster) », « [Deluxe Edition] »,
//!    « - Remastered »). Le titre seul ne suffit jamais : deux « Greatest
//!    Hits » d'artistes différents restent deux œuvres ;
//! 4. l'étiquette `VERSION` des pistes (`track_metadata['version']`), sinon la
//!    mention d'édition retirée du titre, NOMME chaque version ; elle ne
//!    regroupe rien.
//!
//! # Une seule liste : les composantes connexes (fil 3012)
//!
//! Deux albums sont dans la même œuvre s'ils partagent le groupe MusicBrainz
//! OU le master Discogs, et la relation est transitive : A (MB = x, Discogs
//! = y) relie B (MB = x seulement) et C (Discogs = y seulement). Deux groupes
//! MusicBrainz DIFFÉRENTS ne fusionnent jamais : un master Discogs dont les
//! albums portent plusieurs groupes MusicBrainz ne relie personne à eux, il
//! ne réunit que ses albums sans MusicBrainz. Une composante porte donc au
//! plus un groupe MusicBrainz.
//!
//! La clé 3 ne vaut que pour un album sans MusicBrainz ni Discogs. Il rejoint
//! la composante de même artiste et de même titre normalisé quand il n'y en a
//! qu'UNE ; s'il y en a plusieurs, il reste avec ses pareils sans
//! identifiant. Il ne fusionne donc jamais deux composantes.
//!
//! # Les coffrets composés à la main (#8953, fil 3012)
//!
//! Un coffret manuel qui réunit plusieurs éditions ou plusieurs albums n'est
//! la version de rien : il est sa propre œuvre. Celui qui ne réunit que les
//! disques d'UNE sortie (« Physical Graffiti (Disc 1) » et « (Disc 2) »,
//! [`tune_core::db::coffrets_auto::Marqueur::reunit_une_seule_sortie`]) est un
//! album à plusieurs disques comme un coffret automatique : il est regroupé
//! comme les autres, et compte pour UNE version.
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

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::Album;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::groupes_versions::noyau_de_titre;
use tune_core::metadata::discogs_master;
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
    /// Fil 3012 — le master Discogs de l'album, en chiffres.
    pub discogs_master: Option<String>,
    /// #8953 — l'album est un coffret COMPOSÉ À LA MAIN de plusieurs
    /// éditions ou albums : il est sa propre œuvre, jamais une version d'un
    /// autre album. Un coffret réuni automatiquement, ou à la main à partir
    /// des seuls disques d'une sortie (fil 3012), reste un album à plusieurs
    /// disques ordinaire, regroupé comme les autres (#7617) : ce champ est
    /// alors faux.
    pub coffret: bool,
}

/// Comment une œuvre a été formée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Methode {
    MusicBrainz,
    /// Fil 3012 — une composante sans groupe MusicBrainz, réunie par le
    /// master Discogs.
    DiscogsMaster,
    ArtisteTitre,
    /// Une seule version : rien n'a été regroupé.
    Seule,
}

impl Methode {
    pub(crate) fn nom(self) -> &'static str {
        match self {
            Methode::MusicBrainz => "musicbrainz",
            Methode::DiscogsMaster => "discogs_master",
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

/// Un identifiant non vide, épuré.
fn non_vide(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|g| !g.is_empty())
}

/// La clé d'œuvre et la méthode de chaque album, par composantes connexes
/// (fil 3012) — voir l'en-tête du module.
///
/// Aucune union-find n'est nécessaire, parce qu'un album porte au plus UN
/// groupe MusicBrainz et UN master Discogs :
///
/// 1. un album qui a un groupe MusicBrainz `g` est dans `mb:g` ;
/// 2. un album sans MusicBrainz mais avec un master `y` rejoint `mb:g` quand
///    les albums de master `y` ne portent QU'UN groupe MusicBrainz `g` ; sinon
///    (aucun, ou plusieurs : MusicBrainz départage) il est dans `dg:y` ;
/// 3. un album sans aucune des deux clés rejoint l'UNIQUE clé que (1) et (2)
///    ont donnée aux albums de même artiste et de même titre normalisé ;
///    sinon `at:artiste:titre`.
///
/// La transitivité est complète : un master ne relie qu'un groupe
/// MusicBrainz à la fois, un groupe se relie à tous les masters de ses
/// albums par (2). Deux groupes MusicBrainz différents restent séparés.
///
/// 🔴 #8953 (fil 2884) — un coffret manuel de plusieurs éditions
/// (`coffret`) n'est la version de rien : son titre composé perd ce qui
/// distinguait ses disques (« …(50th Anniversary)(Stereo) [DSD64 …] »
/// devient « …(50th Anniversary) »), et son groupe MusicBrainz ou son master
/// sont ceux de l'album cible, hérités par la composition. Il ne prend
/// aucune clé et n'en donne aucune : `album:id`.
fn cles_des_albums(albums: &[Candidat]) -> HashMap<i64, (String, Methode)> {
    let ordinaires = || albums.iter().filter(|c| !c.coffret);

    // (2) Les groupes MusicBrainz des albums de chaque master.
    let mut mb_par_master: HashMap<&str, Vec<&str>> = HashMap::new();
    for c in ordinaires() {
        if let Some(y) = non_vide(&c.discogs_master) {
            let liste = mb_par_master.entry(y).or_default();
            if let Some(g) = non_vide(&c.groupe_mb)
                && !liste.contains(&g)
            {
                liste.push(g);
            }
        }
    }

    let mut cles: HashMap<i64, (String, Methode)> = HashMap::new();
    for c in ordinaires() {
        let cle = match (non_vide(&c.groupe_mb), non_vide(&c.discogs_master)) {
            (Some(g), _) => (format!("mb:{g}"), Methode::MusicBrainz),
            (None, Some(y)) => match mb_par_master.get(y).map(Vec::as_slice) {
                Some([g]) => (format!("mb:{g}"), Methode::MusicBrainz),
                _ => (format!("dg:{y}"), Methode::DiscogsMaster),
            },
            (None, None) => continue,
        };
        cles.insert(c.id, cle);
    }

    // (3) Les clés joignables par artiste et titre : (artiste, titre) → clés
    // distinctes des albums qui en ont une.
    let mut par_titre: HashMap<(i64, String), Vec<(String, Methode)>> = HashMap::new();
    for c in ordinaires() {
        if let (Some(cle), Some(a)) = (cles.get(&c.id), c.artist_id) {
            let liste = par_titre.entry((a, titre_normalise(&c.titre))).or_default();
            if !liste.iter().any(|(k, _)| *k == cle.0) {
                liste.push(cle.clone());
            }
        }
    }
    for c in ordinaires() {
        if cles.contains_key(&c.id) {
            continue;
        }
        let Some(a) = c.artist_id else { continue };
        let titre = titre_normalise(&c.titre);
        let cle = match par_titre.get(&(a, titre.clone())).map(Vec::as_slice) {
            Some([unique]) => unique.clone(),
            _ if titre.is_empty() => continue,
            _ => (format!("at:{a}:{titre}"), Methode::ArtisteTitre),
        };
        cles.insert(c.id, cle);
    }
    cles
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

    let cles = cles_des_albums(albums);
    for c in albums {
        let (cle, methode) = cles
            .get(&c.id)
            .cloned()
            .unwrap_or_else(|| (format!("album:{}", c.id), Methode::Seule));
        match index.get(&cle) {
            Some(&i) => groupes[i].2.push(c),
            None => {
                index.insert(cle.clone(), groupes.len());
                groupes.push((cle, methode, vec![c]));
            }
        }
    }

    let mut oeuvres = Vec::new();
    for (cle, methode, mut membres) in groupes {
        // `separer` est glouton : sans un ordre fixe, la grille (albums triés
        // par titre) et la fiche (triés par identifiant) formeraient des
        // sous-groupes et des `work_id` différents pour la même œuvre.
        membres.sort_by_key(|m| m.id);
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

/// #8953 — les coffrets COMPOSÉS À LA MAIN (marqueur `coffret`, origine
/// `manuel`) de plusieurs éditions ou albums. Les coffrets réunis
/// automatiquement n'y sont pas : un « Disc 1 / Disc 2 » rangé en dossiers
/// est un album double ordinaire, et l'exclure de sa clé MusicBrainz
/// casserait les regroupements de #7617 (The Wall 2 CD et sa version hi-res)
/// selon le seul rangement en dossiers.
///
/// Fil 3012 — pour la même raison, un coffret manuel qui ne réunit que les
/// disques d'UNE sortie n'y est pas non plus : Dimitri réunit « Physical
/// Graffiti (Disc 1) » et « (Disc 2) » à la main, ce que la passe
/// automatique aurait fait ; l'exclure retirait 10 de ses 19 éditions des
/// Versions. Une base illisible rend l'ensemble vide : le regroupement
/// d'avant, journalisé.
fn albums_coffrets(state: &AppState) -> HashSet<i64> {
    match tune_core::db::coffrets_auto::marqueurs(&state.backend) {
        Ok(m) => m
            .into_iter()
            .filter(|(_, marqueur)| marqueur.est_manuel() && !marqueur.reunit_une_seule_sortie())
            .map(|(id, _)| id)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "oeuvres_marqueurs_coffret_illisibles");
            HashSet::new()
        }
    }
}

/// Fil 3012 — le master Discogs de chaque album, en UNE lecture de chacun des
/// deux magasins : celui posé sur l'album (`album_metadata`) prime ; sinon la
/// valeur commune des pistes (`track_metadata`), quand elles n'en portent
/// qu'UNE. Toute valeur passe par [`discogs_master::normaliser`] : une
/// valeur illisible ou vide ne compte pas.
fn masters_des_albums(state: &AppState) -> HashMap<i64, String> {
    let par_piste = state
        .backend
        .query_many(
            &format!(
                "SELECT t.album_id, m.value FROM track_metadata m \
                 JOIN tracks t ON t.id = m.track_id \
                 WHERE m.key = '{cle}' AND m.value <> '' AND t.album_id IS NOT NULL \
                 GROUP BY t.album_id, m.value",
                cle = discogs_master::CLE
            ),
            &[],
        )
        .ou_defaut_journalise();
    let mut vues: HashMap<i64, Option<String>> = HashMap::new();
    for l in &par_piste {
        let (Some(id), Some(v)) = (
            l.first().and_then(|x| x.as_i64()),
            l.get(1)
                .and_then(|x| x.as_string())
                .and_then(|v| discogs_master::normaliser(&v)),
        ) else {
            continue;
        };
        vues.entry(id)
            .and_modify(|e| {
                if e.as_deref() != Some(v.as_str()) {
                    *e = None;
                }
            })
            .or_insert(Some(v));
    }
    let mut masters: HashMap<i64, String> = vues
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
    let par_album = state
        .backend
        .query_many(
            &format!(
                "SELECT album_id, value FROM album_metadata WHERE key = '{cle}'",
                cle = discogs_master::CLE
            ),
            &[],
        )
        .ou_defaut_journalise();
    for l in &par_album {
        if let (Some(id), Some(v)) = (
            l.first().and_then(|x| x.as_i64()),
            l.get(1)
                .and_then(|x| x.as_string())
                .and_then(|v| discogs_master::normaliser(&v)),
        ) {
            masters.insert(id, v);
        }
    }
    masters
}

fn candidat(
    a: &Album,
    versions: &HashMap<i64, String>,
    masters: &HashMap<i64, String>,
    coffrets: &HashSet<i64>,
) -> Option<Candidat> {
    let id = a.id?;
    Some(Candidat {
        discogs_master: masters.get(&id).cloned(),
        coffret: coffrets.contains(&id),
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
///
/// tune-web-client#2731 — avec `max_channels` (le plus grand nombre de canaux
/// de ses pistes, comme dans la grille, #7725) : la fiche dit quelle version
/// est en 2.0 et laquelle en 5.1. Clé absente quand les pistes ne le disent
/// pas ; champ additif, un client qui l'ignore lit la réponse d'avant.
fn version_json(a: &Album, nom: &Option<String>, canaux: &super::albums::CanauxParAlbum) -> Value {
    let mut v = json!({
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
    });
    if let Some(obj) = v.as_object_mut() {
        super::albums::inserer_max_channels(obj, a, canaux);
        // Le numéro de catalogue de l'édition (colonne `albums.catalog_number`,
        // remplie par la balise CATALOGNUMBER ou par Discogs) : l'onglet
        // Versions de la fiche album le montre (web#3064). Inconnu ou vide :
        // la clé est ABSENTE, comme `max_channels`.
        if let Some(cat) = a
            .catalog_number
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            obj.insert("catalog_number".into(), json!(cat));
        }
    }
    v
}

/// Les canaux de toutes les versions des œuvres `oeuvres`, en UNE requête.
fn canaux_des_versions(state: &AppState, oeuvres: &[&Oeuvre]) -> super::albums::CanauxParAlbum {
    let ids: Vec<i64> = oeuvres
        .iter()
        .flat_map(|o| o.versions.iter().map(|v| v.id))
        .collect();
    let repo = AlbumRepo::with_backend(state.backend.clone());
    super::albums::canaux_par_album(&repo, &ids)
}

fn oeuvre_json(
    o: &Oeuvre,
    albums: &HashMap<i64, &Album>,
    canaux: &super::albums::CanauxParAlbum,
) -> Value {
    let preferee = albums.get(&o.preferee).map(|a| {
        let mut j = a.to_json();
        if let Some(obj) = j.as_object_mut() {
            super::albums::inserer_max_channels(obj, a, canaux);
        }
        j
    });
    json!({
        "work_id": o.work_id,
        "method": o.methode.nom(),
        "count": o.versions.len(),
        "preferred_id": o.preferee,
        "preferred": preferee,
        "versions": o
            .versions
            .iter()
            .filter_map(|v| albums.get(&v.id).map(|a| version_json(a, &v.nom, canaux)))
            .collect::<Vec<_>>(),
    })
}

/// Toute la bibliothèque visible (masqués exclus), triée par titre.
fn albums_visibles(state: &AppState) -> Vec<Album> {
    AlbumRepo::with_backend(state.backend.clone())
        .list_filtered(i64::MAX, 0, "title", "asc", None, None, None, false, None)
        .ou_defaut_journalise()
}

/// Les albums masqués à la main (#1391), le même ensemble que celui que
/// `hidden_albums_excluded()` retire de la grille.
fn albums_masques(state: &AppState) -> Vec<i64> {
    state
        .backend
        .query_many(
            "SELECT item_id FROM hidden_items WHERE item_type = 'album'",
            &[],
        )
        .ou_defaut_journalise()
        .iter()
        .filter_map(|l| l.first().and_then(|x| x.as_i64()))
        .collect()
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
    let masters = masters_des_albums(&state);
    let coffrets = albums_coffrets(&state);
    let candidats: Vec<Candidat> = albums
        .iter()
        .filter_map(|a| candidat(a, &versions, &masters, &coffrets))
        .collect();
    let oeuvres = regrouper(&candidats, actif, &distinctes(&state), &preferees(&state));
    let par_id: HashMap<i64, &Album> = albums.iter().filter_map(|a| Some((a.id?, a))).collect();
    let limite = page.limit.clamp(1, 1000) as usize;
    let debut = page.offset.max(0) as usize;
    let de_la_page: Vec<&Oeuvre> = oeuvres.iter().skip(debut).take(limite).collect();
    let canaux = canaux_des_versions(&state, &de_la_page);
    let items: Vec<Value> = de_la_page
        .into_iter()
        .map(|o| oeuvre_json(o, &par_id, &canaux))
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

/// `GET /library/albums/works/members` — les seules œuvres de PLUSIEURS
/// versions, sous forme compacte : ce qu'il faut à une grille qui a déjà ses
/// albums pour n'en montrer qu'un par œuvre.
pub(crate) async fn membres_des_oeuvres(State(state): State<AppState>) -> Json<Value> {
    let actif = regroupement_actif(&state);
    if !actif {
        return Json(json!({ "enabled": false, "works": [] }));
    }
    let albums = albums_visibles(&state);
    let versions = versions_des_albums(&state);
    let masters = masters_des_albums(&state);
    let coffrets = albums_coffrets(&state);
    let candidats: Vec<Candidat> = albums
        .iter()
        .filter_map(|a| candidat(a, &versions, &masters, &coffrets))
        .collect();
    let oeuvres = regrouper(&candidats, true, &distinctes(&state), &preferees(&state));
    let works: Vec<Value> = oeuvres
        .iter()
        .filter(|o| o.versions.len() > 1)
        .map(|o| {
            json!({
                "work_id": o.work_id,
                "preferred_id": o.preferee,
                "ids": o.versions.iter().map(|v| v.id).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({ "enabled": true, "works": works }))
}

/// `GET /library/albums/{id}/versions` — l'œuvre d'un album.
///
/// Les candidats sont les albums du même artiste (clé 3), puis, de proche en
/// proche, ceux qui partagent un groupe MusicBrainz ou un master Discogs avec
/// un candidat de la composante (clés 1 et 2, fil 3012) : ce sont les seuls
/// que le regroupement peut rapprocher de l'album, et la fiche forme ainsi la
/// même œuvre que la grille.
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
    let masters = masters_des_albums(&state);
    let mut albums: BTreeMap<i64, Album> = BTreeMap::new();
    albums.insert(id, album.clone());
    // Les albums d'où partent les clés à suivre : l'album, et ceux du même
    // artiste et du même titre normalisé (la clé 3 peut l'y rattacher).
    let titre = titre_normalise(&album.title);
    let mut a_suivre: Vec<Album> = vec![album.clone()];
    if let Some(a) = album.artist_id {
        for x in repo.list_by_artist(a).ou_defaut_journalise() {
            if let Some(i) = x.id {
                if i != id && !titre.is_empty() && titre_normalise(&x.title) == titre {
                    a_suivre.push(x.clone());
                }
                albums.entry(i).or_insert(x);
            }
        }
    }
    let mut albums_par_master: HashMap<&str, Vec<i64>> = HashMap::new();
    for (album_id, m) in &masters {
        albums_par_master
            .entry(m.as_str())
            .or_default()
            .push(*album_id);
    }
    let mut groupes_vus: HashSet<String> = HashSet::new();
    let mut masters_vus: HashSet<String> = HashSet::new();
    // Borne de sûreté : une composante réelle tient en quelques clés.
    const CLES_MAX: usize = 64;
    while let Some(x) = a_suivre.pop() {
        if groupes_vus.len() + masters_vus.len() >= CLES_MAX {
            tracing::warn!(album = id, "oeuvres_fiche_trop_de_cles");
            break;
        }
        let mut nouveaux: Vec<Album> = Vec::new();
        if let Some(g) = x
            .musicbrainz_release_group_id
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            && groupes_vus.insert(g.to_string())
        {
            nouveaux.extend(repo.list_by_release_group(g).ou_defaut_journalise());
        }
        if let Some(m) = x.id.and_then(|i| masters.get(&i))
            && masters_vus.insert(m.clone())
        {
            for autre in albums_par_master.get(m.as_str()).into_iter().flatten() {
                if !albums.contains_key(autre)
                    && let Some(a) = repo.get(*autre).ok().flatten()
                {
                    nouveaux.push(a);
                }
            }
        }
        for n in nouveaux {
            if let Some(i) = n.id
                && !albums.contains_key(&i)
            {
                a_suivre.push(n.clone());
                albums.insert(i, n);
            } else if n.id.is_some_and(|i| i != id) {
                // Déjà candidat (même artiste) : ses clés restent à suivre.
                a_suivre.push(n);
            }
        }
    }
    // `list_by_release_group` rend aussi les albums masqués : la grille ne
    // les voit pas, la fiche ne doit pas les proposer. L'album demandé reste,
    // même masqué : c'est lui qu'on ouvre.
    for masque in albums_masques(&state) {
        if masque != id {
            albums.remove(&masque);
        }
    }
    let versions = versions_des_albums(&state);
    let coffrets = albums_coffrets(&state);
    let candidats: Vec<Candidat> = albums
        .values()
        .filter_map(|a| candidat(a, &versions, &masters, &coffrets))
        .collect();
    let actif = regroupement_actif(&state);
    let oeuvres = regrouper(&candidats, actif, &distinctes(&state), &preferees(&state));
    let par_id: HashMap<i64, &Album> = albums.iter().map(|(k, v)| (*k, v)).collect();
    let oeuvre = oeuvres
        .iter()
        .find(|o| o.versions.iter().any(|v| v.id == id))
        .ok_or(AppError::not_found("album not found"))?;
    let canaux = canaux_des_versions(&state, &[oeuvre]);
    let mut corps = oeuvre_json(oeuvre, &par_id, &canaux);
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
