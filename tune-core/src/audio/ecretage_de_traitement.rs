//! Le crête-mètre allumait son OVER sur le signal SOURCE.
//!
//! Alex, 26/10/2026 : Qobuz FLAC 44,1/24, serveur sous Docker, client web,
//! volume entre 20 et 30 %, et le crête-mètre annonçait un écrêtage.
//!
//! La cause : le forwarder de niveaux prélève ses fenêtres AU DÉCODEUR
//! (`observation_point: decoded_source`). Le seul gain qu'il y reporte est
//! celui que la sortie LOCALE déclare (#4384). Une zone réseau (DLNA,
//! OpenHome, AirPlay…, c'est-à-dire tout serveur sous Docker) n'en déclare
//! aucun : son volume est celui de l'appareil (#6739), et l'égaliseur, le
//! ReplayGain, le convolveur et le crossfeed sont appliqués APRÈS le point de
//! mesure, dans le relais progressif (`spawn_streaming_dsp_relay`). L'OVER
//! (trois échantillons consécutifs à pleine échelle, #4175) décrivait donc le
//! fichier. Un master moderne limité à 0 dBFS a de telles suites : rouge, quel
//! que soit le volume, et aucun traitement de Tune n'avait rien écrêté.
//!
//! Ce registre dit l'autre chose, celle qui mérite le rouge : **où le
//! traitement de Tune a poussé un échantillon jusqu'au rail**. Le relais le
//! tient en comparant chaque bloc avant et après sa chaîne. Un échantillon
//! écrêté par le traitement est au rail après la chaîne alors que l'échantillon
//! source n'y était pas. Les échantillons déjà au rail dans la source n'y
//! comptent pas : c'est la crête du disque, une information neutre que
//! `source_over_*` porte à part.
//!
//! Le registre est CREUX : seules les tranches écrêtées y entrent. Une piste
//! propre n'y laisse rien, et l'absence d'entrée veut dire « rien d'écrêté ».
//! Un registre par SESSION de flux, partagé par son relais (qui écrit) et son
//! forwarder (qui lit) : une piste suivante pré-chargée ou un saut n'y mêlent
//! jamais leurs tranches.
use std::collections::VecDeque;
use std::sync::Mutex;

/// Finesse des tranches, en millisecondes. La même que celle des crêtes de la
/// sortie locale (`crete_de_sortie::TRANCHE_MS`), qui laisse quatre tranches
/// par fenêtre de 40 ms.
pub const TRANCHE_MS: u32 = 10;

/// Nombre de tranches ÉCRÊTÉES gardées. Le relais court devant la lecture (le
/// renderer tamponne), et le registre ne garde que les tranches écrêtées :
/// 8192 tranches, c'est 82 s d'écrêtage ininterrompu.
const CAPACITE: usize = 8192;

#[derive(Debug, Clone, Copy)]
struct Tranche {
    debut_ms: f64,
    fin_ms: f64,
    gauche: bool,
    droite: bool,
}

/// Voir le module.
#[derive(Debug, Default)]
pub struct EcretagesDeTraitement {
    tranches: Mutex<VecDeque<Tranche>>,
}

/// La valeur brute d'un échantillon entier signé little-endian, et le rail
/// positif de sa profondeur. `None` hors 16, 24 et 32 bits.
fn lire(octets: &[u8], profondeur: u16) -> Option<i64> {
    Some(match profondeur {
        16 => i64::from(i16::from_le_bytes([octets[0], octets[1]])),
        24 => {
            let v = i32::from(octets[0])
                | (i32::from(octets[1]) << 8)
                | (i32::from(octets[2] as i8) << 16);
            i64::from(v)
        }
        32 => i64::from(i32::from_le_bytes([
            octets[0], octets[1], octets[2], octets[3],
        ])),
        _ => return None,
    })
}

/// Au rail : la valeur positive la plus haute, ou la plus basse négative.
fn au_rail(v: i64, profondeur: u16) -> bool {
    let rail = (1_i64 << (profondeur - 1)) - 1;
    v >= rail || v <= -rail - 1
}

impl EcretagesDeTraitement {
    pub fn new() -> Self {
        Self::default()
    }

    fn tranches(&self) -> std::sync::MutexGuard<'_, VecDeque<Tranche>> {
        self.tranches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Compare un bloc de PCM entier entrelacé AVANT (`avant`) et APRÈS
    /// (`apres`) la chaîne de traitement, et relève les tranches où un
    /// échantillon a été poussé au rail par le traitement. Le bloc commence à
    /// `debut_ms` dans la piste. Rend le nombre d'échantillons écrêtés.
    ///
    /// Canal 0 → gauche, canal 1 → droite ; un flux mono alimente les deux.
    /// Les canaux au-delà du second comptent pour la gauche et la droite à la
    /// fois : un centre écrêté est un écrêtage, et l'événement n'a que deux
    /// témoins.
    pub fn relever_pcm(
        &self,
        avant: &[u8],
        apres: &[u8],
        profondeur: u16,
        canaux: u16,
        cadence: u32,
        debut_ms: f64,
    ) -> u64 {
        let octets = usize::from(profondeur / 8);
        let canaux = usize::from(canaux);
        if octets == 0
            || canaux == 0
            || cadence == 0
            || !debut_ms.is_finite()
            || avant.len() != apres.len()
            || !(16..=32).contains(&profondeur)
        {
            return 0;
        }
        let trame = octets * canaux;
        let par_tranche = ((u64::from(cadence) * u64::from(TRANCHE_MS)) / 1000).max(1) as usize;
        let ms_par_trame = 1000.0 / f64::from(cadence);
        let mut ecretes = 0_u64;
        let mut nouvelles: Vec<Tranche> = Vec::new();
        let n_trames = avant.len() / trame;
        let mut t0 = 0_usize;
        while t0 < n_trames {
            let t1 = (t0 + par_tranche).min(n_trames);
            let (mut gauche, mut droite) = (false, false);
            for t in t0..t1 {
                for c in 0..canaux {
                    let o = t * trame + c * octets;
                    let (Some(a), Some(p)) = (
                        lire(&avant[o..o + octets], profondeur),
                        lire(&apres[o..o + octets], profondeur),
                    ) else {
                        continue;
                    };
                    if au_rail(p, profondeur) && !au_rail(a, profondeur) {
                        ecretes += 1;
                        match (canaux, c) {
                            (1, _) => {
                                gauche = true;
                                droite = true;
                            }
                            (_, 0) => gauche = true,
                            (_, 1) => droite = true,
                            _ => {
                                gauche = true;
                                droite = true;
                            }
                        }
                    }
                }
            }
            if gauche || droite {
                let debut = debut_ms + t0 as f64 * ms_par_trame;
                nouvelles.push(Tranche {
                    debut_ms: debut,
                    fin_ms: debut + (t1 - t0) as f64 * ms_par_trame,
                    gauche,
                    droite,
                });
            }
            t0 = t1;
        }
        if !nouvelles.is_empty() {
            let mut tranches = self.tranches();
            for n in nouvelles {
                if tranches.len() == CAPACITE {
                    tranches.pop_front();
                }
                tranches.push_back(n);
            }
        }
        ecretes
    }

    /// Le traitement a-t-il écrêté `(gauche, droite)` sur `[debut_ms, fin_ms)`
    /// de la piste ? `(false, false)` quand rien n'a été relevé.
    pub fn ecretage_entre(&self, debut_ms: f64, fin_ms: f64) -> (bool, bool) {
        let tranches = self.tranches();
        let (mut gauche, mut droite) = (false, false);
        for t in tranches.iter() {
            if t.fin_ms <= debut_ms || t.debut_ms >= fin_ms {
                continue;
            }
            gauche |= t.gauche;
            droite |= t.droite;
        }
        (gauche, droite)
    }
}

/// Ce que le relais progressif tient pour relever : le registre de sa session
/// et sa position dans la piste.
#[derive(Debug)]
pub struct ReleveDEcretage {
    registre: std::sync::Arc<EcretagesDeTraitement>,
    debut_ms: f64,
    cadence: u32,
    canaux: u16,
    profondeur: u16,
    octets_vus: u64,
}

impl ReleveDEcretage {
    /// `debut_ms` : la position de la piste où la session commence (le seek),
    /// dans le référentiel des fenêtres du forwarder de niveaux.
    pub fn new(
        registre: std::sync::Arc<EcretagesDeTraitement>,
        debut_ms: f64,
        cadence: u32,
        canaux: u16,
        profondeur: u16,
    ) -> Self {
        Self {
            registre,
            debut_ms,
            cadence,
            canaux,
            profondeur,
            octets_vus: 0,
        }
    }

    /// Relève un bloc qui vient de traverser la chaîne, puis avance la
    /// position. Un bloc dont la chaîne n'a rien changé coûte une comparaison.
    pub fn relever(&mut self, avant: &[u8], apres: &[u8]) {
        let trame = u64::from(self.profondeur / 8) * u64::from(self.canaux);
        if trame == 0 || self.cadence == 0 {
            return;
        }
        if avant != apres {
            let debut =
                self.debut_ms + (self.octets_vus / trame) as f64 * 1000.0 / f64::from(self.cadence);
            self.registre.relever_pcm(
                avant,
                apres,
                self.profondeur,
                self.canaux,
                self.cadence,
                debut,
            );
        }
        self.octets_vus += apres.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm24(echantillons: &[f64]) -> Vec<u8> {
        let mut v = Vec::with_capacity(echantillons.len() * 3);
        for s in echantillons {
            let x = (s * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
            v.extend_from_slice(&x.to_le_bytes()[..3]);
        }
        v
    }

    #[test]
    fn une_source_au_rail_que_la_chaine_laisse_au_rail_n_est_pas_un_ecretage() {
        let r = EcretagesDeTraitement::new();
        let source = pcm24(&[1.0, -1.0, 1.0, -1.0, 1.0, -1.0]);
        assert_eq!(r.relever_pcm(&source, &source, 24, 2, 44_100, 0.0), 0);
        assert_eq!(r.ecretage_entre(0.0, 40.0), (false, false));
    }

    #[test]
    fn un_echantillon_pousse_au_rail_par_la_chaine_est_releve_sur_son_canal() {
        let r = EcretagesDeTraitement::new();
        let avant = pcm24(&[0.6, 0.2, 0.6, 0.2]);
        let apres = pcm24(&[1.0, 0.4, 1.0, 0.4]);
        assert_eq!(r.relever_pcm(&avant, &apres, 24, 2, 44_100, 1_000.0), 2);
        assert_eq!(r.ecretage_entre(1_000.0, 1_040.0), (true, false));
        assert_eq!(
            r.ecretage_entre(0.0, 999.0),
            (false, false),
            "une fenêtre qui ne touche pas la tranche n'en hérite pas"
        );
    }

    #[test]
    fn la_releve_avance_dans_la_piste_bloc_apres_bloc() {
        let registre = std::sync::Arc::new(EcretagesDeTraitement::new());
        let mut releve = ReleveDEcretage::new(registre.clone(), 5_000.0, 1_000, 1, 24);
        // 1000 Hz mono : 100 trames = 100 ms. Premier bloc intact.
        let propre = pcm24(&vec![0.5; 100]);
        releve.relever(&propre, &propre);
        let ecrete = pcm24(&vec![1.0; 100]);
        releve.relever(&propre, &ecrete);
        assert_eq!(registre.ecretage_entre(5_000.0, 5_100.0), (false, false));
        assert_eq!(registre.ecretage_entre(5_100.0, 5_200.0), (true, true));
    }
}
