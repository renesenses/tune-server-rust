//! Le lecteur Linux : ioctl `CDROMREADTOCHDR`, `CDROMREADTOCENTRY`,
//! `CDROMREADAUDIO` et `CDROM_DRIVE_STATUS` sur `/dev/sr*` (`linux/cdrom.h`).
//!
//! Aucun outil externe : ni `cdparanoia`, ni `cdda2wav`, ni ffmpeg. C'est la
//! seule partie du greffon qui parle au noyau ; elle ne se prouve que sur une
//! machine équipée d'un lecteur (voir la procédure de test manuel de la PR).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

use crate::lecteur::{ErreurCd, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, PisteToc, Toc};

// linux/cdrom.h
const CDROMREADTOCHDR: u64 = 0x5305;
const CDROMREADTOCENTRY: u64 = 0x5306;
const CDROMREADAUDIO: u64 = 0x530e;
const CDROM_DRIVE_STATUS: u64 = 0x5326;
const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;
const CDROM_DATA_TRACK: u8 = 0x04;
const CDSL_CURRENT: libc::c_int = i32::MAX;
const CDS_DISC_OK: libc::c_int = 4;

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
}

impl LecteurLinux {
    pub fn new(chemin: String) -> Self {
        Self {
            chemin,
            fichier: Mutex::new(None),
        }
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

fn derniere_erreur() -> String {
    std::io::Error::last_os_error().to_string()
}

impl LecteurDisque for LecteurLinux {
    fn chemin(&self) -> String {
        self.chemin.clone()
    }

    fn presence(&self) -> Presence {
        let Ok(f) = self.ouvrir() else {
            return Presence::AucunLecteur;
        };
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
        }
    }

    #[test]
    fn un_peripherique_absent_se_dit_aucun_lecteur() {
        let l = LecteurLinux::new("/dev/n-existe-pas-4863".into());
        assert_eq!(l.presence(), Presence::AucunLecteur);
        assert!(l.lire_toc().is_err());
    }
}
