//! `GET /home/because-you-like` — « Parce que vous aimez <artiste> »
//! (renesenses/tune-web-client#3118, demande de FabienM, fil 2864 ; go de
//! Bertrand le 16/11/2026 pour la rc13).
//!
//! Jusqu'à [`MAX_PLAYLISTS`] playlists générées de [`TITRES_PAR_PLAYLIST`]
//! titres, une par artiste parmi les plus écoutés du profil, refaites tous les
//! [`PERIODE_JOURS`] jours — l'esprit d'un « mix pour vous » (décision de
//! Bertrand du 17/11/2026, sur la demande de FabienM, fil 2864 : 20 titres et
//! 3 jours, au lieu de 50 titres et d'un jour).
//!
//! ## Ce qui existait, et ce qui est réutilisé
//!
//! La composition est celle de la radio d'artiste (#5395,
//! `tune_core::playback::radio_artiste::composer_lot`) : environ 20 % de
//! l'artiste de départ puisés dans toute sa discographie, le reste chez ses
//! voisins, sans doublon, sans deux titres du même artiste d'affilée. Cette
//! route n'en change que les SOURCES et le TIRAGE :
//!
//! - **Bibliothèque d'abord.** Les voisins viennent de l'API d'enrichissement
//!   (artistes similaires par MBID, déjà servis à la fiche artiste) quand la
//!   bibliothèque les possède, puis des artistes du même genre. Les services
//!   connectés (Qobuz, TIDAL…) n'interviennent qu'en COMPLÉMENT, quand la
//!   bibliothèque ne remplit pas la playlist — et sous un délai borné.
//! - **Stable sur la période.** Le tirage est graine (période, profil,
//!   artiste) : un rafraîchissement de l'accueil ne rebat pas les cartes. La
//!   période est un bloc fixe de [`PERIODE_JOURS`] jours du calendrier local
//!   ([`periode`]), le même pour tous les profils.
//!
//! ## Le cache, et ce qui s'applique à chaque lecture
//!
//! Le résultat est gardé EN MÉMOIRE, par profil, dans l'état du serveur
//! ([`EtatParceQueVousAimez`]) — pas dans la table des réglages, que
//! `GET /system/config` publie en entier : les playlists d'un profil ne
//! sortent que par cette route, pour ce profil. Un résultat complet vaut pour
//! la période ; un résultat calculé en mode dégradé (enrichissement ou service
//! hors délai) pour [`DUREE_DEGRADE`] ; un résultat vide pour [`DUREE_VIDE`].
//!
//! À CHAQUE lecture, même servie du cache, s'appliquent ce que le profil a
//! décidé depuis : « Pas pour moi » (artistes), titres bannis (locaux et de
//! service), et, pour un profil supervisé, la politique parentale (artistes
//! bloqués, contenus explicites masqués).
//!
//! ## Performance (#8872)
//!
//! Le calcul tourne HORS de l'exécuteur (`spawn_blocking`), dans une tâche
//! détachée : un client qui abandonne ne l'annule pas, et le suivant lit son
//! résultat. Un verrou PAR PROFIL fait qu'un même calcul ne tourne jamais deux
//! fois en même temps ; deux profils ne s'attendent pas. Les recherches par
//! nom passent par `idx_artists_name` (`COLLATE NOCASE`), les autres par
//! `tracks.artist_id` et les clés primaires, sauf la recherche des voisins de
//! même genre, qui ne tourne qu'au calcul (PostgreSQL : index
//! `LOWER(artists.name)` et `tracks.genre`, migration 121).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::facet_filter::Placeholders;
use tune_core::db::politique_parentale::{self, Filtre};
use tune_core::db::suggestions_ecartees::{Ecartees, SuggestionsEcarteesRepo};
use tune_core::playback::radio_artiste::{
    AlbumRadio, Alea, Candidat, SourceRadio, SourceService, composer_lot, normaliser,
};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Playlists au plus (la demande : « jusqu'à 5 »).
pub const MAX_PLAYLISTS: usize = 5;
/// Titres par playlist (décision du 17/11/2026 : 20, au lieu de 50).
pub const TITRES_PAR_PLAYLIST: usize = 20;
/// En dessous, la playlist n'est pas proposée : une « playlist » de trois
/// titres ne vaut pas une carte sur l'accueil. Deux cinquièmes de la taille.
pub const MIN_TITRES: usize = 8;
/// Les playlists sont refaites tous les `PERIODE_JOURS` jours (décision du
/// 17/11/2026 : 3, au lieu de chaque jour).
pub const PERIODE_JOURS: i64 = 3;
/// La fenêtre d'écoute qui désigne les artistes « que vous aimez ». Au-delà,
/// on complète par tout l'historique.
const FENETRE_JOURS: i64 = 90;
/// Artistes candidats lus dans l'historique (avant d'écarter les compilations
/// et les « Pas pour moi »).
const CANDIDATS_GRAINES: i64 = 25;
/// Voisins demandés à l'API d'enrichissement, par artiste.
const VOISINS_CLOUD: usize = 50;
/// Délai accordé à l'API d'enrichissement POUR UNE graine (les graines sont
/// demandées en parallèle).
const DELAI_VOISINS: Duration = Duration::from_secs(8);
/// Délai d'UN complément par les services.
const DELAI_SERVICES: Duration = Duration::from_secs(15);
/// Budget total des compléments par les services, toutes playlists comprises.
const BUDGET_SERVICES: Duration = Duration::from_secs(30);
/// Délai global d'un calcul : au-delà, la réponse est vide et brève.
const DELAI_CALCUL: Duration = Duration::from_secs(120);
/// Ce qu'une requête attend le calcul avant de répondre « en cours » ; le
/// calcul continue et le prochain affichage le lit. Sous le délai de garde
/// des widgets du client web (8 s), pour que la réponse arrive.
const DELAI_REPONSE: Duration = Duration::from_secs(5);
/// Durée de vie d'un résultat calculé en mode dégradé.
pub const DUREE_DEGRADE: Duration = Duration::from_secs(3600);
/// Durée de vie d'un résultat vide (aucune écoute, ou tout filtré).
pub const DUREE_VIDE: Duration = Duration::from_secs(15 * 60);
/// Module du tirage SQL de la période (premier).
const MODULE_TIRAGE: i64 = 1_000_003;
/// Préfixe des clés de réglages d'une version antérieure de cette route, à
/// retirer de la table.
const ANCIEN_PREFIXE_REGLAGES: &str = "home_because_you_like_p";

/// Les « artistes » qui n'en sont pas : une compilation n'est pas un goût.
const PAS_DES_ARTISTES: &[&str] = &[
    "various artists",
    "various",
    "va",
    "v a",
    "artistes divers",
    "artistes varies",
    "divers",
    "varios artistas",
    "verschiedene interpreten",
    "artisti vari",
    "unknown artist",
    "unknown",
    "artiste inconnu",
    "inconnu",
];

#[derive(Debug, Deserialize)]
pub(crate) struct Params {
    limit: Option<usize>,
}

// ── L'état en mémoire ───────────────────────────────────────────────────────

/// La qualité d'un résultat : elle fixe combien de temps il est gardé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Qualite {
    /// Toutes les sources ont répondu : gardé pour la période.
    Complete,
    /// Une source a dépassé son délai : gardé [`DUREE_DEGRADE`].
    Degradee,
    /// Rien à proposer : gardé [`DUREE_VIDE`].
    Vide,
}

#[derive(Debug, Clone)]
struct EntreeCache {
    periode: String,
    playlists: Arc<Vec<Value>>,
    /// `None` : jusqu'à la fin de la période.
    expire: Option<Instant>,
    /// Le profil était-il supervisé au calcul ? Ses graines n'en dépendent :
    /// une politique posée ou retirée depuis invalide l'entrée.
    supervise: bool,
}

impl EntreeCache {
    fn valide(&self, periode: &str, maintenant: Instant, supervise: bool) -> bool {
        self.periode == periode
            && self.supervise == supervise
            && self.expire.is_none_or(|e| maintenant < e)
    }
}

type Case = Arc<tokio::sync::Mutex<Option<EntreeCache>>>;

/// Le cache et les verrous de la route, propres à un état de serveur.
#[derive(Default)]
pub struct EtatParceQueVousAimez {
    profils: std::sync::Mutex<HashMap<i64, Case>>,
    anciennes_cles_retirees: std::sync::atomic::AtomicBool,
}

impl EtatParceQueVousAimez {
    fn case(&self, profil: i64) -> Case {
        let mut m = self
            .profils
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.entry(profil).or_default().clone()
    }
}

// ── La période et son tirage ────────────────────────────────────────────────

/// La période qui contient `date` : le premier jour de son bloc de
/// [`PERIODE_JOURS`] jours, compté depuis l'ère commune — des blocs FIXES, et
/// non « trois jours depuis le dernier calcul », pour que la période ne
/// dépende ni d'un redémarrage ni de l'heure de la première visite. La date
/// est celle du fuseau local du serveur (un conteneur sans fuseau compte en
/// UTC).
pub(crate) fn periode(date: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let decalage = i64::from(date.num_days_from_ce()).rem_euclid(PERIODE_JOURS);
    (date - chrono::Duration::days(decalage))
        .format("%Y-%m-%d")
        .to_string()
}

/// FNV-1a 64 bits : un hachage STABLE d'une version de Rust à l'autre (le
/// `DefaultHasher` ne le promet pas), pour que la graine de la période le
/// reste.
fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.as_bytes().iter().chain(std::iter::once(&0x1f)) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Le tirage SQL de la période : `(id * a + b) % MODULE_TIRAGE`. Un ordre
/// pseudo-aléatoire, mais le MÊME toute la période, sur les deux moteurs, sans
/// `RANDOM()`.
#[derive(Debug, Clone, Copy)]
struct TirageDuJour {
    a: i64,
    b: i64,
}

impl TirageDuJour {
    fn new(jour: &str, profil: i64) -> Self {
        let h = fnv(&[jour, &profil.to_string(), "tirage"]);
        TirageDuJour {
            a: 1 + (h % 999_983) as i64,
            b: ((h >> 24) % MODULE_TIRAGE as u64) as i64,
        }
    }

    /// L'expression d'ordre sur une colonne entière. `CAST` : un `INTEGER`
    /// PostgreSQL multiplié par un grand nombre déborderait.
    fn ordre(&self, colonne: &str) -> String {
        format!(
            "((CAST({colonne} AS BIGINT) * {}) + {}) % {MODULE_TIRAGE}",
            self.a, self.b
        )
    }
}

fn alea_de(jour: &str, profil: i64, artiste: &str) -> Alea {
    Alea::fixe(fnv(&[jour, &profil.to_string(), &normaliser(artiste)]))
}

/// « Ce nom, sans tenir compte de la casse », sous une forme qui profite de
/// `idx_artists_name` (`name COLLATE NOCASE`) en SQLite. `marque` : le
/// marqueur du paramètre.
fn nom_egal(engine: Engine, colonne: &str, marque: &str) -> String {
    match engine {
        Engine::Sqlite => format!("{colonne} = {marque} COLLATE NOCASE"),
        Engine::Postgres => format!("LOWER({colonne}) = LOWER({marque})"),
    }
}

// ── Les filtres du profil ───────────────────────────────────────────────────

/// La politique parentale du profil, quand il est supervisé.
pub(crate) struct Parental {
    filtre: Filtre,
    masquer_explicite: bool,
    /// Les artistes bloqués PAR IDENTIFIANT seul : leur nom se lit dans la
    /// bibliothèque (voir [`charger_filtres`]).
    ids_bloques: Vec<i64>,
}

fn parental(db: &Arc<dyn DbBackend>, profil: i64) -> Option<Parental> {
    match politique_parentale::lire(db, profil) {
        Ok(Some(p)) => Some(Parental {
            filtre: Filtre::depuis(&p.politique),
            masquer_explicite: p.politique.masquer_explicite,
            ids_bloques: p
                .politique
                .artistes_bloques
                .iter()
                .filter_map(|a| a.id)
                .collect(),
        }),
        Ok(None) => None,
        Err(e) if politique_parentale::table_absente(&e) => None,
        Err(e) => {
            // Une politique illisible ne doit pas tout laisser passer : on
            // retient le plus strict qu'on sache appliquer.
            tracing::warn!(profil, error = %e, "parce_que_vous_aimez_politique_illisible");
            Some(Parental {
                filtre: Filtre::depuis(&Default::default()),
                masquer_explicite: true,
                ids_bloques: Vec::new(),
            })
        }
    }
}

/// Tout ce que le profil a décidé et qui retire un titre ou une playlist, lu
/// au moment de répondre.
pub(crate) struct Filtres {
    ecartees: Ecartees,
    bannis_locaux: HashSet<i64>,
    bannis_services: HashSet<(String, String)>,
    parental: Option<Parental>,
    explicites_locaux: HashSet<i64>,
    /// Pistes locales d'un artiste bloqué : artiste de la piste, de l'album ou
    /// balise `ALBUMARTIST`, par identifiant comme par nom
    /// (`politique_parentale::pistes_locales_bloquees`).
    pistes_bloquees: HashSet<i64>,
    /// Noms (normalisés) des artistes bloqués par identifiant seul : couvrent
    /// les graines, les voisins et les titres de service, qui n'ont qu'un nom.
    noms_bloques_par_id: HashSet<String>,
}

fn ids_locaux(playlists: &[Value]) -> Vec<i64> {
    let mut ids: Vec<i64> = playlists
        .iter()
        .flat_map(|p| p["tracks"].as_array().into_iter().flatten())
        .filter(|t| t["source"].as_str().unwrap_or("local") == "local")
        .filter_map(|t| t["track_id"].as_i64())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn charger_filtres(db: &Arc<dyn DbBackend>, profil: i64, playlists: &[Value]) -> Filtres {
    let ids = ids_locaux(playlists);
    let caches = tune_core::db::hidden_repo::HiddenRepo::with_backend(db.clone());
    let bannis_locaux = caches.banned_track_ids(profil, &ids).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "parce_que_vous_aimez_bannis_illisibles");
        HashSet::new()
    });
    let sources: HashSet<String> = playlists
        .iter()
        .flat_map(|p| p["tracks"].as_array().into_iter().flatten())
        .filter_map(|t| t["source"].as_str())
        .filter(|s| *s != "local")
        .map(str::to_owned)
        .collect();
    let mut bannis_services = HashSet::new();
    for s in sources {
        match caches.banned_streaming_ids_for_source(profil, &s) {
            Ok(ids) => bannis_services.extend(ids.into_iter().map(|i| (s.clone(), i))),
            Err(e) => tracing::warn!(error = %e, "parce_que_vous_aimez_bannis_illisibles"),
        }
    }
    let parental = parental(db, profil);
    let explicites_locaux = match &parental {
        Some(p) if p.masquer_explicite && !ids.is_empty() => {
            let liste = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            db.query_many(
                &format!("SELECT id FROM tracks WHERE explicit = 1 AND id IN ({liste})"),
                &[],
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect()
        }
        _ => HashSet::new(),
    };
    let (pistes_bloquees, noms_bloques_par_id) = match &parental {
        Some(p) if p.filtre.bloque_des_artistes() => {
            let pistes = politique_parentale::pistes_locales_bloquees(db, &p.filtre, &ids)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "parce_que_vous_aimez_pistes_bloquees_illisibles");
                    HashSet::new()
                });
            let noms = if p.ids_bloques.is_empty() {
                HashSet::new()
            } else {
                let liste = p
                    .ids_bloques
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                db.query_many(
                    &format!("SELECT name FROM artists WHERE id IN ({liste})"),
                    &[],
                )
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| r.first().and_then(|v| v.as_string()))
                .map(|n| politique_parentale::normaliser(&n))
                .filter(|n| !n.is_empty())
                .collect()
            };
            (pistes, noms)
        }
        _ => (HashSet::new(), HashSet::new()),
    };
    Filtres {
        ecartees: SuggestionsEcarteesRepo::with_backend(db.clone()).ensemble(profil),
        bannis_locaux,
        bannis_services,
        parental,
        explicites_locaux,
        pistes_bloquees,
        noms_bloques_par_id,
    }
}

impl Filtres {
    fn artiste_refuse(&self, nom: &str) -> bool {
        self.ecartees.artiste(nom)
            || self.parental.as_ref().is_some_and(|p| {
                p.filtre.artiste_bloque(None, Some(nom))
                    || self
                        .noms_bloques_par_id
                        .contains(&politique_parentale::normaliser(nom))
            })
    }

    fn titre_refuse(&self, t: &Value) -> bool {
        if t["artist_name"]
            .as_str()
            .is_some_and(|a| self.artiste_refuse(a))
        {
            return true;
        }
        let explicite_masque = self.parental.as_ref().is_some_and(|p| p.masquer_explicite);
        match t["source"].as_str().unwrap_or("local") {
            "local" => t["track_id"].as_i64().is_some_and(|id| {
                self.bannis_locaux.contains(&id)
                    || self.pistes_bloquees.contains(&id)
                    || (explicite_masque && self.explicites_locaux.contains(&id))
            }),
            source => {
                let id = t["source_id"].as_str().unwrap_or_default();
                self.bannis_services
                    .contains(&(source.to_string(), id.to_string()))
                    || (explicite_masque && t["explicit"].as_bool() == Some(true))
            }
        }
    }
}

/// Applique les filtres du profil à des playlists composées (ou lues du
/// cache) : titres et playlists retirés, comptes et pochettes recalculés.
pub(crate) fn appliquer(playlists: &[Value], f: &Filtres) -> Vec<Value> {
    playlists
        .iter()
        .filter(|p| !p["artist"].as_str().is_some_and(|a| f.artiste_refuse(a)))
        .filter_map(|p| {
            let titres: Vec<Value> = p["tracks"]
                .as_array()?
                .iter()
                .filter(|t| !f.titre_refuse(t))
                .cloned()
                .collect();
            if titres.len() < MIN_TITRES {
                return None;
            }
            let mut q = p.clone();
            let o = q.as_object_mut()?;
            let duree: i64 = titres
                .iter()
                .filter_map(|t| t["duration_ms"].as_i64())
                .sum();
            let mut pochettes: Vec<String> = Vec::new();
            for c in titres.iter().filter_map(|t| t["cover_path"].as_str()) {
                if !c.is_empty() && pochettes.len() < 4 && !pochettes.iter().any(|x| x == c) {
                    pochettes.push(c.to_string());
                }
            }
            let voisins: Vec<Value> = p["similar_artists"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| !v.as_str().is_some_and(|a| f.artiste_refuse(a)))
                .cloned()
                .collect();
            o.insert("track_count".into(), json!(titres.len()));
            o.insert("duration_ms".into(), json!(duree));
            o.insert("covers".into(), json!(pochettes));
            o.insert("similar_artists".into(), json!(voisins));
            o.insert("tracks".into(), Value::Array(titres));
            Some(q)
        })
        .collect()
}

// ── Les artistes de départ ──────────────────────────────────────────────────

fn depuis_jours(engine: Engine, jours: i64) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.since_days("listened_at", jours),
        Engine::Postgres => PostgresDialect.since_days("listened_at", jours),
    }
}

/// Les artistes les plus écoutés du profil. Sans politique parentale : ses
/// écoutes, plus les anciennes qui ne portent aucun profil (la grande
/// majorité, voir `history_repo::plays_for_tracks`). Profil SUPERVISÉ : ses
/// seules écoutes, jamais celles des autres. Sans radio ni podcast.
fn sql_graines(engine: Engine, profil: i64, fenetre: Option<i64>, strict: bool) -> String {
    let date = fenetre
        .map(|j| format!(" AND {}", depuis_jours(engine, j)))
        .unwrap_or_default();
    let profil_sql = if strict {
        format!("profile_id = {profil}")
    } else {
        format!("(profile_id IS NULL OR profile_id = {profil})")
    };
    format!(
        "SELECT MIN(artist_name), COUNT(*) AS plays FROM listen_history \
         WHERE artist_name IS NOT NULL AND TRIM(artist_name) <> '' \
         AND {musique} AND {profil_sql}{date} \
         GROUP BY LOWER(artist_name) \
         ORDER BY plays DESC, LOWER(artist_name) \
         LIMIT {CANDIDATS_GRAINES}",
        musique = tune_core::db::history_repo::ECOUTE_DE_MUSIQUE,
    )
}

fn est_une_compilation(nom: &str) -> bool {
    PAS_DES_ARTISTES.contains(&normaliser(nom).as_str())
}

/// Jusqu'à `n` artistes de départ, les plus écoutés d'abord : la fenêtre
/// récente, complétée par tout l'historique. Ni compilation, ni artiste
/// écarté par « Pas pour moi », ni artiste bloqué par la politique parentale.
pub(crate) fn graines(
    db: &Arc<dyn DbBackend>,
    profil: i64,
    filtres: &Filtres,
    n: usize,
) -> Vec<(String, i64)> {
    let strict = filtres.parental.is_some();
    let mut vues: HashSet<String> = HashSet::new();
    let mut sortie = Vec::new();
    for fenetre in [Some(FENETRE_JOURS), None] {
        if sortie.len() >= n {
            break;
        }
        let sql = sql_graines(db.engine(), profil, fenetre, strict);
        let lignes = match db.query_many(&sql, &[]) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "parce_que_vous_aimez_historique_illisible");
                Vec::new()
            }
        };
        for l in lignes {
            let Some(nom) = l.first().and_then(|v| v.as_string()) else {
                continue;
            };
            let nom = nom.trim().to_string();
            let plays = l.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            if nom.is_empty() || est_une_compilation(&nom) || filtres.artiste_refuse(&nom) {
                continue;
            }
            if !vues.insert(normaliser(&nom)) {
                continue;
            }
            sortie.push((nom, plays));
            if sortie.len() >= n {
                break;
            }
        }
    }
    sortie
}

// ── Les sources ─────────────────────────────────────────────────────────────

/// Les voisins de l'API d'enrichissement, demandés UNE fois pour toutes les
/// graines, avant la composition. `presents` : quand il est posé, seuls les
/// voisins que la bibliothèque possède sont rendus (passe locale).
struct VoisinsPrecharges {
    voisins: Arc<HashMap<String, Vec<String>>>,
    presents: Option<Arc<HashSet<String>>>,
}

#[async_trait::async_trait]
impl SourceRadio for VoisinsPrecharges {
    fn nom(&self) -> String {
        "enrichissement".into()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        self.voisins
            .get(&normaliser(artiste))
            .into_iter()
            .flatten()
            .filter(|v| {
                self.presents
                    .as_ref()
                    .is_none_or(|p| p.contains(&v.trim().to_lowercase()))
            })
            .take(max)
            .cloned()
            .collect()
    }

    async fn titres_de(&self, _artiste: &str, _max: usize) -> Vec<Candidat> {
        Vec::new()
    }
}

/// Ce que la bibliothèque a déjà répondu pendant ce calcul : la passe de
/// complément ne refait pas les requêtes de la passe locale.
#[derive(Default)]
struct Memo {
    voisins: std::sync::Mutex<HashMap<(String, usize), Vec<String>>>,
    titres: std::sync::Mutex<HashMap<(String, usize), Vec<Candidat>>>,
    albums: std::sync::Mutex<HashMap<String, Vec<AlbumRadio>>>,
    titres_album: std::sync::Mutex<HashMap<String, Vec<Candidat>>>,
}

fn memo_lire<K: std::hash::Hash + Eq, V: Clone>(
    m: &std::sync::Mutex<HashMap<K, V>>,
    k: &K,
) -> Option<V> {
    m.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(k)
        .cloned()
}

fn memo_ecrire<K: std::hash::Hash + Eq, V>(m: &std::sync::Mutex<HashMap<K, V>>, k: K, v: V) {
    m.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(k, v);
}

/// La bibliothèque, tirée par le [`TirageDuJour`] (de la période) au lieu de
/// `RANDOM()`, avec les titres bannis DU PROFIL APPELANT : les voisins de
/// même genre, les titres d'un artiste, et la discographie de l'artiste de
/// départ (albums dont il est l'artiste d'album, triés par identifiant puis
/// mélangés par l'`Alea` graine).
#[derive(Clone)]
struct BibliothequeDuJour {
    db: Arc<dyn DbBackend>,
    tirage: TirageDuJour,
    profil: i64,
    /// Politique parentale « masquer les contenus explicites ».
    sans_explicite: bool,
    memo: Arc<Memo>,
}

impl BibliothequeDuJour {
    /// Les titres bannis du profil appelant, et les explicites quand sa
    /// politique les masque. Alias `t`.
    fn sans_bannis(&self) -> String {
        let bannis = tune_core::db::facet_filter::banned_tracks_excluded(self.profil);
        if self.sans_explicite {
            format!("{bannis} AND COALESCE(t.explicit, 0) <> 1")
        } else {
            bannis
        }
    }

    fn en_candidats(
        lignes: Vec<Vec<tune_core::db::backend::SqlValue>>,
        album: Option<&str>,
    ) -> Vec<Candidat> {
        lignes
            .into_iter()
            .filter_map(|r| {
                Some(Candidat::Local {
                    track_id: r.first()?.as_i64().filter(|id| *id > 0)?,
                    titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    artiste: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    album: album
                        .map(str::to_owned)
                        .or_else(|| r.get(3).and_then(|v| v.as_string())),
                    duree_ms: r.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl SourceRadio for BibliothequeDuJour {
    fn nom(&self) -> String {
        "local".into()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        let cle = (normaliser(artiste), max);
        if let Some(v) = memo_lire(&self.memo.voisins, &cle) {
            return v;
        }
        let nom = artiste.trim().to_string();
        let engine = self.db.engine();
        let mut ph = Placeholders::new(engine);
        let sql = format!(
            "SELECT t.genre FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
             WHERE {} AND t.genre IS NOT NULL AND t.genre <> '' \
             GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre LIMIT 1",
            nom_egal(engine, "ar.name", &ph.take())
        );
        let genre = self
            .db
            .query_one(&sql, &[&nom as &dyn ToSqlValue])
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        let voisins: Vec<String> = match genre {
            None => Vec::new(),
            Some(genre) => {
                let mut ph = Placeholders::new(engine);
                let (p1, p2) = (ph.take(), ph.take());
                let sql = format!(
                    "SELECT MIN(ar.name) FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
                     WHERE t.genre = {p1} AND NOT ({egal}) \
                     GROUP BY LOWER(ar.name) ORDER BY {ordre}, LOWER(ar.name) LIMIT {max}",
                    egal = nom_egal(engine, "ar.name", &p2),
                    ordre = self.tirage.ordre("MIN(t.id)"),
                );
                self.db
                    .query_many(&sql, &[&genre as &dyn ToSqlValue, &nom])
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|r| r.first().and_then(|v| v.as_string()))
                    .filter(|n| !est_une_compilation(n))
                    .collect()
            }
        };
        memo_ecrire(&self.memo.voisins, cle, voisins.clone());
        voisins
    }

    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        let cle = (normaliser(artiste), max);
        if let Some(v) = memo_lire(&self.memo.titres, &cle) {
            return v;
        }
        let nom = artiste.trim().to_string();
        let engine = self.db.engine();
        let mut ph = Placeholders::new(engine);
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT) \
             FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE {egal} AND {sans_bannis} \
             ORDER BY {ordre}, t.id LIMIT {max}",
            egal = nom_egal(engine, "ar.name", &ph.take()),
            sans_bannis = self.sans_bannis(),
            ordre = self.tirage.ordre("t.id"),
        );
        let titres = Self::en_candidats(
            self.db
                .query_many(&sql, &[&nom as &dyn ToSqlValue])
                .unwrap_or_default(),
            None,
        );
        memo_ecrire(&self.memo.titres, cle, titres.clone());
        titres
    }

    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        let cle = normaliser(artiste);
        if let Some(v) = memo_lire(&self.memo.albums, &cle) {
            return v;
        }
        let nom = artiste.trim().to_string();
        let engine = self.db.engine();
        let mut ph = Placeholders::new(engine);
        let sql = format!(
            "SELECT al.id, al.title FROM albums al JOIN artists aa ON al.artist_id = aa.id \
             WHERE {} ORDER BY al.id",
            nom_egal(engine, "aa.name", &ph.take())
        );
        let albums: Vec<AlbumRadio> = self
            .db
            .query_many(&sql, &[&nom as &dyn ToSqlValue])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(AlbumRadio {
                    id: r.first()?.as_i64()?.to_string(),
                    titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                })
            })
            .collect();
        memo_ecrire(&self.memo.albums, cle, albums.clone());
        albums
    }

    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        let cle = format!("{}\u{1f}{}", album.id, normaliser(artiste));
        if let Some(v) = memo_lire(&self.memo.titres_album, &cle) {
            return v;
        }
        let Ok(album_id) = album.id.parse::<i64>() else {
            return Vec::new();
        };
        let nom = artiste.trim().to_string();
        let engine = self.db.engine();
        let mut ph = Placeholders::new(engine);
        let (p1, p2) = (ph.take(), ph.take());
        let sql = format!(
            "SELECT t.id, t.title, ar.name, NULL, CAST(t.duration_ms AS BIGINT) FROM tracks t \
             JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.album_id = {p1} AND {egal} AND {sans_bannis} ORDER BY t.id",
            egal = nom_egal(engine, "ar.name", &p2),
            sans_bannis = self.sans_bannis(),
        );
        let titres = Self::en_candidats(
            self.db
                .query_many(&sql, &[&album_id as &dyn ToSqlValue, &nom])
                .unwrap_or_default(),
            Some(&album.titre),
        );
        memo_ecrire(&self.memo.titres_album, cle, titres.clone());
        titres
    }
}

/// Une source dont les réponses passent par les refus du profil : un artiste
/// écarté ou bloqué n'est ni un voisin ni une source de titres, et un titre
/// de service explicite est retiré quand la politique les masque. Filtrer
/// AVANT la composition laisse `composer_lot` combler avec d'autres titres.
struct SansRefus {
    source: Box<dyn SourceRadio>,
    filtres: Arc<Filtres>,
}

impl SansRefus {
    fn garder(&self, c: &Candidat) -> bool {
        if self.filtres.artiste_refuse(c.artiste()) {
            return false;
        }
        match c {
            Candidat::Service { piste, .. } => {
                !(piste.explicit
                    && self
                        .filtres
                        .parental
                        .as_ref()
                        .is_some_and(|p| p.masquer_explicite))
            }
            Candidat::Local { .. } => true,
        }
    }
}

#[async_trait::async_trait]
impl SourceRadio for SansRefus {
    fn nom(&self) -> String {
        self.source.nom()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        let mut v = self.source.artistes_similaires(artiste, max).await;
        v.retain(|n| !self.filtres.artiste_refuse(n));
        v
    }

    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        let mut v = self.source.titres_de(artiste, max).await;
        v.retain(|c| self.garder(c));
        v
    }

    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        self.source.albums_de(artiste).await
    }

    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        let mut v = self.source.titres_album(artiste, album).await;
        v.retain(|c| self.garder(c));
        v
    }
}

/// Parmi `noms`, ceux que la bibliothèque possède (avec au moins un titre),
/// en minuscules. Par paquets : un `IN` de mille marqueurs ne passe pas
/// partout.
fn presents_en_bibliotheque(db: &Arc<dyn DbBackend>, noms: &HashSet<String>) -> HashSet<String> {
    let noms: Vec<String> = noms.iter().cloned().collect();
    let engine = db.engine();
    let mut sortie = HashSet::new();
    for paquet in noms.chunks(200) {
        let mut ph = Placeholders::new(engine);
        let marques: Vec<String> = paquet.iter().map(|_| ph.take()).collect();
        let colonne = match engine {
            Engine::Sqlite => "ar.name COLLATE NOCASE",
            Engine::Postgres => "LOWER(ar.name)",
        };
        let sql = format!(
            "SELECT DISTINCT LOWER(ar.name) FROM artists ar \
             WHERE {colonne} IN ({}) \
             AND EXISTS (SELECT 1 FROM tracks t WHERE t.artist_id = ar.id)",
            marques.join(", ")
        );
        let params: Vec<&dyn ToSqlValue> = paquet.iter().map(|n| n as &dyn ToSqlValue).collect();
        match db.query_many(&sql, &params) {
            Ok(lignes) => sortie.extend(
                lignes
                    .into_iter()
                    .filter_map(|r| r.first().and_then(|v| v.as_string())),
            ),
            Err(e) => tracing::warn!(error = %e, "parce_que_vous_aimez_voisins_illisibles"),
        }
    }
    sortie
}

/// Les voisins de chaque graine selon l'API d'enrichissement, toutes en
/// parallèle, chacune sous [`DELAI_VOISINS`]. Rend aussi si une graine a
/// dépassé son délai (résultat dégradé). Hors ligne : aucun voisin, et la
/// bibliothèque répond seule (même genre).
async fn voisins_du_cloud(
    db: &Arc<dyn DbBackend>,
    graines: &[(String, i64)],
) -> (HashMap<String, Vec<String>>, bool) {
    let reponses = futures_util::future::join_all(graines.iter().map(|(nom, _)| async move {
        let r = tokio::time::timeout(
            DELAI_VOISINS,
            tune_core::playback::auto_dj::similar_artist_names(db, nom, VOISINS_CLOUD),
        )
        .await;
        (normaliser(nom), r.ok())
    }))
    .await;
    let mut degrade = false;
    let mut voisins = HashMap::new();
    for (cle, r) in reponses {
        match r {
            Some(v) => {
                voisins.insert(cle, v);
            }
            None => {
                tracing::info!(artiste = %cle, "parce_que_vous_aimez_enrichissement_hors_delai");
                degrade = true;
            }
        }
    }
    (voisins, degrade)
}

/// Les services actifs ET connectés, par ordre alphabétique, comme sources de
/// complément (même construction que `radio_artiste::sources_de_la_radio`),
/// avec les titres que LE PROFIL APPELANT a bannis chez chacun.
async fn sources_services(state: &AppState, profil: i64) -> Vec<Box<dyn SourceRadio>> {
    let (mut noms, par_nom) = {
        let reg = state.orchestrator.services.lock().await;
        let noms = reg.list();
        let par_nom: HashMap<String, _> = noms
            .iter()
            .filter_map(|n| reg.get(n).map(|s| (n.clone(), s)))
            .collect();
        (noms, par_nom)
    };
    noms.sort();
    let caches = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    let mut sources: Vec<Box<dyn SourceRadio>> = Vec::new();
    for nom in noms {
        let Some(service) = par_nom.get(&nom).cloned() else {
            continue;
        };
        if !service.read().await.utilisable().await {
            continue;
        }
        let bannis = caches
            .banned_streaming_ids_for_source(profil, &nom)
            .unwrap_or_default();
        sources.push(Box::new(SourceService::new(&nom, service, bannis, None)));
    }
    sources
}

// ── La composition ──────────────────────────────────────────────────────────

/// Ce que la composition reçoit : tout est déjà résolu, rien n'y part sur le
/// réseau sauf les services de `services` — d'où des épreuves sans réseau.
pub(crate) struct Entrees<'a> {
    pub db: &'a Arc<dyn DbBackend>,
    pub profil: i64,
    /// La période ([`periode`]) : la graine du tirage.
    pub jour: &'a str,
    pub graines: &'a [(String, i64)],
    pub voisins: HashMap<String, Vec<String>>,
    pub services: Vec<Box<dyn SourceRadio>>,
    /// Les refus du profil, appliqués aux sources.
    pub filtres: Arc<Filtres>,
}

/// Ce que rend la composition : les playlists (AVANT les filtres du profil)
/// et si un complément par les services a dépassé son délai.
pub(crate) struct Composition {
    pub playlists: Vec<Value>,
    pub degradee: bool,
}

/// Compose les playlists, dans l'ordre des graines. Un titre déjà pris par une
/// playlist n'entre pas dans les suivantes.
pub(crate) async fn composer(e: Entrees<'_>) -> Composition {
    let tirage = TirageDuJour::new(e.jour, e.profil);
    let voisins = Arc::new(e.voisins);
    let tous_les_voisins: HashSet<String> = voisins
        .values()
        .flatten()
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    let presents = Arc::new(presents_en_bibliotheque(e.db, &tous_les_voisins));
    let bibliotheque = BibliothequeDuJour {
        db: e.db.clone(),
        tirage,
        profil: e.profil,
        sans_explicite: e
            .filtres
            .parental
            .as_ref()
            .is_some_and(|p| p.masquer_explicite),
        memo: Arc::new(Memo::default()),
    };
    let filtres = e.filtres.clone();
    let sans_refus = move |source: Box<dyn SourceRadio>| -> Box<dyn SourceRadio> {
        Box::new(SansRefus {
            source,
            filtres: filtres.clone(),
        })
    };

    // Passe LOCALE : les voisins du cloud que la bibliothèque possède, puis
    // ceux du même genre ; leurs titres dans la bibliothèque.
    let locales: Vec<Box<dyn SourceRadio>> = vec![
        sans_refus(Box::new(VoisinsPrecharges {
            voisins: voisins.clone(),
            presents: Some(presents),
        })),
        sans_refus(Box::new(bibliotheque.clone())),
    ];
    // Passe de COMPLÉMENT : tous les voisins du cloud, la bibliothèque
    // d'abord pour les titres (mêmes réponses, mémorisées), les services
    // ensuite.
    let avec_services = !e.services.is_empty();
    let mut completes: Vec<Box<dyn SourceRadio>> = vec![
        sans_refus(Box::new(VoisinsPrecharges {
            voisins: voisins.clone(),
            presents: None,
        })),
        sans_refus(Box::new(bibliotheque)),
    ];
    completes.extend(e.services.into_iter().map(sans_refus));

    let debut = Instant::now();
    let mut degradee = false;
    let mut deja: HashSet<String> = HashSet::new();
    let mut playlists = Vec::new();
    for (artiste, plays) in e.graines {
        let mut lot = composer_lot(
            artiste,
            &locales,
            TITRES_PAR_PLAYLIST,
            &deja,
            None,
            true,
            &mut alea_de(e.jour, e.profil, artiste),
        )
        .await
        .candidats;
        if lot.len() < TITRES_PAR_PLAYLIST && avec_services {
            if debut.elapsed() >= BUDGET_SERVICES {
                degradee = true;
            } else {
                let mut alea = alea_de(e.jour, e.profil, artiste);
                let complement = composer_lot(
                    artiste,
                    &completes,
                    TITRES_PAR_PLAYLIST,
                    &deja,
                    None,
                    true,
                    &mut alea,
                );
                match tokio::time::timeout(DELAI_SERVICES, complement).await {
                    Ok(c) if c.candidats.len() > lot.len() => lot = c.candidats,
                    Ok(_) => {}
                    Err(_) => {
                        degradee = true;
                        tracing::info!(artiste = %artiste, "parce_que_vous_aimez_services_hors_delai");
                    }
                }
            }
        }
        if lot.len() < MIN_TITRES {
            tracing::debug!(artiste = %artiste, titres = lot.len(), "parce_que_vous_aimez_trop_court");
            continue;
        }
        deja.extend(lot.iter().map(Candidat::cle));
        playlists.push(en_playlist(e.db, artiste, *plays, &lot));
    }
    Composition {
        playlists,
        degradee,
    }
}

/// L'album et la pochette des titres locaux : `Candidat::Local` ne les porte
/// pas. Une requête par clé primaire.
fn albums_des_titres(
    db: &Arc<dyn DbBackend>,
    ids: &[i64],
) -> HashMap<i64, (Option<i64>, Option<String>)> {
    if ids.is_empty() {
        return HashMap::new();
    }
    // Des entiers lus en base, jamais une chaîne du client.
    let liste = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT t.id, t.album_id, al.cover_path FROM tracks t \
         LEFT JOIN albums al ON al.id = t.album_id WHERE t.id IN ({liste})"
    );
    db.query_many(&sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some((
                r.first()?.as_i64()?,
                (
                    r.get(1).and_then(|v| v.as_i64()),
                    r.get(2)
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.is_empty()),
                ),
            ))
        })
        .collect()
}

fn en_playlist(db: &Arc<dyn DbBackend>, artiste: &str, plays: i64, lot: &[Candidat]) -> Value {
    let ids: Vec<i64> = lot
        .iter()
        .filter_map(|c| match c {
            Candidat::Local { track_id, .. } => Some(*track_id),
            Candidat::Service { .. } => None,
        })
        .collect();
    let albums = albums_des_titres(db, &ids);
    let graine = normaliser(artiste);
    let mut voisins: Vec<String> = Vec::new();
    let mut vus_voisins: HashSet<String> = HashSet::new();
    let mut pochettes: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut duree: i64 = 0;
    let titres: Vec<Value> = lot
        .iter()
        .map(|c| {
            let mut j = c.en_json();
            if let Some(o) = j.as_object_mut() {
                match c {
                    Candidat::Local { track_id, .. } => {
                        if let Some((album_id, cover)) = albums.get(track_id) {
                            o.insert("album_id".into(), json!(album_id));
                            o.insert("cover_path".into(), json!(cover));
                        }
                    }
                    // Le marquage explicite du service, pour la politique
                    // parentale appliquée à la lecture.
                    Candidat::Service { piste, .. } => {
                        o.insert("explicit".into(), json!(piste.explicit));
                    }
                }
            }
            duree += j["duration_ms"].as_i64().unwrap_or(0);
            let nom = c.artiste().trim();
            let cle = normaliser(nom);
            if !nom.is_empty() && cle != graine && voisins.len() < 5 && vus_voisins.insert(cle) {
                voisins.push(nom.to_string());
            }
            if let Some(p) = j["cover_path"].as_str().filter(|p| !p.is_empty())
                && pochettes.len() < 4
                && !pochettes.iter().any(|x| x == p)
            {
                pochettes.push(p.to_string());
            }
            if let Some(s) = j["source"].as_str()
                && !sources.iter().any(|x| x == s)
            {
                sources.push(s.to_string());
            }
            j
        })
        .collect();
    json!({
        "id": format!("pvaa-{:016x}", fnv(&[&graine])),
        "artist": artiste,
        "plays": plays,
        "track_count": titres.len(),
        "duration_ms": duree,
        "similar_artists": voisins,
        "covers": pochettes,
        "sources": sources,
        "tracks": titres,
    })
}

// ── Le calcul, le cache et la route ─────────────────────────────────────────

/// Le calcul complet, pour un profil et une période. Tourne sur un fil
/// bloquant. Les playlists rendues sont déjà filtrées pour le profil.
async fn calculer(state: &AppState, profil: i64, jour: &str) -> (Vec<Value>, Qualite, bool) {
    let filtres = Arc::new(charger_filtres(&state.backend, profil, &[]));
    let supervise = filtres.parental.is_some();
    let graines = graines(&state.backend, profil, &filtres, MAX_PLAYLISTS);
    if graines.is_empty() {
        return (Vec::new(), Qualite::Vide, supervise);
    }
    let (voisins, cloud_degrade) = voisins_du_cloud(&state.backend, &graines).await;
    let services = sources_services(state, profil).await;
    let c = composer(Entrees {
        db: &state.backend,
        profil,
        jour,
        graines: &graines,
        voisins,
        services,
        filtres,
    })
    .await;
    let filtres = charger_filtres(&state.backend, profil, &c.playlists);
    let playlists = appliquer(&c.playlists, &filtres);
    let qualite = if playlists.is_empty() {
        Qualite::Vide
    } else if cloud_degrade || c.degradee {
        Qualite::Degradee
    } else {
        Qualite::Complete
    };
    tracing::info!(
        profil,
        graines = graines.len(),
        playlists = playlists.len(),
        ?qualite,
        "parce_que_vous_aimez_compose"
    );
    (playlists, qualite, supervise)
}

/// Retire de la table des réglages les clés qu'une version antérieure de
/// cette route y écrivait. Une fois par état de serveur.
fn retirer_les_anciennes_cles(state: &AppState) {
    use std::sync::atomic::Ordering;
    if state
        .parce_que_vous_aimez
        .anciennes_cles_retirees
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let mut ph = Placeholders::new(state.backend.engine());
    // `_` est un joker de LIKE : échappé, le motif ne vise que ce préfixe.
    let sql = format!(
        "DELETE FROM settings WHERE key LIKE {} ESCAPE '\\'",
        ph.take()
    );
    let motif = format!("{}%", ANCIEN_PREFIXE_REGLAGES.replace('_', "\\_"));
    if let Err(e) = state.backend.execute(&sql, &[&motif as &dyn ToSqlValue]) {
        tracing::warn!(error = %e, "parce_que_vous_aimez_anciennes_cles");
    }
}

/// Les playlists du profil pour la période : du cache quand il est valide,
/// sinon calculées — une seule fois à la fois pour ce profil.
async fn obtenir(state: AppState, profil: i64, jour: String) -> Arc<Vec<Value>> {
    let case = state.parce_que_vous_aimez.case(profil);
    let mut garde = case.lock().await;
    // Une lecture par clé primaire, hors de l'exécuteur comme le reste.
    let db = state.backend.clone();
    let supervise_maintenant = tokio::task::spawn_blocking(move || parental(&db, profil).is_some())
        .await
        .unwrap_or(true);
    if let Some(e) = garde.as_ref()
        && e.valide(&jour, Instant::now(), supervise_maintenant)
    {
        return e.playlists.clone();
    }
    let handle = tokio::runtime::Handle::current();
    let (etat, j) = (state.clone(), jour.clone());
    let calcul = tokio::task::spawn_blocking(move || {
        retirer_les_anciennes_cles(&etat);
        handle.block_on(calculer(&etat, profil, &j))
    });
    let (playlists, qualite, supervise) = match tokio::time::timeout(DELAI_CALCUL, calcul).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::error!(profil, error = %e, "parce_que_vous_aimez_calcul_interrompu");
            (Vec::new(), Qualite::Vide, supervise_maintenant)
        }
        Err(_) => {
            tracing::warn!(profil, "parce_que_vous_aimez_calcul_hors_delai");
            (Vec::new(), Qualite::Vide, supervise_maintenant)
        }
    };
    let maintenant = Instant::now();
    let expire = match qualite {
        Qualite::Complete => None,
        Qualite::Degradee => Some(maintenant + DUREE_DEGRADE),
        Qualite::Vide => Some(maintenant + DUREE_VIDE),
    };
    let playlists = Arc::new(playlists);
    *garde = Some(EntreeCache {
        periode: jour,
        playlists: playlists.clone(),
        expire,
        supervise,
    });
    playlists
}

pub(crate) async fn because_you_like(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Query(p): Query<Params>,
) -> Result<Json<Value>, AppError> {
    let limite = p.limit.unwrap_or(MAX_PLAYLISTS).clamp(1, MAX_PLAYLISTS);
    let profil = profile.id();
    let jour = periode(chrono::Local::now().date_naive());
    // Une tâche détachée : un client qui abandonne n'annule pas le calcul, et
    // le prochain affichage en lit le résultat.
    let tache = tokio::spawn(obtenir(state.clone(), profil, jour.clone()));
    let brutes = match tokio::time::timeout(DELAI_REPONSE, tache).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::error!(profil, error = %e, "parce_que_vous_aimez_tache_interrompue");
            return Err(AppError::internal("because-you-like computation failed"));
        }
        Err(_) => {
            return Ok(Json(json!({
                "date": jour,
                "refresh_days": PERIODE_JOURS,
                "pending": true,
                "playlists": [],
            })));
        }
    };
    // Les décisions du profil prises depuis le calcul (« Pas pour moi »,
    // titres bannis, politique parentale) s'appliquent à chaque lecture.
    let db = state.backend.clone();
    let filtrees = tokio::task::spawn_blocking(move || {
        let f = charger_filtres(&db, profil, &brutes);
        appliquer(&brutes, &f)
    })
    .await
    .map_err(|e| {
        tracing::error!(profil, error = %e, "parce_que_vous_aimez_filtres_interrompus");
        AppError::internal("because-you-like filtering failed")
    })?;
    let p: Vec<&Value> = filtrees.iter().take(limite).collect();
    // `date` : le premier jour de la période ; les playlists sont refaites
    // `refresh_days` jours plus tard.
    Ok(Json(
        json!({"date": jour, "refresh_days": PERIODE_JOURS, "playlists": p}),
    ))
}

#[cfg(test)]
#[path = "parce_que_vous_aimez_tests_3118.rs"]
mod tests;
