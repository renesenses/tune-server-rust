//! Le suiveur et son horloge sont compilés directement depuis leurs sources.
//! Seules leurs entrées système (temps, réception et transport) sont pilotées ici.
//! La caisse est une dépendance de test, jamais liée au greffon livré.
extern crate self as std;
extern crate std as systeme;

pub use systeme::{collections, net, panic, thread};
pub use tune_aes67::horloge;

#[path = "../../src/ptp/horloge_virtuelle.rs"]
pub mod horloge_virtuelle;
mod maitre;
#[path = "../../src/ptp/suiveur.rs"]
pub mod suiveur;
mod transport;
pub use maitre::{jouer, temps_du_maitre};
pub use suiveur::{ConfigPtp, SuiveurPtp};

pub mod process {
    // Le PID ne sert qu'à ensemencer le générateur des intervalles PTP.
    pub fn id() -> u32 {
        1
    }
}

pub mod time {
    use crate::systeme::sync::atomic::{AtomicU64, Ordering};
    pub use crate::systeme::time::{Duration, UNIX_EPOCH};
    use crate::systeme::{ops::Add, time as reel};

    static NS: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Instant(Duration);
    impl Instant {
        pub fn now() -> Self {
            Self(Duration::from_nanos(NS.load(Ordering::Relaxed)))
        }
        pub fn elapsed(self) -> Duration {
            Self::now().saturating_duration_since(self)
        }
        pub fn saturating_duration_since(self, avant: Self) -> Duration {
            self.0.saturating_sub(avant.0)
        }
    }
    impl Add<Duration> for Instant {
        type Output = Self;
        fn add(self, d: Duration) -> Self {
            Self(self.0 + d)
        }
    }

    pub(crate) fn avancer_a(ns: u64) {
        NS.store(ns, Ordering::Relaxed);
    }
    pub struct SystemTime;
    impl SystemTime {
        pub fn now() -> reel::SystemTime {
            UNIX_EPOCH + Instant::now().0
        }
    }
}

pub mod sync {
    pub use crate::systeme::sync::{Arc, Mutex, MutexGuard, atomic};
    pub mod mpsc {
        use crate::{systeme::sync::mpsc as canal, time::Duration, transport::Recu};
        pub use canal::RecvTimeoutError;

        // Chaque attente du vrai fil PTP cède la main au maître. Le temps reste
        // figé jusqu'à la prochaine trame ou au prochain minuteur demandé.
        pub(crate) type Attente<T> = (Duration, canal::SyncSender<Result<T, RecvTimeoutError>>);
        pub struct Receiver<T>(canal::Sender<Attente<T>>);
        impl<T> Receiver<T> {
            pub fn recv_timeout(&self, delai: Duration) -> Result<T, RecvTimeoutError> {
                let (tx, rx) = canal::sync_channel(0);
                self.0
                    .send((delai, tx))
                    .map_err(|_| RecvTimeoutError::Disconnected)?;
                rx.recv().unwrap_or(Err(RecvTimeoutError::Disconnected))
            }
        }
        pub fn channel() -> ((), Receiver<Recu>) {
            super::super::transport::RECEPTION.with(|r| {
                (
                    (),
                    Receiver(r.borrow_mut().take().expect("liaison du banc")),
                )
            })
        }
    }
}
