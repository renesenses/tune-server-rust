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
use std::sync::atomic::{AtomicU64, Ordering};

/// Le poste de relève de l'égaliseur d'UN flux.
#[derive(Default)]
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
}

impl EqEnVol {
    /// Le poste d'un flux radio décodé par Tune.
    pub(crate) fn pour_la_radio() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cuit_seulement_l_egaliseur: true,
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
        *self.attente.lock().unwrap_or_else(|e| e.into_inner()) = Some(profil);
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
        let depose = match self.attente.try_lock() {
            Ok(mut g) => g.take(),
            Err(_) => None,
        };
        let Some(profil) = depose else {
            return;
        };
        let neuf = profil
            .map(|p| EqProcessor::new(&p, sample_rate, channels))
            .filter(EqProcessor::is_enabled);
        let actif = neuf.is_some();
        *courant = EqProcessor::prendre_la_releve(neuf, courant.take());
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
        pcm.chunks_exact(4)
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
