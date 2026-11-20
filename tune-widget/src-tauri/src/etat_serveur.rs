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
//! - une entrée d'action (Windows et macOS) : « Démarrer le serveur » quand
//!   le serveur local est arrêté, « Arrêter le serveur » quand il tourne SUR
//!   CETTE MACHINE ; grisée sinon.
//!
//! # Arrêter : ce serveur-là, et jamais un serveur distant
//!
//! « Sur cette machine » ne se déduit ni de l'adresse seule (un tunnel
//! `ssh -L 8888:…` fait répondre un AUTRE serveur sur `localhost:8888`, et
//! `POST /system/stop` lui arriverait comme un appel local), ni de la seule
//! présence d'un processus `tune-server` (il peut écouter un autre port, ou
//! appartenir à un autre utilisateur). La règle, sur le modèle de
//! `reclaim_port_from_stale_instance` (`tune-server/src/bootstrap.rs`) :
//!
//! 1. l'adresse est de bouclage ([`crate::serveur_local::adresse_locale`]) ;
//! 2. le processus qui ÉCOUTE sur son port ([`ecouteurs`] : `lsof` sous macOS,
//!    `netstat -ano` sous Windows) est UN seul PID, nommé `tune-server`, à
//!    l'utilisateur de la session ([`pid_a_arreter`]). Un tunnel est écarté :
//!    c'est `ssh` qui écoute.
//!
//! Alors seulement : `POST /api/v1/system/stop` (le serveur sort de lui-même ;
//! garde `AdminOuCetteMachine`, acceptée depuis cette machine tant que
//! l'authentification est coupée). Refusée (authentification active : Tune
//! Client n'a pas de jeton) ou sans réponse : ce PID-là, et lui seul, revérifié
//! juste avant ([`arreter_le_pid`]) — SIGTERM sous macOS (arrêt propre, le
//! lanceur `Tune Server.app` quitte ensuite de lui-même) ; `taskkill /PID`
//! sous Windows, `/F` seulement s'il est toujours là.

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
    /// Le serveur Tune de cet utilisateur qui écoute sur le port de
    /// l'adresse ([`pid_a_arreter`]). `None` : rien à arrêter ici.
    pub pid_local: Option<u32>,
}

/// Un processus qui écoute sur le port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ecouteur {
    pub pid: u32,
    /// Nom de l'exécutable (`tune-server`, `ssh`…), vide si inconnu.
    pub nom: String,
    /// Appartient à l'utilisateur de la session.
    pub a_l_utilisateur: bool,
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

/// Le port TCP d'une adresse (`8888`, ou 80/443 par défaut).
pub fn port_de(serveur: &str) -> Option<u16> {
    url::Url::parse(serveur.trim())
        .ok()?
        .port_or_known_default()
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

/// Le nom est-il celui du serveur Tune (`tune-server`, `tune-server.exe`,
/// éventuellement avec un chemin) ?
pub fn nom_du_serveur(nom: &str) -> bool {
    let base = nom.trim().rsplit(['/', '\\']).next().unwrap_or_default();
    base == "tune-server" || base.eq_ignore_ascii_case("tune-server.exe")
}

/// LA décision : le PID à arrêter, ou `None`.
///
/// Tous les écouteurs du port doivent être UN même processus (IPv4 et IPv6
/// donnent deux lignes pour un PID), nommé `tune-server`, à l'utilisateur.
/// Personne, plusieurs processus, un autre programme (`ssh` d'un tunnel), un
/// nom inconnu ou un autre utilisateur : `None`.
pub fn pid_a_arreter(ecouteurs: &[Ecouteur]) -> Option<u32> {
    let premier = ecouteurs.first()?;
    ecouteurs
        .iter()
        .all(|e| e.pid == premier.pid && e.a_l_utilisateur && nom_du_serveur(&e.nom))
        .then_some(premier.pid)
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
        Etat::EnMarche { .. } if i.pid_local.is_some() => Action::Arreter,
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

/// `lsof -t` : un PID par ligne.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pids_de_lsof(sortie: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = sortie
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    pids.dedup();
    pids
}

/// `ps -o uid=,ucomm= -p <pid>` → `(uid, nom)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn uid_et_nom_de_ps(sortie: &str) -> Option<(u32, String)> {
    let ligne = sortie.lines().map(str::trim).find(|l| !l.is_empty())?;
    let (uid, nom) = ligne.split_once(char::is_whitespace)?;
    Some((uid.parse().ok()?, nom.trim().to_string()))
}

/// Les PID qui ÉCOUTENT sur `port` d'après `netstat -ano`. Une socket TCP en
/// écoute a pour adresse distante `0.0.0.0:0` ou `[::]:0` : on se fonde sur
/// elle, pas sur le mot `LISTENING`, que Windows traduit (`ABHÖREN`…).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn pids_de_netstat(sortie: &str, port: u16) -> Vec<u32> {
    let suffixe = format!(":{port}");
    let mut pids = Vec::new();
    for ligne in sortie.lines() {
        let champs: Vec<&str> = ligne.split_whitespace().collect();
        // TCP  <locale>  <distante>  <état>  <pid>
        let [proto, locale, distante, _etat, pid] = champs[..] else {
            continue;
        };
        if !proto.eq_ignore_ascii_case("TCP")
            || !locale.ends_with(&suffixe)
            || !distante.ends_with(":0")
        {
            continue;
        }
        if let Ok(pid) = pid.parse::<u32>() {
            if pid != 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// `tasklist /FI "PID eq <pid>" /FI "USERNAME eq <u>" /FO CSV /NH` : le nom
/// du processus s'il appartient à l'utilisateur (sinon tasklist n'en sort
/// aucune ligne CSV : son message « aucune tâche » est traduit, on l'ignore).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn nom_de_tasklist(sortie: &str, pid: u32) -> Option<String> {
    let pid_csv = format!("\"{pid}\"");
    sortie.lines().find_map(|l| {
        let mut champs = l.trim().split(',');
        let nom = champs.next()?.strip_prefix('"')?.strip_suffix('"')?;
        (champs.next()? == pid_csv).then(|| nom.to_string())
    })
}

/// Le filtre `tasklist` sur l'utilisateur de la session, `None` pour un nom
/// vide ou qui contient un guillemet.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn filtre_utilisateur(nom: &str) -> Option<String> {
    let nom = nom.trim();
    (!nom.is_empty() && !nom.contains('"')).then(|| format!("USERNAME eq {nom}"))
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "macos")]
fn sortie_de(programme: &str, args: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};
    let o = Command::new(programme)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn uid_et_nom(pid: u32) -> Option<(u32, String)> {
    uid_et_nom_de_ps(&sortie_de(
        "/bin/ps",
        &["-o", "uid=,ucomm=", "-p", &pid.to_string()],
    )?)
}

/// Les processus qui écoutent sur `port` (TCP) sur cette machine.
#[cfg(target_os = "macos")]
pub fn ecouteurs(port: u16) -> Vec<Ecouteur> {
    let Some(moi) = uid_et_nom(std::process::id()).map(|(uid, _)| uid) else {
        return Vec::new();
    };
    let Some(sortie) = sortie_de(
        "/usr/sbin/lsof",
        &["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"],
    ) else {
        return Vec::new();
    };
    pids_de_lsof(&sortie)
        .into_iter()
        .map(|pid| match uid_et_nom(pid) {
            Some((uid, nom)) => Ecouteur {
                pid,
                nom,
                a_l_utilisateur: uid == moi,
            },
            None => Ecouteur {
                pid,
                nom: String::new(),
                a_l_utilisateur: false,
            },
        })
        .collect()
}

#[cfg(windows)]
fn sortie_de(programme: &str, args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let o = Command::new(programme)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(windows)]
pub fn ecouteurs(port: u16) -> Vec<Ecouteur> {
    let utilisateur = std::env::var("USERNAME")
        .ok()
        .and_then(|n| filtre_utilisateur(&n));
    let Some(sortie) = sortie_de("netstat", &["-ano"]) else {
        return Vec::new();
    };
    pids_de_netstat(&sortie, port)
        .into_iter()
        .map(|pid| {
            // Sans filtre utilisateur lisible : nom inconnu, donc refus.
            let nom = utilisateur.as_deref().and_then(|u| {
                let filtre_pid = format!("PID eq {pid}");
                let s = sortie_de(
                    "tasklist",
                    &["/FI", &filtre_pid, "/FI", u, "/FO", "CSV", "/NH"],
                )?;
                nom_de_tasklist(&s, pid)
            });
            Ecouteur {
                pid,
                a_l_utilisateur: nom.is_some(),
                nom: nom.unwrap_or_default(),
            }
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn ecouteurs(_port: u16) -> Vec<Ecouteur> {
    Vec::new()
}

/// Le serveur Tune de cet utilisateur qui écoute sur le port de `serveur`.
pub fn pid_du_serveur(serveur: &str) -> Option<u32> {
    pid_a_arreter(&ecouteurs(port_de(serveur)?))
}

/// Le PID écoute-t-il encore sur le port, en serveur Tune de l'utilisateur ?
fn toujours_le_meme(serveur: &str, pid: u32) -> bool {
    pid_du_serveur(serveur) == Some(pid)
}

/// Arrête CE processus, revérifié juste avant (le PID pourrait avoir été
/// repris). `Err` dit pourquoi rien n'a été arrêté.
#[cfg(target_os = "macos")]
pub fn arreter_le_pid(serveur: &str, pid: u32) -> Result<String, String> {
    use std::process::{Command, Stdio};
    if !toujours_le_meme(serveur, pid) {
        return Err(format!("le PID {pid} n'est plus le serveur de ce port"));
    }
    let statut = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("kill : {e}"))?;
    if statut.success() {
        Ok(format!("SIGTERM envoyé au PID {pid}"))
    } else {
        Err(format!("kill -TERM {pid} a échoué ({statut})"))
    }
}

#[cfg(windows)]
pub fn arreter_le_pid(serveur: &str, pid: u32) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let taskkill = |force: bool| {
        let mut c = Command::new("taskkill");
        if force {
            c.arg("/F");
        }
        c.args(["/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if !toujours_le_meme(serveur, pid) {
        return Err(format!("le PID {pid} n'est plus le serveur de ce port"));
    }
    // Sans /F d'abord : le serveur lancé par `--sans-console` n'a pas de
    // fenêtre et le refuse le plus souvent, mais on ne force qu'ensuite.
    let _ = taskkill(false);
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !toujours_le_meme(serveur, pid) {
            return Ok(format!("PID {pid} arrêté"));
        }
    }
    let statut = taskkill(true).map_err(|e| format!("taskkill : {e}"))?;
    if statut.success() {
        Ok(format!("PID {pid} arrêté (/F)"))
    } else {
        Err(format!("taskkill /F /PID {pid} a échoué ({statut})"))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn arreter_le_pid(_serveur: &str, _pid: u32) -> Result<String, String> {
    Err("non pris en charge sur ce système".to_string())
}

/// macOS : le lanceur `Tune Server.app` tourne-t-il ?
#[cfg(target_os = "macos")]
pub fn applet_presente() -> bool {
    use std::process::{Command, Stdio};
    let mut pgrep = Command::new("/usr/bin/pgrep");
    // Le lanceur de CET utilisateur seulement.
    if let Some((uid, _)) = uid_et_nom(std::process::id()) {
        pgrep.args(["-U", &uid.to_string()]);
    }
    pgrep
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(serveur: &str, etat: Etat, pid_local: Option<u32>) -> Instantane {
        Instantane {
            serveur: serveur.to_string(),
            local: crate::serveur_local::adresse_locale(serveur),
            etat,
            pid_local,
        }
    }

    fn en_marche(v: &str) -> Etat {
        Etat::EnMarche {
            version: Some(v.to_string()),
        }
    }

    fn ec(pid: u32, nom: &str, a_l_utilisateur: bool) -> Ecouteur {
        Ecouteur {
            pid,
            nom: nom.to_string(),
            a_l_utilisateur,
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
    fn le_port_vient_de_l_adresse() {
        assert_eq!(port_de("http://localhost:8888"), Some(8888));
        assert_eq!(port_de("http://localhost"), Some(80));
        assert_eq!(port_de("https://127.0.0.1/"), Some(443));
        assert_eq!(port_de("pas une adresse"), None);
    }

    #[test]
    fn la_ligne_d_etat_dit_local_ou_distant() {
        let l = inst("http://localhost:8888", en_marche("1.0.0-rc13"), Some(1));
        assert_eq!(libelle_etat(&l), "Serveur local : en marche (1.0.0-rc13)");
        let l = inst(
            "http://127.0.0.1:8888",
            Etat::EnMarche { version: None },
            Some(1),
        );
        assert_eq!(libelle_etat(&l), "Serveur local : en marche");
        let l = inst("http://localhost:8888", Etat::Arrete, None);
        assert_eq!(libelle_etat(&l), "Serveur local : arrêté");
        let d = inst("http://192.168.1.18:8888", en_marche("1.0.0-rc12"), None);
        assert_eq!(
            libelle_etat(&d),
            "Serveur 192.168.1.18:8888 : en marche (1.0.0-rc12)"
        );
        let d = inst("http://tune.local", Etat::Arrete, None);
        assert_eq!(libelle_etat(&d), "Serveur tune.local : injoignable");
    }

    /// Relecture de #9269 : un tunnel `ssh -L 8888:…` sur localhost, pendant
    /// qu'un `tune-server` de l'utilisateur tourne sur un AUTRE port. L'ancien
    /// code proposait « Arrêter » (un processus `tune-server` existait) et le
    /// `POST /system/stop` arrivait au serveur distant comme un appel local.
    #[test]
    fn un_tunnel_ssh_sur_le_port_n_est_jamais_arrete() {
        assert_eq!(pid_a_arreter(&[ec(4242, "ssh", true)]), None);
        let i = inst(
            "http://localhost:8888",
            en_marche("1"),
            pid_a_arreter(&[ec(4242, "ssh", true)]),
        );
        assert_eq!(action_proposee(&i, true), Action::Aucune);
    }

    #[test]
    fn seul_le_serveur_tune_de_l_utilisateur_qui_ecoute_est_arrete() {
        // IPv4 et IPv6 : deux lignes, un PID.
        assert_eq!(
            pid_a_arreter(&[ec(77, "tune-server", true), ec(77, "tune-server", true)]),
            Some(77)
        );
        assert_eq!(
            pid_a_arreter(&[ec(77, r"C:\T\tune-server.exe", true)]),
            Some(77)
        );
        assert_eq!(pid_a_arreter(&[ec(77, "TUNE-SERVER.EXE", true)]), Some(77));
        // Un autre utilisateur.
        assert_eq!(pid_a_arreter(&[ec(77, "tune-server", false)]), None);
        // Personne n'écoute, nom inconnu, autre programme.
        assert_eq!(pid_a_arreter(&[]), None);
        assert_eq!(pid_a_arreter(&[ec(77, "", true)]), None);
        assert_eq!(pid_a_arreter(&[ec(77, "tune-server-old", true)]), None);
        // Deux serveurs, ou un serveur et un intrus : ambigu, refus.
        assert_eq!(
            pid_a_arreter(&[ec(77, "tune-server", true), ec(78, "tune-server", true)]),
            None
        );
        assert_eq!(
            pid_a_arreter(&[ec(77, "tune-server", true), ec(90, "ssh", true)]),
            None
        );
    }

    #[test]
    fn jamais_d_action_sur_un_serveur_distant() {
        for etat in [en_marche("1"), Etat::Arrete] {
            for pid in [Some(1), None] {
                for serveur in ["http://192.168.1.18:8888", "http://tune.local:8888"] {
                    let d = inst(serveur, etat.clone(), pid);
                    assert_eq!(action_proposee(&d, true), Action::Aucune, "{d:?}");
                }
            }
        }
    }

    #[test]
    fn demarrer_si_arrete_arreter_si_le_serveur_du_port_est_ici() {
        let l = inst("http://localhost:8888", Etat::Arrete, None);
        assert_eq!(action_proposee(&l, true), Action::Demarrer);
        let l = inst("http://localhost:8888", en_marche("1"), Some(77));
        assert_eq!(action_proposee(&l, true), Action::Arreter);
        let l = inst("http://localhost:8888", en_marche("1"), None);
        assert_eq!(action_proposee(&l, true), Action::Aucune);
        // Linux : le serveur est un service système.
        let l = inst("http://localhost:8888", Etat::Arrete, None);
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
        assert_eq!(apres_route(Some(401)), ApresRoute::ParLeProcessus);
        assert_eq!(apres_route(Some(403)), ApresRoute::ParLeProcessus);
        assert_eq!(apres_route(Some(404)), ApresRoute::ParLeProcessus);
        assert_eq!(apres_route(None), ApresRoute::ParLeProcessus);
    }

    #[test]
    fn lsof_et_ps_se_lisent() {
        assert_eq!(pids_de_lsof("123\n123\n456\n"), vec![123, 456]);
        assert!(pids_de_lsof("").is_empty());
        assert_eq!(
            uid_et_nom_de_ps("  501 tune-server\n"),
            Some((501, "tune-server".to_string()))
        );
        assert_eq!(uid_et_nom_de_ps("0 ssh"), Some((0, "ssh".to_string())));
        assert_eq!(uid_et_nom_de_ps(""), None);
        assert_eq!(uid_et_nom_de_ps("abc tune-server"), None);
    }

    #[test]
    fn netstat_se_lit_quelle_que_soit_la_langue() {
        let sortie = "\r\nConnexions actives\r\n\r\n  Proto  Adresse locale         Adresse distante       État\r\n\
  TCP    0.0.0.0:8888           0.0.0.0:0              LISTENING       4242\r\n\
  TCP    [::]:8888              [::]:0                 ABHÖREN         4242\r\n\
  TCP    127.0.0.1:8888         127.0.0.1:51000        ESTABLISHED     4242\r\n\
  TCP    127.0.0.1:51000        127.0.0.1:8888         ESTABLISHED     9000\r\n\
  TCP    0.0.0.0:18888          0.0.0.0:0              LISTENING       5555\r\n\
  UDP    0.0.0.0:8888           *:*                                    6666\r\n";
        assert_eq!(pids_de_netstat(sortie, 8888), vec![4242]);
        assert_eq!(pids_de_netstat(sortie, 18888), vec![5555]);
        assert!(pids_de_netstat(sortie, 9999).is_empty());
    }

    #[test]
    fn tasklist_donne_le_nom_du_pid_demande() {
        let s = "\"tune-server.exe\",\"4242\",\"Console\",\"1\",\"80 000 K\"\r\n";
        assert_eq!(
            nom_de_tasklist(s, 4242),
            Some("tune-server.exe".to_string())
        );
        assert_eq!(nom_de_tasklist(s, 42), None);
        assert_eq!(
            nom_de_tasklist(
                "Informations : aucune tâche en service ne correspond aux critères spécifiés.\r\n",
                4242
            ),
            None
        );
        assert_eq!(nom_de_tasklist("", 1), None);
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
