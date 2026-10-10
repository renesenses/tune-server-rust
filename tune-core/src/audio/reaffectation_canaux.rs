//! #6044 — le greffon « Réaffectation des canaux », côté hôte.
//!
//! Le moteur vit dans `sdk/tune-plugin-channel-remap` (une [`Matrice`] N × M,
//! gains en dB, normalisation, recopie au bit près des permutations). Ce
//! module en fait un réglage de Tune :
//!
//! - **par zone** : `zone_{id}_channel_remap`, un [`ChannelRemapSettings`] en
//!   JSON ;
//! - **par album**, en option (#5279) : `album_{id}_channel_remap`, même
//!   forme. Une règle d'album armée PRIME sur celle de la zone, parce qu'elle
//!   corrige un fichier (surrounds mal étiquetés), là où la zone décrit une
//!   installation ;
//! - **jamais en PURE** : le PCM y atteint la sortie intact, comme pour le
//!   repli mono (#2362).
//!
//! # Où elle s'applique
//!
//! Sur la SORTIE LOCALE, à l'adaptation source → périphérique
//! (`EtageDeConversion::convertir` et `conformer_la_piste_decodee`) : c'est le
//! seul endroit de la chaîne où le nombre de canaux change. Une règle ne
//! s'applique que si ses `inputs` égalent les canaux de la source ET ses
//! `outputs` ceux que le périphérique a ouverts ; sinon l'adaptation par
//! défaut (`audio/channels.rs`) reste, telle qu'avant.
//!
//! # Pourquoi (le défaut des fichiers 4.0)
//!
//! Tune ne lit pas la disposition déclarée par le fichier : il suppose l'ordre
//! par défaut FLAC/WAV d'après le seul nombre de canaux (FL FR BL BR en 4
//! canaux). Vers un ampli HDMI qui annonce 2/6/8 voies, un 4.0 ouvre 6 voies
//! et `adapt_channels_f32` recopie la trame en tête puis complète de silence :
//! **BL et BR partent sur FC et LFE**. Vers une sortie stéréo,
//! `build_downmix_matrix(4, 2)` n'a pas de cas 4 → 2 : **les voies arrière
//! sont perdues**. Les préréglages `quad_to_5_1` et `quad_to_stereo` les
//! remettent à leur place.
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

pub use tune_plugin_channel_remap::{
    CANAUX_MAX, ChannelRemapSettings, ErreurDeMatrice, GAIN_MAX_DB, GAIN_MIN_DB, Matrice,
    PREREGLAGES, identite, noms_des_canaux, prereglage,
};

/// L'identifiant du greffon au catalogue.
pub const ID_GREFFON: &str = "channel-remap";

/// La clé du réglage d'une zone.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_channel_remap")
}

/// La clé de la règle d'un album (#5279).
pub fn cle_d_album(album_id: i64) -> String {
    format!("album_{album_id}_channel_remap")
}

/// Lire un réglage enregistré ; `None` s'il est absent ou illisible.
pub fn lire(settings: &SettingsRepo, cle: &str) -> Option<ChannelRemapSettings> {
    settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str(&brut).ok())
}

/// D'où vient la règle appliquée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origine {
    Album,
    Zone,
}

impl Origine {
    pub fn code(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Zone => "zone",
        }
    }
}

/// La règle qui s'applique à une piste, et d'où elle vient.
#[derive(Debug, Clone)]
pub struct RegleEffective {
    pub reglage: ChannelRemapSettings,
    pub matrice: Arc<Matrice>,
    pub origine: Origine,
}

impl RegleEffective {
    /// « 4 → 6 voies », pour le chemin du signal.
    pub fn description(&self) -> String {
        let prereglage = self
            .reglage
            .preset
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        format!(
            "Matrice {} → {} voies{prereglage}, règle {}{}",
            self.reglage.inputs,
            self.reglage.outputs,
            match self.origine {
                Origine::Album => "de l'album",
                Origine::Zone => "de la zone",
            },
            if self.matrice.est_recopie() {
                " — recopie des canaux, sans calcul"
            } else {
                " — mélange des canaux"
            }
        )
    }
}

fn armee_pour(
    reglage: Option<ChannelRemapSettings>,
    canaux_source: Option<u16>,
    origine: Origine,
) -> Option<RegleEffective> {
    let reglage = reglage?;
    let matrice = Matrice::du_reglage_arme(&reglage)?;
    if canaux_source.is_some_and(|n| n != matrice.entrees()) || matrice.est_identite() {
        return None;
    }
    Some(RegleEffective {
        reglage,
        matrice: Arc::new(matrice),
        origine,
    })
}

/// La décision, pure : la règle d'album armée qui correspond à la source prime ;
/// sinon celle de la zone ; sinon rien. Une matrice identité ne compte pas —
/// elle ne change rien, et le chemin du signal n'a pas à l'annoncer.
pub fn resoudre(
    zone: Option<ChannelRemapSettings>,
    album: Option<ChannelRemapSettings>,
    canaux_source: Option<u16>,
) -> Option<RegleEffective> {
    armee_pour(album, canaux_source, Origine::Album)
        .or_else(|| armee_pour(zone, canaux_source, Origine::Zone))
}

/// La règle d'une zone pour une piste, lue en base ; `None` en PURE.
pub fn regle_effective_with(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    album_id: Option<i64>,
    canaux_source: Option<u16>,
) -> Option<RegleEffective> {
    if crate::audio::audiophile::zone_enabled(db, zone_id) {
        return None;
    }
    let settings = SettingsRepo::with_backend(db.clone());
    // Greffon facultatif et gratuit : sans installation, rien ne s'applique.
    if !crate::audio::premium_plugins::enabled(&settings, ID_GREFFON) {
        return None;
    }
    resoudre(
        lire(&settings, &cle_de_zone(zone_id)),
        album_id.and_then(|a| lire(&settings, &cle_d_album(a))),
        canaux_source,
    )
}

/// L'album et les canaux d'une piste, lus en base.
pub fn album_et_canaux_de_la_piste(
    db: &Arc<dyn DbBackend>,
    track_id: i64,
) -> (Option<i64>, Option<u16>) {
    crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .get(track_id)
        .ok()
        .flatten()
        .map(|t| {
            (
                t.album_id,
                u16::try_from(t.channels).ok().filter(|c| *c > 0),
            )
        })
        .unwrap_or((None, None))
}

/// Adapter un tampon `f32` entrelacé de `source` vers `sortie` canaux PAR LA
/// MATRICE, si elle correspond aux deux ; `None` sinon (l'appelant garde
/// l'adaptation par défaut).
pub fn adapter_f32(
    samples: &[f32],
    source: u16,
    sortie: u16,
    matrice: Option<&Matrice>,
) -> Option<Vec<f32>> {
    let m = matrice?;
    (m.entrees() == source && m.sorties() == sortie && !m.est_identite())
        .then(|| m.appliquer_f32(samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad() -> Vec<f32> {
        // FL FR BL BR, chaque voie reconnaissable.
        (0..16)
            .flat_map(|t| {
                let t = t as f32 / 100.0;
                [0.1 + t, 0.2 + t, 0.3 + t, 0.4 + t]
            })
            .collect()
    }

    fn canal(v: &[f32], n: usize, c: usize) -> Vec<f32> {
        v.iter().skip(c).step_by(n).copied().collect()
    }

    /// Le défaut que le greffon corrige, mesuré sur l'adaptation par défaut :
    /// un 4.0 vers 6 voies met BL/BR sur FC/LFE, vers 2 voies les perd.
    #[test]
    fn adaptation_par_defaut_d_un_4_0_deplace_ou_perd_l_arriere() {
        let entree = quad();
        let six = crate::audio::channels::adapt_channels_f32(&entree, 4, 6).unwrap();
        assert_eq!(canal(&six, 6, 2), canal(&entree, 4, 2), "BL tombe sur FC");
        assert_eq!(canal(&six, 6, 3), canal(&entree, 4, 3), "BR tombe sur LFE");
        assert!(
            canal(&six, 6, 4).iter().all(|x| *x == 0.0),
            "BL du 5.1 vide"
        );
        let deux = crate::audio::channels::adapt_channels_f32(&entree, 4, 2).unwrap();
        assert_eq!(deux, canal_paire(&entree), "4 → 2 ne garde que FL/FR");
    }

    fn canal_paire(v: &[f32]) -> Vec<f32> {
        v.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|t| [t[0], t[1]])
            .collect()
    }

    #[test]
    fn quad_vers_5_1_par_la_matrice_remet_l_arriere_a_sa_place() {
        let entree = quad();
        let m = Matrice::du_reglage_arme(&prereglage("quad_to_5_1").unwrap()).unwrap();
        let six = adapter_f32(&entree, 4, 6, Some(&m)).expect("la matrice correspond");
        assert_eq!(canal(&six, 6, 4), canal(&entree, 4, 2), "BL sur BL");
        assert_eq!(canal(&six, 6, 5), canal(&entree, 4, 3), "BR sur BR");
        assert!(canal(&six, 6, 2).iter().all(|x| *x == 0.0), "FC muet");
        assert!(canal(&six, 6, 3).iter().all(|x| *x == 0.0), "LFE muet");
        // Ouvert en 8 voies : la matrice 4 → 6 ne s'applique pas.
        assert!(adapter_f32(&entree, 4, 8, Some(&m)).is_none());
    }

    #[test]
    fn la_regle_d_album_prime_et_seulement_si_elle_correspond_a_la_source() {
        let zone = prereglage("quad_to_stereo");
        let album = prereglage("quad_to_5_1");
        let r = resoudre(zone.clone(), album.clone(), Some(4)).unwrap();
        assert_eq!(r.origine, Origine::Album);
        assert_eq!(r.matrice.sorties(), 6);
        // Un album 5.1 : ni la règle 4.0 de l'album ni celle de la zone.
        assert!(resoudre(zone.clone(), album.clone(), Some(6)).is_none());
        // Album éteint : la zone reprend la main.
        let mut eteint = album.unwrap();
        eteint.enabled = false;
        let r = resoudre(zone, Some(eteint), Some(4)).unwrap();
        assert_eq!(r.origine, Origine::Zone);
        assert_eq!(r.matrice.sorties(), 2);
        // L'identité n'est jamais une règle.
        assert!(resoudre(Some(identite(2)), None, Some(2)).is_none());
    }
}
