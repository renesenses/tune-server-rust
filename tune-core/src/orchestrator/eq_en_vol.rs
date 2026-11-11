//! #4407 — l'égaliseur remplacé EN VOL dans un flux réseau que Tune fabrique
//! lui-même.
//!
//! Sur une zone réseau (DLNA, navigateur, OAAT), un changement d'égaliseur
//! relançait le flux (`replay_programme`) : nouvelle session HTTP, nouveau
//! `SetAVTransportURI` + `Play` vers le renderer, et 2,787 s de silence sur
//! une radio (Jean Valjean, Marantz ND8006, fil 1771) — dont 2,37 s de
//! pré-tampon, parce qu'une radio doit se reconnecter à la station.
//!
//! Or deux porteurs appliquent déjà l'égaliseur AU FIL DE L'EAU, bloc par
//! bloc, dans le processus :
//!
//! - le décodeur radio (`decode_radio_stream_to_pcm`), qui sert un WAV ;
//! - le relais DSP progressif (`StreamingDsp`, `spawn_streaming_dsp_relay`).
//!
//! Là, rien n'est encore écrit quand le bloc suivant passe : il suffit de
//! remplacer l'`EqProcessor` du porteur, comme la sortie locale le fait
//! derrière son mutex. Ce poste de relève est le point de rendez-vous : la
//! route y dépose le nouveau profil, le porteur le relève au bloc suivant et
//! passe par [`EqProcessor::prendre_la_releve`] (#5215) — héritage des
//! filtres, et fondu enchaîné de 300 ms quand le niveau bouge.
//!
//! Le fichier pré-transcodé (FLAC ré-encodé, `transcoder_vers_fichier`) n'a
//! PAS de poste : ses octets sont écrits, servis avec `Content-Length`, en
//! cache, souvent déjà téléchargés par le renderer. Là, la relance reste le
//! seul chemin, et le journal dit pourquoi.

use crate::audio::eq::{EqProcessor, EqProfile};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Le poste de relève de l'égaliseur d'UN flux.
pub(crate) struct EqEnVol {
    /// Le profil à poser au prochain bloc. `Some(None)` : couper
    /// l'égaliseur ; `None` : rien en attente.
    attente: Mutex<Option<Option<EqProfile>>>,
    poses: AtomicU64,
    releves: AtomicU64,
    /// Le porteur ne cuit QUE l'égaliseur (la radio) : aucun autre étage ne
    /// peut diverger du réglage, la relève rend donc le flux conforme.
    pub(crate) cuit_seulement_l_egaliseur: bool,
    /// Le porteur cuit une compensation de niveau (#5071) calculée depuis
    /// l'égaliseur de départ : la relève ne la recalcule pas.
    pub(crate) compensation_cuite: bool,
    /// #6211 — le porteur sert un flux SANS FIN (radio) : aucune piste
    /// suivante ne relaiera le préampli gardé d'un égaliseur coupé : sa part
    /// supérieure à 1 s'éteint dès la coupure, une part inférieure remonte vers
    /// le niveau d'avant l'égaliseur sans le dépasser (#6741 ; décision du
    /// 21/10, `EqProcessor::eteindre_le_preampli_garde`).
    pub(crate) flux_sans_fin: bool,
    /// #8422 — le FORMAT du flux servi ne dépend d'aucun traitement : une
    /// radio qu'on décode en WAV de toute façon (renderer désigné,
    /// navigateur, codec ambigu, OAAT). Faux pour une radio que Tune ne
    /// décode QUE parce qu'un égaliseur est actif : sans lui (PURE), une
    /// lecture fraîche passerait la station telle quelle, autre format.
    pub(crate) format_independant_du_traitement: bool,
    /// #8422 — le dernier dépôt vient d'une entrée en PURE : le préampli que
    /// garde l'égaliseur coupé remonte jusqu'au signal SEC, sans la borne de
    /// volume de #6741, pour que la chaîne se vide et rende les octets
    /// décodés tels quels.
    vers_le_sec: std::sync::atomic::AtomicBool,
    /// #6741 — volume de la zone (millièmes, sans trim), tenu à jour par
    /// l'orchestrateur à chaque consigne ; `u32::MAX` : inconnu.
    volume_zone: AtomicU32,
    /// #6741 — volume de la zone quand l'égaliseur du flux est devenu actif :
    /// le « niveau d'avant l'égaliseur ». `u32::MAX` : aucun égaliseur actif.
    volume_avant: AtomicU32,
}

/// `u32::MAX` : volume inconnu (voir [`EqEnVol::noter_le_volume`]).
const VOLUME_INCONNU: u32 = u32::MAX;

impl Default for EqEnVol {
    fn default() -> Self {
        Self {
            attente: Mutex::new(None),
            poses: AtomicU64::new(0),
            releves: AtomicU64::new(0),
            cuit_seulement_l_egaliseur: false,
            compensation_cuite: false,
            flux_sans_fin: false,
            format_independant_du_traitement: false,
            vers_le_sec: std::sync::atomic::AtomicBool::new(false),
            volume_zone: AtomicU32::new(VOLUME_INCONNU),
            volume_avant: AtomicU32::new(VOLUME_INCONNU),
        }
    }
}

impl EqEnVol {
    /// #6741 — la consigne de volume de la zone (0..1, sans trim), notée à
    /// la création du poste et à chaque geste de volume.
    pub(crate) fn noter_le_volume(&self, volume: f64) {
        if volume.is_finite() {
            let v = (volume.clamp(0.0, 1.0) * 1000.0).round() as u32;
            self.volume_zone.store(v, Ordering::SeqCst);
        }
    }

    /// #6741 — la borne de remontée d'un flux servi à un APPAREIL : 1 (retour
    /// complet au signal sec) quand la consigne de la zone n'est pas au-dessus
    /// de celle d'avant l'égaliseur, 0 (gain gardé) sinon, ou quand l'une des
    /// deux est inconnue.
    ///
    /// Ce poste sert des sorties à volume MATÉRIEL (DLNA, OAAT…) : la courbe
    /// qui change le pourcentage envoyé en niveau appartient à l'appareil, et
    /// beaucoup sont en dB. Le rapport des pourcentages n'y borne rien : de
    /// 25 % à 50 % sur une courbe de 50 dB, le niveau monte de 12,5 dB, pas
    /// de 6. Seule la comparaison tient sur toute courbe croissante : à une
    /// consigne inférieure ou égale à celle d'avant, le signal sec ne dépasse
    /// pas le niveau d'avant. Au-dessus, le préampli reste gardé jusqu'à un
    /// réglage qui redescend à ce niveau.
    fn borne_de_remontee(&self) -> f64 {
        let avant = self.volume_avant.load(Ordering::SeqCst);
        let courant = self.volume_zone.load(Ordering::SeqCst);
        if avant == VOLUME_INCONNU || courant == VOLUME_INCONNU || courant > avant {
            0.0
        } else {
            1.0
        }
    }

    /// #6741 — suivre le niveau d'avant l'égaliseur, avant chaque bloc : le
    /// retenir quand un égaliseur actif joue sans référence, l'oublier quand
    /// la chaîne est revenue au sec, et borner la remontée d'un préampli
    /// gardé par le volume courant.
    fn suivre_le_niveau_d_avant(&self, courant: &mut Option<EqProcessor>) {
        match courant.as_mut() {
            None => self.volume_avant.store(VOLUME_INCONNU, Ordering::SeqCst),
            Some(eq) if !eq.est_neutre() => {
                let _ = self.volume_avant.compare_exchange(
                    VOLUME_INCONNU,
                    self.volume_zone.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
            Some(eq) => eq.borner_la_remontee(self.borne_de_remontee()),
        }
    }

    /// Le poste d'un flux radio décodé par Tune, quel que soit le réglage.
    pub(crate) fn pour_la_radio() -> std::sync::Arc<Self> {
        Self::pour_la_radio_servie(true)
    }

    /// #8422 — le poste d'un flux radio, en disant si Tune le décoderait
    /// encore sans égaliseur (`format_independant_du_traitement`).
    pub(crate) fn pour_la_radio_servie(
        format_independant_du_traitement: bool,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cuit_seulement_l_egaliseur: true,
            flux_sans_fin: true,
            format_independant_du_traitement,
            ..Default::default()
        })
    }

    /// Le poste d'un relais DSP progressif.
    pub(crate) fn pour_le_relais(compensation_cuite: bool) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            compensation_cuite,
            ..Default::default()
        })
    }

    /// Déposer le profil que le porteur posera au prochain bloc. Un dépôt
    /// plus récent remplace celui qui n'a pas encore été relevé : seul le
    /// dernier réglage compte.
    pub(crate) fn poser(&self, profil: Option<EqProfile>) {
        let mut attente = self.attente.lock().unwrap_or_else(|e| e.into_inner());
        self.vers_le_sec.store(false, Ordering::SeqCst);
        *attente = Some(profil);
        self.poses.fetch_add(1, Ordering::Relaxed);
    }

    /// #8422 — l'entrée en PURE, déposée en vol : couper l'égaliseur ET
    /// rendre le signal sec. Le fondu de #5215 efface la courbe, puis le
    /// préampli gardé remonte jusqu'à 0 dB à la vitesse de #6741 (aucune
    /// marche), sans la borne de volume ; revenu à 0 dB, le neutre quitte la
    /// chaîne et le flux redevient les octets décodés, à l'échantillon près.
    pub(crate) fn poser_vers_le_sec(&self) {
        let mut attente = self.attente.lock().unwrap_or_else(|e| e.into_inner());
        self.vers_le_sec.store(true, Ordering::SeqCst);
        *attente = Some(None);
        self.poses.fetch_add(1, Ordering::Relaxed);
    }

    /// Côté porteur, avant chaque bloc : retirer un égaliseur coupé dont le
    /// fondu est fini, puis relever le profil déposé s'il y en a un.
    ///
    /// `try_lock` : le porteur ne s'arrête jamais pour attendre la route ;
    /// un dépôt en cours sera relevé au bloc suivant.
    pub(crate) fn relever(
        &self,
        courant: &mut Option<EqProcessor>,
        sample_rate: u32,
        channels: u16,
    ) {
        if courant
            .as_ref()
            .is_some_and(EqProcessor::est_neutre_au_repos)
        {
            *courant = None;
        }
        let (depose, vers_le_sec) = match self.attente.try_lock() {
            Ok(mut g) => {
                let depose = g.take();
                let sec = depose.is_some() && self.vers_le_sec.swap(false, Ordering::SeqCst);
                (depose, sec)
            }
            Err(_) => (None, false),
        };
        let Some(profil) = depose else {
            self.suivre_le_niveau_d_avant(courant);
            return;
        };
        let neuf = profil
            .map(|p| EqProcessor::new(&p, sample_rate, channels))
            .filter(EqProcessor::is_enabled);
        let actif = neuf.is_some();
        *courant = EqProcessor::prendre_la_releve(neuf, courant.take());
        if vers_le_sec && let Some(eq) = courant.as_mut() {
            // Avant l'extinction ci-dessous : sa cible est alors le sec.
            eq.liberer_le_gain_garde();
        }
        self.suivre_le_niveau_d_avant(courant);
        if self.flux_sans_fin
            && let Some(eq) = courant.as_mut()
        {
            eq.eteindre_le_preampli_garde();
        }
        self.releves.fetch_add(1, Ordering::Relaxed);
        tracing::info!(sample_rate, channels, actif, "eq_en_vol_releve");
    }

    /// Combien de profils ont été déposés.
    pub(crate) fn poses(&self) -> u64 {
        self.poses.load(Ordering::Relaxed)
    }

    /// Combien de profils le porteur a relevés.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn releves(&self) -> u64 {
        self.releves.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::StreamingDsp;
    use super::EqEnVol;

    const SR: u32 = 44_100;

    /// Sinus 1 kHz stéréo 16 bits, `trames` trames à partir de la trame `debut`.
    fn sinus(debut: usize, trames: usize) -> Vec<u8> {
        (debut..debut + trames)
            .flat_map(|i| {
                let v = (0.25
                    * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(SR)).sin()
                    * 32_767.0) as i16;
                [v.to_le_bytes(), v.to_le_bytes()].concat()
            })
            .collect()
    }

    fn profil() -> crate::audio::eq::EqProfile {
        crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 80.0,
                gain: 9.0,
                q: 0.71,
                band_type: "low_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn voie_gauche(pcm: &[u8]) -> Vec<i32> {
        pcm.as_chunks::<4>()
            .0
            .iter()
            .map(|t| i32::from(i16::from_le_bytes([t[0], t[1]])))
            .collect()
    }

    fn rms(pcm: &[u8]) -> f64 {
        let v = voie_gauche(pcm);
        (v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
    }

    fn plus_grand_saut(pcm: &[u8]) -> i32 {
        voie_gauche(pcm)
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .max()
            .unwrap_or(0)
    }

    /// Passe `n` blocs de sinus dans le relais ; rend le dernier bloc servi.
    fn passer(
        dsp: &mut StreamingDsp,
        position: &mut usize,
        n: usize,
        bloc: usize,
        servi: &mut Vec<u8>,
    ) -> Vec<u8> {
        let mut dernier = Vec::new();
        for _ in 0..n {
            let mut b = sinus(*position, bloc);
            dsp.process(&mut b, 16);
            *position += bloc;
            servi.extend_from_slice(&b);
            dernier = b;
        }
        dernier
    }

    /// #4407 — le relais progressif d'un flux réseau relève l'égaliseur
    /// déposé en vol : sans marche (fondu de #5215 sur le chemin des
    /// OCTETS), au niveau de l'égaliseur une fois le fondu fini, puis de
    /// retour au signal SEC, à l'octet près, quand on le coupe.
    #[test]
    fn le_relais_progressif_releve_l_egaliseur_en_vol_sans_marche_4407() {
        let poste = EqEnVol::pour_le_relais(false);
        let mut dsp = StreamingDsp {
            channels: 2,
            sample_rate: SR,
            en_vol: Some(poste.clone()),
            ..Default::default()
        };
        let bloc = 2_048;
        let mut position = 0usize;
        let mut servi = Vec::new();

        // Sans égaliseur : identité.
        let sec = passer(&mut dsp, &mut position, 4, bloc, &mut servi);
        assert_eq!(servi, sinus(0, 4 * bloc), "sans égaliseur, rien ne change");

        // Dépôt en vol, puis 300 ms de blocs : le fondu est fini.
        poste.poser(Some(profil()));
        let egalise = passer(&mut dsp, &mut position, 8, bloc, &mut servi);
        assert_eq!(poste.releves(), 1);
        let mut etalon = crate::audio::eq::EqProcessor::new(&profil(), SR, 2);
        let mut reference = sinus(0, 40 * bloc);
        etalon.process_pcm(&mut reference, 16);
        let attendu = rms(&reference[reference.len() - 4 * bloc..]);
        let ecart_db = 20.0 * (rms(&egalise) / attendu).log10();
        assert!(
            ecart_db.abs() < 0.5,
            "après le fondu, le flux porte l'égaliseur ({ecart_db:+.2} dB de l'étalon)"
        );
        assert!(
            (20.0 * (rms(&egalise) / rms(&sec)).log10()).abs() > 1.0,
            "prémisse : cet égaliseur change le niveau du sinus"
        );

        // Coupure en vol (#5215, décision du 10/10) : la courbe s'efface,
        // le préampli reste. Fondu fini, le relais rend le sinus au niveau
        // du préampli, et le neutre qui le porte reste monté.
        let preampli = crate::audio::eq::EqProcessor::new(&profil(), SR, 2)
            .preamp_db(0)
            .expect("préampli chiffré");
        assert!(preampli < -1.0, "prémisse : réserve de {preampli} dB");
        poste.poser(None);
        passer(&mut dsp, &mut position, 8, bloc, &mut servi);
        let avant = position;
        let apres = passer(&mut dsp, &mut position, 2, bloc, &mut servi);
        let ecart_db = 20.0 * (rms(&apres) / rms(&sinus(avant + bloc, bloc))).log10();
        assert!(
            (ecart_db - preampli).abs() < 0.1,
            "égaliseur coupé, fondu fini : le relais garde le préampli \
             ({ecart_db:+.2} dB pour {preampli:+.2} dB)"
        );
        assert!(
            dsp.eq.as_ref().is_some_and(|e| e.est_neutre()),
            "le préampli gardé reste porté par un égaliseur neutre"
        );

        // Aucune marche sur tout le flux : le plus grand saut d'un
        // échantillon au suivant reste celui d'un sinus, pas d'une coupure.
        let saut_sinus = plus_grand_saut(&sinus(0, bloc));
        let saut = plus_grand_saut(&servi);
        assert!(
            f64::from(saut) < 1.5 * f64::from(saut_sinus) * 10f64.powf(9.0 / 20.0),
            "marche audible dans le flux : saut {saut} pour {saut_sinus} sur le sinus sec"
        );
    }

    /// Une radio servie par le décodeur de Tune : `n` blocs de sinus 1 kHz,
    /// relève avant chaque bloc comme le porteur ; rend la voie gauche.
    struct RadioFactice {
        poste: std::sync::Arc<EqEnVol>,
        eq: Option<crate::audio::eq::EqProcessor>,
        position: usize,
        servi: Vec<f32>,
    }

    impl RadioFactice {
        const BLOC: usize = 1_024;

        fn new() -> Self {
            Self {
                poste: EqEnVol::pour_la_radio(),
                eq: None,
                position: 0,
                servi: Vec::new(),
            }
        }

        fn passer(&mut self, n: usize) -> Vec<f32> {
            let mut rendu = Vec::new();
            for _ in 0..n {
                let mut b: Vec<f32> = (self.position..self.position + Self::BLOC)
                    .flat_map(|i| {
                        let v = (0.25
                            * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(SR))
                                .sin()) as f32;
                        [v, v]
                    })
                    .collect();
                self.poste.relever(&mut self.eq, SR, 2);
                crate::orchestrator::radio::apply_radio_eq(&mut self.eq, &mut b);
                self.position += Self::BLOC;
                rendu.extend(b.chunks(2).map(|t| t[0]));
            }
            self.servi.extend_from_slice(&rendu);
            rendu
        }

        fn plus_grand_saut(&self) -> f32 {
            self.servi
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0_f32, f32::max)
        }
    }

    fn rms_f32(v: &[f32]) -> f64 {
        (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
    }

    /// #6741 (décision de Bertrand du 22/10, suite de #6211 et #6524) — sur
    /// une RADIO réseau, couper l'égaliseur en vol rend le niveau d'avant
    /// l'égaliseur, sans relancer la station, progressivement, sans marche,
    /// et sans jamais le dépasser. Remplace le témoin du 21/10 qui gardait le
    /// préampli « jusqu'au prochain geste » (#6708).
    #[test]
    fn sur_une_radio_couper_l_egaliseur_rend_le_niveau_d_avant_sans_le_depasser_6741() {
        let mut radio = RadioFactice::new();
        radio.poste.noter_le_volume(0.5);
        let avant = rms_f32(&radio.passer(8));
        radio.poste.poser(Some(profil()));
        radio.passer(20);
        let preampli = radio
            .eq
            .as_ref()
            .and_then(|e| e.preamp_db(0))
            .expect("égaliseur monté");
        assert!(preampli < -1.0, "prémisse : réserve de {preampli} dB");
        radio.poste.poser(None);
        let mut plus_haut = f64::MIN;
        // 300 ms de fondu, puis 3 s au moins de remontée : 6 s jouées.
        for n in 0..260 {
            let bloc_db = 20.0 * (rms_f32(&radio.passer(1)) / avant).log10();
            plus_haut = plus_haut.max(bloc_db);
            assert!(
                bloc_db < 0.1,
                "radio, égaliseur coupé : {bloc_db:+.2} dB au bloc {n}, au-dessus du niveau                  d'avant l'égaliseur — jamais de dépassement (#6741)"
            );
        }
        let apres = rms_f32(&radio.passer(8));
        let ecart_db = 20.0 * (apres / avant).log10();
        assert!(
            ecart_db.abs() < 0.2,
            "radio, égaliseur coupé : {ecart_db:+.2} dB du niveau d'avant l'égaliseur              (préampli {preampli:+.2} dB) — couper l'égaliseur doit le rendre"
        );
        radio.passer(1);
        assert!(
            radio.eq.is_none(),
            "revenu au niveau d'avant, le neutre quitte la chaîne"
        );
        let saut_sinus = 0.25 * 2.0 * std::f32::consts::PI * 1000.0 / SR as f32;
        let saut = radio.plus_grand_saut();
        assert!(
            saut < 1.5 * saut_sinus * 10f32.powf(9.0 / 20.0),
            "marche audible : saut {saut} pour {saut_sinus} sur le sinus sec"
        );
    }

    /// Un appareil dont la courbe de volume est en dB (50 dB de course, le cas
    /// de beaucoup de renderers DLNA) : le niveau rendu n'est PAS
    /// proportionnel au pourcentage envoyé.
    fn appareil_en_db(consigne: f64) -> f64 {
        if consigne <= 0.0 {
            0.0
        } else {
            10f64.powf((consigne - 1.0) * 50.0 / 20.0)
        }
    }

    /// #6741 — la borne, sur un APPAREIL dont Tune ignore la courbe de volume.
    /// L'auditeur monte la consigne de la zone pendant que l'égaliseur joue
    /// (25 % → 50 %). Sur une courbe de 50 dB, c'est +12,5 dB, et non les
    /// +6 dB qu'un rapport de pourcentages supposerait : borner la remontée à
    /// ce rapport ferait passer le niveau 6,5 dB AU-DESSUS de celui d'avant
    /// l'égaliseur. Après la coupure, le préampli est donc GARDÉ tel quel
    /// (aucune hausse) ; une consigne revenue à 25 % le libère, et le niveau
    /// rejoint celui d'avant sans le dépasser.
    #[test]
    fn sur_une_radio_volume_monte_pendant_l_egaliseur_jamais_au_dessus_du_niveau_d_avant_6741() {
        let mut radio = RadioFactice::new();
        let mut consigne = 0.25;
        radio.poste.noter_le_volume(consigne);
        let avant = rms_f32(&radio.passer(8)) * appareil_en_db(consigne);
        radio.poste.poser(Some(profil()));
        radio.passer(20);
        let preampli = radio
            .eq
            .as_ref()
            .and_then(|e| e.preamp_db(0))
            .expect("égaliseur monté");
        consigne = 0.5;
        radio.poste.noter_le_volume(consigne);
        let avant_la_coupure = {
            radio.passer(20);
            rms_f32(&radio.passer(4)) * appareil_en_db(consigne)
        };
        radio.poste.poser(None);
        for n in 0..300 {
            let rendu = rms_f32(&radio.passer(1)) * appareil_en_db(consigne);
            assert!(
                rendu <= avant_la_coupure * 1.012,
                "radio, consigne montée à 50 % : niveau rendu {rendu:.6} au bloc {n}, \
                 au-dessus de celui d'avant la coupure {avant_la_coupure:.6} — la coupure ne \
                 doit rien faire monter quand la courbe de l'appareil est inconnue (#6741)"
            );
        }
        let garde = radio
            .eq
            .as_ref()
            .and_then(|e| e.preampli_garde())
            .expect("le préampli est gardé jusqu'au prochain réglage");
        assert!(
            !garde.en_retour && (garde.db - preampli).abs() < 0.3,
            "gardé {garde:?}, attendu le préampli entier ({preampli:.2} dB)"
        );
        // Le prochain réglage : la consigne revient à 25 %.
        consigne = 0.25;
        radio.poste.noter_le_volume(consigne);
        for n in 0..300 {
            let rendu = rms_f32(&radio.passer(1)) * appareil_en_db(consigne);
            assert!(rendu <= avant * 1.012, "bloc {n} : {rendu:.6} > {avant:.6}");
        }
        let fin = rms_f32(&radio.passer(8)) * appareil_en_db(consigne);
        assert!(
            (fin / avant - 1.0).abs() < 0.02,
            "consigne revenue à 25 % : {fin:.6}, attendu {avant:.6}"
        );
    }

    /// #6741 — un poste dont la consigne de la zone est inconnue ne fait rien
    /// remonter : sans niveau d'avant, le préampli est gardé (#6708).
    #[test]
    fn sans_consigne_connue_le_preampli_reste_garde_6741() {
        let mut radio = RadioFactice::new();
        radio.passer(8);
        radio.poste.poser(Some(profil()));
        radio.passer(20);
        let preampli = radio
            .eq
            .as_ref()
            .and_then(|e| e.preamp_db(0))
            .expect("égaliseur monté");
        radio.poste.poser(None);
        radio.passer(300);
        let garde = radio
            .eq
            .as_ref()
            .and_then(|e| e.preampli_garde())
            .expect("le préampli est gardé");
        assert!(
            (garde.db - preampli).abs() < 0.3,
            "gardé {garde:?}, attendu {preampli:.2} dB : sans consigne connue, rien ne remonte"
        );
    }

    /// #8422 — l'entrée en PURE déposée en vol rend le signal SEC, à
    /// l'échantillon près, même quand la consigne de volume est inconnue (là
    /// où une simple coupure garde le préampli, #6708) : la chaîne se vide,
    /// sans marche, et rien ne dépasse le niveau d'avant l'égaliseur.
    #[test]
    fn entree_en_pure_en_vol_rend_le_signal_sec_a_l_echantillon_pres_8422() {
        let mut radio = RadioFactice::new();
        let avant = rms_f32(&radio.passer(8));
        radio.poste.poser(Some(profil()));
        radio.passer(20);
        assert!(
            radio.eq.as_ref().and_then(|e| e.preamp_db(0)).unwrap() < -1.0,
            "prémisse : l'égaliseur porte une réserve"
        );
        radio.poste.poser_vers_le_sec();
        // 300 ms de fondu, puis la remontée du préampli à 2 dB/s : 8 s jouées.
        for n in 0..350 {
            let bloc_db = 20.0 * (rms_f32(&radio.passer(1)) / avant).log10();
            assert!(bloc_db < 0.1, "{bloc_db:+.2} dB au bloc {n} : dépassement");
        }
        assert!(radio.eq.is_none(), "PURE : plus aucun étage dans la chaîne");
        let debut = radio.position;
        let servi = radio.passer(4);
        let sec: Vec<f32> = (debut..debut + 4 * RadioFactice::BLOC)
            .map(|i| {
                (0.25 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(SR)).sin())
                    as f32
            })
            .collect();
        assert_eq!(servi, sec, "PURE : les échantillons décodés, tels quels");
        let saut_sinus = 0.25 * 2.0 * std::f32::consts::PI * 1000.0 / SR as f32;
        assert!(
            radio.plus_grand_saut() < 1.5 * saut_sinus * 10f32.powf(9.0 / 20.0),
            "marche audible pendant l'entrée en PURE"
        );
    }

    /// #8422 — un dépôt ordinaire après une entrée en PURE annule la
    /// remontée vers le sec : la sortie de PURE repose l'égaliseur.
    #[test]
    fn un_depot_ordinaire_efface_la_demande_de_signal_sec_8422() {
        let poste = EqEnVol::pour_la_radio();
        poste.poser_vers_le_sec();
        poste.poser(Some(profil()));
        let mut courant = None;
        poste.relever(&mut courant, SR, 2);
        assert!(courant.as_ref().is_some_and(|e| !e.est_neutre()));
    }

    /// Un poste dont personne n'a rien déposé ne touche à rien.
    #[test]
    fn un_poste_sans_depot_ne_change_rien() {
        let poste = EqEnVol::pour_la_radio();
        let mut courant = None;
        poste.relever(&mut courant, SR, 2);
        assert!(courant.is_none());
        assert_eq!(poste.releves(), 0);
        assert_eq!(poste.poses(), 0);
    }
}
