//! Inhibition de la veille système pendant une lecture.
//!
//! Windows attache `SetThreadExecutionState` au fil appelant. Une tâche Tokio
//! peut changer de worker entre l'acquisition et la libération : les deux
//! appels doivent donc vivre sur notre fil dédié. macOS n'a pas cette
//! contrainte, mais utilise le même fil pour garder un cycle de vie identique.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::{Mutex, mpsc};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::thread::JoinHandle;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tracing::{info, warn};

#[cfg(any(target_os = "windows", target_os = "macos"))]
enum Command {
    Set(bool),
    Shutdown,
}

pub(crate) struct SystemSleepInhibitor {
    requested: AtomicBool,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    worker: Mutex<Option<Worker>>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct Worker {
    tx: mpsc::Sender<Command>,
    handle: JoinHandle<()>,
}

impl SystemSleepInhibitor {
    pub(crate) fn new() -> Self {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            Self {
                requested: AtomicBool::new(false),
                // Paresseux : la majorité des `PlaybackManager` de tests ne
                // lance jamais de lecture. Ils ne doivent pas créer un fil
                // système pour rien.
                worker: Mutex::new(None),
            }
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        Self {
            requested: AtomicBool::new(false),
        }
    }

    /// Suit l'état agrégé de toutes les zones. Les appels sont sérialisés par
    /// le verrou des zones du `PlaybackManager`, donc l'ordre du canal est
    /// exactement celui des transitions de lecture.
    pub(crate) fn set_active(&self, active: bool) {
        if self.requested.swap(active, Ordering::SeqCst) == active {
            return;
        }

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let mut worker = self.worker.lock().expect("sleep worker lock");
            if worker.is_none() && active {
                let (tx, rx) = mpsc::channel();
                match std::thread::Builder::new()
                    .name("tune-sleep-inhibitor".into())
                    .spawn(move || run_worker(rx))
                {
                    Ok(handle) => *worker = Some(Worker { tx, handle }),
                    Err(error) => {
                        warn!(%error, "system_sleep_inhibitor_worker_spawn_failed");
                    }
                }
            }
            if worker
                .as_ref()
                .is_some_and(|worker| worker.tx.send(Command::Set(active)).is_err())
            {
                warn!(active, "system_sleep_inhibitor_worker_unavailable");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

impl Default for SystemSleepInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl Drop for SystemSleepInhibitor {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut().expect("sleep worker lock").take() {
            let _ = worker.tx.send(Command::Shutdown);
            let _ = worker.handle.join();
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn run_worker(rx: mpsc::Receiver<Command>) {
    let mut held = false;
    let mut platform_state = PlatformState::default();
    while let Ok(command) = rx.recv() {
        match command {
            Command::Set(active) if active != held => {
                if platform_set_active(active, &mut platform_state) {
                    held = active;
                    info!(active, "system_sleep_inhibitor_changed");
                } else {
                    warn!(active, "system_sleep_inhibitor_change_failed");
                }
            }
            Command::Set(_) => {}
            Command::Shutdown => break,
        }
    }
    if held && !platform_set_active(false, &mut platform_state) {
        warn!("system_sleep_inhibitor_release_failed_on_shutdown");
    }
}

#[cfg(target_os = "windows")]
type PlatformState = ();

#[cfg(target_os = "windows")]
fn platform_set_active(active: bool, _state: &mut PlatformState) -> bool {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
    };

    let flags = EXECUTION_STATE(ES_CONTINUOUS.0 | if active { ES_SYSTEM_REQUIRED.0 } else { 0 });
    // SAFETY: this worker owns the thread-scoped execution state for its whole
    // lifetime. The matching release is issued on this exact same thread.
    unsafe { SetThreadExecutionState(flags).0 != 0 }
}

#[cfg(target_os = "macos")]
type PlatformState = Option<u32>;

#[cfg(target_os = "macos")]
fn platform_set_active(active: bool, state: &mut PlatformState) -> bool {
    macos::set_active(active, state)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    type CFStringRef = *const c_void;
    type IOPMAssertionId = u32;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_string: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(value: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionId,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionId) -> i32;
    }

    pub(super) fn set_active(active: bool, assertion: &mut Option<IOPMAssertionId>) -> bool {
        if active {
            if assertion.is_some() {
                return true;
            }
            let assertion_type = cf_string(c"NoIdleSleepAssertion");
            let reason = cf_string(c"Tune is playing music");
            if assertion_type.is_null() || reason.is_null() {
                release_cf(assertion_type);
                release_cf(reason);
                return false;
            }

            let mut id = 0;
            // SAFETY: both CF strings are valid for the duration of the call,
            // and `id` is a writable out-parameter of the documented type.
            let result = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type,
                    K_IOPM_ASSERTION_LEVEL_ON,
                    reason,
                    &mut id,
                )
            };
            release_cf(assertion_type);
            release_cf(reason);
            if result == 0 {
                *assertion = Some(id);
                true
            } else {
                false
            }
        } else if let Some(id) = *assertion {
            // SAFETY: `id` came from a successful create and is released once.
            if unsafe { IOPMAssertionRelease(id) == 0 } {
                *assertion = None;
                true
            } else {
                // Garder l'identifiant permet au shutdown de retenter la
                // libération plutôt que d'abandonner une assertion vivante.
                false
            }
        } else {
            true
        }
    }

    fn cf_string(value: &std::ffi::CStr) -> CFStringRef {
        // SAFETY: the C string is NUL-terminated and lives through the call.
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
    }

    fn release_cf(value: CFStringRef) {
        if !value.is_null() {
            // SAFETY: this function only receives retained create-rule values.
            unsafe { CFRelease(value) };
        }
    }
}
