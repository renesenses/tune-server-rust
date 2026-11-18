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
//! « Modifiée depuis la dernière synchronisation » se lit dans l'**ombre**
//! (`zone_{id}_tess_chain_sync`, [`cle_ombre_zone`]) : la valeur brute de
//! chaque ancienne clé au moment où la chaîne a été écrite. C'est ce qui
//! garde ce que les anciennes clés ne savent pas dire (ordre, greffon décoché,
//! Headphone EQ, upsampling DSD, headroom manuel, dither, `bypass_all`) sans
//! jamais rater une écriture faite par une ancienne route.
//!
//! Une clé illisible donne un **refus explicite** ([`ErreurMigration::CleCorrompue`]),
//! jamais une chaîne vide.
//!
//! # Correspondance
//!
//! | Ancienne clé | Chaîne |
//! |---|---|
//! | `zone_{id}_eq_profile` | une façade d'égaliseur (voir [`classer_egaliseur`]), `enabled` = `enabled` du profil, `settings` = le profil sans `enabled` |
//! | `ir_path_{id}` | `convolution`, coché, `settings.ir_path` |
//! | `zone_{id}_crossfeed` | `crossfeed`, `enabled` = `enabled`, `settings` = le reste |
//! | `zone_{id}_native_plugin_{p}` | greffon `{p}` (Crossfeed Pro, Crosstalk, paquets tiers), idem |
//! | `zone_{id}_mono_downmix` | `mono-downmix` (greffon dédié, décision T02), coché si `"true"` |
//! | `zone_{id}_channel_remap` | `channel-remap`, en dernier, idem |
//! | `zone_{id}_upsampler` | `fixed.upsampling` : `pcm` avec `filter`, `max_ratio`, `max_rate_hz` si `enabled`, sinon `off` |
//! | `zone_{id}_audiophile`, `zone_{id}_strict_bitperfect` | hors chaîne : [`LectureZone::pure`] (`ContexteExecution.pure`) |
//! | `zone_{id}_gain_trim_db` | hors chaîne : reste le gain de zone, porté par l'étage headroom (T07) |
//!
//! Ordre d'une chaîne tirée des anciennes clés : celui de la lecture
//! d'aujourd'hui (égaliseur, convolution, crossfeed, étages tiers, repli mono,
//! réaffectation en dernier).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::chaine::{ChaineTess, MaillonGreffon, Upsampling, VERSION_CHAINE};
use super::greffon::{Origine, catalogue_de_reference, id_valide};
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

fn stockage<T>(r: Result<T, String>) -> Result<T, ErreurMigration> {
    r.map_err(ErreurMigration::Stockage)
}

fn lire_instantane(settings: &SettingsRepo, zone_id: i64) -> Result<Instantane, ErreurMigration> {
    let mut inst = Instantane::new();
    for p in Partie::FIXES {
        let v = stockage(settings.get(&p.cle(zone_id)))?;
        inst.insert(p, v);
    }
    let prefixe = format!("zone_{zone_id}_native_plugin_");
    for (cle, valeur) in stockage(settings.all())? {
        if let Some(id) = cle.strip_prefix(&prefixe)
            && !id.is_empty()
        {
            inst.insert(Partie::Natif(id.to_owned()), Some(valeur));
        }
    }
    Ok(inst)
}

/// Un objet JSON, ou un refus qui nomme la clé.
fn objet(cle: &str, brut: &str) -> Result<Map<String, Value>, ErreurMigration> {
    match serde_json::from_str::<Value>(brut) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(corrompue(cle, "not a JSON object")),
        Err(e) => Err(corrompue(cle, format!("invalid JSON: {e}"))),
    }
}

/// `enabled` d'un objet de réglages : absent = faux ; autre chose qu'un
/// booléen = clé corrompue.
fn coche(cle: &str, m: &Map<String, Value>) -> Result<bool, ErreurMigration> {
    match m.get("enabled") {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(corrompue(cle, "\"enabled\" is not a boolean")),
    }
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
        Partie::Crossfeed | Partie::Natif(_) => {
            let m = objet(cle, brut)?;
            coche(cle, &m)?;
            if matches!(partie, Partie::Crossfeed) {
                serde_json::from_value::<tune_plugin_crossfeed::CrossfeedSettings>(Value::Object(
                    m.clone(),
                ))
                .map_err(|e| corrompue(cle, format!("not crossfeed settings: {e}")))?;
            }
            Decodee::Objet(m)
        }
        Partie::Reaffectation => {
            let m = objet(cle, brut)?;
            coche(cle, &m)?;
            serde_json::from_value::<crate::audio::reaffectation_canaux::ChannelRemapSettings>(
                Value::Object(m.clone()),
            )
            .map_err(|e| corrompue(cle, format!("not channel remap settings: {e}")))?;
            Decodee::Objet(m)
        }
        Partie::Convolution => {
            if brut.trim().is_empty() {
                return Err(corrompue(cle, "empty impulse response path"));
            }
            Decodee::Chemin(brut.to_owned())
        }
        Partie::Mono => match brut {
            "true" => Decodee::Mono(true),
            "false" => Decodee::Mono(false),
            autre => {
                return Err(corrompue(
                    cle,
                    format!("expected \"true\" or \"false\", found {autre:?}"),
                ));
            }
        },
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
fn profil_de_piece(settings: &SettingsRepo, zone_id: i64) -> Option<EqProfile> {
    let brut = settings
        .get(&format!("room_profile_{zone_id}"))
        .ok()
        .flatten()?;
    let profil: crate::room_correction::RoomProfile = serde_json::from_str(&brut).ok()?;
    if profil.filters.is_empty() {
        return None;
    }
    Some(crate::room_correction::profil_eq_de_correction(
        &profil.filters,
    ))
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

fn appliquer(chaine: &mut ChaineTess, partie: &Partie, valeur: Decodee, piece: Option<&EqProfile>) {
    match (partie, valeur) {
        (Partie::Egaliseur, Decodee::Egaliseur(m, profil)) => {
            let principal = egaliseur_principal(chaine);
            let precedente = principal.map(|i| chaine.plugins[i].id.clone());
            let id = classer_egaliseur(&profil, piece, precedente.as_deref());
            let maillon = MaillonGreffon {
                id: id.to_owned(),
                enabled: profil.enabled,
                settings: sans_enabled(m),
            };
            match principal {
                Some(i) => {
                    // Un autre maillon portait déjà cet id : l'ancienne clé
                    // l'emporte, il n'en reste qu'un.
                    if chaine.plugins[i].id != id {
                        let doublon = chaine.plugins.iter().position(|x| x.id == id);
                        chaine.plugins[i] = maillon;
                        if let Some(j) = doublon {
                            chaine.plugins.remove(j);
                        }
                    } else {
                        chaine.plugins[i] = maillon;
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
            let on = m.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            poser(chaine, ID_CROSSFEED, on, sans_enabled(m));
        }
        (Partie::Crossfeed, _) => retirer(chaine, ID_CROSSFEED),
        (Partie::Natif(id), Decodee::Objet(m)) => {
            let on = m.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            poser(chaine, id, on, sans_enabled(m));
        }
        (Partie::Natif(id), _) => retirer(chaine, id),
        (Partie::Reaffectation, Decodee::Objet(m)) => {
            let on = m.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            poser(chaine, ID_REAFFECTATION, on, sans_enabled(m));
        }
        (Partie::Reaffectation, _) => retirer(chaine, ID_REAFFECTATION),
        (Partie::Mono, Decodee::Mono(true)) => poser(chaine, ID_MONO, true, Value::Null),
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
    /// Aucune chaîne n'était enregistrée : elle vient des anciennes clés.
    pub depuis_les_anciennes_cles: bool,
    /// Un égaliseur paramétrique coché a été converti en graphique (P2).
    pub conversion_p2: bool,
    /// Valeur brute des anciennes clés à cet instant (l'ombre à écrire).
    ombre: BTreeMap<String, Option<String>>,
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

fn lire_ombre(
    settings: &SettingsRepo,
    zone_id: i64,
) -> Result<Option<BTreeMap<String, Option<String>>>, ErreurMigration> {
    let cle = cle_ombre_zone(zone_id);
    let Some(brut) = stockage(settings.get(&cle))? else {
        return Ok(None);
    };
    serde_json::from_str(&brut)
        .map(Some)
        .map_err(|e| corrompue(&cle, format!("invalid sync record: {e}")))
}

fn ombre_de(inst: &Instantane) -> BTreeMap<String, Option<String>> {
    inst.iter().map(|(p, v)| (p.nom(), v.clone())).collect()
}

fn lire_pure(settings: &SettingsRepo, zone_id: i64) -> Result<bool, ErreurMigration> {
    let cle = format!("zone_{zone_id}_audiophile");
    let audiophile = match stockage(settings.get(&cle))? {
        None => false,
        Some(brut) => coche(&cle, &objet(&cle, &brut)?)?,
    };
    let cle = crate::audio::bitperfect_strict::cle_de_zone(zone_id);
    let strict = match stockage(settings.get(&cle))?.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(autre) => {
            return Err(corrompue(
                &cle,
                format!("expected \"true\" or \"false\", found {autre:?}"),
            ));
        }
    };
    Ok(audiophile || strict)
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
/// égaliseur graphique au plus près (décision P2) ; décoché, il est gardé tel
/// quel (un greffon décoché ne demande aucun droit).
///
/// Une chaîne enregistrée par une version plus récente est rendue telle
/// quelle, sans y réappliquer les anciennes clés (la résolution l'éteint,
/// `chain_version_unsupported`).
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

    // Chaque ancienne clé est décodée, qu'elle ait changé ou non : une clé
    // illisible refuse toujours la zone.
    let mut decodees = Vec::with_capacity(inst.len());
    for (partie, brut) in &inst {
        let cle = partie.cle(zone_id);
        decodees.push((partie.clone(), decoder(partie, &cle, brut.as_deref())?));
    }

    if let Some(c) = &stockee
        && c.version > VERSION_CHAINE
    {
        return Ok(LectureZone {
            chaine: c.clone(),
            pure,
            depuis_les_anciennes_cles: false,
            conversion_p2: false,
            ombre: ombre.unwrap_or_default(),
        });
    }

    let depuis_les_anciennes_cles = stockee.is_none();
    // Sans ombre, toute clé compte comme modifiée (chaîne posée par une
    // version sans ombre, ou ombre perdue) : les anciennes clés l'emportent.
    let (mut chaine, ombre_lue) = match stockee {
        Some(c) => (c.mettre_a_niveau(), ombre.unwrap_or_default()),
        None => (ChaineTess::default(), BTreeMap::new()),
    };

    let piece = profil_de_piece(settings, zone_id);
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
        let inchangee = ombre_lue.get(&partie.nom()) == Some(&brut);
        if depuis_les_anciennes_cles || !inchangee {
            appliquer(&mut chaine, &partie, valeur, piece.as_ref());
        }
    }

    let conversion_p2 = !premium && convertir_p2(&mut chaine, grille_reglee(settings));
    Ok(LectureZone {
        chaine,
        pure,
        depuis_les_anciennes_cles,
        conversion_p2,
        ombre: ombre_de(&inst),
    })
}

// ── P2 : paramétrique → graphique ───────────────────────────────────

/// P2 : l'égaliseur paramétrique COCHÉ d'un compte gratuit devient un
/// égaliseur graphique au plus près, sur la grille réglée. Rend `true` si la
/// chaîne a changé. Rien ne change si la chaîne a déjà un égaliseur graphique
/// (deux maillons ne portent jamais le même id).
pub fn convertir_p2(chaine: &mut ChaineTess, grille: u32) -> bool {
    if chaine.plugins.iter().any(|m| m.id == ID_EQ_GRAPHIQUE) {
        return false;
    }
    let Some(m) = chaine
        .plugins
        .iter_mut()
        .find(|m| m.id == ID_EQ_PARAMETRIQUE && m.enabled)
    else {
        return false;
    };
    let profil = match &m.settings {
        Value::Object(s) => {
            let mut s = s.clone();
            s.insert("enabled".into(), Value::Bool(true));
            serde_json::from_value::<EqProfile>(Value::Object(s)).ok()
        }
        _ => None,
    };
    m.id = ID_EQ_GRAPHIQUE.to_owned();
    if let Some(p) = profil {
        let converti = profil_graphique_au_plus_pres(&p, grille);
        if let Ok(Value::Object(s)) = serde_json::to_value(&converti) {
            m.settings = sans_enabled(s);
        }
    }
    true
}

/// Fréquence d'échantillonnage du calcul de courbe.
const TAUX_DE_CALCUL: u32 = 48_000;
/// Gain maximal d'un curseur graphique, en dB (client web : ±12).
const GAIN_GRAPHIQUE_MAX_DB: f64 = 12.0;

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

/// Deux objets de réglages ont-ils le même sens ? `enabled` absent = faux.
fn meme_objet(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let norm = |m: &Map<String, Value>| {
        let mut m = m.clone();
        let on = m.get("enabled").and_then(Value::as_bool).unwrap_or(false);
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
            base.insert("enabled".into(), Value::Bool(m.enabled));
            valider(&base).map_err(|raison| ErreurMigration::ReglagesInvalides {
                id: m.id.clone(),
                raison,
            })?;
            base
        }
        None => match &actuel {
            Some(a) if a.get("enabled").and_then(Value::as_bool) == Some(true) => {
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
            Some(a) if a.get("enabled").and_then(Value::as_bool) == Some(true) => {
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
        (None, Some(_)) => Ecriture::Effacer,
        (None, None) => Ecriture::Rien,
    })
}

fn projeter_mono(chaine: &ChaineTess, courant: Option<&str>) -> Ecriture {
    let voulu = chaine.plugins.iter().any(|m| m.id == ID_MONO && m.enabled);
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
/// exprime (écriture croisée). Aucune ancienne clé n'est réécrite si son sens
/// ne change pas. Rien n'est validé contre le catalogue ici : le `PUT` passe
/// par `ChaineTess::depuis_corps` puis `valider` (T08) avant d'arriver là.
///
/// Les réglages de chaque maillon sont vérifiés AVANT toute écriture : un
/// refus ne laisse aucune clé à moitié écrite.
pub fn ecrire_chaine(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
) -> Result<BilanEcriture, ErreurMigration> {
    if chaine.version > VERSION_CHAINE {
        return Err(ErreurMigration::VersionPlusRecente {
            trouvee: chaine.version,
        });
    }
    let inst = lire_instantane(settings, zone_id)?;
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
        match e {
            Ecriture::Poser(v) => stockage(settings.set(&cle, &v))?,
            Ecriture::Effacer => stockage(settings.delete(&cle))?,
            Ecriture::Rien => continue,
        }
        bilan.cles_modifiees.push(cle);
    }

    let apres = lire_instantane(settings, zone_id)?;
    bilan.chaine_ecrite = enregistrer(
        settings,
        zone_id,
        &chaine.clone().mettre_a_niveau(),
        &ombre_de(&apres),
    )?;
    Ok(bilan)
}

/// Écrit la chaîne et l'ombre si elles diffèrent de ce qui est enregistré.
fn enregistrer(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine: &ChaineTess,
    ombre: &BTreeMap<String, Option<String>>,
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
    pub chaine: ChaineTess,
    /// La chaîne ou l'ombre ont été (ré)écrites ; `false` = déjà à jour.
    pub ecrite: bool,
    pub conversion_p2: bool,
}

/// Migre une zone : enregistre la chaîne lue ([`lire_zone`]) et l'ombre.
/// N'écrit JAMAIS une ancienne clé : la lecture d'aujourd'hui ne change pas.
/// Idempotente. Une chaîne d'une version plus récente n'est pas touchée.
pub fn migrer_zone(
    settings: &SettingsRepo,
    zone_id: i64,
    premium: bool,
) -> Result<BilanMigration, ErreurMigration> {
    let lecture = lire_zone(settings, zone_id, premium)?;
    let ecrite = if lecture.chaine.version > VERSION_CHAINE {
        false
    } else {
        enregistrer(settings, zone_id, &lecture.chaine, &lecture.ombre)?
    };
    Ok(BilanMigration {
        zone_id,
        chaine: lecture.chaine,
        ecrite,
        conversion_p2: lecture.conversion_p2,
    })
}

/// La migration de chaque zone : son id et son bilan, ou son refus.
pub type MigrationDesZones = Vec<(i64, Result<BilanMigration, ErreurMigration>)>;

/// Migre chaque zone de la base. Une zone refusée (clé corrompue) n'arrête
/// pas les autres : son refus est rendu avec elle.
pub fn migrer_toutes_les_zones(
    backend: &Arc<dyn DbBackend>,
    premium: bool,
) -> Result<MigrationDesZones, ErreurMigration> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let zones = stockage(crate::db::zone_repo::ZoneRepo::with_backend(backend.clone()).list())?;
    Ok(zones
        .into_iter()
        .filter_map(|z| z.id)
        .map(|id| (id, migrer_zone(&settings, id, premium)))
        .collect())
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
