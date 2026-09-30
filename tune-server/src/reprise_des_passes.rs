//! Reprise des passes d'enrichissement coupées par un arrêt du serveur (#5469).
//!
//! Les trois passes « métadonnées » (`POST /library/enrich-all`), « pochettes
//! d'albums » (`POST /library/artwork/enrich`) et « images d'artistes »
//! (`POST /library/artwork/enrich-artists`) vivent dans un `tokio::spawn`. Elles
//! tiennent leur liste en mémoire, et un arrêt du serveur les tue. Avant ce
//! module, rien ne les relançait : au redémarrage, la pause était restaurée
//! (`taches_de_fond::hydrater`), mais « Reprendre » ne faisait que lever un
//! drapeau sur une passe qui n'existait plus. La carte retombait « Au repos »
//! et rien ne repartait (Tades, fil 2042).
//!
//! ## Le repère
//!
//! Une passe ouverte écrit un repère en base (`settings`, sous
//! [`Passe::cle_reglage`]) : sa portée, un jeton et le nombre de relances
//! automatiques déjà faites. Elle l'efface à sa fin NORMALE, et seulement là.
//! Un arrêt, un `kill` ou une panique le laissent en place, et c'est
//! exactement ce qui le rend utile : un repère présent au démarrage désigne une
//! passe morte avant d'avoir fini.
//!
//! Il n'est pas effacé dans un `Drop` : l'arrêt du runtime détruit les tâches
//! en cours, et un `Drop` effacerait le repère au moment précis où il doit
//! survivre.
//!
//! ## Deux déclencheurs, un seul chemin
//!
//! * **Au démarrage** ([`spawn`]) : chaque passe dont le repère est présent est
//!   relancée, avec la même portée. Une pause restaurée n'y change rien : la
//!   passe relancée se gare à sa première frontière (`attendre_son_tour`), et
//!   « Reprendre » la trouve vivante.
//! * **« Reprendre »** ([`relancer_pour`]) : quand la passe n'existe plus, le
//!   bouton en relance une au lieu de seulement lever le drapeau.
//!
//! Les deux passent par [`relancer_si_absente`], qui ne relance JAMAIS une
//! passe encore vivante (registre `background_tasks`).
//!
//! ## Le quota gratuit ne s'applique pas à une reprise
//!
//! `gate_enrichment` compte des GESTES : un clic consomme un des dix gestes du
//! jour, quelle que soit la taille de la passe qu'il lance. La reprise n'est
//! pas un geste nouveau. Elle continue une passe dont le geste a déjà été
//! compté. La refuser à un compte gratuit dont le quota du jour est épuisé
//! laisserait la passe morte pour la journée, pour un clic qu'il a déjà payé.
//! La reprise ne consulte donc pas le quota et ne l'entame pas. Pour que ce
//! passe-droit ne tourne pas en boucle, un plafond l'accompagne :
//! [`RELANCES_MAX`] relances automatiques consécutives. Au-delà, le démarrage
//! renonce et le journalise (`reprise_passe_abandonnee`). « Reprendre », qui
//! est un geste de l'utilisateur, reste possible et remet le compteur à zéro.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::taches_de_fond::Tache;

use crate::state::AppState;

/// Relances automatiques consécutives au démarrage, au-delà desquelles on
/// renonce. Une passe qui meurt à chaque démarrage, par une panique ou un
/// arrêt trop précoce, ne doit pas relancer MusicBrainz indéfiniment.
pub const RELANCES_MAX: u32 = 3;

/// Délai entre le démarrage et la reprise. Le scan de démarrage et les passes
/// qui décodent partent dans la même minute ; les passes d'enrichissement sont
/// des requêtes réseau, elles peuvent attendre que le serveur soit posé.
pub const DELAI_AVANT_REPRISE: Duration = Duration::from_secs(60);

/// Une passe d'enrichissement qui sait se relancer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passe {
    /// `POST /library/enrich-all` : les métadonnées MusicBrainz.
    Metadonnees,
    /// `POST /library/artwork/enrich` : les pochettes d'albums manquantes.
    PochettesAlbums,
    /// `POST /library/artwork/enrich-artists` : les images d'artistes
    /// manquantes. La variante « forcée » n'est pas reprise : elle refait TOUS
    /// les artistes depuis le début, et une reprise la recommencerait à zéro.
    ImagesArtistes,
}

impl Passe {
    /// Toutes les passes reprenables.
    pub const TOUTES: [Passe; 3] = [
        Passe::Metadonnees,
        Passe::PochettesAlbums,
        Passe::ImagesArtistes,
    ];

    /// L'identifiant sous lequel la passe s'inscrit au registre
    /// `background_tasks` le temps qu'elle vit.
    pub fn id(self) -> &'static str {
        match self {
            Passe::Metadonnees => "enrich_all",
            Passe::PochettesAlbums => "artwork",
            Passe::ImagesArtistes => "artist_artwork",
        }
    }

    /// La clé `settings` du repère.
    pub fn cle_reglage(self) -> &'static str {
        match self {
            Passe::Metadonnees => "reprise_passe_enrich_all",
            Passe::PochettesAlbums => "reprise_passe_artwork",
            Passe::ImagesArtistes => "reprise_passe_artist_artwork",
        }
    }

    /// Le traitement suspendable qui porte la passe : c'est sa carte sur
    /// l'écran « État du serveur », et son bouton « Reprendre ».
    pub fn tache(self) -> Tache {
        match self {
            Passe::Metadonnees => Tache::Enrichissement,
            Passe::PochettesAlbums | Passe::ImagesArtistes => Tache::ImagesArtistes,
        }
    }
}

/// Qui lance la passe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declencheur {
    /// Un clic de l'utilisateur : garde du quota, compteur de relances à zéro.
    Geste,
    /// Une reprise : ni garde ni consommation du quota (voir l'en-tête du
    /// module). `relances` est écrit tel quel dans le repère.
    Reprise { relances: u32 },
}

impl Declencheur {
    /// La garde du quota s'applique-t-elle ?
    pub fn garde_le_quota(self) -> bool {
        matches!(self, Declencheur::Geste)
    }

    fn relances(self) -> u32 {
        match self {
            Declencheur::Geste => 0,
            Declencheur::Reprise { relances } => relances,
        }
    }
}

/// Le repère d'une passe ouverte, tel que la base le porte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repere {
    /// Le répertoire auquel la passe était limitée (#1660). `None` = toute la
    /// bibliothèque.
    pub portee: Option<String>,
    /// Identifie la passe qui a écrit le repère : seule elle peut l'effacer.
    pub jeton: String,
    /// Relances automatiques déjà faites depuis le dernier geste.
    pub relances: u32,
}

/// Lire le repère d'une passe. `None` si la passe s'est terminée normalement
/// ou n'a jamais été lancée. Un repère illisible compte comme absent : dans le
/// doute on ne relance pas, faute de savoir sur quelle portée.
pub fn lire(backend: &Arc<dyn DbBackend>, passe: Passe) -> Option<Repere> {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(passe.cle_reglage())
        .ok()
        .flatten()?;
    let v: Value = serde_json::from_str(&brut).ok()?;
    Some(Repere {
        portee: v.get("portee").and_then(Value::as_str).map(str::to_string),
        jeton: v.get("jeton").and_then(Value::as_str)?.to_string(),
        relances: v.get("relances").and_then(Value::as_u64).unwrap_or(0) as u32,
    })
}

/// Poser le repère à l'ouverture d'une passe. Rend le jeton que la passe
/// présentera à [`noter_fin`].
pub(crate) fn noter_ouverture(
    backend: &Arc<dyn DbBackend>,
    passe: Passe,
    portee: Option<&str>,
    declencheur: Declencheur,
) -> String {
    let jeton = uuid::Uuid::new_v4().to_string();
    let corps = json!({
        "portee": portee,
        "jeton": jeton,
        "relances": declencheur.relances(),
    });
    if let Err(e) =
        SettingsRepo::with_backend(backend.clone()).set(passe.cle_reglage(), &corps.to_string())
    {
        // Pas de repère : la passe tourne quand même, elle ne sera simplement
        // pas reprise si le serveur s'arrête avant sa fin.
        warn!(passe = passe.id(), error = %e, "reprise_passe_repere_non_ecrit");
    }
    jeton
}

/// Effacer le repère à la fin NORMALE de la passe, si c'est bien le sien. Un
/// second lancement du même bouton a pu le remplacer. La première passe qui
/// finit ne doit pas effacer le repère de celle qui tourne encore.
pub(crate) fn noter_fin(backend: &Arc<dyn DbBackend>, passe: Passe, jeton: &str) {
    if lire(backend, passe).is_some_and(|r| r.jeton == jeton) {
        effacer(backend, passe);
    }
}

fn effacer(backend: &Arc<dyn DbBackend>, passe: Passe) {
    if let Err(e) = SettingsRepo::with_backend(backend.clone()).delete(passe.cle_reglage()) {
        warn!(passe = passe.id(), error = %e, "reprise_passe_repere_non_efface");
    }
}

/// La passe est-elle vivante dans CE processus ?
pub fn vivante(state: &AppState, passe: Passe) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.id == passe.id())
}

/// Qui demande la reprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origine {
    /// Le démarrage du serveur : borné par [`RELANCES_MAX`].
    Demarrage,
    /// Le bouton « Reprendre » : un geste de l'utilisateur, non borné.
    Reprendre,
}

/// Ce que [`relancer_si_absente`] a fait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relance {
    /// Une passe est repartie.
    Relancee,
    /// La passe tourne déjà : rien à faire.
    DejaVivante,
    /// Aucun repère : la dernière passe a fini normalement, ou n'a jamais été
    /// lancée.
    RienAReprendre,
    /// Plafond de relances automatiques atteint : le démarrage renonce. Le
    /// repère est conservé, « Reprendre » peut encore la relancer.
    Abandonnee { relances: u32 },
    /// La route a refusé (portée devenue invalide, base en panne…). Le repère
    /// est effacé si le refus est définitif.
    Refusee { statut: u16 },
}

/// Un seul appelant à la fois entre le constat « la passe n'est pas vivante »
/// et son inscription au registre. Sans ce verrou, le démarrage et un clic sur
/// « Reprendre » arrivés ensemble lanceraient deux fois la même passe.
static UN_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Relancer une passe si elle a un repère et n'est pas vivante.
pub async fn relancer_si_absente(state: &AppState, passe: Passe, origine: Origine) -> Relance {
    let _un_a_la_fois = UN_A_LA_FOIS.lock().await;
    if vivante(state, passe) {
        return Relance::DejaVivante;
    }
    let Some(repere) = lire(&state.backend, passe) else {
        return Relance::RienAReprendre;
    };
    let relances = match origine {
        Origine::Demarrage => {
            if repere.relances >= RELANCES_MAX {
                warn!(
                    passe = passe.id(),
                    relances = repere.relances,
                    max = RELANCES_MAX,
                    "reprise_passe_abandonnee — la passe est morte à chaque démarrage ; « Reprendre » peut encore la relancer"
                );
                return Relance::Abandonnee {
                    relances: repere.relances,
                };
            }
            repere.relances + 1
        }
        Origine::Reprendre => 0,
    };
    let declencheur = Declencheur::Reprise { relances };
    let reponse = match passe {
        Passe::Metadonnees => {
            crate::routes::library::demarrer_enrich_all(state, repere.portee.clone(), declencheur)
                .await
        }
        Passe::PochettesAlbums => {
            crate::routes::library::demarrer_pochettes_albums(state, declencheur).await
        }
        Passe::ImagesArtistes => {
            crate::routes::library::demarrer_images_artistes(state, declencheur).await
        }
    };
    let statut = reponse.status();
    if statut == StatusCode::ACCEPTED {
        info!(
            passe = passe.id(),
            origine = ?origine,
            relances,
            portee = ?repere.portee,
            "reprise_passe_relancee — hors quota : le geste qui l'a ouverte est déjà compté"
        );
        return Relance::Relancee;
    }
    if statut == StatusCode::OK {
        // « skipped » : plus rien à faire, la passe a fini sans le savoir.
        info!(passe = passe.id(), "reprise_passe_rien_a_faire");
        effacer(&state.backend, passe);
        return Relance::RienAReprendre;
    }
    if statut.is_client_error() {
        // Portée hors des racines musicales, ou dossier devenu invalide. Jamais
        // de repli sur toute la bibliothèque (#1660) : le repère part.
        warn!(
            passe = passe.id(),
            statut = statut.as_u16(),
            portee = ?repere.portee,
            "reprise_passe_refusee_definitivement — repère effacé"
        );
        effacer(&state.backend, passe);
    } else {
        warn!(
            passe = passe.id(),
            statut = statut.as_u16(),
            "reprise_passe_refusee — repère conservé pour le prochain essai"
        );
    }
    Relance::Refusee {
        statut: statut.as_u16(),
    }
}

/// Relancer les passes portées par un traitement. C'est ce qu'appelle
/// « Reprendre ». Rend les identifiants des passes relancées.
pub async fn relancer_pour(state: &AppState, tache: Tache, origine: Origine) -> Vec<&'static str> {
    let mut relancees = Vec::new();
    for passe in Passe::TOUTES.into_iter().filter(|p| p.tache() == tache) {
        if relancer_si_absente(state, passe, origine).await == Relance::Relancee {
            relancees.push(passe.id());
        }
    }
    relancees
}

/// Relancer, au démarrage, toutes les passes que l'arrêt précédent a coupées.
pub async fn relancer_les_passes_interrompues(state: &AppState) -> Vec<&'static str> {
    let mut relancees = Vec::new();
    for passe in Passe::TOUTES {
        if relancer_si_absente(state, passe, Origine::Demarrage).await == Relance::Relancee {
            relancees.push(passe.id());
        }
    }
    relancees
}

/// La reprise de démarrage, en fond, après [`DELAI_AVANT_REPRISE`]. Appelée
/// par `background::spawn_background_tasks` APRÈS `hydrater` : une passe
/// relancée doit trouver sa pause déjà restaurée.
pub fn spawn(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DELAI_AVANT_REPRISE).await;
        let relancees = relancer_les_passes_interrompues(&state).await;
        if !relancees.is_empty() {
            info!(passes = ?relancees, "reprise_des_passes_au_demarrage");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Câblage : la reprise de démarrage est lancée par
    /// `spawn_background_tasks`, et APRÈS `hydrater`. Sans l'appel, ce module
    /// serait du code mort ; avant `hydrater`, une passe relancée travaillerait
    /// malgré une pause posée la veille.
    #[test]
    fn la_reprise_est_cablee_apres_la_relecture_des_pauses() {
        let source = include_str!("background.rs");
        let corps = source
            .split("pub async fn spawn_background_tasks")
            .nth(1)
            .expect("spawn_background_tasks existe")
            .split("\n}\n")
            .next()
            .expect("corps de la fonction");
        let appel = corps
            .find("crate::reprise_des_passes::spawn(state);")
            .expect("spawn_background_tasks doit lancer la reprise des passes (#5469)");
        let hydratation = corps
            .find("tune_core::taches_de_fond::hydrater(&state.backend);")
            .expect("hydrater est appelé");
        assert!(
            hydratation < appel,
            "la reprise doit partir APRÈS la relecture des pauses"
        );
    }

    /// Chaque passe a sa clé et son identifiant de registre ; deux passes à la
    /// même clé s'effaceraient le repère l'une de l'autre.
    #[test]
    fn les_passes_ont_des_cles_distinctes() {
        let mut cles: Vec<&str> = Passe::TOUTES.iter().map(|p| p.cle_reglage()).collect();
        let mut ids: Vec<&str> = Passe::TOUTES.iter().map(|p| p.id()).collect();
        cles.sort_unstable();
        cles.dedup();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(cles.len(), Passe::TOUTES.len());
        assert_eq!(ids.len(), Passe::TOUTES.len());
    }
}
