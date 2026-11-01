//! Des passes d'images plus rapides, sans dépasser le débit d'aucun service
//! (01/11/2026).
//!
//! # Ce que la mesure a montré (.18, 2 626 artistes, journal du 31/10)
//!
//! - 2 141 artistes en 2 h 03, soit **3,4 s par artiste**, un artiste après
//!   l'autre ; les passes suivantes retentaient les 265 mêmes échecs, à
//!   8,1 s chacun (36 min pour deux images trouvées).
//! - Une bonne part de ce temps était faite de **délais fixes** (100, 200,
//!   500, 200, 200, 200, 100, 100 ms entre les marches de la cascade, 1 s
//!   avant la recherche du MBID, 1 s par membre d'un crédit collé), ajoutés
//!   au limiteur MusicBrainz qui espace déjà ses requêtes.
//! - Pendant ce temps, MusicBrainz attendait souvent son tour à vide, et
//!   les autres services n'étaient jamais interrogés en même temps.
//!
//! # Ce que ce module apporte
//!
//! - [`passer`] : la **porte d'un hôte**. Au plus [`REQUETES_PAR_HOTE`]
//!   requêtes simultanées par service, au plus [`PLAFOND_GLOBAL`] pour toutes
//!   les passes d'images, et l'**intervalle du service** tenu par son
//!   limiteur ([`Hote::limiteur`]) : MusicBrainz 1/s (le limiteur partagé),
//!   Discogs 0,9/s, TheAudioDB 30/min, iTunes 20/min… Un `429`/`503`
//!   ralentit la clé et honore `Retry-After` ([`RateLimiter::constater`]).
//! - [`premiere_retenue`] : les **sources d'un même élément interrogées en
//!   parallèle**, la meilleure gardée selon l'ordre de priorité existant.
//!   Seule la gagnante télécharge son image : une source de rang inférieur
//!   ne télécharge que si toutes celles d'avant n'ont rien donné.
//! - [`en_parallele_borne`] : **plusieurs éléments à la fois**
//!   ([`ELEMENTS_EN_PARALLELE`]). L'admission reste un élément à la fois :
//!   pendant une lecture, la pause de [`crate::taches_de_fond`] espace donc
//!   toujours les éléments comme avant (un toutes les 5 s), elle ne se
//!   multiplie pas par la largeur.
//! - [`CacheNegatif`] : un élément qu'**aucune** source ne connaît n'est
//!   retenté qu'après [`DELAI_ABSENT_S`]. Un échec passager (réseau, `429`,
//!   `5xx`) n'est jamais inscrit : l'élément revient à la passe suivante.

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, Stream, StreamExt};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::http::fetch::{
    CLE_DISCOGS, CLE_WIKIDATA, DISCOGS, FetchOutcome, MUSICBRAINZ, RateLimiter, WIKIDATA,
};

/// Combien d'éléments (artistes, albums) une passe traite à la fois.
pub const ELEMENTS_EN_PARALLELE: usize = 4;

/// Combien de requêtes simultanées vers un même service.
pub const REQUETES_PAR_HOTE: usize = 4;

/// Combien de requêtes simultanées pour toutes les passes d'images réunies.
pub const PLAFOND_GLOBAL: usize = 8;

/// Un élément qu'aucune source ne connaît n'est pas retenté avant sept jours.
/// Les images approuvées du site, elles, sont reprises à chaque passe (phase
/// communautaire) : une image partagée entre-temps arrive quand même.
pub const DELAI_ABSENT_S: u64 = 7 * 24 * 3600;

/// La clé du cache négatif des images d'artistes, dans les réglages.
pub const CLE_ABSENTS_ARTISTES: &str = "artist_image_absent_v1";

/// La clé du cache négatif des pochettes d'albums, dans les réglages.
pub const CLE_ABSENTS_ALBUMS: &str = "album_cover_absent_v1";

/// Un service interrogé par les passes d'images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hote {
    Mozaiklabs,
    Fanart,
    TheAudioDb,
    MusicBrainz,
    CoverArtArchive,
    Wikimedia,
    Wikidata,
    Discogs,
    LastFm,
    Deezer,
    Itunes,
    /// Tout autre hôte (le CDN d'une image) : 4/s par nom d'hôte.
    Autre,
}

static LIMITEUR_MOZAIKLABS: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(10.0));
static LIMITEUR_FANART: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(2.0));
/// TheAudioDB, clé gratuite : 30 requêtes par minute.
static LIMITEUR_THEAUDIODB: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(0.5));
/// Cover Art Archive : aucun plafond publié ; 4/s par retenue.
static LIMITEUR_CAA: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(4.0));
static LIMITEUR_WIKIMEDIA: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(4.0));
/// Last.fm : « pas plus de 5 requêtes par seconde » ; 4 par marge.
static LIMITEUR_LASTFM: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(4.0));
/// Deezer : 50 requêtes par 5 s ; 8/s par marge.
static LIMITEUR_DEEZER: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(8.0));
/// iTunes Search : environ 20 requêtes par minute.
static LIMITEUR_ITUNES: LazyLock<RateLimiter> =
    LazyLock::new(|| RateLimiter::with_interval(Duration::from_secs(3)));
static LIMITEUR_AUTRES: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(4.0));

static PORTES_PAR_HOTE: LazyLock<Vec<Semaphore>> = LazyLock::new(|| {
    Hote::TOUS
        .iter()
        .map(|_| Semaphore::new(REQUETES_PAR_HOTE))
        .collect()
});
static PORTE_GLOBALE: Semaphore = Semaphore::const_new(PLAFOND_GLOBAL);

/// Échecs passagers (réseau, `429`, `5xx`) depuis le démarrage.
pub static ECHECS_TRANSITOIRES: AtomicU64 = AtomicU64::new(0);

tokio::task_local! {
    static TRANSITOIRES_DE_L_ELEMENT: Cell<u32>;
}

impl Hote {
    pub const TOUS: [Hote; 12] = [
        Hote::Mozaiklabs,
        Hote::Fanart,
        Hote::TheAudioDb,
        Hote::MusicBrainz,
        Hote::CoverArtArchive,
        Hote::Wikimedia,
        Hote::Wikidata,
        Hote::Discogs,
        Hote::LastFm,
        Hote::Deezer,
        Hote::Itunes,
        Hote::Autre,
    ];

    fn rang(self) -> usize {
        Hote::TOUS
            .iter()
            .position(|h| *h == self)
            .unwrap_or(Hote::TOUS.len() - 1)
    }

    /// Le service d'une adresse, d'après son nom d'hôte.
    pub fn de_l_url(url: &str) -> Hote {
        let hote = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let est = |domaine: &str| hote == domaine || hote.ends_with(&format!(".{domaine}"));
        if est("mozaiklabs.fr") {
            Hote::Mozaiklabs
        } else if est("fanart.tv") {
            Hote::Fanart
        } else if est("theaudiodb.com") {
            Hote::TheAudioDb
        } else if est("musicbrainz.org") {
            Hote::MusicBrainz
        } else if est("coverartarchive.org") {
            Hote::CoverArtArchive
        } else if est("wikimedia.org") {
            Hote::Wikimedia
        } else if est("wikidata.org") {
            Hote::Wikidata
        } else if est("discogs.com") {
            Hote::Discogs
        } else if est("audioscrobbler.com") || est("last.fm") {
            Hote::LastFm
        } else if est("deezer.com") || est("dzcdn.net") {
            Hote::Deezer
        } else if est("itunes.apple.com") {
            Hote::Itunes
        } else {
            Hote::Autre
        }
    }

    /// Le limiteur du service et sa clé. MusicBrainz, Wikidata et Discogs
    /// passent par les limiteurs PARTAGÉS du serveur : une passe d'images et
    /// une passe d'identification ne doublent pas leur débit à elles deux.
    pub fn limiteur(self) -> (&'static RateLimiter, &'static str) {
        match self {
            Hote::Mozaiklabs => (&LIMITEUR_MOZAIKLABS, "mozaiklabs"),
            Hote::Fanart => (&LIMITEUR_FANART, "fanart"),
            Hote::TheAudioDb => (&LIMITEUR_THEAUDIODB, "theaudiodb"),
            Hote::MusicBrainz => (
                &MUSICBRAINZ,
                crate::metadata::musicbrainz_release::CLE_LIMITEUR_MUSICBRAINZ,
            ),
            Hote::CoverArtArchive => (&LIMITEUR_CAA, "caa"),
            Hote::Wikimedia => (&LIMITEUR_WIKIMEDIA, "wikimedia"),
            Hote::Wikidata => (&WIKIDATA, CLE_WIKIDATA),
            Hote::Discogs => (&DISCOGS, CLE_DISCOGS),
            Hote::LastFm => (&LIMITEUR_LASTFM, "lastfm"),
            Hote::Deezer => (&LIMITEUR_DEEZER, "deezer"),
            Hote::Itunes => (&LIMITEUR_ITUNES, "itunes"),
            Hote::Autre => (&LIMITEUR_AUTRES, "autre"),
        }
    }
}

/// Le droit de parler à un hôte : une place dans sa porte et une dans la
/// porte globale, rendues quand le passage tombe.
pub struct Passage {
    _hote: SemaphorePermit<'static>,
    _global: SemaphorePermit<'static>,
}

/// Attendre son tour pour `hote` (`cle_autre` : le nom d'hôte, pour
/// [`Hote::Autre`]). Ordre : la porte de l'hôte, puis l'intervalle du
/// service, puis la porte globale — jamais une place globale tenue pendant
/// qu'on dort sur l'intervalle d'un service lent.
pub async fn passer(hote: Hote, cle_autre: &str) -> Option<Passage> {
    let porte_hote = PORTES_PAR_HOTE[hote.rang()].acquire().await.ok()?;
    let (limiteur, cle) = hote.limiteur();
    limiteur
        .acquire(if hote == Hote::Autre { cle_autre } else { cle })
        .await;
    let global = PORTE_GLOBALE.acquire().await.ok()?;
    Some(Passage {
        _hote: porte_hote,
        _global: global,
    })
}

/// Noter un échec passager, pour l'élément en cours et pour le total.
pub fn noter_transitoire() {
    ECHECS_TRANSITOIRES.fetch_add(1, Ordering::Relaxed);
    let _ = TRANSITOIRES_DE_L_ELEMENT.try_with(|c| c.set(c.get() + 1));
}

/// Faire `f` en comptant ses échecs passagers : `(résultat, échecs)`.
/// Toutes les requêtes de `f`, sources parallèles comprises, sont comptées :
/// elles tournent dans la même tâche.
pub async fn en_comptant_les_transitoires<F: Future>(f: F) -> (F::Output, u32) {
    TRANSITOIRES_DE_L_ELEMENT
        .scope(Cell::new(0), async move {
            let sortie = f.await;
            let n = TRANSITOIRES_DE_L_ELEMENT.with(|c| c.get());
            (sortie, n)
        })
        .await
}

fn cle_d_hote(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Une réponse JSON, par la porte de l'hôte de `url`. `None` si le service
/// n'a rien rendu d'utilisable ; un échec passager est compté.
pub async fn json(
    req: reqwest::RequestBuilder,
    url_pour_l_hote: &str,
) -> Option<serde_json::Value> {
    let hote = Hote::de_l_url(url_pour_l_hote);
    let cle_autre = cle_d_hote(url_pour_l_hote);
    let _passage = passer(hote, &cle_autre).await?;
    let (limiteur, cle) = hote.limiteur();
    let cle = if hote == Hote::Autre {
        cle_autre.as_str()
    } else {
        cle
    };
    let reponse = match req.send().await {
        Ok(r) => r,
        Err(_) => {
            noter_transitoire();
            return None;
        }
    };
    limiteur.constater_reponse(cle, &reponse);
    let statut = reponse.status().as_u16();
    if statut == 429 || statut >= 500 {
        noter_transitoire();
        return None;
    }
    if !reponse.status().is_success() {
        return None;
    }
    reponse.json().await.ok()
}

/// Les octets d'une image, par la porte de son hôte.
pub async fn octets(client: &reqwest::Client, url: &str, min_len: usize) -> FetchOutcome {
    let hote = Hote::de_l_url(url);
    let cle_autre = cle_d_hote(url);
    let Some(_passage) = passer(hote, &cle_autre).await else {
        return FetchOutcome::Error("porte fermée".into());
    };
    let (limiteur, cle) = hote.limiteur();
    let cle = if hote == Hote::Autre {
        cle_autre.as_str()
    } else {
        cle
    };
    let issue = crate::http::fetch::fetch_bytes(client, url, min_len).await;
    match &issue {
        FetchOutcome::RateLimited => {
            limiteur.signaler_refus(cle, None);
            noter_transitoire();
        }
        FetchOutcome::Error(_) => noter_transitoire(),
        FetchOutcome::Success(_) => limiteur.signaler_succes(cle),
        _ => {}
    }
    issue
}

/// Les sources d'un même élément, interrogées en parallèle ; la meilleure
/// est gardée selon l'ordre de `sources` (la première est la préférée).
///
/// `retenir` fait le travail coûteux (télécharger l'image, la confronter aux
/// empreintes rejetées) et n'est appelée que pour une source dont TOUTES les
/// précédentes ont fini sans rien donner : la priorité est exactement celle
/// de la cascade séquentielle, seule l'attente des recherches se recouvre.
/// Dès qu'une source est retenue, les recherches restantes sont abandonnées.
pub async fn premiere_retenue<'a, S, L, T, F, Fut>(
    sources: Vec<(S, BoxFuture<'a, Option<L>>)>,
    mut retenir: F,
) -> Option<(T, S)>
where
    S: Copy,
    F: FnMut(S, L) -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let n = sources.len();
    let mut noms = Vec::with_capacity(n);
    let mut resultats: Vec<Option<Option<L>>> = (0..n).map(|_| None).collect();
    let mut en_vol = FuturesUnordered::new();
    for (i, (nom, recherche)) in sources.into_iter().enumerate() {
        noms.push(nom);
        en_vol.push(async move { (i, recherche.await) });
    }
    for i in 0..n {
        while resultats[i].is_none() {
            match en_vol.next().await {
                Some((j, r)) => resultats[j] = Some(r),
                None => break,
            }
        }
        if let Some(Some(trouve)) = resultats[i].take()
            && let Some(retenu) = retenir(noms[i], trouve).await
        {
            return Some((retenu, noms[i]));
        }
    }
    None
}

/// Les éléments traités `largeur` à la fois. `admettre` est attendue avant
/// chaque élément, UN élément à la fois : c'est là que la passe cède à la
/// lecture et se gare quand l'utilisateur la suspend. Les résultats sortent
/// dans l'ordre où ils finissent.
pub fn en_parallele_borne<I, A, AF, F, Fut>(
    elements: I,
    largeur: usize,
    mut admettre: A,
    travail: F,
) -> impl Stream<Item = Fut::Output>
where
    I: IntoIterator,
    A: FnMut() -> AF,
    AF: Future<Output = ()>,
    F: FnMut(I::Item) -> Fut,
    Fut: Future,
{
    futures_util::stream::iter(elements)
        .then(move |x| {
            let admission = admettre();
            async move {
                admission.await;
                x
            }
        })
        .map(travail)
        .buffer_unordered(largeur.max(1))
}

/// Les éléments qu'aucune source ne connaissait, et quand on l'a constaté
/// (secondes Unix), gardés dans un réglage.
#[derive(Debug, Default, Clone)]
pub struct CacheNegatif {
    cle: &'static str,
    entrees: HashMap<i64, u64>,
    modifie: bool,
}

impl CacheNegatif {
    pub fn vide(cle: &'static str) -> Self {
        Self {
            cle,
            entrees: HashMap::new(),
            modifie: false,
        }
    }

    pub fn charger(settings: &crate::db::settings_repo::SettingsRepo, cle: &'static str) -> Self {
        let entrees = settings
            .get(cle)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<HashMap<String, u64>>(&s).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, v)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            cle,
            entrees,
            modifie: false,
        }
    }

    /// Faut-il demander `id` maintenant ?
    pub fn a_retenter(&self, id: i64, maintenant: u64) -> bool {
        self.entrees
            .get(&id)
            .is_none_or(|vu| maintenant.saturating_sub(*vu) >= DELAI_ABSENT_S)
    }

    pub fn noter_absent(&mut self, id: i64, maintenant: u64) {
        self.entrees.insert(id, maintenant);
        self.modifie = true;
    }

    pub fn oublier(&mut self, id: i64) {
        if self.entrees.remove(&id).is_some() {
            self.modifie = true;
        }
    }

    pub fn len(&self) -> usize {
        self.entrees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entrees.is_empty()
    }

    /// Écrire le cache (les entrées échues sont retirées), s'il a changé.
    pub fn enregistrer(
        &mut self,
        settings: &crate::db::settings_repo::SettingsRepo,
        maintenant: u64,
    ) {
        if !self.modifie {
            return;
        }
        self.entrees
            .retain(|_, vu| maintenant.saturating_sub(*vu) < DELAI_ABSENT_S);
        let carte: HashMap<String, u64> = self
            .entrees
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        if let Ok(texte) = serde_json::to_string(&carte)
            && settings.set(self.cle, &texte).is_ok()
        {
            self.modifie = false;
        }
    }
}

/// Maintenant, en secondes Unix.
pub fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "images_rapides_tests.rs"]
mod tests;
