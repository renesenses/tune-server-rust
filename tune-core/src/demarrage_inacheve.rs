//! #6452 — le témoin de démarrage inachevé.
//!
//! Jean-Pierre (fil 2248, Windows 10, ASIO4ALL) : chaque démarrage qui suit le
//! choix de l'hôte ASIO reste bloqué avant `listening`, et l'interface ne
//! revient jamais. Avec une base neuve (donc WASAPI), tout marche. Le choix de
//! l'hôte est en base et passe AVANT `TUNE_LOCAL_AUDIO_BACKEND`
//! (`AppState::effective_audio_backend`) : une fois ASIO choisi, plus rien ne
//! permet de démarrer sans lui, sinon d'effacer tout le dossier de données.
//!
//! Le témoin `asio-warm.pending` (#1283, #4168, #4556) ne couvre pas ce cas :
//! il n'entoure QUE le préchauffage ASIO (`startup::spawn_asio_warm_scan`), et
//! il est retiré dès que ce balayage revient — en 0,45 s dans le journal du
//! 20/10, bien avant que le démarrage ne bloque. Il dit « un pilote a emporté
//! le processus pendant l'énumération », pas « le démarrage n'est pas allé au
//! bout ». D'où un second témoin, posé au tout début du démarrage et retiré à
//! `listening`, dans le même dossier.
//!
//! Ce que ce module fait : si le témoin est encore là au démarrage suivant ET
//! que l'hôte choisi est un hôte susceptible de bloquer (ASIO, le seul que le
//! code déclare exclusif par nature, voir
//! [`crate::config::ExclusiveModeConstraint`]), CE démarrage-là se fait avec
//! l'hôte par défaut (WASAPI partagé sous Windows). Le choix enregistré en
//! base n'est PAS touché : si ce démarrage aboutit, le témoin est retiré, et
//! le suivant retente ASIO.
//!
//! Ce que ce module ne fait pas : corriger le pilote. Il rend l'interface.
//!
//! La décision est une fonction pure ([`decider`]) : le chemin Windows ne se
//! joue pas sur la machine de compilation, la règle, si.

use std::path::Path;
use std::sync::OnceLock;

/// Nom du témoin, posé dans le dossier de données (celui du journal et de
/// `asio-warm.pending`, donc `%LOCALAPPDATA%\TuneServer` sous Windows).
pub const TEMOIN: &str = "demarrage.pending";

/// Ce que l'écran dit quand le repli est appliqué.
pub const DETAIL: &str = "ASIO non utilisé : le démarrage précédent n'a pas abouti";

/// Le nom de la ligne de journal du repli.
pub const EVENEMENT: &str = "demarrage_precedent_inacheve_repli_hote_par_defaut";

/// La valeur de `local_audio_backend` qui désigne l'hôte par défaut.
pub const HOTE_PAR_DEFAUT: &str = "auto";

/// L'hôte demandé est-il de ceux qui peuvent retenir le démarrage ?
///
/// ASIO seulement : c'est le seul hôte que le code déclare exclusif par nature
/// (`ExclusiveModeConstraint::AsioAlwaysExclusive`), et le seul qui charge un
/// pilote tiers DANS le processus (COM + `ASIOInit`, voir `list_asio_devices`).
/// WASAPI, CoreAudio et ALSA passent par le système.
pub fn hote_susceptible_de_bloquer(hote: &str) -> bool {
    hote.trim().eq_ignore_ascii_case("asio")
}

/// Ce que ce démarrage fait de l'hôte demandé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoteDeCeDemarrage {
    /// Le choix enregistré s'applique tel quel.
    Demande,
    /// Le démarrage précédent n'a pas abouti avec un hôte qui peut bloquer :
    /// celui-ci part sur l'hôte par défaut, sans toucher au choix enregistré.
    RepliHoteParDefaut,
}

/// La décision, sans effet de bord.
///
/// `on_windows` est un PARAMÈTRE, comme dans
/// [`crate::config::exclusive_mode_status`] : hors Windows, `asio` est une
/// valeur héritée qui n'ouvre déjà rien (#1268), il n'y a rien à replier ni à
/// annoncer.
pub fn decider(temoin_present: bool, hote_demande: &str, on_windows: bool) -> HoteDeCeDemarrage {
    if temoin_present && on_windows && hote_susceptible_de_bloquer(hote_demande) {
        HoteDeCeDemarrage::RepliHoteParDefaut
    } else {
        HoteDeCeDemarrage::Demande
    }
}

/// L'hôte à ouvrir, sachant le choix enregistré et le repli éventuel.
///
/// `repli` est l'hôte remplacé pour ce démarrage (voir [`repli_actif`]).
pub fn hote_de_ce_demarrage(demande: String, repli: Option<&str>) -> String {
    if repli.is_some() && hote_susceptible_de_bloquer(&demande) {
        HOTE_PAR_DEFAUT.to_string()
    } else {
        demande
    }
}

/// La demande d'exclusif qui vaut pour ce démarrage.
///
/// Choisir ASIO écrit `local_exclusive_mode = true` et pose le témoin
/// `REGLAGE_EXCLUSIF_ARME_PAR_ASIO` (#4184) : ce `true`-là est l'écho de la
/// contrainte ASIO, pas une demande. Sous le repli, l'hôte n'est plus ASIO :
/// l'écho ne vaut rien, et la sortie s'ouvre en PARTAGÉ. Une demande posée par
/// l'utilisateur lui-même (sans témoin) reste honorée, comme pour une sortie
/// WASAPI sous ASIO (`demande_d_exclusif_du_peripherique`, #5353).
pub fn exclusif_de_ce_demarrage(demande: bool, arme_par_asio: bool, repli_applique: bool) -> bool {
    demande && !(repli_applique && arme_par_asio)
}

/// L'hôte remplacé pour CE processus, posé une fois au démarrage.
static REPLI: OnceLock<String> = OnceLock::new();

/// Arme le repli pour ce processus. Sans effet après le premier appel.
pub fn activer_le_repli(hote_demande: &str) {
    let _ = REPLI.set(hote_demande.trim().to_ascii_lowercase());
}

/// L'hôte écarté pour ce démarrage, s'il y en a un (`Some("asio")`).
pub fn repli_actif() -> Option<&'static str> {
    REPLI.get().map(String::as_str)
}

/// Contenu du témoin : la version et l'instant, pour un rapport de support.
fn contenu_du_temoin(version: &str, depuis_epoque_s: u64) -> String {
    format!("demarrage en cours\nversion={version}\ndepuis={depuis_epoque_s}\n")
}

/// Pose le témoin et dit s'il était déjà là (le démarrage précédent n'a pas
/// atteint `listening`).
///
/// Un échec d'écriture n'empêche pas de démarrer : il est rendu pour être
/// journalisé, et la seule conséquence est qu'un blocage de CE démarrage ne
/// sera pas vu au suivant.
pub fn poser_le_temoin(chemin: &Path) -> (bool, std::io::Result<()>) {
    let present = chemin.exists();
    if let Some(dossier) = chemin.parent() {
        let _ = std::fs::create_dir_all(dossier);
    }
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let ecrit = std::fs::write(chemin, contenu_du_temoin(crate::version(), maintenant));
    (present, ecrit)
}

/// Retire le témoin : le serveur écoute, le démarrage a abouti.
pub fn retirer_le_temoin(chemin: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(chemin) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        autre => autre,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas de Jean-Pierre : témoin laissé, ASIO choisi, Windows.
    #[test]
    fn temoin_present_et_asio_donnent_l_hote_par_defaut() {
        assert_eq!(
            decider(true, "asio", true),
            HoteDeCeDemarrage::RepliHoteParDefaut
        );
        assert_eq!(
            decider(true, " ASIO ", true),
            HoteDeCeDemarrage::RepliHoteParDefaut
        );
        assert_eq!(
            hote_de_ce_demarrage("asio".into(), Some("asio")),
            HOTE_PAR_DEFAUT
        );
    }

    #[test]
    fn temoin_absent_garde_asio() {
        assert_eq!(decider(false, "asio", true), HoteDeCeDemarrage::Demande);
        assert_eq!(hote_de_ce_demarrage("asio".into(), None), "asio");
    }

    /// Un démarrage inachevé en WASAPI n'a pas d'hôte plus sûr où se replier.
    #[test]
    fn temoin_present_sans_hote_bloquant_ne_change_rien() {
        for hote in ["auto", "wasapi", "", "n'importe quoi"] {
            assert_eq!(
                decider(true, hote, true),
                HoteDeCeDemarrage::Demande,
                "{hote}"
            );
            assert_eq!(hote_de_ce_demarrage(hote.into(), Some("asio")), hote);
        }
    }

    /// Hors Windows, `asio` n'ouvre déjà rien (#1268) : rien à annoncer.
    #[test]
    fn hors_windows_rien_n_est_replie() {
        assert_eq!(decider(true, "asio", false), HoteDeCeDemarrage::Demande);
    }

    #[test]
    fn l_echo_d_asio_ne_vaut_rien_sous_le_repli() {
        // L'écho du choix d'ASIO : partagé sous le repli.
        assert!(!exclusif_de_ce_demarrage(true, true, true));
        // Une demande de l'utilisateur lui-même reste honorée.
        assert!(exclusif_de_ce_demarrage(true, false, true));
        // Sans repli, rien ne change.
        assert!(exclusif_de_ce_demarrage(true, true, false));
        assert!(!exclusif_de_ce_demarrage(false, true, true));
    }

    /// Le cycle du fichier : posé, retrouvé s'il n'est pas retiré, effacé à
    /// `listening`.
    #[test]
    fn le_temoin_est_pose_retrouve_puis_efface() {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join(TEMOIN);

        let (present, ecrit) = poser_le_temoin(&chemin);
        assert!(!present, "premier démarrage : pas de témoin précédent");
        ecrit.unwrap();
        assert!(chemin.exists());
        let contenu = std::fs::read_to_string(&chemin).unwrap();
        assert!(contenu.contains(&format!("version={}", crate::version())));

        // Démarrage resté bloqué (témoin jamais retiré) : le suivant le voit.
        let (present, _) = poser_le_temoin(&chemin);
        assert!(present, "le démarrage précédent n'a pas abouti");

        // Démarrage réussi : le témoin est effacé, le suivant ne voit rien.
        retirer_le_temoin(&chemin).unwrap();
        assert!(!chemin.exists());
        let (present, _) = poser_le_temoin(&chemin);
        assert!(!present);
        retirer_le_temoin(&chemin).unwrap();
        // Retirer un témoin absent n'est pas une erreur.
        retirer_le_temoin(&chemin).unwrap();
    }

    #[test]
    fn le_message_nomme_asio_et_la_cause() {
        assert!(DETAIL.contains("ASIO non utilisé"));
        assert!(DETAIL.contains("démarrage précédent n'a pas abouti"));
    }
}
