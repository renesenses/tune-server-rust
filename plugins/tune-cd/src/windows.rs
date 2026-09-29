//! Lecture CD-DA Windows par `DeviceIoControl` sur `\\.\X:`.
//!
//! La TOC est demandée en LBA (`IOCTL_CDROM_READ_TOC_EX`, `Msf = 0`), puis
//! les secteurs audio bruts par `IOCTL_CDROM_RAW_READ` en mode CDDA. Le
//! périphérique est partagé avec les autres lecteurs, sans extraction.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Mutex;

use windows_sys::Win32::Devices::Cdrom::{
    CDDA, CDROM_READ_TOC_EX, CDROM_TOC, IOCTL_CDROM_RAW_READ, IOCTL_CDROM_READ_TOC_EX,
    RAW_READ_INFO,
};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MEDIA_IN_DRIVE, ERROR_NOT_READY, GENERIC_READ, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::IOCTL_STORAGE_CHECK_VERIFY;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_CDROM;

use crate::lecteur::{ErreurCd, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, Toc};
use crate::windows_toc::decoder_toc;

pub struct LecteurWindows {
    chemin: String,
    handle: Mutex<Option<OwnedHandle>>,
}

impl LecteurWindows {
    pub fn new(chemin: String) -> Self {
        Self {
            chemin,
            handle: Mutex::new(None),
        }
    }

    fn ouvrir(&self) -> std::io::Result<OwnedHandle> {
        let mut nom: Vec<u16> = self.chemin.encode_utf16().collect();
        nom.push(0);
        // SAFETY: chaîne UTF-16 terminée ; CreateFileW garde aucun pointeur.
        let handle = unsafe {
            CreateFileW(
                nom.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: handle vient de CreateFileW et nous en prenons possession.
            Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
        }
    }

    fn avec_handle<T>(
        &self,
        f: impl FnOnce(&OwnedHandle) -> Result<T, ErreurCd>,
    ) -> Result<T, ErreurCd> {
        let mut garde = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        if garde.is_none() {
            *garde =
                Some(self.ouvrir().map_err(|e| {
                    ErreurCd::Autre(format!("{} ne s'ouvre pas : {e}", self.chemin))
                })?);
        }
        let resultat = f(garde.as_ref().unwrap());
        if resultat.is_err() {
            *garde = None;
        }
        resultat
    }
}

/// Cherche les lettres dont Windows dit qu'elles sont des lecteurs optiques.
/// `GetDriveTypeW` distingue un lecteur vide d'une lettre inexistante.
pub fn premier_lecteur() -> Option<String> {
    (b'A'..=b'Z').find_map(|lettre| {
        let racine = [u16::from(lettre), u16::from(b':'), u16::from(b'\\'), 0];
        // SAFETY: racine UTF-16 terminée, vivante pendant l'appel.
        (unsafe { GetDriveTypeW(racine.as_ptr()) } == DRIVE_CDROM)
            .then(|| format!(r"\\.\{}:", char::from(lettre)))
    })
}

fn ioctl(
    handle: &OwnedHandle,
    code: u32,
    entree: *const std::ffi::c_void,
    taille_entree: u32,
    sortie: &mut [u8],
) -> std::io::Result<usize> {
    let mut lus = 0;
    // SAFETY: les tampons et leurs tailles concordent ; handle est vivant et
    // l'appel est synchrone (OVERLAPPED nul).
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            code,
            entree,
            taille_entree,
            if sortie.is_empty() {
                std::ptr::null_mut()
            } else {
                sortie.as_mut_ptr().cast()
            },
            sortie.len() as u32,
            &mut lus,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(lus as usize)
    }
}

fn absence_disque(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(code) if code == ERROR_NOT_READY as i32 || code == ERROR_NO_MEDIA_IN_DRIVE as i32)
}

impl LecteurDisque for LecteurWindows {
    fn chemin(&self) -> String {
        self.chemin.clone()
    }

    fn presence(&self) -> Presence {
        let handle = match self.ouvrir() {
            Ok(handle) => handle,
            Err(e) if absence_disque(&e) => return Presence::Vide,
            Err(_) => return Presence::AucunLecteur,
        };
        match ioctl(
            &handle,
            IOCTL_STORAGE_CHECK_VERIFY,
            std::ptr::null(),
            0,
            &mut [],
        ) {
            Ok(_) => Presence::Disque,
            Err(_) => Presence::Vide,
        }
    }

    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        self.avec_handle(|handle| {
            let demande = CDROM_READ_TOC_EX {
                // Format TOC = 0 (bits 0..3), Msf = 0 (bit 7) : LBA.
                _bitfield: 0,
                SessionTrack: 1,
                Reserved2: 0,
                Reserved3: 0,
            };
            let mut octets = [0u8; std::mem::size_of::<CDROM_TOC>()];
            let n = ioctl(
                handle,
                IOCTL_CDROM_READ_TOC_EX,
                (&demande as *const CDROM_READ_TOC_EX).cast(),
                std::mem::size_of::<CDROM_READ_TOC_EX>() as u32,
                &mut octets,
            )
            .map_err(|e| {
                if absence_disque(&e) {
                    ErreurCd::AucunDisque
                } else {
                    ErreurCd::Autre(format!("IOCTL_CDROM_READ_TOC_EX : {e}"))
                }
            })?;
            decoder_toc(&octets[..n]).map_err(ErreurCd::Autre)
        })
    }

    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        let attendu = (nombre as usize)
            .checked_mul(OCTETS_PAR_SECTEUR)
            .ok_or_else(|| ErreurCd::Autre("nombre de secteurs trop grand".into()))?;
        if sortie.len() != attendu {
            return Err(ErreurCd::Autre("tampon de taille fausse".into()));
        }
        if nombre == 0 {
            return Ok(());
        }
        let demande = RAW_READ_INFO {
            // L'API Windows mesure DiskOffset en secteurs logiques de 2048
            // octets, même lorsque le résultat CDDA fait 2352 octets.
            DiskOffset: i64::from(lba) * 2048,
            SectorCount: nombre,
            TrackMode: CDDA,
        };
        let resultat = self.avec_handle(|handle| {
            let lus = ioctl(
                handle,
                IOCTL_CDROM_RAW_READ,
                (&demande as *const RAW_READ_INFO).cast(),
                std::mem::size_of::<RAW_READ_INFO>() as u32,
                sortie,
            )
            .map_err(|e| {
                if absence_disque(&e) {
                    ErreurCd::AucunDisque
                } else {
                    ErreurCd::Lecture {
                        lba,
                        raison: e.to_string(),
                    }
                }
            })?;
            if lus != attendu {
                return Err(ErreurCd::Lecture {
                    lba,
                    raison: format!("{lus} octets rendus au lieu de {attendu}"),
                });
            }
            Ok(())
        });
        if matches!(resultat, Err(ErreurCd::Lecture { .. })) && self.presence() != Presence::Disque
        {
            return Err(ErreurCd::AucunDisque);
        }
        resultat
    }
}
