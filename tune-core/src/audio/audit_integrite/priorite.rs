//! Priorité disque basse pendant tout l'audit, même sans musique en cours.
//! Garde limitée au décodage : jamais d'écriture SQL à priorité abaissée.
use std::marker::PhantomData;

pub(super) struct Garde {
    #[cfg(target_os = "linux")]
    avant: Option<libc::c_long>,
    #[cfg(any(target_os = "macos", windows))]
    active: bool,
    // Une priorité de fil se restaure sur le même fil.
    _fil: PhantomData<*const ()>,
}

pub(super) fn baisser() -> Garde {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: who=1, who_id=0 désignent uniquement le fil appelant.
        let avant = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, 0) };
        let deja_basse = avant >= 0 && (avant >> 13 == 3 || (avant >> 13 == 2 && avant & 255 >= 7));
        let change = avant >= 0
            && !deja_basse
            && unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7) } == 0;
        Garde {
            avant: change.then_some(avant),
            _fil: PhantomData,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: le mode arrière-plan est limité au fil courant.
        let active =
            unsafe { libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG) } == 0;
        Garde {
            active,
            _fil: PhantomData,
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        // SAFETY: pseudo-handle du fil courant, valide jusqu'à son retour.
        let active =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }.is_ok();
        Garde {
            active,
            _fil: PhantomData,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Garde { _fil: PhantomData }
    }
}
impl Drop for Garde {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(avant) = self.avant {
            // SAFETY: restaure exactement la priorité du même fil.
            unsafe {
                libc::syscall(libc::SYS_ioprio_set, 1, 0, avant);
            }
        }
        #[cfg(target_os = "macos")]
        if self.active {
            unsafe {
                libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, 0);
            }
        }
        #[cfg(windows)]
        if self.active {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
            };
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn priorite_du_fil_restauree_apres_audit() {
        let avant = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, 0) };
        assert!(avant >= 0);
        {
            let _garde = super::baisser();
            let pendant = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, 0) };
            assert!(
                pendant >> 13 == 3 || (pendant >> 13 == 2 && pendant & 255 == 7),
                "lecture disque non déprioritisée : {pendant}"
            );
        }
        assert_eq!(unsafe { libc::syscall(libc::SYS_ioprio_get, 1, 0) }, avant);
    }
}
