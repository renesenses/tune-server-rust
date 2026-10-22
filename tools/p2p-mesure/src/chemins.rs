//! Suivi du chemin retenu par iroh (cahier § 3.1, questions 1 et 2).
//!
//! On lit le chemin **sélectionné** pour les données applicatives
//! (`Path::is_selected`) : direct (IP) ou relais. Échantillonné toutes les
//! secondes, et à chaque changement annoncé par iroh (`paths_stream`) pour
//! dater précisément le passage en direct.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::TransportAddr;
use n0_future::StreamExt;
use tokio::sync::oneshot;

use crate::protocole::famille;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chemin {
    Relais,
    Direct,
}

/// État partagé avec les pings, pour ranger chaque RTT selon le chemin.
pub const INCONNU: u8 = 0;
pub const RELAIS: u8 = 1;
pub const DIRECT: u8 = 2;

#[derive(Debug, Default, Clone)]
pub struct SuiviChemins {
    /// États observés, dans l'ordre, sans répétition consécutive.
    pub sequence: Vec<Chemin>,
    pub t_direct: Option<Duration>,
    /// Famille du dernier chemin direct retenu.
    pub famille_direct: Option<&'static str>,
}

impl SuiviChemins {
    fn observer(&mut self, chemin: Chemin, famille: Option<&'static str>, depuis_t0: Duration) {
        if self.sequence.last() != Some(&chemin) {
            self.sequence.push(chemin);
        }
        if chemin == Chemin::Direct {
            self.t_direct.get_or_insert(depuis_t0);
            if famille.is_some() {
                self.famille_direct = famille;
            }
        }
    }
}

/// `direct` : passé en direct et jamais revenu au relais ensuite ;
/// `relais` : jamais en direct ; `mixte` : revenu au moins une fois au relais
/// après être passé en direct. iroh commence toujours par le relais quand il
/// en a un : un départ en relais suivi du direct reste `direct`.
pub fn chemin_final(sequence: &[Chemin]) -> Option<&'static str> {
    let premier_direct = sequence.iter().position(|c| *c == Chemin::Direct);
    match premier_direct {
        None if sequence.is_empty() => None,
        None => Some("relais"),
        Some(i) if sequence[i..].contains(&Chemin::Relais) => Some("mixte"),
        Some(_) => Some("direct"),
    }
}

fn lire_selection(conn: &Connection) -> Option<(Chemin, Option<&'static str>)> {
    let chemins = conn.paths();
    let choisi = chemins.iter().find(|p| p.is_selected())?;
    match choisi.remote_addr() {
        TransportAddr::Ip(sa) => Some((Chemin::Direct, Some(famille(sa.ip())))),
        TransportAddr::Relay(_) => Some((Chemin::Relais, None)),
        _ => None,
    }
}

/// Suit le chemin jusqu'à `arret`, en publiant l'état courant dans `courant`.
pub async fn suivre(
    conn: Connection,
    t0: Instant,
    courant: Arc<AtomicU8>,
    mut arret: oneshot::Receiver<()>,
) -> SuiviChemins {
    let mut suivi = SuiviChemins::default();
    let mut changements = conn.paths_stream();
    let mut tic = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut arret => break,
            evt = changements.next() => {
                if evt.is_none() {
                    break;
                }
            }
            _ = tic.tick() => {}
        }
        if let Some((chemin, fam)) = lire_selection(&conn) {
            courant.store(
                if chemin == Chemin::Direct {
                    DIRECT
                } else {
                    RELAIS
                },
                Ordering::Relaxed,
            );
            suivi.observer(chemin, fam, t0.elapsed());
        }
    }
    suivi
}

#[cfg(test)]
mod tests {
    use super::*;
    use Chemin::*;

    #[test]
    fn chemin_final_selon_la_sequence() {
        assert_eq!(chemin_final(&[]), None);
        assert_eq!(chemin_final(&[Relais]), Some("relais"));
        assert_eq!(chemin_final(&[Direct]), Some("direct"));
        assert_eq!(chemin_final(&[Relais, Direct]), Some("direct"));
        assert_eq!(chemin_final(&[Relais, Direct, Relais]), Some("mixte"));
        assert_eq!(
            chemin_final(&[Relais, Direct, Relais, Direct]),
            Some("mixte")
        );
    }

    #[test]
    fn t_direct_est_le_premier_passage() {
        let mut s = SuiviChemins::default();
        s.observer(Relais, None, Duration::from_millis(400));
        s.observer(Direct, Some("ipv6"), Duration::from_millis(1830));
        s.observer(Direct, Some("ipv6"), Duration::from_millis(2830));
        s.observer(Relais, None, Duration::from_millis(3000));
        s.observer(Direct, Some("ipv4"), Duration::from_millis(5000));
        assert_eq!(s.t_direct, Some(Duration::from_millis(1830)));
        assert_eq!(s.sequence, vec![Relais, Direct, Relais, Direct]);
        assert_eq!(s.famille_direct, Some("ipv4"));
    }
}
