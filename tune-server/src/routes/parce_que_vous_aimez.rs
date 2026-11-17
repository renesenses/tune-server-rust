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
//!   artiste) : un rafraîchissement de l'accueil ne rebat pas les cartes. Le
//!   résultat est de plus gardé dans les réglages jusqu'à la période suivante,
//!   ce qui couvre aussi les réponses des services, qui ne sont pas les nôtres.
//!   La période est un bloc fixe de [`PERIODE_JOURS`] jours du calendrier
//!   local ([`periode`]), le même pour tous les profils.
//!
//! ## Performance (#8872)
//!
//! Le calcul tourne HORS de l'exécuteur (`spawn_blocking`), une seule fois à
//! la fois (verrou), et une seule fois par période et par profil (cache) : une
//! bibliothèque de 500 000 pistes ne tient jamais un fil de travail tokio.
//! Toutes les requêtes passent par des index (`tracks.artist_id`, clés
//! primaires) sauf la recherche des voisins de même genre, qui ne tourne
//! qu'au premier affichage de la période.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::facet_filter::Placeholders;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::suggestions_ecartees::Ecartees;
use tune_core::playback::radio_artiste::{
    AlbumRadio, Alea, Candidat, SourceBibliotheque, SourceRadio, SourceService, composer_lot,
    normaliser,
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
/// Délai TOTAL accordé à l'API d'enrichissement (toutes les graines en
/// parallèle). Hors ligne, l'accueil n'attend pas plus.
const DELAI_VOISINS: Duration = Duration::from_secs(8);
/// Délai d'UN complément par les services.
const DELAI_SERVICES: Duration = Duration::from_secs(15);
/// Budget total des compléments par les services, toutes playlists comprises.
const BUDGET_SERVICES: Duration = Duration::from_secs(30);
/// Format du cache : changer la forme de la réponse impose de l'incrémenter.
const VERSION_CACHE: i64 = 2;
/// Module du tirage SQL de la période (premier).
const MODULE_TIRAGE: i64 = 1_000_003;

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

// ── La période et son tirage ────────────────────────────────────────────────

/// La période qui contient `date` : le premier jour de son bloc de
/// [`PERIODE_JOURS`] jours, compté depuis l'ère commune — des blocs FIXES, et
/// non « trois jours depuis le dernier calcul », pour que la période ne
/// dépende ni d'un redémarrage ni de l'heure de la première visite.
pub(crate) fn periode(date: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let decalage = i64::from(date.num_days_from_ce()).rem_euclid(PERIODE_JOURS);
    (date - chrono::Duration::days(decalage))
        .format("%Y-%m-%d")
        .to_string()
}

/// FNV-1a 64 bits : un hachage STABLE d'une version de Rust à l'autre (le
/// `DefaultHasher` ne le promet pas), pour que la graine de la période le reste.
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

// ── Les artistes de départ ──────────────────────────────────────────────────

fn depuis_jours(engine: Engine, jours: i64) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.since_days("listened_at", jours),
        Engine::Postgres => PostgresDialect.since_days("listened_at", jours),
    }
}

/// Les artistes les plus écoutés du profil : les écoutes portant son
/// `profile_id`, plus les anciennes qui n'en portent aucun (la grande
/// majorité, voir `history_repo::plays_for_tracks`). Sans radio ni podcast.
fn sql_graines(engine: Engine, profil: i64, fenetre: Option<i64>) -> String {
    let date = fenetre
        .map(|j| format!(" AND {}", depuis_jours(engine, j)))
        .unwrap_or_default();
    format!(
        "SELECT MIN(artist_name), COUNT(*) AS plays FROM listen_history \
         WHERE artist_name IS NOT NULL AND TRIM(artist_name) <> '' \
         AND {musique} \
         AND (profile_id IS NULL OR profile_id = {profil}){date} \
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
/// écarté par « Pas pour moi ».
pub(crate) fn graines(
    db: &Arc<dyn DbBackend>,
    profil: i64,
    ecartees: &Ecartees,
    n: usize,
) -> Vec<(String, i64)> {
    let mut vues: HashSet<String> = HashSet::new();
    let mut sortie = Vec::new();
    for fenetre in [Some(FENETRE_JOURS), None] {
        if sortie.len() >= n {
            break;
        }
        let lignes = match db.query_many(&sql_graines(db.engine(), profil, fenetre), &[]) {
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
            if nom.is_empty() || est_une_compilation(&nom) || ecartees.artiste(&nom) {
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

/// La bibliothèque, tirée par le [`TirageDuJour`] (de la période) au lieu de `RANDOM()` :
/// les voisins de même genre et les titres d'un artiste. La discographie de
/// l'artiste de départ (albums triés par identifiant, puis mélangés par
/// l'`Alea` graine) est celle de la radio.
struct BibliothequeDuJour {
    db: Arc<dyn DbBackend>,
    radio: SourceBibliotheque,
    tirage: TirageDuJour,
    profil: i64,
}

impl BibliothequeDuJour {
    fn new(db: Arc<dyn DbBackend>, tirage: TirageDuJour, profil: i64) -> Self {
        BibliothequeDuJour {
            radio: SourceBibliotheque::new(db.clone()),
            db,
            tirage,
            profil,
        }
    }
}

#[async_trait::async_trait]
impl SourceRadio for BibliothequeDuJour {
    fn nom(&self) -> String {
        "local".into()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        let nom = artiste.trim().to_lowercase();
        let mut ph = Placeholders::new(self.db.engine());
        let sql = format!(
            "SELECT t.genre FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
             WHERE LOWER(ar.name) = {} AND t.genre IS NOT NULL AND t.genre <> '' \
             GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre LIMIT 1",
            ph.take()
        );
        let genre = self
            .db
            .query_one(&sql, &[&nom as &dyn ToSqlValue])
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        let Some(genre) = genre else {
            return Vec::new();
        };
        let mut ph = Placeholders::new(self.db.engine());
        let (p1, p2) = (ph.take(), ph.take());
        let sql = format!(
            "SELECT MIN(ar.name) FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.genre = {p1} AND LOWER(ar.name) <> {p2} \
             GROUP BY LOWER(ar.name) ORDER BY {ordre}, LOWER(ar.name) LIMIT {max}",
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

    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        let nom = artiste.trim().to_lowercase();
        let mut ph = Placeholders::new(self.db.engine());
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT) \
             FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE LOWER(ar.name) = {p1} AND {sans_bannis} \
             ORDER BY {ordre}, t.id LIMIT {max}",
            p1 = ph.take(),
            sans_bannis = tune_core::db::facet_filter::banned_tracks_excluded(self.profil),
            ordre = self.tirage.ordre("t.id"),
        );
        self.db
            .query_many(&sql, &[&nom as &dyn ToSqlValue])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(Candidat::Local {
                    track_id: r.first()?.as_i64().filter(|id| *id > 0)?,
                    titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    artiste: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    album: r.get(3).and_then(|v| v.as_string()),
                    duree_ms: r.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect()
    }

    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        self.radio.albums_de(artiste).await
    }

    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        self.radio.titres_album(artiste, album).await
    }
}

/// Parmi `noms`, ceux que la bibliothèque possède (avec au moins un titre),
/// en minuscules. Par paquets : un `IN` de mille marqueurs ne passe pas
/// partout.
fn presents_en_bibliotheque(db: &Arc<dyn DbBackend>, noms: &HashSet<String>) -> HashSet<String> {
    let noms: Vec<String> = noms.iter().cloned().collect();
    let mut sortie = HashSet::new();
    for paquet in noms.chunks(200) {
        let mut ph = Placeholders::new(db.engine());
        let marques: Vec<String> = paquet.iter().map(|_| ph.take()).collect();
        let sql = format!(
            "SELECT DISTINCT LOWER(ar.name) FROM artists ar \
             WHERE LOWER(ar.name) IN ({}) \
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
/// parallèle et sous [`DELAI_VOISINS`]. Hors ligne : aucun, et la
/// bibliothèque répond seule (même genre).
async fn voisins_du_cloud(
    db: &Arc<dyn DbBackend>,
    graines: &[(String, i64)],
) -> HashMap<String, Vec<String>> {
    let demandes = futures_util::future::join_all(graines.iter().map(|(nom, _)| async move {
        (
            normaliser(nom),
            tune_core::playback::auto_dj::similar_artist_names(db, nom, VOISINS_CLOUD).await,
        )
    }));
    match tokio::time::timeout(DELAI_VOISINS, demandes).await {
        Ok(v) => v.into_iter().collect(),
        Err(_) => {
            tracing::info!("parce_que_vous_aimez_enrichissement_hors_delai");
            HashMap::new()
        }
    }
}

/// Les services actifs ET connectés, par ordre alphabétique, comme sources de
/// complément (même construction que `radio_artiste::sources_de_la_radio`).
async fn sources_services(state: &AppState) -> Vec<Box<dyn SourceRadio>> {
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
    let mut sources: Vec<Box<dyn SourceRadio>> = Vec::new();
    for nom in noms {
        let Some(service) = par_nom.get(&nom).cloned() else {
            continue;
        };
        if !service.read().await.utilisable().await {
            continue;
        }
        let mut bannis = HashSet::new();
        tune_core::db::hidden_repo::exclure_les_titres_de_service_bannis(
            &state.backend,
            &nom,
            &mut bannis,
        );
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
    pub ecartees: &'a Ecartees,
}

/// Compose les playlists, dans l'ordre des graines. Un titre déjà pris par une
/// playlist n'entre pas dans les suivantes.
pub(crate) async fn composer(e: Entrees<'_>) -> Vec<Value> {
    let tirage = TirageDuJour::new(e.jour, e.profil);
    let voisins = Arc::new(e.voisins);
    let tous_les_voisins: HashSet<String> = voisins
        .values()
        .flatten()
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    let presents = Arc::new(presents_en_bibliotheque(e.db, &tous_les_voisins));

    // Passe LOCALE : les voisins du cloud que la bibliothèque possède, puis
    // ceux du même genre ; leurs titres dans la bibliothèque.
    let locales: Vec<Box<dyn SourceRadio>> = vec![
        Box::new(VoisinsPrecharges {
            voisins: voisins.clone(),
            presents: Some(presents),
        }),
        Box::new(BibliothequeDuJour::new(e.db.clone(), tirage, e.profil)),
    ];
    // Passe de COMPLÉMENT : tous les voisins du cloud, la bibliothèque
    // d'abord pour les titres, les services ensuite.
    let avec_services = !e.services.is_empty();
    let mut completes: Vec<Box<dyn SourceRadio>> = vec![
        Box::new(VoisinsPrecharges {
            voisins: voisins.clone(),
            presents: None,
        }),
        Box::new(BibliothequeDuJour::new(e.db.clone(), tirage, e.profil)),
    ];
    completes.extend(e.services);

    let debut = Instant::now();
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
        if lot.len() < TITRES_PAR_PLAYLIST && avec_services && debut.elapsed() < BUDGET_SERVICES {
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
                    tracing::info!(artiste = %artiste, "parce_que_vous_aimez_services_hors_delai")
                }
            }
        }
        lot.retain(|c| !e.ecartees.artiste(c.artiste()));
        if lot.len() < MIN_TITRES {
            tracing::debug!(artiste = %artiste, titres = lot.len(), "parce_que_vous_aimez_trop_court");
            continue;
        }
        deja.extend(lot.iter().map(Candidat::cle));
        playlists.push(en_playlist(e.db, artiste, *plays, &lot));
    }
    playlists
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
            if let Candidat::Local { track_id, .. } = c
                && let Some((album_id, cover)) = albums.get(track_id)
                && let Some(o) = j.as_object_mut()
            {
                o.insert("album_id".into(), json!(album_id));
                o.insert("cover_path".into(), json!(cover));
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

// ── Le cache de la période ──────────────────────────────────────────────────

fn cle_cache(profil: i64) -> String {
    format!("home_because_you_like_p{profil}")
}

fn lire_cache(db: &Arc<dyn DbBackend>, profil: i64, jour: &str) -> Option<Vec<Value>> {
    let brut = SettingsRepo::with_backend(db.clone())
        .get(&cle_cache(profil))
        .ok()
        .flatten()?;
    let v: Value = serde_json::from_str(&brut).ok()?;
    (v["v"].as_i64() == Some(VERSION_CACHE) && v["jour"].as_str() == Some(jour))
        .then(|| v["playlists"].as_array().cloned())
        .flatten()
}

fn ecrire_cache(db: &Arc<dyn DbBackend>, profil: i64, jour: &str, playlists: &[Value]) {
    let v = json!({"v": VERSION_CACHE, "jour": jour, "playlists": playlists});
    if let Err(e) = SettingsRepo::with_backend(db.clone()).set(&cle_cache(profil), &v.to_string()) {
        tracing::warn!(error = %e, "parce_que_vous_aimez_cache_non_ecrit");
    }
}

/// Un seul calcul à la fois : deux accueils ouverts en même temps ne lancent
/// pas deux fois le même travail — le second lit le cache du premier.
fn verrou() -> &'static tokio::sync::Mutex<()> {
    static VERROU: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    VERROU.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn reponse(jour: &str, playlists: &[Value], limite: usize) -> Json<Value> {
    let p: Vec<&Value> = playlists.iter().take(limite).collect();
    // `date` : le premier jour de la période ; les playlists sont refaites
    // `refresh_days` jours plus tard.
    Json(json!({"date": jour, "refresh_days": PERIODE_JOURS, "playlists": p}))
}

/// Le calcul complet, pour un profil et une période. Tourne sur un fil bloquant.
async fn calculer(state: &AppState, profil: i64, jour: &str) -> Vec<Value> {
    let ecartees = tune_core::db::suggestions_ecartees::SuggestionsEcarteesRepo::with_backend(
        state.backend.clone(),
    )
    .ensemble(profil);
    let graines = graines(&state.backend, profil, &ecartees, MAX_PLAYLISTS);
    if graines.is_empty() {
        return Vec::new();
    }
    let voisins = voisins_du_cloud(&state.backend, &graines).await;
    let services = sources_services(state).await;
    let playlists = composer(Entrees {
        db: &state.backend,
        profil,
        jour,
        graines: &graines,
        voisins,
        services,
        ecartees: &ecartees,
    })
    .await;
    tracing::info!(
        profil,
        graines = graines.len(),
        playlists = playlists.len(),
        "parce_que_vous_aimez_compose"
    );
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
    if let Some(v) = lire_cache(&state.backend, profil, &jour) {
        return Ok(reponse(&jour, &v, limite));
    }
    let _garde = verrou().lock().await;
    if let Some(v) = lire_cache(&state.backend, profil, &jour) {
        return Ok(reponse(&jour, &v, limite));
    }
    // #8872 — hors de l'exécuteur : les requêtes de la bibliothèque sont
    // synchrones, et une grande bibliothèque ne doit tenir aucun fil tokio.
    let handle = tokio::runtime::Handle::current();
    let (etat, j) = (state.clone(), jour.clone());
    let playlists =
        tokio::task::spawn_blocking(move || handle.block_on(calculer(&etat, profil, &j)))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "parce_que_vous_aimez_calcul_interrompu");
                AppError::internal("because-you-like computation failed")
            })?;
    // Une réponse vide ne se garde pas : le profil qui commence à écouter
    // aujourd'hui doit voir ses playlists sans attendre la période suivante.
    if !playlists.is_empty() {
        ecrire_cache(&state.backend, profil, &jour, &playlists);
    }
    Ok(reponse(&jour, &playlists, limite))
}

#[cfg(test)]
#[path = "parce_que_vous_aimez_tests_3118.rs"]
mod tests;
