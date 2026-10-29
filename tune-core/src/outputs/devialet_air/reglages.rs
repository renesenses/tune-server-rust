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

/// #6918 — borne basse réglée de la course du curseur, en dB.
pub const CLE_VOLUME_MIN_DB: &str = "devialet_air_volume_min_db";

/// #6918 — borne haute réglée de la course du curseur, en dB. Jamais
/// au-dessus de [`PLAFOND_DB`] : la borne se règle SOUS le plafond, elle ne
/// le déplace pas.
pub const CLE_VOLUME_MAX_DB: &str = "devialet_air_volume_max_db";

/// Le pas de l'ampli, en dB.
pub const PAS_DB: f64 = 0.5;

/// #6918 — écart minimal entre les deux bornes : en dessous, le curseur
/// n'aurait plus de course utile.
pub const ECART_MINIMAL_DB: f64 = 5.0;

/// #6918 — la course du curseur en dB : `0` envoie `min_db`, `1` envoie
/// `max_db`. Les deux bornes sont sur la grille de 0,5 dB de l'ampli et
/// tiennent dans `[PLANCHER_DB ; PLAFOND_DB]` : une course ne se construit
/// que par [`BornesDb::nouvelles`], qui le garantit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BornesDb {
    min_db: f64,
    max_db: f64,
}

impl BornesDb {
    /// La course d'avant #6918 : −80 à −10 dB.
    pub const PAR_DEFAUT: Self = Self {
        min_db: PLANCHER_DB,
        max_db: PLAFOND_DB,
    };

    /// Une course réglée. Les bornes sont ramenées sur la grille de 0,5 dB
    /// vers l'INTÉRIEUR (le haut vers le bas, le bas vers le haut). Refusée
    /// si une borne n'est pas un nombre, sort de `[−80 ; −10]` dB ou si
    /// l'écart est inférieur à [`ECART_MINIMAL_DB`]. Une borne haute
    /// au-dessus du plafond est REFUSÉE, pas ramenée : la ramener ferait
    /// croire qu'elle a été prise.
    pub fn nouvelles(min_db: f64, max_db: f64) -> Result<Self, String> {
        if !min_db.is_finite() || !max_db.is_finite() {
            return Err("les bornes de volume doivent être des nombres".into());
        }
        if max_db > PLAFOND_DB {
            return Err(format!(
                "la borne haute ({max_db} dB) dépasse le plafond de {PLAFOND_DB} dB, qui n'est pas un réglage"
            ));
        }
        if min_db < PLANCHER_DB {
            return Err(format!(
                "la borne basse ({min_db} dB) est sous le plancher de {PLANCHER_DB} dB"
            ));
        }
        let min_db = (min_db / PAS_DB).ceil() * PAS_DB;
        let max_db = (max_db / PAS_DB).floor() * PAS_DB;
        if max_db - min_db < ECART_MINIMAL_DB {
            return Err(format!(
                "les bornes doivent être écartées d'au moins {ECART_MINIMAL_DB} dB (borne basse {min_db} dB, borne haute {max_db} dB)"
            ));
        }
        Ok(Self { min_db, max_db })
    }

    #[must_use]
    pub fn min_db(&self) -> f64 {
        self.min_db
    }

    #[must_use]
    pub fn max_db(&self) -> f64 {
        self.max_db
    }

    /// Curseur `0..=1` → consigne en dB, sur la grille de 0,5 dB, dans la
    /// course, et JAMAIS au-dessus de [`PLAFOND_DB`] : la dernière borne est
    /// le plafond lui-même, quelle que soit la course.
    #[must_use]
    pub fn volume_vers_db(&self, volume: f64) -> f64 {
        let v = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        let db = self.min_db + v * (self.max_db - self.min_db);
        ((db / PAS_DB).round() * PAS_DB)
            .clamp(self.min_db, self.max_db)
            .min(PLAFOND_DB)
    }

    /// dB → curseur `0..=1`. Un niveau au-dessus de la course (réglé à la
    /// télécommande) s'affiche en haut, un niveau au-dessous en bas.
    #[must_use]
    pub fn db_vers_volume(&self, db: f64) -> f64 {
        if db.is_nan() {
            return 0.0;
        }
        ((db - self.min_db) / (self.max_db - self.min_db)).clamp(0.0, 1.0)
    }

    /// La position `volume` en dB, SANS arrondi à la grille ni plafond :
    /// la lecture exacte qui sert à garder un niveau quand la course change.
    #[must_use]
    pub fn db_exacts(&self, volume: f64) -> f64 {
        self.min_db + volume.clamp(0.0, 1.0) * (self.max_db - self.min_db)
    }

    /// dB DEMANDÉS par un client → curseur `0..=1`. Contrairement à
    /// [`Self::db_vers_volume`], rien n'est ramené en silence : une demande
    /// au-dessus de la borne haute (donc à plus forte raison au-dessus du
    /// plafond) ou sous la borne basse est refusée en nommant la course.
    pub fn demande_vers_volume(&self, db: f64) -> Result<f64, String> {
        if !db.is_finite() {
            return Err("device_volume_db n'est pas un nombre".into());
        }
        if db > self.max_db || db < self.min_db {
            return Err(format!(
                "{db} dB est hors de la course réglée de cette sortie ({} à {} dB ; plafond {PLAFOND_DB} dB)",
                self.min_db, self.max_db
            ));
        }
        Ok(self.db_vers_volume(db))
    }

    /// La course enregistrée. Absente : la course par défaut. Illisible ou
    /// hors bornes : la course par défaut aussi, dont le haut reste le
    /// plafond — une valeur abîmée en base ne peut rien faire monter
    /// au-dessus de −10 dB.
    pub fn lues(db: &Arc<dyn DbBackend>) -> Self {
        let r = SettingsRepo::with_backend(db.clone());
        let lire = |cle: &str| {
            r.get(cle)
                .ok()
                .flatten()
                .and_then(|v| v.trim().parse::<f64>().ok())
        };
        let min_db = lire(CLE_VOLUME_MIN_DB).unwrap_or(PLANCHER_DB);
        let max_db = lire(CLE_VOLUME_MAX_DB).unwrap_or(PLAFOND_DB);
        Self::nouvelles(min_db, max_db).unwrap_or(Self::PAR_DEFAUT)
    }

    /// Enregistre la course.
    pub fn enregistrer(&self, db: &Arc<dyn DbBackend>) -> Result<(), String> {
        let r = SettingsRepo::with_backend(db.clone());
        r.set(CLE_VOLUME_MIN_DB, &self.min_db.to_string())?;
        r.set(CLE_VOLUME_MAX_DB, &self.max_db.to_string())
    }

    /// #6918 — la course publiée dans une charge utile.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "min_db": self.min_db,
            "max_db": self.max_db,
            "step_db": PAS_DB,
            "floor_db": PLANCHER_DB,
            "ceiling_db": PLAFOND_DB,
        })
    }
}

/// Curseur `0..=1` → consigne en dB sur la course par défaut (−80 à
/// −10 dB), sur la grille de 0,5 dB de l'ampli, jamais au-dessus du plafond.
#[must_use]
pub fn volume_vers_db(volume: f64) -> f64 {
    BornesDb::PAR_DEFAUT.volume_vers_db(volume)
}

/// dB lus dans l'annonce → curseur `0..=1`, sur la course par défaut. Un
/// réglage fait à la télécommande au-dessus du plafond s'affiche en haut de
/// la course.
#[must_use]
pub fn db_vers_volume(db: f64) -> f64 {
    BornesDb::PAR_DEFAUT.db_vers_volume(db)
}
