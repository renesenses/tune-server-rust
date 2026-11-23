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

/// Avance d'envoi voulue, avant plafonnement par le tampon annoncé.
///
/// Cinquante paquets d'avance (0,54 s à 44,1 kHz, 0,125 s à 192 kHz)
/// laissaient le reste du tampon se remplir en temps réel. 1,5 s est le
/// plafond : le tampon cpal (`ring_buffer_frames`) ne tient qu'environ 1 s,
/// et l'excédent serait jeté. L'avance réelle est le minimum de cette
/// valeur et de `HelloAck.buffer_size_ms`.
pub(super) const AVANCE_TAMPON_ENDPOINT: std::time::Duration =
    std::time::Duration::from_millis(1500);

/// L'avance se constitue à ce multiple du temps réel, pas d'un seul jet :
/// 1,5 s de DSD64 envoyées d'un coup font ~1 Mo, cinq fois le tampon de
/// réception UDP par défaut d'un Raspberry Pi (~208 Ko) ; la rafale était
/// perdue et le DSD ne jouait plus. À 4×, 1 s d'avance (tampon cpal) est
/// constituée en 250 ms.
pub(super) const ACCELERATION_AVANCE: u32 = 4;

/// Échéance d'envoi d'un paquet dont l'audio commence à `audio_envoye`.
///
/// L'avance est [`AVANCE_TAMPON_ENDPOINT`], plafonnée par `tampon` (la
/// capacité annoncée par l'endpoint). On ne va jamais plus vite que
/// [`ACCELERATION_AVANCE`] fois le temps réel.
pub(super) fn echeance_avec_avance(
    audio_envoye: std::time::Duration,
    tampon: std::time::Duration,
) -> std::time::Duration {
    let avance = AVANCE_TAMPON_ENDPOINT.min(tampon);
    audio_envoye
        .saturating_sub(avance)
        .max(audio_envoye / ACCELERATION_AVANCE)
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
    fn l_endpoint_recoit_une_avance_fixe_quelle_que_soit_la_frequence() {
        use std::time::Duration;
        let tampon = Duration::from_millis(2_000);
        assert_eq!(echeance_avec_avance(Duration::ZERO, tampon), Duration::ZERO);
        // Au-delà : temps réel décalé de l'avance, l'endpoint garde 1,5 s.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(10_000), tampon),
            Duration::from_millis(8_500),
            "l'envoi doit garder 1,5 s d'avance sur le temps réel"
        );
    }

    #[test]
    fn l_avance_se_constitue_a_quatre_fois_le_temps_reel_pas_d_un_seul_jet() {
        use std::time::Duration;
        let tampon = Duration::from_millis(2_000);
        // 1 s d'audio part en 250 ms, pas instantanément : une rafale de DSD
        // déborderait du tampon UDP de l'endpoint.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(1000), tampon),
            Duration::from_millis(250),
            "l'avance doit se constituer à 4x le temps réel, pas en rafale"
        );
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(500), tampon),
            Duration::from_millis(125)
        );
        // Raccord continu : à 2 s d'audio, les deux bornes valent 0,5 s.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(2000), tampon),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn l_avance_est_plafonnee_par_le_tampon_annonce() {
        use std::time::Duration;
        // cpal tient ~1 s : l'excédent de 1,5 s serait jeté.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(10_000), Duration::from_millis(1_000)),
            Duration::from_millis(9_000)
        );
        // Le mock n'annonce que 100 ms : 2 s d'audio partent près du temps réel.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(2_000), Duration::from_millis(100)),
            Duration::from_millis(1_900)
        );
        // Tampon nul : pas d'avance, envoi au temps réel.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(1_000), Duration::ZERO),
            Duration::from_millis(1_000)
        );
        // La rampe à 4× reste, le temps de remplir le tampon plafonné.
        assert_eq!(
            echeance_avec_avance(Duration::from_millis(200), Duration::from_millis(1_000)),
            Duration::from_millis(50)
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
