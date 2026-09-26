//! Le lecteur macOS : le volume `cddafs` que le système monte pour un CD
//! audio, lu par [`crate::cddafs::LecteurVolume`].
//!
//! Deux appels au système seulement, sans commande shell :
//!
//! * `getfsstat` : les volumes montés dont le type est `cddafs` — le disque ;
//! * IOKit (`IOServiceMatching("IOCDBlockStorageDevice")`) : un lecteur
//!   optique est-il branché ? C'est ce qui distingue « lecteur vide » de
//!   « aucun lecteur ». Les lecteurs DVD et BD en héritent.
//!
//! `TUNE_CD_DEVICE` impose un DOSSIER de volume (essais, volume fabriqué).

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::cddafs::LecteurVolume;

/// Les points de montage des volumes `cddafs`, triés.
pub fn volumes_cddafs() -> Vec<PathBuf> {
    // SAFETY: un tampon nul et une taille nulle demandent le nombre de volumes.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    // Marge : un volume peut se monter entre les deux appels.
    let capacite = n as usize + 4;
    // SAFETY: `statfs` est une structure C sans invariant ; zéro est valide.
    let mut tampon: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacite];
    let octets = (capacite * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `tampon` porte `capacite` structures inscriptibles, `octets` les mesure.
    let n = unsafe { libc::getfsstat(tampon.as_mut_ptr(), octets, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v: Vec<PathBuf> = tampon[..(n as usize).min(capacite)]
        .iter()
        .filter(|s| {
            // SAFETY: le noyau termine `f_fstypename` par un NUL.
            unsafe { CStr::from_ptr(s.f_fstypename.as_ptr()) }.to_bytes() == b"cddafs"
        })
        .map(|s| {
            // SAFETY: idem pour `f_mntonname`.
            let c = unsafe { CStr::from_ptr(s.f_mntonname.as_ptr()) };
            PathBuf::from(OsStr::from_bytes(c.to_bytes()))
        })
        .collect();
    v.sort();
    v
}

#[allow(non_camel_case_types)]
type io_object_t = u32;
#[allow(non_camel_case_types)]
type kern_return_t = i32;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(nom: *const libc::c_char) -> *mut std::ffi::c_void;
    fn IOServiceGetMatchingServices(
        port: u32,
        correspondance: *mut std::ffi::c_void,
        iterateur: *mut io_object_t,
    ) -> kern_return_t;
    fn IOIteratorNext(iterateur: io_object_t) -> io_object_t;
    fn IOObjectRelease(objet: io_object_t) -> kern_return_t;
}

/// Un lecteur optique (CD, DVD ou BD) est-il branché ?
pub fn lecteur_optique_branche() -> bool {
    // SAFETY: chaîne C statique ; le dictionnaire rendu est CONSOMMÉ par
    // `IOServiceGetMatchingServices` (pas de libération à faire) ; le port 0
    // est `kIOMainPortDefault` ; chaque objet rendu est libéré.
    unsafe {
        let dico = IOServiceMatching(c"IOCDBlockStorageDevice".as_ptr());
        if dico.is_null() {
            return false;
        }
        let mut it: io_object_t = 0;
        if IOServiceGetMatchingServices(0, dico, &mut it) != 0 || it == 0 {
            return false;
        }
        let service = IOIteratorNext(it);
        let present = service != 0;
        if present {
            IOObjectRelease(service);
        }
        IOObjectRelease(it);
        present
    }
}

pub fn lecteur_du_systeme() -> LecteurVolume {
    if let Ok(dossier) = std::env::var("TUNE_CD_DEVICE") {
        return LecteurVolume::sur_dossier(PathBuf::from(dossier));
    }
    LecteurVolume::new(
        "lecteur optique",
        || volumes_cddafs().into_iter().next(),
        lecteur_optique_branche,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les deux appels système répondent sans planter ; la racine n'est
    /// jamais un volume `cddafs`.
    #[test]
    fn les_appels_systeme_repondent() {
        let v = volumes_cddafs();
        assert!(!v.contains(&PathBuf::from("/")));
        let _ = lecteur_optique_branche();
    }
}
