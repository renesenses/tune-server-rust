//! Arithmétique de présentation : ne perdre aucun reste entre deux pistes.
//! Les unités sont choisies par le chemin (trames PCM, bits DSD, etc.).

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct FractionDePresentation {
    reste: u64,
}

impl FractionDePresentation {
    /// PTS d'une unité locale, depuis l'ancre entière de la piste.
    /// `None` refuse une cadence nulle ou une échéance non représentable.
    pub(super) fn pts(&self, debut_ns: u64, unites: u64, par_seconde: u64) -> Option<u64> {
        if par_seconde == 0 || self.reste >= par_seconde {
            return None;
        }
        let numerateur = u128::from(unites) * 1_000_000_000 + u128::from(self.reste);
        let delai = u64::try_from(numerateur / u128::from(par_seconde)).ok()?;
        debut_ns.checked_add(delai)
    }

    /// La nouvelle piste commence immédiatement après les unités réellement
    /// envoyées. Le reste est conservé : additionner des durées déjà arrondies
    /// ferait dériver une suite de pistes à 44,1 kHz.
    pub(super) fn enchainer(&mut self, debut_ns: &mut u64, unites: u64, par_seconde: u64) -> bool {
        let Some(fin) = self.pts(*debut_ns, unites, par_seconde) else {
            return false;
        };
        let numerateur = u128::from(unites) * 1_000_000_000 + u128::from(self.reste);
        self.reste = (numerateur % u128::from(par_seconde)) as u64;
        *debut_ns = fin;
        true
    }

    /// Seek, nouveau flux ou format incompatible : une nouvelle horloge.
    pub(super) fn reinitialiser(&mut self) {
        self.reste = 0;
    }

    /// Avance aussi le repère du producteur. En DSD après seek, ce repère
    /// représente encore le début absolu du média, contrairement au PTS.
    pub(super) fn enchainer_cadencement(
        &mut self,
        debut_ns: &mut u64,
        depart: &mut std::time::Instant,
        unites: u64,
        origine_cadencement: u64,
        par_seconde: u64,
    ) -> bool {
        let mut suivante = *self;
        let mut fin = *debut_ns;
        if !suivante.enchainer(&mut fin, unites, par_seconde) {
            return false;
        }
        let Some(origine_ns) = Self::default().pts(0, origine_cadencement, par_seconde) else {
            return false;
        };
        let Some(avance) = (fin - *debut_ns).checked_add(origine_ns) else {
            return false;
        };
        let Some(prochain_depart) = depart.checked_add(std::time::Duration::from_nanos(avance))
        else {
            return false;
        };
        *self = suivante;
        *debut_ns = fin;
        *depart = prochain_depart;
        true
    }
}

fn attente_cadencement(
    depart: std::time::Instant,
    attendu: std::time::Duration,
    maintenant: std::time::Instant,
) -> Option<std::time::Duration> {
    Some(
        depart
            .checked_add(attendu)?
            .saturating_duration_since(maintenant),
    )
}

/// Avance d'envoi sur le temps réel : ce que l'endpoint reçoit d'emblée.
///
/// Un endpoint ALSA tamponne 2 s. Cinquante paquets d'avance (0,54 s à
/// 44,1 kHz, 0,125 s à 192 kHz) laissaient le reste du tampon se remplir en
/// temps réel : près de 2 s entre « play » et le premier son. Avec 1,5 s
/// envoyées d'un coup, l'endpoint démarre aussitôt et garde sa marge contre
/// la gigue. Rester sous les 2 s du tampon : l'excédent bloquerait l'endpoint
/// et déborderait de son socket UDP.
pub(super) const AVANCE_TAMPON_ENDPOINT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Échéance d'envoi d'un paquet dont l'audio commence à `audio_envoye` :
/// le temps réel, moins l'avance donnée à l'endpoint.
pub(super) fn echeance_avec_avance(audio_envoye: std::time::Duration) -> std::time::Duration {
    audio_envoye.saturating_sub(AVANCE_TAMPON_ENDPOINT)
}

/// Une origine avancée peut être encore dans le futur après préremplissage.
/// `depart.elapsed()` seul perdrait alors l'attente jusqu'à cette origine.
pub(super) async fn cadencer(depart: std::time::Instant, attendu: std::time::Duration) -> bool {
    let Some(attente) = attente_cadencement(depart, attendu, std::time::Instant::now()) else {
        return false;
    };
    if !attente.is_zero() {
        tokio::time::sleep(attente).await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_endpoint_recoit_d_emblee_une_avance_fixe_quelle_que_soit_la_frequence() {
        use std::time::Duration;
        // Dans l'avance : envoi immédiat.
        assert_eq!(echeance_avec_avance(Duration::ZERO), Duration::ZERO);
        assert_eq!(echeance_avec_avance(Duration::from_millis(1499)), Duration::ZERO);
        // Au-delà : temps réel décalé de l'avance, l'endpoint garde 1,5 s.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(10_000)),
            Duration::from_millis(8_500),
            "l'envoi doit garder 1,5 s d'avance sur le temps réel"
        );
    }

    #[test]
    fn une_origine_future_ne_perd_pas_le_delai_de_cadencement_7332() {
        use std::time::{Duration, Instant};
        let maintenant = Instant::now();
        assert_eq!(
            attente_cadencement(
                maintenant + Duration::from_millis(500),
                Duration::from_millis(20),
                maintenant,
            ),
            Some(Duration::from_millis(520)),
        );
        assert_eq!(
            attente_cadencement(
                maintenant - Duration::from_millis(500),
                Duration::from_millis(20),
                maintenant,
            ),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn le_reste_de_44100_est_conserve_entre_trois_pistes_7332() {
        let mut fraction = FractionDePresentation::default();
        let mut debut = 1_000;
        assert_eq!(fraction.pts(debut, 1, 44_100), Some(23_675));
        assert!(fraction.enchainer(&mut debut, 1, 44_100));
        assert_eq!(debut, 23_675);
        assert_eq!(fraction.pts(debut, 1, 44_100), Some(46_351));
        assert!(fraction.enchainer(&mut debut, 1, 44_100));
        assert_eq!(debut, 46_351);
        assert_eq!(fraction.pts(debut, 1, 44_100), Some(69_027));
        fraction.reinitialiser();
        assert_eq!(
            fraction.pts(5_000, 1, 48_000),
            Some(25_833),
            "un nouveau repère ne transporte pas l'ancien reste"
        );
    }

    #[test]
    fn une_echeance_impossible_ne_modifie_pas_l_ancre_7332() {
        let mut fraction = FractionDePresentation::default();
        let mut debut = u64::MAX;
        assert!(!fraction.enchainer(&mut debut, 1, 44_100));
        assert_eq!(debut, u64::MAX);
        assert!(!fraction.enchainer(&mut debut, 1, 0));
        assert_eq!(debut, u64::MAX);
    }
}
