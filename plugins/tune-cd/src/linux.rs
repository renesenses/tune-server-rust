//! Le lecteur Linux : ioctl `CDROMREADTOCHDR`, `CDROMREADTOCENTRY`,
//! `CDROMREADAUDIO` et `CDROM_DRIVE_STATUS` sur `/dev/sr*` (`linux/cdrom.h`),
//! et `CDROMEJECT` pour éjecter (fil 2135). Un Apple SuperDrive reçoit
//! en plus sa commande d'éveil par `SG_IO` (#5729, voir
//! [`CDB_EVEIL_SUPERDRIVE`]).
//!
//! Aucun outil externe pour lire : ni `cdparanoia`, ni `cdda2wav`, ni ffmpeg.
//! L'éjection seule se replie sur la commande `eject` si l'ioctl est refusé
//! (voir [`LecteurLinux::ejecter_disque`]). C'est la
//! seule partie du greffon qui parle au noyau ; elle ne se prouve que sur une
//! machine équipée d'un lecteur (voir la procédure de test manuel de la PR).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::lecteur::{ErreurCd, ErreurEjection, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, PisteToc, Toc};

// linux/cdrom.h
const CDROMREADTOCHDR: u64 = 0x5305;
const CDROMREADTOCENTRY: u64 = 0x5306;
const CDROMREADAUDIO: u64 = 0x530e;
const CDROM_DRIVE_STATUS: u64 = 0x5326;
const CDROMEJECT: u64 = 0x5309;
const CDROM_LOCKDOOR: u64 = 0x5329;
const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;
const CDROM_DATA_TRACK: u8 = 0x04;
const CDSL_CURRENT: libc::c_int = i32::MAX;
const CDS_DISC_OK: libc::c_int = 4;

// scsi/sg.h
const SG_IO: u64 = 0x2285;
const SG_DXFER_NONE: libc::c_int = -1;
/// Délai de la commande d'éveil, en millisecondes.
const DELAI_EVEIL_MS: u32 = 5_000;

/// #5729 — la commande vendeur qui « éveille » un Apple SuperDrive.
///
/// Hors d'un Mac, le SuperDrive refuse ou recrache tout disque tant qu'il
/// n'a pas reçu cette commande : c'est l'équivalent de
/// `sg_raw /dev/srN EA 00 00 00 00 00 01`, que la règle udev connue des
/// distributions envoie au branchement. Aucun transfert de données.
pub const CDB_EVEIL_SUPERDRIVE: [u8; 7] = [0xEA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];

/// `struct sg_io_hdr` (`scsi/sg.h`).
#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: libc::c_int,
    duration: u32,
    info: u32,
}

/// Ce qui envoie la commande d'éveil au périphérique `chemin`. Injecté dans
/// les tests ; [`eveiller_par_sg_io`] sinon.
pub type Eveil = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// #5729 — les lecteurs déjà éveillés, par chemin. Un [`LecteurLinux`]
/// neuf est créé à chaque recherche de lecteur (toutes les 2 s tant que le
/// lecteur est vide) : l'éveil se compte donc pour le PÉRIPHÉRIQUE, pas pour
/// l'objet. Un lecteur qui disparaît (débranché) en sort, et sera éveillé
/// de nouveau à son retour.
static LECTEURS_EVEILLES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// #5729 — `vendor` et `model` (sysfs) désignent-ils un Apple SuperDrive ?
/// Le noyau les complète d'espaces (`"Apple   "`, `"SuperDrive      "`).
pub fn est_un_superdrive(fabricant: &str, modele: &str) -> bool {
    fabricant.trim().eq_ignore_ascii_case("apple")
        && modele.to_ascii_lowercase().contains("superdrive")
}

/// #5729 — `vendor` et `model` du lecteur `chemin` (`/dev/sr0`), lus dans
/// `<racine_sys>/block/sr0/device/`. `None` si l'un manque.
pub fn identite_scsi(racine_sys: &Path, chemin: &str) -> Option<(String, String)> {
    let nom = Path::new(chemin).file_name()?.to_str()?;
    let dossier = racine_sys.join("block").join(nom).join("device");
    let lire = |f: &str| std::fs::read_to_string(dossier.join(f)).ok();
    Some((lire("vendor")?, lire("model")?))
}

/// Envoie [`CDB_EVEIL_SUPERDRIVE`] à `chemin` par `SG_IO`.
///
/// Le noyau ne laisse passer une commande vendeur qu'avec `CAP_SYS_RAWIO` :
/// c'est le cas du service Tune OS (`User=root`), pas du paquet `.deb`
/// (`User=tune`), où l'appel rend `EPERM` — l'erreur le dit.
pub fn eveiller_par_sg_io(chemin: &str) -> Result<(), String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(chemin)
        .map_err(|e| format!("{chemin} ne s'ouvre pas : {e}"))?;
    let mut sense = [0u8; 32];
    let mut hdr = SgIoHdr {
        interface_id: b'S' as libc::c_int,
        dxfer_direction: SG_DXFER_NONE,
        cmd_len: CDB_EVEIL_SUPERDRIVE.len() as u8,
        mx_sb_len: sense.len() as u8,
        iovec_count: 0,
        dxfer_len: 0,
        dxferp: std::ptr::null_mut(),
        cmdp: CDB_EVEIL_SUPERDRIVE.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: DELAI_EVEIL_MS,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    // SAFETY: `hdr` est une `sg_io_hdr` valide ; `cmdp` pointe sur une
    // constante de 7 octets, `sbp` sur `sense` (32 octets), tous deux
    // vivants le temps de l'appel ; aucun transfert (`SG_DXFER_NONE`).
    if unsafe { libc::ioctl(f.as_raw_fd(), SG_IO as _, &mut hdr as *mut SgIoHdr) } < 0 {
        return Err(format!("SG_IO : {}", derniere_erreur()));
    }
    if hdr.status != 0 || hdr.host_status != 0 || hdr.driver_status != 0 {
        return Err(format!(
            "SG_IO : statut {:#04x}, hôte {:#06x}, pilote {:#06x}",
            hdr.status, hdr.host_status, hdr.driver_status
        ));
    }
    Ok(())
}

#[repr(C)]
#[derive(Default)]
struct CdromTochdr {
    trk0: u8,
    trk1: u8,
}

/// `struct cdrom_tocentry` : `cdte_adr:4` et `cdte_ctrl:4` partagent un octet
/// (adr dans les 4 bits de poids faible sur les ABI petit-boutistes que Tune
/// vise), l'adresse est une union de 4 octets alignée sur 4.
#[repr(C)]
#[derive(Default)]
struct CdromTocentry {
    track: u8,
    adr_ctrl: u8,
    format: u8,
    addr_lba: i32,
    // Jamais lu, mais il fait partie de la disposition noyau.
    #[allow(dead_code)]
    datamode: u8,
}

#[repr(C)]
struct CdromReadAudio {
    addr_lba: i32,
    addr_format: u8,
    nframes: i32,
    buf: *mut u8,
}

pub struct LecteurLinux {
    chemin: String,
    /// Descripteur gardé ouvert entre deux lectures (une ouverture par bloc
    /// de secteurs coûterait un `open` 3 fois par seconde). Rouvert après
    /// toute erreur.
    fichier: Mutex<Option<File>>,
    /// #5729 — où lire `vendor` et `model` (`/sys`).
    racine_sys: PathBuf,
    /// #5729 — l'envoi de la commande d'éveil d'un SuperDrive.
    eveil: Eveil,
}

impl LecteurLinux {
    pub fn new(chemin: String) -> Self {
        Self::avec_eveil(chemin, PathBuf::from("/sys"), Box::new(eveiller_par_sg_io))
    }

    /// Comme [`LecteurLinux::new`], avec la racine sysfs et l'envoi de
    /// l'éveil fournis (tests, #5729).
    pub fn avec_eveil(chemin: String, racine_sys: PathBuf, eveil: Eveil) -> Self {
        Self {
            chemin,
            fichier: Mutex::new(None),
            racine_sys,
            eveil,
        }
    }

    /// #5729 — un Apple SuperDrive reçoit sa commande d'éveil, une fois par
    /// branchement. Un échec est journalisé et n'empêche rien : le lecteur
    /// reste sondé comme les autres.
    fn eveiller_si_superdrive(&self) {
        {
            let mut faits = LECTEURS_EVEILLES.lock().unwrap_or_else(|e| e.into_inner());
            if faits.contains(&self.chemin) {
                return;
            }
            faits.push(self.chemin.clone());
        }
        let Some((fabricant, modele)) = identite_scsi(&self.racine_sys, &self.chemin) else {
            return;
        };
        if !est_un_superdrive(&fabricant, &modele) {
            return;
        }
        match (self.eveil)(&self.chemin) {
            Ok(()) => tracing::info!(lecteur = %self.chemin, "cd_superdrive_eveille"),
            Err(raison) => tracing::warn!(
                lecteur = %self.chemin,
                %raison,
                "cd_superdrive_eveil_refuse : il faut envoyer EA 00 00 00 00 00 01 \
                 en root (sg_raw, ou règle udev) pour qu'il accepte un disque"
            ),
        }
    }

    /// #5729 — le lecteur a disparu : il sera éveillé de nouveau à son retour.
    fn oublier_l_eveil(&self) {
        LECTEURS_EVEILLES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c != &self.chemin);
    }

    fn ouvrir(&self) -> std::io::Result<File> {
        // O_NONBLOCK : un lecteur sans disque s'ouvre quand même, et c'est
        // l'ioctl d'état qui dit « vide ».
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.chemin)
    }

    /// Exécute `f` avec le descripteur, en l'ouvrant au besoin ; le jette
    /// après une erreur pour que la prochaine lecture reparte d'un `open`.
    fn avec_fd<T>(
        &self,
        f: impl FnOnce(libc::c_int) -> Result<T, ErreurCd>,
    ) -> Result<T, ErreurCd> {
        let mut garde = self.fichier.lock().unwrap_or_else(|e| e.into_inner());
        if garde.is_none() {
            *garde =
                Some(self.ouvrir().map_err(|e| {
                    ErreurCd::Autre(format!("{} ne s'ouvre pas : {e}", self.chemin))
                })?);
        }
        let fd = garde.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1);
        let r = f(fd);
        if r.is_err() {
            *garde = None;
        }
        r
    }
}

/// Tous les lecteurs optiques de `dossier` (`/dev`) : les `srN`, dans l'ordre
/// de N (`sr2` avant `sr10`). Fil 2135 : seul `/dev/sr0..3` était regardé,
/// et seul le premier présent était gardé.
pub fn peripheriques_optiques(dossier: &std::path::Path) -> Vec<String> {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return Vec::new();
    };
    let mut trouves: Vec<(u32, String)> = entrees
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let nom = e.file_name().into_string().ok()?;
            let n = nom.strip_prefix("sr")?;
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((
                n.parse().ok()?,
                dossier.join(&nom).to_string_lossy().into_owned(),
            ))
        })
        .collect();
    trouves.sort();
    trouves.into_iter().map(|(_, c)| c).collect()
}

fn derniere_erreur() -> String {
    std::io::Error::last_os_error().to_string()
}

impl LecteurDisque for LecteurLinux {
    fn chemin(&self) -> String {
        self.chemin.clone()
    }

    fn presence(&self) -> Presence {
        let Ok(f) = self.ouvrir() else {
            self.oublier_l_eveil();
            return Presence::AucunLecteur;
        };
        self.eveiller_si_superdrive();
        // SAFETY: ioctl sans pointeur ; l'argument est un entier.
        let etat = unsafe { libc::ioctl(f.as_raw_fd(), CDROM_DRIVE_STATUS as _, CDSL_CURRENT) };
        if etat == CDS_DISC_OK {
            Presence::Disque
        } else {
            Presence::Vide
        }
    }

    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        if self.presence() != Presence::Disque {
            return Err(ErreurCd::AucunDisque);
        }
        self.avec_fd(|fd| {
            let mut hdr = CdromTochdr::default();
            // SAFETY: `hdr` est une `cdrom_tochdr` valide, vivante le temps de l'appel.
            if unsafe { libc::ioctl(fd, CDROMREADTOCHDR as _, &mut hdr as *mut CdromTochdr) } < 0 {
                return Err(ErreurCd::Autre(format!(
                    "CDROMREADTOCHDR : {}",
                    derniere_erreur()
                )));
            }
            let entree = |piste: u8| -> Result<CdromTocentry, ErreurCd> {
                let mut e = CdromTocentry {
                    track: piste,
                    format: CDROM_LBA,
                    ..Default::default()
                };
                // SAFETY: `e` est une `cdrom_tocentry` valide, vivante le temps de l'appel.
                if unsafe { libc::ioctl(fd, CDROMREADTOCENTRY as _, &mut e as *mut CdromTocentry) }
                    < 0
                {
                    return Err(ErreurCd::Autre(format!(
                        "CDROMREADTOCENTRY {piste} : {}",
                        derniere_erreur()
                    )));
                }
                Ok(e)
            };
            let mut pistes = Vec::new();
            for n in hdr.trk0..=hdr.trk1 {
                let e = entree(n)?;
                pistes.push(PisteToc {
                    numero: n,
                    debut: e.addr_lba.max(0) as u32,
                    audio: (e.adr_ctrl >> 4) & CDROM_DATA_TRACK == 0,
                });
            }
            let fin = entree(CDROM_LEADOUT)?.addr_lba.max(0) as u32;
            Toc::nouvelle(pistes, fin).map_err(ErreurCd::Autre)
        })
    }

    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        if sortie.len() != nombre as usize * OCTETS_PAR_SECTEUR {
            return Err(ErreurCd::Autre("tampon de taille fausse".into()));
        }
        let r = self.avec_fd(|fd| {
            let mut ra = CdromReadAudio {
                addr_lba: lba as i32,
                addr_format: CDROM_LBA,
                nframes: nombre as i32,
                buf: sortie.as_mut_ptr(),
            };
            // SAFETY: `buf` pointe sur `nombre × 2 352` octets inscriptibles
            // (vérifié plus haut), vivants le temps de l'appel.
            if unsafe { libc::ioctl(fd, CDROMREADAUDIO as _, &mut ra as *mut CdromReadAudio) } < 0 {
                return Err(ErreurCd::Lecture {
                    lba,
                    raison: derniere_erreur(),
                });
            }
            Ok(())
        });
        // Un échec peut être une éjection : le dire comme tel, pour que le
        // flux s'arrête au lieu de rejouer et de remplir de silence.
        if r.is_err() && self.presence() != Presence::Disque {
            return Err(ErreurCd::AucunDisque);
        }
        r
    }

    /// `CDROMEJECT` sur un descripteur NEUF, le descripteur gardé fermé.
    ///
    /// Le pilote `cdrom` refuse l'éjection (`EBUSY`) tant que le
    /// périphérique est ouvert plus d'une fois (`cdi->use_count != 1`) : le
    /// descripteur de lecture est donc jeté d'abord, et le verrou pris pour
    /// qu'aucune lecture ne le rouvre pendant l'ioctl. La porte est
    /// déverrouillée avant (`CDROM_LOCKDOOR 0`, sans effet si elle ne l'est
    /// pas). Si un autre programme tient le lecteur ouvert (udisks, un
    /// lecteur de musique), l'ioctl reste refusé : repli sur `eject`, qui
    /// passe par une commande SCSI. Sans `eject`, l'erreur du noyau est
    /// rendue telle quelle.
    fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
        match self.presence() {
            Presence::AucunLecteur => return Err(ErreurEjection::AucunLecteur),
            Presence::Vide => return Err(ErreurEjection::AucunDisque),
            Presence::Disque => {}
        }
        let mut garde = self.fichier.lock().unwrap_or_else(|e| e.into_inner());
        *garde = None;
        let par_ioctl = (|| {
            let f = self
                .ouvrir()
                .map_err(|e| format!("{} ne s'ouvre pas : {e}", self.chemin))?;
            // SAFETY: ioctls sans pointeur ; l'argument est un entier.
            unsafe { libc::ioctl(f.as_raw_fd(), CDROM_LOCKDOOR as _, 0 as libc::c_int) };
            // SAFETY: idem, `CDROMEJECT` ne prend aucun argument.
            if unsafe { libc::ioctl(f.as_raw_fd(), CDROMEJECT as _, 0 as libc::c_int) } < 0 {
                return Err(format!("CDROMEJECT : {}", derniere_erreur()));
            }
            Ok(())
        })();
        let Err(raison) = par_ioctl else {
            tracing::info!(lecteur = %self.chemin, "cd_disque_ejecte");
            return Ok(());
        };
        tracing::warn!(lecteur = %self.chemin, %raison, "cd_ejection_ioctl_refusee");
        match std::process::Command::new("eject")
            .arg(&self.chemin)
            .output()
        {
            Ok(sortie) if sortie.status.success() => {
                tracing::info!(lecteur = %self.chemin, "cd_disque_ejecte_par_eject");
                Ok(())
            }
            Ok(sortie) => Err(ErreurEjection::Echec(format!(
                "{raison} ; eject : {}",
                String::from_utf8_lossy(&sortie.stderr).trim()
            ))),
            Err(_) => Err(ErreurEjection::Echec(raison)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les tailles des structures noyau : une erreur ici corromprait la
    /// mémoire à l'ioctl, et ne se verrait sinon que sur un vrai lecteur.
    #[test]
    fn les_structures_ont_la_disposition_de_linux_cdrom_h() {
        assert_eq!(std::mem::size_of::<CdromTochdr>(), 2);
        assert_eq!(std::mem::size_of::<CdromTocentry>(), 12);
        assert_eq!(std::mem::offset_of!(CdromTocentry, addr_lba), 4);
        assert_eq!(std::mem::offset_of!(CdromTocentry, datamode), 8);
        assert_eq!(std::mem::offset_of!(CdromReadAudio, addr_format), 4);
        assert_eq!(std::mem::offset_of!(CdromReadAudio, nframes), 8);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::offset_of!(CdromReadAudio, buf), 16);
            assert_eq!(std::mem::size_of::<CdromReadAudio>(), 24);
            // `struct sg_io_hdr` (#5729).
            assert_eq!(std::mem::offset_of!(SgIoHdr, dxfer_len), 12);
            assert_eq!(std::mem::offset_of!(SgIoHdr, cmdp), 24);
            assert_eq!(std::mem::offset_of!(SgIoHdr, timeout), 40);
            assert_eq!(std::mem::offset_of!(SgIoHdr, usr_ptr), 56);
            assert_eq!(std::mem::offset_of!(SgIoHdr, status), 64);
            assert_eq!(std::mem::offset_of!(SgIoHdr, resid), 72);
            assert_eq!(std::mem::size_of::<SgIoHdr>(), 88);
        }
    }

    /// #5729 — la commande d'éveil est exactement celle de la règle udev
    /// (`sg_raw /dev/srN EA 00 00 00 00 00 01`).
    #[test]
    fn la_commande_d_eveil_est_celle_de_sg_raw_5729() {
        assert_eq!(CDB_EVEIL_SUPERDRIVE, [0xEA, 0, 0, 0, 0, 0, 0x01]);
    }

    /// #5729 — `vendor`/`model` tels que le noyau les écrit (complétés
    /// d'espaces) ; un autre lecteur Apple ou un Samsung ne sont pas visés.
    #[test]
    fn un_superdrive_se_reconnait_a_vendor_et_model_5729() {
        assert!(est_un_superdrive("Apple   \n", "SuperDrive      \n"));
        assert!(est_un_superdrive("APPLE", "superdrive"));
        assert!(!est_un_superdrive("TSSTcorp", "CDDVDW SE-208GB \n"));
        assert!(!est_un_superdrive("Apple   ", "iPod            "));
        assert!(!est_un_superdrive("Applet", "SuperDrive"));
    }

    /// Un faux `/dev/srN` (fichier ordinaire, il s'ouvre) et son sysfs.
    fn faux_lecteur_5729(
        nom: &str,
        fabricant: &str,
        modele: &str,
    ) -> (tune_core::test_scratch::ScratchDir, PathBuf, String) {
        let dossier = tune_core::test_scratch::scratch_dir(nom);
        let dev = dossier.join("dev").join("sr0");
        std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
        std::fs::write(&dev, b"").unwrap();
        let device = dossier.join("sys/block/sr0/device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("vendor"), fabricant).unwrap();
        std::fs::write(device.join("model"), modele).unwrap();
        let sys = dossier.join("sys");
        (dossier, sys, dev.to_string_lossy().into_owned())
    }

    fn compteur_5729() -> (std::sync::Arc<std::sync::Mutex<Vec<String>>>, Eveil) {
        let appels = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = appels.clone();
        (
            appels,
            Box::new(move |c: &str| {
                a.lock().unwrap().push(c.to_string());
                Ok(())
            }),
        )
    }

    /// #5729 — sonder un SuperDrive lui envoie la commande d'éveil, UNE
    /// fois, même si la recherche recrée l'objet à chaque sondage.
    #[test]
    fn sonder_un_superdrive_l_eveille_une_fois_5729() {
        let (_d, sys, dev) =
            faux_lecteur_5729("cd-5729-superdrive", "Apple   \n", "SuperDrive      \n");
        let (appels, eveil) = compteur_5729();
        let l = LecteurLinux::avec_eveil(dev.clone(), sys.clone(), eveil);
        l.presence();
        l.presence();
        // La recherche du lecteur branchable crée un objet neuf.
        let (appels2, eveil2) = compteur_5729();
        LecteurLinux::avec_eveil(dev.clone(), sys, eveil2).presence();
        assert_eq!(
            *appels.lock().unwrap(),
            [dev.clone()],
            "#5729 : le SuperDrive doit recevoir sa commande d'éveil au premier sondage, une seule fois"
        );
        assert!(
            appels2.lock().unwrap().is_empty(),
            "#5729 : éveil renvoyé au même lecteur"
        );
        // Débranché puis rebranché : il est éveillé de nouveau.
        std::fs::remove_file(&dev).unwrap();
        assert_eq!(l.presence(), Presence::AucunLecteur);
        std::fs::write(&dev, b"").unwrap();
        l.presence();
        assert_eq!(
            appels.lock().unwrap().len(),
            2,
            "#5729 : un SuperDrive rebranché n'est pas éveillé"
        );
    }

    /// #5729 — un autre lecteur ne reçoit aucune commande vendeur, et un
    /// éveil refusé n'empêche pas de sonder.
    #[test]
    fn un_autre_lecteur_n_est_pas_eveille_5729() {
        let (_d2, sys, dev) = faux_lecteur_5729("cd-5729-samsung", "TSSTcorp", "CDDVDW SE-208GB ");
        let (appels, eveil) = compteur_5729();
        let l = LecteurLinux::avec_eveil(dev, sys, eveil);
        assert_eq!(l.presence(), Presence::Vide);
        assert!(appels.lock().unwrap().is_empty());

        let (_d3, sys, dev) = faux_lecteur_5729("cd-5729-refus", "Apple", "SuperDrive");
        let l = LecteurLinux::avec_eveil(dev, sys, Box::new(|_| Err("EPERM".into())));
        assert_eq!(
            l.presence(),
            Presence::Vide,
            "#5729 : un éveil refusé ne doit rien bloquer"
        );
    }

    /// Fil 2135 : TOUS les `srN`, au-delà de `sr3`, dans l'ordre numérique,
    /// et rien d'autre (`sg0`, `sda`, `srx`).
    #[test]
    fn tous_les_lecteurs_optiques_sont_enumeres() {
        // Effacé à la fin, même sur panique (`test_scratch`, #3030).
        let dossier = tune_core::test_scratch::scratch_dir("cd-2135-dev");
        for nom in ["sr1", "sr10", "sda", "sr0", "sg0", "srx", "sr", "sr4"] {
            std::fs::write(dossier.join(nom), b"").unwrap();
        }
        let d = dossier.to_string_lossy().into_owned();
        assert_eq!(
            peripheriques_optiques(&dossier),
            ["sr0", "sr1", "sr4", "sr10"].map(|n| format!("{d}/{n}"))
        );
        assert!(peripheriques_optiques(&dossier.join("absent")).is_empty());
    }

    #[test]
    fn un_peripherique_absent_se_dit_aucun_lecteur() {
        let l = LecteurLinux::new("/dev/n-existe-pas-4863".into());
        assert_eq!(l.presence(), Presence::AucunLecteur);
        assert!(l.lire_toc().is_err());
        // Rien à éjecter, et surtout aucune commande lancée au hasard.
        assert_eq!(l.ejecter_disque(), Err(ErreurEjection::AucunLecteur));
    }
}
