//! TESS T02 — la chaîne d'une zone : migration des anciennes clés et
//! **écriture croisée**.
//!
//! La chaîne est enregistrée sous `zone_{id}_tess_chain` ([`super::cle_chaine_zone`]).
//! Les anciennes clés restent la vérité des anciennes routes et des clients
//! mobiles ; ce module tient les deux côtés d'accord :
//!
//! - [`lire_zone`] rend la chaîne de la zone : la chaîne enregistrée, à
//!   laquelle on réapplique chaque ancienne clé **modifiée depuis la dernière
//!   synchronisation** (une ancienne route a écrit) ; sans chaîne enregistrée,
//!   la chaîne est entièrement tirée des anciennes clés ;
//! - [`ecrire_chaine`] enregistre une chaîne ET met à jour les anciennes clés
//!   qu'elle exprime ; une clé dont le sens ne change pas n'est pas réécrite
//!   (l'aller-retour ancienne route → TESS → ancienne route est identique à
//!   l'octet près) ;
//! - [`migrer_zone`] enregistre la chaîne lue, sans jamais toucher une
//!   ancienne clé. Deux migrations valent une.
//!
//! **La chaîne enregistrée ne dépend jamais du droit.** La conversion P2
//! (paramétrique → graphique sans Premium) n'est appliquée qu'à la lecture
//! ([`lire_zone`] avec `premium = false`) : un compte qui (re)devient Premium
//! retrouve son égaliseur paramétrique.
//!
//! « Modifiée depuis la dernière synchronisation » se lit dans l'**ombre**
//! (`zone_{id}_tess_chain_sync`, [`cle_ombre_zone`]) : la valeur brute de
//! chaque ancienne clé au moment où la chaîne a été écrite, construite à
//! partir de ce qui a été lu PUIS écrit, jamais relue (une ancienne route qui
//! écrit pendant une écriture TESS n'est donc jamais avalée). Deux valeurs de
//! même sens JSON comptent comme égales.
//!
//! **Verrou par zone** : [`ecrire_chaine`] et [`migrer_zone`] prennent
//! [`verrou_de_zone`] ; T03 (migration au démarrage) et T08 (`PUT
//! /zones/{id}/tess`) n'écrivent la chaîne que par ces deux fonctions. Les
//! anciennes routes ne prennent pas le verrou : l'ombre suffit pour elles.
//!
//! **Lecture tolérante comme aujourd'hui** : un booléen non canonique
//! (`"on"`, `"yes"`, `"enabled": "yes"`…) vaut ce que le code actuel en fait
//! (faux), sans refuser la zone ; une ombre illisible vaut une ombre absente.
//! Le refus explicite ([`ErreurMigration::CleCorrompue`]) est réservé à une clé
//! que son lecteur actuel ne sait pas lire : JSON invalide, ou réglage typé
//! illisible (`eq_profile`, `channel_remap`, `upsampler`), et à une chaîne
//! enregistrée illisible. Jamais une chaîne vide.
//!
//! # Correspondance
//!
//! | Ancienne clé | Chaîne |
//! |---|---|
//! | `zone_{id}_eq_profile` | une façade d'égaliseur (voir [`classer_egaliseur`] ; `room-correction` porte son origine, D1), `enabled` = `enabled` du profil, `settings` = le profil sans `enabled` (sa réserve `headroom_mode` comprise) |
//! | `ir_path_{id}` | `convolution`, coché, `settings.ir_path` |
//! | `zone_{id}_crossfeed` | `crossfeed`, `enabled` = `enabled`, `settings` = le reste |
//! | `zone_{id}_native_plugin_{p}` | greffon `{p}` (Crossfeed Pro, Crosstalk, paquets tiers), idem |
//! | `zone_{id}_mono_downmix` | `mono-downmix` (greffon dédié, D2), coché si `"true"` sur une sortie locale |
//! | `zone_{id}_channel_remap` | `channel-remap`, en dernier, idem |
//! | `zone_{id}_upsampler` | `fixed.upsampling` : `pcm` avec `filter`, `max_ratio`, `max_rate_hz` si `enabled`, sinon `off` |
//! | `zone_{id}_audiophile`, `zone_{id}_strict_bitperfect` | hors chaîne : [`LectureZone::pure`] (`ContexteExecution.pure`) |
//! | `zone_{id}_level_compensation` | hors chaîne : [`LectureZone::compensation_de_niveau`] (T07) |
//! | colonne `zones.dsd_mode` | hors chaîne : [`LectureZone::dsd_mode`], [`lecture_dsd`] (T16) |
//! | `zone_{id}_gain_trim_db` | hors chaîne : reste le gain de zone, porté par l'étage headroom (T07) |
//!
//! Groupe crossfeed (décision B) : un seul membre coché ; voir
//! [`MARQUE_REMPLACE`].
//!
//! Ordre d'une chaîne tirée des anciennes clés : celui de la lecture
//! d'aujourd'hui (égaliseur, convolution, crossfeed, étages tiers, repli mono,
//! réaffectation en dernier).
//!
//! # Décisions de Bertrand (18/11)
//!
//! - **D1, correction de pièce** : un égaliseur de pièce posé dans
//!   `eq_profile` devient `room-correction`, avec son origine dans ses
//!   réglages ([`CHAMP_ORIGINE`], jamais écrit dans `eq_profile`) :
//!   - MESURÉE au micro (C2, `"measured"`) : Premium pour tous. Pour un compte
//!     gratuit, elle est convertie en égaliseur graphique au plus près à la
//!     LECTURE, comme P2 ; jamais dans la chaîne enregistrée ni dans
//!     `eq_profile` ;
//!   - CALCULÉE SANS MICRO (C5, #7919, `"estimation"`) : Free, active sans
//!     Premium, jamais convertie ([`correction_estimee`]).
//!   - l'origine se lit dans `room_profile_{id}` ([`origine_de_la_piece`]).
//! - **D2, `mono-downmix`** : 11ᵉ greffon, Free. Sur une zone RÉSEAU, où
//!   l'interrupteur de repli mono était inerte (#3254, sortie locale
//!   seulement), il est DÉCOCHÉ d'office, avec `"inert_on_network": true`
//!   ([`MARQUE_MONO_INERTE`]) : aucun changement audible, et l'ancienne clé
//!   n'est pas touchée. Coché ensuite, il agit partout.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

use super::chaine::{ChaineTess, MaillonGreffon, Upsampling, VERSION_CHAINE};
use super::greffon::{
    Origine, PRIORITE_CROSSFEED_INTEGRE, PRIORITE_CROSSFEED_TIERS, PRIORITE_CROSSTALK,
    catalogue_de_reference, id_valide,
};
use super::resolution::LectureDsd;
use crate::audio::crossfeed::{ID_CROSSTALK, est_de_la_famille_du_crossfeed};
use crate::audio::eq::{EqBandSpec, EqProfile};
use crate::audio::surechantillonnage::UpsamplerSettings;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

pub const ID_EQ_GRAPHIQUE: &str = "equalizer-graphic";
pub const ID_EQ_PARAMETRIQUE: &str = "equalizer-parametric";
pub const ID_EQ_CASQUE: &str = "equalizer-headphone";
pub const ID_CORRECTION_PIECE: &str = "room-correction";
pub const ID_CONVOLUTION: &str = "convolution";
pub const ID_CROSSFEED: &str = "crossfeed";
pub const ID_MONO: &str = "mono-downmix";
pub const ID_REAFFECTATION: &str = "channel-remap";

/// D1 : réglage d'un `room-correction` qui dit d'où vient la correction :
/// [`ORIGINE_MESURE`] (micro, Premium) ou [`ORIGINE_ESTIMATION`] (calculée
/// sans micro, Free). N'appartient qu'à la chaîne, jamais à `eq_profile`.
pub const CHAMP_ORIGINE: &str = "origin";
pub const ORIGINE_MESURE: &str = crate::room_correction::ORIGINE_MESUREE;
pub const ORIGINE_ESTIMATION: &str = crate::room_correction::ORIGINE_ESTIMEE;
/// Le nom par défaut d'un profil de pièce calculé sans micro (C5,
/// `plugins/tune-calibration/src/routes.rs` : « Estimation {id} »).
pub const PREFIXE_NOM_ESTIMATION: &str = "Estimation ";
/// D2 : réglage qui marque un `mono-downmix` décoché parce que
/// l'interrupteur actuel est inerte sur une sortie réseau.
pub const MARQUE_MONO_INERTE: &str = "inert_on_network";
/// Groupe crossfeed (décision B de Bertrand) : réglage d'un membre DÉCOCHÉ
/// par la migration parce qu'un membre plus prioritaire était coché avec lui
/// ; sa valeur est l'id du gagnant. L'ancienne clé, elle, reste cochée.
pub const MARQUE_REMPLACE: &str = "replaced_in_group";

/// Les quatre façades portées par `zone_{id}_eq_profile`.
pub const FACADES_EQ: [&str; 4] = [
    ID_EQ_GRAPHIQUE,
    ID_EQ_PARAMETRIQUE,
    ID_EQ_CASQUE,
    ID_CORRECTION_PIECE,
];

/// Réglage global de la grille de l'égaliseur graphique (10, 15 ou 31).
pub const CLE_GRILLE_EQ: &str = "eq_expert_bands";

/// Les grilles ISO de l'égaliseur graphique et leur Q, celles du client web
/// (`EqualizerV2.svelte`, `GRIDS` et `GRID_Q`).
pub const GRILLE_15: [f64; 15] = [
    25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0, 4000.0, 6300.0,
    10000.0, 16000.0,
];
pub const GRILLE_31: [f64; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0,
    8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
];

/// La grille à `n` bandes et son Q ; 10 pour toute autre valeur.
pub fn grille_graphique(n: u32) -> (&'static [f64], f64) {
    match n {
        15 => (&GRILLE_15, 2.15),
        31 => (&GRILLE_31, 4.32),
        _ => (&crate::audio::eq_presets::GRILLE_10, 1.0),
    }
}

/// L'ombre de la zone : la valeur brute des anciennes clés à la dernière
/// écriture de la chaîne.
pub fn cle_ombre_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_tess_chain_sync")
}

/// Le profil d'égaliseur tel qu'il était avant que TESS le réécrive pour la
/// première fois (rien ne se perd, même un profil paramétrique converti, P2).
pub fn cle_sauvegarde_eq(zone_id: i64) -> String {
    format!("zone_{zone_id}_eq_profile_before_tess")
}

/// Le verrou d'écriture de la chaîne d'une zone, dans ce processus. Pris par
/// [`ecrire_chaine`] et [`migrer_zone`] : une migration au démarrage (T03)
/// ne réécrit jamais une chaîne plus ancienne par-dessus un `PUT` TESS (T08)
/// qui vient de passer.
pub fn verrou_de_zone(zone_id: i64) -> Arc<Mutex<()>> {
    static VERROUS: OnceLock<Mutex<HashMap<i64, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut table = VERROUS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.entry(zone_id).or_default().clone()
}

// ── Erreurs ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ErreurMigration {
    /// Une clé ne se relit pas : la zone n'est PAS migrée, rien n'est écrit.
    CleCorrompue { cle: String, raison: String },
    /// Les réglages d'un greffon de la chaîne ne s'écrivent pas dans
    /// l'ancienne clé correspondante.
    ReglagesInvalides { id: String, raison: String },
    /// La chaîne vient d'une version plus récente : jamais réécrite ici.
    VersionPlusRecente { trouvee: u32 },
    /// La base a refusé une lecture ou une écriture.
    Stockage(String),
}

impl fmt::Display for ErreurMigration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CleCorrompue { cle, raison } => {
                write!(f, "unreadable setting {cle}: {raison}")
            }
            Self::ReglagesInvalides { id, raison } => {
                write!(f, "invalid settings for plugin {id}: {raison}")
            }
            Self::VersionPlusRecente { trouvee } => write!(
                f,
                "chain version {trouvee} is newer than this server ({VERSION_CHAINE})"
            ),
            Self::Stockage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for ErreurMigration {}

fn corrompue(cle: &str, raison: impl Into<String>) -> ErreurMigration {
    ErreurMigration::CleCorrompue {
        cle: cle.to_owned(),
        raison: raison.into(),
    }
}

// ── Les anciennes clés ──────────────────────────────────────────────

/// Une ancienne clé de zone exprimée par la chaîne.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Partie {
    Egaliseur,
    Convolution,
    Crossfeed,
    Natif(String),
    Mono,
    Reaffectation,
    Surechantillonnage,
}

impl Partie {
    const FIXES: [Partie; 6] = [
        Partie::Egaliseur,
        Partie::Convolution,
        Partie::Crossfeed,
        Partie::Mono,
        Partie::Reaffectation,
        Partie::Surechantillonnage,
    ];

    /// Le nom de la partie dans l'ombre.
    pub fn nom(&self) -> String {
        match self {
            Self::Egaliseur => "eq_profile".into(),
            Self::Convolution => "ir_path".into(),
            Self::Crossfeed => "crossfeed".into(),
            Self::Natif(id) => format!("native_plugin_{id}"),
            Self::Mono => "mono_downmix".into(),
            Self::Reaffectation => "channel_remap".into(),
            Self::Surechantillonnage => "upsampler".into(),
        }
    }

    fn depuis_nom(nom: &str) -> Option<Self> {
        Some(match nom {
            "eq_profile" => Self::Egaliseur,
            "ir_path" => Self::Convolution,
            "crossfeed" => Self::Crossfeed,
            "mono_downmix" => Self::Mono,
            "channel_remap" => Self::Reaffectation,
            "upsampler" => Self::Surechantillonnage,
            autre => Self::Natif(autre.strip_prefix("native_plugin_")?.to_owned()),
        })
    }

    /// La clé de réglage de cette partie pour la zone.
    pub fn cle(&self, zone_id: i64) -> String {
        match self {
            Self::Egaliseur => format!("zone_{zone_id}_eq_profile"),
            Self::Convolution => format!("ir_path_{zone_id}"),
            Self::Crossfeed => format!("zone_{zone_id}_crossfeed"),
            Self::Natif(id) => crate::audio::natifs_tiers::cle_de_zone(zone_id, id),
            Self::Mono => format!("zone_{zone_id}_mono_downmix"),
            Self::Reaffectation => crate::audio::reaffectation_canaux::cle_de_zone(zone_id),
            Self::Surechantillonnage => crate::audio::surechantillonnage::cle_de_zone(zone_id),
        }
    }
}

/// La valeur brute de chaque ancienne clé (`None` = absente).
type Instantane = BTreeMap<Partie, Option<String>>;
/// L'ombre : nom de partie → valeur brute.
type Ombre = BTreeMap<String, Option<String>>;

fn stockage<T>(r: Result<T, String>) -> Result<T, ErreurMigration> {
    r.map_err(ErreurMigration::Stockage)
}

fn lire_instantane(settings: &SettingsRepo, zone_id: i64) -> Result<Instantane, ErreurMigration> {
    let mut inst = Instantane::new();
    for p in Partie::FIXES {
        let v = stockage(settings.get(&p.cle(zone_id)))?;
        inst.insert(p, v);
    }
    // Par préfixe : jamais toute la table `settings` (caches compris).
    let prefixe = format!("zone_{zone_id}_native_plugin_");
    for (cle, valeur) in stockage(settings.par_prefixe(&prefixe))? {
        if let Some(id) = cle.strip_prefix(&prefixe)
            && !id.is_empty()
        {
            inst.insert(Partie::Natif(id.to_owned()), Some(valeur));
        }
    }
    Ok(inst)
}

/// Deux valeurs brutes ont-elles le même sens ? Égales, ou le même JSON.
fn meme_valeur(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x == y
                || matches!(
                    (serde_json::from_str::<Value>(x), serde_json::from_str::<Value>(y)),
                    (Ok(u), Ok(v)) if u == v
                )
        }
        _ => false,
    }
}

/// Un objet JSON, ou un refus qui nomme la clé.
fn objet(cle: &str, brut: &str) -> Result<Map<String, Value>, ErreurMigration> {
    match serde_json::from_str::<Value>(brut) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(corrompue(cle, "not a JSON object")),
        Err(e) => Err(corrompue(cle, format!("invalid JSON: {e}"))),
    }
}

/// `enabled` d'un objet de réglages, lu comme aujourd'hui : seul `true`
/// coche ; absent ou non booléen = décoché, sans refus.
fn coche(m: &Map<String, Value>) -> bool {
    m.get("enabled").and_then(Value::as_bool).unwrap_or(false)
}

fn sans_enabled(mut m: Map<String, Value>) -> Value {
    m.remove("enabled");
    Value::Object(m)
}

/// Une ancienne clé décodée.
#[derive(Debug, Clone)]
enum Decodee {
    Absente,
    /// Profil d'égaliseur, objet brut et profil typé.
    Egaliseur(Map<String, Value>, EqProfile),
    /// Objet de réglages d'un greffon (crossfeed, tiers, réaffectation).
    Objet(Map<String, Value>),
    Chemin(String),
    Mono(bool),
    Surechantillonnage(UpsamplerSettings),
}

fn decoder(partie: &Partie, cle: &str, brut: Option<&str>) -> Result<Decodee, ErreurMigration> {
    let Some(brut) = brut else {
        return Ok(Decodee::Absente);
    };
    Ok(match partie {
        Partie::Egaliseur => {
            let m = objet(cle, brut)?;
            let profil: EqProfile = serde_json::from_value(Value::Object(m.clone()))
                .map_err(|e| corrompue(cle, format!("not an equalizer profile: {e}")))?;
            Decodee::Egaliseur(m, profil)
        }
        // Lecteurs actuels non typés : un objet JSON suffit.
        Partie::Crossfeed | Partie::Natif(_) => Decodee::Objet(objet(cle, brut)?),
        Partie::Reaffectation => {
            let m = objet(cle, brut)?;
            serde_json::from_value::<crate::audio::reaffectation_canaux::ChannelRemapSettings>(
                Value::Object(m.clone()),
            )
            .map_err(|e| corrompue(cle, format!("not channel remap settings: {e}")))?;
            Decodee::Objet(m)
        }
        // Un chemin vide ne désigne aucune réponse impulsionnelle.
        Partie::Convolution if brut.trim().is_empty() => Decodee::Absente,
        Partie::Convolution => Decodee::Chemin(brut.to_owned()),
        // Comme aujourd'hui : seul « true » arme le repli mono.
        Partie::Mono => Decodee::Mono(brut == "true"),
        Partie::Surechantillonnage => {
            let v: Value = serde_json::from_str(brut)
                .map_err(|e| corrompue(cle, format!("invalid JSON: {e}")))?;
            let s = crate::audio::surechantillonnage::greffon::settings(&v)
                .map_err(|e| corrompue(cle, format!("refused by the upsampler: {e:?}")))?;
            Decodee::Surechantillonnage(s)
        }
    })
}

// ── Égaliseur : quelle façade ? ─────────────────────────────────────

/// La courbe peut-elle être réémise sans perte par l'égaliseur graphique ?
/// Même règle que le client web (`estCourbeGraphique`) : une grille ISO (10,
/// 15 ou 31 points) avec son Q, des bandes en cloche, une passe sans canal ou
/// deux passes gauche puis droite.
pub fn est_courbe_graphique(bandes: &[EqBandSpec]) -> bool {
    [10u32, 15, 31].into_iter().any(|n| {
        let (grille, q) = grille_graphique(n);
        let g = grille.len();
        (bandes.len() == g || bandes.len() == 2 * g)
            && bandes.iter().enumerate().all(|(i, b)| {
                b.freq == grille[i % g]
                    && b.q == q
                    && b.band_type == "peak"
                    && if bandes.len() == g {
                        b.channel.is_none()
                    } else {
                        b.channel == Some(if i < g { 0 } else { 1 })
                    }
            })
    })
}

/// Le profil de pièce mesuré de la zone, posé dans l'égaliseur par
/// `POST /room-correction/profiles/{zone}/apply` (#7632 C2).
fn profil_de_piece(settings: &SettingsRepo, zone_id: i64) -> Option<(EqProfile, &'static str)> {
    let brut = settings
        .get(&format!("room_profile_{zone_id}"))
        .ok()
        .flatten()?;
    let profil: crate::room_correction::RoomProfile = serde_json::from_str(&brut).ok()?;
    if profil.filters.is_empty() {
        return None;
    }
    let origine = serde_json::from_str::<Value>(&brut)
        .map(|v| origine_de_la_piece(&v))
        .unwrap_or(ORIGINE_MESURE);
    Some((
        crate::room_correction::profil_eq_de_correction(&profil.filters),
        origine,
    ))
}

/// D1 — d'où vient un profil de pièce (`room_profile_{id}`, JSON brut) ?
///
/// On tranche ainsi :
/// 1. `"origin": "estimation"|"measured"`, que le serveur enregistre à la
///    création du profil (`RoomProfile::origin`), l'emporte. Aucun autre
///    champ n'est cru : le serveur n'écrit jamais `"estimation": bool` dans
///    un profil, et un client ne peut pas écrire `room_profile_*` par
///    `PATCH /system/config` (voir [`cle_tenue_par_le_serveur`]) ;
/// 2. sinon, le nom par défaut d'une estimation C5 (« Estimation {id} »,
///    [`PREFIXE_NOM_ESTIMATION`]) désigne une estimation ;
/// 3. sinon : MESURÉE. Les bornes plus serrées de C5 ne servent pas d'indice
///    (le mode expert, #8332, les relève à celles d'une mesure). Dans le
///    doute, la correction est traitée comme Premium : sur un compte gratuit
///    elle est convertie en graphique, jamais détruite.
pub fn origine_de_la_piece(profil: &Value) -> &'static str {
    match profil.get("origin").and_then(Value::as_str) {
        Some(ORIGINE_ESTIMATION) => return ORIGINE_ESTIMATION,
        Some(ORIGINE_MESURE) => return ORIGINE_MESURE,
        _ => {}
    }
    if profil
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.starts_with(PREFIXE_NOM_ESTIMATION))
    {
        ORIGINE_ESTIMATION
    } else {
        ORIGINE_MESURE
    }
}

fn memes_bandes(a: &[EqBandSpec], b: &[EqBandSpec]) -> bool {
    let proche = |x: f64, y: f64| (x - y).abs() < 1e-6;
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.band_type == y.band_type
                && x.channel == y.channel
                && proche(x.freq, y.freq)
                && proche(x.gain, y.gain)
                && proche(x.q, y.q)
        })
}

/// La façade d'un profil `zone_{id}_eq_profile` :
/// - le profil de pièce mesuré de la zone → `room-correction` ;
/// - aucune bande (curseurs grave/médium/aigu) ou une courbe graphique →
///   `equalizer-graphic` ;
/// - sinon `equalizer-headphone` si la chaîne l'avait déjà ainsi (profils
///   AutoEQ/OPRA : rien d'autre ne les distingue), sinon
///   `equalizer-parametric`.
pub fn classer_egaliseur(
    profil: &EqProfile,
    piece: Option<&EqProfile>,
    precedente: Option<&str>,
) -> &'static str {
    if piece.is_some_and(|p| !profil.bands.is_empty() && memes_bandes(&profil.bands, &p.bands)) {
        ID_CORRECTION_PIECE
    } else if profil.bands.is_empty() || est_courbe_graphique(&profil.bands) {
        ID_EQ_GRAPHIQUE
    } else if precedente == Some(ID_EQ_CASQUE) {
        ID_EQ_CASQUE
    } else {
        ID_EQ_PARAMETRIQUE
    }
}

/// D1 : réécrit l'origine de chaque `room-correction` d'après
/// `room_profile_{id}`, quoi que dise le client : `"estimation"` seulement si
/// ses bandes sont celles d'un profil de pièce calculé sans micro de la zone,
/// `"measured"` sinon. Un corps de `PUT` ne peut donc jamais lever le droit
/// Premium en se disant « sans micro ».
pub fn origines_par_le_serveur(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
) -> ChaineTess {
    let mut c = chaine.clone();
    if !c.plugins.iter().any(|m| m.id == ID_CORRECTION_PIECE) {
        return c;
    }
    let piece = profil_de_piece(settings, zone_id);
    for m in &mut c.plugins {
        if m.id != ID_CORRECTION_PIECE {
            continue;
        }
        let bandes: Option<Vec<EqBandSpec>> = m
            .settings
            .get("bands")
            .and_then(|b| serde_json::from_value(b.clone()).ok());
        let origine = match (&piece, bandes) {
            (Some((p, o)), Some(b)) if !b.is_empty() && memes_bandes(&b, &p.bands) => *o,
            _ => ORIGINE_MESURE,
        };
        if let Value::Object(s) = &mut m.settings {
            s.insert(CHAMP_ORIGINE.into(), Value::String(origine.into()));
        }
    }
    c
}

/// Les marques posées par la lecture d'anciennes clés ([`MARQUE_REMPLACE`]
/// du groupe crossfeed, [`MARQUE_MONO_INERTE`] de D2) appartiennent au
/// serveur, comme l'origine D1 : un corps de `PUT` ne les garde que si le
/// maillon, décoché, porte DÉJÀ la même marque dans la chaîne que le serveur
/// lit pour la zone (`reference`), et, pour `replaced_in_group`, si le
/// gagnant nommé reste coché. Sinon la marque est retirée et le maillon
/// décoché est projeté normalement (son ancienne clé est décochée).
///
/// Sans cela, un client qui renvoie les réglages tels qu'il les a lus
/// garderait l'ancienne clé d'un membre écarté cochée après avoir décoché le
/// gagnant (l'ancien moteur jouerait ce membre, et la lecture le recocherait),
/// et un repli mono décoché à la main sur une zone réseau serait recoché en
/// passant sur une sortie locale.
fn marques_par_le_serveur(reference: Option<&ChaineTess>, chaine: &ChaineTess) -> ChaineTess {
    let mut c = chaine.clone();
    let coches: Vec<String> = c
        .plugins
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.id.clone())
        .collect();
    for m in &mut c.plugins {
        for marque in [MARQUE_REMPLACE, MARQUE_MONO_INERTE] {
            let Some(valeur) = m.settings.get(marque).cloned() else {
                continue;
            };
            let deja_posee = reference.is_some_and(|r| {
                r.plugins
                    .iter()
                    .any(|a| a.id == m.id && !a.enabled && a.settings.get(marque) == Some(&valeur))
            });
            let gagnant_coche = marque != MARQUE_REMPLACE
                || valeur
                    .as_str()
                    .is_some_and(|g| coches.iter().any(|id| id == g));
            if (m.enabled || !deja_posee || !gagnant_coche)
                && let Value::Object(s) = &mut m.settings
            {
                s.remove(marque);
            }
        }
    }
    c
}

/// D1 : les réglages que seul le serveur écrit, refusés par l'écriture
/// générique (`PATCH /system/config`) : le profil de pièce de chaque zone et
/// son index (l'origine y décide du droit Premium), la chaîne TESS de chaque
/// zone et son ombre (`zone_{id}_tess_chain`, `zone_{id}_tess_chain_sync`).
pub fn cle_tenue_par_le_serveur(cle: &str) -> bool {
    if cle.starts_with("room_profile_") {
        return true;
    }
    cle.strip_prefix("zone_")
        .and_then(|reste| reste.split_once('_'))
        .is_some_and(|(id, suite)| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && (suite == "tess_chain" || suite == "tess_chain_sync")
        })
}

/// D1 : ce maillon est-il une correction de pièce CALCULÉE SANS MICRO (C5),
/// Free, active sans Premium ? Seul un `room-correction` dont l'origine est
/// [`ORIGINE_ESTIMATION`]. L'origine est tirée de `room_profile_{id}` par le
/// serveur, à la lecture comme à l'écriture ([`ecrire_chaine`] la réécrit) :
/// un `PUT` ne peut pas la poser lui-même.
pub fn correction_estimee(m: &MaillonGreffon) -> bool {
    m.id == ID_CORRECTION_PIECE
        && m.settings.get(CHAMP_ORIGINE).and_then(Value::as_str) == Some(ORIGINE_ESTIMATION)
}

// ── Place dans la chaîne ────────────────────────────────────────────

/// Le rang d'un greffon dans l'ordre de la lecture d'aujourd'hui.
fn rang(id: &str) -> u8 {
    if FACADES_EQ.contains(&id) {
        0
    } else if id == ID_CONVOLUTION {
        1
    } else if id == ID_CROSSFEED {
        2
    } else if est_de_la_famille_du_crossfeed(id) {
        3
    } else if id == ID_CROSSTALK {
        4
    } else if id == ID_MONO {
        6
    } else if id == ID_REAFFECTATION {
        7
    } else {
        5
    }
}

fn inserer(chaine: &mut ChaineTess, maillon: MaillonGreffon) {
    let r = rang(&maillon.id);
    let pos = chaine
        .plugins
        .iter()
        .position(|m| rang(&m.id) > r)
        .unwrap_or(chaine.plugins.len());
    chaine.plugins.insert(pos, maillon);
}

/// Remplace le greffon `id` sur place, ou l'insère à son rang.
fn poser(chaine: &mut ChaineTess, id: &str, enabled: bool, settings: Value) {
    if let Some(m) = chaine.plugins.iter_mut().find(|m| m.id == id) {
        m.enabled = enabled;
        m.settings = settings;
    } else {
        inserer(
            chaine,
            MaillonGreffon {
                id: id.to_owned(),
                enabled,
                settings,
            },
        );
    }
}

fn retirer(chaine: &mut ChaineTess, id: &str) {
    chaine.plugins.retain(|m| m.id != id);
}

/// L'égaliseur que porte `zone_{id}_eq_profile` : la première façade
/// cochée, sinon la première façade.
fn egaliseur_principal(chaine: &ChaineTess) -> Option<usize> {
    let facades = || {
        chaine
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, m)| FACADES_EQ.contains(&m.id.as_str()))
    };
    facades()
        .find(|(_, m)| m.enabled)
        .or_else(|| facades().next())
        .map(|(i, _)| i)
}

/// Les greffons portés par `zone_{id}_native_plugin_{p}` : les paquets du
/// catalogue (Crossfeed Pro, Crosstalk) et tout id hors catalogue de
/// référence (paquets tiers).
fn est_natif(id: &str) -> bool {
    id_valide(id)
        && match catalogue_de_reference().get(id) {
            Some(d) => d.origin == Origine::Package,
            None => !FACADES_EQ.contains(&id) && id != ID_MONO,
        }
}

// ── Ancienne clé → chaîne ───────────────────────────────────────────

/// Ce que la zone sait d'elle-même, hors anciennes clés.
struct ContexteZone {
    /// Le profil de pièce de la zone et son origine.
    piece: Option<(EqProfile, &'static str)>,
    /// Sortie locale : le repli mono y agit aujourd'hui.
    sortie_locale: bool,
    /// Aucune chaîne enregistrée : c'est la migration (D2 ne s'applique
    /// qu'à ce moment-là).
    premiere_lecture: bool,
}

fn appliquer(chaine: &mut ChaineTess, partie: &Partie, valeur: Decodee, ctx: &ContexteZone) {
    match (partie, valeur) {
        (Partie::Egaliseur, Decodee::Egaliseur(m, profil)) => {
            let principal = egaliseur_principal(chaine);
            let precedente = principal.map(|i| chaine.plugins[i].id.clone());
            let id = classer_egaliseur(
                &profil,
                ctx.piece.as_ref().map(|(p, _)| p),
                precedente.as_deref(),
            );
            let mut settings = sans_enabled(m);
            if id == ID_CORRECTION_PIECE
                && let (Value::Object(s), Some((_, origine))) = (&mut settings, &ctx.piece)
            {
                s.insert(CHAMP_ORIGINE.into(), Value::String((*origine).into()));
            }
            let maillon = MaillonGreffon {
                id: id.to_owned(),
                enabled: profil.enabled,
                settings,
            };
            match principal {
                Some(i) => {
                    // Un autre maillon portait déjà cet id : l'ancienne clé
                    // l'emporte, il n'en reste qu'un.
                    let doublon = (chaine.plugins[i].id != id)
                        .then(|| chaine.plugins.iter().position(|x| x.id == id))
                        .flatten();
                    chaine.plugins[i] = maillon;
                    if let Some(j) = doublon {
                        chaine.plugins.remove(j);
                    }
                }
                None => inserer(chaine, maillon),
            }
        }
        (Partie::Egaliseur, _) => {
            if let Some(i) = egaliseur_principal(chaine) {
                chaine.plugins.remove(i);
            }
        }
        (Partie::Convolution, Decodee::Chemin(chemin)) => {
            let mut s = match chaine.plugins.iter().find(|m| m.id == ID_CONVOLUTION) {
                Some(MaillonGreffon {
                    settings: Value::Object(s),
                    ..
                }) => s.clone(),
                _ => Map::new(),
            };
            s.insert("ir_path".into(), Value::String(chemin));
            poser(chaine, ID_CONVOLUTION, true, Value::Object(s));
        }
        (Partie::Convolution, _) => retirer(chaine, ID_CONVOLUTION),
        (Partie::Crossfeed, Decodee::Objet(m)) => {
            let on = coche(&m);
            poser(chaine, ID_CROSSFEED, on, sans_enabled(m));
        }
        (Partie::Crossfeed, _) => retirer(chaine, ID_CROSSFEED),
        (Partie::Natif(id), Decodee::Objet(m)) => {
            let on = coche(&m);
            poser(chaine, id, on, sans_enabled(m));
        }
        (Partie::Natif(id), _) => retirer(chaine, id),
        (Partie::Reaffectation, Decodee::Objet(m)) => {
            let on = coche(&m);
            poser(chaine, ID_REAFFECTATION, on, sans_enabled(m));
        }
        (Partie::Reaffectation, _) => retirer(chaine, ID_REAFFECTATION),
        (Partie::Mono, Decodee::Mono(true)) if ctx.sortie_locale || !ctx.premiere_lecture => {
            poser(chaine, ID_MONO, true, Value::Null)
        }
        // D2 : inerte jusqu'ici sur une sortie réseau, décoché d'office UNE
        // fois, à la migration, pour ne rien changer à l'écoute ; coché
        // ensuite (TESS ou ancienne route), il agit partout.
        (Partie::Mono, Decodee::Mono(true)) => {
            let mut marque = Map::new();
            marque.insert(MARQUE_MONO_INERTE.into(), Value::Bool(true));
            poser(chaine, ID_MONO, false, Value::Object(marque))
        }
        (Partie::Mono, _) => retirer(chaine, ID_MONO),
        (Partie::Surechantillonnage, Decodee::Surechantillonnage(s)) => {
            chaine.fixed.upsampling = if s.enabled {
                Upsampling::Pcm {
                    filter: s.filter,
                    max_ratio: s.max_ratio,
                    max_rate_hz: s.max_rate_hz,
                }
            } else {
                Upsampling::Off {}
            };
        }
        (Partie::Surechantillonnage, _) => chaine.fixed.upsampling = Upsampling::Off {},
    }
}

// ── Lecture ─────────────────────────────────────────────────────────

/// La chaîne d'une zone, et ce qui l'accompagne hors chaîne.
#[derive(Debug, Clone, PartialEq)]
pub struct LectureZone {
    pub chaine: ChaineTess,
    /// Mode Pure demandé (`audiophile` ou `strict_bitperfect`) : alimente
    /// `ContexteExecution.pure`, ce n'est pas un champ de la chaîne.
    pub pure: bool,
    /// #4685 — `zone_{id}_level_compensation` (vrai sauf `"false"`) : la
    /// compensation du gain MOYEN de l'égaliseur et du crossfeed, au volume.
    /// Ce n'est pas le headroom (réserve de crête) : T07 la garde telle
    /// quelle, l'interrupteur coupé reste coupé.
    pub compensation_de_niveau: bool,
    /// La colonne `zones.dsd_mode` (`auto`, `native`, `dop`, `pcm`…) : ce que
    /// la zone fait d'une SOURCE DSD ([`lecture_dsd`]). L'upsampling DSD de la
    /// chaîne ne concerne que les sources PCM.
    pub dsd_mode: String,
    /// La zone sort sur une sortie locale (`local:`) ; inconnue = locale.
    pub sortie_locale: bool,
    /// Aucune chaîne n'était enregistrée : elle vient des anciennes clés.
    pub depuis_les_anciennes_cles: bool,
    /// Un égaliseur Premium coché a été converti en graphique (compte
    /// gratuit), pour cette lecture seulement : le paramétrique (P2) ou la
    /// correction de pièce mesurée (D1, voir [`Self::conversion_piece`]).
    pub conversion_p2: bool,
    /// La conversion portait sur une correction de pièce MESURÉE (D1).
    pub conversion_piece: bool,
    /// Valeur brute des anciennes clés à cet instant (l'ombre à écrire).
    ombre: Ombre,
}

impl LectureZone {
    /// Le texte de la chaîne et celui de l'ombre, tels que [`migrer_zone`]
    /// les enregistrerait (sert à l'export de configuration).
    pub fn textes(&self) -> Result<(String, String), ErreurMigration> {
        let chaine = serde_json::to_string(&self.chaine)
            .map_err(|e| ErreurMigration::Stockage(e.to_string()))?;
        let ombre = serde_json::to_string(&self.ombre)
            .map_err(|e| ErreurMigration::Stockage(e.to_string()))?;
        Ok((chaine, ombre))
    }
}

/// Ce qu'un `dsd_mode` fait d'une source DSD, pour `SourceFlux::Dsd` (T16) :
/// `native` et `dop` restent hors traitement (`dsd_source_native`), `pcm` est
/// converti avant la chaîne. `auto` (ou inconnu) dépend du périphérique :
/// `None`, la sortie décide.
pub fn lecture_dsd(dsd_mode: &str) -> Option<LectureDsd> {
    match dsd_mode {
        "native" => Some(LectureDsd::Native),
        "dop" => Some(LectureDsd::Dop),
        "pcm" => Some(LectureDsd::ConvertedToPcm),
        _ => None,
    }
}

/// L'ombre enregistrée ; illisible = absente (les anciennes clés
/// l'emportent alors), jamais un refus.
fn lire_ombre(settings: &SettingsRepo, zone_id: i64) -> Result<Option<Ombre>, ErreurMigration> {
    let cle = cle_ombre_zone(zone_id);
    Ok(stockage(settings.get(&cle))?.and_then(|brut| serde_json::from_str(&brut).ok()))
}

/// D2 : l'entrée de l'ombre qui garde la nature de la sortie de la zone à
/// la dernière synchronisation ([`SORTIE_LOCALE`] ou [`SORTIE_RESEAU`]). Ce
/// n'est pas une partie : [`Partie::depuis_nom`] l'ignore.
pub const OMBRE_SORTIE: &str = "output";
pub const SORTIE_LOCALE: &str = "local";
pub const SORTIE_RESEAU: &str = "network";

fn ombre_de(inst: &Instantane, sortie_locale: bool) -> Ombre {
    let mut o: Ombre = inst.iter().map(|(p, v)| (p.nom(), v.clone())).collect();
    o.insert(
        OMBRE_SORTIE.into(),
        Some(
            if sortie_locale {
                SORTIE_LOCALE
            } else {
                SORTIE_RESEAU
            }
            .into(),
        ),
    );
    o
}

/// La sortie de la zone est-elle locale (le repli mono y agit aujourd'hui) ?
/// Zone inconnue : locale, comme la lecture d'aujourd'hui.
fn sortie_locale_de(settings: &SettingsRepo, zone_id: i64) -> Result<bool, ErreurMigration> {
    let zone = stockage(
        crate::db::zone_repo::ZoneRepo::with_backend(settings.backend().clone()).get(zone_id),
    )?;
    Ok(zone.as_ref().is_none_or(|z| {
        crate::audio::mono_downmix::mono_downmix_runs_on_output(z.output_device_id.as_deref())
    }))
}

/// Mode Pure, lu exactement comme aujourd'hui : `audiophile` objet JSON avec
/// `"enabled": true`, ou `strict_bitperfect` égal à `"true"`. Tout le reste
/// vaut faux, sans refus.
fn lire_pure(settings: &SettingsRepo, zone_id: i64) -> Result<bool, ErreurMigration> {
    let audiophile = stockage(settings.get(&format!("zone_{zone_id}_audiophile")))?
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .and_then(|v| v.get("enabled").and_then(Value::as_bool))
        .unwrap_or(false);
    let strict = stockage(settings.get(&crate::audio::bitperfect_strict::cle_de_zone(zone_id)))?
        .as_deref()
        == Some("true");
    Ok(audiophile || strict)
}

/// #4685 — vrai par défaut ; seul `"false"` l'éteint (comme l'orchestrateur).
fn lire_compensation(settings: &SettingsRepo, zone_id: i64) -> Result<bool, ErreurMigration> {
    Ok(
        stockage(settings.get(&format!("zone_{zone_id}_level_compensation")))?.as_deref()
            != Some("false"),
    )
}

/// La grille graphique réglée sur ce serveur (`eq_expert_bands`, 10 par défaut).
pub fn grille_reglee(settings: &SettingsRepo) -> u32 {
    settings
        .get(CLE_GRILLE_EQ)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| [10, 15, 31].contains(n))
        .unwrap_or(10)
}

/// La chaîne de la zone (voir l'en-tête du module).
///
/// `premium` : sans Premium, un égaliseur paramétrique COCHÉ est converti en
/// égaliseur graphique au plus près (décision P2), **dans la chaîne rendue
/// seulement** ; décoché, il est gardé tel quel (un greffon décoché ne
/// demande aucun droit). Rien n'est écrit.
///
/// Une chaîne enregistrée par une version plus récente est rendue sans y
/// réappliquer les anciennes clés (la résolution l'éteint,
/// `chain_version_unsupported`) ; l'origine de ses corrections de pièce est
/// tout de même recalculée, et P2/D1 appliqués sans Premium.
pub fn lire_zone(
    settings: &SettingsRepo,
    zone_id: i64,
    premium: bool,
) -> Result<LectureZone, ErreurMigration> {
    let cle_chaine = super::cle_chaine_zone(zone_id);
    let stockee = match stockage(settings.get(&cle_chaine))? {
        None => None,
        Some(brut) => Some(
            serde_json::from_str::<ChaineTess>(&brut)
                .map_err(|e| corrompue(&cle_chaine, format!("invalid chain: {e}")))?,
        ),
    };
    let ombre = lire_ombre(settings, zone_id)?;
    let inst = lire_instantane(settings, zone_id)?;
    let pure = lire_pure(settings, zone_id)?;
    let compensation_de_niveau = lire_compensation(settings, zone_id)?;
    let zone = stockage(
        crate::db::zone_repo::ZoneRepo::with_backend(settings.backend().clone()).get(zone_id),
    )?;
    let sortie_locale = zone.as_ref().is_none_or(|z| {
        crate::audio::mono_downmix::mono_downmix_runs_on_output(z.output_device_id.as_deref())
    });
    let dsd_mode = if zone.is_some() {
        crate::db::zone_repo::ZoneRepo::with_backend(settings.backend().clone())
            .get_dsd_mode(zone_id)
    } else {
        "auto".to_owned()
    };

    // Chaque ancienne clé est décodée, qu'elle ait changé ou non : une clé
    // illisible refuse toujours la zone.
    let mut decodees = Vec::with_capacity(inst.len());
    for (partie, brut) in &inst {
        let cle = partie.cle(zone_id);
        decodees.push((partie.clone(), decoder(partie, &cle, brut.as_deref())?));
    }

    let lecture = |chaine: ChaineTess,
                   depuis: bool,
                   conversion: Option<&'static str>,
                   ombre: Ombre| LectureZone {
        chaine,
        pure,
        compensation_de_niveau,
        dsd_mode: dsd_mode.clone(),
        sortie_locale,
        depuis_les_anciennes_cles: depuis,
        conversion_p2: conversion.is_some(),
        conversion_piece: conversion == Some(ID_CORRECTION_PIECE),
        ombre,
    };

    if let Some(c) = &stockee
        && c.version > VERSION_CHAINE
    {
        // Rendue sans y réappliquer les anciennes clés, mais D1 vaut aussi
        // ici : l'origine d'une correction de pièce est recalculée d'après
        // `room_profile_{id}`, et un compte gratuit a la même conversion
        // qu'ailleurs. Aucune origine ne vient du stockage brut.
        let mut chaine = origines_par_le_serveur(settings, zone_id, c);
        let conversion = if premium {
            None
        } else {
            convertir_p2(&mut chaine, grille_reglee(settings))
        };
        return Ok(lecture(
            chaine,
            false,
            conversion,
            ombre.unwrap_or_default(),
        ));
    }

    let depuis_les_anciennes_cles = stockee.is_none();
    // Sans ombre, toute clé compte comme modifiée (chaîne posée par une
    // version sans ombre, ombre perdue ou illisible) : les anciennes clés
    // l'emportent.
    let (mut chaine, ombre_lue, ombre_connue) = match (stockee, ombre) {
        (Some(c), Some(o)) => (c.mettre_a_niveau(), o, true),
        (Some(c), None) => (c.mettre_a_niveau(), Ombre::new(), false),
        (None, _) => (ChaineTess::default(), Ombre::new(), false),
    };

    let ctx = ContexteZone {
        piece: profil_de_piece(settings, zone_id),
        sortie_locale,
        premiere_lecture: depuis_les_anciennes_cles,
    };
    // Les parties disparues depuis l'ombre (un réglage tiers effacé) : on les
    // applique comme absentes.
    for nom in ombre_lue.keys() {
        if let Some(p) = Partie::depuis_nom(nom)
            && !inst.contains_key(&p)
        {
            decodees.push((p, Decodee::Absente));
        }
    }
    decodees.sort_by(|a, b| a.0.cmp(&b.0));
    for (partie, valeur) in decodees {
        let brut = inst.get(&partie).cloned().flatten();
        let inchangee = ombre_connue
            && ombre_lue
                .get(&partie.nom())
                .is_some_and(|o| meme_valeur(o.as_deref(), brut.as_deref()));
        if !inchangee {
            appliquer(&mut chaine, &partie, valeur, &ctx);
        }
    }

    exclusivite_du_groupe(&mut chaine, &|id| membre_en_service(settings, id));
    // D2 (18/11 soir) : la zone est passée d'une sortie locale au réseau
    // depuis la dernière synchronisation, et l'interrupteur mono n'a pas
    // bougé : même règle qu'à la migration, le mono est décoché d'office
    // (l'ancien moteur l'ignorait sur le réseau). Recoché ensuite (`PUT`),
    // l'ombre dit « réseau » et il agit.
    let mono_inchange = inst.get(&Partie::Mono).is_some_and(|brut| {
        ombre_lue
            .get(&Partie::Mono.nom())
            .is_some_and(|o| meme_valeur(o.as_deref(), brut.as_deref()))
    });
    if !sortie_locale
        && ombre_connue
        && mono_inchange
        && ombre_lue.get(OMBRE_SORTIE).and_then(|o| o.as_deref()) == Some(SORTIE_LOCALE)
    {
        for m in &mut chaine.plugins {
            if m.id == ID_MONO && m.enabled {
                let mut marque = Map::new();
                marque.insert(MARQUE_MONO_INERTE.into(), Value::Bool(true));
                m.enabled = false;
                m.settings = Value::Object(marque);
            }
        }
    }
    // D2 : un repli mono décoché parce qu'inerte sur le réseau redevient
    // coché si la zone sort maintenant en local (l'interrupteur, resté à
    // « true », y agit).
    if sortie_locale {
        for m in &mut chaine.plugins {
            if m.id == ID_MONO
                && !m.enabled
                && m.settings.get(MARQUE_MONO_INERTE).and_then(Value::as_bool) == Some(true)
            {
                m.enabled = true;
                m.settings = Value::Null;
            }
        }
    }

    // D1 : l'origine d'une correction de pièce ne vient jamais du stockage
    // brut de la chaîne (un client a pu l'écrire) : elle est recalculée
    // d'après `room_profile_{id}`, comme à l'écriture.
    let mut chaine = origines_par_le_serveur(settings, zone_id, &chaine);

    let conversion = if premium {
        None
    } else {
        convertir_p2(&mut chaine, grille_reglee(settings))
    };
    Ok(lecture(
        chaine,
        depuis_les_anciennes_cles,
        conversion,
        ombre_de(&inst, sortie_locale),
    ))
}

// ── Groupe crossfeed : un seul membre coché ─────────────────────────

/// La priorité d'un membre du groupe crossfeed / Crosstalk, ou `None`.
pub fn priorite_dans_le_groupe(id: &str) -> Option<u8> {
    if id == ID_CROSSTALK {
        Some(PRIORITE_CROSSTALK)
    } else if est_de_la_famille_du_crossfeed(id) {
        Some(PRIORITE_CROSSFEED_TIERS)
    } else if id == ID_CROSSFEED {
        Some(PRIORITE_CROSSFEED_INTEGRE)
    } else {
        None
    }
}

/// Ce membre traite-t-il aujourd'hui, à droit égal ? Le crossfeed intégré si
/// son greffon est installé et activé ; un paquet s'il est installé et
/// activé (`plugin_{id}_installed`, `plugin_{id}_enabled`). Le droit Premium
/// n'entre pas en compte : la chaîne enregistrée n'en dépend pas.
fn membre_en_service(settings: &SettingsRepo, id: &str) -> bool {
    if id == ID_CROSSFEED {
        crate::audio::premium_plugins::enabled(settings, "crossfeed")
    } else {
        crate::audio::natifs_tiers::demande(settings, id)
    }
}

/// Décision B : une chaîne ne coche qu'un membre du groupe crossfeed. Quand
/// les anciennes clés en cochent plusieurs, on garde celui qui traite
/// aujourd'hui (#7616 : Crosstalk > `crossfeed-*` > crossfeed intégré, parmi
/// les membres en service ; à égalité, le premier de la chaîne) et on
/// DÉCOCHE les autres, réglages gardés, marqués [`MARQUE_REMPLACE`] : rien ne
/// change à l'oreille, et aucune chaîne migrée n'est refusée. Un membre
/// écarté dont le gagnant n'est plus coché (ancienne route) est recoché.
fn exclusivite_du_groupe(chaine: &mut ChaineTess, en_service: &dyn Fn(&str) -> bool) {
    let coches: Vec<String> = chaine
        .plugins
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.id.clone())
        .collect();
    for m in &mut chaine.plugins {
        let gagnant = m.settings.get(MARQUE_REMPLACE).and_then(Value::as_str);
        if !m.enabled
            && let Some(g) = gagnant
            && !coches.iter().any(|c| c == g)
        {
            m.enabled = true;
            if let Value::Object(s) = &mut m.settings {
                s.remove(MARQUE_REMPLACE);
            }
        }
    }
    let membres: Vec<(usize, u8, bool)> = chaine
        .plugins
        .iter()
        .enumerate()
        .filter(|(_, m)| m.enabled)
        .filter_map(|(i, m)| priorite_dans_le_groupe(&m.id).map(|p| (i, p, en_service(&m.id))))
        .collect();
    if membres.len() < 2 {
        return;
    }
    let tous_hors_service = membres.iter().all(|x| !x.2);
    let Some(&(gagnant, _, _)) = membres
        .iter()
        .filter(|x| x.2 || tous_hors_service)
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
    else {
        return;
    };
    let id_gagnant = chaine.plugins[gagnant].id.clone();
    for &(i, _, _) in &membres {
        if i == gagnant {
            continue;
        }
        let m = &mut chaine.plugins[i];
        m.enabled = false;
        if !m.settings.is_object() {
            m.settings = Value::Object(Map::new());
        }
        if let Value::Object(s) = &mut m.settings {
            s.insert(MARQUE_REMPLACE.into(), Value::String(id_gagnant.clone()));
        }
    }
}

// ── P2 : paramétrique → graphique ───────────────────────────────────

/// P2 et D1 : pour un compte gratuit, l'égaliseur Premium COCHÉ porté par
/// `eq_profile` devient un égaliseur graphique au plus près, sur la grille
/// réglée : le paramétrique (P2), sinon la correction de pièce MESURÉE (D1).
/// Une correction calculée sans micro (Free) n'est jamais convertie. Rend
/// l'id converti. Rien ne change si la chaîne a déjà un égaliseur graphique
/// (deux maillons ne portent jamais le même id) : l'autre reste alors éteint
/// par la résolution (`premium_required`).
pub fn convertir_p2(chaine: &mut ChaineTess, grille: u32) -> Option<&'static str> {
    if chaine.plugins.iter().any(|m| m.id == ID_EQ_GRAPHIQUE) {
        return None;
    }
    let indice = chaine
        .plugins
        .iter()
        .position(|m| m.id == ID_EQ_PARAMETRIQUE && m.enabled)
        .or_else(|| {
            chaine
                .plugins
                .iter()
                .position(|m| m.id == ID_CORRECTION_PIECE && m.enabled && !correction_estimee(m))
        })?;
    let m = &mut chaine.plugins[indice];
    let converti_de: &'static str = if m.id == ID_EQ_PARAMETRIQUE {
        ID_EQ_PARAMETRIQUE
    } else {
        ID_CORRECTION_PIECE
    };
    let profil = match &m.settings {
        Value::Object(s) => {
            let mut s = s.clone();
            s.remove(CHAMP_ORIGINE);
            s.insert("enabled".into(), Value::Bool(true));
            serde_json::from_value::<EqProfile>(Value::Object(s)).ok()
        }
        _ => None,
    };
    m.id = ID_EQ_GRAPHIQUE.to_owned();
    match profil {
        Some(p) => {
            let converti = profil_graphique_au_plus_pres(&p, grille);
            if let Ok(Value::Object(s)) = serde_json::to_value(&converti) {
                m.settings = sans_enabled(s);
            }
        }
        None => {
            if let Value::Object(s) = &mut m.settings {
                s.remove(CHAMP_ORIGINE);
            }
        }
    }
    Some(converti_de)
}

/// Fréquence d'échantillonnage du calcul de courbe.
const TAUX_DE_CALCUL: u32 = 48_000;
/// Gain maximal d'un curseur graphique, en dB (client web : ±12).
pub const GAIN_GRAPHIQUE_MAX_DB: f64 = 12.0;

/// La réponse en dB de `profil` (sans son préampli) aux fréquences données,
/// canal gauche et droit.
pub fn courbe_db(profil: &EqProfile, frequences: &[f64]) -> [Vec<f64>; 2] {
    let mut p = profil.clone();
    p.enabled = true;
    let proc = tune_plugin_equalizer::EqProcessor::new(&p, TAUX_DE_CALCUL, 2);
    let rep = proc.response(TAUX_DE_CALCUL);
    let hz: Vec<f64> = rep["frequency_hz"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    let canal = |c: usize| -> Vec<f64> {
        let db: Vec<f64> = rep["channels_db"][c]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        let preampli = proc.preamp_db(c as u16).unwrap_or(0.0);
        frequences
            .iter()
            .map(|&f| interpoler(&hz, &db, f) - preampli)
            .collect()
    };
    [canal(0), canal(1)]
}

/// Interpolation linéaire en log(f) ; hors bornes, la valeur du bord.
fn interpoler(hz: &[f64], db: &[f64], f: f64) -> f64 {
    if hz.is_empty() || db.len() != hz.len() {
        return 0.0;
    }
    if f <= hz[0] {
        return db[0];
    }
    for i in 1..hz.len() {
        if f <= hz[i] {
            let t = (f.ln() - hz[i - 1].ln()) / (hz[i].ln() - hz[i - 1].ln());
            return db[i - 1] + t * (db[i] - db[i - 1]);
        }
    }
    db[db.len() - 1]
}

fn profil_graphique(
    modele: &EqProfile,
    grille: &[f64],
    q: f64,
    canaux: &[Option<u16>],
    gains: &[Vec<f64>],
) -> EqProfile {
    let mut p = modele.clone();
    p.bands = canaux
        .iter()
        .zip(gains)
        .flat_map(|(canal, g)| {
            grille.iter().zip(g).map(move |(&freq, &gain)| EqBandSpec {
                freq,
                gain,
                q,
                band_type: "peak".into(),
                channel: *canal,
            })
        })
        .collect();
    p
}

/// L'égaliseur graphique (grille de `n` bandes) le plus proche de `profil` :
/// les gains de la grille sont ajustés par itérations jusqu'à ce que la
/// courbe graphique passe par la courbe paramétrique à chaque fréquence de la
/// grille (±12 dB au plus, arrondi à 0,1 dB). Deux courbes (gauche, droite)
/// si le profil traite les canaux différemment, une seule sinon. Les autres
/// champs du profil (activé, réserve, curseurs) sont gardés.
pub fn profil_graphique_au_plus_pres(profil: &EqProfile, n: u32) -> EqProfile {
    if profil.bands.is_empty() || est_courbe_graphique(&profil.bands) {
        return profil.clone();
    }
    let (grille, q) = grille_graphique(n);
    let cible = courbe_db(profil, grille);
    let lie = profil.bands.iter().all(|b| b.channel.is_none())
        || cible[0]
            .iter()
            .zip(&cible[1])
            .all(|(a, b)| (a - b).abs() < 1e-9);
    let canaux: Vec<Option<u16>> = if lie {
        vec![None]
    } else {
        vec![Some(0), Some(1)]
    };
    let borne = |g: f64| g.clamp(-GAIN_GRAPHIQUE_MAX_DB, GAIN_GRAPHIQUE_MAX_DB);
    let mut gains: Vec<Vec<f64>> = canaux
        .iter()
        .map(|c| {
            cible[usize::from(c.unwrap_or(0))]
                .iter()
                .map(|&g| borne(g))
                .collect()
        })
        .collect();
    let mut meilleurs = gains.clone();
    let mut meilleur_ecart = f64::INFINITY;
    for _ in 0..80 {
        let essai = profil_graphique(profil, grille, q, &canaux, &gains);
        let rep = courbe_db(&essai, grille);
        let mut ecart = 0.0_f64;
        for (k, c) in canaux.iter().enumerate() {
            let ch = usize::from(c.unwrap_or(0));
            for i in 0..grille.len() {
                let e = borne(cible[ch][i]) - rep[ch][i];
                ecart = ecart.max(e.abs());
                gains[k][i] = borne(gains[k][i] + 0.7 * e);
            }
        }
        if ecart < meilleur_ecart {
            meilleur_ecart = ecart;
            meilleurs = gains.clone();
        }
        if ecart < 0.01 {
            break;
        }
    }
    let arrondis: Vec<Vec<f64>> = meilleurs
        .iter()
        .map(|g| g.iter().map(|x| (x * 10.0).round() / 10.0 + 0.0).collect())
        .collect();
    profil_graphique(profil, grille, q, &canaux, &arrondis)
}

// ── Chaîne → anciennes clés ─────────────────────────────────────────

/// Ce qu'il faut faire d'une ancienne clé.
#[derive(Debug, Clone, PartialEq)]
enum Ecriture {
    Rien,
    Poser(String),
    Effacer,
}

/// Deux objets de réglages ont-ils le même sens ? `enabled` lu comme
/// aujourd'hui (seul `true` coche).
fn meme_objet(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let norm = |m: &Map<String, Value>| {
        let mut m = m.clone();
        let on = coche(&m);
        m.insert("enabled".into(), Value::Bool(on));
        m
    };
    norm(a) == norm(b)
}

fn objet_courant(brut: Option<&str>) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(brut?) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Projeter un maillon « objet de réglages » (crossfeed, tiers,
/// réaffectation) sur son ancienne clé. Maillon absent : l'ancienne clé est
/// décochée, jamais effacée (ses réglages restent).
fn projeter_objet(
    maillon: Option<&MaillonGreffon>,
    courant: Option<&str>,
    valider: impl Fn(&Map<String, Value>) -> Result<(), String>,
) -> Result<Ecriture, ErreurMigration> {
    let actuel = objet_courant(courant);
    // Groupe crossfeed : écarté par la migration, l'ancienne clé reste
    // cochée (l'ancien moteur appliquait déjà la priorité).
    if maillon.is_some_and(|m| !m.enabled && m.settings.get(MARQUE_REMPLACE).is_some()) {
        return Ok(Ecriture::Rien);
    }
    let voulu = match maillon {
        Some(m) => {
            let mut base = match &m.settings {
                Value::Object(s) => s.clone(),
                Value::Null => actuel.clone().unwrap_or_default(),
                _ => {
                    return Err(ErreurMigration::ReglagesInvalides {
                        id: m.id.clone(),
                        raison: "settings must be an object or null".into(),
                    });
                }
            };
            base.remove(MARQUE_REMPLACE);
            base.insert("enabled".into(), Value::Bool(m.enabled));
            valider(&base).map_err(|raison| ErreurMigration::ReglagesInvalides {
                id: m.id.clone(),
                raison,
            })?;
            base
        }
        None => match &actuel {
            Some(a) if coche(a) => {
                let mut a = a.clone();
                a.insert("enabled".into(), Value::Bool(false));
                a
            }
            _ => return Ok(Ecriture::Rien),
        },
    };
    if actuel.as_ref().is_some_and(|a| meme_objet(a, &voulu)) {
        return Ok(Ecriture::Rien);
    }
    Ok(Ecriture::Poser(Value::Object(voulu).to_string()))
}

fn projeter_egaliseur(
    chaine: &ChaineTess,
    courant: Option<&str>,
) -> Result<Ecriture, ErreurMigration> {
    let actuel = objet_courant(courant);
    let voulu = match egaliseur_principal(chaine).map(|i| &chaine.plugins[i]) {
        Some(m) => {
            let mut base = match &m.settings {
                Value::Object(s) => s.clone(),
                Value::Null => match &actuel {
                    Some(a) => a.clone(),
                    None => match serde_json::to_value(EqProfile::default()) {
                        Ok(Value::Object(d)) => d,
                        _ => Map::new(),
                    },
                },
                _ => {
                    return Err(ErreurMigration::ReglagesInvalides {
                        id: m.id.clone(),
                        raison: "settings must be an object or null".into(),
                    });
                }
            };
            // L'origine d'une correction de pièce (D1) n'appartient qu'à la
            // chaîne.
            base.remove(CHAMP_ORIGINE);
            base.insert("enabled".into(), Value::Bool(m.enabled));
            serde_json::from_value::<EqProfile>(Value::Object(base.clone())).map_err(|e| {
                ErreurMigration::ReglagesInvalides {
                    id: m.id.clone(),
                    raison: format!("not an equalizer profile: {e}"),
                }
            })?;
            base
        }
        None => match &actuel {
            Some(a) if coche(a) => {
                let mut a = a.clone();
                a.insert("enabled".into(), Value::Bool(false));
                a
            }
            _ => return Ok(Ecriture::Rien),
        },
    };
    if actuel.as_ref() == Some(&voulu) {
        return Ok(Ecriture::Rien);
    }
    Ok(Ecriture::Poser(Value::Object(voulu).to_string()))
}

fn projeter_convolution(
    chaine: &ChaineTess,
    courant: Option<&str>,
) -> Result<Ecriture, ErreurMigration> {
    let voulu = match chaine.plugins.iter().find(|m| m.id == ID_CONVOLUTION) {
        Some(m) if m.enabled => match m.settings.get("ir_path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => Some(p.to_owned()),
            _ => {
                return Err(ErreurMigration::ReglagesInvalides {
                    id: m.id.clone(),
                    raison: "settings.ir_path is missing".into(),
                });
            }
        },
        // Décochée ou retirée : la convolution n'a pas d'état « décoché »
        // dans les anciennes clés ; le chemin reste dans la chaîne.
        _ => None,
    };
    Ok(match (voulu, courant) {
        (Some(v), Some(c)) if v == c => Ecriture::Rien,
        (Some(v), _) => Ecriture::Poser(v),
        // Un chemin vide ne désignait déjà rien : on n'y touche pas.
        (None, Some(c)) if c.trim().is_empty() => Ecriture::Rien,
        (None, Some(_)) => Ecriture::Effacer,
        (None, None) => Ecriture::Rien,
    })
}

fn projeter_mono(chaine: &ChaineTess, courant: Option<&str>) -> Ecriture {
    let maillon = chaine.plugins.iter().find(|m| m.id == ID_MONO);
    // D2 : décoché parce qu'inerte sur une sortie réseau — l'ancienne clé
    // reste ce qu'elle est.
    if maillon.is_some_and(|m| {
        !m.enabled && m.settings.get(MARQUE_MONO_INERTE).and_then(Value::as_bool) == Some(true)
    }) {
        return Ecriture::Rien;
    }
    let voulu = maillon.is_some_and(|m| m.enabled);
    match (voulu, courant) {
        (true, Some("true")) => Ecriture::Rien,
        (true, _) => Ecriture::Poser("true".into()),
        (false, Some("true")) => Ecriture::Effacer,
        (false, _) => Ecriture::Rien,
    }
}

fn projeter_surechantillonnage(chaine: &ChaineTess, courant: Option<&str>) -> Ecriture {
    let actuel: Option<UpsamplerSettings> = courant
        .and_then(|b| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| crate::audio::surechantillonnage::greffon::settings(&v).ok());
    let voulu = match &chaine.fixed.upsampling {
        Upsampling::Pcm {
            filter,
            max_ratio,
            max_rate_hz,
        } => UpsamplerSettings {
            enabled: true,
            filter: filter.clone(),
            max_rate_hz: *max_rate_hz,
            max_ratio: *max_ratio,
        },
        // Le suréchantillonneur d'aujourd'hui ne fait pas de DSD : pour les
        // anciennes routes, il est éteint ; ses autres réglages restent.
        Upsampling::Off {} | Upsampling::Dsd { .. } => match &actuel {
            Some(a) => UpsamplerSettings {
                enabled: false,
                ..a.clone()
            },
            None if courant.is_none() => return Ecriture::Rien,
            None => UpsamplerSettings::default(),
        },
        Upsampling::Unknown => return Ecriture::Rien,
    };
    if actuel.as_ref() == Some(&voulu) {
        return Ecriture::Rien;
    }
    match serde_json::to_string(&voulu) {
        Ok(s) => Ecriture::Poser(s),
        Err(_) => Ecriture::Rien,
    }
}

fn projeter(
    partie: &Partie,
    chaine: &ChaineTess,
    courant: Option<&str>,
) -> Result<Ecriture, ErreurMigration> {
    let trouver = |id: &str| chaine.plugins.iter().find(|m| m.id == id);
    match partie {
        Partie::Egaliseur => projeter_egaliseur(chaine, courant),
        Partie::Convolution => projeter_convolution(chaine, courant),
        Partie::Crossfeed => projeter_objet(trouver(ID_CROSSFEED), courant, |m| {
            serde_json::from_value::<tune_plugin_crossfeed::CrossfeedSettings>(Value::Object(
                m.clone(),
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())
        }),
        Partie::Natif(id) => projeter_objet(trouver(id), courant, |m| {
            let taille = Value::Object(m.clone()).to_string().len();
            if taille > crate::audio::natifs_tiers::REGLAGE_MAX_OCTETS {
                Err(format!("settings are {taille} bytes, over the limit"))
            } else {
                Ok(())
            }
        }),
        Partie::Reaffectation => projeter_objet(trouver(ID_REAFFECTATION), courant, |m| {
            serde_json::from_value::<crate::audio::reaffectation_canaux::ChannelRemapSettings>(
                Value::Object(m.clone()),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        }),
        Partie::Mono => Ok(projeter_mono(chaine, courant)),
        Partie::Surechantillonnage => Ok(projeter_surechantillonnage(chaine, courant)),
    }
}

/// Ce qu'une écriture a changé.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BilanEcriture {
    /// Les anciennes clés réécrites ou effacées. La route qui écrit (T08)
    /// rafraîchit la sortie vivante pour chacune, comme les anciennes routes
    /// (`REGLAGES_A_RAFRAICHIR`).
    pub cles_modifiees: Vec<String>,
    /// La chaîne enregistrée a changé.
    pub chaine_ecrite: bool,
}

/// Enregistre `chaine` pour la zone et met à jour les anciennes clés qu'elle
/// exprime (écriture croisée), sous [`verrou_de_zone`]. Aucune ancienne clé
/// n'est réécrite si son sens ne change pas. Rien n'est validé contre le
/// catalogue ici : le `PUT` passe par `ChaineTess::depuis_corps` puis
/// `valider` (T08) avant d'arriver là.
///
/// Les réglages de chaque maillon sont vérifiés AVANT toute écriture : un
/// refus ne laisse aucune clé à moitié écrite.
pub fn ecrire_chaine(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
) -> Result<BilanEcriture, ErreurMigration> {
    ecrire_chaine_avec(settings, zone_id, chaine, || {})
}

/// [`ecrire_chaine`], avec `entre` appelé après les écritures des anciennes
/// clés et avant celle de la chaîne et de l'ombre (sert à éprouver la course
/// avec une ancienne route).
fn ecrire_chaine_avec(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
    entre: impl FnOnce(),
) -> Result<BilanEcriture, ErreurMigration> {
    if chaine.version > VERSION_CHAINE {
        return Err(ErreurMigration::VersionPlusRecente {
            trouvee: chaine.version,
        });
    }
    let verrou = verrou_de_zone(zone_id);
    let _garde = verrou.lock().unwrap_or_else(|e| e.into_inner());
    // D1 : l'origine d'une correction de pièce est décidée par le serveur,
    // jamais par le corps d'un `PUT`.
    let chaine = &origines_par_le_serveur(settings, zone_id, chaine);
    // Groupe crossfeed et D2 : les marques de la lecture appartiennent au
    // serveur (la chaîne qu'il lit, migration à la lecture comprise).
    let reference = match lire_zone(settings, zone_id, true) {
        Ok(l) => Some(l.chaine),
        Err(e) => {
            // Sans chaîne de référence, toutes les marques du corps sont
            // retirées (repli sûr) ; on le dit.
            tracing::warn!(zone_id, erreur = %e, "tess_ecriture_chaine_de_reference_illisible");
            None
        }
    };
    let chaine = &marques_par_le_serveur(reference.as_ref(), chaine);

    let mut inst = lire_instantane(settings, zone_id)?;
    let mut parties: Vec<Partie> = inst.keys().cloned().collect();
    for m in &chaine.plugins {
        let p = Partie::Natif(m.id.clone());
        if est_natif(&m.id) && !parties.contains(&p) {
            parties.push(p);
        }
    }
    let mut plan = Vec::new();
    for p in &parties {
        let courant = inst.get(p).cloned().flatten();
        match projeter(p, chaine, courant.as_deref())? {
            Ecriture::Rien => {}
            e => plan.push((p.clone(), courant, e)),
        }
    }

    let mut bilan = BilanEcriture::default();
    for (partie, courant, e) in plan {
        let cle = partie.cle(zone_id);
        if partie == Partie::Egaliseur
            && let Some(avant) = &courant
            && stockage(settings.get(&cle_sauvegarde_eq(zone_id)))?.is_none()
        {
            stockage(settings.set(&cle_sauvegarde_eq(zone_id), avant))?;
        }
        // L'ombre est ce qui a été lu, puis ce qui vient d'être écrit :
        // jamais relue, pour qu'une ancienne route qui écrit maintenant
        // reste visible à la prochaine lecture.
        match e {
            Ecriture::Poser(v) => {
                stockage(settings.set(&cle, &v))?;
                inst.insert(partie, Some(v));
            }
            Ecriture::Effacer => {
                stockage(settings.delete(&cle))?;
                inst.insert(partie, None);
            }
            Ecriture::Rien => continue,
        }
        bilan.cles_modifiees.push(cle);
    }

    entre();
    bilan.chaine_ecrite = enregistrer(
        settings,
        zone_id,
        &chaine.clone().mettre_a_niveau(),
        &ombre_de(&inst, sortie_locale_de(settings, zone_id)?),
    )?;
    Ok(bilan)
}

/// Écrit la chaîne et l'ombre si elles diffèrent de ce qui est enregistré.
fn enregistrer(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
    ombre: &Ombre,
) -> Result<bool, ErreurMigration> {
    let texte =
        serde_json::to_string(chaine).map_err(|e| ErreurMigration::Stockage(e.to_string()))?;
    let texte_ombre =
        serde_json::to_string(ombre).map_err(|e| ErreurMigration::Stockage(e.to_string()))?;
    let cle = super::cle_chaine_zone(zone_id);
    let mut ecrite = false;
    if stockage(settings.get(&cle))?.as_deref() != Some(texte.as_str()) {
        stockage(settings.set(&cle, &texte))?;
        ecrite = true;
    }
    let cle_ombre = cle_ombre_zone(zone_id);
    if stockage(settings.get(&cle_ombre))?.as_deref() != Some(texte_ombre.as_str()) {
        stockage(settings.set(&cle_ombre, &texte_ombre))?;
        ecrite = true;
    }
    Ok(ecrite)
}

// ── Migration ───────────────────────────────────────────────────────

/// Ce que la migration d'une zone a fait.
#[derive(Debug, Clone, PartialEq)]
pub struct BilanMigration {
    pub zone_id: i64,
    /// La chaîne ENREGISTRÉE, neutre vis-à-vis du droit (sans P2).
    pub chaine: ChaineTess,
    /// La chaîne ou l'ombre ont été (ré)écrites ; `false` = déjà à jour.
    pub ecrite: bool,
}

/// Migre une zone : enregistre la chaîne lue ([`lire_zone`]) et l'ombre,
/// sous [`verrou_de_zone`]. N'écrit JAMAIS une ancienne clé : la lecture
/// d'aujourd'hui ne change pas. Ne dépend pas du droit : P2 n'est jamais
/// enregistrée. Idempotente. Une chaîne d'une version plus récente n'est pas
/// touchée.
pub fn migrer_zone(
    settings: &SettingsRepo,
    zone_id: i64,
) -> Result<BilanMigration, ErreurMigration> {
    let verrou = verrou_de_zone(zone_id);
    let _garde = verrou.lock().unwrap_or_else(|e| e.into_inner());
    let lecture = lire_zone(settings, zone_id, true)?;
    let ecrite = if lecture.chaine.version > VERSION_CHAINE {
        false
    } else {
        enregistrer(settings, zone_id, &lecture.chaine, &lecture.ombre)?
    };
    Ok(BilanMigration {
        zone_id,
        chaine: lecture.chaine,
        ecrite,
    })
}

/// La migration de chaque zone : son id et son bilan, ou son refus.
pub type MigrationDesZones = Vec<(i64, Result<BilanMigration, ErreurMigration>)>;

/// Migre chaque zone de la base. Une zone refusée (clé corrompue) n'arrête
/// pas les autres : son refus est rendu avec elle.
pub fn migrer_toutes_les_zones(
    backend: &Arc<dyn DbBackend>,
) -> Result<MigrationDesZones, ErreurMigration> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let zones = stockage(crate::db::zone_repo::ZoneRepo::with_backend(backend.clone()).list())?;
    Ok(zones
        .into_iter()
        .filter_map(|z| z.id)
        .map(|id| (id, migrer_zone(&settings, id)))
        .collect())
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
