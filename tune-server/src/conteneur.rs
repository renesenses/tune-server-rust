//! Tune tourne-t-il dans un conteneur ? (renesenses/tune-server-rust#7074)
//!
//! « Redémarrer le serveur » n'a pas le même sens dans un conteneur. Le
//! gestionnaire de `POST /system/restart` relance le binaire EN PLACE
//! (`execv`, même PID) : dans l'image Docker, le point d'entrée fait `exec`
//! du serveur, qui est donc le PID 1 — le conteneur reste debout. Mais si
//! l'`execv` échoue, le processus se termine, et le conteneur avec lui : il ne
//! revient que si l'hôte lui a donné une politique de redémarrage
//! (`restart: unless-stopped` dans le `docker-compose.yml` fourni). Même chose
//! pour « Arrêter » : le processus sort avec le code 0, et `unless-stopped`
//! relance quand même le conteneur.
//!
//! Le client web le DIT à l'utilisateur, d'où ce signal : `GET /system/env`
//! rend `container` (`"docker"`, `"podman"`, `"kubernetes"`, `"container"` ou
//! `null`). La détection est une fonction pure sur ce qu'elle lit, pour se
//! tester sans conteneur.

/// Ce que la détection lit sur la machine.
#[derive(Debug, Default, Clone)]
pub struct Indices<'a> {
    /// `/.dockerenv` existe (Docker, et la plupart des moteurs compatibles).
    pub dockerenv: bool,
    /// `/run/.containerenv` existe (Podman).
    pub containerenv: bool,
    /// Contenu de `/proc/1/cgroup`.
    pub cgroup: &'a str,
    /// `KUBERNETES_SERVICE_HOST` est posée.
    pub kubernetes: bool,
}

/// Le moteur de conteneur, ou `None` sur une machine ordinaire.
pub fn moteur(i: &Indices<'_>) -> Option<&'static str> {
    if i.containerenv {
        return Some("podman");
    }
    if i.kubernetes || i.cgroup.contains("kubepods") {
        return Some("kubernetes");
    }
    if i.dockerenv || i.cgroup.contains("docker") {
        return Some("docker");
    }
    if i.cgroup.contains("containerd") || i.cgroup.contains("/lxc/") {
        return Some("container");
    }
    None
}

/// Le moteur de conteneur de CE processus, lu sur la machine.
pub fn detecter() -> Option<&'static str> {
    let cgroup = std::fs::read_to_string("/proc/1/cgroup").unwrap_or_default();
    moteur(&Indices {
        dockerenv: std::path::Path::new("/.dockerenv").exists(),
        containerenv: std::path::Path::new("/run/.containerenv").exists(),
        cgroup: &cgroup,
        kubernetes: std::env::var_os("KUBERNETES_SERVICE_HOST").is_some(),
    })
}

/// Pour la PREMIÈRE CONFIGURATION seulement (décision de sécurité) : un
/// conteneur est-il PROBABLE ? Plus large que [`moteur`], qui sert au reste du
/// serveur (persistance, extinction, partages) et ne change pas : ajoute
/// LXC/Incus, systemd-nspawn, Podman (`libpod`), `/run/systemd/container`,
/// `container=` dans l'environnement du processus 1, et un cgroup v2 à la
/// racine (`0::/`, espace de noms cgroup privé — mais aussi un Linux sans
/// systemd : dans le doute, conteneur).
pub fn conteneur_probable(
    i: &Indices<'_>,
    systemd_container: bool,
    environ_container: bool,
) -> bool {
    moteur(i).is_some()
        || systemd_container
        || environ_container
        || [
            "/lxc",
            "lxc.payload",
            "machine.slice",
            "libpod",
            "containerd",
        ]
        .iter()
        .any(|m| i.cgroup.contains(m))
        || i.cgroup.lines().any(|l| l.trim() == "0::/")
}

/// `/proc/1/environ` porte-t-il `container=` ?
fn environ_container(environ: &[u8]) -> bool {
    environ
        .split(|o| *o == 0)
        .any(|v| v.starts_with(b"container=") && v.len() > b"container=".len())
}

/// Pour la première configuration : ce processus est-il (probablement) dans
/// un conteneur ? [`conteneur_probable`] sur la machine, plus
/// `systemd-detect-virt -c` s'il existe ; sous Linux, une table des cgroups
/// illisible compte comme un conteneur. Lu une fois.
pub fn dans_un_conteneur() -> bool {
    static REPONSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REPONSE.get_or_init(|| {
        let cgroup = std::fs::read_to_string("/proc/1/cgroup");
        if cfg!(target_os = "linux") && cgroup.is_err() {
            return true;
        }
        let cgroup = cgroup.unwrap_or_default();
        let indices = Indices {
            dockerenv: std::path::Path::new("/.dockerenv").exists(),
            containerenv: std::path::Path::new("/run/.containerenv").exists(),
            cgroup: &cgroup,
            kubernetes: std::env::var_os("KUBERNETES_SERVICE_HOST").is_some(),
        };
        let environ = std::fs::read("/proc/1/environ")
            .map(|e| environ_container(&e))
            .unwrap_or(false);
        if conteneur_probable(
            &indices,
            std::path::Path::new("/run/systemd/container").exists(),
            environ,
        ) {
            return true;
        }
        if cfg!(target_os = "linux")
            && let Ok(sortie) = std::process::Command::new("systemd-detect-virt")
                .arg("-c")
                .output()
        {
            let nom = String::from_utf8_lossy(&sortie.stdout).trim().to_string();
            if sortie.status.success() && !nom.is_empty() && nom != "none" {
                return true;
            }
        }
        false
    })
}

/// Le nom sous lequel `docker restart` retrouve CE conteneur : le nom d'hôte,
/// que Docker fixe à l'identifiant court (12 hexadécimaux) tant qu'on ne l'a
/// pas changé. Tout autre nom d'hôte est inutilisable ici : `None`.
pub fn identifiant_court(hostname: &str) -> Option<&str> {
    let h = hostname.trim();
    (h.len() == 12 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Contre-épreuve : retirer le bras `dockerenv` de `moteur` fait rougir
    /// `docker_se_reconnait_a_dockerenv`.
    #[test]
    fn docker_se_reconnait_a_dockerenv() {
        let i = Indices {
            dockerenv: true,
            ..Default::default()
        };
        assert_eq!(moteur(&i), Some("docker"));
    }

    #[test]
    fn docker_se_reconnait_au_cgroup_sans_dockerenv() {
        let i = Indices {
            cgroup: "12:pids:/docker/3f2a9c1b7d44\n",
            ..Default::default()
        };
        assert_eq!(moteur(&i), Some("docker"));
    }

    #[test]
    fn podman_et_kubernetes_sont_nommes() {
        let p = Indices {
            containerenv: true,
            dockerenv: true,
            ..Default::default()
        };
        assert_eq!(moteur(&p), Some("podman"));
        let k = Indices {
            cgroup: "0::/kubepods/burstable/pod1\n",
            ..Default::default()
        };
        assert_eq!(moteur(&k), Some("kubernetes"));
    }

    /// Le reste du serveur ne change pas : un Linux sans systemd (`0::/`)
    /// n'est pas un conteneur pour `moteur`.
    #[test]
    fn moteur_ne_voit_pas_de_conteneur_a_la_racine_des_cgroups() {
        let i = Indices {
            cgroup: "0::/\n",
            ..Default::default()
        };
        assert_eq!(moteur(&i), None);
    }

    #[test]
    fn conteneurs_sans_marqueur_docker() {
        for cgroup in [
            "0::/\n",
            "0::/lxc.payload.incus-1\n",
            "0::/machine.slice/systemd-nspawn@deb.service/payload\n",
            "0::/system.slice/containerd.service\n",
            "0::/machine.slice/libpod-3f2a.scope\n",
        ] {
            let i = Indices {
                cgroup,
                ..Default::default()
            };
            assert!(conteneur_probable(&i, false, false), "{cgroup}");
        }
        assert!(conteneur_probable(&Indices::default(), true, false));
        assert!(conteneur_probable(&Indices::default(), false, true));
        let systemd = Indices {
            cgroup: "0::/init.scope\n",
            ..Default::default()
        };
        assert!(!conteneur_probable(&systemd, false, false));
        assert!(!conteneur_probable(&Indices::default(), false, false));
        assert!(environ_container(b"PATH=/bin\0container=lxc\0"));
        assert!(!environ_container(b"PATH=/bin\0HOME=/root\0"));
        assert!(!environ_container(b"container=\0"));
    }

    #[test]
    fn une_machine_ordinaire_n_est_pas_un_conteneur() {
        let i = Indices {
            cgroup: "0::/user.slice/user-1000.slice/session-3.scope\n",
            ..Default::default()
        };
        assert_eq!(moteur(&i), None);
        assert_eq!(moteur(&Indices::default()), None);
    }

    #[test]
    fn l_identifiant_court_n_est_donne_que_s_il_en_est_un() {
        assert_eq!(identifiant_court("3f2a9c1b7d44\n"), Some("3f2a9c1b7d44"));
        assert_eq!(identifiant_court("shrek"), None);
        assert_eq!(identifiant_court("tune-server"), None);
        assert_eq!(identifiant_court("zzzzzzzzzzzz"), None);
    }
}
