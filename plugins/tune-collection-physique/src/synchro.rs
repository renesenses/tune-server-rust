//! La synchronisation en tâche de fond : lire la collection, la chercher
//! sur les services, la rapprocher de la bibliothèque, puis l'affiner par
//! MusicBrainz.
//!
//! # Les étapes, dans l'ordre (essai réel du 31/10/2026, correctif rc10)
//!
//! 1. **Liste** — toutes les pages de la collection (100 disques par
//!    page). La liste remplace l'ancienne : un disque vendu en sort.
//! 2. **Streaming** — disque par disque : sa fiche Discogs si on ne la
//!    connaît pas encore (pour son code-barre, une seule fois par disque),
//!    puis Qobuz et TIDAL, par code-barre d'abord, puis par artiste + titre,
//!    pour un disque qui n'est pas déjà reconnu dans la bibliothèque.
//!    « En streaming » se remplit dès les premières minutes.
//! 3. **Bibliothèque** — le rapprochement avec les albums locaux (code-barre,
//!    artiste + titre, et les liens MusicBrainz déjà connus).
//! 4. **Affinage MusicBrainz** — À PART, en tâche de fond
//!    ([`Moteur::lancer_affinage`]) : les identifiants reliés à la release
//!    et au master Discogs (`/ws/2/url?resource=…`). Il ne sert qu'à mieux
//!    reconnaître un disque dans la bibliothèque : il ne bloque rien, ne
//!    laisse aucune erreur, recule exponentiellement quand MusicBrainz
//!    refuse ou se tait, et reprend après un redémarrage là où il s'était
//!    arrêté (ce qui est appris est rangé dans la mémoire).
//!
//! Avant le correctif, MusicBrainz passait AVANT les services : sur le .18
//! (908 disques, 680 à rapprocher), 6 disques en 10 minutes, et 531 disques
//! « à vérifier » pendant des heures.
//!
//! Une panne de Discogs arrête la synchronisation et laisse ce qui a été
//! appris : la suivante reprend où celle-ci s'est arrêtée. La mémoire est
//! écrite tous les [`ECRIRE_TOUS_LES`] disques.
//!
//! # Débit
//!
//! Discogs autorise 60 requêtes par minute avec un jeton, 25 sans. L'hôte
//! fait passer chaque appel par le limiteur Discogs PARTAGÉ du serveur
//! (`tune_core::http::fetch::DISCOGS`, ralenti sur un 429) ; le greffon
//! espace EN PLUS ses propres appels ([`Cadence`]) pour tenir sous le
//! plafond du régime sans jeton et laisser de la place aux autres passes.
//! MusicBrainz : le limiteur partagé de l'hôte (1 par seconde), plus
//! [`Cadence::musicbrainz`] ici. Les services : un appel toutes les
//! [`Cadence::service`].
//!
//! Un 429 de Discogs : attendre [`Cadence::attente_limite`], réessayer, au
//! plus [`Cadence::essais_limite`] fois.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::correspondance::{
    AlbumDeService, AlbumLocal, Index, LiensMusicBrainz, choisir_sur_le_service,
};
use crate::discogs::{self, Disque, PAGES_MAX, PAR_PAGE};
use crate::memoire::{Fiche, Liens, Memoire, Streaming, SurUnService};

pub const ECRIRE_TOUS_LES: usize = 10;

/// Les services de streaming interrogés, dans cet ordre (cadrage de #7195).
pub const SERVICES: [&str; 2] = ["qobuz", "tidal"];

/// Une réponse d'un service tiers, telle que l'hôte la rend.
#[derive(Debug, Clone, PartialEq)]
pub enum Reponse {
    Json(Value),
    /// 404 (ou 400 pour MusicBrainz) : l'objet n'existe pas.
    Introuvable,
    /// 401 / 403 : refusé (collection privée, jeton refusé).
    Refusee,
    /// 429 / 503 : trop d'appels.
    Limite,
}

/// Une image de Discogs, telle que l'hôte la rend (cache de pochettes).
#[derive(Debug, Clone, PartialEq)]
pub enum Image {
    /// Les octets de l'image (bornés par l'hôte).
    Octets(Vec<u8>),
    /// 404 / 403 / 410 : l'image n'existe plus ; ne pas la redemander.
    Introuvable,
    /// 429 / 503 : trop d'appels ; reculer.
    Limite,
}

/// Le service n'a pas répondu (transport, délai, statut inattendu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panne;

/// Le jeton Discogs dont dispose le serveur. Sa VALEUR ne sort jamais de
/// l'hôte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Jeton {
    /// Aucun : collection publique seulement, 25 appels par minute.
    Aucun,
    /// Le jeton du serveur (`tune.toml`, environnement) : il lit les
    /// collections PUBLIQUES, pas celle, privée, de l'utilisateur.
    Integre,
    /// Le jeton personnel de l'utilisateur (Réglages › Clés d'API), rangé
    /// scellé dans le coffre de Tune : il lit sa collection, même privée.
    Personnel,
}

/// Ce que l'hôte fait pour le greffon. Aucun appel réseau dans ce crate :
/// tout passe ici.
#[async_trait]
pub trait Hote: Send + Sync {
    fn jeton_discogs(&self) -> Jeton;
    /// `GET https://api.discogs.com{chemin}?{params}`, sous le limiteur
    /// partagé, avec le User-Agent du serveur et le jeton résolu.
    async fn discogs(&self, chemin: &str, params: &[(&str, String)]) -> Result<Reponse, Panne>;
    /// `GET https://musicbrainz.org/ws/2/{chemin}?{params}&fmt=json`, sous le
    /// limiteur partagé.
    async fn musicbrainz(&self, chemin: &str, params: &[(&str, String)]) -> Result<Reponse, Panne>;
    /// Ceux de [`SERVICES`] qui sont activés et connectés.
    async fn services_connectes(&self) -> Vec<String>;
    /// Les albums d'une recherche chez `service`.
    async fn chercher_albums(
        &self,
        service: &str,
        requete: &str,
    ) -> Result<Vec<AlbumDeService>, Panne>;
    /// Les albums LOCAUX de la bibliothèque, masqués exclus.
    async fn albums_locaux(&self) -> Result<Vec<AlbumLocal>, String>;
    /// `GET` d'une image de Discogs (`url` déjà passée par
    /// [`discogs::vignette_discogs`] : `https`, hôte d'images de Discogs),
    /// avec le User-Agent du serveur et SANS le jeton (le CDN n'en veut
    /// pas ; il ne sort pas vers un autre hôte que l'API).
    async fn image_discogs(&self, url: &str) -> Result<Image, Panne>;
}

/// L'espacement des appels du greffon.
#[derive(Debug, Clone)]
pub struct Cadence {
    pub discogs_avec_jeton: Duration,
    pub discogs_sans_jeton: Duration,
    pub service: Duration,
    /// L'espacement des appels de l'affinage MusicBrainz (en plus du
    /// limiteur partagé de l'hôte).
    pub musicbrainz: Duration,
    /// Discogs : l'attente après un 429. MusicBrainz : le PREMIER recul
    /// après un refus ou un silence, doublé à chaque échec de suite.
    pub attente_limite: Duration,
    /// Le plus long recul de l'affinage MusicBrainz.
    pub attente_max: Duration,
    pub essais_limite: u32,
    /// L'espacement des téléchargements de pochettes (cache de pochettes).
    pub image: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            // 1,1 s : sous 60 par minute avec la marge de MetaRust.
            discogs_avec_jeton: Duration::from_millis(1_100),
            // 2,5 s : 24 par minute, sous les 25 du régime sans jeton.
            discogs_sans_jeton: Duration::from_millis(2_500),
            service: Duration::from_millis(1_500),
            // 1 par seconde : la règle de MusicBrainz.
            musicbrainz: Duration::from_secs(1),
            attente_limite: Duration::from_secs(60),
            attente_max: Duration::from_secs(30 * 60),
            essais_limite: 3,
            // Une pochette par seconde : 908 disques en un quart d'heure,
            // sans jamais solliciter le CDN de Discogs en rafale.
            image: Duration::from_secs(1),
        }
    }
}

impl Cadence {
    /// Sans attente, pour les essais.
    pub fn immediate() -> Self {
        Self {
            discogs_avec_jeton: Duration::ZERO,
            discogs_sans_jeton: Duration::ZERO,
            service: Duration::ZERO,
            musicbrainz: Duration::ZERO,
            attente_limite: Duration::ZERO,
            attente_max: Duration::ZERO,
            essais_limite: 3,
            image: Duration::ZERO,
        }
    }
}

/// L'étape en cours de la synchronisation, dans l'ordre où elles se
/// suivent. L'affinage MusicBrainz, qui vient après, a son propre
/// avancement ([`Affinage`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Etape {
    Liste,
    Streaming,
    Bibliotheque,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Avancement {
    pub en_cours: bool,
    pub etape: Option<Etape>,
    pub fait: usize,
    pub total: usize,
}

/// L'affinage MusicBrainz, tel que l'écran le dit discrètement
/// (« Affinage MusicBrainz : N restants »).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Affinage {
    pub en_cours: bool,
    /// Les disques de la liste dont les liens MusicBrainz restent à lire.
    pub restants: usize,
}

/// Les codes d'erreur d'une synchronisation : stables, traduits par l'écran.
pub mod code {
    pub const AUCUN_COMPTE: &str = "aucun_compte";
    pub const UTILISATEUR_INCONNU: &str = "utilisateur_inconnu";
    pub const COLLECTION_PRIVEE: &str = "collection_privee";
    pub const DISCOGS_LIMITE: &str = "discogs_limite";
    pub const DISCOGS_INDISPONIBLE: &str = "discogs_indisponible";
    /// N'est PLUS rendu (MusicBrainz ne bloque plus rien) : une mémoire
    /// d'avant le correctif qui le porte encore l'oublie à la lecture.
    pub const MUSICBRAINZ_INDISPONIBLE: &str = "musicbrainz_indisponible";
    pub const BIBLIOTHEQUE_ILLISIBLE: &str = "bibliotheque_illisible";
    pub const JETON_PERSONNEL_ABSENT: &str = "jeton_personnel_absent";
    pub const JETON_REFUSE: &str = "jeton_refuse";
    pub const ECRITURE: &str = "ecriture";
}

pub fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) async fn attendre(d: Duration) {
    if !d.is_zero() {
        tokio::time::sleep(d).await;
    }
}

pub struct Moteur {
    pub(crate) hote: Arc<dyn Hote>,
    pub(crate) dossier: PathBuf,
    pub(crate) memoire: Mutex<Memoire>,
    avancement: Mutex<Avancement>,
    en_cours: AtomicBool,
    affinage_en_cours: AtomicBool,
    taches: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub(crate) cadence: Cadence,
    pub(crate) pochettes: crate::pochettes::Pochettes,
}

pub(crate) fn verrou<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ce que la recherche d'un disque chez UN service a donné.
enum SurLeService {
    Trouve(SurUnService),
    /// Le service a répondu à chaque recherche, sans le disque.
    Absent,
    /// Le service n'a pas répondu (au moins une recherche) : il n'a pas dit
    /// son dernier mot, le disque n'est pas « nulle part ».
    SansReponse,
}

impl Moteur {
    pub fn new(hote: Arc<dyn Hote>, dossier: PathBuf, cadence: Cadence) -> Self {
        let mut memoire = Memoire::lire(&dossier);
        // Le message rouge d'avant le correctif : MusicBrainz ne bloque plus.
        if memoire.derniere_erreur.as_deref() == Some(code::MUSICBRAINZ_INDISPONIBLE) {
            memoire.derniere_erreur = None;
        }
        let pochettes = crate::pochettes::Pochettes::new(&dossier);
        Self {
            hote,
            dossier,
            memoire: Mutex::new(memoire),
            avancement: Mutex::new(Avancement::default()),
            en_cours: AtomicBool::new(false),
            affinage_en_cours: AtomicBool::new(false),
            taches: Mutex::new(Vec::new()),
            pochettes,
            cadence,
        }
    }

    pub fn memoire(&self) -> Memoire {
        verrou(&self.memoire).clone()
    }

    pub fn avancement(&self) -> Avancement {
        let mut a = verrou(&self.avancement).clone();
        a.en_cours = self.en_cours.load(Ordering::SeqCst);
        a
    }

    /// L'affinage MusicBrainz : en cours ou non, et combien de disques de la
    /// liste restent à lire.
    pub fn affinage(&self) -> Affinage {
        let t = maintenant();
        let restants = {
            let m = verrou(&self.memoire);
            Self::releases(&m)
                .iter()
                .filter(|d| m.liens_a_revoir(d.release_id, t))
                .count()
        };
        Affinage {
            en_cours: self.affinage_en_cours.load(Ordering::SeqCst),
            restants,
        }
    }

    /// Arrête les tâches de fond (synchronisation, affinage). Ce qui a été
    /// appris et écrit reste dans la mémoire.
    pub fn arreter(&self) {
        for t in verrou(&self.taches).drain(..) {
            t.abort();
        }
        self.en_cours.store(false, Ordering::SeqCst);
        self.affinage_en_cours.store(false, Ordering::SeqCst);
        self.pochettes.arretee();
    }

    pub(crate) fn garder(&self, t: tokio::task::JoinHandle<()>) {
        let mut taches = verrou(&self.taches);
        taches.retain(|t| !t.is_finished());
        taches.push(t);
    }

    /// Change le compte suivi : la liste d'un autre compte est oubliée.
    pub fn changer_de_compte(&self, nom: Option<String>) -> Result<(), String> {
        let mut m = verrou(&self.memoire);
        m.changer_de_compte(nom);
        m.ecrire(&self.dossier)
    }

    fn ecrire(&self) -> Result<(), &'static str> {
        verrou(&self.memoire).ecrire(&self.dossier).map_err(|e| {
            tracing::warn!(target: "plugin", erreur = %e, "collection_physique_ecriture_echouee");
            code::ECRITURE
        })
    }

    fn etape(&self, etape: Etape, total: usize) {
        *verrou(&self.avancement) = Avancement {
            en_cours: true,
            etape: Some(etape),
            fait: 0,
            total,
        };
    }

    fn un_de_plus(&self) {
        verrou(&self.avancement).fait += 1;
    }

    /// Lance une synchronisation de `nom` en tâche de fond, puis l'affinage
    /// MusicBrainz. `false` si une synchronisation tourne déjà.
    pub fn lancer(self: &Arc<Self>, nom: String) -> bool {
        if self.en_cours.swap(true, Ordering::SeqCst) {
            return false;
        }
        let moteur = self.clone();
        self.garder(tokio::spawn(async move {
            let _ = moteur.synchroniser_maintenant(&nom).await;
            // Même après une panne de Discogs : la liste déjà lue s'affine,
            // et ses pochettes se rangent.
            moteur.lancer_affinage();
            moteur.lancer_pochettes();
        }));
        true
    }

    /// Lance l'affinage MusicBrainz en tâche de fond, s'il reste des disques
    /// à lire. `false` s'il tourne déjà ou s'il n'y a rien à faire. Au
    /// démarrage du greffon, c'est ainsi qu'il reprend où il s'était arrêté.
    pub fn lancer_affinage(self: &Arc<Self>) -> bool {
        if self.affinage().restants == 0 {
            return false;
        }
        if self.affinage_en_cours.swap(true, Ordering::SeqCst) {
            return false;
        }
        let moteur = self.clone();
        self.garder(tokio::spawn(async move {
            moteur.affiner().await;
            moteur.affinage_en_cours.store(false, Ordering::SeqCst);
        }));
        true
    }

    /// L'affinage, attendu jusqu'au bout (essais). Ne rend la main qu'une
    /// fois chaque disque de la liste lu chez MusicBrainz.
    pub async fn affiner_maintenant(&self) {
        self.affinage_en_cours.store(true, Ordering::SeqCst);
        self.affiner().await;
        self.affinage_en_cours.store(false, Ordering::SeqCst);
    }

    /// Synchronise `nom` et attend la fin. Le drapeau « en cours » doit être
    /// levé par l'appelant ([`Self::lancer`]) ou l'est ici. L'affinage
    /// MusicBrainz n'en fait PAS partie.
    pub async fn synchroniser_maintenant(&self, nom: &str) -> Result<(), &'static str> {
        self.en_cours.store(true, Ordering::SeqCst);
        let r = self.synchroniser(nom).await;
        {
            let mut m = verrou(&self.memoire);
            match r {
                Ok(()) => {
                    m.derniere_synchro = Some(maintenant());
                    m.derniere_erreur = None;
                }
                Err(c) => m.derniere_erreur = Some(c.to_string()),
            }
        }
        let _ = self.ecrire();
        *verrou(&self.avancement) = Avancement::default();
        self.en_cours.store(false, Ordering::SeqCst);
        match r {
            Ok(()) => tracing::info!(target: "plugin", "collection_physique_synchro_finie"),
            Err(c) => {
                tracing::info!(target: "plugin", code = c, "collection_physique_synchro_arretee")
            }
        }
        r
    }

    fn intervalle_discogs(&self) -> Duration {
        match self.hote.jeton_discogs() {
            Jeton::Aucun => self.cadence.discogs_sans_jeton,
            Jeton::Integre | Jeton::Personnel => self.cadence.discogs_avec_jeton,
        }
    }

    /// Un appel Discogs, espacé, avec les nouveaux essais d'un 429.
    async fn discogs(
        &self,
        chemin: &str,
        params: &[(&str, String)],
    ) -> Result<Reponse, &'static str> {
        let mut essais = 0;
        loop {
            attendre(self.intervalle_discogs()).await;
            match self.hote.discogs(chemin, params).await {
                Ok(Reponse::Limite) => {
                    essais += 1;
                    if essais > self.cadence.essais_limite {
                        return Err(code::DISCOGS_LIMITE);
                    }
                    attendre(self.cadence.attente_limite).await;
                }
                Ok(r) => return Ok(r),
                Err(Panne) => return Err(code::DISCOGS_INDISPONIBLE),
            }
        }
    }

    /// Le nom du compte du jeton PERSONNEL (`GET /oauth/identity`).
    pub async fn identite(&self) -> Result<String, &'static str> {
        if self.hote.jeton_discogs() != Jeton::Personnel {
            return Err(code::JETON_PERSONNEL_ABSENT);
        }
        match self.discogs("/oauth/identity", &[]).await? {
            Reponse::Json(v) => discogs::nom_de_l_identite(&v).ok_or(code::DISCOGS_INDISPONIBLE),
            Reponse::Refusee => Err(code::JETON_REFUSE),
            Reponse::Introuvable | Reponse::Limite => Err(code::DISCOGS_INDISPONIBLE),
        }
    }

    async fn synchroniser(&self, nom: &str) -> Result<(), &'static str> {
        if !discogs::nom_utilisateur_valide(nom) {
            return Err(code::AUCUN_COMPTE);
        }
        self.lire_la_liste(nom).await?;
        self.chercher_en_streaming().await?;
        self.rapprocher_de_la_bibliotheque().await?;
        Ok(())
    }

    /// Étape 1 : la liste, triée par date d'ajout, la plus récente d'abord.
    ///
    /// **Relecture partielle** (cache du 01/11/2026) : quand la liste rangée
    /// est du même compte, de la forme courante et relue en entier il y a
    /// moins de [`crate::memoire::RELIRE_TOUTE_LA_LISTE_JOURS`] jours, la
    /// lecture s'arrête à la première page qui contient un exemplaire déjà
    /// connu (même `instance_id`, même `date_added`) : ce qui est au-dessus
    /// est nouveau, ce qui est en dessous est déjà rangé. La fusion n'est
    /// retenue que si elle compte EXACTEMENT les exemplaires que Discogs
    /// annonce (`pagination.items`) ; sinon un disque est sorti de la
    /// collection (vendu, retiré), et la lecture continue jusqu'au bout.
    /// Pour 908 disques : une page au lieu de dix.
    async fn lire_la_liste(&self, nom: &str) -> Result<(), &'static str> {
        self.etape(Etape::Liste, 0);
        let chemin = format!(
            "/users/{}/collection/folders/0/releases",
            urlencoding::encode(nom)
        );
        let debut = maintenant();
        let mut connus: Option<Vec<Disque>> = {
            let m = verrou(&self.memoire);
            m.liste_partielle_possible(nom, debut)
                .then(|| m.disques.clone())
        };
        let mut disques: Vec<Disque> = Vec::new();
        let mut fusion: Option<Vec<Disque>> = None;
        let mut tronquee = false;
        let mut page = 1u32;
        loop {
            let params = [
                ("page", page.to_string()),
                ("per_page", PAR_PAGE.to_string()),
                ("sort", "added".to_string()),
                ("sort_order", "desc".to_string()),
            ];
            let corps = match self.discogs(&chemin, &params).await? {
                Reponse::Json(v) => v,
                Reponse::Introuvable => return Err(code::UTILISATEUR_INCONNU),
                Reponse::Refusee => return Err(code::COLLECTION_PRIVEE),
                Reponse::Limite => return Err(code::DISCOGS_LIMITE),
            };
            let (lus, pages) = discogs::page_de_collection(&corps);
            let vide = lus.is_empty();
            let touche = connus.as_ref().is_some_and(|c| {
                lus.iter().any(|d| {
                    c.iter()
                        .any(|k| k.instance_id == d.instance_id && k.ajoute_le == d.ajoute_le)
                })
            });
            disques.extend(lus);
            {
                let mut a = verrou(&self.avancement);
                a.fait = page as usize;
                a.total = pages.unwrap_or(page) as usize;
            }
            if touche
                && let Some(c) = connus.take()
                && let Some(annonces) = discogs::exemplaires_annonces(&corps)
            {
                let lus: std::collections::HashSet<u64> =
                    disques.iter().map(|d| d.instance_id).collect();
                let mut f = disques.clone();
                f.extend(c.into_iter().filter(|d| !lus.contains(&d.instance_id)));
                if f.len() == annonces {
                    fusion = Some(f);
                    break;
                }
                tracing::info!(
                    target: "plugin",
                    annonces,
                    fusion = f.len(),
                    "collection_physique_liste_relue_en_entier"
                );
            }
            let pages = pages.unwrap_or(page);
            if vide || page >= pages {
                break;
            }
            if page >= PAGES_MAX {
                tronquee = true;
                break;
            }
            page += 1;
        }
        let partielle = fusion.is_some();
        tracing::info!(target: "plugin", partielle, pages = page, "collection_physique_liste_lue");
        {
            let mut m = verrou(&self.memoire);
            m.changer_de_compte(Some(nom.to_string()));
            match fusion {
                Some(f) => m.disques = f,
                None => {
                    m.disques = disques;
                    m.tronquee = tronquee;
                    m.liste_complete_le = Some(debut);
                }
            }
            m.version_liste = crate::memoire::VERSION_LISTE;
            m.liste_lue_le = Some(maintenant());
        }
        self.ecrire()
    }

    /// Les releases de la liste, sans doublon, dans l'ordre de la liste.
    pub(crate) fn releases(m: &Memoire) -> Vec<Disque> {
        let mut vus = std::collections::HashSet::new();
        m.disques
            .iter()
            .filter(|d| vus.insert(d.release_id))
            .cloned()
            .collect()
    }

    /// Ceux de [`SERVICES`] qui sont connectés, dans l'ordre de [`SERVICES`].
    async fn services(&self) -> Vec<String> {
        let connectes = self.hote.services_connectes().await;
        SERVICES
            .iter()
            .filter(|s| connectes.iter().any(|c| c == *s))
            .map(|s| s.to_string())
            .collect()
    }

    /// La fiche Discogs d'un disque (ses codes-barres), lue une fois.
    async fn lire_la_fiche(&self, release: u64) -> Result<Vec<String>, &'static str> {
        if let Some(f) = verrou(&self.memoire).fiches.get(&release) {
            return Ok(f.codes_barres.clone());
        }
        let codes = match self.discogs(&format!("/releases/{release}"), &[]).await? {
            Reponse::Json(v) => Some(discogs::codes_barres_de_la_fiche(&v)),
            // Fiche retirée de Discogs : rien à apprendre, ne plus la demander.
            Reponse::Introuvable => Some(Vec::new()),
            // Refus passager : revue à la synchronisation suivante.
            Reponse::Refusee | Reponse::Limite => None,
        };
        if let Some(codes_barres) = &codes {
            verrou(&self.memoire).fiches.insert(
                release,
                Fiche {
                    codes_barres: codes_barres.clone(),
                    lue_le: maintenant(),
                },
            );
        }
        Ok(codes.unwrap_or_default())
    }

    /// Étape 2 : disque par disque, sa fiche Discogs si besoin, puis les
    /// services. MusicBrainz n'est PAS attendu : un disque reconnu dans la
    /// bibliothèque par son code-barre ou par artiste + titre (ou par des
    /// liens MusicBrainz déjà appris) n'est pas cherché ; les autres le sont.
    async fn chercher_en_streaming(&self) -> Result<(), &'static str> {
        let services = self.services().await;
        let albums = self
            .hote
            .albums_locaux()
            .await
            .map_err(|_| code::BIBLIOTHEQUE_ILLISIBLE)?;
        let index = Index::new(&albums);
        let t = maintenant();
        let a_voir: Vec<Disque> = {
            let m = verrou(&self.memoire);
            Self::releases(&m)
                .into_iter()
                .filter(|d| {
                    !m.fiches.contains_key(&d.release_id)
                        || (!services.is_empty()
                            && m.streaming_a_revoir(d.release_id, &services, t))
                })
                .collect()
        };
        self.etape(Etape::Streaming, a_voir.len());
        for (n, d) in a_voir.into_iter().enumerate() {
            let codes = match self.lire_la_fiche(d.release_id).await {
                Ok(c) => c,
                Err(c) => {
                    let _ = self.ecrire();
                    return Err(c);
                }
            };
            let a_chercher = {
                let m = verrou(&self.memoire);
                let mb = m.liens_mb.get(&d.release_id).map(|l| &l.liens);
                !services.is_empty()
                    && m.streaming_a_revoir(d.release_id, &services, t)
                    && index.trouver(&d, &codes, mb).is_none()
            };
            if a_chercher {
                let mut trouves = Vec::new();
                let mut ont_repondu = Vec::new();
                for service in &services {
                    match self.chercher_sur(service, &d, &codes).await {
                        SurLeService::Trouve(s) => {
                            trouves.push(s);
                            ont_repondu.push(service.clone());
                        }
                        SurLeService::Absent => ont_repondu.push(service.clone()),
                        SurLeService::SansReponse => {}
                    }
                }
                verrou(&self.memoire).streaming.insert(
                    d.release_id,
                    Streaming {
                        trouves,
                        services: ont_repondu,
                        lu_le: maintenant(),
                    },
                );
            }
            self.un_de_plus();
            if (n + 1) % ECRIRE_TOUS_LES == 0 {
                self.ecrire()?;
            }
        }
        self.ecrire()
    }

    /// Étape 3 : le rapprochement avec la bibliothèque. Il se calcule à
    /// chaque lecture de la collection (la bibliothèque change) ; l'étape
    /// relit les albums locaux et compte les disques reconnus, pour le dire.
    async fn rapprocher_de_la_bibliotheque(&self) -> Result<(), &'static str> {
        let albums = self
            .hote
            .albums_locaux()
            .await
            .map_err(|_| code::BIBLIOTHEQUE_ILLISIBLE)?;
        let index = Index::new(&albums);
        let m = self.memoire();
        let disques = Self::releases(&m);
        self.etape(Etape::Bibliotheque, disques.len());
        let mut reconnus = 0usize;
        for d in &disques {
            let codes = m
                .fiches
                .get(&d.release_id)
                .map(|f| f.codes_barres.as_slice())
                .unwrap_or_default();
            let mb = m.liens_mb.get(&d.release_id).map(|l| &l.liens);
            if index.trouver(d, codes, mb).is_some() {
                reconnus += 1;
            }
            self.un_de_plus();
        }
        tracing::info!(target: "plugin", reconnus, disques = disques.len(), "collection_physique_bibliotheque");
        Ok(())
    }

    /// Étape 4, à part : les liens MusicBrainz de chaque disque qui n'en a
    /// pas encore. Un refus (429 / 503) ou un silence fait reculer :
    /// [`Cadence::attente_limite`], doublée à chaque échec de suite jusqu'à
    /// [`Cadence::attente_max`], et le MÊME disque est redemandé. Aucun
    /// échec n'est écrit comme erreur. Ce qui est appris est rangé tous les
    /// [`ECRIRE_TOUS_LES`] disques et avant chaque recul : après un
    /// redémarrage, l'affinage reprend aux disques qui restent.
    async fn affiner(&self) {
        loop {
            let t = maintenant();
            let a_lire: Vec<Disque> = {
                let m = verrou(&self.memoire);
                Self::releases(&m)
                    .into_iter()
                    .filter(|d| m.liens_a_revoir(d.release_id, t))
                    .collect()
            };
            if a_lire.is_empty() {
                let _ = self.ecrire();
                return;
            }
            tracing::info!(target: "plugin", restants = a_lire.len(), "collection_physique_affinage");
            let mut groupes_du_master: HashMap<u64, Vec<String>> = HashMap::new();
            let mut lus = 0usize;
            for d in a_lire {
                let mut recul = self.cadence.attente_limite;
                let liens = loop {
                    attendre(self.cadence.musicbrainz).await;
                    match self.liens_du_disque(&d, &mut groupes_du_master).await {
                        Ok(liens) => break liens,
                        Err(Panne) => {
                            let _ = self.ecrire();
                            tracing::debug!(
                                target: "plugin",
                                recul_s = recul.as_secs(),
                                "collection_physique_musicbrainz_recul"
                            );
                            // Rend la main même sans attente (essais) :
                            // la tâche reste interruptible.
                            tokio::task::yield_now().await;
                            attendre(recul).await;
                            recul = (recul * 2).min(self.cadence.attente_max);
                        }
                    }
                };
                verrou(&self.memoire).liens_mb.insert(
                    d.release_id,
                    Liens {
                        liens,
                        lus_le: maintenant(),
                    },
                );
                lus += 1;
                if lus.is_multiple_of(ECRIRE_TOUS_LES) {
                    let _ = self.ecrire();
                }
            }
            let _ = self.ecrire();
        }
    }

    /// Un appel MusicBrainz, une fois : un refus (429 / 503) ou un silence
    /// est une [`Panne`], que l'affinage traite par le recul.
    async fn musicbrainz(&self, chemin: &str, params: &[(&str, String)]) -> Result<Reponse, Panne> {
        match self.hote.musicbrainz(chemin, params).await {
            Ok(Reponse::Limite) | Err(Panne) => Err(Panne),
            Ok(r) => Ok(r),
        }
    }

    /// Les identifiants MusicBrainz reliés à une adresse Discogs.
    async fn relies(&self, adresse: String, inc: &str, cle: &str) -> Result<Vec<String>, Panne> {
        let params = [("resource", adresse), ("inc", inc.to_string())];
        match self.musicbrainz("url", &params).await? {
            Reponse::Json(v) => Ok(identifiants_relies(&v, cle)),
            Reponse::Introuvable | Reponse::Refusee | Reponse::Limite => Ok(Vec::new()),
        }
    }

    /// Les liens d'un disque : sa release, puis son master (une fois par
    /// master et par passage).
    async fn liens_du_disque(
        &self,
        d: &Disque,
        groupes_du_master: &mut HashMap<u64, Vec<String>>,
    ) -> Result<LiensMusicBrainz, Panne> {
        let releases = self
            .relies(
                format!("https://www.discogs.com/release/{}", d.release_id),
                "release-rels",
                "release",
            )
            .await?;
        let groupes = match d.master_id {
            Some(m) => match groupes_du_master.get(&m) {
                Some(g) => g.clone(),
                None => {
                    attendre(self.cadence.musicbrainz).await;
                    let g = self
                        .relies(
                            format!("https://www.discogs.com/master/{m}"),
                            "release-group-rels",
                            "release_group",
                        )
                        .await?;
                    groupes_du_master.insert(m, g.clone());
                    g
                }
            },
            None => Vec::new(),
        };
        Ok(LiensMusicBrainz { releases, groupes })
    }

    /// Le disque chez un service : par code-barre (UPC / EAN de la fiche
    /// Discogs) d'abord, puis par artiste + titre.
    async fn chercher_sur(&self, service: &str, d: &Disque, codes: &[String]) -> SurLeService {
        let mut requetes: Vec<(String, bool)> = codes.iter().map(|c| (c.clone(), true)).collect();
        requetes.push((format!("{} {}", d.artiste, d.titre), false));
        let mut sans_reponse = false;
        for (requete, par_code) in requetes {
            attendre(self.cadence.service).await;
            let Ok(candidats) = self.hote.chercher_albums(service, &requete).await else {
                tracing::debug!(target: "plugin", service, "collection_physique_service_sans_reponse");
                sans_reponse = true;
                continue;
            };
            if let Some(a) = choisir_sur_le_service(d, &candidats, par_code) {
                return SurLeService::Trouve(SurUnService {
                    service: service.to_string(),
                    id: a.id.clone(),
                    titre: a.titre.clone(),
                    artiste: a.artiste.clone(),
                    par: if par_code {
                        "code_barre"
                    } else {
                        "artiste_titre"
                    }
                    .to_string(),
                });
            }
        }
        if sans_reponse {
            SurLeService::SansReponse
        } else {
            SurLeService::Absent
        }
    }
}

/// Les identifiants des relations `cle` (`release`, `release_group`) d'une
/// réponse `/ws/2/url`. MusicBrainz écrit `release_group` avec un souligné
/// dans ces relations (réponse enregistrée le 30/10/2026,
/// `tests/fixtures/mb_url_master_96559.json`) ; la forme à tiret est lue
/// aussi.
pub fn identifiants_relies(v: &Value, cle: &str) -> Vec<String> {
    let tiret = cle.replace('_', "-");
    let mut ids = Vec::new();
    for r in v["relations"].as_array().into_iter().flatten() {
        let objet = if r[cle].is_object() {
            &r[cle]
        } else {
            &r[tiret.as_str()]
        };
        if let Some(id) = objet["id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let id = id.to_ascii_lowercase();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}
