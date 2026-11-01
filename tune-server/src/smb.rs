//! Montage CIFS : l'echelle de dialectes, et la question « est-ce monte ? ».
//!
//! Ce module existe parce que le montage se fait a DEUX endroits — la route
//! interactive (`routes/network.rs`) et le remontage au demarrage
//! (`startup.rs`) — et que ces deux endroits doivent imperativement parler le
//! meme dialecte.
//!
//! Ils ne le faisaient pas. La route avait appris a negocier (#1834) ; le
//! remontage, lui, imposait toujours `vers=3.0`. Consequence pour Philippe
//! Landes, dont le streamer Rose ne parle que SMB 1.0 : l'assistant montait son
//! partage, il retrouvait sa musique, et le **premier redemarrage le lui
//! reprenait** — le remontage retentait un dialecte que son materiel refuse.
//! L'interface, elle, continuait d'afficher le partage comme monte (#1916).
//!
//! Le commentaire d'origine assumait la recopie : « la route rend des erreurs
//! HTTP a un humain qui attend, celle-ci journalise et passe au suivant ». Cet
//! argument tient toujours pour la *restitution* — et c'est pourquoi les deux
//! appelants gardent la leur. Il ne tient pas pour la *strategie de montage* :
//! deux echelles de dialectes qui divergent, c'est un partage qui monte a
//! l'ecran et se demonte au redemarrage.

use std::time::Duration;

/// Dialectes essayes, dans l'ordre.
///
/// `None` = aucune option `vers=` : c'est ce qui declenche la negociation du
/// noyau, pas une valeur particuliere. Le module CIFS ne negocie de lui-meme
/// qu'entre 2.1, 3.0 et 3.1.1 — il ne descend jamais jusqu'a SMB 1, sorti de la
/// negociation par `CONFIG_CIFS_ALLOW_INSECURE_LEGACY`. Il faut le lui demander
/// explicitement, d'ou les deux echelons du bas.
pub const DIALECTES: [Option<&str>; 3] = [None, Some("2.0"), Some("1.0")];

/// Delai par essai. Trois essais tiennent alors sous le delai d'attente de 60 s
/// de l'API, la ou 15 s l'auraient frole.
pub const ESSAI_TIMEOUT: Duration = Duration::from_secs(10);

/// Etiquette d'un dialecte pour les journaux et pour la base.
///
/// La negociation libre s'ecrit `negocie` plutot que de laisser un trou : une
/// colonne vide se lit « on ne sait pas », alors qu'ici on sait tres bien.
pub fn etiquette(dialecte: Option<&str>) -> &str {
    dialecte.unwrap_or("negocie")
}

/// L'inverse d'[`etiquette`] : ce que la base a retenu redevient une option.
pub fn depuis_etiquette(etiquette: &str) -> Option<&str> {
    match etiquette.trim() {
        "" | "negocie" => None,
        v => Some(v),
    }
}

/// Options passees a `mount.cifs` pour un essai (service en root).
///
/// `iocharset=utf8` est impose : sans lui, le noyau convertit les noms de
/// fichiers avec le jeu de caracteres par defaut du systeme, qui n'est pas
/// toujours UTF-8. Tout caractere qu'il ne sait pas representer — apostrophe
/// typographique ’, n tilde ñ, e accent aigu decompose, œ — devient alors un
/// `?` dans le nom que liste le partage. Le fichier apparait au scan, mais
/// `?` n'est pas son vrai nom : l'ouvrir echoue par `No such file or
/// directory`, et la piste est perdue pour la bibliotheque.
///
/// Les options sont TOUTES composees ici, a partir d'une liste fermee : aucune
/// valeur venue du client n'entre dans la chaine. Les identifiants passent par
/// `credentials=<fichier 0600>` ([`FichierIdentifiants`]) — ni sur la ligne de
/// commande, ni dans une option ou une virgule les decouperait. `nosuid`,
/// `nodev` et `noexec` : un partage de musique n'a rien a executer.
///
/// Les deux appelants (route interactive et remontage au demarrage) passent
/// par ici : un partage monte en UTF-8 depuis l'assistant doit le rester au
/// redemarrage.
pub fn options_de_montage(fichier_identifiants: &str, dialecte: Option<&str>) -> String {
    let mut opts = format!("credentials={fichier_identifiants},iocharset=utf8");
    if let Some(v) = dialecte.filter(|v| DIALECTES.contains(&Some(*v))) {
        opts.push_str(&format!(",vers={v}"));
    }
    opts.push_str(",nosuid,nodev,noexec");
    opts
}

/// La commande d'UN essai de montage Linux, selon le compte du service (#3206).
///
/// - root : `mount.cifs <unc> <point> -o <options>`, les identifiants dans
///   `fichier_identifiants` (voir [`options_de_montage`]) ;
/// - autre compte : `sudo -n tune-os-privilege smb-mount <unc> <point>
///   <utilisateur> <dialecte>`, le mot de passe sur l'entrée standard. C'est
///   l'assistant qui compose les options, avec `uid`/`gid` du compte appelant
///   et `nosuid,nodev,noexec`.
///
/// Les deux appelants (route interactive, remontage au démarrage) passent par
/// [`lancer_un_essai`] pour la même raison que l'échelle : ce qui monte d'un
/// côté doit monter de l'autre.
#[allow(clippy::too_many_arguments)]
pub fn commande_de_montage(
    euid: u32,
    sudo: &str,
    unc: &str,
    point: &str,
    user: &str,
    pass: &str,
    dialecte: Option<&str>,
    fichier_identifiants: &str,
) -> crate::privilege::Commande {
    use crate::privilege::Commande;
    if euid == 0 {
        let opts = options_de_montage(fichier_identifiants, dialecte);
        Commande::directe("mount.cifs", &[unc, point, "-o", &opts])
    } else {
        Commande::par_l_assistant(sudo, "smb-mount", &[unc, point, user, etiquette(dialecte)])
            .avec_entree(pass)
    }
}

/// Le fichier `credentials=` de `mount.cifs` : nom aléatoire, créé en
/// exclusivité, droits 0600, supprimé à la fin de l'essai (`Drop`, panique et
/// délai dépassé compris).
pub struct FichierIdentifiants {
    fichier: tempfile::NamedTempFile,
}

impl FichierIdentifiants {
    pub fn creer(user: &str, pass: &str) -> std::io::Result<Self> {
        use std::io::Write;
        verifier_identifiants(user, pass)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut fichier = tempfile::Builder::new()
            .prefix(".tune-smb-")
            .rand_bytes(16)
            .tempfile()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fichier
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if fichier.path().to_string_lossy().contains(',') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credentials file path contains a comma",
            ));
        }
        write!(fichier, "username={user}\npassword={pass}\n")?;
        fichier.as_file().sync_all()?;
        Ok(Self { fichier })
    }

    pub fn chemin(&self) -> String {
        self.fichier.path().to_string_lossy().into_owned()
    }
}

/// Un essai de montage Linux, identifiants compris : la seule entrée des deux
/// appelants. Le fichier d'identifiants ne vit que le temps de l'essai.
pub async fn lancer_un_essai(
    euid: u32,
    sudo: &str,
    unc: &str,
    point: &str,
    user: &str,
    pass: &str,
    dialecte: Option<&str>,
) -> std::io::Result<std::process::Output> {
    verifier_identifiants(user, pass)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if euid == 0 {
        let identifiants = FichierIdentifiants::creer(user, pass)?;
        let sortie = commande_de_montage(
            euid,
            sudo,
            unc,
            point,
            user,
            pass,
            dialecte,
            &identifiants.chemin(),
        )
        .lancer()
        .await;
        drop(identifiants);
        sortie
    } else {
        commande_de_montage(euid, sudo, unc, point, user, pass, dialecte, "")
            .lancer()
            .await
    }
}

/// Identifiants acceptables : aucun caractère de contrôle (un saut de ligne
/// ajouterait une ligne au fichier `credentials=`), longueur bornée.
pub fn verifier_identifiants(user: &str, pass: &str) -> Result<(), &'static str> {
    if user.chars().any(char::is_control) || pass.chars().any(char::is_control) {
        return Err("SMB credentials must not contain control characters");
    }
    if user.len() > 256 || pass.len() > 1024 {
        return Err("SMB credentials are too long");
    }
    Ok(())
}

/// Un hôte SMB : nom, IPv4 ou IPv6 (zone `%` comprise), sans séparateur,
/// sans espace, et qui ne commence pas par `-` (lu comme une option).
pub fn verifier_hote(hote: &str) -> Result<(), &'static str> {
    if hote.is_empty() || hote.len() > 253 || hote.starts_with('-') {
        return Err("invalid SMB host");
    }
    if hote
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%' | '[' | ']'))
    {
        Ok(())
    } else {
        Err("invalid SMB host")
    }
}

/// Un nom de partage : non vide, sans caractère de contrôle ni virgule, sans
/// composant `..`.
pub fn verifier_partage(partage: &str) -> Result<(), &'static str> {
    if partage.trim().is_empty()
        || partage.len() > 512
        || partage.chars().any(|c| c.is_control() || c == ',')
        || partage.split(['/', '\\']).any(|c| c == "..")
        || partage.starts_with(['/', '\\'])
    {
        return Err("invalid SMB share name");
    }
    Ok(())
}

/// Le nom du point de montage d'un partage, sous la racine des montages :
/// `<hôte>_<partage>`, chaque caractère hors de l'ensemble sûr remplacé par
/// `_`. Un seul composant de chemin, jamais `.` ni `..`.
pub fn nom_du_point(hote: &str, partage: &str) -> String {
    let nettoyer = |s: &str, garde: &dyn Fn(char) -> bool| -> String {
        s.chars().map(|c| if garde(c) { c } else { '_' }).collect()
    };
    let hote = nettoyer(hote, &|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%')
    });
    // Mêmes remplacements qu'avant pour le partage (`/`, `\`, espace) : les
    // points déjà enregistrés gardent leur nom.
    let partage = nettoyer(partage, &|c| {
        !(c == '/' || c == '\\' || c == ' ' || c.is_control())
    });
    let nom = format!("{hote}_{partage}");
    if nom.starts_with('.') {
        format!("_{nom}")
    } else {
        nom
    }
}

/// Le point de montage est-il un ENFANT DIRECT d'une des racines contrôlées
/// par le serveur ?
///
/// Lexicalement d'abord (chemin absolu, aucun `.` ni `..`, parent égal à une
/// racine) ; puis, sur disque, ni le point ni son parent ne doivent être un
/// lien symbolique qui mènerait ailleurs. C'est la seule forme de chemin que
/// la route de montage, le remontage au démarrage et l'oubli d'un partage
/// acceptent : aucun chemin fourni par un client ne l'atteint.
pub fn est_confine(point: &std::path::Path, racines: &[std::path::PathBuf]) -> bool {
    use std::path::Component;
    if !point.is_absolute()
        || point
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let Some(nom) = point.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if nom.is_empty() || nom == "." || nom == ".." {
        return false;
    }
    let Some(parent) = point.parent() else {
        return false;
    };
    let normaliser = |p: &std::path::Path| -> std::path::PathBuf {
        p.components().collect::<std::path::PathBuf>()
    };
    let Some(racine) = racines
        .iter()
        .find(|r| r.is_absolute() && normaliser(r) == normaliser(parent))
    else {
        return false;
    };
    if let Ok(meta) = std::fs::symlink_metadata(point)
        && meta.file_type().is_symlink()
    {
        return false;
    }
    match (std::fs::canonicalize(parent), std::fs::canonicalize(racine)) {
        (Ok(p), Ok(r)) => p == r,
        // Racine absente : rien n'existe encore sous elle, le contrôle
        // lexical suffit (le dossier sera créé par le serveur).
        (Err(_), Err(_)) => true,
        _ => false,
    }
}

/// La commande de démontage d'un partage, selon le compte du service.
pub fn commande_de_demontage(euid: u32, sudo: &str, point: &str) -> crate::privilege::Commande {
    use crate::privilege::Commande;
    if euid == 0 {
        Commande::directe("umount", &[point])
    } else {
        Commande::par_l_assistant(sudo, "smb-umount", &[point])
    }
}

#[cfg(test)]
mod commande_de_montage_tests {
    use super::*;
    use crate::privilege::{ASSISTANT, SUDO};

    #[test]
    fn en_root_les_identifiants_passent_par_un_fichier() {
        let c = commande_de_montage(
            0,
            SUDO,
            "//nas/m",
            "/mnt/nas_m",
            "u",
            "p",
            Some("2.0"),
            "/run/f",
        );
        assert_eq!(c.programme, "mount.cifs");
        assert_eq!(
            c.args,
            vec![
                "//nas/m",
                "/mnt/nas_m",
                "-o",
                "credentials=/run/f,iocharset=utf8,vers=2.0,nosuid,nodev,noexec"
            ]
        );
        assert_eq!(c.entree, None);
        let d = commande_de_demontage(0, SUDO, "/mnt/nas_m");
        assert_eq!(
            (d.programme.as_str(), d.args.clone()),
            ("umount", vec!["/mnt/nas_m".to_string()])
        );
    }

    /// Une virgule dans les identifiants n'ajoute aucune option : ils ne
    /// figurent plus du tout dans la ligne de commande.
    #[test]
    fn aucune_option_ne_vient_des_identifiants() {
        let hostile = "x,uid=0,setuids,file_mode=06777";
        for dialecte in DIALECTES {
            let c = commande_de_montage(
                0,
                SUDO,
                "//nas/m",
                "/mnt/nas_m",
                hostile,
                hostile,
                dialecte,
                "/run/f",
            );
            let opts = &c.args[3];
            assert!(!opts.contains("uid="), "{opts}");
            assert!(!opts.contains("setuids"), "{opts}");
            assert!(!opts.contains("file_mode"), "{opts}");
            assert!(
                !opts.contains("username=") && !opts.contains("password="),
                "{opts}"
            );
            for o in ["nosuid", "nodev", "noexec"] {
                assert!(opts.split(',').any(|x| x == o), "{o} manque : {opts}");
            }
            assert!(!c.args.iter().any(|a| a.contains(hostile)), "{:?}", c.args);
        }
    }

    #[test]
    fn le_fichier_d_identifiants_est_prive_et_ephemere() {
        let f = FichierIdentifiants::creer("temoin", "p,a=b").unwrap();
        let chemin = std::path::PathBuf::from(f.chemin());
        assert_eq!(
            std::fs::read_to_string(&chemin).unwrap(),
            "username=temoin\npassword=p,a=b\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&chemin).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "droits {mode:o}");
        }
        drop(f);
        assert!(!chemin.exists(), "le fichier doit disparaître avec l'essai");
        assert!(FichierIdentifiants::creer("u\nusername=root", "p").is_err());
        assert!(FichierIdentifiants::creer("u", "p\ndomain=x").is_err());
    }

    #[test]
    fn hors_root_l_assistant_recoit_le_mot_de_passe_sur_l_entree() {
        for dialecte in DIALECTES {
            let c = commande_de_montage(
                1000,
                SUDO,
                "//nas/m",
                "/mnt/nas_m",
                "u",
                "p@ss,1",
                dialecte,
                "",
            );
            assert_eq!(c.programme, SUDO);
            assert_eq!(
                c.args,
                vec![
                    "-n",
                    ASSISTANT,
                    "smb-mount",
                    "//nas/m",
                    "/mnt/nas_m",
                    "u",
                    etiquette(dialecte)
                ]
            );
            assert!(
                !c.args.iter().any(|a| a.contains("p@ss")),
                "le mot de passe ne doit pas passer en argument (sudo journalise la ligne)"
            );
            assert_eq!(c.entree.as_deref(), Some("p@ss,1"));
        }
        let d = commande_de_demontage(1000, SUDO, "/mnt/nas_m");
        assert_eq!(d.args, vec!["-n", ASSISTANT, "smb-umount", "/mnt/nas_m"]);
    }

    /// Un refus de sudo ne se répare pas en changeant de dialecte.
    #[test]
    fn un_refus_de_sudo_arrete_l_echelle() {
        assert!(arrete_l_echelle("sudo: a password is required"));
    }

    #[test]
    fn le_point_de_montage_reste_un_enfant_direct_de_la_racine() {
        let racines = vec![std::path::PathBuf::from("/racine-inexistante-7a1/mnt")];
        let ok = |p: &str| est_confine(std::path::Path::new(p), &racines);
        assert!(ok("/racine-inexistante-7a1/mnt/192.168.1.10_Musique"));
        assert!(!ok("/usr/bin"));
        assert!(!ok("/etc/cron.d"));
        assert!(!ok("/racine-inexistante-7a1/mnt"));
        assert!(!ok("/racine-inexistante-7a1/mnt/a/b"));
        assert!(!ok("/racine-inexistante-7a1/mnt/../etc"));
        assert!(!ok("racine-inexistante-7a1/mnt/x"));
        let nom = nom_du_point("../../etc", "a/../b");
        assert!(!nom.contains('/'), "{nom}");
        assert!(ok(&format!("/racine-inexistante-7a1/mnt/{nom}")), "{nom}");
        assert_eq!(
            nom_du_point("192.168.1.10", "Ma Musique"),
            "192.168.1.10_Ma_Musique"
        );
        assert_eq!(nom_du_point("fd12::58d1", "Music"), "fd12::58d1_Music");
    }

    #[cfg(unix)]
    #[test]
    fn un_lien_symbolique_ne_sort_pas_de_la_racine() {
        let d = tune_core::test_scratch::scratch_dir("tune-smb-lien");
        let racine = d.join("montages");
        std::fs::create_dir_all(&racine).unwrap();
        std::os::unix::fs::symlink("/etc", racine.join("nas_m")).unwrap();
        assert!(!est_confine(
            &racine.join("nas_m"),
            std::slice::from_ref(&racine)
        ));
        std::fs::create_dir_all(racine.join("nas_n")).unwrap();
        assert!(est_confine(&racine.join("nas_n"), &[racine]));
    }

    #[test]
    fn hotes_et_partages_hostiles_refuses() {
        assert!(verifier_hote("192.168.1.10").is_ok());
        assert!(verifier_hote("fe80::1%eth0").is_ok());
        assert!(verifier_hote("nas.local").is_ok());
        for h in ["", "-o", "a b", "a/b", "a,b", "a\nb"] {
            assert!(verifier_hote(h).is_err(), "{h:?}");
        }
        assert!(verifier_partage("Musique").is_ok());
        assert!(verifier_partage("Ma Musique/Jazz").is_ok());
        for p in ["", "a,b", "../x", "/x", "a\0b"] {
            assert!(verifier_partage(p).is_err(), "{p:?}");
        }
    }
}

#[cfg(test)]
mod options_de_montage_tests {
    use super::options_de_montage;

    /// Retour de terrain : sans `iocharset=utf8`, les noms avec ’, ñ ou é passaient
    /// en `?` et les fichiers devenaient introuvables.
    #[test]
    fn chaque_essai_monte_en_utf8() {
        for dialecte in super::DIALECTES {
            let opts = options_de_montage("/run/f", dialecte);
            assert!(
                opts.split(',').any(|o| o == "iocharset=utf8"),
                "essai {dialecte:?} sans iocharset=utf8 : {opts}"
            );
        }
    }

    #[test]
    fn le_dialecte_reste_une_option_a_part() {
        assert_eq!(
            options_de_montage("/run/f", Some("1.0")),
            "credentials=/run/f,iocharset=utf8,vers=1.0,nosuid,nodev,noexec"
        );
        assert!(!options_de_montage("/run/f", None).contains("vers="));
        // Un dialecte hors de l'échelle n'entre pas dans les options.
        assert!(!options_de_montage("/run/f", Some("3.0,uid=0")).contains("uid="));
    }
}

/// L'echelle a parcourir, le dialecte connu d'abord.
///
/// Un partage qui a deja monte en SMB 1.0 remonte en SMB 1.0 du premier coup :
/// sans cela, chaque demarrage rejouerait deux essais voues a l'echec, soit
/// vingt secondes de retard par partage avant que la bibliotheque ne soit
/// lisible. Le reste de l'echelle suit quand meme — un NAS mis a jour, ou
/// remplace, ne doit pas rester prisonnier de ce qu'il repondait l'an dernier.
pub fn echelle(connu: Option<&str>) -> Vec<Option<&str>> {
    let mut ordre: Vec<Option<&str>> = Vec::with_capacity(DIALECTES.len());
    if let Some(c) = connu {
        // Seul un dialecte de l'echelle est retenu : une valeur aberrante en
        // base ne doit pas devenir une option passee a `mount.cifs`.
        if let Some(d) = DIALECTES.iter().find(|d| etiquette(**d) == c) {
            ordre.push(*d);
        }
    }
    let deja = ordre.clone();
    ordre.extend(DIALECTES.iter().filter(|d| !deja.contains(d)).copied());
    ordre
}

/// Le message d'erreur de `mount.cifs` traduit-il un refus d'identifiants ?
///
/// La distinction porte une decision : un dialecte inadapte se repare en en
/// essayant un autre, un mot de passe refuse non. Reessayer trois fois ferait
/// patienter l'utilisateur trente secondes pour lui resservir la meme reponse.
pub fn est_refus_d_authentification(stderr: &str) -> bool {
    let bas = stderr.to_lowercase();
    bas.contains("permission denied")
        || bas.contains("access denied")
        || bas.contains("bad user name or password")
}

/// Le message de `mount.cifs` dit-il que le point de montage est deja occupe ?
///
/// `mount error(16): Device or resource busy` (EBUSY) : le noyau refuse un
/// second montage sur un point deja monte. Changer de dialecte n'y fera rien —
/// et c'est pourtant ce que faisait l'echelle : apres l'EBUSY, elle essayait
/// `vers=2.0` puis `vers=1.0`, que le NAS refusait (`error(95)`), et c'est ce
/// DERNIER message que l'utilisateur lisait. Daniel Levy (fil 2145) a ainsi vu
/// « Operation not supported » pour un partage qui etait simplement deja monte.
pub fn est_deja_monte(stderr: &str) -> bool {
    let bas = stderr.to_lowercase();
    bas.contains("error(16)") || bas.contains("device or resource busy")
}

/// Faut-il arreter l'echelle des dialectes sur cet echec ?
///
/// Seuls les echecs qu'un autre dialecte ne reparera pas l'arretent : un refus
/// d'identifiants, et un point de montage deja occupe (fil 2145). Les deux
/// appelants — la route interactive et le remontage au demarrage — passent par
/// ici, pour ne pas diverger (voir l'en-tete du module).
///
/// Un refus de sudo (service hors root sans regle sudoers, #3206) non plus.
pub fn arrete_l_echelle(stderr: &str) -> bool {
    est_refus_d_authentification(stderr)
        || est_deja_monte(stderr)
        || crate::privilege::est_un_refus_d_elevation(stderr)
}

/// Defaire les echappements octaux de `/proc/self/mounts` (`\040` pour une
/// espace, `\011`, `\012`, `\134`).
fn desechapper_octal(champ: &str) -> String {
    let octets = champ.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len());
    let mut i = 0;
    while i < octets.len() {
        if octets[i] == b'\\'
            && i + 3 < octets.len()
            && (b'0'..=b'3').contains(&octets[i + 1])
            && octets[i + 2..i + 4]
                .iter()
                .all(|c| (b'0'..=b'7').contains(c))
        {
            let v =
                (octets[i + 1] - b'0') * 64 + (octets[i + 2] - b'0') * 8 + (octets[i + 3] - b'0');
            sortie.push(v);
            i += 4;
        } else {
            sortie.push(octets[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&sortie).into_owned()
}

/// La source montee sur `chemin`, lue dans une table au format
/// `/proc/self/mounts`. Le DERNIER montage l'emporte : c'est lui qui est
/// visible quand plusieurs sont empiles sur le meme point.
pub fn source_dans_la_table(table: &str, chemin: &str) -> Option<String> {
    let chemin = chemin.trim_end_matches('/');
    let chemin = if chemin.is_empty() { "/" } else { chemin };
    table.lines().rev().find_map(|ligne| {
        let mut champs = ligne.split_whitespace();
        let source = champs.next()?;
        let cible = champs.next()?;
        (desechapper_octal(cible) == chemin).then(|| desechapper_octal(source))
    })
}

/// La source effectivement montee sur `chemin` (`//hote/partage` pour un
/// montage CIFS). `None` quand on ne sait pas la lire : hors Linux, ou table
/// des montages illisible.
pub fn source_du_montage(chemin: &std::path::Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let canonique = std::fs::canonicalize(chemin).unwrap_or_else(|_| chemin.to_path_buf());
    source_dans_la_table(&table, &canonique.to_string_lossy())
}

/// `source` (lue dans la table des montages) designe-t-elle `//hote/partage` ?
///
/// La comparaison ignore la casse (SMB l'ignore, pour l'hote comme pour le
/// partage), le sens des barres (`\\hote\partage`), les crochets d'une IPv6
/// litterale et une barre finale.
pub fn meme_source(source: &str, hote: &str, partage: &str) -> bool {
    fn normaliser(s: &str) -> String {
        s.replace('\\', "/")
            .replace(['[', ']'], "")
            .trim_end_matches('/')
            .to_lowercase()
    }
    normaliser(source) == normaliser(&format!("//{hote}/{partage}"))
}

/// Identifiant de serveur SMB2 (`ServerGuid` de la reponse NEGOTIATE).
pub type GuidServeur = [u8; 16];

/// Une requete SMB2 NEGOTIATE minimale, cadree NetBIOS (port 445).
///
/// Les dialectes 2.0.2 a 3.0.2 seulement : 3.1.1 exigerait des contextes de
/// negociation, inutiles pour lire le `ServerGuid`.
pub fn requete_negotiate() -> Vec<u8> {
    const DIALECTES_SMB2: [u16; 4] = [0x0202, 0x0210, 0x0300, 0x0302];
    let mut smb = Vec::with_capacity(64 + 36 + 8);
    // En-tete SMB2 (64 octets).
    smb.extend_from_slice(&[0xFE, b'S', b'M', b'B']);
    smb.extend_from_slice(&64u16.to_le_bytes()); // StructureSize
    smb.extend_from_slice(&0u16.to_le_bytes()); // CreditCharge
    smb.extend_from_slice(&0u32.to_le_bytes()); // Status
    smb.extend_from_slice(&0u16.to_le_bytes()); // Command = NEGOTIATE
    smb.extend_from_slice(&1u16.to_le_bytes()); // CreditRequest
    smb.extend_from_slice(&0u32.to_le_bytes()); // Flags
    smb.extend_from_slice(&0u32.to_le_bytes()); // NextCommand
    smb.extend_from_slice(&0u64.to_le_bytes()); // MessageId
    smb.extend_from_slice(&0u32.to_le_bytes()); // Reserved
    smb.extend_from_slice(&0u32.to_le_bytes()); // TreeId
    smb.extend_from_slice(&0u64.to_le_bytes()); // SessionId
    smb.extend_from_slice(&[0u8; 16]); // Signature
    // Corps NEGOTIATE.
    smb.extend_from_slice(&36u16.to_le_bytes()); // StructureSize
    smb.extend_from_slice(&(DIALECTES_SMB2.len() as u16).to_le_bytes());
    smb.extend_from_slice(&1u16.to_le_bytes()); // SecurityMode : signature possible
    smb.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    smb.extend_from_slice(&0u32.to_le_bytes()); // Capabilities
    smb.extend_from_slice(b"Tune-client-guid"); // ClientGuid (16 octets)
    smb.extend_from_slice(&0u64.to_le_bytes()); // ClientStartTime
    for d in DIALECTES_SMB2 {
        smb.extend_from_slice(&d.to_le_bytes());
    }
    let mut trame = Vec::with_capacity(4 + smb.len());
    trame.push(0);
    let n = smb.len() as u32;
    trame.extend_from_slice(&n.to_be_bytes()[1..]);
    trame.extend_from_slice(&smb);
    trame
}

/// Le `ServerGuid` d'une reponse NEGOTIATE (trame NetBIOS comprise).
///
/// `None` si la reponse n'est pas un NEGOTIATE SMB2 reussi, ou si le GUID est
/// nul : un GUID nul ne distingue personne, le prendre pour une identite
/// ferait confondre deux serveurs.
pub fn guid_de_la_reponse(trame: &[u8]) -> Option<GuidServeur> {
    let smb = trame.get(4..)?;
    if smb.get(0..4)? != [0xFE, b'S', b'M', b'B'] {
        return None;
    }
    let statut = u32::from_le_bytes(smb.get(8..12)?.try_into().ok()?);
    let commande = u16::from_le_bytes(smb.get(12..14)?.try_into().ok()?);
    if statut != 0 || commande != 0 {
        return None;
    }
    let corps = smb.get(64..)?;
    if u16::from_le_bytes(corps.get(0..2)?.try_into().ok()?) != 65 {
        return None;
    }
    let guid: GuidServeur = corps.get(8..24)?.try_into().ok()?;
    (guid != [0u8; 16]).then_some(guid)
}

/// Demander son `ServerGuid` au serveur SMB `hote:port`.
///
/// C'est ce qui reconnait un meme NAS sous deux adresses (fil 2145 : un
/// Synology vu en IPv6 par la decouverte, puis en IPv4 par la saisie). Samba
/// derive ce GUID du nom NetBIOS du serveur (`smbd_server_guid`), Windows le
/// garde en registre : il est donc le meme sur toutes les interfaces d'une
/// machine, quelle que soit l'adresse par laquelle on l'interroge.
///
/// `None` si le serveur ne repond pas, ou seulement en SMB1 : on ne sait
/// alors pas conclure, et l'appelant ne doit rien fusionner.
pub async fn guid_du_serveur(hote: &str, port: u16) -> Option<GuidServeur> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hote = hote.trim_start_matches('[').trim_end_matches(']');
    let essai = async {
        let mut flux = tokio::net::TcpStream::connect((hote, port)).await.ok()?;
        flux.write_all(&requete_negotiate()).await.ok()?;
        let mut entete = [0u8; 4];
        flux.read_exact(&mut entete).await.ok()?;
        let n = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]) as usize;
        if n > 64 * 1024 {
            return None;
        }
        let mut reste = vec![0u8; n];
        flux.read_exact(&mut reste).await.ok()?;
        let mut trame = entete.to_vec();
        trame.extend_from_slice(&reste);
        guid_de_la_reponse(&trame)
    };
    tokio::time::timeout(Duration::from_secs(3), essai)
        .await
        .ok()
        .flatten()
}

/// Le chemin est-il reellement un point de montage ?
///
/// Le garde-fou « deja monte » du remontage testait `read_dir().next().
/// is_some()` : *il y a des fichiers, donc c'est monte*. Un point de montage
/// non monte mais portant des residus — le scan a ecrit dedans pendant que le
/// partage etait tombe, ou un `mount` precedent a laisse des fichiers — faisait
/// donc sauter le remontage **sans un mot**, et l'utilisateur se retrouvait
/// avec une bibliotheque a moitie lisible que rien n'expliquait.
///
/// Le test juste est celui de `mountpoint(1)` : un point de montage ne porte
/// pas le meme peripherique que son parent. Il ne depend d'aucun format de
/// fichier systeme, donc il vaut sur Linux comme sur macOS.
///
/// La racine `/` est son propre parent : elle est donc toujours vue comme
/// montee, ce qui est exact — et de toute facon aucun partage n'y est monte.
#[cfg(unix)]
pub fn est_un_point_de_montage(chemin: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(ici) = std::fs::metadata(chemin) else {
        return false;
    };
    let Some(parent) = chemin.parent() else {
        return true;
    };
    match std::fs::metadata(parent) {
        Ok(dessus) => ici.dev() != dessus.dev(),
        // Parent illisible : on ne peut pas conclure. Repondre « non monte »
        // ferait retenter un montage par-dessus un montage existant.
        Err(_) => true,
    }
}

/// Windows ne monte pas de partage CIFS par cette route (`mount.cifs` n'y
/// existe pas) ; la question ne s'y pose donc jamais.
#[cfg(not(unix))]
pub fn est_un_point_de_montage(_chemin: &std::path::Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_refus_d_identifiants_arrete_les_essais() {
        assert!(est_refus_d_authentification(
            "mount error(13): Permission denied"
        ));
        assert!(est_refus_d_authentification("Access denied"));
        assert!(est_refus_d_authentification(
            "mount error: bad user name or password"
        ));
    }

    /// Le cas de Philippe Landes : `mount error(22): Invalid argument`, obtenu
    /// avec vers=3.0, puis en negociation libre, puis avec vers=2.0 — alors que
    /// `smbclient -L` listait le partage avec les MEMES identifiants. Si ce
    /// message etait pris pour un refus d'authentification, la boucle
    /// s'arreterait au premier essai et n'atteindrait jamais le dialecte qui
    /// marche : le correctif ne corrigerait rien.
    #[test]
    fn un_dialecte_inadapte_laisse_la_boucle_continuer() {
        assert!(!est_refus_d_authentification(
            "mount error(22): Invalid argument"
        ));
        assert!(!est_refus_d_authentification(
            "mount error(112): Host is down"
        ));
        assert!(!est_refus_d_authentification("Device or resource busy"));
        assert!(!est_refus_d_authentification(""));
    }

    /// Fil 2145 (Daniel Levy) : `negocie` rendait `mount error(16): Device or
    /// resource busy`, l'echelle continuait sur 2.0 puis 1.0, et l'utilisateur
    /// lisait l'`error(95)` du dernier essai. L'EBUSY doit arreter l'echelle.
    #[test]
    fn un_point_deja_occupe_arrete_l_echelle() {
        let ebusy = "mount error(16): Device or resource busy\n\
                     Refer to the mount.cifs(8) manual page (e.g. man mount.cifs)";
        assert!(est_deja_monte(ebusy));
        assert!(arrete_l_echelle(ebusy));
        assert!(arrete_l_echelle("mount error(13): Permission denied"));
        // Un dialecte refuse, lui, doit laisser l'echelle continuer.
        assert!(!arrete_l_echelle(
            "mount error(95): Operation not supported"
        ));
        assert!(!arrete_l_echelle("mount error(22): Invalid argument"));
        assert!(!est_deja_monte("mount error(112): Host is down"));
    }

    #[test]
    fn la_source_d_un_point_se_lit_dans_la_table_des_montages() {
        let table = "\
proc /proc proc rw,nosuid 0 0
//192.168.10.69/Music /mnt/192.168.10.69_Music cifs rw,vers=3.1.1 0 0
//fd12::1/Ma\\040Musique /mnt/fd12::1_Ma_Musique cifs rw 0 0
tmpfs /mnt/empile tmpfs rw 0 0
//nas/A /mnt/empile cifs rw 0 0
";
        assert_eq!(
            source_dans_la_table(table, "/mnt/192.168.10.69_Music").as_deref(),
            Some("//192.168.10.69/Music")
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/192.168.10.69_Music/").as_deref(),
            Some("//192.168.10.69/Music"),
            "une barre finale ne change pas le point"
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/fd12::1_Ma_Musique").as_deref(),
            Some("//fd12::1/Ma Musique"),
            "les echappements octaux sont defaits"
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/empile").as_deref(),
            Some("//nas/A"),
            "le dernier montage empile est celui qu'on voit"
        );
        assert_eq!(source_dans_la_table(table, "/mnt/absent"), None);
    }

    #[test]
    fn la_meme_source_s_ecrit_de_plusieurs_facons() {
        assert!(meme_source(
            "//192.168.10.69/Music",
            "192.168.10.69",
            "Music"
        ));
        assert!(meme_source(
            "//192.168.10.69/music/",
            "192.168.10.69",
            "Music"
        ));
        assert!(meme_source(r"\\NAS\Music", "nas", "music"));
        assert!(meme_source("//fd12::1/Music", "[fd12::1]", "Music"));
        assert!(!meme_source(
            "//192.168.10.69/Video",
            "192.168.10.69",
            "Music"
        ));
        assert!(!meme_source(
            "//192.168.10.70/Music",
            "192.168.10.69",
            "Music"
        ));
        assert!(!meme_source("tmpfs", "192.168.10.69", "Music"));
    }

    /// Une reponse NEGOTIATE SMB2 telle qu'un serveur la rend.
    fn reponse_negotiate(guid: GuidServeur, statut: u32) -> Vec<u8> {
        let mut smb = vec![0u8; 64];
        smb[0..4].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        smb[4..6].copy_from_slice(&64u16.to_le_bytes());
        smb[8..12].copy_from_slice(&statut.to_le_bytes());
        let mut corps = vec![0u8; 64];
        corps[0..2].copy_from_slice(&65u16.to_le_bytes());
        corps[4..6].copy_from_slice(&0x0302u16.to_le_bytes());
        corps[8..24].copy_from_slice(&guid);
        smb.extend_from_slice(&corps);
        let mut trame = vec![0u8];
        trame.extend_from_slice(&(smb.len() as u32).to_be_bytes()[1..]);
        trame.extend_from_slice(&smb);
        trame
    }

    #[test]
    fn la_requete_negotiate_est_bien_cadree() {
        let r = requete_negotiate();
        let n = u32::from_be_bytes([0, r[1], r[2], r[3]]) as usize;
        assert_eq!(r[0], 0);
        assert_eq!(n, r.len() - 4, "longueur NetBIOS");
        assert_eq!(&r[4..8], &[0xFE, b'S', b'M', b'B']);
        assert_eq!(u16::from_le_bytes([r[68], r[69]]), 36, "StructureSize");
        assert_eq!(n, 64 + 36 + 2 * 4);
    }

    #[test]
    fn le_guid_se_lit_dans_la_reponse() {
        let guid = *b"daniel-synology!";
        assert_eq!(guid_de_la_reponse(&reponse_negotiate(guid, 0)), Some(guid));
        // Un refus (STATUS_NOT_SUPPORTED) ou un GUID nul n'identifient rien.
        assert_eq!(
            guid_de_la_reponse(&reponse_negotiate(guid, 0xC00000BB)),
            None
        );
        assert_eq!(guid_de_la_reponse(&reponse_negotiate([0; 16], 0)), None);
        // Une reponse SMB1 (0xFF 'SMB') non plus.
        let mut smb1 = reponse_negotiate(guid, 0);
        smb1[4] = 0xFF;
        assert_eq!(guid_de_la_reponse(&smb1), None);
        assert_eq!(guid_de_la_reponse(&[0, 0]), None);
    }

    /// Un faux serveur SMB2 qui rend `guid` a chaque NEGOTIATE.
    async fn faux_serveur(adresse: &str, guid: GuidServeur) -> Option<u16> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ecoute = tokio::net::TcpListener::bind((adresse, 0)).await.ok()?;
        let port = ecoute.local_addr().ok()?.port();
        tokio::spawn(async move {
            while let Ok((mut flux, _)) = ecoute.accept().await {
                let mut entete = [0u8; 4];
                if flux.read_exact(&mut entete).await.is_err() {
                    continue;
                }
                let n = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]) as usize;
                let mut reste = vec![0u8; n];
                let _ = flux.read_exact(&mut reste).await;
                let _ = flux.write_all(&reponse_negotiate(guid, 0)).await;
            }
        });
        Some(port)
    }

    /// Le cas du fil 2145 rejoue : le meme serveur interroge par son IPv4 et
    /// par son IPv6 rend le meme GUID ; un autre serveur, un autre.
    #[tokio::test]
    async fn le_meme_serveur_se_reconnait_par_ses_deux_adresses() {
        let nas = *b"daniel-synology!";
        let p4 = faux_serveur("127.0.0.1", nas).await.expect("ecoute IPv4");
        let par_ipv4 = guid_du_serveur("127.0.0.1", p4).await;
        assert_eq!(par_ipv4, Some(nas));
        if let Some(p6) = faux_serveur("::1", nas).await {
            assert_eq!(guid_du_serveur("[::1]", p6).await, par_ipv4);
        }
        let autre = faux_serveur("127.0.0.1", *b"un-autre-serveur")
            .await
            .unwrap();
        assert_ne!(guid_du_serveur("127.0.0.1", autre).await, par_ipv4);
    }

    #[tokio::test]
    async fn un_serveur_muet_n_a_pas_d_identite() {
        // Port ferme : on ne sait pas, et on le dit.
        let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ecoute.local_addr().unwrap().port();
        drop(ecoute);
        assert_eq!(guid_du_serveur("127.0.0.1", port).await, None);
    }

    /// La casse de `mount.cifs` varie selon les versions.
    #[test]
    fn la_casse_du_message_ne_change_rien() {
        assert!(est_refus_d_authentification("PERMISSION DENIED"));
        assert!(est_refus_d_authentification(
            "Mount Error(13): Permission Denied"
        ));
    }

    #[test]
    fn sans_dialecte_connu_l_echelle_est_celle_d_origine() {
        assert_eq!(echelle(None), DIALECTES.to_vec());
    }

    /// Le cas de Philippe apres son premier montage reussi : SMB 1.0 est en
    /// base, il doit repasser en premier. Sans cela, chaque demarrage rejoue
    /// deux essais de dix secondes avant d'arriver au seul qui marche.
    #[test]
    fn le_dialecte_connu_passe_en_premier_sans_perdre_les_autres() {
        let ordre = echelle(Some("1.0"));
        assert_eq!(ordre.first(), Some(&Some("1.0")));
        assert_eq!(
            ordre.len(),
            DIALECTES.len(),
            "aucun dialecte perdu : {ordre:?}"
        );
        for d in DIALECTES {
            assert!(ordre.contains(&d), "{d:?} manque dans {ordre:?}");
        }
    }

    /// La negociation libre est un dialecte comme un autre une fois retenue.
    #[test]
    fn la_negociation_libre_se_relit_depuis_la_base() {
        assert_eq!(etiquette(None), "negocie");
        assert_eq!(depuis_etiquette("negocie"), None);
        assert_eq!(depuis_etiquette(""), None);
        assert_eq!(depuis_etiquette("1.0"), Some("1.0"));
        assert_eq!(echelle(Some("negocie")).first(), Some(&None));
    }

    /// Une valeur aberrante en base — colonne editee a la main, migration
    /// bancale — ne doit pas devenir une option `vers=` passee au noyau.
    #[test]
    fn un_dialecte_inconnu_en_base_est_ignore() {
        let ordre = echelle(Some("4.2"));
        assert_eq!(ordre, DIALECTES.to_vec());
    }

    /// `/tmp` n'est pas un point de montage sur toutes les machines, mais un
    /// repertoire quelconque cree dans le repertoire temporaire n'en est
    /// JAMAIS un — c'est exactement le cas que l'ancien garde-fou confondait
    /// des qu'il contenait un fichier.
    #[cfg(unix)]
    #[test]
    fn un_repertoire_avec_des_residus_n_est_pas_un_point_de_montage() {
        let base = tune_core::test_scratch::scratch_dir("tune_smb_test");
        std::fs::write(base.join("residu.flac"), b"x").unwrap();
        assert!(
            !est_un_point_de_montage(&base),
            "un repertoire ordinaire portant des fichiers a ete pris pour un montage"
        );
    }

    #[cfg(unix)]
    #[test]
    fn un_chemin_absent_n_est_pas_un_point_de_montage() {
        assert!(!est_un_point_de_montage(std::path::Path::new(
            "/n/existe/pas/du/tout"
        )));
    }
}
