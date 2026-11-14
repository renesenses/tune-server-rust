//! Renvoi UNIQUE des images d'artistes perdues par le site (rc12, décision de
//! Bertrand du 13/11/2026).
//!
//! Du 22/10 au 13/11/2026, le dossier d'écriture des images d'artistes de
//! mozaiklabs.fr était fermé : le site a enregistré 19 310 dépôts SANS fichier
//! (`image_path = "0"`) en répondant 201. 10 169 artistes y ont perdu leur seule
//! image communautaire (site-mozaiklabs#536, qui efface ces lignes).
//!
//! Un serveur Tune ne garde aucune trace de ce qu'il a envoyé : il n'envoie une
//! image qu'au moment où il la trouve (`library::artwork`), et tient « already
//! submitted » / « rejected » pour un succès. Sans ce renvoi, les images
//! perdues ne reviendraient jamais.
//!
//! La passe, UNE fois pour toutes ([`CLE_MARQUEUR`], constant : une nouvelle
//! version du serveur ne la relance pas) :
//!
//! 1. liste les artistes locaux qui ont un MBID ET une image en cache venue
//!    d'une source externe ([`SOURCES_A_RENVOYER`] ; jamais `community`, l'image
//!    que le site a lui-même servie, ni Roon, ni une image posée à la main) ;
//! 2. demande au site, par lots de 500, lesquels n'y ont aucune image
//!    approuvée ou en attente (`POST /api/v1/community/artist-images/missing`) ;
//! 3. renvoie ceux-là, un toutes les [`CADENCE_ENVOI`], en cédant à la pause
//!    de l'utilisateur et à la lecture à chaque image ;
//! 4. pose le marqueur quand tous les lots ont été traités.
//!
//! Le consentement (`cloud::consent::contribution_autorisee`) est relu avant
//! chaque lot et chaque envoi : retiré en cours de route, la passe s'arrête
//! sans marqueur.
//!
//! Un site injoignable, un 404 (route pas encore déployée), un 429 ou un 5xx —
//! dont le 503 « stockage indisponible » de #536 — ne font pas sauter l'image :
//! la passe attend puis retente la même requête, avec un délai qui double
//! jusqu'à une heure. Au bout de [`Reglages::essais_max`] échecs d'affilée,
//! elle s'arrête SANS marqueur : le prochain démarrage reprend. La reprise ne
//! renvoie rien deux fois : ce qui est parti est désormais en attente ou
//! approuvé sur le site, la question suivante ne le rend plus.
//!
//! Le client HTTP est le client partagé (`http::client::shared`, garde
//! `http_client_seam`).

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::cloud::community::{self, MBID_MANQUANTS_PAR_LOT, RefusDuSite};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Réglage posé quand le renvoi est fait. Sa clé est CONSTANTE : le renvoi
/// est unique (décision de Bertrand du 13/11/2026), pas une passe par
/// version. Sa valeur garde, pour le diagnostic, la version qui l'a fait.
pub const CLE_MARQUEUR: &str = "renvoi_images_2026_11_v1";

/// Les `artists.image_source` d'une image trouvée par la cascade externe — les
/// seules que la passe d'images proposait au site. `auto` est la cascade par
/// MBID (Fanart, TheAudioDB, MusicBrainz/Wikimedia, Deezer, Last.fm…),
/// `discogs` et `lastfm` la passe par nom.
pub const SOURCES_A_RENVOYER: [&str; 3] = ["auto", "discogs", "lastfm"];

/// Un envoi toutes les deux secondes au plus.
pub const CADENCE_ENVOI: Duration = Duration::from_secs(2);

/// Le site refuse au-delà de 5 Mo (`max:5120`).
const TAILLE_MAX: u64 = 5 * 1024 * 1024;

/// Délai plafond entre deux essais d'une requête refusée « transitoire ».
const ATTENTE_MAX: Duration = Duration::from_secs(3600);

/// Un artiste local dont l'image peut repartir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidat {
    pub mbid: String,
    pub nom: String,
    pub image_path: String,
    pub source: String,
}

/// Les artistes à proposer, dans l'ordre de leur identifiant, un seul par MBID
/// (minuscules). L'image doit être dans le cache : un `image_path` qui est une
/// URL distante ou dont le fichier a disparu n'a rien à renvoyer.
pub fn candidats(db: &Arc<dyn DbBackend>, cache_dir: &std::path::Path) -> Vec<Candidat> {
    let sources = SOURCES_A_RENVOYER
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, name, musicbrainz_id, image_path, image_source FROM artists \
         WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id <> '' \
         AND image_path IS NOT NULL AND image_path <> '' \
         AND image_source IN ({sources}) ORDER BY id"
    );
    let lignes = match db.query_many(&sql, &[]) {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "renvoi_images_artistes_lecture_echec");
            return Vec::new();
        }
    };
    let mut vus = HashSet::new();
    let mut sortie = Vec::new();
    for cols in lignes {
        let texte = |i: usize| cols.get(i).and_then(|v| v.as_string()).unwrap_or_default();
        let mbid = texte(2).trim().to_ascii_lowercase();
        let image_path = texte(3);
        if !mbid_valide(&mbid)
            || crate::library::artwork::fichier_image_en_cache(cache_dir, &image_path).is_none()
            || !vus.insert(mbid.clone())
        {
            continue;
        }
        sortie.push(Candidat {
            mbid,
            nom: texte(1),
            image_path,
            source: texte(4),
        });
    }
    sortie
}

/// Le site valide les MBID comme des UUID : un MBID local abîmé ferait
/// refuser tout le lot (422).
fn mbid_valide(mbid: &str) -> bool {
    mbid.len() == 36
        && mbid.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Le renvoi est-il déjà fait, par n'importe quelle version ?
pub fn deja_fait(settings: &SettingsRepo) -> bool {
    settings
        .get(CLE_MARQUEUR)
        .ok()
        .flatten()
        .is_some_and(|v| !v.trim().is_empty())
}

/// Ce qui règle une passe. Les témoins raccourcissent cadence et attentes.
#[derive(Debug, Clone)]
pub struct Reglages {
    pub base_url: String,
    pub instance_id: String,
    pub version: String,
    pub cache_dir: PathBuf,
    pub cadence: Duration,
    /// Première attente après un refus transitoire (doublée ensuite).
    pub attente_transitoire: Duration,
    /// Échecs transitoires d'affilée tolérés avant de remettre à plus tard.
    pub essais_max: u32,
}

impl Reglages {
    pub fn production(instance_id: String, version: String, cache_dir: PathBuf) -> Self {
        Self {
            base_url: community::DEFAULT_BASE_URL.to_string(),
            instance_id,
            version,
            cache_dir,
            cadence: CADENCE_ENVOI,
            attente_transitoire: Duration::from_secs(5 * 60),
            // 5 + 10 + 20 + 40 + 60 × 4 min : un peu plus de 5 h.
            essais_max: 8,
        }
    }
}

/// Le bilan d'une passe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Artistes candidats examinés.
    pub examines: u64,
    /// Artistes que le site disait sans image.
    pub manquants: u64,
    /// Images renvoyées (acceptées, ou « déjà reçue » / « refusée » par le site).
    pub renvoyees: u64,
    /// Images que le site a refusées définitivement, ou illisibles en local.
    pub ecartees: u64,
    /// Le marqueur est posé : la passe ne reviendra plus.
    pub terminee: bool,
    /// Pourquoi la passe s'est arrêtée avant la fin, le cas échéant.
    pub reportee: Option<String>,
}

enum Arret {
    Consentement,
    /// Refus définitif de la requête (422…).
    Definitif(String),
    /// Le site ne répond plus après `essais_max` essais.
    Injoignable(String),
}

/// Retente `requete` tant que le site répond « transitoire », au plus
/// `essais_max` fois d'affilée.
async fn avec_reprises<T, F, Fut>(reglages: &Reglages, mut requete: F) -> Result<T, Arret>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RefusDuSite>>,
{
    let mut attente = reglages.attente_transitoire;
    let mut echecs = 0u32;
    loop {
        match requete().await {
            Ok(v) => return Ok(v),
            Err(RefusDuSite::Definitif(raison)) => return Err(Arret::Definitif(raison)),
            Err(RefusDuSite::Transitoire {
                raison,
                retry_after,
            }) => {
                echecs += 1;
                if echecs >= reglages.essais_max {
                    return Err(Arret::Injoignable(raison));
                }
                let annonce = retry_after.map(Duration::from_secs).unwrap_or_default();
                let delai = attente.max(annonce).min(ATTENTE_MAX);
                debug!(error = %raison, attente_s = delai.as_secs(), echecs, "renvoi_images_artistes_reessai");
                tokio::time::sleep(delai).await;
                attente = (attente * 2).min(ATTENTE_MAX);
            }
        }
    }
}

/// La passe entière.
///
/// - `consentie` est relue avant chaque lot et chaque envoi ;
/// - `frontiere` est attendue avant chaque envoi (pause de l'utilisateur,
///   cession à la lecture : `taches_de_fond::attendre_son_tour`) ;
/// - `avancement(examinés, total)` publie la progression.
///
/// Pose [`CLE_MARQUEUR`] = `reglages.version` quand tous les lots ont été
/// traités.
pub async fn executer<C, F, FF, A>(
    db: Arc<dyn DbBackend>,
    reglages: &Reglages,
    consentie: C,
    mut frontiere: F,
    mut avancement: A,
) -> Bilan
where
    C: Fn() -> bool,
    F: FnMut() -> FF,
    FF: Future<Output = ()>,
    A: FnMut(u64, u64),
{
    let settings = SettingsRepo::with_backend(db.clone());
    let mut bilan = Bilan::default();
    if !consentie() || reglages.instance_id.is_empty() {
        bilan.reportee = Some("consentement".into());
        return bilan;
    }
    let tous = {
        let db = db.clone();
        let cache = reglages.cache_dir.clone();
        tokio::task::spawn_blocking(move || candidats(&db, &cache))
            .await
            .unwrap_or_default()
    };
    let total = tous.len() as u64;
    avancement(0, total);
    info!(candidats = total, "renvoi_images_artistes_debut");

    let resultat = async {
        for lot in tous.chunks(MBID_MANQUANTS_PAR_LOT) {
            if !consentie() {
                return Err(Arret::Consentement);
            }
            let mbids: Vec<String> = lot.iter().map(|c| c.mbid.clone()).collect();
            let manquants: HashSet<String> = avec_reprises(reglages, || {
                community::artist_images_missing(&reglages.base_url, &reglages.instance_id, &mbids)
            })
            .await?
            .into_iter()
            .map(|m| m.to_ascii_lowercase())
            .collect();

            for candidat in lot {
                bilan.examines += 1;
                if manquants.contains(&candidat.mbid) {
                    bilan.manquants += 1;
                    frontiere().await;
                    if !consentie() {
                        return Err(Arret::Consentement);
                    }
                    match renvoyer(reglages, candidat).await {
                        Ok(true) => bilan.renvoyees += 1,
                        Ok(false) => bilan.ecartees += 1,
                        Err(arret) => return Err(arret),
                    }
                    if !reglages.cadence.is_zero() {
                        tokio::time::sleep(reglages.cadence).await;
                    }
                }
                avancement(bilan.examines, total);
            }
        }
        Ok(())
    }
    .await;

    match resultat {
        Ok(()) => {
            if let Err(e) = settings.set(CLE_MARQUEUR, &reglages.version) {
                warn!(error = %e, "renvoi_images_artistes_marqueur_echec");
            } else {
                bilan.terminee = true;
            }
            info!(
                examines = bilan.examines,
                manquants = bilan.manquants,
                renvoyees = bilan.renvoyees,
                ecartees = bilan.ecartees,
                "renvoi_images_artistes_termine"
            );
        }
        Err(Arret::Consentement) => {
            info!(
                examines = bilan.examines,
                "renvoi_images_artistes_consentement_retire"
            );
            bilan.reportee = Some("consentement".into());
        }
        Err(Arret::Definitif(raison) | Arret::Injoignable(raison)) => {
            warn!(error = %raison, examines = bilan.examines, renvoyees = bilan.renvoyees, "renvoi_images_artistes_reporte");
            bilan.reportee = Some(raison);
        }
    }
    bilan
}

/// Un envoi. `Ok(false)` : image écartée (illisible, trop lourde, refus
/// définitif du site pour CETTE image) ; `Err` : le site ne répond plus,
/// la passe est remise à plus tard.
async fn renvoyer(reglages: &Reglages, candidat: &Candidat) -> Result<bool, Arret> {
    let Some((chemin, mime)) =
        crate::library::artwork::fichier_image_en_cache(&reglages.cache_dir, &candidat.image_path)
    else {
        return Ok(false);
    };
    let octets = match tokio::fs::read(&chemin).await {
        Ok(o) if !o.is_empty() && (o.len() as u64) <= TAILLE_MAX => o,
        _ => return Ok(false),
    };
    let envoi = avec_reprises(reglages, || {
        community::resubmit_artist_image(
            &reglages.base_url,
            &candidat.mbid,
            &candidat.nom,
            &reglages.instance_id,
            &candidat.source,
            mime,
            octets.clone(),
        )
    })
    .await;
    match envoi {
        Ok(()) => {
            debug!(mbid = %candidat.mbid, source = %candidat.source, "renvoi_image_artiste_envoyee");
            Ok(true)
        }
        // 422 et consorts : cette image-là ne passera pas, les suivantes si.
        Err(Arret::Definitif(raison)) => {
            debug!(mbid = %candidat.mbid, error = %raison, "renvoi_image_artiste_ecartee");
            Ok(false)
        }
        Err(arret) => Err(arret),
    }
}

#[cfg(test)]
#[path = "renvoi_images_artistes_tests.rs"]
mod tests;
