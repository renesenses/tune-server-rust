//! #6782 — découvrir les artistes du même label.
//!
//! Demande de Bertrand du 22/10/2026, avec la vue grille des Labels (#6736) :
//! « n'oublie pas la fonction de découverte d'artistes du même label ! ».
//!
//! La page d'un label montre deux listes :
//!
//! 1. **Dans ma bibliothèque** ([`artistes_de_la_bibliotheque`]) : les
//!    artistes qui ont au moins un album sous ce label, du plus grand nombre
//!    d'albums au plus petit, avec leur portrait. Lu en base, aucun appel.
//! 2. **À découvrir** ([`regrouper`]) : les artistes du catalogue Qobuz du
//!    label qui ne sont PAS dans la bibliothèque, regroupés depuis
//!    `label/get` (`extra=albums`, la même lecture que la page d'un label
//!    Qobuz, bornée à 500 albums et sans précommandes). Chaque artiste porte
//!    son nombre d'albums et un album phare.
//!
//! Le label Qobuz est celui que le lot L a rapproché et gardé
//! ([`super::logos_labels::EntreeLogo::qobuz_label_id`]) : on ne le cherche
//! pas une seconde fois.
//!
//! # Pas de rafale
//!
//! Le catalogue d'un label coûte jusqu'à dix pages. Il est gardé
//! [`DUREE_SUCCES_S`] (une journée) par [`CacheDecouverte`], une erreur
//! [`DUREE_ECHEC_S`] (une heure), et UNE seule lecture de catalogue tourne
//! à la fois, tous labels confondus : dix ouvertures simultanées de la même
//! page ne font qu'une lecture, et deux labels ne se lisent pas en parallèle.
//!
//! Sans compte Qobuz connecté : la seule bibliothèque, aucun appel (la route
//! ne construit même pas de catalogue).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

use crate::TuneError;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::streaming::qobuz::QobuzService;
use crate::streaming::traits::StreamAlbum;

/// Un catalogue gardé une journée : un label ne publie pas d'album d'heure
/// en heure.
pub const DUREE_SUCCES_S: u64 = 24 * 3600;
/// Une lecture en échec se retente au bout d'une heure, pas à chaque
/// ouverture de la page.
pub const DUREE_ECHEC_S: u64 = 3600;

/// Le catalogue d'un label chez un service. Un trait pour que les essais
/// le remplacent sans réseau.
#[async_trait::async_trait]
pub trait CatalogueDuLabel: Send + Sync {
    async fn albums_du_label(&self, label_id: &str) -> Result<Vec<StreamAlbum>, String>;
}

#[async_trait::async_trait]
impl CatalogueDuLabel for QobuzService {
    async fn albums_du_label(&self, label_id: &str) -> Result<Vec<StreamAlbum>, String> {
        QobuzService::albums_du_label(self, label_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Le catalogue d'un label lu par le trait d'un service quelconque
/// ([`crate::streaming::StreamingService::get_label_catalogue`]) : la page
/// « Label » d'un service (#6934) passe par le même cache.
struct CatalogueDuService<'a>(&'a dyn crate::streaming::StreamingService);

#[async_trait::async_trait]
impl CatalogueDuLabel for CatalogueDuService<'_> {
    async fn albums_du_label(&self, label_id: &str) -> Result<Vec<StreamAlbum>, String> {
        self.0
            .get_label_catalogue(label_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// La clé d'un nom d'artiste : sans accents, minuscules, espaces resserrés.
/// « Bill Évans » et « bill  evans » sont le même artiste ; « Bill Evans
/// Trio » ne l'est pas.
pub fn cle_d_artiste(nom: &str) -> String {
    crate::db::engine::fold_diacritics(nom)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Les noms qui ne sont pas un artiste à découvrir.
fn artiste_collectif(cle: &str) -> bool {
    matches!(
        cle,
        "" | "various artists" | "various" | "va" | "artistes divers" | "artistes variés"
    )
}

/// L'album qui représente un artiste à découvrir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlbumPhare {
    /// L'identifiant Qobuz de l'album (`/streaming/qobuz/albums/{id}`).
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
}

/// Un artiste du catalogue du label, absent de la bibliothèque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtisteADecouvrir {
    pub name: String,
    /// L'identifiant Qobuz de l'artiste, quand le catalogue le donne.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qobuz_artist_id: Option<String>,
    /// Ses albums dans le catalogue du label (tel que lu, 500 au plus).
    pub album_count: usize,
    pub album_phare: AlbumPhare,
}

/// L'année d'un album du catalogue, pour choisir l'album phare.
fn annee(a: &StreamAlbum) -> u32 {
    a.year.unwrap_or(0)
}

/// Regroupe le catalogue d'un label par artiste, sans les artistes dont la
/// clé de nom est dans `deja` (la bibliothèque) ni les « Various Artists ».
///
/// - un artiste par clé de nom ([`cle_d_artiste`]) ; son nom affiché est
///   celui de son premier album dans l'ordre du catalogue ;
/// - l'album phare est le plus RÉCENT de ses albums (année), et à année
///   égale le premier dans l'ordre du catalogue — un choix stable, qui ne
///   dépend d'aucune donnée de popularité que Qobuz ne donne pas ici ;
/// - trié du plus grand nombre d'albums au plus petit, puis par nom.
pub fn regrouper(albums: &[StreamAlbum], deja: &HashSet<String>) -> Vec<ArtisteADecouvrir> {
    let mut ordre: Vec<String> = Vec::new();
    let mut groupes: HashMap<String, ArtisteADecouvrir> = HashMap::new();
    let mut phares: HashMap<String, u32> = HashMap::new();
    for a in albums {
        let cle = cle_d_artiste(&a.artist);
        if artiste_collectif(&cle) || deja.contains(&cle) {
            continue;
        }
        let phare = AlbumPhare {
            id: a.id.clone(),
            title: a.title.clone(),
            cover_url: a.cover_path.clone(),
            year: a.year,
        };
        match groupes.get_mut(&cle) {
            None => {
                ordre.push(cle.clone());
                phares.insert(cle.clone(), annee(a));
                groupes.insert(
                    cle,
                    ArtisteADecouvrir {
                        name: a.artist.trim().to_string(),
                        qobuz_artist_id: a.artist_id.clone(),
                        album_count: 1,
                        album_phare: phare,
                    },
                );
            }
            Some(g) => {
                g.album_count += 1;
                if g.qobuz_artist_id.is_none() {
                    g.qobuz_artist_id = a.artist_id.clone();
                }
                let meilleure = phares.entry(cle).or_insert(0);
                if annee(a) > *meilleure {
                    *meilleure = annee(a);
                    g.album_phare = phare;
                }
            }
        }
    }
    let mut rendu: Vec<ArtisteADecouvrir> = ordre
        .into_iter()
        .filter_map(|c| groupes.remove(&c))
        .collect();
    rendu.sort_by(|a, b| {
        b.album_count
            .cmp(&a.album_count)
            .then_with(|| cle_d_artiste(&a.name).cmp(&cle_d_artiste(&b.name)))
    });
    rendu
}

// ---------------------------------------------------------------------------
// Le cache des catalogues
// ---------------------------------------------------------------------------

type Lecture = Result<Arc<Vec<StreamAlbum>>, String>;

/// Les catalogues de labels déjà lus, et le verrou qui empêche deux lectures
/// à la fois.
#[derive(Default)]
pub struct CacheDecouverte {
    entrees: Mutex<HashMap<String, (u64, Lecture)>>,
    lecture: tokio::sync::Mutex<()>,
}

impl CacheDecouverte {
    /// Le cache du processus.
    pub fn global() -> &'static CacheDecouverte {
        static CACHE: OnceLock<CacheDecouverte> = OnceLock::new();
        CACHE.get_or_init(CacheDecouverte::default)
    }

    fn frais(&self, label_id: &str, maintenant: u64) -> Option<Lecture> {
        let entrees = self.entrees.lock().unwrap_or_else(|e| e.into_inner());
        let (quand, lecture) = entrees.get(label_id)?;
        let duree = if lecture.is_ok() {
            DUREE_SUCCES_S
        } else {
            DUREE_ECHEC_S
        };
        (maintenant.saturating_sub(*quand) < duree).then(|| lecture.clone())
    }

    /// Le catalogue du label `label_id` : celui du cache s'il est encore
    /// frais, sinon une lecture — une seule à la fois, et celles qui
    /// attendaient la même trouvent ensuite le cache rempli.
    pub async fn albums(
        &self,
        catalogue: &dyn CatalogueDuLabel,
        label_id: &str,
        maintenant: impl Fn() -> u64,
    ) -> Lecture {
        if let Some(l) = self.frais(label_id, maintenant()) {
            return l;
        }
        let _une_a_la_fois = self.lecture.lock().await;
        if let Some(l) = self.frais(label_id, maintenant()) {
            return l;
        }
        let lecture = catalogue.albums_du_label(label_id).await.map(Arc::new);
        if let Err(e) = &lecture {
            tracing::debug!(label_id, erreur = %e, "artistes_du_label_catalogue_echec");
        }
        self.entrees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(label_id.to_string(), (maintenant(), lecture.clone()));
        lecture
    }
}

impl CacheDecouverte {
    /// #6934 — comme [`Self::albums`], le catalogue lu par le trait du service
    /// (`get_label_catalogue`), sous la même clé : l'identifiant du label.
    pub async fn albums_du_service(
        &self,
        service: &dyn crate::streaming::StreamingService,
        label_id: &str,
        maintenant: impl Fn() -> u64,
    ) -> Result<Arc<Vec<StreamAlbum>>, String> {
        self.albums(&CatalogueDuService(service), label_id, maintenant)
            .await
    }
}

// ---------------------------------------------------------------------------
// La bibliothèque
// ---------------------------------------------------------------------------

/// Un artiste de la bibliothèque qui a au moins un album sous le label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtisteDuLabel {
    pub id: i64,
    pub name: String,
    /// Ses albums visibles sous ce label.
    pub album_count: i64,
    /// `/api/v1/library/artists/{id}/image`, seulement quand l'artiste a un
    /// portrait — jamais une adresse qui rendrait 404.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

fn placeholder(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Postgres => PostgresDialect.placeholder(i),
        Engine::Sqlite => SqliteDialect.placeholder(i),
    }
}

/// Les artistes de la bibliothèque sous le label `nom` (espaces de bord
/// retirés, comme le regroupement de `GET /library/labels`), avec les mêmes
/// exclusions que cette liste : albums masqués, doubles distants d'un album
/// local. Sans les « Various Artists ».
pub fn artistes_de_la_bibliotheque(
    db: &Arc<dyn DbBackend>,
    nom: &str,
) -> Result<Vec<ArtisteDuLabel>, TuneError> {
    let engine = db.engine();
    let sql = format!(
        "SELECT ar.id, ar.name, ar.image_path, COUNT(DISTINCT a.id) AS n \
         FROM albums a JOIN artists ar ON ar.id = a.artist_id \
         WHERE a.label IS NOT NULL AND TRIM(a.label) = {p1} AND {masques} AND {doubles} \
         GROUP BY ar.id, ar.name, ar.image_path",
        p1 = placeholder(engine, 1),
        masques = crate::db::facet_filter::hidden_albums_excluded(),
        doubles = crate::db::facet_filter::album_distant_double_exclu(engine, "a"),
    );
    let nom = nom.trim().to_string();
    let mut rendu: Vec<ArtisteDuLabel> = db
        .query_many(&sql, &[&nom as &dyn ToSqlValue])?
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let name = r.get(1)?.as_string()?;
            if artiste_collectif(&cle_d_artiste(&name)) {
                return None;
            }
            let portrait = r
                .get(2)
                .and_then(|v| v.as_string())
                .is_some_and(|p| !p.trim().is_empty());
            Some(ArtisteDuLabel {
                id,
                name,
                album_count: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
                image_url: portrait.then(|| format!("/api/v1/library/artists/{id}/image")),
            })
        })
        .collect();
    rendu.sort_by(|a, b| {
        b.album_count
            .cmp(&a.album_count)
            .then_with(|| cle_d_artiste(&a.name).cmp(&cle_d_artiste(&b.name)))
    });
    Ok(rendu)
}

/// Les clés de nom ([`cle_d_artiste`]) de TOUS les artistes de la
/// bibliothèque : un artiste déjà présent, sous ce label ou un autre, n'est
/// pas « à découvrir ».
pub fn cles_de_la_bibliotheque(db: &Arc<dyn DbBackend>) -> Result<HashSet<String>, TuneError> {
    Ok(db
        .query_many("SELECT name FROM artists", &[])?
        .into_iter()
        .filter_map(|r| r.first()?.as_string())
        .map(|n| cle_d_artiste(&n))
        .collect())
}

#[cfg(test)]
#[path = "artistes_du_label_tests.rs"]
mod tests;
