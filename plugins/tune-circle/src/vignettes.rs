//! Les vignettes des pochettes partagées avec le cercle (#6521).
//!
//! Décision de Bertrand du 19/10/2026, qui modifie la décision 4 du 28/09
//! (#5325) : Cover Art Archive par MBID laissait 95 à 100 % des albums d'une
//! bibliothèque amie sans pochette, faute de MBID. Le serveur du propriétaire
//! envoie donc au cloud, pendant la synchro, une petite vignette par album
//! partagé ; mozaiklabs la sert aux SEULS membres de son cercle. Cover Art
//! Archive reste le repli côté écran.
//!
//! ## Garde-fous
//!
//! * Rien ne part si ce serveur ne partage pas sa bibliothèque avec au moins
//!   un cercle ([`library_sync::partage_de_cercle_actif`]), ni si le réglage
//!   [`CLE_REGLAGE`] est coupé. Le réglage est ACTIVÉ par défaut (décision de
//!   Bertrand du 19/10) ; le couper efface chez le cloud les vignettes déjà
//!   envoyées ([`Vignettes::couper`]).
//! * Une vignette est une image NEUVE : carrée, [`COTE`] px, JPEG réencodé
//!   depuis les pixels. Aucune métadonnée de la pochette d'origine (EXIF, XMP,
//!   ICC, commentaires) ne peut y survivre : l'encodeur n'en écrit aucune.
//! * Jamais une pochette pleine taille, jamais un chemin : la seule trace de
//!   la pochette locale est [`cle_de_source`], une empreinte opaque.
//! * Incrémental : le cloud rend, par album, l'empreinte SHA-256 de la
//!   vignette qu'il tient et la clé de source. Seules les vignettes nouvelles
//!   ou changées partent. Le manifeste est gardé en mémoire et relu toutes
//!   les [`RELIRE_LE_MANIFESTE`].
//! * Débit borné : au plus [`PLAFOND_PAR_PASSAGE`] vignettes par passage, par
//!   lots de [`PAR_ENVOI`], une pause de [`PAUSE_ENTRE_ENVOIS`] entre deux
//!   lots, sous la portée de débit [`CloudScope::CircleThumbnails`] (un 429
//!   est mémorisé et respecté).
//!
//! ## Contrat (site-mozaiklabs, `auth.cloud-library`, jeton de liaison exigé)
//!
//! * `GET {api}/{server_id}/thumbnails` → `{ thumbnails: { "<album_id>": { sha256, source } } }`
//! * `POST {api}/{server_id}/thumbnails` `{ thumbnails: [{ album_id, sha256, source, mime, data? }] }`
//!   → `{ accepted: [..], rejected: [{ album_id, error }] }` ; sans `data`,
//!   seule la source change (même image, autre pochette locale).
//! * `DELETE {api}/{server_id}/thumbnails` → `{ deleted }`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use tune_core::cloud::library_sync::{self, CLE_JETON_DE_LIAISON, EN_TETE_JETON_DE_SERVEUR};
use tune_core::cloud::rate_limit::{self, AppelCloud, CloudScope};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

/// Réglage « Partager les pochettes avec mon cercle ». Absent = ACTIVÉ
/// (décision de Bertrand du 19/10/2026) ; seul `"false"` le coupe.
pub const CLE_REGLAGE: &str = "circle_share_artwork";

/// Posé quand le réglage est coupé et tant que le cloud n'a pas confirmé
/// l'effacement : le passage suivant le redemande.
pub const CLE_EFFACEMENT_EN_ATTENTE: &str = "circle_share_artwork_purge_pending";

/// Le côté d'une vignette, en pixels.
pub const COTE: u32 = 160;
/// Qualité JPEG : ~5 à 12 Ko à 160 px pour une pochette réelle.
pub const QUALITE: u8 = 75;
/// Version de la fabrication : la changer fait repartir toutes les vignettes.
pub const VERSION: &str = "v1";

pub const PAR_ENVOI: usize = 20;
pub const PLAFOND_PAR_PASSAGE: usize = 200;
pub const PAUSE_ENTRE_ENVOIS: Duration = Duration::from_secs(1);
pub const RELIRE_LE_MANIFESTE: Duration = Duration::from_secs(6 * 3600);
/// Entre deux passages de fond.
pub const INTERVALLE: Duration = Duration::from_secs(15 * 60);

/// Le réglage est-il actif ? Absent ou illisible = oui (défaut du 19/10).
pub fn reglage_actif(settings: &SettingsRepo) -> bool {
    !settings
        .get(CLE_REGLAGE)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("false"))
}

/// Une vignette carrée de [`COTE`] px, en JPEG, sans aucune métadonnée, ou
/// `None` si l'image ne se décode pas (format non compilé, bornes dépassées).
///
/// Mêmes bornes de décodage que `tune_core::library::artwork::make_thumbnail`.
/// L'image est recadrée au centre (`resize_to_fill`) puis RÉENCODÉE depuis ses
/// pixels : ni EXIF, ni XMP, ni profil ICC, ni commentaire n'est recopié.
pub fn vignette(source: &[u8]) -> Option<Vec<u8>> {
    use std::io::Cursor;
    let mut lecteur = image::ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .ok()?;
    let mut bornes = image::Limits::default();
    bornes.max_image_width = Some(8192);
    bornes.max_image_height = Some(8192);
    bornes.max_alloc = Some(256 * 1024 * 1024);
    lecteur.limits(bornes);
    let image = lecteur.decode().ok()?;
    let carree = image
        .resize_to_fill(COTE, COTE, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let mut sortie = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut sortie, QUALITE)
        .encode(
            carree.as_raw(),
            carree.width(),
            carree.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(sortie.into_inner())
}

/// SHA-256 en hexadécimal minuscule.
pub fn empreinte(octets: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(octets);
    format!("{:x}", h.finalize())
}

/// L'empreinte opaque de la pochette locale d'un album : elle change quand
/// la pochette change, et ne dit rien d'elle (ni chemin, ni nom de fichier).
pub fn cle_de_source(cover_path: &str) -> String {
    empreinte(
        format!(
            "tune-circle-vignette:{VERSION}:{COTE}:{}",
            cover_path.trim()
        )
        .as_bytes(),
    )
}

/// Ce que le cloud tient pour un album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenue {
    pub sha256: String,
    pub source: Option<String>,
}

/// Le bilan d'un passage.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bilan {
    /// Vignettes envoyées avec leurs octets.
    pub envoyees: usize,
    /// Seule la clé de source a changé : rien d'autre n'est parti.
    pub sources_seules: usize,
    pub refusees: usize,
    /// Le passage n'a rien fait (réglage coupé, pas de partage, pas de session…).
    pub rien: bool,
    /// Le cloud a confirmé l'effacement de toutes les vignettes.
    pub effacees: bool,
}

pub struct Vignettes {
    backend: Arc<dyn DbBackend>,
    artwork_dir: PathBuf,
    /// `…/api/v1/cloud-library`, sans barre finale.
    api: String,
    client: reqwest::Client,
    manifeste: Mutex<Option<(Instant, HashMap<i64, Tenue>)>>,
    pause: Duration,
    reveil: tokio::sync::Notify,
}

impl Vignettes {
    pub fn new(backend: Arc<dyn DbBackend>, artwork_dir: PathBuf, api: &str) -> Self {
        Self {
            backend,
            artwork_dir,
            api: api.trim_end_matches('/').to_string(),
            client: tune_core::http::client::shared().clone(),
            manifeste: Mutex::new(None),
            pause: PAUSE_ENTRE_ENVOIS,
            reveil: tokio::sync::Notify::new(),
        }
    }

    /// Pour les bancs : pas de pause entre deux lots.
    #[doc(hidden)]
    pub fn sans_pause(mut self) -> Self {
        self.pause = Duration::ZERO;
        self
    }

    pub fn reglages(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.backend.clone())
    }

    /// Avance le prochain passage de fond.
    pub fn reveiller(&self) {
        self.reveil.notify_one();
    }

    /// La boucle de fond : un passage toutes les [`INTERVALLE`], ou plus tôt
    /// sur [`Self::reveiller`].
    pub async fn tourner(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(180)).await;
        loop {
            let bilan = self.passage().await;
            if !bilan.rien {
                info!(
                    envoyees = bilan.envoyees,
                    sources_seules = bilan.sources_seules,
                    refusees = bilan.refusees,
                    effacees = bilan.effacees,
                    "circle_vignettes_passage"
                );
            }
            tokio::select! {
                _ = tokio::time::sleep(INTERVALLE) => {}
                _ = self.reveil.notified() => {}
            }
        }
    }

    /// Coupe ou rallume le réglage. Couper efface aussitôt chez le cloud ; un
    /// échec laisse [`CLE_EFFACEMENT_EN_ATTENTE`], que le passage suivant
    /// rejoue. Rend `true` si le cloud a confirmé (ou s'il n'y avait rien à
    /// effacer faute de session).
    pub async fn regler(&self, actif: bool) -> bool {
        let s = self.reglages();
        s.set(CLE_REGLAGE, if actif { "true" } else { "false" })
            .ok();
        *self.manifeste.lock().await = None;
        if actif {
            s.delete(CLE_EFFACEMENT_EN_ATTENTE).ok();
            self.reveiller();
            return true;
        }
        s.set(CLE_EFFACEMENT_EN_ATTENTE, "true").ok();
        self.couper().await
    }

    /// Demande au cloud d'effacer toutes les vignettes de ce serveur.
    async fn couper(&self) -> bool {
        let s = self.reglages();
        let Some((server_id, acces, liaison)) = identite(&s) else {
            // Pas de session ni de liaison : rien n'a pu partir, et le cloud
            // efface lui-même les vignettes d'un serveur délié.
            s.delete(CLE_EFFACEMENT_EN_ATTENTE).ok();
            return true;
        };
        let requete = self
            .client
            .delete(format!("{}/{server_id}/thumbnails", self.api))
            .bearer_auth(acces)
            .header(EN_TETE_JETON_DE_SERVEUR, liaison)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(15));
        match rate_limit::appeler(&s, CloudScope::CircleThumbnails, requete).await {
            AppelCloud::Reponse(r) if r.status().is_success() => {
                s.delete(CLE_EFFACEMENT_EN_ATTENTE).ok();
                info!("circle_vignettes_effacees");
                true
            }
            AppelCloud::Reponse(r) => {
                warn!(
                    statut = r.status().as_u16(),
                    "circle_vignettes_effacement_refuse"
                );
                false
            }
            _ => false,
        }
    }

    /// Un passage : effacement en attente, ou envoi des vignettes nouvelles
    /// ou changées, sous les plafonds.
    pub async fn passage(&self) -> Bilan {
        let s = self.reglages();
        let mut bilan = Bilan::default();

        if !reglage_actif(&s) {
            if s.get(CLE_EFFACEMENT_EN_ATTENTE).ok().flatten().is_some() {
                bilan.effacees = self.couper().await;
                return bilan;
            }
            bilan.rien = true;
            return bilan;
        }
        if !library_sync::partage_de_cercle_actif(&s) {
            bilan.rien = true;
            return bilan;
        }
        let Some((server_id, acces, liaison)) = identite(&s) else {
            bilan.rien = true;
            return bilan;
        };

        let Some(tenues) = self.manifeste(&s, &server_id, &acces, &liaison).await else {
            bilan.rien = true;
            return bilan;
        };

        // Les albums de la bibliothèque qui ont une pochette LOCALE.
        let mut a_envoyer: Vec<(i64, String, String)> = Vec::new();
        for (album_id, cover_path) in albums_avec_pochette(&self.backend) {
            let source = cle_de_source(&cover_path);
            if tenues.get(&album_id).and_then(|t| t.source.as_deref()) == Some(source.as_str()) {
                continue;
            }
            a_envoyer.push((album_id, cover_path, source));
            if a_envoyer.len() >= PLAFOND_PAR_PASSAGE {
                break;
            }
        }
        if a_envoyer.is_empty() {
            bilan.rien = true;
            return bilan;
        }

        let mut lot: Vec<Value> = Vec::new();
        let mut premiers = true;
        for (album_id, cover_path, source) in a_envoyer {
            let Some(octets) = self.lire_la_pochette(&cover_path).await else {
                continue;
            };
            let Some(v) = tokio::task::spawn_blocking(move || vignette(&octets))
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            let sha = empreinte(&v);
            let mut item = json!({
                "album_id": album_id, "sha256": sha, "source": source, "mime": "image/jpeg",
            });
            if tenues.get(&album_id).map(|t| t.sha256.as_str()) != Some(sha.as_str()) {
                use base64::Engine as _;
                item["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&v));
            }
            lot.push(item);
            if lot.len() >= PAR_ENVOI {
                if !premiers && !self.pause.is_zero() {
                    tokio::time::sleep(self.pause).await;
                }
                premiers = false;
                if !self
                    .envoyer(
                        &s,
                        &server_id,
                        &acces,
                        &liaison,
                        std::mem::take(&mut lot),
                        &mut bilan,
                    )
                    .await
                {
                    return bilan;
                }
            }
        }
        if !lot.is_empty() {
            if !premiers && !self.pause.is_zero() {
                tokio::time::sleep(self.pause).await;
            }
            self.envoyer(&s, &server_id, &acces, &liaison, lot, &mut bilan)
                .await;
        }
        bilan
    }

    /// La pochette locale d'un album, depuis le cache de pochettes. Une
    /// adresse distante (pochette de service) n'est jamais téléchargée ici.
    async fn lire_la_pochette(&self, cover_path: &str) -> Option<Vec<u8>> {
        use tune_core::library::artwork;
        let c = cover_path.trim();
        if c.starts_with("http://") || c.starts_with("https://") {
            return None;
        }
        let est_un_condensat =
            (c.len() == 32 || c.len() == 64) && c.chars().all(|x| x.is_ascii_hexdigit());
        let condensat = if est_un_condensat {
            c.to_string()
        } else {
            artwork::artwork_hash(c)
        };
        let (chemin, _) = artwork::find_cached(&self.artwork_dir, &condensat)?;
        tokio::fs::read(chemin).await.ok()
    }

    /// Le manifeste du cloud, relu s'il manque ou date de plus de
    /// [`RELIRE_LE_MANIFESTE`]. `None` si le cloud ne l'a pas rendu.
    async fn manifeste(
        &self,
        s: &SettingsRepo,
        server_id: &str,
        acces: &str,
        liaison: &str,
    ) -> Option<HashMap<i64, Tenue>> {
        let mut garde = self.manifeste.lock().await;
        if let Some((quand, m)) = garde.as_ref()
            && quand.elapsed() < RELIRE_LE_MANIFESTE
        {
            return Some(m.clone());
        }
        let requete = self
            .client
            .get(format!("{}/{server_id}/thumbnails", self.api))
            .bearer_auth(acces)
            .header(EN_TETE_JETON_DE_SERVEUR, liaison)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(30));
        let AppelCloud::Reponse(r) =
            rate_limit::appeler(s, CloudScope::CircleThumbnails, requete).await
        else {
            return None;
        };
        if !r.status().is_success() {
            debug!(
                statut = r.status().as_u16(),
                "circle_vignettes_manifeste_refuse"
            );
            return None;
        }
        let corps: Value = r.json().await.ok()?;
        let mut m = HashMap::new();
        for (cle, v) in corps.get("thumbnails")?.as_object()? {
            let (Ok(id), Some(sha)) = (cle.parse::<i64>(), v.get("sha256").and_then(Value::as_str))
            else {
                continue;
            };
            m.insert(
                id,
                Tenue {
                    sha256: sha.to_string(),
                    source: v.get("source").and_then(Value::as_str).map(str::to_string),
                },
            );
        }
        *garde = Some((Instant::now(), m.clone()));
        Some(m)
    }

    /// Envoie un lot. `false` : arrêter le passage (refus, quota, panne, 429).
    async fn envoyer(
        &self,
        s: &SettingsRepo,
        server_id: &str,
        acces: &str,
        liaison: &str,
        lot: Vec<Value>,
        bilan: &mut Bilan,
    ) -> bool {
        let avec_donnees: HashMap<i64, bool> = lot
            .iter()
            .filter_map(|i| Some((i["album_id"].as_i64()?, i.get("data").is_some())))
            .collect();
        let tenues: HashMap<i64, Tenue> = lot
            .iter()
            .filter_map(|i| {
                Some((
                    i["album_id"].as_i64()?,
                    Tenue {
                        sha256: i["sha256"].as_str()?.to_string(),
                        source: i["source"].as_str().map(str::to_string),
                    },
                ))
            })
            .collect();
        let requete = self
            .client
            .post(format!("{}/{server_id}/thumbnails", self.api))
            .bearer_auth(acces)
            .header(EN_TETE_JETON_DE_SERVEUR, liaison)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({ "thumbnails": lot }))
            .timeout(Duration::from_secs(30));
        let r = match rate_limit::appeler(s, CloudScope::CircleThumbnails, requete).await {
            AppelCloud::Reponse(r) => r,
            AppelCloud::Retenu(_) => return false,
            AppelCloud::Erreur(e) => {
                warn!(error = %e.without_url(), "circle_vignettes_envoi_echoue");
                return false;
            }
        };
        if !r.status().is_success() {
            // 403 `circle_share_required` (partage coupé entre-temps), 429,
            // 5xx : on s'arrête, le passage suivant reprendra.
            debug!(
                statut = r.status().as_u16(),
                "circle_vignettes_envoi_refuse"
            );
            *self.manifeste.lock().await = None;
            return false;
        }
        let Ok(corps) = r.json::<Value>().await else {
            return false;
        };
        let mut garde = self.manifeste.lock().await;
        for id in corps["accepted"].as_array().into_iter().flatten() {
            let Some(id) = id.as_i64() else { continue };
            if avec_donnees.get(&id) == Some(&true) {
                bilan.envoyees += 1;
            } else {
                bilan.sources_seules += 1;
            }
            if let (Some((_, m)), Some(t)) = (garde.as_mut(), tenues.get(&id)) {
                m.insert(id, t.clone());
            }
        }
        let mut quota = false;
        for refus in corps["rejected"].as_array().into_iter().flatten() {
            bilan.refusees += 1;
            if refus["error"].as_str() == Some("thumbnail_quota_exceeded") {
                quota = true;
            }
        }
        if quota {
            info!("circle_vignettes_quota_atteint");
            return false;
        }
        true
    }
}

/// `server_id`, jeton d'accès SSO, jeton de liaison : les trois, ou rien.
fn identite(s: &SettingsRepo) -> Option<(String, String, String)> {
    let lire = |cle: &str| {
        s.get(cle)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some((
        lire("server_id")?,
        lire("mozaik_access_token")?,
        lire(CLE_JETON_DE_LIAISON)?,
    ))
}

/// `(album_id, cover_path)` des albums qui ont au moins une piste et une
/// pochette, par identifiant croissant.
fn albums_avec_pochette(backend: &Arc<dyn DbBackend>) -> Vec<(i64, String)> {
    backend
        .query_many(
            "SELECT a.id, a.cover_path FROM albums a \
             WHERE a.cover_path IS NOT NULL AND a.cover_path <> '' \
               AND EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id) \
             ORDER BY a.id",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_avec_exif(l: u32, h: u32) -> Vec<u8> {
        // Un JPEG ordinaire, puis un segment APP1 « Exif » glissé après SOI,
        // qui porte une marque reconnaissable.
        let img = image::RgbImage::from_fn(l, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        let mut brut = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut brut, 90)
            .encode(img.as_raw(), l, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        let brut = brut.into_inner();
        let mut charge = b"Exif\0\0GPS-SECRET-6521 Appareil-Photo-de-Bertrand".to_vec();
        let longueur = (charge.len() + 2) as u16;
        let mut sortie = vec![0xFF, 0xD8, 0xFF, 0xE1];
        sortie.extend_from_slice(&longueur.to_be_bytes());
        sortie.append(&mut charge);
        sortie.extend_from_slice(&brut[2..]);
        sortie
    }

    /// Les marqueurs d'un JPEG jusqu'au début des données d'image.
    fn marqueurs(j: &[u8]) -> Vec<u8> {
        let mut i = 2;
        let mut vus = Vec::new();
        while i + 4 <= j.len() && j[i] == 0xFF {
            let m = j[i + 1];
            vus.push(m);
            if m == 0xDA {
                break;
            }
            i += 2 + u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        }
        vus
    }

    #[test]
    fn la_vignette_est_un_carre_de_160_sans_aucune_metadonnee() {
        let source = jpeg_avec_exif(1200, 900);
        assert!(
            source.windows(4).any(|w| w == b"Exif"),
            "le banc porte bien un EXIF"
        );
        let v = vignette(&source).expect("décodable");
        let lue = image::load_from_memory(&v).unwrap();
        assert_eq!((lue.width(), lue.height()), (COTE, COTE));
        assert!(!v.windows(4).any(|w| w == b"Exif"));
        assert!(!v.windows(10).any(|w| w == b"GPS-SECRET"));
        // Aucun APPn hors APP0 (JFIF), aucun commentaire.
        let m = marqueurs(&v);
        assert!(
            m.iter().all(|&x| !(0xE1..=0xEF).contains(&x) && x != 0xFE),
            "{m:x?}"
        );
        assert!(v.len() < 32 * 1024, "{} octets", v.len());
    }

    #[test]
    fn une_petite_pochette_est_aussi_mise_au_carre() {
        let v = vignette(&jpeg_avec_exif(90, 60)).unwrap();
        let lue = image::load_from_memory(&v).unwrap();
        assert_eq!((lue.width(), lue.height()), (COTE, COTE));
    }

    #[test]
    fn ce_qui_n_est_pas_une_image_ne_donne_rien() {
        assert!(vignette(b"pas une image").is_none());
    }

    #[test]
    fn empreinte_et_cle_de_source() {
        assert_eq!(
            empreinte(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let a = cle_de_source("/Users/secret/Musique/cover.jpg");
        assert_eq!(a.len(), 64);
        assert!(!a.contains("secret"));
        assert_ne!(a, cle_de_source("/Users/secret/Musique/cover2.jpg"));
        assert_eq!(a, cle_de_source(" /Users/secret/Musique/cover.jpg "));
    }
}
