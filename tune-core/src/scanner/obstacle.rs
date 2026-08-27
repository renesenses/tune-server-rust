//! Nommer un obstacle de scan : une cause compréhensible, et le chemin en cause.
//!
//! ## Le défaut
//!
//! Une racine de bibliothèque que `read_dir` refuse était rendue à l'écran
//! par un `format!("{}: {:?} — {}", chemin, e.kind(), e)`. Le `{:?}` d'un
//! [`std::io::ErrorKind`] n'est pas un message : c'est le nom d'une variante
//! de la bibliothèque standard, en anglais, et — pour tout errno que Rust ne
//! mappe pas — le nom d'une variante **interne et instable**,
//! `Uncategorized`.
//!
//! C'est ce mot que JeromeQ a recopié du fil 1539 le 24/08/2026 :
//!
//! ```text
//! /mnt/eversolo_nvme/77A6-799D: Uncategorized — No such device (os error 19)
//! ```
//!
//! Sa capture `mount` montre deux montages empilés sur le même répertoire —
//! un `autofs` de systemd, recouvert d'un `mount -t cifs` fait à la main. Que
//! ce soit là l'origine de l'`ENODEV` reste une explication, pas un fait
//! établi ; ce qui EST établi, c'est qu'aucune de ces pistes n'était dicible
//! depuis le mot « Uncategorized », et qu'il a conclu — c'est écrit — « mes
//! connaissances de codeur s'arrêtent là ».
//!
//! Ce module ne prétend donc pas diagnostiquer sa machine : il rend l'obstacle
//! nommable, pour que la question suivante puisse être posée.
//!
//! ## La règle
//!
//! Un obstacle de scan doit être **nommé** (une cause en français, et le
//! chemin concerné) et **visible** (jamais sauté en silence). Aucun nom de
//! variante de `ErrorKind`, aucun `os error N` ne doit atteindre un écran :
//! `missing_dir_reasons` est rendu **verbatim** par le client web
//! (`SettingsView.svelte`).
//!
//! ## Pourquoi lire `raw_os_error()` et non `kind()`
//!
//! `ErrorKind` est un dénominateur commun entre systèmes : il ne nomme pas
//! `ENODEV`, et il ne le nommera peut-être jamais. Or ce sont précisément les
//! errno d'un montage réseau qui décroche (`ENODEV`, `ENOTCONN`, `ESTALE`,
//! `EHOSTDOWN`) qui intéressent une bibliothèque musicale sur NAS. On lit donc
//! l'errno, et `kind()` ne sert plus que de filet.
//!
//! ## Un seul mécanisme
//!
//! La forme de retour — `(motif, message)`, un identifiant machine stable et
//! une phrase pour l'utilisateur — est **celle de
//! `tune-server/src/routes/network.rs::obstacle_de_montage`** (#1847,
//! Dominique Comet), délibérément, et le vocabulaire des motifs est le même.
//! Ce module vit dans `tune-core` pour que le côté serveur puisse s'y brancher
//! sans qu'un second mécanisme apparaisse ; `network.rs` est en cours de
//! modification par ailleurs et n'a pas été touché ici.
//!
//! Refs #2356, #2357, #1190, #1847.

/// Texte du système d'exploitation, débarrassé du suffixe `(os error N)`.
///
/// `Display` de [`std::io::Error`] colle ce suffixe à la fin. Il est du bruit
/// pour l'utilisateur — et c'est la moitié de ce que JeromeQ a recopié sans
/// pouvoir l'exploiter. On garde la phrase du système (utile au support), on
/// jette le code, qui part dans le journal.
fn texte_systeme(e: &std::io::Error) -> Option<String> {
    let rendu = e.to_string();
    let nu = match rendu.rfind(" (os error ") {
        Some(i) => rendu[..i].trim().to_string(),
        None => rendu.trim().to_string(),
    };
    if nu.is_empty() || nu.contains("os error") {
        None
    } else {
        Some(nu)
    }
}

/// Annexe technique : la phrase du système entre parenthèses, ou rien.
fn annexe(e: &std::io::Error) -> String {
    match texte_systeme(e) {
        Some(t) => format!(" (le système répond : « {t} »)"),
        None => String::new(),
    }
}

/// `{chemin}` n'est pas un dossier — cas séparé parce qu'il est atteignable
/// sans erreur d'E/S (le chemin a été remplacé entre la sonde et l'usage).
pub fn pas_un_dossier(chemin: &str) -> (&'static str, String) {
    (
        "pas_un_dossier",
        format!(
            "{chemin} n'est pas un dossier. Les dossiers de musique doivent \
             désigner un répertoire, pas un fichier : corrigez le chemin dans \
             les réglages."
        ),
    )
}

/// Traduire l'échec d'ouverture d'une racine de bibliothèque en obstacle
/// NOMMÉ : un motif machine stable, et une phrase qui dit la cause et le
/// chemin.
///
/// Le motif n'est pas affiché ; il part dans le journal (`motif=…`) et sert à
/// regrouper les incidents sans dépendre du texte, qui, lui, peut être
/// reformulé.
pub fn obstacle_de_lecture(chemin: &str, e: &std::io::Error) -> (&'static str, String) {
    if let Some(nomme) = obstacle_par_code(chemin, e) {
        return nomme;
    }
    // Filet : erreurs sans code système (E/S synthétique), et systèmes dont on
    // n'énumère pas les codes.
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => droits_refuses(chemin),
        std::io::ErrorKind::NotFound => dossier_absent(chemin),
        _ => indetermine(chemin, e),
    }
}

fn droits_refuses(chemin: &str) -> (&'static str, String) {
    (
        "privileges_insuffisants",
        format!(
            "Le serveur n'a pas le droit d'ouvrir {chemin}. Le dossier est là, \
             mais l'utilisateur sous lequel tourne Tune ne peut pas le lire — \
             le plus souvent parce qu'un partage réseau a été monté sous une \
             autre identité (options uid=/gid= du montage), ou parce que les \
             droits du dossier l'interdisent."
        ),
    )
}

fn dossier_absent(chemin: &str) -> (&'static str, String) {
    (
        "dossier_absent",
        format!(
            "Le dossier {chemin} n'existe pas pour le serveur. S'il s'agit \
             d'un partage réseau, il n'est plus monté ; s'il s'agit d'un \
             lecteur réseau Windows (Z:\\), il peut être monté dans votre \
             session sans l'être pour le service. Sinon, le chemin déclaré \
             dans les dossiers de musique est à corriger."
        ),
    )
}

fn indetermine(chemin: &str, e: &std::io::Error) -> (&'static str, String) {
    let code = match e.raw_os_error() {
        Some(n) => format!(" Code système {n}."),
        None => String::new(),
    };
    (
        "obstacle_indetermine",
        format!(
            "Le dossier {chemin} n'a pas pu être ouvert{}.{code} Si c'est un \
             partage réseau, vérifiez qu'il est bien monté et lisible par \
             l'utilisateur qui exécute Tune, puis relancez un scan.",
            annexe(e)
        ),
    )
}

/// Les errno d'un montage réseau, et ceux d'un support local en peine.
///
/// Les valeurs numériques diffèrent d'un système à l'autre (`ENOTCONN` vaut
/// 107 sous Linux et 57 sous macOS) : on passe par les constantes de `libc`,
/// jamais par des littéraux.
#[cfg(unix)]
fn obstacle_par_code(chemin: &str, e: &std::io::Error) -> Option<(&'static str, String)> {
    let code = e.raw_os_error()?;
    let nomme = match code {
        // Le cas de JeromeQ. « No such device » sur un répertoire n'est pas un
        // problème de contenu : la couche de montage sous ce chemin ne peut
        // plus servir de demande.
        libc::ENODEV => (
            "montage_indisponible",
            format!(
                "Le dossier {chemin} n'a pas pu être ouvert : le système ne \
                 trouve plus le périphérique qui le porte. Ce n'est pas un \
                 problème de contenu, c'est le montage lui-même qui ne répond \
                 pas — typiquement un partage réseau (SMB/CIFS, NFS) décroché, \
                 ou deux montages empilés sur le même répertoire, dont celui \
                 du dessous ne peut plus être satisfait. Vérifiez le montage \
                 (sous Linux : findmnt {chemin}) avant de relancer un scan. \
                 Les pistes déjà connues sous ce dossier sont conservées."
            ),
        ),
        libc::ENOTCONN => (
            "montage_decroche",
            format!(
                "Le dossier {chemin} est sur un montage réseau qui a perdu la \
                 connexion à son serveur. Le point de montage existe encore, \
                 mais plus rien ne répond derrière. Remontez le partage, puis \
                 relancez un scan. Les pistes déjà connues sous ce dossier \
                 sont conservées."
            ),
        ),
        libc::ESTALE => (
            "montage_perime",
            format!(
                "Le montage qui porte {chemin} est périmé : le partage a été \
                 remonté ou remplacé côté serveur pendant que Tune l'utilisait. \
                 Démontez puis remontez le partage avant de relancer un scan. \
                 Les pistes déjà connues sous ce dossier sont conservées."
            ),
        ),
        libc::EHOSTDOWN
        | libc::EHOSTUNREACH
        | libc::ENETUNREACH
        | libc::ENETDOWN
        | libc::ETIMEDOUT
        | libc::ECONNREFUSED
        | libc::ECONNRESET
        | libc::ECONNABORTED => (
            "serveur_injoignable",
            format!(
                "Le serveur réseau qui héberge {chemin} est injoignable \
                 (éteint, en veille, ou coupé du réseau). Le montage est \
                 toujours déclaré, mais aucune lecture ne passe. Les pistes \
                 déjà connues sous ce dossier sont conservées."
            ),
        ),
        libc::ENOTDIR => return Some(pas_un_dossier(chemin)),
        libc::ELOOP => (
            "boucle_de_liens",
            format!(
                "Le chemin {chemin} traverse une boucle de liens symboliques : \
                 le système ne peut pas le résoudre. Déclarez la cible réelle \
                 plutôt que le lien."
            ),
        ),
        libc::EIO => (
            "erreur_de_support",
            format!(
                "Erreur d'entrée/sortie en ouvrant {chemin}. Le support est en \
                 cause, pas la configuration : disque ou clé défaillante, câble \
                 USB, ou système de fichiers endommagé. Vérifiez le support \
                 avant de relancer un scan."
            ),
        ),
        libc::EMFILE | libc::ENFILE => (
            "trop_de_fichiers_ouverts",
            format!(
                "Le système a refusé d'ouvrir {chemin} : la limite de fichiers \
                 ouverts est atteinte. Relevez cette limite pour l'utilisateur \
                 qui exécute Tune (ulimit -n), puis relancez un scan."
            ),
        ),
        libc::EACCES | libc::EPERM => return Some(droits_refuses(chemin)),
        libc::ENOENT => return Some(dossier_absent(chemin)),
        _ => return None,
    };
    Some(nomme)
}

/// Codes Win32 des partages réseau. Ils ne passent pas par `errno` : la même
/// valeur numérique y désigne autre chose, d'où deux tables séparées.
#[cfg(windows)]
fn obstacle_par_code(chemin: &str, e: &std::io::Error) -> Option<(&'static str, String)> {
    // ERROR_NOT_READY 21, ERROR_BAD_NETPATH 53, ERROR_DEV_NOT_EXIST 55,
    // ERROR_UNEXP_NET_ERR 59, ERROR_NETNAME_DELETED 64, ERROR_BAD_NET_NAME 67,
    // ERROR_SESSION_CREDENTIAL_CONFLICT 1219, ERROR_LOGON_FAILURE 1326,
    // ERROR_NETWORK_UNREACHABLE 1231, ERROR_HOST_UNREACHABLE 1232.
    let code = e.raw_os_error()?;
    let nomme = match code {
        21 => (
            "montage_indisponible",
            format!(
                "Le périphérique qui porte {chemin} n'est pas prêt. Ce n'est \
                 pas un problème de contenu : le lecteur ou le montage ne \
                 répond pas encore. Vérifiez-le avant de relancer un scan. Les \
                 pistes déjà connues sous ce dossier sont conservées."
            ),
        ),
        55 | 59 | 64 => (
            "montage_decroche",
            format!(
                "La connexion réseau qui porte {chemin} a été rompue. Le \
                 lecteur réseau est toujours déclaré, mais plus rien ne répond \
                 derrière. Reconnectez-le, puis relancez un scan. Les pistes \
                 déjà connues sous ce dossier sont conservées."
            ),
        ),
        53 | 67 | 1231 | 1232 => (
            "serveur_injoignable",
            format!(
                "Le serveur réseau qui héberge {chemin} est injoignable \
                 (éteint, en veille, ou nom introuvable sur le réseau). Les \
                 pistes déjà connues sous ce dossier sont conservées."
            ),
        ),
        1219 | 1326 => (
            "identifiants_refuses",
            format!(
                "Les identifiants du partage qui porte {chemin} ont été \
                 refusés. Sous Windows, une session ne peut pas ouvrir deux \
                 connexions au même serveur sous deux comptes différents : \
                 fermez la connexion existante (net use * /delete) ou \
                 réutilisez le même compte."
            ),
        ),
        _ => return None,
    };
    Some(nomme)
}

#[cfg(not(any(unix, windows)))]
fn obstacle_par_code(_chemin: &str, _e: &std::io::Error) -> Option<(&'static str, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::obstacle_de_lecture;
    use std::io::Error;

    /// Le mot que JeromeQ a recopié du fil 1539 : `Uncategorized`. C'est le
    /// `Debug` d'une variante INTERNE et instable de `std::io::ErrorKind`, pas
    /// un mot de notre vocabulaire. Il ne doit atteindre aucun écran, quel que
    /// soit le code système — y compris ceux que Rust ne nomme pas.
    #[test]
    fn aucun_nom_de_variante_rust_ne_sort_jamais() {
        let interdits = [
            "Uncategorized",
            "NotFound",
            "PermissionDenied",
            "NotConnected",
            "StaleNetworkFileHandle",
            "HostUnreachable",
            "NetworkUnreachable",
            "NotADirectory",
            "os error",
        ];
        // Balayage large : tout code plausible, pour qu'aucun cas non couvert
        // ne puisse ressortir en jargon par le filet du fourre-tout.
        for code in 1..=140i32 {
            let (_, rendu) = obstacle_de_lecture(
                "/mnt/eversolo_nvme/77A6-799D",
                &Error::from_raw_os_error(code),
            );
            for mot in interdits {
                assert!(
                    !rendu.contains(mot),
                    "code {code} rend « {mot} » à l'écran : {rendu:?}"
                );
            }
        }
    }

    /// Le chemin fautif doit toujours être dans la phrase : sans lui,
    /// l'utilisateur qui a cinq racines ne sait pas laquelle est en cause.
    #[test]
    fn le_chemin_concerne_est_toujours_nomme() {
        for code in 1..=140i32 {
            let (_, rendu) = obstacle_de_lecture(
                "/mnt/eversolo_nvme/77A6-799D",
                &Error::from_raw_os_error(code),
            );
            assert!(
                rendu.contains("/mnt/eversolo_nvme/77A6-799D"),
                "code {code} : chemin absent de {rendu:?}"
            );
        }
    }

    /// Un motif machine est toujours rendu, et jamais vide : c'est lui qui
    /// permet de regrouper les incidents dans le journal sans dépendre du
    /// texte.
    #[test]
    fn un_motif_machine_est_toujours_rendu() {
        for code in 1..=140i32 {
            let (motif, _) = obstacle_de_lecture("/mnt/nas", &Error::from_raw_os_error(code));
            assert!(!motif.is_empty(), "code {code} sans motif");
            assert!(
                motif.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "motif non machine : {motif:?}"
            );
        }
    }

    /// Le cas de JeromeQ, fil 1539 : ENODEV (19) sur un point de montage CIFS
    /// qui en portait deux, empilés. Il doit être nommé comme un problème de
    /// MONTAGE, et surtout pas laissé à confusion avec un dossier vide.
    #[cfg(unix)]
    #[test]
    fn enodev_est_nomme_comme_un_probleme_de_montage() {
        let (motif, rendu) = obstacle_de_lecture(
            "/mnt/eversolo_nvme/77A6-799D",
            &Error::from_raw_os_error(libc::ENODEV),
        );
        assert_eq!(motif, "montage_indisponible");
        let bas = rendu.to_lowercase();
        assert!(
            bas.contains("montage"),
            "ENODEV doit parler de montage : {rendu:?}"
        );
        assert!(
            !bas.contains("vide"),
            "ENODEV ne doit pas évoquer un dossier vide : {rendu:?}"
        );
    }

    /// Les autres errno d'un montage réseau qui décroche doivent eux aussi
    /// être nommés — pas retomber dans le fourre-tout.
    #[cfg(unix)]
    #[test]
    fn les_errno_de_montage_reseau_sont_tous_nommes() {
        for code in [
            libc::ENOTCONN,
            libc::ESTALE,
            libc::EHOSTDOWN,
            libc::EHOSTUNREACH,
        ] {
            let (motif, rendu) =
                obstacle_de_lecture("/mnt/nas/Musique", &Error::from_raw_os_error(code));
            assert_ne!(motif, "obstacle_indetermine", "errno {code} non nommé");
            let bas = rendu.to_lowercase();
            assert!(
                bas.contains("montage") || bas.contains("serveur") || bas.contains("réseau"),
                "errno {code} non nommé : {rendu:?}"
            );
        }
    }

    /// Un refus de droits reste un refus de droits, en français et avec la
    /// même famille de mots que le nommage d'obstacle du montage SMB
    /// (`privileges_insuffisants`, network.rs).
    #[cfg(unix)]
    #[test]
    fn eacces_parle_de_droits() {
        let (motif, rendu) =
            obstacle_de_lecture("/mnt/nas/Musique", &Error::from_raw_os_error(libc::EACCES));
        assert_eq!(motif, "privileges_insuffisants");
        assert!(rendu.to_lowercase().contains("droit"), "EACCES : {rendu:?}");
    }

    /// Une racine réellement absente doit le dire — en français.
    #[cfg(unix)]
    #[test]
    fn enoent_dit_que_le_dossier_est_absent() {
        let (motif, rendu) =
            obstacle_de_lecture("/mnt/nas/Musique", &Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(motif, "dossier_absent");
        let bas = rendu.to_lowercase();
        assert!(
            bas.contains("n'existe pas") || bas.contains("introuvable") || bas.contains("absent"),
            "ENOENT : {rendu:?}"
        );
    }

    /// Deux errno distincts ne doivent pas rendre la même phrase : c'est ce
    /// que faisait le fourre-tout, et c'est exactement ce qui n'aidait pas.
    #[cfg(unix)]
    #[test]
    fn les_causes_distinctes_rendent_des_phrases_distinctes() {
        let phrase = |c| obstacle_de_lecture("/mnt/nas", &Error::from_raw_os_error(c)).1;
        assert_ne!(phrase(libc::ENODEV), phrase(libc::ENOTCONN));
        assert_ne!(phrase(libc::ENODEV), phrase(libc::ENOENT));
        assert_ne!(phrase(libc::EACCES), phrase(libc::ENOENT));
    }

    /// Le suffixe `(os error N)` que colle `Display` est du bruit : il est
    /// retiré, et le code part dans une mention explicite plutôt que collé au
    /// message du système.
    #[test]
    fn le_fourre_tout_ne_recopie_pas_le_suffixe_de_display() {
        // Un code volontairement exotique, hors de toutes les tables.
        let (motif, rendu) = obstacle_de_lecture("/mnt/nas", &Error::from_raw_os_error(4095));
        assert_eq!(motif, "obstacle_indetermine");
        assert!(!rendu.contains("os error"), "{rendu:?}");
        assert!(
            rendu.contains("4095"),
            "le code doit rester lisible : {rendu:?}"
        );
    }
}
