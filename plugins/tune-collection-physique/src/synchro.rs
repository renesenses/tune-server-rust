//! La synchronisation en tâche de fond : lire la collection, puis apprendre
//! de chaque disque ce qui ne se lit qu'un disque à la fois.
//!
//! # Les quatre étapes
//!
//! 1. **Liste** — toutes les pages de la collection (100 disques par
//!    page). La liste remplace l'ancienne : un disque vendu en sort.
//! 2. **Codes-barres** — la fiche Discogs de chaque disque qu'on ne connaît
//!    pas encore : c'est l'étape INCRÉMENTALE, la seule qui coûte un appel
//!    par disque, et elle n'est faite qu'une fois par disque.
//! 3. **MusicBrainz** — les identifiants reliés à la release et au master
//!    Discogs (`/ws/2/url?resource=…`), une fois par disque (et par master).
//! 4. **Streaming** — Qobuz et TIDAL, pour les disques qui ne sont PAS dans
//!    la bibliothèque, par code-barre puis par artiste + titre.
//!
//! Une panne arrête la synchronisation et laisse ce qui a été appris : la
//! suivante reprend où celle-ci s'est arrêtée. La mémoire est écrite tous
//! les [`ECRIRE_TOUS_LES`] disques.
//!
//! # Débit
//!
//! Discogs autorise 60 requêtes par minute avec un jeton, 25 sans. L'hôte
//! fait passer chaque appel par le limiteur Discogs PARTAGÉ du serveur
//! (`tune_core::http::fetch::DISCOGS`, ralenti sur un 429) ; le greffon
//! espace EN PLUS ses propres appels ([`Cadence`]) pour tenir sous le
//! plafond du régime sans jeton et laisser de la place aux autres passes.
//! MusicBrainz : le limiteur partagé (1 par seconde). Les services : un
//! appel toutes les [`Cadence::service`].
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

/// Après autant de disques DE SUITE sans réponse de MusicBrainz, l'étape
/// s'arrête pour cette synchronisation : les disques restants sont comptés
/// « sans identifiant » et revus à la suivante. Sans ce disjoncteur, un
/// MusicBrainz tombé coûterait `essais × attente` par disque (908 disques
/// sur le .18 : des heures), et l'étape des services n'arriverait jamais.
pub const MUSICBRAINZ_ECHECS_DE_SUITE_MAX: usize = 3;

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
}

/// L'espacement des appels du greffon.
#[derive(Debug, Clone)]
pub struct Cadence {
    pub discogs_avec_jeton: Duration,
    pub discogs_sans_jeton: Duration,
    pub service: Duration,
    pub attente_limite: Duration,
    pub essais_limite: u32,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            // 1,1 s : sous 60 par minute avec la marge de MetaRust.
            discogs_avec_jeton: Duration::from_millis(1_100),
            // 2,5 s : 24 par minute, sous les 25 du régime sans jeton.
            discogs_sans_jeton: Duration::from_millis(2_500),
            service: Duration::from_millis(1_500),
            attente_limite: Duration::from_secs(60),
            essais_limite: 3,
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
            attente_limite: Duration::ZERO,
            essais_limite: 3,
        }
    }
}

/// L'étape en cours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Etape {
    Liste,
    CodesBarres,
    Musicbrainz,
    Streaming,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Avancement {
    pub en_cours: bool,
    pub etape: Option<Etape>,
    pub fait: usize,
    pub total: usize,
}

/// Les codes d'erreur d'une synchronisation : stables, traduits par l'écran.
pub mod code {
    pub const AUCUN_COMPTE: &str = "aucun_compte";
    pub const UTILISATEUR_INCONNU: &str = "utilisateur_inconnu";
    pub const COLLECTION_PRIVEE: &str = "collection_privee";
    pub const DISCOGS_LIMITE: &str = "discogs_limite";
    pub const DISCOGS_INDISPONIBLE: &str = "discogs_indisponible";
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

async fn attendre(d: Duration) {
    if !d.is_zero() {
        tokio::time::sleep(d).await;
    }
}

pub struct Moteur {
    pub(crate) hote: Arc<dyn Hote>,
    dossier: PathBuf,
    pub(crate) memoire: Mutex<Memoire>,
    avancement: Mutex<Avancement>,
    en_cours: AtomicBool,
    cadence: Cadence,
}

fn verrou<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Moteur {
    pub fn new(hote: Arc<dyn Hote>, dossier: PathBuf, cadence: Cadence) -> Self {
        let memoire = Memoire::lire(&dossier);
        Self {
            hote,
            dossier,
            memoire: Mutex::new(memoire),
            avancement: Mutex::new(Avancement::default()),
            en_cours: AtomicBool::new(false),
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

    /// Lance une synchronisation de `nom` en tâche de fond. `false` si une
    /// synchronisation tourne déjà.
    pub fn lancer(self: &Arc<Self>, nom: String) -> bool {
        if self.en_cours.swap(true, Ordering::SeqCst) {
            return false;
        }
        let moteur = self.clone();
        tokio::spawn(async move {
            let _ = moteur.synchroniser_maintenant(&nom).await;
        });
        true
    }

    /// Synchronise `nom` et attend la fin. Le drapeau « en cours » doit être
    /// levé par l'appelant ([`Self::lancer`]) ou l'est ici.
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

    async fn musicbrainz(
        &self,
        chemin: &str,
        params: &[(&str, String)],
    ) -> Result<Reponse, &'static str> {
        // Un refus (429 / 503) comme une panne de transport se réessaient :
        // le limiteur partagé de l'hôte espace déjà chaque appel, et ralentit
        // pour tout le serveur après un refus (essai réel du 31/10/2026 :
        // l'enrichissement interrogeait MusicBrainz en même temps).
        let mut essais = 0;
        loop {
            match self.hote.musicbrainz(chemin, params).await {
                Ok(Reponse::Limite) | Err(Panne) => {
                    essais += 1;
                    if essais > self.cadence.essais_limite {
                        return Err(code::MUSICBRAINZ_INDISPONIBLE);
                    }
                    attendre(self.cadence.attente_limite).await;
                }
                Ok(r) => return Ok(r),
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
        self.lire_les_codes_barres().await?;
        self.lire_les_liens_musicbrainz().await?;
        self.chercher_en_streaming().await?;
        Ok(())
    }

    async fn lire_la_liste(&self, nom: &str) -> Result<(), &'static str> {
        self.etape(Etape::Liste, 0);
        let chemin = format!(
            "/users/{}/collection/folders/0/releases",
            urlencoding::encode(nom)
        );
        let mut disques: Vec<Disque> = Vec::new();
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
            disques.extend(lus);
            {
                let mut a = verrou(&self.avancement);
                a.fait = page as usize;
                a.total = pages.unwrap_or(page) as usize;
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
        {
            let mut m = verrou(&self.memoire);
            m.changer_de_compte(Some(nom.to_string()));
            m.disques = disques;
            m.tronquee = tronquee;
            m.liste_lue_le = Some(maintenant());
        }
        self.ecrire()
    }

    /// Les releases de la liste, sans doublon, dans l'ordre de la liste.
    fn releases(m: &Memoire) -> Vec<Disque> {
        let mut vus = std::collections::HashSet::new();
        m.disques
            .iter()
            .filter(|d| vus.insert(d.release_id))
            .cloned()
            .collect()
    }

    async fn lire_les_codes_barres(&self) -> Result<(), &'static str> {
        let a_lire: Vec<u64> = {
            let m = verrou(&self.memoire);
            Self::releases(&m)
                .into_iter()
                .map(|d| d.release_id)
                .filter(|r| !m.fiches.contains_key(r))
                .collect()
        };
        self.etape(Etape::CodesBarres, a_lire.len());
        for (n, release) in a_lire.into_iter().enumerate() {
            let fiche = match self.discogs(&format!("/releases/{release}"), &[]).await {
                Ok(Reponse::Json(v)) => Some(discogs::codes_barres_de_la_fiche(&v)),
                // Fiche retirée de Discogs : rien à apprendre, ne plus la demander.
                Ok(Reponse::Introuvable) => Some(Vec::new()),
                Ok(Reponse::Refusee) | Ok(Reponse::Limite) => None,
                Err(c) => {
                    let _ = self.ecrire();
                    return Err(c);
                }
            };
            if let Some(codes_barres) = fiche {
                verrou(&self.memoire).fiches.insert(
                    release,
                    Fiche {
                        codes_barres,
                        lue_le: maintenant(),
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

    /// Les identifiants MusicBrainz reliés à une adresse Discogs.
    async fn relies(
        &self,
        adresse: String,
        inc: &str,
        cle: &str,
    ) -> Result<Vec<String>, &'static str> {
        let params = [("resource", adresse), ("inc", inc.to_string())];
        match self.musicbrainz("url", &params).await? {
            Reponse::Json(v) => Ok(identifiants_relies(&v, cle)),
            Reponse::Introuvable | Reponse::Refusee | Reponse::Limite => Ok(Vec::new()),
        }
    }

    /// Les identifiants MusicBrainz de chaque disque qu'on ne connaît pas.
    ///
    /// Ils sont un PLUS (la correspondance retombe sur le code-barre, puis
    /// sur artiste + titre) : MusicBrainz qui refuse ou ne répond pas
    /// n'arrête JAMAIS la synchronisation (essai réel du 31/10/2026 : 908
    /// disques, synchronisation arrêtée sur `musicbrainz_indisponible`, les
    /// services jamais interrogés). Un disque sans réponse n'est pas noté :
    /// il est revu à la synchronisation suivante, et ce qui a été appris est
    /// gardé. Le nombre de disques restés sans réponse est retenu
    /// (`Memoire::musicbrainz_manquants`) et l'écran le dit.
    async fn lire_les_liens_musicbrainz(&self) -> Result<(), &'static str> {
        let t = maintenant();
        let a_lire: Vec<Disque> = {
            let m = verrou(&self.memoire);
            Self::releases(&m)
                .into_iter()
                .filter(|d| m.liens_a_revoir(d.release_id, t))
                .collect()
        };
        self.etape(Etape::Musicbrainz, a_lire.len());
        let total = a_lire.len();
        let mut groupes_du_master: HashMap<u64, Vec<String>> = HashMap::new();
        let mut manquants = 0usize;
        let mut echecs_de_suite = 0usize;
        for (n, d) in a_lire.into_iter().enumerate() {
            if echecs_de_suite >= MUSICBRAINZ_ECHECS_DE_SUITE_MAX {
                // Disjoncté : le reste attendra la synchronisation suivante.
                manquants += total - n;
                tracing::info!(
                    target: "plugin",
                    restants = total - n,
                    "collection_physique_musicbrainz_disjoncte"
                );
                break;
            }
            match self.liens_du_disque(&d, &mut groupes_du_master).await {
                Ok(liens) => {
                    echecs_de_suite = 0;
                    verrou(&self.memoire).liens_mb.insert(
                        d.release_id,
                        Liens {
                            liens,
                            lus_le: maintenant(),
                        },
                    );
                }
                Err(_) => {
                    echecs_de_suite += 1;
                    manquants += 1;
                }
            }
            self.un_de_plus();
            if (n + 1) % ECRIRE_TOUS_LES == 0 {
                self.ecrire()?;
            }
        }
        if manquants > 0 {
            tracing::info!(target: "plugin", manquants, "collection_physique_musicbrainz_partiel");
        }
        verrou(&self.memoire).musicbrainz_manquants = manquants;
        self.ecrire()
    }

    /// Les liens d'un disque : sa release, puis son master (une fois par
    /// master et par synchronisation).
    async fn liens_du_disque(
        &self,
        d: &Disque,
        groupes_du_master: &mut HashMap<u64, Vec<String>>,
    ) -> Result<LiensMusicBrainz, &'static str> {
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

    async fn chercher_en_streaming(&self) -> Result<(), &'static str> {
        let services = self.hote.services_connectes().await;
        let services: Vec<String> = SERVICES
            .iter()
            .filter(|s| services.iter().any(|c| c == *s))
            .map(|s| s.to_string())
            .collect();
        if services.is_empty() {
            self.etape(Etape::Streaming, 0);
            return Ok(());
        }
        let albums = self
            .hote
            .albums_locaux()
            .await
            .map_err(|_| code::BIBLIOTHEQUE_ILLISIBLE)?;
        let index = Index::new(&albums);
        let t = maintenant();
        let a_chercher: Vec<(Disque, Vec<String>)> = {
            let m = verrou(&self.memoire);
            Self::releases(&m)
                .into_iter()
                .filter(|d| m.streaming_a_revoir(d.release_id, &services, t))
                .map(|d| {
                    let codes = m
                        .fiches
                        .get(&d.release_id)
                        .map(|f| f.codes_barres.clone())
                        .unwrap_or_default();
                    (d, codes)
                })
                .filter(|(d, codes)| {
                    let mb = m.liens_mb.get(&d.release_id).map(|l| &l.liens);
                    index.trouver(d, codes, mb).is_none()
                })
                .collect()
        };
        self.etape(Etape::Streaming, a_chercher.len());
        for (n, (d, codes)) in a_chercher.into_iter().enumerate() {
            let mut trouves = Vec::new();
            for service in &services {
                if let Some(s) = self.chercher_sur(service, &d, &codes).await {
                    trouves.push(s);
                }
            }
            verrou(&self.memoire).streaming.insert(
                d.release_id,
                Streaming {
                    trouves,
                    services: services.clone(),
                    lu_le: maintenant(),
                },
            );
            self.un_de_plus();
            if (n + 1) % ECRIRE_TOUS_LES == 0 {
                self.ecrire()?;
            }
        }
        self.ecrire()
    }

    /// Le disque chez un service : par code-barre d'abord, puis par
    /// artiste + titre. Une panne du service vaut « pas trouvé » pour ce
    /// passage ; le disque sera revu dans trente jours.
    async fn chercher_sur(
        &self,
        service: &str,
        d: &Disque,
        codes: &[String],
    ) -> Option<SurUnService> {
        let mut requetes: Vec<(String, bool)> = codes.iter().map(|c| (c.clone(), true)).collect();
        requetes.push((format!("{} {}", d.artiste, d.titre), false));
        for (requete, par_code) in requetes {
            attendre(self.cadence.service).await;
            let Ok(candidats) = self.hote.chercher_albums(service, &requete).await else {
                tracing::debug!(target: "plugin", service, "collection_physique_service_sans_reponse");
                continue;
            };
            if let Some(a) = choisir_sur_le_service(d, &candidats, par_code) {
                return Some(SurUnService {
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
        None
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
