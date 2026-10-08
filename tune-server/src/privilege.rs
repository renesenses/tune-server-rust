//! Opérations privilégiées d'un serveur qui ne tourne PAS en root (#3206).
//!
//! L'image Tune OS lance désormais `tune.service` sous le compte `tune`. Trois
//! familles d'actions restent réservées à root : monter et démonter un
//! partage SMB (`mount.cifs`, `umount`), et écrire puis activer une unité de
//! montage dans `/etc/systemd/system` (stockage de l'appliance). Le Wi-Fi
//! (`nmcli`) et l'extinction (`systemctl poweroff`) passent par polkit, sans
//! rien changer ici.
//!
//! Sous un autre compte que root, ces actions passent par
//! `sudo -n /usr/local/libexec/tune-os-privilege <action> …`, un assistant
//! installé par l'image et seul programme autorisé par
//! `/etc/sudoers.d/tune`. Ce n'est pas `mount.cifs` lui-même que l'on autorise,
//! et c'est voulu : `mount.cifs` exécuté par root accepte n'importe quel point
//! de montage (un lien symbolique vers `/etc` compris) et n'importe quelle
//! option (`suid`, `uid=0`). Aucun motif de sudoers ne borne cela ;
//! l'assistant, lui, vérifie le point de montage, impose `nosuid,nodev,noexec`
//! et le propriétaire, et écrit lui-même le contenu des unités.
//!
//! Le mot de passe d'un partage ne passe JAMAIS sur la ligne de commande dans
//! ce cas : sudo journalise la ligne complète. Il est écrit sur l'entrée
//! standard de l'assistant.
//!
//! `-n` est indispensable : sans règle sudoers, sudo demanderait un mot de
//! passe que personne ne tapera, et la requête HTTP resterait pendue. Avec
//! `-n`, il refuse immédiatement, et ce refus est reconnu
//! ([`est_un_refus_d_elevation`]) pour être journalisé tel quel.
//!
//! En root (images historiques, installations manuelles), rien ne change :
//! les commandes d'origine sont lancées directement.

use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// L'assistant root installé par l'image Tune OS.
pub const ASSISTANT: &str = "/usr/local/libexec/tune-os-privilege";

/// sudo, par son chemin absolu : jamais résolu par le `PATH` du service.
pub const SUDO: &str = "/usr/bin/sudo";

/// Le sudo que lancent les routes : [`SUDO`], sauf si `TUNE_SUDO_BIN` en
/// désigne un autre.
///
/// Même mécanisme que `TUNE_SYSTEMCTL_BIN` ou `TUNE_NMCLI_BIN` : il sert aux
/// tests d'intégration, qui ne tournent ni en root ni avec l'assistant
/// installé, et y mettent un faux sudo jouant l'assistant. Ce faux vérifie
/// la commande reçue et rejoue l'action ; le flux complet hors root reste
/// ainsi éprouvé de bout en bout. Tune OS ne pose pas cette variable : le
/// service lance `/usr/bin/sudo`, comme avant. Elle n'ouvre rien de plus
/// que les autres `TUNE_*_BIN` : qui fixe l'environnement du service choisit
/// déjà, en root, le `systemctl` qu'il exécute.
pub fn sudo() -> String {
    std::env::var("TUNE_SUDO_BIN")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SUDO.to_string())
}

/// UID effectif du processus.
pub fn euid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid() ne peut pas échouer et ne touche à aucune mémoire.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Une commande prête à lancer, et ce qu'elle lit sur son entrée standard.
///
/// Séparée de son lancement pour que les tests puissent vérifier la
/// commande construite (programme, arguments, absence du mot de passe) sans
/// rien exécuter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commande {
    pub programme: String,
    pub args: Vec<String>,
    /// Écrit sur l'entrée standard, puis fermé. `None` : entrée vide.
    pub entree: Option<String>,
}

impl Commande {
    /// Une commande lancée telle quelle (service en root).
    pub fn directe(programme: &str, args: &[&str]) -> Self {
        Self {
            programme: programme.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            entree: None,
        }
    }

    /// `sudo -n ASSISTANT <action> <args…>` (service sous un autre compte).
    ///
    /// `sudo` est un paramètre pour les tests : un faux sudo y simule le refus
    /// ou l'accord sans toucher au vrai.
    pub fn par_l_assistant(sudo: &str, action: &str, args: &[&str]) -> Self {
        let mut tous = vec!["-n".to_string(), ASSISTANT.to_string(), action.to_string()];
        tous.extend(args.iter().map(|a| a.to_string()));
        Self {
            programme: sudo.to_string(),
            args: tous,
            entree: None,
        }
    }

    pub fn avec_entree(mut self, entree: &str) -> Self {
        self.entree = Some(entree.to_string());
        self
    }

    /// Passe-t-elle par sudo ?
    pub fn est_elevee(&self) -> bool {
        self.args.first().map(String::as_str) == Some("-n")
            && self.args.get(1).map(String::as_str) == Some(ASSISTANT)
    }

    /// Lance la commande et attend sa fin (stdout et stderr capturés).
    pub async fn lancer(&self) -> std::io::Result<Output> {
        let mut cmd = Command::new(&self.programme);
        cmd.args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match &self.entree {
            None => {
                cmd.stdin(Stdio::null());
                cmd.output().await
            }
            Some(texte) => {
                cmd.stdin(Stdio::piped());
                let mut enfant = cmd.spawn()?;
                if let Some(mut stdin) = enfant.stdin.take() {
                    // Un assistant qui sort sans lire (refus de sudo) ferme le
                    // tube : l'erreur d'écriture n'est pas celle à rapporter,
                    // son code de sortie et son stderr le sont.
                    let _ = stdin.write_all(texte.as_bytes()).await;
                    let _ = stdin.write_all(b"\n").await;
                    drop(stdin);
                }
                enfant.wait_with_output().await
            }
        }
    }
}

/// Le stderr d'une commande élevée est-il un refus de sudo lui-même (et non
/// une erreur de l'action) ?
///
/// sudo préfixe tous ses messages par « sudo: » : `a password is required`
/// (pas de règle NOPASSWD), `a terminal is required`, `… command not found`
/// (assistant absent), ou « Sorry, user tune is not allowed to execute … ».
/// L'assistant, lui, préfixe les siens par « tune-os-privilege: ».
pub fn est_un_refus_d_elevation(stderr: &str) -> bool {
    let t = stderr.trim_start();
    t.starts_with("sudo:") || t.starts_with("Sorry, user ")
}

/// Texte d'erreur d'une élévation refusée, pour l'utilisateur et le journal.
pub fn message_de_refus(stderr: &str) -> String {
    format!(
        "élévation refusée par sudo ({}) : le service ne tourne pas en root et \
         {ASSISTANT} n'est pas autorisé par /etc/sudoers.d/tune",
        stderr.trim()
    )
}

/// Écrit un script exécutable pour un test, et rend son chemin.
///
/// L'écriture passe par un `sh` enfant, jamais par ce processus : un
/// descripteur ouvert en écriture ici serait hérité par les `fork` des tests
/// voisins, et l'exécution du script échouerait alors, de temps en temps, par
/// `ETXTBSY` (« Text file busy »).
#[cfg(all(test, unix))]
pub(crate) fn script_de_test(chemin: &std::path::Path, corps: &str) -> String {
    let statut = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(chemin)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut enfant| {
            use std::io::Write;
            let mut entree = enfant.stdin.take().expect("stdin");
            entree.write_all(format!("#!/bin/sh\n{corps}\n").as_bytes())?;
            drop(entree);
            enfant.wait()
        })
        .expect("écriture du script de test");
    assert!(statut.success(), "écriture de {}", chemin.display());
    chemin.to_string_lossy().into_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Un faux sudo : un script shell écrit dans un dossier temporaire.
    fn faux_sudo(dossier: &std::path::Path, corps: &str) -> String {
        script_de_test(&dossier.join("sudo"), corps)
    }

    #[test]
    fn la_commande_elevee_est_sudo_n_assistant_action() {
        let c = Commande::par_l_assistant(SUDO, "smb-umount", &["/mnt/nas"]);
        assert_eq!(c.programme, "/usr/bin/sudo");
        assert_eq!(c.args, vec!["-n", ASSISTANT, "smb-umount", "/mnt/nas"]);
        assert!(c.est_elevee());
        assert!(!Commande::directe("umount", &["/mnt/nas"]).est_elevee());
    }

    #[test]
    fn les_refus_de_sudo_sont_reconnus() {
        for refus in [
            "sudo: a password is required",
            "sudo: a terminal is required to read the password",
            "sudo: /usr/local/libexec/tune-os-privilege: command not found",
            "Sorry, user tune is not allowed to execute '/usr/local/libexec/tune-os-privilege smb-umount /mnt/x' as root on tune-1234.",
        ] {
            assert!(est_un_refus_d_elevation(refus), "{refus}");
        }
        for erreur in [
            "mount error(13): Permission denied",
            "tune-os-privilege: point de montage refusé : /etc",
            "",
        ] {
            assert!(!est_un_refus_d_elevation(erreur), "{erreur}");
        }
    }

    /// Refus simulé : sudo sans règle NOPASSWD, appelé avec `-n`.
    #[tokio::test]
    async fn un_refus_de_sudo_remonte_son_message_et_un_code_non_nul() {
        let tmp = tempfile::tempdir().unwrap();
        let sudo = faux_sudo(
            tmp.path(),
            "echo 'sudo: a password is required' >&2\nexit 1",
        );
        let sortie = Commande::par_l_assistant(&sudo, "smb-umount", &["/mnt/nas"])
            .lancer()
            .await
            .unwrap();
        assert!(!sortie.status.success());
        let stderr = String::from_utf8_lossy(&sortie.stderr);
        assert!(est_un_refus_d_elevation(&stderr), "{stderr}");
        assert!(message_de_refus(&stderr).contains("a password is required"));
    }

    /// Accord simulé : le faux sudo vérifie `-n`, l'assistant, l'action, et
    /// que le mot de passe arrive sur l'entrée standard, jamais en argument.
    #[tokio::test]
    async fn un_accord_de_sudo_recoit_le_secret_sur_l_entree_seulement() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = tmp.path().join("argv");
        let sudo = faux_sudo(
            tmp.path(),
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\n\
                 [ \"$1\" = -n ] || exit 90\n\
                 [ \"$2\" = '{ASSISTANT}' ] || exit 91\n\
                 [ \"$3\" = smb-mount ] || exit 92\n\
                 read -r secret\n\
                 [ \"$secret\" = 's3cr,et' ] || exit 93\n\
                 echo monte",
                trace.display()
            ),
        );
        let sortie = Commande::par_l_assistant(
            &sudo,
            "smb-mount",
            &["//nas/musique", "/mnt/nas_musique", "invite", "negocie"],
        )
        .avec_entree("s3cr,et")
        .lancer()
        .await
        .unwrap();
        assert!(
            sortie.status.success(),
            "code {:?}, stderr {}",
            sortie.status.code(),
            String::from_utf8_lossy(&sortie.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&sortie.stdout).trim(), "monte");
        let argv = std::fs::read_to_string(&trace).unwrap();
        assert!(
            !argv.contains("s3cr,et"),
            "le secret est sur la ligne de commande : {argv}"
        );
    }

    /// Une commande directe (root) n'a pas d'entrée : elle ne doit pas
    /// attendre un tube jamais fermé.
    #[tokio::test]
    async fn une_commande_directe_sans_entree_ne_bloque_pas() {
        let sortie = Commande::directe("/bin/sh", &["-c", "cat; echo fini"])
            .lancer()
            .await
            .unwrap();
        assert!(sortie.status.success());
        assert_eq!(String::from_utf8_lossy(&sortie.stdout).trim(), "fini");
    }
}
