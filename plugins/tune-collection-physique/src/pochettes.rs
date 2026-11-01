//! Le cache de pochettes (demande de Bertrand du 01/11/2026).
//!
//! Avant : la grille chargeait chaque vignette DIRECTEMENT depuis le CDN de
//! Discogs (`https://i.discogs.com/…`), à chaque affichage, depuis le
//! navigateur — 908 images à chaque ouverture de l'écran chez Bertrand.
//!
//! Maintenant : le serveur télécharge chaque pochette UNE fois et la range
//! dans le dossier de données du greffon, `pochettes/` :
//!
//! * `{release}.jpg` — la vignette, JPEG de [`COTE_VIGNETTE`] px au plus
//!   (la grande image de la page de collection, `cover_image`, réduite) ;
//! * `{release}-originale.{jpg|png|webp}` — l'image telle que Discogs l'a
//!   rendue (~600 px), gardée si elle pèse moins de [`ORIGINALE_MAX`].
//!
//! La route `GET /pochettes/{release}` les sert en local, avec
//! `Cache-Control`, un `ETag` et une réponse 304. L'écran n'appelle plus
//! jamais le CDN de Discogs.
//!
//! Le débit : UNE file (un téléchargement à la fois, verrou [`Pochettes`]),
//! espacée de [`Cadence::image`](crate::Cadence), et sur un 429 un recul
//! qui double à chaque refus ([`Cadence::attente_limite`](crate::Cadence)
//! puis jusqu'à [`Cadence::attente_max`](crate::Cadence)), au plus
//! [`Cadence::essais_limite`](crate::Cadence) fois. L'hôte envoie le
//! User-Agent du serveur.
//!
//! Le préremplissage : après chaque synchronisation, et au démarrage (pour
//! reprendre après un redémarrage), une passe en tâche de fond range les
//! pochettes qui manquent. Une pochette demandée par l'écran avant son tour
//! passe devant (même file).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::synchro::{Image, Moteur, Panne, attendre, code, verrou};

pub const DOSSIER: &str = "pochettes";
/// Le grand côté de la vignette : la grille l'affiche autour de 150 px, la
/// densité des écrans Retina double.
pub const COTE_VIGNETTE: u32 = 300;
/// L'originale n'est gardée qu'en dessous de ce poids.
pub const ORIGINALE_MAX: usize = 1_500_000;
/// La qualité JPEG de la vignette.
pub const QUALITE_JPEG: u8 = 85;

/// La taille demandée à la route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taille {
    Vignette,
    Originale,
}

/// Ce que l'écran voit du cache (`GET /etat`).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct EtatPochettes {
    pub en_cours: bool,
    /// Les disques de la liste dont la vignette est rangée.
    pub en_cache: usize,
    /// Ceux qui ont une image chez Discogs et pas encore de vignette ici.
    pub restantes: usize,
}

/// L'état du cache, porté par le [`Moteur`].
pub struct Pochettes {
    dossier: PathBuf,
    /// La file : un téléchargement à la fois, préremplissage et demandes
    /// de l'écran confondus.
    file: tokio::sync::Mutex<()>,
    /// Les pochettes que Discogs n'a plus (404) ou qui ne se lisent pas :
    /// pas redemandées avant le prochain démarrage ou effacement du cache.
    echecs: Mutex<HashSet<u64>>,
    en_cours: AtomicBool,
}

impl Pochettes {
    pub fn new(dossier_du_greffon: &Path) -> Self {
        Self {
            dossier: dossier_du_greffon.join(DOSSIER),
            file: tokio::sync::Mutex::new(()),
            echecs: Mutex::new(HashSet::new()),
            en_cours: AtomicBool::new(false),
        }
    }

    pub fn dossier(&self) -> &Path {
        &self.dossier
    }

    pub fn chemin_vignette(&self, release: u64) -> PathBuf {
        self.dossier.join(format!("{release}.jpg"))
    }

    /// L'originale rangée, quelle que soit son extension.
    pub fn chemin_originale(&self, release: u64) -> Option<PathBuf> {
        ["jpg", "png", "webp"]
            .iter()
            .map(|ext| self.dossier.join(format!("{release}-originale.{ext}")))
            .find(|p| p.is_file())
    }

    pub fn a_la_vignette(&self, release: u64) -> bool {
        self.chemin_vignette(release).is_file()
    }

    pub(crate) fn arretee(&self) {
        self.en_cours.store(false, Ordering::SeqCst);
    }

    /// Efface toutes les pochettes rangées.
    pub fn effacer(&self) -> Result<(), String> {
        verrou(&self.echecs).clear();
        match std::fs::remove_dir_all(&self.dossier) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Écrit `octets` à `chemin` par un fichier temporaire renommé : un arrêt
/// brutal ne laisse jamais une moitié d'image.
fn ecrire_entier(chemin: &Path, octets: &[u8]) -> Result<(), String> {
    let provisoire = chemin.with_extension("tmp");
    std::fs::write(&provisoire, octets).map_err(|e| e.to_string())?;
    std::fs::rename(&provisoire, chemin).map_err(|e| e.to_string())
}

/// Range une image téléchargée : l'originale (si elle est raisonnable),
/// puis la vignette — dont la présence dit « en cache ». Hors du runtime
/// (décodage et réduction).
pub fn ranger(dossier: &Path, release: u64, octets: &[u8]) -> Result<(), String> {
    let format = image::guess_format(octets).map_err(|e| e.to_string())?;
    let ext = match format {
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Png => "png",
        image::ImageFormat::WebP => "webp",
        autre => return Err(format!("format non pris : {autre:?}")),
    };
    let img = image::load_from_memory_with_format(octets, format).map_err(|e| e.to_string())?;
    let petite = if img.width() > COTE_VIGNETTE || img.height() > COTE_VIGNETTE {
        img.thumbnail(COTE_VIGNETTE, COTE_VIGNETTE)
    } else {
        img
    };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, QUALITE_JPEG)
        .encode_image(&petite.to_rgb8())
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dossier).map_err(|e| e.to_string())?;
    if octets.len() <= ORIGINALE_MAX {
        ecrire_entier(&dossier.join(format!("{release}-originale.{ext}")), octets)?;
    }
    ecrire_entier(&dossier.join(format!("{release}.jpg")), &jpeg)
}

impl Moteur {
    /// L'adresse Discogs de l'image d'une release de la liste : la grande
    /// (`cover_image`) d'abord, sinon la vignette (`thumb`).
    fn source_de_la_pochette(&self, release: u64) -> Option<String> {
        let m = verrou(&self.memoire);
        m.disques
            .iter()
            .find(|d| d.release_id == release)
            .and_then(|d| d.pochette.clone().or_else(|| d.vignette.clone()))
    }

    /// La release a-t-elle une image chez Discogs ?
    pub fn a_une_pochette(&self, release: u64) -> bool {
        self.source_de_la_pochette(release).is_some()
    }

    /// Le fichier d'une pochette, téléchargé d'abord s'il manque (par la
    /// file). `None` : pas dans la liste, sans image chez Discogs, ou le
    /// téléchargement n'a pas abouti.
    pub async fn pochette(&self, release: u64, taille: Taille) -> Option<PathBuf> {
        if !self.pochettes.a_la_vignette(release) {
            let url = self.source_de_la_pochette(release)?;
            self.telecharger(release, &url).await.ok()?;
        }
        let chemin = match taille {
            Taille::Vignette => self.pochettes.chemin_vignette(release),
            Taille::Originale => self
                .pochettes
                .chemin_originale(release)
                .unwrap_or_else(|| self.pochettes.chemin_vignette(release)),
        };
        chemin.is_file().then_some(chemin)
    }

    /// Télécharge et range UNE pochette, par la file, avec le recul d'un
    /// 429. `Err` : le code de l'échec (`discogs_limite` après les essais,
    /// `discogs_indisponible`, ou `introuvable`).
    pub(crate) async fn telecharger(&self, release: u64, url: &str) -> Result<(), &'static str> {
        let _tour = self.pochettes.file.lock().await;
        if self.pochettes.a_la_vignette(release) {
            return Ok(());
        }
        if verrou(&self.pochettes.echecs).contains(&release) {
            return Err(INTROUVABLE);
        }
        let mut refus = 0u32;
        let mut recul = self.cadence.attente_limite;
        loop {
            attendre(self.cadence.image).await;
            match self.hote.image_discogs(url).await {
                Ok(Image::Octets(octets)) => {
                    let dossier = self.pochettes.dossier().to_path_buf();
                    let range =
                        tokio::task::spawn_blocking(move || ranger(&dossier, release, &octets))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                    return match range {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            tracing::debug!(target: "plugin", release, erreur = %e, "collection_physique_pochette_illisible");
                            verrou(&self.pochettes.echecs).insert(release);
                            Err(INTROUVABLE)
                        }
                    };
                }
                Ok(Image::Introuvable) => {
                    verrou(&self.pochettes.echecs).insert(release);
                    return Err(INTROUVABLE);
                }
                Ok(Image::Limite) => {
                    refus += 1;
                    if refus > self.cadence.essais_limite {
                        tracing::info!(target: "plugin", release, "collection_physique_pochettes_limite");
                        return Err(code::DISCOGS_LIMITE);
                    }
                    tracing::debug!(target: "plugin", recul_ms = recul.as_millis() as u64, "collection_physique_pochette_recul");
                    tokio::task::yield_now().await;
                    attendre(recul).await;
                    recul =
                        (recul * 2).min(self.cadence.attente_max.max(self.cadence.attente_limite));
                }
                Err(Panne) => return Err(code::DISCOGS_INDISPONIBLE),
            }
        }
    }

    /// Les releases de la liste qui ont une image et pas encore de
    /// vignette rangée, dans l'ordre de la liste.
    fn pochettes_a_ranger(&self) -> Vec<(u64, String)> {
        let m = verrou(&self.memoire);
        let echecs = verrou(&self.pochettes.echecs);
        Moteur::releases(&m)
            .into_iter()
            .filter(|d| !echecs.contains(&d.release_id))
            .filter_map(|d| {
                let url = d.pochette.clone().or_else(|| d.vignette.clone())?;
                Some((d.release_id, url))
            })
            .filter(|(r, _)| !self.pochettes.a_la_vignette(*r))
            .collect()
    }

    /// L'état du cache, pour l'écran.
    pub fn etat_pochettes(&self) -> EtatPochettes {
        let (en_cache, avec_image) = {
            let m = verrou(&self.memoire);
            let releases = Moteur::releases(&m);
            let avec: Vec<u64> = releases
                .iter()
                .filter(|d| d.pochette.is_some() || d.vignette.is_some())
                .map(|d| d.release_id)
                .collect();
            let en_cache = avec
                .iter()
                .filter(|r| self.pochettes.a_la_vignette(**r))
                .count();
            (en_cache, avec.len())
        };
        let echecs = verrou(&self.pochettes.echecs).len();
        EtatPochettes {
            en_cours: self.pochettes.en_cours.load(Ordering::SeqCst),
            en_cache,
            restantes: avec_image.saturating_sub(en_cache).saturating_sub(echecs),
        }
    }

    /// Range, l'une après l'autre, les pochettes qui manquent. S'arrête au
    /// premier refus durable (429 après les essais, panne) : la passe
    /// suivante reprendra où celle-ci s'est arrêtée. Rend le nombre de
    /// pochettes rangées.
    pub async fn ranger_les_pochettes(&self) -> usize {
        let a_ranger = self.pochettes_a_ranger();
        if a_ranger.is_empty() {
            return 0;
        }
        tracing::info!(target: "plugin", restantes = a_ranger.len(), "collection_physique_pochettes");
        let mut rangees = 0;
        for (release, url) in a_ranger {
            match self.telecharger(release, &url).await {
                Ok(()) => rangees += 1,
                Err(INTROUVABLE) => {}
                Err(c) => {
                    tracing::info!(target: "plugin", code = c, rangees, "collection_physique_pochettes_arretees");
                    return rangees;
                }
            }
        }
        tracing::info!(target: "plugin", rangees, "collection_physique_pochettes_rangees");
        rangees
    }

    /// Lance le préremplissage en tâche de fond. `false` s'il tourne déjà
    /// ou s'il n'y a rien à ranger.
    pub fn lancer_pochettes(self: &Arc<Self>) -> bool {
        if self.pochettes_a_ranger().is_empty() {
            return false;
        }
        if self.pochettes.en_cours.swap(true, Ordering::SeqCst) {
            return false;
        }
        let moteur = self.clone();
        self.garder(tokio::spawn(async move {
            moteur.ranger_les_pochettes().await;
            moteur.pochettes.en_cours.store(false, Ordering::SeqCst);
        }));
        true
    }

    /// « Effacer le cache » (réglages du greffon) : arrête les tâches de
    /// fond, efface les pochettes rangées et, si `tout`, la mémoire (liste,
    /// fiches, liens MusicBrainz, streaming). Le compte suivi, qui est un
    /// réglage, reste ; la synchronisation suivante relit tout.
    pub fn effacer_le_cache(&self, tout: bool) -> Result<(), String> {
        self.arreter();
        self.pochettes.effacer()?;
        if tout {
            let mut m = verrou(&self.memoire);
            *m = crate::memoire::Memoire::default();
            m.ecrire(&self.dossier)?;
        }
        tracing::info!(target: "plugin", tout, "collection_physique_cache_efface");
        Ok(())
    }
}

/// Le code d'une pochette absente chez Discogs ou illisible.
pub const INTROUVABLE: &str = "introuvable";

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(cote: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(cote, cote, image::Rgb([200, 30, 30]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&img)
            .unwrap();
        out
    }

    #[test]
    fn une_grande_image_donne_une_vignette_de_300_et_garde_l_originale() {
        let dir = tempfile::tempdir().unwrap();
        ranger(dir.path(), 7, &jpeg(600)).unwrap();
        let v = image::open(dir.path().join("7.jpg")).unwrap();
        assert_eq!((v.width(), v.height()), (300, 300));
        assert!(dir.path().join("7-originale.jpg").is_file());
        assert!(!dir.path().join("7.tmp").exists());
    }

    #[test]
    fn une_petite_image_n_est_pas_agrandie() {
        let dir = tempfile::tempdir().unwrap();
        ranger(dir.path(), 8, &jpeg(150)).unwrap();
        let v = image::open(dir.path().join("8.jpg")).unwrap();
        assert_eq!(v.width(), 150);
    }

    #[test]
    fn des_octets_qui_ne_sont_pas_une_image_ne_rangent_rien() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ranger(dir.path(), 9, b"<html>pas une image</html>").is_err());
        assert!(!dir.path().join("9.jpg").exists());
    }
}
