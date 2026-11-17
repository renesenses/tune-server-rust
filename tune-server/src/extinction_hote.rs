//! Extinction de la machine HÔTE par l'arrêt programmé, hors Tune OS (#8909).
//!
//! laguespa (fil forum 2720) veut que l'arrêt programmé éteigne aussi son
//! ordinateur, qui n'est pas un Tune OS. Jusqu'à la rc12 (#8213, #8424),
//! `action: "poweroff"` n'était permis que sur Tune OS. Ce module ajoute
//! l'extinction de l'hôte sous Linux, macOS et Windows, avec ces règles :
//!
//! - **une seule porte d'entrée** : [`extinction_programmee`], appelée par le
//!   crochet du planificateur de l'arrêt programmé (`background.rs`) et par
//!   rien d'autre. Aucune route HTTP n'éteint l'hôte : la route du réglage
//!   (`PUT /system/settings/scheduled-stop`, administrateur) ne fait que POSER
//!   l'option, désactivée par défaut (`action: "stop_playback"`) ;
//! - **aucune commande construite** : chaque commande est un chemin ABSOLU et
//!   des arguments CONSTANTS ([`Commande`], `&'static str` seulement), lancée
//!   sans shell, sans sudo, sans variable d'environnement qui choisisse le
//!   binaire ;
//! - **jamais dans un conteneur** : Docker, Podman, Kubernetes… ne doivent pas
//!   éteindre l'hôte (et ne le peuvent pas proprement). L'option y est
//!   masquée (`poweroff_available: false`), refusée par la route, et refusée
//!   encore au déclenchement ;
//! - **un refus se dit** : polkit (Linux), l'autorisation d'automatisation
//!   (macOS) ou le privilège d'arrêt (Windows) qui manquent donnent un message
//!   clair, gardé pour l'écran (`last_poweroff_error`) et le journal.
//!
//! Linux : `systemctl --no-ask-password poweroff`, c'est-à-dire l'appel
//! logind `org.freedesktop.login1.Manager.PowerOff(interactive=false)` pour un
//! compte non root ; sans systemd (OpenRC + elogind), `loginctl
//! --no-ask-password poweroff`, même appel D-Bus. La règle polkit à poser
//! est dans `docs/ARRET-PROGRAMME-EXTINCTION.md`.
//! macOS : `osascript -e 'tell application "System Events" to shut down'`
//! (script constant), qui demande l'autorisation d'automatisation une fois.
//! Windows : `shutdown.exe /s /t 0`, avec le privilège d'arrêt du compte du
//! service.
//!
//! Tout se teste sans rien éteindre : la plateforme, le conteneur et
//! l'exécution des commandes sont des paramètres ([`Contexte`],
//! [`Executeur`]).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Le système hôte, pour choisir les commandes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plateforme {
    Linux,
    MacOs,
    Windows,
    /// BSD et autres : pas d'extinction de l'hôte.
    Autre,
}

impl Plateforme {
    /// La plateforme de CE binaire.
    pub fn courante() -> Self {
        if cfg!(target_os = "linux") {
            Plateforme::Linux
        } else if cfg!(target_os = "macos") {
            Plateforme::MacOs
        } else if cfg!(windows) {
            Plateforme::Windows
        } else {
            Plateforme::Autre
        }
    }
}

/// Une commande d'extinction : un programme absolu et ses arguments, tous
/// CONSTANTS. Le type n'admet que des `&'static str` : aucune donnée reçue
/// (requête, réglage, base) ne peut y entrer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commande {
    pub programme: &'static str,
    pub args: &'static [&'static str],
}

/// `--no-ask-password` : logind est appelé avec `interactive=false`, polkit
/// répond oui ou non sans chercher d'agent d'authentification.
const ARGS_LOGIND: &[&str] = &["--no-ask-password", "poweroff"];

const LINUX: &[Commande] = &[
    Commande {
        programme: "/usr/bin/systemctl",
        args: ARGS_LOGIND,
    },
    Commande {
        programme: "/bin/systemctl",
        args: ARGS_LOGIND,
    },
    // OpenRC + elogind : pas de systemctl, même méthode logind.
    Commande {
        programme: "/usr/bin/loginctl",
        args: ARGS_LOGIND,
    },
    Commande {
        programme: "/bin/loginctl",
        args: ARGS_LOGIND,
    },
];

const MACOS: &[Commande] = &[Commande {
    programme: "/usr/bin/osascript",
    args: &["-e", "tell application \"System Events\" to shut down"],
}];

const WINDOWS: &[Commande] = &[Commande {
    programme: r"C:\Windows\System32\shutdown.exe",
    args: &["/s", "/t", "0"],
}];

/// Les commandes candidates, dans l'ordre d'essai.
pub fn commandes(plateforme: Plateforme) -> &'static [Commande] {
    match plateforme {
        Plateforme::Linux => LINUX,
        Plateforme::MacOs => MACOS,
        Plateforme::Windows => WINDOWS,
        Plateforme::Autre => &[],
    }
}

/// Ce que l'extinction sait de la machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contexte {
    pub plateforme: Plateforme,
    /// Le moteur de conteneur ([`crate::conteneur::detecter`]), `None` hors
    /// conteneur.
    pub conteneur: Option<&'static str>,
}

/// Le conteneur de CE processus. `TUNE_CONTENEUR_FORCE` sert aux essais :
/// forcer un conteneur (« docker ») est permis partout, c'est le sens qui
/// REFUSE ; forcer « aucun » ne vaut que dans un binaire de développement.
fn conteneur_courant() -> Option<&'static str> {
    match std::env::var("TUNE_CONTENEUR_FORCE").ok().as_deref() {
        Some("docker") => return Some("docker"),
        Some("aucun") if cfg!(debug_assertions) => return None,
        _ => {}
    }
    crate::conteneur::detecter()
}

impl Contexte {
    pub fn courant() -> Self {
        Contexte {
            plateforme: Plateforme::courante(),
            conteneur: conteneur_courant(),
        }
    }

    /// L'extinction de l'hôte est-elle proposée ici ?
    pub fn disponible(&self) -> bool {
        self.conteneur.is_none() && !commandes(self.plateforme).is_empty()
    }
}

/// Pourquoi l'hôte ne s'est pas éteint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refus {
    /// Tune tourne dans un conteneur.
    Conteneur(&'static str),
    /// Système sans commande d'extinction connue.
    NonPrisEnCharge,
    /// Linux : polkit refuse `org.freedesktop.login1.power-off` au compte.
    Polkit(String),
    /// macOS : l'automatisation de « System Events » n'est pas autorisée.
    AutorisationMacos(String),
    /// Windows : le compte n'a pas le privilège d'arrêt.
    PrivilegeWindows(String),
    /// Autre échec de la commande.
    Echec(String),
}

pub const REFUS_CONTENEUR: &str = "Tune is running in a container: it does not power off the \
host machine. Schedule the host's shutdown on the host itself.";

pub const REFUS_NON_PRIS_EN_CHARGE: &str =
    "Powering off the machine is not supported on this operating system.";

impl Refus {
    /// Code stable, pour l'écran et le journal.
    pub fn code(&self) -> &'static str {
        match self {
            Refus::Conteneur(_) => "container",
            Refus::NonPrisEnCharge => "unsupported",
            Refus::Polkit(_) => "polkit_denied",
            Refus::AutorisationMacos(_) => "macos_not_authorized",
            Refus::PrivilegeWindows(_) => "windows_privilege",
            Refus::Echec(_) => "failed",
        }
    }

    /// Le message, en clair.
    pub fn message(&self) -> String {
        match self {
            Refus::Conteneur(_) => REFUS_CONTENEUR.to_string(),
            Refus::NonPrisEnCharge => REFUS_NON_PRIS_EN_CHARGE.to_string(),
            Refus::Polkit(detail) => format!(
                "Linux refused to power off the machine (polkit): the account running Tune is \
                 not allowed to call org.freedesktop.login1.power-off without a password. Add \
                 the polkit rule given in docs/ARRET-PROGRAMME-EXTINCTION.md. ({detail})"
            ),
            Refus::AutorisationMacos(detail) => format!(
                "macOS did not allow Tune to shut down the computer: allow Tune to control \
                 \"System Events\" in System Settings > Privacy & Security > Automation. \
                 ({detail})"
            ),
            Refus::PrivilegeWindows(detail) => format!(
                "Windows refused to shut down the computer: the account running Tune does not \
                 have the \"Shut down the system\" right. ({detail})"
            ),
            Refus::Echec(detail) => format!("Powering off the machine failed: {detail}"),
        }
    }
}

/// Ce qu'une commande a rendu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sortie {
    /// Code de sortie, `None` si tuée par un signal.
    pub code: Option<i32>,
    pub stderr: String,
}

pub type Lancement<'a> = Pin<Box<dyn Future<Output = std::io::Result<Sortie>> + Send + 'a>>;

/// L'exécution des commandes, injectable : la production lance le
/// programme, les essais enregistrent l'appel sans rien éteindre.
pub trait Executeur: Send + Sync {
    /// Le programme existe-t-il ?
    fn existe(&self, programme: &'static str) -> bool;
    /// Lance la commande, sans shell, et rend sa sortie.
    fn lancer<'a>(&'a self, commande: &'a Commande) -> Lancement<'a>;
}

/// L'exécuteur réel.
pub struct ExecuteurSysteme;

/// Au-delà, la commande est abandonnée (elle rend normalement la main en
/// moins d'une seconde).
const DELAI_COMMANDE: Duration = Duration::from_secs(30);

impl Executeur for ExecuteurSysteme {
    fn existe(&self, programme: &'static str) -> bool {
        std::path::Path::new(programme).is_file()
    }

    fn lancer<'a>(&'a self, commande: &'a Commande) -> Lancement<'a> {
        Box::pin(async move {
            // Programme et arguments constants, passés en argv : pas de shell.
            let mut cmd = tokio::process::Command::new(commande.programme);
            cmd.args(commande.args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let sortie = tokio::time::timeout(DELAI_COMMANDE, cmd.output())
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))??;
            Ok(Sortie {
                code: sortie.status.code(),
                stderr: String::from_utf8_lossy(&sortie.stderr).trim().to_string(),
            })
        })
    }
}

/// Classe l'échec d'une commande.
fn classer(plateforme: Plateforme, sortie: &Sortie) -> Refus {
    let detail = if sortie.stderr.is_empty() {
        format!("exit code {:?}", sortie.code)
    } else {
        sortie.stderr.clone()
    };
    let bas = sortie.stderr.to_ascii_lowercase();
    match plateforme {
        // systemctl/loginctl : « Access denied », « Interactive authentication
        // required » (messages non traduits de systemd/elogind).
        Plateforme::Linux
            if bas.contains("access denied")
                || bas.contains("authentication required")
                || bas.contains("not authorized") =>
        {
            Refus::Polkit(detail)
        }
        // osascript : « Not authorized to send Apple events… (-1743) ».
        Plateforme::MacOs if bas.contains("-1743") || bas.contains("not authorized") => {
            Refus::AutorisationMacos(detail)
        }
        // shutdown.exe rend le code d'erreur Win32 : 5 = ERROR_ACCESS_DENIED
        // (le texte, lui, est traduit dans la langue du système).
        Plateforme::Windows if sortie.code == Some(5) => Refus::PrivilegeWindows(detail),
        _ => Refus::Echec(detail),
    }
}

/// Éteint l'hôte — **paramètres explicites, pour les essais**.
///
/// Dans un conteneur, rien n'est lancé. Sinon, les commandes candidates qui
/// existent sont essayées dans l'ordre : la première qui réussit l'emporte ;
/// un refus de droits (polkit, automatisation, privilège) arrête là, une
/// autre panne laisse sa chance à la suivante (systemctl présent sur une
/// machine sans systemd, par exemple).
pub async fn eteindre_avec(contexte: Contexte, executeur: &dyn Executeur) -> Result<(), Refus> {
    if let Some(moteur) = contexte.conteneur {
        return Err(Refus::Conteneur(moteur));
    }
    let mut dernier = Refus::NonPrisEnCharge;
    for commande in commandes(contexte.plateforme) {
        if !executeur.existe(commande.programme) {
            continue;
        }
        tracing::info!(programme = commande.programme, "host_poweroff_command");
        match executeur.lancer(commande).await {
            Ok(sortie) if sortie.code == Some(0) => return Ok(()),
            Ok(sortie) => {
                let refus = classer(contexte.plateforme, &sortie);
                if !matches!(refus, Refus::Echec(_)) {
                    return Err(refus);
                }
                dernier = refus;
            }
            Err(e) => dernier = Refus::Echec(format!("{}: {e}", commande.programme)),
        }
    }
    Err(dernier)
}

/// Comment la machine s'éteint ici, si elle le peut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Tune OS : la fonction d'extinction de l'appliance (#8213).
    TuneOs,
    /// Hôte ordinaire (Linux, macOS, Windows) : ce module (#8909).
    Hote,
}

impl Mode {
    pub fn code(self) -> &'static str {
        match self {
            Mode::TuneOs => "tune_os",
            Mode::Hote => "host",
        }
    }
}

/// Le mode d'extinction, paramètres explicites. Un conteneur l'emporte sur
/// tout, même sur un `TUNE_APPLIANCE=1` posé dans l'image.
pub fn mode_avec(contexte: Contexte, appliance: bool) -> Option<Mode> {
    if contexte.conteneur.is_some() {
        return None;
    }
    if appliance {
        return Some(Mode::TuneOs);
    }
    contexte.disponible().then_some(Mode::Hote)
}

/// Le mode d'extinction de CETTE machine (`None` : l'option est masquée et
/// refusée).
pub fn mode() -> Option<Mode> {
    mode_avec(
        Contexte::courant(),
        crate::routes::appliance::is_appliance(),
    )
}

/// Le refus à donner quand [`mode`] est `None`.
pub fn refus_indisponible() -> Refus {
    let contexte = Contexte::courant();
    match contexte.conteneur {
        Some(moteur) => Refus::Conteneur(moteur),
        None => Refus::NonPrisEnCharge,
    }
}

/// LA porte de l'extinction programmée : appelée par le crochet du
/// planificateur de l'arrêt programmé, et par lui seul. Tune OS garde sa
/// fonction d'extinction ([`crate::routes::appliance::ordonner`]) ; un hôte
/// ordinaire passe par [`eteindre_avec`]. Le mode est relu À L'ÉCHÉANCE : un
/// réglage importé ou recopié dans un conteneur ne passe pas.
pub async fn extinction_programmee() -> Result<(), String> {
    match mode() {
        Some(Mode::TuneOs) => {
            crate::routes::appliance::ordonner(crate::routes::appliance::OrdreMachine::Eteindre)
                .await
        }
        Some(Mode::Hote) => eteindre_avec(Contexte::courant(), &ExecuteurSysteme)
            .await
            .map_err(|r| r.message()),
        None => Err(refus_indisponible().message()),
    }
}

#[cfg(test)]
mod tests;
