//! L'horloge que `statime` asservit : une horloge VIRTUELLE posée sur
//! l'horloge monotone du système.
//!
//! Tune ne touche jamais à l'horloge du système (ni `clock_settime`, ni
//! `adjtimex`, ni carte réseau PTP) : ce serait demander des droits root et
//! se battre avec NTP sur Tune OS. Le suiveur règle donc une horloge à lui :
//!
//! ```text
//! virtuel(m) = base_virtuelle + (m - base_monotone) × (1 + ppm × 10⁻⁶)
//! ```
//!
//! où `m` est l'horloge monotone (ancrée une fois sur le temps UNIX, comme
//! [`crate::horloge::HorlogeSysteme`]). Un réglage de fréquence REBASE la
//! formule à l'instant présent, sans saut ; un saut (`step_clock`) décale la
//! base et fait avancer la génération partagée, pour que le cadenceur reparte
//! d'une nouvelle origine.
//!
//! On n'a pas pris `statime::OverlayClock` : sa remise à l'heure perd la
//! correction de fréquence accumulée depuis le dernier réglage (jusqu'à 1 ms
//! après 10 s à 100 ppm), ce que cette formule ne fait pas.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use statime::Clock;
use statime::config::TimePropertiesDS;
use statime::time::{Duration as DureePtp, Time};
use tracing::info;

/// Une lecture de `Time` en nanosecondes entières.
pub fn ns_de(t: Time) -> u64 {
    t.secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(t.subsec_nanos()))
}

/// L'horloge monotone du système, ancrée une fois sur le temps UNIX.
#[derive(Debug, Clone, Copy)]
pub struct HorlogeMonotone {
    origine: Instant,
    origine_ns: u64,
}

impl HorlogeMonotone {
    pub fn new() -> Self {
        let origine_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            origine: Instant::now(),
            origine_ns,
        }
    }

    pub fn ns(&self) -> u64 {
        let ecoule = u64::try_from(self.origine.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.origine_ns.saturating_add(ecoule)
    }
}

impl Default for HorlogeMonotone {
    fn default() -> Self {
        Self::new()
    }
}

/// L'horloge virtuelle refuse une seule chose : un temps négatif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempsNegatif;

#[derive(Debug)]
struct Formule {
    base_monotone: u64,
    base_virtuelle: i128,
    ppm: f64,
}

impl Formule {
    fn virtuel(&self, m: u64) -> i128 {
        let ecoule = i128::from(m) - i128::from(self.base_monotone);
        let correction = (ecoule as f64 * self.ppm * 1e-6) as i128;
        self.base_virtuelle + ecoule + correction
    }
}

#[derive(Debug)]
struct Interne {
    monotone: HorlogeMonotone,
    formule: Mutex<Formule>,
    sauts: AtomicU64,
    corrections: AtomicU64,
    dernier_saut: Mutex<Option<Instant>>,
    generation: Arc<AtomicU64>,
}

/// L'horloge virtuelle, partagée entre le fil PTP (qui la règle), les fils
/// de réception (qui horodatent) et le cadenceur (qui la lit).
#[derive(Debug, Clone)]
pub struct HorlogeVirtuelle(Arc<Interne>);

impl HorlogeVirtuelle {
    /// Démarre alignée sur l'horloge monotone (donc sur le temps UNIX), à
    /// fréquence nominale. `generation` avance à chaque saut.
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        let monotone = HorlogeMonotone::new();
        let m = monotone.ns();
        Self(Arc::new(Interne {
            monotone,
            formule: Mutex::new(Formule {
                base_monotone: m,
                base_virtuelle: i128::from(m),
                ppm: 0.0,
            }),
            sauts: AtomicU64::new(0),
            corrections: AtomicU64::new(0),
            dernier_saut: Mutex::new(None),
            generation,
        }))
    }

    fn formule(&self) -> std::sync::MutexGuard<'_, Formule> {
        self.0.formule.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Le temps virtuel, en ns (0 si la formule tombait sous zéro).
    pub fn maintenant_ns(&self) -> u64 {
        let m = self.0.monotone.ns();
        u64::try_from(self.formule().virtuel(m).max(0)).unwrap_or(u64::MAX)
    }

    /// L'écart de fréquence appliqué, en ppm.
    pub fn ppm(&self) -> f64 {
        self.formule().ppm
    }

    /// Nombre de sauts (`step_clock`) depuis la création.
    pub fn sauts(&self) -> u64 {
        self.0.sauts.load(Ordering::Acquire)
    }

    /// L'instant du dernier saut.
    pub fn dernier_saut(&self) -> Option<Instant> {
        *self
            .0
            .dernier_saut
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Nombre de réglages (saut ou fréquence) depuis la création.
    pub fn corrections(&self) -> u64 {
        self.0.corrections.load(Ordering::Acquire)
    }

    fn rebaser(f: &mut Formule, m: u64) {
        f.base_virtuelle = f.virtuel(m);
        f.base_monotone = m;
    }
}

impl Clock for HorlogeVirtuelle {
    type Error = TempsNegatif;

    fn now(&self) -> Time {
        Time::from_nanos(self.maintenant_ns())
    }

    fn step_clock(&mut self, offset: DureePtp) -> Result<Time, Self::Error> {
        let m = self.0.monotone.ns();
        let mut f = self.formule();
        Self::rebaser(&mut f, m);
        let nouveau = f.base_virtuelle + offset.nanos_rounded();
        if nouveau < 0 {
            return Err(TempsNegatif);
        }
        f.base_virtuelle = nouveau;
        drop(f);
        *self
            .0
            .dernier_saut
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        self.0.sauts.fetch_add(1, Ordering::AcqRel);
        self.0.corrections.fetch_add(1, Ordering::AcqRel);
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        info!(
            saut_us = offset.nanos_rounded() / 1000,
            "aes67_ptp_saut_d_horloge"
        );
        Ok(Time::from_nanos(u64::try_from(nouveau).unwrap_or(u64::MAX)))
    }

    fn set_frequency(&mut self, ppm: f64) -> Result<Time, Self::Error> {
        let m = self.0.monotone.ns();
        let mut f = self.formule();
        Self::rebaser(&mut f, m);
        f.ppm = ppm;
        let maintenant = f.base_virtuelle;
        drop(f);
        self.0.corrections.fetch_add(1, Ordering::AcqRel);
        Ok(Time::from_nanos(
            u64::try_from(maintenant.max(0)).unwrap_or(0),
        ))
    }

    fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), Self::Error> {
        // Échelle, secondes intercalaires : de la métadonnée pour nous. Le
        // flux AES67 n'en dépend pas (seule la cadence compte).
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn horloge() -> (HorlogeVirtuelle, Arc<AtomicU64>) {
        let g = Arc::new(AtomicU64::new(0));
        (HorlogeVirtuelle::new(g.clone()), g)
    }

    #[test]
    fn au_depart_l_horloge_virtuelle_suit_le_temps_unix() {
        let (h, g) = horloge();
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let ecart = h.maintenant_ns().abs_diff(unix);
        assert!(ecart < 50_000_000, "{ecart} ns");
        assert_eq!(g.load(Ordering::Acquire), 0);
        assert_eq!(h.corrections(), 0);
    }

    #[test]
    fn un_saut_decale_le_temps_et_avance_la_generation() {
        let (mut h, g) = horloge();
        let avant = h.maintenant_ns();
        h.step_clock(DureePtp::from_secs(1000)).unwrap();
        let apres = h.maintenant_ns();
        let ecart = apres - avant;
        assert!(
            (1_000_000_000_000..1_000_050_000_000).contains(&ecart),
            "{ecart}"
        );
        h.step_clock(DureePtp::from_millis(-500)).unwrap();
        assert!(h.maintenant_ns() < apres);
        assert_eq!(g.load(Ordering::Acquire), 2);
        assert_eq!(h.sauts(), 2);
        assert!(h.step_clock(DureePtp::from_secs(-4_000_000_000)).is_err());
    }

    #[test]
    fn la_frequence_change_la_pente_sans_saut() {
        let (mut h, g) = horloge();
        let m = HorlogeMonotone::new();
        h.set_frequency(1000.0).unwrap(); // +1000 ppm : exagéré, pour mesurer.
        let (v0, m0) = (h.maintenant_ns(), m.ns());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (v1, m1) = (h.maintenant_ns(), m.ns());
        let dv = (v1 - v0) as f64;
        let dm = (m1 - m0) as f64;
        let ppm = (dv / dm - 1.0) * 1e6;
        assert!((ppm - 1000.0).abs() < 50.0, "{ppm} ppm");
        // Revenir à 0 ppm ne fait pas reculer le temps.
        let avant = h.maintenant_ns();
        h.set_frequency(0.0).unwrap();
        assert!(h.maintenant_ns() >= avant);
        assert_eq!(g.load(Ordering::Acquire), 0, "pas de saut");
        assert_eq!(h.corrections(), 2);
        assert_eq!(h.ppm(), 0.0);
    }

    #[test]
    fn la_lecture_en_time_et_en_ns_concordent() {
        let t = Time::from_nanos(1_793_000_000_123_456_789);
        assert_eq!(ns_de(t), 1_793_000_000_123_456_789);
    }
}
