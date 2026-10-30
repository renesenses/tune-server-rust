//! #6736 — le logo d'un label de la bibliothèque, lu chez Qobuz.
//!
//! # Le modèle
//!
//! Un label n'a pas d'identité dans la base (voir `db::tag_repo`) : l'onglet
//! Labels regroupe les albums par la chaîne libre `albums.label`. Ce module ne
//! change pas ce modèle. Il tient, À CÔTÉ, un petit index « nom de label →
//! label Qobuz rapproché » :
//!
//! - l'identifiant Qobuz du label (`qobuz_label_id`), gardé même quand le label
//!   n'a pas d'image — c'est la clé dont la découverte des artistes du même
//!   label (#6782) aura besoin ;
//! - le condensat de son image dans le **cache des pochettes**
//!   ([`crate::library::artwork::cache_fetched_image`]) : le logo est servi par
//!   `GET /library/artwork/{condensat}`, exactement comme une pochette ;
//! - la date de la dernière vérification, pour que l'absence soit, elle aussi,
//!   mise en cache.
//!
//! L'index est un fichier JSON posé dans le dossier du cache des pochettes
//! ([`FICHIER_INDEX`]) : aucune migration de schéma, rien à tenir en parité
//! SQLite / PostgreSQL. Un cache perdu se reconstruit tout seul.
//!
//! # Pas de rafale d'appels
//!
//! La résolution est **paresseuse et bornée** : la liste des labels n'attend
//! jamais Qobuz ; elle rend ce que l'index sait déjà, et lance au plus UNE
//! passe à la fois ([`CacheLogosLabels::reserver_passe`]), d'au plus
//! [`PASSE_MAX`] labels, un label après l'autre, avec une pause entre deux.
//! Un label sans correspondance n'est pas redemandé avant
//! [`DELAI_ABSENT_S`] ; une erreur (réseau, refus) pas avant
//! [`DELAI_ERREUR_S`].
//!
//! Sans compte Qobuz, l'appelant ne fournit aucun catalogue Qobuz : aucune
//! passe Qobuz, aucun appel (voir `tune-server`, `routes::library::labels`).
//!
//! # #7089 — les logos du web ouvert
//!
//! [`super::logos_web`] est un autre [`CatalogueDeLabels`], sans compte :
//! après un rapprochement MusicBrainz sûr, Commons (libre), puis Wikipédia,
//! puis le site officiel, puis Discogs. Il a SON index
//! ([`super::logos_web::FICHIER_INDEX_WEB`], même dossier, même forme) : un
//! « absent » chez Qobuz ne bloque pas le web, et inversement. Ses entrées
//! gardent le MBID du label (`mb_label_id`) et l'[`Attribution`] du logo
//! (source, licence connue), que la liste des labels expose.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Le nom de l'index, dans le dossier du cache des pochettes.
pub const FICHIER_INDEX: &str = "logos_labels.json";

/// Au plus tant de labels résolus par passe.
pub const PASSE_MAX: usize = 8;

/// Un label sans correspondance (ou sans image) n'est pas redemandé avant
/// trente jours.
pub const DELAI_ABSENT_S: u64 = 30 * 24 * 3600;

/// Une erreur (réseau, refus du service) n'est pas retentée avant un jour.
pub const DELAI_ERREUR_S: u64 = 24 * 3600;

/// Une image de logo plus lourde que ceci est refusée.
pub const TAILLE_MAX_IMAGE: usize = 5 * 1024 * 1024;

/// La pause entre deux labels d'une même passe, en production.
pub const PAUSE_ENTRE_LABELS: Duration = Duration::from_millis(750);

/// D'où vient un logo (#7089).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceDuLogo {
    Commons,
    Wikipedia,
    SiteOfficiel,
    Discogs,
    Qobuz,
}

/// Ce qu'on sait de la licence d'un logo (#7089).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatutDeLicence {
    /// Domaine public, CC0, CC BY, CC BY-SA, lu sur la page du fichier.
    Libre,
    /// Non libre (« fair use » de Wikipédia) : usage d'identification du
    /// label, dans le cache local du serveur, sans redistribution.
    NonLibreIdentification,
    /// Site officiel, Discogs, Qobuz : licence non déclarée.
    Inconnue,
}

/// L'attribution d'un logo (#7089) : sa source, ce qu'on sait de sa
/// licence, de qui, et la page d'où il vient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    pub source: SourceDuLogo,
    pub statut_licence: StatutDeLicence,
    /// Le nom affiché : « Wikimedia Commons », « Wikipedia », le domaine
    /// du site officiel, « Discogs ».
    pub fournisseur: String,
    /// Le nom court de la licence (« Public domain », « CC BY-SA 4.0 »,
    /// « Fair use »…), quand elle est déclarée.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auteur: Option<String>,
    /// La page d'où vient le logo : page du fichier, site, fiche Discogs.
    pub source_url: String,
    /// Le nom du fichier chez Commons ou Wikipédia.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fichier: Option<String>,
    /// Un logo de marque déclaré « œuvre personnelle » par qui l'a versé
    /// (`PD-self`, `Self-published work`) : la licence est à vérifier.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub douteux: bool,
}

/// Un label du catalogue, tel que le service l'a rapproché d'un nom local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDuCatalogue {
    pub id: String,
    pub nom: String,
    /// L'adresse de son image chez le service, s'il en a une.
    pub image_url: Option<String>,
    /// L'attribution de cette image, pour une source qui en exige une
    /// (#7089). `None` pour Qobuz.
    pub attribution: Option<Attribution>,
}

/// Ce qu'un service de streaming doit savoir faire pour donner un logo.
#[async_trait::async_trait]
pub trait CatalogueDeLabels: Send + Sync {
    /// Le label du catalogue qui porte ce nom. `album_du_catalogue` est, s'il
    /// y en a un, l'identifiant chez ce service d'un album de la bibliothèque
    /// qui porte ce label : son `label.id` est la correspondance la plus sûre.
    /// `Ok(None)` : rien ne correspond.
    async fn trouver_label(
        &self,
        nom: &str,
        album_du_catalogue: Option<&str>,
    ) -> Result<Option<LabelDuCatalogue>, String>;

    /// Les octets d'une image du service.
    async fn telecharger_image(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Ce que l'index sait d'un label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Statut {
    /// Un logo est en cache.
    Trouve,
    /// Aucune correspondance, ou un label rapproché sans image.
    Absent,
    /// Le service n'a pas répondu ; à retenter plus tard.
    Erreur,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntreeLogo {
    pub statut: Statut,
    /// L'identifiant Qobuz du label rapproché (#6782), même sans image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qobuz_label_id: Option<String>,
    /// Le MBID du label rapproché par les sources du web (#7089).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mb_label_id: Option<String>,
    /// L'attribution du logo en cache (#7089).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
    /// Le condensat du logo dans le cache des pochettes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Secondes depuis l'époque Unix.
    pub verifie_le: u64,
}

/// La clé d'index d'un nom de label : espaces resserrés, minuscules. Deux
/// graphies qui ne diffèrent que par la casse partagent leur logo.
pub fn cle_de_label(nom: &str) -> String {
    nom.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn compact(nom: &str) -> String {
    nom.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Deux noms de label désignent-ils le même label ? Égalité stricte une fois
/// la casse, les espaces et la ponctuation retirés — « Warp Records » et
/// « Warp » restent DEUX labels : un faux logo est pire qu'un monogramme.
pub fn noms_concordent(a: &str, b: &str) -> bool {
    let a = compact(a);
    !a.is_empty() && a == compact(b)
}

/// Un label à résoudre : son nom, et l'identifiant au catalogue d'un album
/// de la bibliothèque qui le porte, s'il y en a un.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demande {
    pub nom: String,
    pub album_du_catalogue: Option<String>,
}

/// L'index des logos de labels, adossé au cache des pochettes.
pub struct CacheLogosLabels {
    dossier: PathBuf,
    /// Le nom de l'index dans `dossier`.
    fichier: &'static str,
    entrees: Mutex<BTreeMap<String, EntreeLogo>>,
    passe_en_cours: AtomicBool,
}

/// Tant qu'elle vit, aucune autre passe ne peut démarrer.
pub struct GardePasse<'a>(&'a AtomicBool);

impl Drop for GardePasse<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl CacheLogosLabels {
    /// L'index du dossier `dossier` (le cache des pochettes), relu sur disque.
    /// Un index absent ou illisible est un index vide.
    pub fn ouvrir(dossier: &Path) -> Self {
        Self::ouvrir_index(dossier, FICHIER_INDEX)
    }

    /// L'index `fichier` du dossier `dossier`. [`FICHIER_INDEX`] est celui de
    /// Qobuz : ses identifiants sont des `qobuz_label_id` ; tout autre index
    /// (le web, #7089) garde des MBID (`mb_label_id`).
    pub fn ouvrir_index(dossier: &Path, fichier: &'static str) -> Self {
        let entrees = std::fs::read(dossier.join(fichier))
            .ok()
            .and_then(|o| serde_json::from_slice(&o).ok())
            .unwrap_or_default();
        Self {
            dossier: dossier.to_path_buf(),
            fichier,
            entrees: Mutex::new(entrees),
            passe_en_cours: AtomicBool::new(false),
        }
    }

    /// Une entrée sans identifiant ni attribution, que [`Self::avec_id`]
    /// complète.
    fn entree_nue(statut: Statut, image: Option<String>, verifie_le: u64) -> EntreeLogo {
        EntreeLogo {
            statut,
            qobuz_label_id: None,
            mb_label_id: None,
            attribution: None,
            image,
            verifie_le,
        }
    }

    /// Range l'identifiant du label dans le champ de la source de cet index.
    fn avec_id(&self, mut entree: EntreeLogo, id: Option<String>) -> EntreeLogo {
        if self.fichier == FICHIER_INDEX {
            entree.qobuz_label_id = id;
        } else {
            entree.mb_label_id = id;
        }
        entree
    }

    fn id_de(&self, entree: &EntreeLogo) -> Option<String> {
        if self.fichier == FICHIER_INDEX {
            entree.qobuz_label_id.clone()
        } else {
            entree.mb_label_id.clone()
        }
    }

    pub fn dossier(&self) -> &Path {
        &self.dossier
    }

    fn entrees(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, EntreeLogo>> {
        self.entrees.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ce que l'index sait de ce label.
    pub fn entree(&self, nom: &str) -> Option<EntreeLogo> {
        self.entrees().get(&cle_de_label(nom)).cloned()
    }

    /// Le condensat du logo de ce label, s'il est en cache ET que son fichier
    /// existe encore : jamais une adresse qui rendrait 404.
    pub fn image(&self, nom: &str) -> Option<String> {
        let entree = self.entree(nom)?;
        let image = entree.image.filter(|_| entree.statut == Statut::Trouve)?;
        crate::library::artwork::find_cached(&self.dossier, &image).map(|_| image)
    }

    /// Les labels de `demandes` qu'une passe doit (re)demander à
    /// `maintenant`, au plus `max`, sans doublon de clé.
    pub fn a_resoudre(&self, demandes: &[Demande], maintenant: u64, max: usize) -> Vec<Demande> {
        let entrees = self.entrees();
        let mut vues = std::collections::HashSet::new();
        demandes
            .iter()
            .filter(|d| {
                let cle = cle_de_label(&d.nom);
                if cle.is_empty() || !vues.insert(cle.clone()) {
                    return false;
                }
                match entrees.get(&cle) {
                    None => true,
                    Some(e) => match e.statut {
                        // Un logo dont le fichier a disparu du cache se
                        // redemande ; sinon, jamais.
                        Statut::Trouve => e.image.as_deref().is_none_or(|h| {
                            crate::library::artwork::find_cached(&self.dossier, h).is_none()
                        }),
                        Statut::Absent => maintenant.saturating_sub(e.verifie_le) >= DELAI_ABSENT_S,
                        Statut::Erreur => maintenant.saturating_sub(e.verifie_le) >= DELAI_ERREUR_S,
                    },
                }
            })
            .take(max)
            .cloned()
            .collect()
    }

    /// Réserve LA passe. `None` : une passe tourne déjà.
    pub fn reserver_passe(&self) -> Option<GardePasse<'_>> {
        self.passe_en_cours
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| GardePasse(&self.passe_en_cours))
    }

    /// Inscrit une entrée et réécrit l'index sur disque.
    pub fn inscrire(&self, nom: &str, entree: EntreeLogo) {
        let instantane = {
            let mut entrees = self.entrees();
            entrees.insert(cle_de_label(nom), entree);
            serde_json::to_vec_pretty(&*entrees).ok()
        };
        if let Some(octets) = instantane {
            self.ecrire_index(&octets);
        }
    }

    fn ecrire_index(&self, octets: &[u8]) {
        if std::fs::create_dir_all(&self.dossier).is_err() {
            return;
        }
        let provisoire = self.dossier.join(format!("{}.part", self.fichier));
        if std::fs::write(&provisoire, octets).is_ok()
            && std::fs::rename(&provisoire, self.dossier.join(self.fichier)).is_err()
        {
            let _ = std::fs::remove_file(&provisoire);
        }
    }

    /// Résout `demandes` une par une auprès de `catalogue`, avec `pause` entre
    /// deux labels, et inscrit chaque résultat — l'absence et l'erreur
    /// comprises. Rend le nombre de logos trouvés.
    pub async fn resoudre(
        &self,
        catalogue: &dyn CatalogueDeLabels,
        demandes: Vec<Demande>,
        maintenant: impl Fn() -> u64,
        pause: Duration,
    ) -> usize {
        let mut trouves = 0;
        for (i, demande) in demandes.into_iter().enumerate() {
            if i > 0 && !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            let precedent = self.entree(&demande.nom);
            let entree = match catalogue
                .trouver_label(&demande.nom, demande.album_du_catalogue.as_deref())
                .await
            {
                Err(e) => {
                    tracing::debug!(label = %demande.nom, erreur = %e, "logo_label_erreur");
                    let id = precedent.as_ref().and_then(|p| self.id_de(p));
                    self.avec_id(Self::entree_nue(Statut::Erreur, None, maintenant()), id)
                }
                Ok(None) => Self::entree_nue(Statut::Absent, None, maintenant()),
                Ok(Some(label)) => {
                    let image = match label.image_url.as_deref() {
                        None => Ok(None),
                        Some(url) => self.mettre_en_cache(catalogue, url).await.map(Some),
                    };
                    let id = Some(label.id);
                    match image {
                        Ok(Some(condensat)) => {
                            trouves += 1;
                            let mut e = self.avec_id(
                                Self::entree_nue(Statut::Trouve, Some(condensat), maintenant()),
                                id,
                            );
                            e.attribution = label.attribution;
                            e
                        }
                        Ok(None) => {
                            self.avec_id(Self::entree_nue(Statut::Absent, None, maintenant()), id)
                        }
                        Err(e) => {
                            tracing::debug!(label = %demande.nom, erreur = %e, "logo_label_image_erreur");
                            self.avec_id(Self::entree_nue(Statut::Erreur, None, maintenant()), id)
                        }
                    }
                }
            };
            self.inscrire(&demande.nom, entree);
        }
        trouves
    }

    async fn mettre_en_cache(
        &self,
        catalogue: &dyn CatalogueDeLabels,
        url: &str,
    ) -> Result<String, String> {
        let octets = catalogue.telecharger_image(url).await?;
        if octets.len() > TAILLE_MAX_IMAGE {
            return Err(format!("image trop lourde ({} octets)", octets.len()));
        }
        let ext = crate::library::artwork::sniff_image_ext(&octets)
            .ok_or_else(|| String::from("ce n'est pas une image servable"))?;
        crate::library::artwork::cache_fetched_image(&octets, &self.dossier, ext)
            .ok_or_else(|| String::from("écriture du cache impossible"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-logo-de-test-";

    /// Un faux catalogue : il connaît « Verve » (avec image) et « ECM » (sans
    /// image), compte ses appels, et peut tomber en panne.
    #[derive(Default)]
    struct FauxCatalogue {
        appels: AtomicUsize,
        telechargements: AtomicUsize,
        en_panne: AtomicBool,
        albums_vus: Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl CatalogueDeLabels for FauxCatalogue {
        async fn trouver_label(
            &self,
            nom: &str,
            album: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.appels.fetch_add(1, Ordering::SeqCst);
            self.albums_vus
                .lock()
                .unwrap()
                .push(album.map(String::from));
            if self.en_panne.load(Ordering::SeqCst) {
                return Err("panne simulée".into());
            }
            Ok(if noms_concordent(nom, "Verve") {
                Some(LabelDuCatalogue {
                    id: "7".into(),
                    nom: "Verve".into(),
                    image_url: Some("http://img.test/verve.png".into()),
                    attribution: None,
                })
            } else if noms_concordent(nom, "ECM") {
                Some(LabelDuCatalogue {
                    id: "9".into(),
                    nom: "ECM".into(),
                    image_url: None,
                    attribution: None,
                })
            } else {
                None
            })
        }

        async fn telecharger_image(&self, _url: &str) -> Result<Vec<u8>, String> {
            self.telechargements.fetch_add(1, Ordering::SeqCst);
            Ok(PNG.to_vec())
        }
    }

    fn demande(nom: &str) -> Demande {
        Demande {
            nom: nom.into(),
            album_du_catalogue: None,
        }
    }

    #[test]
    fn les_noms_concordent_sans_casse_ni_ponctuation_mais_jamais_par_morceau() {
        assert!(noms_concordent(
            "Deutsche Grammophon",
            "deutsche  grammophon"
        ));
        assert!(noms_concordent("Blue Note", "BLUE-NOTE"));
        assert!(!noms_concordent("Warp Records", "Warp"));
        assert!(!noms_concordent("", ""));
        assert!(!noms_concordent("!!", "??"));
    }

    #[tokio::test]
    async fn un_logo_trouve_est_mis_en_cache_comme_une_pochette_et_garde_l_id_qobuz() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-trouve");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let faux = FauxCatalogue::default();

        let n = cache
            .resoudre(&faux, vec![demande("verve")], || 1_000, Duration::ZERO)
            .await;

        assert_eq!(n, 1);
        let condensat = cache.image("Verve").expect("logo en cache");
        assert_eq!(condensat, crate::library::artwork::content_hash(PNG));
        assert!(crate::library::artwork::find_cached(dossier.path(), &condensat).is_some());
        assert_eq!(
            cache.entree("VERVE").unwrap().qobuz_label_id.as_deref(),
            Some("7")
        );

        // L'index survit à un redémarrage.
        let relu = CacheLogosLabels::ouvrir(dossier.path());
        assert_eq!(relu.image("Verve"), Some(condensat));
    }

    #[tokio::test]
    async fn l_absence_est_mise_en_cache_et_n_est_pas_redemandee_avant_le_delai() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-absent");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let faux = FauxCatalogue::default();
        let demandes = vec![demande("Inconnu Records"), demande("ECM")];

        let a_faire = cache.a_resoudre(&demandes, 1_000, PASSE_MAX);
        cache
            .resoudre(&faux, a_faire, || 1_000, Duration::ZERO)
            .await;
        assert_eq!(faux.appels.load(Ordering::SeqCst), 2);
        assert_eq!(cache.image("Inconnu Records"), None);
        assert_eq!(
            cache.entree("Inconnu Records").unwrap().statut,
            Statut::Absent
        );
        // Sans image, l'identifiant du label rapproché est gardé (#6782).
        assert_eq!(
            cache.entree("ECM").unwrap().qobuz_label_id.as_deref(),
            Some("9")
        );
        assert_eq!(faux.telechargements.load(Ordering::SeqCst), 0);

        // Une seconde liste, le lendemain : rien à redemander.
        assert!(
            cache
                .a_resoudre(&demandes, 1_000 + 24 * 3600, PASSE_MAX)
                .is_empty()
        );
        // Trente jours plus tard : on redemande.
        assert_eq!(
            cache
                .a_resoudre(&demandes, 1_000 + DELAI_ABSENT_S, PASSE_MAX)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn une_erreur_est_retentee_apres_un_jour_seulement() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-erreur");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let faux = FauxCatalogue::default();
        faux.en_panne.store(true, Ordering::SeqCst);
        let demandes = vec![demande("Verve")];

        cache
            .resoudre(&faux, demandes.clone(), || 1_000, Duration::ZERO)
            .await;
        assert_eq!(cache.entree("Verve").unwrap().statut, Statut::Erreur);
        assert!(
            cache
                .a_resoudre(&demandes, 1_000 + 3600, PASSE_MAX)
                .is_empty()
        );
        assert_eq!(
            cache
                .a_resoudre(&demandes, 1_000 + DELAI_ERREUR_S, PASSE_MAX)
                .len(),
            1
        );
    }

    #[test]
    fn une_passe_est_bornee_et_sans_doublon_de_cle() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-borne");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let mut demandes: Vec<Demande> = (0..50).map(|i| demande(&format!("Label {i}"))).collect();
        demandes.insert(0, demande("verve"));
        demandes.insert(1, demande("Verve"));

        let a_faire = cache.a_resoudre(&demandes, 0, PASSE_MAX);
        assert_eq!(a_faire.len(), PASSE_MAX);
        assert_eq!(
            a_faire
                .iter()
                .filter(|d| cle_de_label(&d.nom) == "verve")
                .count(),
            1
        );
    }

    #[test]
    fn une_seule_passe_a_la_fois() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-passe");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let garde = cache.reserver_passe().expect("première passe");
        assert!(
            cache.reserver_passe().is_none(),
            "une seconde passe ne démarre pas"
        );
        drop(garde);
        assert!(
            cache.reserver_passe().is_some(),
            "la passe finie libère la place"
        );
    }

    #[tokio::test]
    async fn un_logo_efface_du_cache_n_est_plus_annonce_et_se_redemande() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-efface");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let faux = FauxCatalogue::default();
        cache
            .resoudre(&faux, vec![demande("Verve")], || 1_000, Duration::ZERO)
            .await;
        let condensat = cache.image("Verve").unwrap();
        let (fichier, _) =
            crate::library::artwork::find_cached(dossier.path(), &condensat).unwrap();
        std::fs::remove_file(fichier).unwrap();

        assert_eq!(
            cache.image("Verve"),
            None,
            "jamais une adresse qui rend 404"
        );
        assert_eq!(
            cache
                .a_resoudre(&[demande("Verve")], 1_001, PASSE_MAX)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn l_album_rapproche_est_transmis_au_catalogue() {
        let dossier = crate::test_scratch::scratch_dir("logos-labels-album");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        let faux = FauxCatalogue::default();
        let d = Demande {
            nom: "Verve".into(),
            album_du_catalogue: Some("0602".into()),
        };
        cache.resoudre(&faux, vec![d], || 1, Duration::ZERO).await;
        assert_eq!(
            *faux.albums_vus.lock().unwrap(),
            vec![Some(String::from("0602"))]
        );
    }

    #[tokio::test]
    async fn une_image_qui_n_en_est_pas_une_n_entre_pas_au_cache() {
        struct Menteur;
        #[async_trait::async_trait]
        impl CatalogueDeLabels for Menteur {
            async fn trouver_label(
                &self,
                _: &str,
                _: Option<&str>,
            ) -> Result<Option<LabelDuCatalogue>, String> {
                Ok(Some(LabelDuCatalogue {
                    id: "1".into(),
                    nom: "X".into(),
                    image_url: Some("http://img.test/x".into()),
                    attribution: None,
                }))
            }
            async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
                Ok(b"<html>pas une image</html>".to_vec())
            }
        }
        let dossier = crate::test_scratch::scratch_dir("logos-labels-menteur");
        let cache = CacheLogosLabels::ouvrir(dossier.path());
        cache
            .resoudre(&Menteur, vec![demande("X")], || 1, Duration::ZERO)
            .await;
        assert_eq!(cache.entree("X").unwrap().statut, Statut::Erreur);
        assert_eq!(cache.image("X"), None);
    }
}
