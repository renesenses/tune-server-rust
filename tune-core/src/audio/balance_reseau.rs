//! #8740 — la balance gauche/droite sur les flux RÉSEAU (DLNA, AirPlay,
//! Chromecast, OAAT…).
//!
//! ## Le défaut
//!
//! La balance se règle dans le greffon « Réaffectation des canaux » : une
//! matrice 2 → 2 qui baisse une voie (`[[0, null], [null, -2]]`). Seule la
//! sortie LOCALE posait cette matrice ; sur une zone réseau (DrUPnP,
//! slim2diretta), le réglage était enregistré et ne s'entendait jamais.
//!
//! Décision de Bertrand (17/11) : la balance s'applique aussi aux flux réseau,
//! **hors PURE et bit-perfect strict**, et seulement quand Tune traite le PCM.
//!
//! ## Ce que ce module décide
//!
//! - [`regle_reseau_with`] : la règle de ZONE que le traitement réseau
//!   applique, ou pourquoi il ne l'applique pas ([`RefusReseau`]). Elle doit
//!   partir de 2 canaux, en produire 2, et ne JAMAIS monter le niveau
//!   ([`sans_hausse`]) : la sécurité du volume prime, un flux réseau n'a pas de
//!   volume logiciel pour rattraper un gain.
//! - [`appliquer_pcm_stereo`] : l'étage lui-même, sur du PCM entier
//!   entrelacé (16, 24 ou 32 bits, petit-boutiste), en place. Une recopie
//!   (échange gauche/droite) reste au bit près ; un gain est arrondi au plus
//!   proche et borné à la pleine échelle.
//! - [`reglage_de_balance`] / [`balance_du_reglage`] : la balance −100…+100
//!   écrite comme une matrice 2 → 2, et relue depuis elle.
//!
//! ## La loi : atténuer le seul côté opposé
//!
//! Deux lois sont d'usage. La loi à puissance constante (sinus/cosinus) est
//! celle du PANORAMIQUE d'une console : elle déplace une source mono et, pour
//! garder la puissance, pose les deux voies à −3 dB au centre ou monte le côté
//! favorisé de +3 dB. Aucune des deux ne convient ici : la première change le
//! niveau d'une zone dont la balance est au centre (0 doit rester l'identité,
//! au bit près), la seconde monte le niveau (interdit).
//!
//! La balance d'un amplificateur hi-fi, d'un système d'exploitation ou d'un
//! lecteur logiciel laisse le côté favorisé à 0 dB et n'atténue QUE le côté
//! opposé. C'est la loi retenue, en amplitude linéaire : le côté opposé reçoit
//! `1 − |b| / 100` (−6 dB à ±50, −20 dB à ±90, muet à ±100). Au centre, rien
//! ne change ; nulle part le niveau ne monte.
use std::sync::Arc;

use crate::audio::reaffectation_canaux as rc;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Le préréglage informatif d'une matrice écrite par [`reglage_de_balance`].
pub const PREREGLAGE_BALANCE: &str = "balance";

/// Bornes de la balance : −100 (tout à gauche) à +100 (tout à droite).
pub const BALANCE_MAX: i32 = 100;

/// Le gain LINÉAIRE du côté opposé pour une balance `b` (déjà bornée).
fn gain_oppose(b: i32) -> f64 {
    1.0 - f64::from(b.unsigned_abs().min(BALANCE_MAX as u32)) / 100.0
}

/// Le gain en dB d'une voie atténuée, `None` (muet) à ±100.
fn db_oppose(b: i32) -> Option<f32> {
    let g = gain_oppose(b);
    (g > 0.0).then(|| (20.0 * g.log10()) as f32)
}

/// La matrice 2 → 2 d'une balance `b` (bornée à −100…+100) : diagonale, le
/// côté favorisé à 0 dB, le côté opposé atténué. `b = 0` rend l'identité,
/// éteinte : rien à appliquer.
pub fn reglage_de_balance(b: i32) -> rc::ChannelRemapSettings {
    let b = b.clamp(-BALANCE_MAX, BALANCE_MAX);
    let (gauche, droite) = match b {
        0 => (Some(0.0), Some(0.0)),
        b if b > 0 => (db_oppose(b), Some(0.0)),
        b => (Some(0.0), db_oppose(b)),
    };
    rc::ChannelRemapSettings {
        enabled: b != 0,
        inputs: 2,
        outputs: 2,
        gains_db: vec![vec![gauche, None], vec![None, droite]],
        normalize: true,
        preset: Some(PREREGLAGE_BALANCE.to_string()),
    }
}

/// La balance qu'une matrice 2 → 2 exprime, si elle n'est QU'une balance
/// (diagonale, un côté à 0 dB, l'autre atténué ou muet). `None` pour toute
/// autre matrice (échange, mono, mélange, gain positif) : l'écran garde alors
/// la grille. Une règle éteinte vaut 0.
pub fn balance_du_reglage(s: &rc::ChannelRemapSettings) -> Option<i32> {
    if s.inputs != 2 || s.outputs != 2 || s.gains_db.len() != 2 {
        return None;
    }
    if !s.enabled {
        return Some(0);
    }
    let ligne = |o: usize| s.gains_db.get(o).filter(|l| l.len() == 2);
    let (g, d) = (ligne(0)?, ligne(1)?);
    if g[1].is_some() || d[0].is_some() {
        return None;
    }
    let vers_b = |db: Option<f32>| -> Option<i32> {
        match db {
            None => Some(BALANCE_MAX),
            Some(db) if db <= 0.0 => {
                let g = 10f64.powf(f64::from(db) / 20.0);
                Some(((1.0 - g) * 100.0).round() as i32)
            }
            Some(_) => None,
        }
    };
    match (g[0], d[1]) {
        (Some(0.0), Some(0.0)) => Some(0),
        (gauche, Some(0.0)) => vers_b(gauche),
        (Some(0.0), droite) => vers_b(droite).map(|b| -b),
        _ => None,
    }
}

/// La matrice ne monte-t-elle le niveau d'AUCUNE sortie ? Chaque coefficient
/// effectif (après normalisation) est positif ou nul et chaque ligne somme à
/// 1 au plus : `|Σ cᵢ·xᵢ| ≤ max |xᵢ|`, aucune crête ne peut grandir.
pub fn sans_hausse(m: &rc::Matrice) -> bool {
    let (n, s) = (usize::from(m.entrees()), usize::from(m.sorties()));
    (0..s).all(|o| {
        let ligne = (0..n).map(|i| m.coefficient(o, i));
        ligne.clone().all(|c| c >= 0.0) && ligne.sum::<f64>() <= 1.0 + 1e-9
    })
}

/// Pourquoi le traitement réseau n'applique pas la règle de la zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusReseau {
    /// Mode PURE, ou mesure de pièce en cours : le PCM reste intact.
    Pure,
    /// Bit-perfect strict : rien ne touche le signal.
    BitPerfectStrict,
    /// Greffon non installé, ou aucune règle de zone armée (l'identité n'en
    /// est pas une).
    AucuneRegle,
    /// La règle ne fait pas 2 → 2 : le flux réseau est stéréo.
    PasStereo,
    /// La règle monterait le niveau d'une voie.
    HausseDeNiveau,
}

impl RefusReseau {
    /// Code stable, pour la machine.
    pub fn code(self) -> &'static str {
        match self {
            Self::Pure => "pure_mode",
            Self::BitPerfectStrict => "bit_perfect_strict",
            Self::AucuneRegle => "no_rule",
            Self::PasStereo => "network_stereo_only",
            Self::HausseDeNiveau => "network_gain_increase",
        }
    }
}

/// La règle de ZONE que le traitement réseau applique au PCM de cette zone.
///
/// Ne regarde ni la sortie ni le format : l'appelant ne la pose que sur un
/// flux réseau dont Tune traite le PCM. La règle d'album n'entre pas ici : la
/// balance décrit une installation, pas un fichier.
pub fn regle_reseau_with(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
) -> Result<rc::RegleEffective, RefusReseau> {
    if crate::audio::audiophile::traitements_contournes(db, zone_id) {
        return Err(RefusReseau::Pure);
    }
    if crate::audio::bitperfect_strict::zone_enabled(db, zone_id) {
        return Err(RefusReseau::BitPerfectStrict);
    }
    let settings = SettingsRepo::with_backend(db.clone());
    if !crate::audio::premium_plugins::enabled(&settings, rc::ID_GREFFON) {
        return Err(RefusReseau::AucuneRegle);
    }
    let reglage = rc::lire(&settings, &rc::cle_de_zone(zone_id));
    let regle = rc::resoudre(reglage.clone(), None, None).ok_or(RefusReseau::AucuneRegle)?;
    if regle.matrice.entrees() != 2 || regle.matrice.sorties() != 2 {
        return Err(RefusReseau::PasStereo);
    }
    if !sans_hausse(&regle.matrice) {
        return Err(RefusReseau::HausseDeNiveau);
    }
    Ok(regle)
}

/// L'étage : applique une matrice 2 → 2 à du PCM stéréo entrelacé, EN PLACE.
///
/// `bit_depth` 16, 24 ou 32 (entiers signés petit-boutistes, la forme que les
/// autres étages réseau traitent) ; toute autre profondeur, ou une matrice qui
/// n'est pas 2 → 2, laisse le tampon intact. Une trame incomplète en fin de
/// tampon n'est pas touchée.
pub fn appliquer_pcm_stereo(m: &rc::Matrice, pcm: &mut [u8], bit_depth: u16) {
    if m.entrees() != 2 || m.sorties() != 2 || m.est_identite() {
        return;
    }
    let octets = match bit_depth {
        16 => 2,
        24 => 3,
        32 => 4,
        _ => return,
    };
    let min = -(1i64 << (bit_depth - 1)) as f64;
    let max = ((1i64 << (bit_depth - 1)) - 1) as f64;
    let lire = |s: &[u8]| -> i32 {
        match octets {
            2 => i32::from(i16::from_le_bytes([s[0], s[1]])),
            3 => (i32::from(s[2]) << 24 | i32::from(s[1]) << 16 | i32::from(s[0]) << 8) >> 8,
            _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        }
    };
    let ecrire = |s: &mut [u8], v: i32| {
        let o = v.to_le_bytes();
        s.copy_from_slice(&o[..octets]);
    };
    for trame in pcm.chunks_exact_mut(octets * 2) {
        let entree = [lire(&trame[..octets]), lire(&trame[octets..])];
        let mut sortie = [0i32; 2];
        m.appliquer_trame(&entree, &mut sortie, 0, f64::from, |v| {
            v.round().clamp(min, max) as i32
        });
        let (g, d) = trame.split_at_mut(octets);
        ecrire(g, sortie[0]);
        ecrire(d, sortie[1]);
    }
}

/// La balance ENTRE dans la clé du cache de transcodage : elle change les
/// octets encodés, comme le crossfeed. Sans elle, une rendition sans balance
/// (pré-chauffage, ou d'avant ce correctif) serait servie à une zone qui en a
/// une, et inversement. `None` rend la clé d'avant, inchangée.
pub fn empreinte_avec_balance(dsp: Option<[u8; 32]>, m: Option<&rc::Matrice>) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some(m) = m else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"balance-reseau\0");
    for o in 0..usize::from(m.sorties()) {
        for i in 0..usize::from(m.entrees()) {
            h.update(m.coefficient(o, i).to_le_bytes());
        }
    }
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn matrice(b: i32) -> rc::Matrice {
        rc::Matrice::depuis_reglage(&reglage_de_balance(b)).expect("matrice valide")
    }

    /// PCM 16 bits stéréo, les deux voies à `niveau`, sur `trames` trames.
    fn pcm16(niveau: i16, trames: usize) -> Vec<u8> {
        (0..trames)
            .flat_map(|t| {
                let v = if t % 2 == 0 { niveau } else { -niveau };
                [v.to_le_bytes(), v.to_le_bytes()].concat()
            })
            .collect()
    }

    fn crete16(pcm: &[u8], voie: usize) -> i32 {
        pcm.as_chunks::<4>()
            .0
            .iter()
            .map(|t| i32::from(i16::from_le_bytes([t[voie * 2], t[voie * 2 + 1]])).abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn la_loi_n_attenue_que_le_cote_oppose() {
        let m = matrice(50);
        assert_eq!(m.coefficient(1, 1), 1.0, "droite favorisée : 0 dB");
        assert!((m.coefficient(0, 0) - 0.5).abs() < 1e-6, "gauche à −6 dB");
        let m = matrice(-90);
        assert_eq!(m.coefficient(0, 0), 1.0);
        assert!((m.coefficient(1, 1) - 0.1).abs() < 1e-6, "droite à −20 dB");
        assert_eq!(m.coefficient(0, 1), 0.0, "aucun mélange");
        assert_eq!(m.coefficient(1, 0), 0.0, "aucun mélange");
        // Au centre : l'identité, éteinte — rien ne s'applique.
        let centre = reglage_de_balance(0);
        assert!(!centre.enabled);
        assert!(rc::Matrice::depuis_reglage(&centre).unwrap().est_identite());
        // Hors bornes : ramené à ±100.
        assert_eq!(reglage_de_balance(250), reglage_de_balance(100));
    }

    #[test]
    fn la_balance_se_relit_depuis_sa_matrice() {
        for b in [-100, -73, -50, -1, 0, 1, 37, 50, 99, 100] {
            assert_eq!(
                balance_du_reglage(&reglage_de_balance(b)),
                Some(b),
                "b = {b}"
            );
        }
        // Le réglage du testeur (droite −2 dB) est une balance vers la gauche.
        let testeur = rc::ChannelRemapSettings {
            enabled: true,
            inputs: 2,
            outputs: 2,
            gains_db: vec![vec![Some(0.0), None], vec![None, Some(-2.0)]],
            normalize: true,
            preset: None,
        };
        assert_eq!(balance_du_reglage(&testeur), Some(-21));
        // Un échange ou un mono n'est pas une balance.
        assert_eq!(
            balance_du_reglage(&rc::prereglage("swap_lr").unwrap()),
            None
        );
        assert_eq!(balance_du_reglage(&rc::prereglage("mono").unwrap()), None);
    }

    /// Épreuve : canaux mesurés à −100, 0 et +100 ; contre-épreuve : le
    /// centre laisse le tampon identique à l'octet.
    #[test]
    fn canaux_mesures_a_moins_100_zero_et_plus_100() {
        let source = pcm16(20_000, 64);
        let mut gauche_seule = source.clone();
        appliquer_pcm_stereo(&matrice(-100), &mut gauche_seule, 16);
        assert_eq!(crete16(&gauche_seule, 0), 20_000, "−100 : gauche intacte");
        assert_eq!(crete16(&gauche_seule, 1), 0, "−100 : droite muette");

        let mut droite_seule = source.clone();
        appliquer_pcm_stereo(&matrice(100), &mut droite_seule, 16);
        assert_eq!(crete16(&droite_seule, 0), 0, "+100 : gauche muette");
        assert_eq!(crete16(&droite_seule, 1), 20_000, "+100 : droite intacte");

        let mut centre = source.clone();
        appliquer_pcm_stereo(&matrice(0), &mut centre, 16);
        assert_eq!(centre, source, "0 : aucun octet touché");

        let mut moitie = source.clone();
        appliquer_pcm_stereo(&matrice(50), &mut moitie, 16);
        assert_eq!(crete16(&moitie, 0), 10_000, "+50 : gauche à −6 dB");
        assert_eq!(crete16(&moitie, 1), 20_000);
    }

    #[test]
    fn l_etage_traite_le_24_et_le_32_bits() {
        let v24: i32 = -6_000_000;
        let mut pcm: Vec<u8> = (0..8)
            .flat_map(|_| {
                let o = v24.to_le_bytes();
                [o[0], o[1], o[2], o[0], o[1], o[2]]
            })
            .collect();
        appliquer_pcm_stereo(&matrice(50), &mut pcm, 24);
        let lire =
            |s: &[u8]| (i32::from(s[2]) << 24 | i32::from(s[1]) << 16 | i32::from(s[0]) << 8) >> 8;
        assert_eq!(
            lire(&pcm[0..3]),
            -3_000_000,
            "24 bits : gauche à −6 dB, signe gardé"
        );
        assert_eq!(lire(&pcm[3..6]), v24);
        let mut pcm32: Vec<u8> = [i32::MIN.to_le_bytes(), i32::MAX.to_le_bytes()].concat();
        appliquer_pcm_stereo(&matrice(-100), &mut pcm32, 32);
        assert_eq!(
            &pcm32[0..4],
            &i32::MIN.to_le_bytes(),
            "32 bits : gauche intacte"
        );
        assert_eq!(&pcm32[4..8], &0i32.to_le_bytes(), "32 bits : droite muette");
        // Profondeur exotique : intact.
        let mut huit = vec![1u8, 2, 3, 4];
        appliquer_pcm_stereo(&matrice(100), &mut huit, 8);
        assert_eq!(huit, vec![1, 2, 3, 4]);
    }

    /// Aucune hausse de niveau, quelle que soit la balance : aucune voie ne
    /// dépasse sa crête d'entrée, même à pleine échelle.
    #[test]
    fn aucune_balance_ne_monte_le_niveau() {
        let source = pcm16(i16::MAX, 32);
        for b in -100..=100 {
            let m = matrice(b);
            assert!(sans_hausse(&m), "b = {b}");
            let mut pcm = source.clone();
            appliquer_pcm_stereo(&m, &mut pcm, 16);
            assert!(crete16(&pcm, 0) <= crete16(&source, 0), "b = {b}");
            assert!(crete16(&pcm, 1) <= crete16(&source, 1), "b = {b}");
        }
        // Contre-épreuve : une matrice à +6 dB est refusée par la garde.
        let hausse = rc::ChannelRemapSettings {
            enabled: true,
            inputs: 2,
            outputs: 2,
            gains_db: vec![vec![Some(6.0), None], vec![None, Some(0.0)]],
            normalize: false,
            preset: None,
        };
        assert!(!sans_hausse(&rc::Matrice::depuis_reglage(&hausse).unwrap()));
        // Une recopie (échange) et un mono normalisé n'en montent aucun.
        for p in ["swap_lr", "mono"] {
            let m = rc::Matrice::du_reglage_arme(&rc::prereglage(p).unwrap()).unwrap();
            assert!(sans_hausse(&m), "{p}");
        }
    }

    #[test]
    fn un_echange_reste_au_bit_pres() {
        let m = rc::Matrice::du_reglage_arme(&rc::prereglage("swap_lr").unwrap()).unwrap();
        let mut pcm: Vec<u8> = [1i16, -2, 3, -4]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        appliquer_pcm_stereo(&m, &mut pcm, 16);
        let attendu: Vec<u8> = [-2i16, 1, -4, 3]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(pcm, attendu);
    }

    #[test]
    fn la_balance_entre_dans_la_cle_du_cache() {
        assert_eq!(empreinte_avec_balance(None, None), None);
        let base = Some([7u8; 32]);
        assert_eq!(empreinte_avec_balance(base, None), base);
        let a = empreinte_avec_balance(base, Some(&matrice(30)));
        let b = empreinte_avec_balance(base, Some(&matrice(-30)));
        assert!(a.is_some() && a != base && a != b);
        assert_eq!(a, empreinte_avec_balance(base, Some(&matrice(30))));
    }

    fn mem_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base mémoire");
        db.init_schema().expect("schéma");
        migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    fn poser(db: &Arc<dyn DbBackend>, cle: &str, valeur: &str) {
        SettingsRepo::with_backend(db.clone())
            .set(cle, valeur)
            .expect("écrire le réglage");
    }

    /// Épreuve : la balance de la zone est retenue pour le flux réseau.
    /// Contre-épreuves : PURE, bit-perfect strict, greffon absent, matrice
    /// multicanale ou qui monte le niveau — aucune règle.
    #[test]
    fn la_regle_reseau_respecte_pure_bit_perfect_et_la_securite_du_volume() {
        let db = mem_db();
        let zone = 7;
        let cle = rc::cle_de_zone(zone);
        poser(
            &db,
            &cle,
            &serde_json::to_string(&reglage_de_balance(40)).unwrap(),
        );
        assert_eq!(
            regle_reseau_with(&db, zone).err(),
            Some(RefusReseau::AucuneRegle),
            "greffon non installé"
        );
        poser(&db, "plugin_channel-remap_installed", "true");
        let regle = regle_reseau_with(&db, zone).expect("balance retenue");
        assert!((regle.matrice.coefficient(0, 0) - 0.6).abs() < 1e-6);

        poser(
            &db,
            &format!("zone_{zone}_audiophile"),
            r#"{"enabled":true}"#,
        );
        assert_eq!(regle_reseau_with(&db, zone).err(), Some(RefusReseau::Pure));
        poser(
            &db,
            &format!("zone_{zone}_audiophile"),
            r#"{"enabled":false}"#,
        );

        poser(
            &db,
            &crate::audio::bitperfect_strict::cle_de_zone(zone),
            "true",
        );
        assert_eq!(
            regle_reseau_with(&db, zone).err(),
            Some(RefusReseau::BitPerfectStrict)
        );
        poser(
            &db,
            &crate::audio::bitperfect_strict::cle_de_zone(zone),
            "false",
        );
        assert!(regle_reseau_with(&db, zone).is_ok());

        let quad = rc::prereglage("quad_to_stereo").unwrap();
        poser(&db, &cle, &serde_json::to_string(&quad).unwrap());
        assert_eq!(
            regle_reseau_with(&db, zone).err(),
            Some(RefusReseau::PasStereo)
        );

        let hausse = rc::ChannelRemapSettings {
            enabled: true,
            inputs: 2,
            outputs: 2,
            gains_db: vec![vec![Some(3.0), None], vec![None, Some(0.0)]],
            // Sans normalisation : armée, elle ramènerait la ligne à 1
            // (l'identité), ce qui n'est plus une hausse.
            normalize: false,
            preset: None,
        };
        poser(&db, &cle, &serde_json::to_string(&hausse).unwrap());
        assert_eq!(
            regle_reseau_with(&db, zone).err(),
            Some(RefusReseau::HausseDeNiveau)
        );

        poser(
            &db,
            &cle,
            &serde_json::to_string(&reglage_de_balance(0)).unwrap(),
        );
        assert_eq!(
            regle_reseau_with(&db, zone).err(),
            Some(RefusReseau::AucuneRegle),
            "balance au centre : rien à appliquer"
        );
    }
}
