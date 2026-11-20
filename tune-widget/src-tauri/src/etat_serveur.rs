//! L'état du serveur dans le menu de l'icône de Tune Client (#9244, fil 2966).
//!
//! Décision de Bertrand (20/11/2026) : pas d'icône propre à Tune Server.
//! C'est Tune Client, qui a déjà son icône dans la barre des menus (macOS) ou
//! la zone de notification (Windows), qui montre l'état du serveur et permet
//! de le démarrer ou de l'arrêter.
//!
//! # Ce que le menu montre
//!
//! - une ligne d'état, inactive : « Serveur local : en marche (1.0.0-rc13) »,
//!   « Serveur local : arrêté », ou pour un serveur d'une autre machine
//!   « Serveur 192.168.1.18:8888 : en marche / injoignable » ;
//! - une entrée d'action : « Démarrer le serveur » quand le serveur local est
//!   arrêté, « Arrêter le serveur » quand il tourne SUR CETTE MACHINE ; grisée
//!   sinon (serveur distant, Linux, serveur qui répond sur `localhost` sans
//!   qu'aucun processus `tune-server` ne tourne ici : conteneur, tunnel SSH).
//!
//! # Arrêter, proprement, et jamais un serveur distant
//!
//! 1. Seule une adresse de bouclage compte comme locale
//!    ([`crate::serveur_local::adresse_locale`]) ET un processus `tune-server`
//!    doit tourner sur cette machine ([`processus_serveur_present`]).
//! 2. D'abord la route du serveur, `POST /api/v1/system/stop` : le serveur
//!    sort lui-même (`exit(0)`). Sa garde, `AdminOuCetteMachine`, l'accepte
//!    d'un appel de cette machine tant que l'authentification est coupée (le
//!    défaut) ; hors de la machine elle répond `403 not_local`.
//! 3. Refusée (authentification active : Tune Client n'a pas de jeton), ou
//!    sans réponse : par le processus ([`arreter_par_le_processus`]).
//!    - macOS : SIGTERM au processus `tune-server` de l'utilisateur
//!      (`pkill -TERM -x tune-server`) ; le serveur s'arrête proprement sur ce
//!      signal (`bootstrap.rs`), et le lanceur `Tune Server.app` quitte de
//!      lui-même quand son serveur s'arrête (`on idle`). Pas d'Apple Event
//!      « quit » au lanceur : il demanderait l'autorisation d'automatisation
//!      (TCC) au nom de Tune Client.
//!    - Windows : `taskkill /F` sur `tune-server.exe`, limité aux processus de
//!      l'utilisateur de la session. `/F` parce que le serveur lancé par
//!      `--sans-console` n'a pas de fenêtre à qui demander de se fermer.

/// Ce que le serveur configuré répond à `GET /api/v1/system/version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Etat {
    /// Il répond (n'importe quel code HTTP : le port est tenu).
    EnMarche { version: Option<String> },
    /// Rien ne répond.
    Arrete,
}

/// L'action que propose le menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Demarrer,
    Arreter,
    /// Rien à proposer : l'entrée est grisée.
    Aucune,
}

/// Une photographie de la situation, ce que le menu affiche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instantane {
    pub serveur: String,
    pub local: bool,
    pub etat: Etat,
    /// Un processus `tune-server` tourne-t-il sur cette machine ?
    pub processus_local: bool,
}

/// La route publique qui donne la version.
pub const ROUTE_VERSION: &str = "/api/v1/system/version";
/// La route qui arrête le processus serveur (`config::stop`).
pub const ROUTE_ARRET: &str = "/api/v1/system/stop";

/// La version lue dans `{"version": "…", "engine": "rust"}`.
pub fn version_de(corps: &serde_json::Value) -> Option<String> {
    corps["version"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .map(str::to_string)
}

/// `hôte:port` d'une adresse, pour nommer un serveur distant.
fn hote_de(serveur: &str) -> String {
    match url::Url::parse(serveur.trim()) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => serveur.trim().to_string(),
        },
        Err(_) => serveur.trim().to_string(),
    }
}

/// La ligne d'état du menu.
pub fn libelle_etat(i: &Instantane) -> String {
    let qui = if i.local {
        "Serveur local".to_string()
    } else {
        format!("Serveur {}", hote_de(&i.serveur))
    };
    match &i.etat {
        Etat::EnMarche { version: Some(v) } => format!("{qui} : en marche ({v})"),
        Etat::EnMarche { version: None } => format!("{qui} : en marche"),
        Etat::Arrete if i.local => format!("{qui} : arrêté"),
        Etat::Arrete => format!("{qui} : injoignable"),
    }
}

/// L'action proposée. `pris_en_charge` : le système sait lancer le serveur
/// ([`crate::serveur_local::PRIS_EN_CHARGE`], Windows et macOS).
pub fn action_proposee(i: &Instantane, pris_en_charge: bool) -> Action {
    if !pris_en_charge || !i.local {
        return Action::Aucune;
    }
    match i.etat {
        Etat::Arrete => Action::Demarrer,
        Etat::EnMarche { .. } if i.processus_local => Action::Arreter,
        Etat::EnMarche { .. } => Action::Aucune,
    }
}

/// Le texte de l'entrée d'action et si elle est active.
pub fn libelle_action(action: Action) -> (&'static str, bool) {
    match action {
        Action::Demarrer => ("Démarrer le serveur", true),
        Action::Arreter => ("Arrêter le serveur", true),
        Action::Aucune => ("Arrêter le serveur", false),
    }
}

/// Ce que dit la réponse de `POST /system/stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApresRoute {
    /// 2xx : le serveur s'arrête de lui-même.
    Accepte,
    /// Refus (authentification active), autre code, ou pas de réponse :
    /// passer par le processus.
    ParLeProcessus,
}

pub fn apres_route(statut: Option<u16>) -> ApresRoute {
    match statut {
        Some(s) if (200..300).contains(&s) => ApresRoute::Accepte,
        _ => ApresRoute::ParLeProcessus,
    }
}

/// Le nom de processus du serveur sous macOS (`Contents/Resources/tune-server`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const PROCESSUS_MACOS: &str = "tune-server";

/// Les arguments de `pkill` pour arrêter le serveur sous macOS. Constantes :
/// rien n'y entre d'une saisie. Sans `-U` : un utilisateur ordinaire ne peut
/// de toute façon signaler que ses propres processus.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const ARGS_PKILL_MACOS: [&str; 3] = ["-TERM", "-x", PROCESSUS_MACOS];

/// `tasklist … /FO CSV /NH` liste-t-il `tune-server.exe` ? (Le message « aucune
/// tâche » est traduit selon la langue de Windows : on cherche le nom.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tasklist_contient_le_serveur(sortie: &str) -> bool {
    sortie.lines().any(|l| {
        l.trim_start()
            .to_ascii_lowercase()
            .starts_with("\"tune-server.exe\"")
    })
}

/// Le filtre `taskkill`/`tasklist` sur l'utilisateur de la session, `None`
/// pour un nom vide ou qui contient un guillemet (on ne filtre alors pas, et on
/// ne tue rien : voir [`arreter_par_le_processus`]).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn filtre_utilisateur(nom: &str) -> Option<String> {
    let nom = nom.trim();
    (!nom.is_empty() && !nom.contains('"')).then(|| format!("USERNAME eq {nom}"))
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Un processus serveur tourne-t-il sur cette machine ?
#[cfg(target_os = "macos")]
pub fn processus_serveur_present() -> bool {
    use std::process::{Command, Stdio};
    Command::new("/usr/bin/pgrep")
        .arg("-x")
        .arg(PROCESSUS_MACOS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(windows)]
pub fn processus_serveur_present() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let Some(utilisateur) = std::env::var("USERNAME")
        .ok()
        .and_then(|n| filtre_utilisateur(&n))
    else {
        return false;
    };
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq tune-server.exe", "/FI"])
        .arg(utilisateur)
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| tasklist_contient_le_serveur(&String::from_utf8_lossy(&o.stdout)))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn processus_serveur_present() -> bool {
    false
}

/// macOS : le lanceur `Tune Server.app` tourne-t-il ?
#[cfg(target_os = "macos")]
pub fn applet_presente() -> bool {
    use std::process::{Command, Stdio};
    Command::new("/usr/bin/pgrep")
        .arg("-x")
        .arg(crate::serveur_local::APPLET_MACOS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(target_os = "macos"))]
pub fn applet_presente() -> bool {
    false
}

/// Arrête le serveur local par son processus (repli de la route). `Err` dit
/// pourquoi rien n'a été arrêté.
#[cfg(target_os = "macos")]
pub fn arreter_par_le_processus() -> Result<String, String> {
    use std::process::{Command, Stdio};
    let statut = Command::new("/usr/bin/pkill")
        .args(ARGS_PKILL_MACOS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("pkill : {e}"))?;
    if statut.success() {
        Ok("SIGTERM envoyé à tune-server".to_string())
    } else {
        Err(format!("pkill n'a arrêté aucun tune-server ({statut})"))
    }
}

#[cfg(windows)]
pub fn arreter_par_le_processus() -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let utilisateur = std::env::var("USERNAME")
        .ok()
        .and_then(|n| filtre_utilisateur(&n))
        .ok_or_else(|| "utilisateur de la session inconnu : rien n'est arrêté".to_string())?;
    let statut = Command::new("taskkill")
        .args(["/F", "/IM", "tune-server.exe", "/FI"])
        .arg(utilisateur)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("taskkill : {e}"))?;
    if statut.success() {
        Ok("tune-server.exe arrêté".to_string())
    } else {
        Err(format!("taskkill a échoué ({statut})"))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn arreter_par_le_processus() -> Result<String, String> {
    Err("non pris en charge sur ce système".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(serveur: &str, etat: Etat, processus_local: bool) -> Instantane {
        Instantane {
            serveur: serveur.to_string(),
            local: crate::serveur_local::adresse_locale(serveur),
            etat,
            processus_local,
        }
    }

    fn en_marche(v: &str) -> Etat {
        Etat::EnMarche {
            version: Some(v.to_string()),
        }
    }

    #[test]
    fn la_version_vient_de_la_route_publique() {
        let corps = serde_json::json!({"version": "1.0.0-rc13", "engine": "rust"});
        assert_eq!(version_de(&corps), Some("1.0.0-rc13".to_string()));
        assert_eq!(version_de(&serde_json::json!({})), None);
        assert_eq!(version_de(&serde_json::json!({"version": " "})), None);
        assert_eq!(version_de(&serde_json::json!({"version": 3})), None);
        assert_eq!(
            version_de(&serde_json::json!({"version": "x".repeat(65)})),
            None
        );
    }

    #[test]
    fn la_ligne_d_etat_dit_local_ou_distant() {
        let l = inst("http://localhost:8888", en_marche("1.0.0-rc13"), true);
        assert_eq!(libelle_etat(&l), "Serveur local : en marche (1.0.0-rc13)");
        let l = inst(
            "http://127.0.0.1:8888",
            Etat::EnMarche { version: None },
            true,
        );
        assert_eq!(libelle_etat(&l), "Serveur local : en marche");
        let l = inst("http://localhost:8888", Etat::Arrete, false);
        assert_eq!(libelle_etat(&l), "Serveur local : arrêté");
        let d = inst("http://192.168.1.18:8888", en_marche("1.0.0-rc12"), false);
        assert_eq!(
            libelle_etat(&d),
            "Serveur 192.168.1.18:8888 : en marche (1.0.0-rc12)"
        );
        let d = inst("http://tune.local", Etat::Arrete, false);
        assert_eq!(libelle_etat(&d), "Serveur tune.local : injoignable");
    }

    #[test]
    fn jamais_d_action_sur_un_serveur_distant() {
        for etat in [en_marche("1"), Etat::Arrete] {
            for processus in [true, false] {
                for serveur in ["http://192.168.1.18:8888", "http://tune.local:8888"] {
                    let d = inst(serveur, etat.clone(), processus);
                    assert_eq!(action_proposee(&d, true), Action::Aucune, "{d:?}");
                }
            }
        }
    }

    #[test]
    fn demarrer_si_arrete_arreter_si_le_processus_est_ici() {
        let l = inst("http://localhost:8888", Etat::Arrete, false);
        assert_eq!(action_proposee(&l, true), Action::Demarrer);
        let l = inst("http://localhost:8888", en_marche("1"), true);
        assert_eq!(action_proposee(&l, true), Action::Arreter);
        // Répond sur localhost sans processus ici : conteneur, tunnel SSH.
        let l = inst("http://localhost:8888", en_marche("1"), false);
        assert_eq!(action_proposee(&l, true), Action::Aucune);
        // Linux : le serveur est un service système.
        let l = inst("http://localhost:8888", Etat::Arrete, false);
        assert_eq!(action_proposee(&l, false), Action::Aucune);
    }

    #[test]
    fn l_entree_d_action_est_grisee_sans_action() {
        assert_eq!(
            libelle_action(Action::Demarrer),
            ("Démarrer le serveur", true)
        );
        assert_eq!(
            libelle_action(Action::Arreter),
            ("Arrêter le serveur", true)
        );
        assert!(!libelle_action(Action::Aucune).1);
    }

    #[test]
    fn la_route_d_abord_le_processus_si_elle_refuse() {
        assert_eq!(apres_route(Some(200)), ApresRoute::Accepte);
        assert_eq!(apres_route(Some(204)), ApresRoute::Accepte);
        // Authentification active : Tune Client n'a pas de jeton.
        assert_eq!(apres_route(Some(401)), ApresRoute::ParLeProcessus);
        assert_eq!(apres_route(Some(403)), ApresRoute::ParLeProcessus);
        // Ancien serveur sans la route.
        assert_eq!(apres_route(Some(404)), ApresRoute::ParLeProcessus);
        assert_eq!(apres_route(None), ApresRoute::ParLeProcessus);
    }

    #[test]
    fn macos_signale_seulement_le_processus_du_serveur_par_son_nom_exact() {
        assert_eq!(ARGS_PKILL_MACOS, ["-TERM", "-x", "tune-server"]);
    }

    #[test]
    fn tasklist_se_lit_quelle_que_soit_la_langue() {
        assert!(tasklist_contient_le_serveur(
            "\"tune-server.exe\",\"4242\",\"Console\",\"1\",\"80 000 K\"\r\n"
        ));
        assert!(tasklist_contient_le_serveur("\"Tune-Server.EXE\",\"1\"\n"));
        assert!(!tasklist_contient_le_serveur(
            "INFO: No tasks are running which match the specified criteria.\r\n"
        ));
        assert!(!tasklist_contient_le_serveur(
            "Informations : aucune tâche en service ne correspond aux critères spécifiés.\r\n"
        ));
        assert!(!tasklist_contient_le_serveur(
            "\"tune-server.exe.bak\",\"1\"\n"
        ));
        assert!(!tasklist_contient_le_serveur(""));
    }

    #[test]
    fn le_filtre_utilisateur_refuse_un_nom_douteux() {
        assert_eq!(
            filtre_utilisateur("bertrand"),
            Some("USERNAME eq bertrand".to_string())
        );
        assert_eq!(filtre_utilisateur("  "), None);
        assert_eq!(filtre_utilisateur("a\" /IM x"), None);
    }
}
