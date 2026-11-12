//! #7107 — les titres phares d'un artiste, complétés par la popularité
//! Last.fm.
//!
//! Bertrand, 26/10/2026, page de Michael Kiwanuka (33 albums) : les dix
//! « titres phares » étaient les dix premières pistes de l'album FUDGE.
//! `artist/get?extra=tracks` de Qobuz rend le catalogue dans l'ordre d'un
//! album, sans popularité ; seuls les titres qui avaient un total
//! communautaire remontaient.
//!
//! Ici, la popularité vient d'une source externe réelle : `artist.getTopTracks`
//! de Last.fm, par le client et la clé déjà présents dans Tune (réglage
//! `lastfm_api_key`, puis l'environnement). Rien n'est inventé.
//!
//! Le classement final (voir [`super::titres_phares_ecoutes::classer`]) :
//! 1. le total de la communauté (`play_count`) ;
//! 2. les écoutes de ce serveur (`local_play_count`, locales et du service) ;
//! 3. le rang Last.fm (`lastfm_rank`, 1 = le plus écouté) ;
//! 4. l'ordre du service, en dernier.
//!
//! Ce module pose le rang (3) et AJOUTE les titres Last.fm que la liste du
//! service ne contenait pas, retrouvés par une recherche chez le service :
//! c'est ce qui couvre toute la discographie, pas seulement le dernier album.
//!
//! Le rapprochement d'un titre Last.fm avec une piste jouable : Last.fm ne
//! donne ni durée ni ISRC dans cette réponse, donc le titre NORMALISÉ
//! ([`cle_titre`]) plus l'interprète ; l'ISRC et l'identifiant ne servent
//! qu'à ne pas ajouter deux fois la même piste. Les versions live, remaster,
//! mono, démo… sont écartées quand une version d'origine existe.
//!
//! #8350 — jamais d'homonyme (DidLyon, fil 2394 : « Ange », « Barbara ») :
//! Last.fm est interrogé par le MBID de l'artiste quand la bibliothèque le
//! connaît ; sans MBID, par nom sans autocorrect, et sa liste n'est retenue
//! que si elle recoupe le catalogue de l'artiste chez le service. Une piste
//! ajoutée par recherche doit porter l'identifiant de l'artiste de la page
//! chez le service : un même nom ne suffit jamais.
//!
//! Sans clé, ou si Last.fm échoue (réseau, quota, réponse vide) : la liste
//! sort INCHANGÉE, sans erreur. Un échec est mémorisé 15 minutes, un succès
//! 12 heures ; le cache est borné. Les recherches sont séquentielles, en
//! nombre borné et espacées : aucune rafale.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::streaming::traits::StreamTrack;

/// Le rang du titre sur Last.fm, 1 = le plus écouté. Absent si Last.fm ne le
/// connaît pas (ou n'a pas été interrogé).
pub const CHAMP_RANG: &str = "lastfm_rank";

/// Combien de titres Last.fm on demande.
const TITRES_DEMANDES: usize = 20;
/// Combien de titres Last.fm on regarde (les plus populaires).
const TITRES_RETENUS: usize = 15;
/// Recherches chez le service au plus, par requête.
pub const RECHERCHES_MAX: usize = 8;
/// Pause entre deux recherches successives.
pub const PAUSE_ENTRE_RECHERCHES: Duration = Duration::from_millis(250);
const DUREE_SUCCES: Duration = Duration::from_secs(12 * 3600);
const DUREE_ECHEC: Duration = Duration::from_secs(15 * 60);
const TAILLE_CACHE: usize = 256;

/// Un cache à durée de vie et à taille bornées.
struct CacheBorne<V> {
    map: HashMap<String, (Instant, V)>,
}

impl<V: Clone> CacheBorne<V> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
    fn get(&self, cle: &str) -> Option<V> {
        self.map
            .get(cle)
            .filter(|(fin, _)| Instant::now() < *fin)
            .map(|(_, v)| v.clone())
    }
    fn put(&mut self, cle: String, v: V, duree: Duration) {
        let maintenant = Instant::now();
        if self.map.len() >= TAILLE_CACHE && !self.map.contains_key(&cle) {
            self.map.retain(|_, (fin, _)| maintenant < *fin);
            if self.map.len() >= TAILLE_CACHE
                && let Some(plus_ancienne) = self
                    .map
                    .iter()
                    .min_by_key(|(_, (fin, _))| *fin)
                    .map(|(k, _)| k.clone())
            {
                self.map.remove(&plus_ancienne);
            }
        }
        self.map.insert(cle, (maintenant + duree, v));
    }
}

/// `Some(titres)` : Last.fm a répondu. `None` : il a échoué (mémorisé court).
static TOPS: LazyLock<Mutex<CacheBorne<Option<Vec<String>>>>> =
    LazyLock::new(|| Mutex::new(CacheBorne::new()));
/// Le résultat d'une recherche d'un titre Last.fm chez un service (`None` :
/// introuvable, mémorisé aussi).
static RESOLUS: LazyLock<Mutex<CacheBorne<Option<StreamTrack>>>> =
    LazyLock::new(|| Mutex::new(CacheBorne::new()));

/// Les réglages d'un passage ; les tests injectent un faux Last.fm et
/// suppriment les pauses.
pub struct Reglages<'a> {
    pub base: &'a str,
    pub cle: &'a str,
    pub pause: Duration,
    pub recherches_max: usize,
}

impl<'a> Reglages<'a> {
    /// La production : l'API de Last.fm, les pauses et bornes par défaut.
    pub fn production(cle: &'a str) -> Self {
        Self {
            base: crate::metadata::lastfm::LASTFM_API,
            cle,
            pause: PAUSE_ENTRE_RECHERCHES,
            recherches_max: RECHERCHES_MAX,
        }
    }
}

/// L'artiste dont on complète les titres phares.
pub struct ArtisteDeLaPage<'a> {
    /// Son identifiant chez le service (celui de la page). Vide : inconnu.
    pub id: &'a str,
    /// Son MBID, quand la bibliothèque le connaît.
    pub mbid: Option<&'a str>,
}

/// #8350 — le MBID de l'artiste de ce nom dans la bibliothèque, s'il en a un
/// et s'il est le SEUL artiste de ce nom : deux « Ange » en bibliothèque, et
/// le nom ne dit plus lequel est celui de la page (pas de MBID, donc le
/// garde-fou du recoupement s'applique).
pub fn mbid_de_la_bibliotheque(
    backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    nom: &str,
) -> Option<String> {
    use crate::db::backend::ToSqlValue;
    if nom.trim().is_empty() {
        return None;
    }
    let lignes = backend
        .query_many(
            "SELECT musicbrainz_id FROM artists WHERE LOWER(name) = LOWER(?)",
            &[&nom as &dyn ToSqlValue],
        )
        .ok()?;
    if lignes.len() != 1 {
        return None;
    }
    lignes[0]
        .first()
        .and_then(|v| v.as_string())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// La clé Last.fm de l'application : le réglage saisi dans Tune d'abord,
/// l'environnement à défaut — la même résolution que la biographie et les
/// vignettes (`bio_batch::cle_lastfm_avec_reglage`).
pub fn cle_lastfm(backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>) -> String {
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    crate::metadata::bio_batch::cle_lastfm_avec_reglage(
        reglages.get("lastfm_api_key").ok().flatten(),
    )
}

const MARQUEURS: &[&str] = &[
    "live",
    "remaster",
    "remastered",
    "mono",
    "stereo",
    "demo",
    "acoustic",
    "unplugged",
    "session",
    "sessions",
    "edit",
    "version",
    "mix",
    "bbc",
    "deluxe",
];

fn a_un_marqueur(texte: &str) -> bool {
    texte
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|m| MARQUEURS.contains(&m))
}

/// Le titre débarrassé de ses mentions de version, et « est-ce une version
/// autre que l'originale ? » (live, remaster, mono, démo…).
fn decouper_version(titre: &str) -> (String, bool) {
    let mut base = String::new();
    let mut alt = false;
    let mut chars = titre.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' || c == '[' {
            let fin = if c == '(' { ')' } else { ']' };
            let mut groupe = String::new();
            for g in chars.by_ref() {
                if g == fin {
                    break;
                }
                groupe.push(g);
            }
            if a_un_marqueur(&groupe) {
                alt = true;
            } else {
                base.push(c);
                base.push_str(&groupe);
                base.push(fin);
            }
        } else {
            base.push(c);
        }
    }
    if let Some(i) = base.find(" - ")
        && a_un_marqueur(&base[i..])
    {
        alt = true;
        base.truncate(i);
    }
    (base, alt)
}

fn alphanum(t: &str) -> String {
    t.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// La clé de rapprochement d'un titre : sans mention de version, sans casse,
/// sans ponctuation. « Love & Hate (Live at Abbey Road) » et « Love & Hate »
/// ont la même clé.
pub fn cle_titre(titre: &str) -> String {
    alphanum(&decouper_version(titre).0)
}

/// Vrai pour un live, un remaster, une version mono, une démo…
pub fn est_version_alternative(titre: &str) -> bool {
    decouper_version(titre).1
}

fn meme_artiste(a: &str, b: &str) -> bool {
    let (a, b) = (alphanum(a), alphanum(b));
    !a.is_empty() && !b.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

fn texte<'a>(v: &'a Value, champ: &str) -> &'a str {
    v.get(champ).and_then(Value::as_str).unwrap_or("")
}

/// Les titres Last.fm de l'artiste, du cache ou de Last.fm. `None` : échec ou
/// rien à dire, l'appelant garde le classement actuel.
async fn titres_lastfm(
    client: &reqwest::Client,
    cfg: &Reglages<'_>,
    artiste: &str,
    mbid: Option<&str>,
) -> Option<Vec<String>> {
    let cle_cache = format!("{}|{}|{}", cfg.base, alphanum(artiste), mbid.unwrap_or(""));
    if let Some(v) = TOPS.lock().ok().and_then(|c| c.get(&cle_cache)) {
        return v.filter(|t| !t.is_empty());
    }
    let resultat = crate::metadata::lastfm::top_tracks_artiste(
        client,
        cfg.base,
        artiste,
        mbid,
        cfg.cle,
        TITRES_DEMANDES,
    )
    .await;
    let (valeur, duree) = match resultat {
        Ok(t) => (Some(t), DUREE_SUCCES),
        Err(e) => {
            tracing::debug!(artiste, error = %e, "titres_phares_lastfm_echec");
            (None, DUREE_ECHEC)
        }
    };
    if let Ok(mut c) = TOPS.lock() {
        c.put(cle_cache, valeur.clone(), duree);
    }
    valeur.filter(|t| !t.is_empty())
}

/// Pose `lastfm_rank` sur les titres du service qui sont sur Last.fm, ajoute
/// ceux de Last.fm que le service n'a pas rendus (`chercher` interroge le
/// service), et écarte les versions alternatives d'un titre dont l'originale
/// est là. Sans clé ou si Last.fm échoue : `items` n'est PAS touché.
pub async fn completer<F, Fut>(
    client: &reqwest::Client,
    cfg: &Reglages<'_>,
    service: &str,
    page: &ArtisteDeLaPage<'_>,
    items: &mut Vec<Value>,
    mut chercher: F,
) where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Vec<StreamTrack>>,
{
    if cfg.cle.trim().is_empty() || items.is_empty() {
        return;
    }
    let artiste = texte(&items[0], "artist_name").to_string();
    let mbid = page.mbid.map(str::trim).filter(|m| !m.is_empty());
    let Some(titres) = titres_lastfm(client, cfg, &artiste, mbid).await else {
        return;
    };
    let titres: Vec<&String> = titres.iter().take(TITRES_RETENUS).collect();

    // #8350 — sans MBID, le nom peut désigner un homonyme : la liste Last.fm
    // n'est retenue que si elle recoupe le catalogue de l'artiste.
    if mbid.is_none() {
        let recoupe = titres.iter().any(|t| {
            let cle = cle_titre(t);
            !cle.is_empty() && items.iter().any(|v| cle_titre(texte(v, "title")) == cle)
        });
        if !recoupe {
            tracing::debug!(artiste, "titres_phares_lastfm_homonyme_probable");
            return;
        }
    }

    // Une version alternative s'efface devant l'originale de la liste.
    let originales: Vec<String> = items
        .iter()
        .filter(|v| !est_version_alternative(texte(v, "title")))
        .map(|v| cle_titre(texte(v, "title")))
        .collect();
    items.retain(|v| {
        let t = texte(v, "title");
        !est_version_alternative(t) || !originales.contains(&cle_titre(t))
    });

    // Le rang, sur la version d'origine de préférence.
    let mut manquants: Vec<(usize, &String)> = Vec::new();
    for (i, titre) in titres.iter().enumerate() {
        let cle = cle_titre(titre);
        let position = items
            .iter()
            .position(|v| {
                cle_titre(texte(v, "title")) == cle && !est_version_alternative(texte(v, "title"))
            })
            .or_else(|| {
                items
                    .iter()
                    .position(|v| cle_titre(texte(v, "title")) == cle)
            });
        match position {
            Some(p) => {
                if let Some(o) = items[p].as_object_mut() {
                    o.entry(CHAMP_RANG).or_insert(json!(i + 1));
                }
            }
            None => manquants.push((i + 1, titre)),
        }
    }

    // Les titres que le service n'a pas rendus : on les cherche chez lui.
    let mut cherches = 0;
    for (rang, titre) in manquants {
        if cherches >= cfg.recherches_max {
            break;
        }
        let cle = cle_titre(titre);
        let cle_resolu = format!(
            "{}|{service}|{}|{}|{cle}",
            cfg.base,
            alphanum(&artiste),
            page.id
        );
        let en_cache = RESOLUS.lock().ok().and_then(|c| c.get(&cle_resolu));
        let piste = match en_cache {
            Some(p) => p,
            None => {
                if cherches > 0 && !cfg.pause.is_zero() {
                    tokio::time::sleep(cfg.pause).await;
                }
                cherches += 1;
                let trouvees = chercher(format!("{artiste} {titre}")).await;
                let choisie = choisir(&trouvees, &artiste, page.id, &cle);
                if let Ok(mut c) = RESOLUS.lock() {
                    c.put(cle_resolu, choisie.clone(), DUREE_SUCCES);
                }
                choisie
            }
        };
        let Some(piste) = piste else { continue };
        let deja = items.iter().any(|v| {
            texte(v, "source_id") == piste.id
                || piste
                    .isrc
                    .as_deref()
                    .is_some_and(|i| !i.is_empty() && texte(v, "isrc") == i)
        });
        if deja {
            continue;
        }
        let mut v = json!(piste);
        if let Some(o) = v.as_object_mut() {
            o.insert(CHAMP_RANG.into(), json!(rang));
        }
        items.push(v);
    }
}

/// Parmi les résultats d'une recherche : même interprète, même titre
/// normalisé, la version d'origine d'abord. #8350 — quand l'identifiant de
/// l'artiste de la page est connu, la piste doit le porter : un homonyme
/// (même nom, autre artiste) ou une piste sans identifiant n'entre pas —
/// sauf chez un service qui n'en donne jamais (YouTube), où le nom reste le
/// seul critère.
fn choisir(
    trouvees: &[StreamTrack],
    artiste: &str,
    artiste_id: &str,
    cle: &str,
) -> Option<StreamTrack> {
    let artiste_id = artiste_id.trim();
    // Un service qui ne donne AUCUN identifiant d'artiste dans ses résultats
    // (YouTube) : seul le nom peut servir, comme avant.
    let sans_identifiants = trouvees.iter().all(|t| t.artist_id.is_none());
    let candidats: Vec<&StreamTrack> = trouvees
        .iter()
        .filter(|t| {
            let bon_artiste = if artiste_id.is_empty() || sans_identifiants {
                meme_artiste(&t.artist, artiste)
            } else {
                t.artist_id.as_deref().map(str::trim) == Some(artiste_id)
            };
            bon_artiste && cle_titre(&t.title) == cle
        })
        .collect();
    candidats
        .iter()
        .find(|t| !est_version_alternative(&t.title))
        .or_else(|| candidats.first())
        .map(|t| (*t).clone())
}

#[cfg(test)]
#[path = "titres_phares_lastfm_tests.rs"]
mod tests;
