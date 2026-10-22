//! Les réglages persistants de la sortie Devialet AIR, et les deux politiques
//! qui ne dépendent d'aucun appareil : la cadence envoyée et le volume.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// « Devialet AIR (expérimental) » : désactivé par défaut (décision de
/// Bertrand, 19/10). Le protocole est reconstitué par rétro-ingénierie, il
/// n'est ni documenté ni approuvé par Devialet, et personne chez nous n'a pu
/// l'essayer sur un ampli.
pub const CLE_ACTIVEE: &str = "devialet_air_experimental";

/// L'indice de la source « AIR » de l'ampli. Roon choisit la source 9 sur
/// l'ampli qui a servi aux captures ; on ne sait pas comment il la détermine
/// (question ouverte Q4) : elle est donc réglable.
pub const CLE_INDICE_SOURCE: &str = "devialet_air_source_index";

/// La source AIR quand rien n'est réglé.
pub const INDICE_SOURCE_PAR_DEFAUT: u8 = 9;

/// Plus grand indice de source qu'une annonce décrit (15 sources, 0 à 14).
pub const INDICE_SOURCE_MAX: u8 = 14;

/// Préfixe du refus « option non activée », reconnaissable par l'appelant.
pub const SENTINELLE_NON_ACTIVEE: &str = "devialet_air_disabled:";

pub const MOTIF_NON_ACTIVEE: &str =
    "Sortie Devialet AIR non activée (option expérimentale dans les Réglages)";

/// Le plafond de volume, en dB. Le volume maximal que l'ampli accepte n'a
/// pas été mesuré (question ouverte Q5) ; −10 dB est le plafond de devimote.
/// Le dépasser demande un essai sur l'ampli, avec l'accord de son
/// propriétaire : ce n'est pas un réglage.
pub const PLAFOND_DB: f64 = -10.0;

/// Le bas de la course du curseur, en dB. Le curseur à zéro envoie ce
/// niveau ; c'est la sourdine, pas le volume, qui coupe le son.
pub const PLANCHER_DB: f64 = -80.0;

/// L'option est-elle activée ? Absente ou illisible : non.
pub fn activee(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(CLE_ACTIVEE)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// L'indice de source réglé, ou 9. Une valeur illisible ou hors bornes
/// retombe sur le défaut.
pub fn indice_source(db: &Arc<dyn DbBackend>) -> u8 {
    SettingsRepo::with_backend(db.clone())
        .get(CLE_INDICE_SOURCE)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .filter(|&i| i <= INDICE_SOURCE_MAX)
        .unwrap_or(INDICE_SOURCE_PAR_DEFAUT)
}

/// La cadence envoyée à l'ampli pour une source à `source` Hz.
///
/// - 44,1, 48, 88,2 et 96 kHz partent telles quelles : le codec AIR est sans
///   perte jusque-là.
/// - Au-delà (176,4, 192 kHz, DSD converti), rééchantillonnage à 96 kHz
///   (décision du 19/10) : AIR ne quantifie plus les résidus qu'avec perte
///   au-dessus de 96 kHz, environ 15 bits utiles. 96 kHz sans perte vaut
///   mieux que 192 kHz dégradé, et le chemin du signal affiche la conversion.
/// - En dessous de 44,1 kHz ou sur une cadence que AIR ne connaît pas :
///   44,1 kHz pour la famille 11,025 kHz, 48 kHz sinon.
#[must_use]
pub fn cadence_envoyee(source: u32) -> u32 {
    match source {
        44_100 | 48_000 | 88_200 | 96_000 => source,
        s if s > 96_000 => 96_000,
        s if s % 11_025 == 0 => 44_100,
        _ => 48_000,
    }
}

/// Curseur `0..=1` → consigne en dB, sur la grille de 0,5 dB de l'ampli,
/// jamais au-dessus du plafond.
#[must_use]
pub fn volume_vers_db(volume: f64) -> f64 {
    let v = if volume.is_nan() {
        0.0
    } else {
        volume.clamp(0.0, 1.0)
    };
    let db = PLANCHER_DB + v * (PLAFOND_DB - PLANCHER_DB);
    ((db * 2.0).round() / 2.0).clamp(PLANCHER_DB, PLAFOND_DB)
}

/// dB lus dans l'annonce → curseur `0..=1`. Un réglage fait à la
/// télécommande au-dessus du plafond s'affiche en haut de la course.
#[must_use]
pub fn db_vers_volume(db: f64) -> f64 {
    ((db - PLANCHER_DB) / (PLAFOND_DB - PLANCHER_DB)).clamp(0.0, 1.0)
}
