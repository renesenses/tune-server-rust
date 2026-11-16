//! #8869 (forum, fil 2860) — le silence de verrouillage d'une sortie locale.
//!
//! Un DAC qui change de cadence ou de mode (PCM, DSD natif, DoP) se tait le
//! temps de se verrouiller sur le nouveau flux : sur une puce AKM (Topping
//! E30 II en ASIO, DSD natif), la première note de la piste part pendant ce
//! temps-là et n'est jamais entendue.
//!
//! Le réglage de zone `zone_{id}_silence_verrouillage_ms` (0 à
//! [`SILENCE_MAX_MS`], **0 par défaut** : aucune zone existante ne change)
//! fait précéder la piste d'autant de silence, **seulement** quand le format
//! envoyé au DAC change par rapport au flux précédent de la même sortie, ou
//! au premier flux de cette sortie. Une piste au même format part comme
//! avant, sans un échantillon de plus.
//!
//! Ce module tient la partie pure : la clé, la lecture bornée du réglage, la
//! signature du format envoyé au DAC et la mémoire du dernier format, par
//! sortie. Ce qu'est un silence pour chaque transport (zéros PCM, trames de
//! repos DoP, motif DSD 0x69) et l'endroit où il est inséré vivent dans
//! `outputs::local::silence_de_verrouillage_8869`.

use std::sync::{Arc, Mutex};

use crate::db::{backend::DbBackend, settings_repo::SettingsRepo};

/// Borne haute du réglage : la borne du champ de l'interface.
pub const SILENCE_MAX_MS: u32 = 2_000;

/// La clé du réglage d'une zone.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_silence_verrouillage_ms")
}

/// Une valeur demandée ramenée dans `0..=SILENCE_MAX_MS`.
pub fn borner(ms: i64) -> u32 {
    ms.clamp(0, i64::from(SILENCE_MAX_MS)) as u32
}

/// Le réglage de la zone (ms), borné ; `0` quand il est absent ou illisible.
pub fn lire(db: &Arc<dyn DbBackend>, zone_id: i64) -> u32 {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(borner)
        .unwrap_or(0)
}

/// Écrit le réglage de la zone, borné. `0` SUPPRIME la clé : l'absence et le
/// défaut sont un seul état. Rend la valeur retenue.
pub fn ecrire(db: &Arc<dyn DbBackend>, zone_id: i64, ms: i64) -> Result<u32, String> {
    let ms = borner(ms);
    let reglages = SettingsRepo::with_backend(db.clone());
    let cle = cle_de_zone(zone_id);
    if ms == 0 {
        reglages.delete(&cle)?;
    } else {
        reglages.set(&cle, &ms.to_string())?;
    }
    Ok(ms)
}

/// Le mode du flux tel que le DAC le reçoit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeDuFlux {
    /// Du PCM.
    Pcm,
    /// Du DSD porté dans des mots PCM 24 bits (marqueurs 0x05 / 0xFA).
    DoP,
    /// Du DSD natif (ASIO natif, ALSA `DSD_U32`).
    DsdNatif,
}

impl ModeDuFlux {
    /// Code stable, pour le journal.
    pub fn code(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::DoP => "dop",
            Self::DsdNatif => "dsd_natif",
        }
    }
}

/// Ce qui, envoyé au DAC, l'oblige à se reverrouiller quand ça change.
///
/// `cadence` est la cadence du flux au DAC : celle du périphérique ouvert en
/// PCM et en DoP (176 400 Hz pour un DSD64 en DoP), la cadence DSD en natif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureDuFormat {
    pub cadence: u32,
    pub canaux: u16,
    pub mode: ModeDuFlux,
}

/// LA règle, pure : la durée de silence (ms) à insérer devant un flux de
/// format `nouveau`, quand le flux précédent de la même sortie était
/// `precedent` (`None` : premier flux de la sortie).
pub fn duree_du_silence_ms(
    precedent: Option<SignatureDuFormat>,
    nouveau: SignatureDuFormat,
    reglage_ms: u32,
) -> u32 {
    if reglage_ms == 0 || precedent == Some(nouveau) {
        return 0;
    }
    reglage_ms.min(SILENCE_MAX_MS)
}

/// Le nombre de trames de `ms` millisecondes à `cadence` trames par seconde.
pub fn trames_pour_ms(cadence: u32, ms: u32) -> usize {
    (u64::from(cadence) * u64::from(ms) / 1000) as usize
}

/// Le dernier format envoyé au DAC par une sortie. Partagé entre la sortie et
/// le fil de lecture de chaque piste ; vit aussi longtemps que la sortie.
#[derive(Debug, Clone, Default)]
pub struct MemoireDuFormat(Arc<Mutex<Option<SignatureDuFormat>>>);

impl MemoireDuFormat {
    /// Retient `nouveau` et rend le format précédent.
    pub fn retenir(&self, nouveau: SignatureDuFormat) -> Option<SignatureDuFormat> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(nouveau)
    }

    /// Le dernier format retenu.
    pub fn dernier(&self) -> Option<SignatureDuFormat> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Ce que le fil de lecture d'UNE piste emporte : le réglage, lu une fois par
/// piste comme les autres réglages de zone, et la mémoire de la sortie.
#[derive(Debug, Clone, Default)]
pub struct SilenceDeVerrouillage {
    reglage_ms: u32,
    memoire: MemoireDuFormat,
}

impl SilenceDeVerrouillage {
    pub fn new(reglage_ms: u32, memoire: MemoireDuFormat) -> Self {
        Self {
            reglage_ms: reglage_ms.min(SILENCE_MAX_MS),
            memoire,
        }
    }

    /// Le réglage lu pour cette piste.
    pub fn reglage_ms(&self) -> u32 {
        self.reglage_ms
    }

    /// Le flux part au format `signature` : retient ce format (même réglage
    /// nul, pour qu'un réglage posé plus tard compare au vrai précédent) et
    /// rend la durée de silence à insérer devant lui.
    pub fn constater(&self, signature: SignatureDuFormat) -> u32 {
        let precedent = self.memoire.retenir(signature);
        duree_du_silence_ms(precedent, signature, self.reglage_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCM_44: SignatureDuFormat = SignatureDuFormat {
        cadence: 44_100,
        canaux: 2,
        mode: ModeDuFlux::Pcm,
    };

    #[test]
    fn format_inchange_aucun_silence() {
        assert_eq!(duree_du_silence_ms(Some(PCM_44), PCM_44, 500), 0);
    }

    #[test]
    fn premier_flux_de_la_sortie_silence_regle() {
        assert_eq!(duree_du_silence_ms(None, PCM_44, 500), 500);
    }

    #[test]
    fn cadence_canaux_ou_mode_qui_changent_silence_regle() {
        let cadence = SignatureDuFormat {
            cadence: 96_000,
            ..PCM_44
        };
        let canaux = SignatureDuFormat {
            canaux: 6,
            ..PCM_44
        };
        let dop = SignatureDuFormat {
            cadence: 176_400,
            mode: ModeDuFlux::DoP,
            ..PCM_44
        };
        let pcm_176 = SignatureDuFormat {
            cadence: 176_400,
            ..PCM_44
        };
        let natif = SignatureDuFormat {
            cadence: 2_822_400,
            mode: ModeDuFlux::DsdNatif,
            ..PCM_44
        };
        assert_eq!(duree_du_silence_ms(Some(PCM_44), cadence, 750), 750);
        assert_eq!(duree_du_silence_ms(Some(PCM_44), canaux, 750), 750);
        assert_eq!(duree_du_silence_ms(Some(PCM_44), natif, 750), 750);
        // Même cadence au DAC, autre mode : le DAC bascule PCM ↔ DSD.
        assert_eq!(duree_du_silence_ms(Some(pcm_176), dop, 750), 750);
    }

    #[test]
    fn reglage_nul_jamais_de_silence() {
        assert_eq!(duree_du_silence_ms(None, PCM_44, 0), 0);
        let natif = SignatureDuFormat {
            cadence: 2_822_400,
            mode: ModeDuFlux::DsdNatif,
            ..PCM_44
        };
        assert_eq!(duree_du_silence_ms(Some(PCM_44), natif, 0), 0);
    }

    #[test]
    fn la_memoire_suit_les_flux_meme_reglage_nul() {
        let memoire = MemoireDuFormat::default();
        // Réglage nul : rien d'inséré, mais le format est retenu.
        assert_eq!(
            SilenceDeVerrouillage::new(0, memoire.clone()).constater(PCM_44),
            0
        );
        assert_eq!(memoire.dernier(), Some(PCM_44));
        // Le réglage posé ensuite compare au VRAI précédent.
        let v = SilenceDeVerrouillage::new(400, memoire.clone());
        assert_eq!(v.constater(PCM_44), 0);
        let dop = SignatureDuFormat {
            cadence: 176_400,
            mode: ModeDuFlux::DoP,
            ..PCM_44
        };
        assert_eq!(v.constater(dop), 400);
        assert_eq!(v.constater(dop), 0);
        assert_eq!(v.constater(PCM_44), 400);
    }

    #[test]
    fn bornes_et_trames() {
        assert_eq!(borner(-5), 0);
        assert_eq!(borner(9_999), SILENCE_MAX_MS);
        assert_eq!(
            SilenceDeVerrouillage::new(9_999, MemoireDuFormat::default()).reglage_ms(),
            2_000
        );
        assert_eq!(trames_pour_ms(44_100, 1_000), 44_100);
        assert_eq!(trames_pour_ms(176_400, 500), 88_200);
        assert_eq!(trames_pour_ms(48_000, 0), 0);
    }

    #[test]
    fn le_reglage_se_persiste_borne_et_zero_supprime_la_cle() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        assert_eq!(lire(&db, 7), 0, "absent : 0 par défaut");
        assert_eq!(ecrire(&db, 7, 5_000).unwrap(), SILENCE_MAX_MS);
        assert_eq!(lire(&db, 7), SILENCE_MAX_MS);
        assert_eq!(ecrire(&db, 7, 300).unwrap(), 300);
        assert_eq!(lire(&db, 7), 300);
        assert_eq!(lire(&db, 8), 0, "réglage PAR zone");
        assert_eq!(ecrire(&db, 7, 0).unwrap(), 0);
        assert!(
            SettingsRepo::with_backend(db.clone())
                .get(&cle_de_zone(7))
                .unwrap()
                .is_none(),
            "0 supprime la clé"
        );
    }
}
