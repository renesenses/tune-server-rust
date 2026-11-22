//! Le dossier de données est-il persistant ? (renesenses/tune-server-rust#8747)
//!
//! ## Le défaut
//!
//! Dans l'image Docker, tout l'état de Tune vit dans `/data` : la base
//! (`TUNE_DB_PATH=/data/tune.db`), les pochettes, les greffons, les jetons des
//! services. L'image déclare `VOLUME ["/data"]`. Si l'utilisateur n'y monte
//! RIEN (ni dossier de l'hôte, ni volume nommé), Docker crée un volume
//! ANONYME. Un conteneur recréé — `docker run` relancé, outil de mise à jour,
//! « Image › Mettre à jour » de Container Manager sur Synology (fil 2798) —
//! reçoit un NOUVEAU volume anonyme, vide : Tune démarre sur une base neuve,
//! bibliothèque et services perdus, et rien ne le disait.
//!
//! ## Ce que fait ce module
//!
//! Au démarrage, dans un conteneur seulement, il lit `/proc/self/mountinfo`
//! et classe le dossier de la base :
//!
//! * `host_dir` : un dossier de l'hôte lié au conteneur (persistant) ;
//! * `named_volume` : un volume nommé (persistant) ;
//! * `anonymous_volume` : un volume anonyme (perdu à la recréation) ;
//! * `not_mounted` : rien n'est monté, le dossier est dans la couche du
//!   conteneur (perdu à la recréation) ;
//! * `tmpfs` : un montage en mémoire (perdu à l'arrêt).
//!
//! Les trois derniers donnent un `WARN donnees_non_persistantes` au journal,
//! une ligne du rapport de bogue, et `data_persistence` dans `GET /system/env`,
//! que l'interface montre en bandeau.
//!
//! La classification est une fonction PURE sur le texte de `mountinfo`, pour
//! se tester sans conteneur. Elle ne fait que lire : rien n'est déplacé ni
//! refusé, un faux diagnostic ne casserait rien.
//!
//! ## Les formes reconnues
//!
//! Une ligne de `mountinfo` (proc(5)) : `id parent maj:min racine point
//! options… - type source options-fs`. La « racine » est le chemin DANS le
//! système de fichiers source. Un volume Docker ou Podman est un dossier
//! `…/volumes/<nom>/_data` du moteur : `/var/lib/docker/volumes/<nom>/_data`
//! quand `/var/lib/docker` est sur la partition racine, `/volumes/<nom>/_data`
//! quand c'est une partition à part, `/@docker/volumes/<nom>/_data` sur un
//! Synology (`/volume1/@docker`). Le nom d'un volume anonyme est un
//! identifiant de 64 hexadécimaux. Tout autre montage est un dossier de
//! l'hôte.

use serde_json::{Value, json};
use std::path::Path;

/// Ce que devient le dossier de données quand le conteneur est recréé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistance {
    DossierHote,
    VolumeNomme,
    VolumeAnonyme,
    NonMonte,
    Tmpfs,
}

impl Persistance {
    /// Le code stable publié par l'API.
    pub fn code(self) -> &'static str {
        match self {
            Persistance::DossierHote => "host_dir",
            Persistance::VolumeNomme => "named_volume",
            Persistance::VolumeAnonyme => "anonymous_volume",
            Persistance::NonMonte => "not_mounted",
            Persistance::Tmpfs => "tmpfs",
        }
    }

    /// Les données survivent-elles à la recréation du conteneur ?
    pub fn persistante(self) -> bool {
        matches!(self, Persistance::DossierHote | Persistance::VolumeNomme)
    }
}

/// Un montage lu dans `mountinfo`.
#[derive(Debug)]
struct Montage {
    racine: String,
    point: String,
    type_fs: String,
}

/// `mountinfo` échappe espace, tabulation, saut de ligne et barre oblique
/// inverse en octal (`\040`, `\011`, `\012`, `\134`).
fn desechapper(s: &str) -> String {
    let octets = s.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len());
    let mut i = 0;
    while i < octets.len() {
        if octets[i] == b'\\'
            && i + 3 < octets.len()
            && octets[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let v = (octets[i + 1] - b'0') as u32 * 64
                + (octets[i + 2] - b'0') as u32 * 8
                + (octets[i + 3] - b'0') as u32;
            if let Ok(b) = u8::try_from(v) {
                sortie.push(b);
                i += 4;
                continue;
            }
        }
        sortie.push(octets[i]);
        i += 1;
    }
    String::from_utf8_lossy(&sortie).into_owned()
}

fn lire_montages(mountinfo: &str) -> Vec<Montage> {
    mountinfo
        .lines()
        .filter_map(|ligne| {
            let champs: Vec<&str> = ligne.split(' ').collect();
            let sep = champs.iter().position(|c| *c == "-")?;
            if champs.len() < 5 || sep < 5 {
                return None;
            }
            Some(Montage {
                racine: desechapper(champs[3]),
                point: desechapper(champs[4]),
                type_fs: champs.get(sep + 1).copied().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// `dossier` est-il `point` ou en dessous, composant par composant ?
fn sous_le_point(point: &str, dossier: &str) -> bool {
    if point == "/" {
        return true;
    }
    let point = point.trim_end_matches('/');
    dossier == point
        || dossier
            .strip_prefix(point)
            .is_some_and(|reste| reste.starts_with('/'))
}

/// Le nom du volume quand `racine` est le `_data` d'un volume de moteur.
fn nom_de_volume(racine: &str) -> Option<&str> {
    let sans_data = racine.trim_end_matches('/').strip_suffix("/_data")?;
    let (avant, nom) = sans_data.rsplit_once('/')?;
    (avant == "/volumes" || avant.ends_with("/volumes")).then_some(nom)
}

/// Docker et Podman nomment un volume anonyme par 64 hexadécimaux.
fn est_anonyme(nom: &str) -> bool {
    nom.len() == 64 && nom.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Le sort de `dossier` d'après `mountinfo`, ou `None` si aucun montage ne le
/// couvre (texte vide ou illisible : on ne conclut rien).
pub fn classer(mountinfo: &str, dossier: &str) -> Option<Persistance> {
    // Le montage le plus profond qui couvre le dossier ; à profondeur égale,
    // le dernier listé, qui recouvre les précédents.
    let montage = lire_montages(mountinfo)
        .into_iter()
        .filter(|m| sous_le_point(&m.point, dossier))
        .fold(None::<Montage>, |meilleur, m| match meilleur {
            Some(b) if b.point.len() > m.point.len() => Some(b),
            _ => Some(m),
        })?;
    if montage.point == "/" {
        return Some(Persistance::NonMonte);
    }
    if montage.type_fs == "tmpfs" || montage.type_fs == "ramfs" {
        return Some(Persistance::Tmpfs);
    }
    Some(match nom_de_volume(&montage.racine) {
        Some(nom) if est_anonyme(nom) => Persistance::VolumeAnonyme,
        Some(_) => Persistance::VolumeNomme,
        None => Persistance::DossierHote,
    })
}

/// Le relevé publié : le dossier examiné et son sort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Releve {
    pub dossier: String,
    pub persistance: Persistance,
}

/// Relevé à partir de ce qu'on a lu : `None` hors conteneur (une machine
/// ordinaire n'a pas de « recréation ») ou quand rien ne se conclut.
pub fn releve_avec(conteneur: Option<&str>, mountinfo: &str, db_path: &str) -> Option<Releve> {
    conteneur?;
    let dossier = tune_core::coffre::dossier_de_la_base(db_path);
    // Le chemin VU du conteneur, sans `..` ni lien : c'est ce que liste
    // `mountinfo`. Un dossier introuvable garde son chemin écrit.
    let dossier = std::fs::canonicalize(&dossier).unwrap_or(dossier);
    let dossier = dossier.to_string_lossy().into_owned();
    if !dossier.starts_with('/') {
        return None;
    }
    let persistance = classer(mountinfo, &dossier)?;
    Some(Releve {
        dossier,
        persistance,
    })
}

/// Relevé de CE processus : conteneur détecté et `/proc/self/mountinfo`.
pub fn relever(db_path: &str) -> Option<Releve> {
    relever_depuis(
        crate::conteneur::detecter(),
        Path::new("/proc/self/mountinfo"),
        db_path,
    )
}

/// [`relever`] avec la table des montages lue dans `mountinfo`.
pub fn relever_depuis(conteneur: Option<&str>, mountinfo: &Path, db_path: &str) -> Option<Releve> {
    conteneur?;
    let texte = std::fs::read_to_string(mountinfo).ok()?;
    releve_avec(conteneur, &texte, db_path)
}

/// Le champ `data_persistence` de `GET /system/env` : `null` hors conteneur.
pub fn en_json(releve: Option<&Releve>) -> Value {
    match releve {
        None => Value::Null,
        Some(r) => json!({
            "dir": r.dossier,
            "kind": r.persistance.code(),
            "persistent": r.persistance.persistante(),
        }),
    }
}

/// La ligne du rapport de bogue, ou `None` hors conteneur.
pub fn ligne_du_rapport(releve: Option<&Releve>) -> Option<String> {
    let r = releve?;
    let sort = match r.persistance {
        Persistance::DossierHote => "host folder (persistent)",
        Persistance::VolumeNomme => "named volume (persistent)",
        Persistance::VolumeAnonyme => {
            "ANONYMOUS volume (NOT persistent: lost when the container is recreated)"
        }
        Persistance::NonMonte => {
            "NOT MOUNTED (container layer: lost when the container is recreated)"
        }
        Persistance::Tmpfs => "tmpfs (NOT persistent: lost when the container stops)",
    };
    Some(format!("**Data dir**: {} — {sort}\n", r.dossier))
}

/// Au démarrage : le dire au journal quand les données ne survivront pas.
pub fn journaliser_au_demarrage(db_path: &str) -> Option<Releve> {
    let releve = relever(db_path);
    match &releve {
        Some(r) if !r.persistance.persistante() => tracing::warn!(
            dossier = %r.dossier,
            sort = r.persistance.code(),
            "donnees_non_persistantes : la base et les reglages seront perdus a la \
             recreation du conteneur (mise a jour de l'image comprise). Montez ce \
             dossier sur un dossier de l'hote ou un volume nomme (-v tune-data:/data)."
        ),
        Some(r) => tracing::info!(
            dossier = %r.dossier,
            sort = r.persistance.code(),
            "donnees_persistantes"
        ),
        None => {}
    }
    releve
}

#[cfg(test)]
mod tests {
    use super::*;

    const RACINE_OVERLAY: &str = "1043 960 0:229 / / rw,relatime master:500 - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/ABC,upperdir=/var/lib/docker/overlay2/x/diff,workdir=/var/lib/docker/overlay2/x/work\n\
1044 1043 0:232 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n\
1050 1043 8:1 /var/lib/docker/containers/abc/resolv.conf /etc/resolv.conf rw,relatime - ext4 /dev/sda1 rw\n";

    const ANONYME: &str = "3f1c0a9b8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a3928170615243a";

    fn avec(ligne: &str) -> String {
        format!("{RACINE_OVERLAY}{ligne}\n")
    }

    /// Fil 2798 : `/data` sans `-v` → volume anonyme de l'image.
    /// Contre-épreuve : faire rendre `false` à `est_anonyme` fait rougir ce
    /// test (le volume passe pour nommé, donc persistant).
    #[test]
    fn volume_anonyme_de_l_image() {
        let mi = avec(&format!(
            "1060 1043 8:1 /var/lib/docker/volumes/{ANONYME}/_data /data rw,relatime master:1 - ext4 /dev/sda1 rw"
        ));
        assert_eq!(classer(&mi, "/data"), Some(Persistance::VolumeAnonyme));
        assert!(!Persistance::VolumeAnonyme.persistante());
    }

    #[test]
    fn volume_nomme_de_la_documentation() {
        let mi = avec(
            "1060 1043 8:1 /var/lib/docker/volumes/tune-data/_data /data rw,relatime - ext4 /dev/sda1 rw",
        );
        assert_eq!(classer(&mi, "/data"), Some(Persistance::VolumeNomme));
        // Compose préfixe le nom du projet : toujours un volume nommé.
        let mi =
            avec("1060 1043 8:1 /volumes/tune-server_tune-data/_data /data rw - xfs /dev/sdb1 rw");
        assert_eq!(classer(&mi, "/data"), Some(Persistance::VolumeNomme));
    }

    /// Synology : `/volume1/@docker/volumes/<id>/_data` sur un btrfs, dont la
    /// racine listée commence au sous-volume.
    #[test]
    fn synology_volume_anonyme_et_dossier_partage() {
        let mi = avec(&format!(
            "1060 1043 0:45 /@docker/volumes/{ANONYME}/_data /data rw,relatime - btrfs /dev/mapper/cachedev_0 rw"
        ));
        assert_eq!(classer(&mi, "/data"), Some(Persistance::VolumeAnonyme));
        let mi = avec(
            "1060 1043 0:45 /docker/tune/data /data rw,relatime - btrfs /dev/mapper/cachedev_0 rw",
        );
        assert_eq!(classer(&mi, "/data"), Some(Persistance::DossierHote));
    }

    #[test]
    fn dossier_de_l_hote() {
        let mi = avec("1060 1043 8:1 /home/jon/tune-data /data rw,relatime - ext4 /dev/sda1 rw");
        assert_eq!(classer(&mi, "/data"), Some(Persistance::DossierHote));
        assert!(Persistance::DossierHote.persistante());
    }

    /// Rien de monté sur `/data` : le seul montage qui le couvre est `/`.
    /// Contre-épreuve : retirer le bras `point == "/"` fait rougir ce test
    /// (la couche overlay passe pour un dossier de l'hôte).
    #[test]
    fn rien_de_monte_sur_data() {
        assert_eq!(
            classer(RACINE_OVERLAY, "/data"),
            Some(Persistance::NonMonte)
        );
        // `/data2` monté ne couvre pas `/data`.
        let mi = avec("1060 1043 8:1 /srv/x /data2 rw - ext4 /dev/sda1 rw");
        assert_eq!(classer(&mi, "/data"), Some(Persistance::NonMonte));
    }

    #[test]
    fn tmpfs_n_est_pas_persistant() {
        let mi = avec("1060 1043 0:300 / /data rw,nosuid - tmpfs tmpfs rw,size=65536k");
        assert_eq!(classer(&mi, "/data"), Some(Persistance::Tmpfs));
    }

    /// Le montage le plus profond l'emporte : `/data` sur un volume nommé,
    /// `/data/artwork_cache` sur un tmpfs, la base est dans `/data`.
    #[test]
    fn le_montage_le_plus_profond_l_emporte() {
        let mi = avec(
            "1060 1043 8:1 /var/lib/docker/volumes/tune-data/_data /data rw - ext4 /dev/sda1 rw\n\
1061 1060 0:300 / /data/artwork_cache rw - tmpfs tmpfs rw",
        );
        assert_eq!(classer(&mi, "/data"), Some(Persistance::VolumeNomme));
        assert_eq!(
            classer(&mi, "/data/artwork_cache"),
            Some(Persistance::Tmpfs)
        );
    }

    #[test]
    fn un_chemin_avec_espace_est_desechappe() {
        let mi =
            avec("1060 1043 8:1 /home/jon/Tune\\040data /mes\\040donnees rw - ext4 /dev/sda1 rw");
        assert_eq!(classer(&mi, "/mes donnees"), Some(Persistance::DossierHote));
    }

    #[test]
    fn un_texte_illisible_ne_conclut_rien() {
        assert_eq!(classer("", "/data"), None);
        assert_eq!(classer("n'importe quoi\n", "/data"), None);
    }

    /// Hors conteneur : aucun relevé, quel que soit `mountinfo`.
    #[test]
    fn hors_conteneur_rien_n_est_releve() {
        assert_eq!(releve_avec(None, RACINE_OVERLAY, "/data/tune.db"), None);
        assert_eq!(en_json(None), Value::Null);
        assert_eq!(ligne_du_rapport(None), None);
    }

    /// Épreuve de bout en bout sur un FAUX `mountinfo` écrit dans un dossier
    /// jetable (#3030) : la base dans un dossier qui n'est pas monté.
    #[test]
    fn faux_mountinfo_de_bout_en_bout() {
        let dossier = tune_core::test_scratch::scratch_dir("tune_8747_mountinfo");
        let donnees = dossier.join("data");
        std::fs::create_dir_all(&donnees).unwrap();
        let donnees = std::fs::canonicalize(&donnees).unwrap();
        let db = donnees.join("tune.db");
        let chemin = dossier.join("mountinfo");

        std::fs::write(&chemin, RACINE_OVERLAY).unwrap();
        let r = relever_depuis(Some("docker"), &chemin, db.to_str().unwrap()).unwrap();
        assert_eq!(r.persistance, Persistance::NonMonte);
        assert_eq!(r.dossier, donnees.to_string_lossy());
        let v = en_json(Some(&r));
        assert_eq!(v["kind"], "not_mounted");
        assert_eq!(v["persistent"], false);
        assert!(ligne_du_rapport(Some(&r)).unwrap().contains("NOT MOUNTED"));

        // Contre-épreuve : le même dossier lié depuis l'hôte est persistant.
        let point = donnees.to_string_lossy().replace(' ', "\\040");
        std::fs::write(
            &chemin,
            avec(&format!(
                "1060 1043 8:1 /srv/tune-data {point} rw - ext4 /dev/sda1 rw"
            )),
        )
        .unwrap();
        let r = relever_depuis(Some("docker"), &chemin, db.to_str().unwrap()).unwrap();
        assert_eq!(r.persistance, Persistance::DossierHote);
        assert_eq!(en_json(Some(&r))["persistent"], true);

        // Hors conteneur, le fichier n'est même pas lu.
        assert_eq!(relever_depuis(None, &chemin, db.to_str().unwrap()), None);
    }
}
