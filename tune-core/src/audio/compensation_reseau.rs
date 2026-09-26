//! #5071 — la compensation de niveau (#4685) CUITE dans le flux réseau.
//!
//! Sur une sortie locale, la compensation rend par le VOLUME ce que
//! l'égaliseur (réserve anti-écrêtage) et le crossfeed retirent au niveau
//! moyen ; le volume effectif est raboté à l'unité, donc elle ne peut pas
//! écrêter. Une zone réseau (DLNA, OpenHome, Chromecast, navigateur…) n'a pas
//! ce levier : son volume est tenu par le renderer. Jusqu'ici l'interrupteur y
//! était enregistré, et rien ne l'appliquait : l'égaliseur baissait le niveau
//! de sa réserve, et rien ne le rendait (Jean Valjean, fil 1771, 0.9.165).
//!
//! Décision de Bertrand (25/09) : l'appliquer AUSSI au flux réseau, par le
//! MÊME chemin que l'égaliseur. Cet étage n'est donc pas un second moteur :
//! c'est un gain, appliqué par la primitive du ReplayGain
//! ([`crate::audio::replaygain::apply_gain_pcm_compte`] : même dither, même
//! saturation, même comptage), en DERNIER dans les deux porteurs du réseau —
//! le fichier ré-encodé (`transcode_source_to_file_avec_crossfeed`) et le
//! relais progressif (`StreamingDsp`).
//!
//! ## La réserve : jamais d'écrêtage
//!
//! Rendre `+C` dB après l'égaliseur, c'est rendre aux crêtes les `C` dB que sa
//! réserve leur avait pris. La sortie locale s'en protège par le rabot du
//! volume ; ici, le gain est borné PAR LE SIGNAL : avant chaque bloc, sa crête
//! est mesurée, et le gain retenu vaut `min(cible, PLAFOND / crête)`. Le gain
//! ne REMONTE jamais au cours du flux (aucun pompage) : il ne peut que
//! descendre, une fois, au premier bloc qui l'exige.
//!
//! - **fichier ré-encodé** : la piste ENTIÈRE est un seul bloc. Le gain est
//!   donc constant sur toute la piste, borné par sa vraie crête ;
//! - **relais progressif** : le gain part de la cible et descend au premier
//!   bloc dont la crête l'exige. Un saut de niveau, une fois, à la frontière
//!   d'un bloc — le prix d'un flux qui ne voit pas l'avenir, et le seul moyen
//!   de ne jamais écrêter sans limiteur.
//!
//! `PLAFOND` laisse 0,1 dB sous la pleine échelle : le dither TPDF (±1 LSB) et
//! l'arrondi ne peuvent donc jamais pousser un échantillon au rail.
//!
//! ## Ce que l'étage ne fait pas
//!
//! - rien sans égaliseur ni crossfeed actifs : la cible est alors nulle et
//!   l'étage n'existe pas ([`CompensationReseau::depuis_db`] rend `None`) —
//!   le flux reste celui d'avant, à l'octet près ;
//! - rien en mode PURE : les chargeurs de l'égaliseur et du crossfeed rendent
//!   `None`, la cible est nulle (même règle que la sortie locale) ;
//! - rien sur une sortie `local:`, qui compense déjà par son volume.

use crate::audio::ecretage::CompteurDEcretage;

/// Le plafond des crêtes après compensation : −0,1 dBFS.
pub const PLAFOND_DBFS: f64 = -0.1;

/// Une compensation plus petite que ce seuil n'est pas appliquée : elle ne
/// changerait rien d'audible, et laisser l'étage absent garde le flux intact.
pub const SEUIL_DB: f64 = 0.01;

/// L'étage de compensation d'un flux réseau.
#[derive(Debug, Clone)]
pub struct CompensationReseau {
    /// Ce que l'interrupteur demande, en dB : l'inverse du gain moyen de
    /// l'égaliseur et du crossfeed installés.
    cible_db: f64,
    /// Le gain LINÉAIRE retenu à ce point du flux. Part de la cible, ne fait
    /// que descendre.
    gain: f64,
    /// Nombre de fois où la crête a fait descendre le gain.
    rabots: u64,
    /// Ce que l'étage a écrêté — zéro par construction ; compté par la
    /// primitive du gain, pour que les tests et le journal le PROUVENT.
    ecretage: CompteurDEcretage,
}

fn plafond_lineaire() -> f64 {
    10f64.powf(PLAFOND_DBFS / 20.0)
}

impl CompensationReseau {
    /// L'étage pour une compensation de `compensation_db` dB. `None` quand
    /// elle est nulle (sous [`SEUIL_DB`]) ou non finie : pas d'étage, pas un
    /// octet touché.
    pub fn depuis_db(compensation_db: f64) -> Option<Self> {
        if !compensation_db.is_finite() || compensation_db.abs() < SEUIL_DB {
            return None;
        }
        Some(Self {
            cible_db: compensation_db,
            gain: 10f64.powf(compensation_db / 20.0),
            rabots: 0,
            ecretage: CompteurDEcretage::default(),
        })
    }

    /// La compensation demandée, en dB.
    pub fn cible_db(&self) -> f64 {
        self.cible_db
    }

    /// Le gain réellement appliqué au dernier bloc, en dB.
    pub fn gain_applique_db(&self) -> f64 {
        20.0 * self.gain.log10()
    }

    /// Combien de fois la crête a fait descendre le gain.
    pub fn rabots(&self) -> u64 {
        self.rabots
    }

    /// Ce que l'étage a écrêté depuis sa création.
    pub fn ecretage(&self) -> &CompteurDEcretage {
        &self.ecretage
    }

    /// Applique la compensation EN PLACE sur un bloc de PCM entier entrelacé
    /// (16, 24 ou 32 bits, petit-boutiste — le format de tous les porteurs).
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16) {
        if pcm.is_empty() {
            return;
        }
        let crete = crete_relative(pcm, bit_depth);
        let plafond = plafond_lineaire();
        if crete > 0.0 && crete * self.gain > plafond {
            let avant = self.gain_applique_db();
            self.gain = plafond / crete;
            self.rabots += 1;
            tracing::debug!(
                cible_db = self.cible_db,
                avant_db = avant,
                apres_db = self.gain_applique_db(),
                crete_dbfs = 20.0 * crete.log10(),
                "compensation_reseau_rabotee_a_la_crete"
            );
        }
        crate::audio::replaygain::apply_gain_pcm_compte(
            pcm,
            bit_depth,
            self.gain,
            &mut self.ecretage,
        );
    }
}

/// La compensation ENTRE dans la clé du cache de transcodage (LAT-F2) : elle
/// change les octets encodés. `None` rend la clé d'avant, inchangée.
pub fn empreinte_avec_compensation(
    dsp: Option<[u8; 32]>,
    compensation_db: Option<f64>,
) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some(db) = compensation_db else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"compensation\0");
    h.update(db.to_le_bytes());
    // Le plafond fait partie du calcul du gain : le changer change les octets.
    h.update(PLAFOND_DBFS.to_le_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

/// La crête d'un bloc, relative à la pleine échelle (0,0 à 1,0).
pub fn crete_relative(pcm: &[u8], bit_depth: u16) -> f64 {
    match bit_depth {
        16 => {
            pcm.chunks_exact(2)
                .map(|s| (i16::from_le_bytes([s[0], s[1]]) as i32).unsigned_abs())
                .max()
                .unwrap_or(0) as f64
                / 32_768.0
        }
        24 => {
            pcm.chunks_exact(3)
                .map(|s| {
                    (((s[2] as i32) << 24 | (s[1] as i32) << 16 | (s[0] as i32) << 8) >> 8)
                        .unsigned_abs()
                })
                .max()
                .unwrap_or(0) as f64
                / 8_388_608.0
        }
        32 => {
            pcm.chunks_exact(4)
                .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]).unsigned_abs())
                .max()
                .unwrap_or(0) as f64
                / 2_147_483_648.0
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16(echantillons: &[i16]) -> Vec<u8> {
        echantillons.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn lire16(pcm: &[u8]) -> Vec<i16> {
        pcm.chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]))
            .collect()
    }

    fn sinus16(amplitude: f64, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                (amplitude * 32_767.0 * (i as f64 * 2.0 * std::f64::consts::PI / 100.0).sin())
                    .round() as i16
            })
            .collect()
    }

    #[test]
    fn une_compensation_nulle_n_installe_aucun_etage() {
        assert!(CompensationReseau::depuis_db(0.0).is_none());
        assert!(CompensationReseau::depuis_db(0.004).is_none());
        assert!(CompensationReseau::depuis_db(f64::NAN).is_none());
        assert!(CompensationReseau::depuis_db(3.0).is_some());
    }

    #[test]
    fn un_signal_bas_recoit_tout_le_gain() {
        // Crête à −20 dBFS, +6 dB demandés : rien ne borne, le gain est plein.
        let mut etage = CompensationReseau::depuis_db(6.0).unwrap();
        let mut pcm = pcm16(&sinus16(0.1, 4410));
        let avant = crete_relative(&pcm, 16);
        etage.process_pcm(&mut pcm, 16);
        let apres = crete_relative(&pcm, 16);
        let gain_db = 20.0 * (apres / avant).log10();
        assert!((gain_db - 6.0).abs() < 0.05, "gain mesuré {gain_db} dB");
        assert_eq!(etage.rabots(), 0);
        assert_eq!(etage.ecretage().echantillons_ecretes, 0);
    }

    #[test]
    fn un_signal_fort_est_borne_sans_ecreter() {
        // Crête à −1 dBFS, +10 dB demandés : sans réserve, 9 dB d'écrêtage.
        let mut etage = CompensationReseau::depuis_db(10.0).unwrap();
        let mut pcm = pcm16(&sinus16(0.891, 44_100));
        etage.process_pcm(&mut pcm, 16);
        assert_eq!(etage.ecretage().echantillons_ecretes, 0);
        let crete = crete_relative(&pcm, 16);
        // Le dither TPDF (±1 LSB) et l'arrondi peuvent dépasser le plafond de
        // 2 LSB au plus — sous le rail de ~370 LSB : c'est la marge.
        assert!(
            crete <= plafond_lineaire() + 2.0 / 32_768.0,
            "crête {crete}"
        );
        assert!(
            lire16(&pcm).iter().all(|&s| s != i16::MAX && s != i16::MIN),
            "un échantillon a touché le rail"
        );
        assert_eq!(etage.rabots(), 1);
        assert!(etage.gain_applique_db() < 1.0);
    }

    #[test]
    fn le_gain_ne_remonte_jamais() {
        let mut etage = CompensationReseau::depuis_db(10.0).unwrap();
        let mut fort = pcm16(&sinus16(0.9, 4410));
        etage.process_pcm(&mut fort, 16);
        let apres_fort = etage.gain_applique_db();
        let mut faible = pcm16(&sinus16(0.01, 4410));
        etage.process_pcm(&mut faible, 16);
        assert_eq!(etage.gain_applique_db(), apres_fort);
    }

    #[test]
    fn vingt_quatre_bits_aussi() {
        let mut etage = CompensationReseau::depuis_db(12.0).unwrap();
        let mut pcm: Vec<u8> = (0..4410)
            .flat_map(|i| {
                let v = (8_000_000.0 * (i as f64 / 7.0).sin()) as i32;
                [
                    (v & 0xFF) as u8,
                    ((v >> 8) & 0xFF) as u8,
                    ((v >> 16) & 0xFF) as u8,
                ]
            })
            .collect();
        etage.process_pcm(&mut pcm, 24);
        assert_eq!(etage.ecretage().echantillons_ecretes, 0);
        assert!(crete_relative(&pcm, 24) <= plafond_lineaire() + 1e-6);
    }
}
