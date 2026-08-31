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
