//! Écoutes de la communauté entre parenthèses (#6738) : « So What (128) »
//! dans une liste de pistes, le total d'un album, celui d'un artiste.
//!
//! Le site les rend par lot (`POST /api/v1/community/listen-counts`,
//! site-mozaiklabs#330) : jusqu'à [`LOT_MAX`] éléments, chacun nommé par son
//! MBID (enregistrement, release ou artiste) et, en repli, par l'artiste et
//! le titre. Il ne rend un nombre qu'à partir de trois comptes distincts ;
//! sous ce seuil, ou pour un élément inconnu, l'élément est **absent** de sa
//! réponse. Ici aussi : le champ [`CHAMP`] est posé seulement quand le site a
//! rendu un nombre, jamais `null` ni `0` inventé.
//!
//! ## Rien ne part sans DEUX conditions
//!
//! 1. le serveur est relié à un compte, jeton de liaison compris
//!    ([`super::sauvegarde_config::compte`]) ;
//! 2. l'envoi des écoutes au compte est activé
//!    ([`super::ecoutes_au_compte::envoi_active`], désactivé par défaut).
//!
//! ## Ce qui part
//!
//! Le `server_id` (la preuve de liaison que le contrat demande), et par
//! élément : son type, son MBID s'il est connu, l'artiste et le titre. Son
//! `id` est sa **position dans le lot** (« 0 », « 1 », …) : ni l'identifiant
//! local de la piste, ni le profil, ni aucun chemin ne part.
//!
//! ## Pas de rafale, et jamais une liste qui attend le site
//!
//! - chaque élément lu est gardé [`CACHE_SECONDES`] (24 h), son absence
//!   comprise : une liste revue ne redemande rien ;
//! - ce qui manque part en lots de [`LOT_MAX`], au plus [`LOTS_PAR_REPONSE`]
//!   lots par réponse, dans une tâche **à part** ; la réponse l'attend au plus
//!   [`ATTENTE_MAX`] (le site répond d'ordinaire bien avant), puis part avec
//!   ce qu'elle a : ce qui arrive après remplit le cache pour l'affichage
//!   suivant ;
//! - un élément déjà demandé par une autre réponse n'est pas redemandé ;
//! - après un échec (réseau, refus, 429), plus aucun appel pendant
//!   [`PANNE_SECONDES`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tracing::debug;

use crate::cloud::ecoutes_au_compte::envoi_active;
use crate::cloud::notes_communautaires::{Reponse, Session, raison};
use crate::cloud::rate_limit::CloudScope;
use crate::cloud::sauvegarde_config::{self as sc, Compte};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Le champ posé sur une piste, un album ou un artiste.
pub const CHAMP: &str = "community_listens";
/// La route du site.
pub const CHEMIN: &str = "/api/v1/community/listen-counts";
/// Éléments au plus par lot (le site refuse au-delà : 422).
pub const LOT_MAX: usize = 200;
/// Lots au plus lancés pour une réponse : une liste de 5 000 pistes n'en
/// demande que les 1 000 premières, le reste viendra aux affichages suivants.
pub const LOTS_PAR_REPONSE: usize = 5;
/// Durée de vie d'un nombre (ou d'une absence) lu au site.
pub const CACHE_SECONDES: u64 = 24 * 3600;
/// Silence après un échec.
pub const PANNE_SECONDES: u64 = 300;
/// Ce qu'une réponse de liste attend au plus le site.
pub const ATTENTE_MAX: Duration = Duration::from_millis(400);
/// Entrées au plus dans le cache.
const CACHE_MAX: usize = 100_000;
/// Longueur d'un nom envoyé : celle des écoutes reçues par le site
/// ([`super::ecoutes_au_compte::LONGUEUR_MAX`]), pour que le repli par les
/// noms retombe sur les mêmes clés.
const LONGUEUR_MAX: usize = super::ecoutes_au_compte::LONGUEUR_MAX;
/// Longueur maximale d'un MBID accepté par le site.
const MBID_MAX: usize = 64;

/// Ce que désigne un élément.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Genre {
    /// Une piste : `musicbrainz_recording_id`, `artist_name`, `title`.
    Piste,
    /// Un album : `musicbrainz_release_id`, `artist_name`, `title`.
    Album,
    /// Un artiste : `musicbrainz_id`, `name`.
    Artiste,
    /// Une piste d'un service de streaming (#7026, « Top tracks » d'une fiche
    /// artiste) : comme [`Genre::Piste`], plus son `isrc` et sa
    /// `duration_ms`, que le site sait rapprocher (ISRC exact ; repli par les
    /// noms à 3 s près, jamais entre deux ISRC différents). Un genre à part
    /// pour que les listes de la bibliothèque gardent la demande d'avant.
    PisteDeService,
}

impl Genre {
    /// Le `type` du contrat.
    pub const fn type_du_site(self) -> &'static str {
        match self {
            Self::Piste | Self::PisteDeService => "recording",
            Self::Album => "release",
            Self::Artiste => "artist",
        }
    }

    /// (MBID, artiste, titre) : les champs JSON lus sur l'objet.
    const fn champs(self) -> (&'static str, &'static str, Option<&'static str>) {
        match self {
            Self::Piste | Self::PisteDeService => {
                ("musicbrainz_recording_id", "artist_name", Some("title"))
            }
            Self::Album => ("musicbrainz_release_id", "artist_name", Some("title")),
            Self::Artiste => ("musicbrainz_id", "name", None),
        }
    }
}

/// Un élément tel qu'il part, et la clé de son cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Demande {
    genre: Genre,
    mbid: Option<String>,
    artiste: Option<String>,
    titre: Option<String>,
    /// [`Genre::PisteDeService`] seulement.
    isrc: Option<String>,
    /// [`Genre::PisteDeService`] seulement.
    duree_ms: Option<u64>,
}

/// Longueur maximale d'un ISRC accepté par le site.
const ISRC_MAX: usize = 32;

fn texte(objet: &Map<String, Value>, champ: &str, max: usize) -> Option<String> {
    let s = objet.get(champ)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.chars().take(max).collect())
}

impl Demande {
    /// La demande d'un objet JSON, ou `None` quand il ne nomme rien
    /// d'utilisable : ni MBID, ni artiste (et titre, pour une piste ou un
    /// album). Le site ne regroupe jamais par le titre seul.
    fn depuis(genre: Genre, valeur: &Value) -> Option<Self> {
        let objet = valeur.as_object()?;
        let (c_mbid, c_artiste, c_titre) = genre.champs();
        let mbid = texte(objet, c_mbid, usize::MAX)
            .map(|m| m.to_lowercase())
            .filter(|m| m.len() <= MBID_MAX);
        let artiste = texte(objet, c_artiste, LONGUEUR_MAX);
        let titre = c_titre.and_then(|c| texte(objet, c, LONGUEUR_MAX));
        let noms_suffisants = artiste.is_some() && (c_titre.is_none() || titre.is_some());
        let (isrc, duree_ms) = if genre == Genre::PisteDeService {
            (
                texte(objet, "isrc", usize::MAX)
                    .map(|i| i.to_uppercase())
                    .filter(|i| i.len() <= ISRC_MAX),
                objet
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .filter(|d| *d > 0 && *d <= 86_400_000),
            )
        } else {
            (None, None)
        };
        if mbid.is_none() && isrc.is_none() && !noms_suffisants {
            return None;
        }
        Some(Self {
            genre,
            mbid,
            artiste,
            titre,
            isrc,
            duree_ms,
        })
    }

    fn cle(&self, base: &str) -> String {
        let bas = |o: &Option<String>| o.as_deref().unwrap_or("").to_lowercase();
        format!(
            "{base}\x1f{}\x1f{}\x1f{}\x1f{}\x1f{}\x1f{}",
            self.genre.type_du_site(),
            bas(&self.mbid),
            bas(&self.artiste),
            bas(&self.titre),
            bas(&self.isrc),
            self.duree_ms.map(|d| d.to_string()).unwrap_or_default()
        )
    }

    fn json(&self, id: usize) -> Value {
        let mut o = Map::new();
        o.insert("id".into(), json!(id.to_string()));
        o.insert("type".into(), json!(self.genre.type_du_site()));
        if let Some(m) = &self.mbid {
            o.insert("mbid".into(), json!(m));
        }
        if let Some(a) = &self.artiste {
            o.insert("artist".into(), json!(a));
        }
        if let Some(t) = &self.titre {
            o.insert("title".into(), json!(t));
        }
        if let Some(i) = &self.isrc {
            o.insert("isrc".into(), json!(i));
        }
        if let Some(d) = self.duree_ms {
            o.insert("duration_ms".into(), json!(d));
        }
        Value::Object(o)
    }
}

#[derive(Default)]
struct Etat {
    /// Clé → (lu le, nombre rendu par le site ou absence).
    cache: HashMap<String, (Instant, Option<i64>)>,
    /// Clés demandées en ce moment.
    en_vol: HashSet<String>,
    /// Base du site → fin du silence après un échec.
    panne: HashMap<String, Instant>,
}

static ETAT: LazyLock<Mutex<Etat>> = LazyLock::new(|| Mutex::new(Etat::default()));

fn frais(quand: Instant) -> bool {
    quand.elapsed() < Duration::from_secs(CACHE_SECONDES)
}

/// Le compte, si l'appel est permis : relié (jeton de liaison compris) ET
/// envoi des écoutes activé. Sinon `None`, et rien ne part.
fn compte_permis(settings: &SettingsRepo) -> Option<Compte> {
    if !envoi_active(settings) {
        return None;
    }
    sc::compte(settings).ok().filter(|c| c.liaison.is_some())
}

/// Pose [`CHAMP`] sur chaque objet de `items` dont le site connaît le
/// nombre d'écoutes. Ne touche à rien d'autre ; n'enlève rien.
///
/// N'appelle jamais le site sans compte relié ni envoi des écoutes activé.
/// Attend le site au plus [`ATTENTE_MAX`].
pub async fn annoter(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    genre: Genre,
    items: &mut [Value],
) {
    if items.is_empty() {
        return;
    }
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(compte) = compte_permis(&settings) else {
        return;
    };
    let demandes: Vec<Option<(Demande, String)>> = items
        .iter()
        .map(|v| {
            Demande::depuis(genre, v).map(|d| {
                let cle = d.cle(&compte.base);
                (d, cle)
            })
        })
        .collect();

    // Ce que le cache sait déjà, et ce qui manque (chaque clé une fois).
    let mut manquantes: Vec<(Demande, String)> = Vec::new();
    {
        let Ok(mut etat) = ETAT.lock() else {
            return;
        };
        let en_panne = etat
            .panne
            .get(&compte.base)
            .is_some_and(|fin| Instant::now() < *fin);
        let mut vues = HashSet::new();
        for (d, cle) in demandes.iter().flatten() {
            let connu = etat.cache.get(cle).is_some_and(|(quand, _)| frais(*quand));
            if connu || en_panne || etat.en_vol.contains(cle) || !vues.insert(cle.clone()) {
                continue;
            }
            if manquantes.len() < LOT_MAX * LOTS_PAR_REPONSE {
                manquantes.push((d.clone(), cle.clone()));
            }
        }
        for (_, cle) in &manquantes {
            etat.en_vol.insert(cle.clone());
        }
    }

    if !manquantes.is_empty() {
        let (fini, attente) = tokio::sync::oneshot::channel::<()>();
        let backend = backend.clone();
        let http = http.clone();
        tokio::spawn(async move {
            remplir(&backend, &http, settings, compte, manquantes).await;
            fini.send(()).ok();
        });
        if tokio::time::timeout(ATTENTE_MAX, attente).await.is_err() {
            debug!("community_listen_counts_late");
        }
    }

    let Ok(etat) = ETAT.lock() else {
        return;
    };
    for (item, demande) in items.iter_mut().zip(&demandes) {
        let Some((_, cle)) = demande else {
            continue;
        };
        if let Some((quand, Some(n))) = etat.cache.get(cle)
            && frais(*quand)
            && let Some(objet) = item.as_object_mut()
        {
            objet.insert(CHAMP.into(), json!(n));
        }
    }
}

/// Demande au site les éléments manquants, lot par lot, et range chaque
/// réponse au cache. Un échec arrête tout et pose le silence.
async fn remplir(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    settings: SettingsRepo,
    compte: Compte,
    manquantes: Vec<(Demande, String)>,
) {
    let base = compte.base.clone();
    let server_id = compte.server_id.clone();
    let mut session = Session {
        backend,
        http,
        settings,
        compte,
        liaison_tentee: false,
        session_rafraichie: false,
    };
    for lot in manquantes.chunks(LOT_MAX) {
        let corps = json!({
            "server_id": server_id,
            "items": lot.iter().enumerate().map(|(i, (d, _))| d.json(i)).collect::<Vec<_>>(),
        });
        let r = session
            .appeler(
                reqwest::Method::POST,
                CHEMIN,
                |q| q.json(&corps),
                CloudScope::CommunityListenCountsRead,
            )
            .await;
        let Reponse::Ok(reponse) = r else {
            debug!(raison = %raison(&r), "community_listen_counts_unavailable");
            if let Ok(mut etat) = ETAT.lock() {
                etat.panne.insert(
                    base.clone(),
                    Instant::now() + Duration::from_secs(PANNE_SECONDES),
                );
            }
            break;
        };
        let mut rendus: HashMap<usize, i64> = HashMap::new();
        for e in reponse
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = e
                .get("id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<usize>().ok());
            let n = e.get("listens").and_then(Value::as_i64).filter(|n| *n > 0);
            if let (Some(id), Some(n)) = (id, n)
                && id < lot.len()
            {
                rendus.insert(id, n);
            }
        }
        if let Ok(mut etat) = ETAT.lock() {
            if etat.cache.len() + lot.len() > CACHE_MAX {
                etat.cache.retain(|_, (quand, _)| frais(*quand));
                if etat.cache.len() + lot.len() > CACHE_MAX {
                    etat.cache.clear();
                }
            }
            let maintenant = Instant::now();
            for (i, (_, cle)) in lot.iter().enumerate() {
                etat.cache
                    .insert(cle.clone(), (maintenant, rendus.get(&i).copied()));
            }
        }
    }
    if let Ok(mut etat) = ETAT.lock() {
        for (_, cle) in &manquantes {
            etat.en_vol.remove(cle);
        }
    }
}

/// [`annoter`] pour un objet seul (la fiche d'un album, d'un artiste).
pub async fn annoter_un(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    genre: Genre,
    item: &mut Value,
) {
    annoter(backend, http, genre, std::slice::from_mut(item)).await;
}

/// [`annoter`] pour un tableau JSON (`items` d'une page, ou la liste nue).
/// Ne fait rien si `liste` n'est pas un tableau.
pub async fn annoter_liste(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    genre: Genre,
    liste: Option<&mut Value>,
) {
    if let Some(Value::Array(items)) = liste {
        annoter(backend, http, genre, items).await;
    }
}

#[cfg(test)]
#[path = "ecoutes_communaute_tests.rs"]
mod tests;
