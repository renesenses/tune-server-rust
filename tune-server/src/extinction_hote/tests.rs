//! #8909 — l'extinction de l'hôte, sans rien éteindre : la plateforme, le
//! conteneur et l'exécution sont des paramètres.

use super::*;
use std::sync::Mutex;

/// Un exécuteur qui ENREGISTRE les commandes au lieu de les lancer.
struct Faux {
    presents: Vec<&'static str>,
    /// Sortie rendue pour chaque programme (absent : succès).
    sorties: Vec<(&'static str, Sortie)>,
    lances: Mutex<Vec<Commande>>,
}

impl Faux {
    fn new(presents: &[&'static str]) -> Self {
        Faux {
            presents: presents.to_vec(),
            sorties: Vec::new(),
            lances: Mutex::new(Vec::new()),
        }
    }
    fn rend(mut self, programme: &'static str, code: i32, stderr: &str) -> Self {
        self.sorties.push((
            programme,
            Sortie {
                code: Some(code),
                stderr: stderr.into(),
            },
        ));
        self
    }
    fn lances(&self) -> Vec<Commande> {
        self.lances.lock().unwrap().clone()
    }
}

impl Executeur for Faux {
    fn existe(&self, programme: &'static str) -> bool {
        self.presents.contains(&programme)
    }
    fn lancer<'a>(&'a self, commande: &'a Commande) -> Lancement<'a> {
        self.lances.lock().unwrap().push(*commande);
        let sortie = self
            .sorties
            .iter()
            .find(|(p, _)| *p == commande.programme)
            .map(|(_, s)| s.clone())
            .unwrap_or(Sortie {
                code: Some(0),
                stderr: String::new(),
            });
        Box::pin(async move { Ok(sortie) })
    }
}

fn hote(plateforme: Plateforme) -> Contexte {
    Contexte {
        plateforme,
        conteneur: None,
    }
}

#[tokio::test]
async fn linux_appelle_systemctl_poweroff_sans_mot_de_passe_en_argv_fixe() {
    let faux = Faux::new(&["/usr/bin/systemctl", "/usr/bin/loginctl"]);
    assert_eq!(eteindre_avec(hote(Plateforme::Linux), &faux).await, Ok(()));
    assert_eq!(
        faux.lances(),
        vec![Commande {
            programme: "/usr/bin/systemctl",
            args: &["--no-ask-password", "poweroff"],
        }],
        "une seule commande, la première présente, argv exact"
    );
}

#[tokio::test]
async fn linux_sans_systemd_passe_par_loginctl() {
    let faux = Faux::new(&["/bin/loginctl"]);
    assert_eq!(eteindre_avec(hote(Plateforme::Linux), &faux).await, Ok(()));
    assert_eq!(faux.lances()[0].programme, "/bin/loginctl");
    assert_eq!(faux.lances()[0].args, &["--no-ask-password", "poweroff"]);
}

#[tokio::test]
async fn polkit_refuse_donne_un_message_clair_et_s_arrete_la() {
    let faux = Faux::new(&["/usr/bin/systemctl", "/usr/bin/loginctl"]).rend(
        "/usr/bin/systemctl",
        1,
        "Call to PowerOff failed: Access denied",
    );
    let refus = eteindre_avec(hote(Plateforme::Linux), &faux)
        .await
        .unwrap_err();
    assert_eq!(refus.code(), "polkit_denied", "{refus:?}");
    let message = refus.message();
    assert!(message.contains("polkit"), "{message}");
    assert!(
        message.contains("org.freedesktop.login1.power-off"),
        "{message}"
    );
    assert!(
        message.contains("docs/ARRET-PROGRAMME-EXTINCTION.md"),
        "le message renvoie à la règle documentée : {message}"
    );
    assert!(
        message.contains("Access denied"),
        "le détail reste : {message}"
    );
    assert_eq!(faux.lances().len(), 1, "un refus de droits n'insiste pas");

    // « Interactive authentication required » : même refus.
    let faux = Faux::new(&["/usr/bin/systemctl"]).rend(
        "/usr/bin/systemctl",
        1,
        "Failed to power off system via logind: Interactive authentication required.",
    );
    assert_eq!(
        eteindre_avec(hote(Plateforme::Linux), &faux)
            .await
            .unwrap_err()
            .code(),
        "polkit_denied"
    );
}

#[tokio::test]
async fn systemctl_sans_systemd_laisse_sa_chance_a_loginctl() {
    let faux = Faux::new(&["/usr/bin/systemctl", "/usr/bin/loginctl"]).rend(
        "/usr/bin/systemctl",
        1,
        "System has not been booted with systemd as init system (PID 1). Can't operate.",
    );
    assert_eq!(eteindre_avec(hote(Plateforme::Linux), &faux).await, Ok(()));
    let lances: Vec<_> = faux.lances().iter().map(|c| c.programme).collect();
    assert_eq!(lances, vec!["/usr/bin/systemctl", "/usr/bin/loginctl"]);
}

#[tokio::test]
async fn macos_et_windows_par_l_api_systeme_en_argv_fixe() {
    let faux = Faux::new(&["/usr/bin/osascript"]);
    assert_eq!(eteindre_avec(hote(Plateforme::MacOs), &faux).await, Ok(()));
    assert_eq!(
        faux.lances(),
        vec![Commande {
            programme: "/usr/bin/osascript",
            args: &["-e", "tell application \"System Events\" to shut down"],
        }]
    );
    let faux = Faux::new(&[r"C:\Windows\System32\shutdown.exe"]);
    assert_eq!(
        eteindre_avec(hote(Plateforme::Windows), &faux).await,
        Ok(())
    );
    assert_eq!(faux.lances()[0].args, &["/s", "/t", "0"]);
}

#[tokio::test]
async fn macos_et_windows_disent_l_autorisation_qui_manque() {
    let faux = Faux::new(&["/usr/bin/osascript"]).rend(
        "/usr/bin/osascript",
        1,
        "execution error: Not authorized to send Apple events to System Events. (-1743)",
    );
    let refus = eteindre_avec(hote(Plateforme::MacOs), &faux)
        .await
        .unwrap_err();
    assert_eq!(refus.code(), "macos_not_authorized");
    assert!(refus.message().contains("Automation"));

    let faux = Faux::new(&[r"C:\Windows\System32\shutdown.exe"]).rend(
        r"C:\Windows\System32\shutdown.exe",
        5,
        "Accès refusé.(5)",
    );
    let refus = eteindre_avec(hote(Plateforme::Windows), &faux)
        .await
        .unwrap_err();
    assert_eq!(refus.code(), "windows_privilege");
}

#[tokio::test]
async fn dans_un_conteneur_rien_n_est_lance() {
    let faux = Faux::new(&["/usr/bin/systemctl"]);
    let contexte = Contexte {
        plateforme: Plateforme::Linux,
        conteneur: Some("docker"),
    };
    let refus = eteindre_avec(contexte, &faux).await.unwrap_err();
    assert_eq!(refus, Refus::Conteneur("docker"));
    assert!(
        faux.lances().is_empty(),
        "aucune commande dans un conteneur"
    );
    assert!(!contexte.disponible());
    assert_eq!(mode_avec(contexte, false), None);
    assert_eq!(
        mode_avec(contexte, true),
        None,
        "un TUNE_APPLIANCE=1 dans l'image ne rouvre pas l'extinction"
    );
}

#[tokio::test]
async fn sans_commande_presente_ou_systeme_inconnu_rien_ne_part() {
    let faux = Faux::new(&[]);
    assert_eq!(
        eteindre_avec(hote(Plateforme::Linux), &faux).await,
        Err(Refus::NonPrisEnCharge)
    );
    assert_eq!(
        eteindre_avec(hote(Plateforme::Autre), &faux).await,
        Err(Refus::NonPrisEnCharge)
    );
    assert!(faux.lances().is_empty());
    assert!(!hote(Plateforme::Autre).disponible());
}

#[test]
fn le_mode_suit_tune_os_puis_l_hote() {
    assert_eq!(mode_avec(hote(Plateforme::Linux), true), Some(Mode::TuneOs));
    assert_eq!(mode_avec(hote(Plateforme::Linux), false), Some(Mode::Hote));
    assert_eq!(mode_avec(hote(Plateforme::MacOs), false), Some(Mode::Hote));
    assert_eq!(
        mode_avec(hote(Plateforme::Windows), false),
        Some(Mode::Hote)
    );
    assert_eq!(mode_avec(hote(Plateforme::Autre), false), None);
}

/// Toutes les commandes sont absolues, sans shell ni sudo.
#[test]
fn aucune_commande_ne_passe_par_un_shell_ou_sudo() {
    for p in [Plateforme::Linux, Plateforme::MacOs, Plateforme::Windows] {
        for c in commandes(p) {
            assert!(
                c.programme.starts_with('/') || c.programme.starts_with(r"C:\"),
                "{c:?}"
            );
            for interdit in ["sh", "bash", "zsh", "cmd.exe", "powershell", "sudo", "doas"] {
                let nom = c.programme.rsplit(['/', '\\']).next().unwrap();
                assert_ne!(nom, interdit, "{c:?}");
                assert!(!c.args.contains(&interdit), "{c:?}");
            }
        }
    }
}

// ─── À l'échéance : mode rangé et authentification ────────────────────

#[test]
fn hote_sans_authentification_active_rien_ne_part() {
    assert_eq!(
        decider(Some(Mode::Hote), Some("host"), false),
        Err(Refus::AuthentificationRequise)
    );
    assert_eq!(
        decider(Some(Mode::Hote), Some("host"), true),
        Ok(Mode::Hote)
    );
    // Tune OS garde son comportement : l'authentification n'y est pas exigée.
    assert_eq!(
        decider(Some(Mode::TuneOs), Some("tune_os"), false),
        Ok(Mode::TuneOs)
    );
    assert_eq!(Refus::AuthentificationRequise.code(), "auth_required");
    assert!(
        Refus::AuthentificationRequise
            .message()
            .contains("enable authentication")
    );
}

/// Un réglage restauré ou importé : sans mode rangé, ou avec le mode d'une
/// autre machine, l'échéance n'éteint rien.
#[test]
fn un_mode_range_different_du_mode_courant_refuse() {
    for (courant, range) in [
        (Some(Mode::Hote), None),
        (Some(Mode::Hote), Some("tune_os")),
        (Some(Mode::TuneOs), Some("host")),
        (Some(Mode::TuneOs), None),
        (None, Some("host")),
    ] {
        let refus = decider(courant, range, true).unwrap_err();
        assert_eq!(refus.code(), "mode_mismatch", "{courant:?} / {range:?}");
    }
    // Conteneur et réglage sans mode : indisponible, pas d'extinction.
    assert!(decider(None, None, true).is_err());
}

/// La porte réelle, avec un réglage importé SANS mode rangé : elle refuse
/// avant de lancer quoi que ce soit, quelle que soit la machine de test
/// (hôte, Tune OS ou conteneur).
#[tokio::test]
async fn la_porte_refuse_un_reglage_importe_sans_mode() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let refus = extinction_programmee(state.backend.clone(), None)
        .await
        .unwrap_err();
    assert!(
        ["mode_mismatch", "container", "unsupported"].contains(&refus.code.as_str()),
        "{refus:?}"
    );
}
