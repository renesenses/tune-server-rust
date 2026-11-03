//! #6253 — lancer Tune Server sous Windows SANS fenêtre de terminal.
//!
//! Demande de DidLyon (forum, fil 2257) : sous Windows, le serveur tournait
//! dans une fenêtre de console. Peu esthétique, et la fermer par mégarde
//! arrêtait le serveur. Le testeur s'en était sorti avec deux VBScript
//! (lancement caché, arrêt).
//!
//! # Ce que fait `--sans-console`
//!
//! `tune-server.exe --sans-console` ne sert rien lui-même : il relance le même
//! exécutable, avec les mêmes arguments moins ce drapeau, dans un processus
//! **sans fenêtre** (`CREATE_NO_WINDOW`) et dans son propre groupe
//! (`CREATE_NEW_PROCESS_GROUP` : un Ctrl+C dans la console d'origine ne
//! l'atteint pas), puis sort. Le processus détaché porte
//! [`MARQUEUR_ENFANT`] dans son environnement : il ne se relance pas une
//! seconde fois, et la mise à jour sait qu'elle doit le relancer de la même
//! façon (`update.rs`, `windows_update_bat`).
//!
//! Pas de VBScript : Windows 11 retire VBScript par étapes (fonctionnalité à
//! la demande depuis 24H2). Pas de second exécutable non plus : rien à
//! ajouter à la chaîne de build et de signature.
//!
//! # Les journaux restent accessibles
//!
//! Le serveur écrit TOUJOURS son journal dans un fichier, console ou pas
//! (`bootstrap::installer_le_journal`) : sous Windows,
//! `%LOCALAPPDATA%\TuneServer\tune-server.log`, borné à 10 Mio plus une
//! sauvegarde `.1`. C'est lui que relisent « Exporter le journal » et le
//! rapport de bogue de l'interface web. Le lanceur affiche ce chemin avant de
//! rendre la main.
//!
//! # Ce que cela ne change pas
//!
//! Sans le drapeau, rien ne change, sur aucun système. Hors Windows, le
//! drapeau est sans effet (le serveur sert dans le processus lancé) : un
//! service systemd ou un `.app` n'ont pas de fenêtre de console à cacher.

/// Le drapeau de ligne de commande.
pub(crate) const DRAPEAU: &str = "--sans-console";

/// Posé dans l'environnement du processus détaché.
pub(crate) const MARQUEUR_ENFANT: &str = "TUNE_SANS_CONSOLE_ENFANT";

/// `CREATE_NO_WINDOW` : un processus console exécuté sans fenêtre de console.
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `CREATE_NEW_PROCESS_GROUP` : le Ctrl+C de la console du lanceur ne
/// l'atteint pas.
pub(crate) const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Les drapeaux de création du processus détaché.
pub(crate) const DRAPEAUX_DE_CREATION: u32 = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;

/// Ce que le démarrage doit faire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Servir dans ce processus — le cas de toujours.
    Servir,
    /// Relancer l'exécutable sans fenêtre avec ces arguments, puis sortir.
    Detacher { args: Vec<String> },
}

/// La décision, sans effet de bord.
///
/// `enfant` : ce processus est-il déjà le détaché ([`MARQUEUR_ENFANT`] à `1`) ?
/// `windows` : le système courant, en paramètre pour que les témoins éprouvent
/// les deux branches sur n'importe quelle machine.
pub(crate) fn decider(args: &[String], enfant: bool, windows: bool) -> Decision {
    if !windows || enfant || !args.iter().any(|a| a == DRAPEAU) {
        return Decision::Servir;
    }
    Decision::Detacher {
        args: args.iter().filter(|a| *a != DRAPEAU).cloned().collect(),
    }
}

/// Ce processus est-il le serveur détaché, sans fenêtre ?
pub(crate) fn est_l_enfant_detache() -> bool {
    std::env::var(MARQUEUR_ENFANT).ok().as_deref() == Some("1")
}

/// Le message que le lanceur affiche avant de rendre la main.
pub(crate) fn message_de_detachement(pid: u32, journal: &std::path::Path) -> String {
    format!(
        "Tune Server tourne en arrière-plan, sans fenêtre (processus {pid}).\n\
         Journal : {}\n\
         Pour l'arrêter : « Arrêter le serveur » dans l'interface web, ou le \
         raccourci « Quitter Tune Server » du menu Démarrer.",
        journal.display()
    )
}

/// Relance l'exécutable courant sans fenêtre, et rend le PID du détaché.
#[cfg(windows)]
pub(crate) fn detacher(args: &[String]) -> std::io::Result<u32> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe()?;
    let enfant = std::process::Command::new(exe)
        .args(args)
        .env(MARQUEUR_ENFANT, "1")
        .creation_flags(DRAPEAUX_DE_CREATION)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(enfant.id())
}

/// Au démarrage : relancer sans fenêtre si on le demande, et sortir.
///
/// Ne rend la main que si ce processus doit servir lui-même.
pub(crate) fn appliquer_au_demarrage() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let decision = decider(&args, est_l_enfant_detache(), cfg!(windows));
    let Decision::Detacher { args } = decision else {
        return;
    };
    #[cfg(windows)]
    {
        match detacher(&args) {
            Ok(pid) => {
                println!(
                    "{}",
                    message_de_detachement(pid, &crate::config::default_log_file_path())
                );
                std::process::exit(0);
            }
            Err(e) => {
                // Mieux vaut une fenêtre qu'aucun serveur : on sert ici.
                eprintln!(
                    "tune-server: lancement sans fenêtre impossible ({e}) — le serveur démarre dans cette console"
                );
            }
        }
    }
    #[cfg(not(windows))]
    let _ = args;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sans_le_drapeau_rien_ne_change() {
        assert_eq!(decider(&a(&[]), false, true), Decision::Servir);
        assert_eq!(
            decider(&a(&["--port", "8888"]), false, true),
            Decision::Servir
        );
    }

    #[test]
    fn le_drapeau_sous_windows_detache_avec_les_autres_arguments() {
        assert_eq!(
            decider(&a(&["--port", "8888", DRAPEAU]), false, true),
            Decision::Detacher {
                args: a(&["--port", "8888"])
            }
        );
    }

    #[test]
    fn le_detache_ne_se_relance_jamais_lui_meme() {
        // Sans cette garde, chaque détaché en lancerait un autre : une chaîne
        // de processus sans fin, tous sans fenêtre, donc invisibles.
        assert_eq!(decider(&a(&[DRAPEAU]), true, true), Decision::Servir);
    }

    #[test]
    fn hors_windows_le_drapeau_est_sans_effet() {
        assert_eq!(decider(&a(&[DRAPEAU]), false, false), Decision::Servir);
    }

    #[test]
    fn le_processus_detache_n_a_ni_fenetre_ni_le_ctrl_c_du_lanceur() {
        assert_eq!(DRAPEAUX_DE_CREATION & CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(DRAPEAUX_DE_CREATION & CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
        // DETACHED_PROCESS (0x8) ferait ignorer CREATE_NO_WINDOW et laisserait
        // le serveur sans console du tout : ses enfants console (pare-feu,
        // ffmpeg) en ouvriraient alors une, visible.
        assert_eq!(DRAPEAUX_DE_CREATION & 0x0000_0008, 0);
    }

    #[test]
    fn le_message_dit_ou_lire_le_journal() {
        let m = message_de_detachement(
            42,
            std::path::Path::new(r"C:\Users\x\AppData\Local\TuneServer\tune-server.log"),
        );
        assert!(m.contains("tune-server.log"), "{m}");
        assert!(m.contains("42"), "{m}");
    }
}

/// L'installeur Windows (NSIS, écrit dans `release.yml`) doit poser le
/// lanceur sans fenêtre À CÔTÉ de l'ancien, sans toucher à ce dernier.
#[cfg(test)]
mod installeur_tests {
    fn installeur() -> String {
        let chemin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.github/workflows/release.yml");
        std::fs::read_to_string(&chemin).expect("release.yml lisible")
    }

    /// Une instruction ACTIVE du script NSIS qui commence par `debut` : une
    /// ligne en commentaire (`; …`) ne compte pas.
    fn instruction(yml: &str, debut: &str) -> bool {
        yml.lines().any(|l| l.trim_start().starts_with(debut))
    }

    #[test]
    fn l_installeur_pose_le_lanceur_sans_console_6253() {
        let yml = installeur();
        assert!(
            instruction(
                &yml,
                "FileWrite `$0 'tune-server.exe --sans-console$\\r$\\n'"
            ),
            "le lanceur sans fenêtre doit passer `--sans-console`"
        );
        assert!(
            instruction(
                &yml,
                "CreateShortcut \"`$SMPROGRAMS\\Tune Server\\Tune Server (sans console).lnk\" \
                 \"`$INSTDIR\\start-tune-server-sans-console.bat\" \"\" \"`$INSTDIR\\tune-server.exe\" 0 SW_SHOWMINIMIZED"
            ),
            "raccourci du menu Démarrer, console réduite le temps du détachement"
        );
        assert!(
            instruction(
                &yml,
                "CreateShortcut \"`$SMPROGRAMS\\Tune Server\\Quitter Tune Server.lnk\" \"`$SYSDIR\\taskkill.exe\" \"/F /IM tune-server.exe\""
            ),
            "un serveur sans fenêtre doit pouvoir s'arrêter sans le Gestionnaire des tâches"
        );
    }

    #[test]
    fn les_raccourcis_d_avant_ne_changent_pas_6253() {
        let yml = installeur();
        assert!(instruction(
            &yml,
            "CreateShortcut \"`$DESKTOP\\Tune Server.lnk\" \"`$INSTDIR\\start-tune-server.bat\""
        ));
        assert!(instruction(
            &yml,
            "CreateShortcut \"`$SMPROGRAMS\\Tune Server\\Tune Server.lnk\" \"`$INSTDIR\\start-tune-server.bat\""
        ));
        assert!(instruction(&yml, "FileWrite `$0 'tune-server.exe$\\r$\\n'"));
    }
}
