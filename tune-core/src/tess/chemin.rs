//! TESS T16 — le **chemin du signal** (maquette A4) : les étapes TESS
//! nommées, avec leurs paramètres, dans l'ordre RÉEL, et le badge.
//!
//! # Ne ment jamais
//!
//! La [`Resolution`] dit ce que la chaîne DEMANDE et ce qui devrait
//! s'appliquer. Ce module la **rapproche** de ce qui tourne réellement
//! ([`EnService`]) et c'est le réel qui l'emporte, dans les deux sens :
//!
//! - une étape résolue effective qui ne tourne pas est éteinte
//!   (`not_in_service`) ;
//! - un étage qui tourne sans que la résolution l'attende est publié
//!   effectif, quitte à ajouter une étape que la chaîne ne nomme pas ;
//! - chaque écart est publié (`discrepancies`) et journalisé par l'appelant.
//!
//! Le badge se déduit des étapes RAPPROCHÉES : « Bit-perfect » seulement si
//! rien ne modifie les échantillons (volume compris : en Pure, le volume
//! logiciel reste effectif sauf verrou, décision T07, et le badge n'est
//! « Bit-perfect » qu'à 0 dB) **et** si le reste du chemin est intact
//! (`chemin_intact`) ; sinon `altered_outside_tess`.
//!
//! # Ce qui tourne
//!
//! - [`GreffonsEnService::Exacts`] : l'exécuteur TESS (T03, T04) publie la
//!   liste exacte des étages qu'il appelle, dans l'ordre où il les appelle.
//!   Cet ordre fait foi pour le chemin du signal.
//! - [`GreffonsEnService::Sonde`] : avant le branchement de l'exécuteur, la
//!   sortie ne sait dire que « un traitement a touché les échantillons » (sonde
//!   `dsp` de la sortie locale, octets du flux réseau). Les greffons effectifs
//!   de la résolution sont alors publiés dans l'ordre que les moteurs
//!   historiques appliquent ([`rang_historique`] : égaliseur, convolution,
//!   crossfeed, tiers, repli mono, réaffectation), et ceux que la sonde sait
//!   éteints (`hors_service`) le sont.
//!
//! # Ordre (Q9)
//!
//! `Source → Headroom → greffons (ordre réel) → Upsampling → Volume →
//! Quantization (« Dither ») → Output` ; vers DSD, `… → Volume →
//! DsdModulator → Output`.
//!
//! # Niveau (T07)
//!
//! [`NiveauPublie`] porte `absorbe_par_les_greffons_db` (part du headroom
//! automatique déjà couverte par la réserve des greffons) et `rabote_db` (dB
//! que le rabot à l'unité n'a pas rendus), publiés en JSON
//! `absorbed_by_plugins_db` et `unrendered_db` (#5069), sur les étapes
//! Headroom et Volume et dans `level`. Seul un étage EN SERVICE les fournit :
//! jamais un calcul de ce que TESS ferait.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::chaine::{ChaineTess, Dither, Upsampling};
use super::etages_fixes::{BilanDeNiveau, GainDeRendu};
use super::greffon::Catalogue;
use super::migration::MARQUE_MONO_INERTE;
use super::resolution::{
    Badge, CodeEtape, EtapeResolue, EtatVolume, MotifEtape, PlacementVolume, Resolution,
};

/// Les greffons réellement en service.
#[derive(Debug, Clone, PartialEq)]
pub enum GreffonsEnService {
    /// Publiés par l'exécuteur TESS : les ids, dans l'ordre d'appel.
    Exacts(Vec<String>),
    /// Sonde globale d'une sortie d'avant l'exécuteur : `actif` dit si un
    /// traitement a touché les échantillons ; `hors_service` nomme les
    /// greffons que la sortie sait éteints (RI à la mauvaise cadence…).
    Sonde {
        actif: bool,
        hors_service: Vec<String>,
    },
}

/// Ce qui tourne réellement sur la zone.
#[derive(Debug, Clone, PartialEq)]
pub struct EnService {
    pub greffons: GreffonsEnService,
    /// Gain de l'étage Headroom de TESS réellement appliqué (dB ≤ 0) ;
    /// `None` = étage absent.
    pub headroom_db: Option<f64>,
    /// Le suréchantillonneur porte la cadence.
    pub upsampling: bool,
    /// Un volume logiciel différent de l'unité multiplie les échantillons.
    pub volume_logiciel: bool,
    /// Latence de chaque greffon préparé, en trames (id du greffon).
    pub latences: Vec<(String, u32)>,
}

/// T07 — le bilan de niveau d'un étage en service, tel qu'il est publié.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct NiveauPublie {
    /// `BilanDeNiveau::absorbe_par_les_greffons_db` (≥ 0).
    pub absorbed_by_plugins_db: f64,
    /// `GainDeRendu::rabote_db` (≥ 0), le `unrendered_db` de #5069.
    pub unrendered_db: f64,
}

impl NiveauPublie {
    pub fn depuis(bilan: &BilanDeNiveau, rendu: &GainDeRendu) -> Self {
        Self {
            absorbed_by_plugins_db: bilan.absorbe_par_les_greffons_db,
            unrendered_db: rendu.rabote_db,
        }
    }
}

/// Une étape du chemin, avec son nom d'affichage (anglais, traduit par les
/// clients).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtapeNommee {
    pub name: String,
    #[serde(flatten)]
    pub etape: EtapeResolue,
}

/// Le sens d'un écart entre la résolution et le réel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensEcart {
    /// Résolu effectif, mais rien ne tourne.
    RequestedNotRunning,
    /// Tourne, alors que la résolution ne l'attend pas.
    RunningNotRequested,
    #[serde(other)]
    Unknown,
}

/// Un écart constaté (publié dans `discrepancies`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ecart {
    pub code: CodeEtape,
    pub plugin_id: Option<String>,
    pub kind: SensEcart,
}

/// Le bloc `tess` du chemin du signal (maquette A4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheminTess {
    pub badge: Badge,
    /// Headroom réellement appliqué (0 sans étage en service).
    pub headroom_db: f64,
    pub volume: EtatVolume,
    /// Bilan de niveau (T07) ; `None` tant qu'aucun étage ne le publie.
    pub level: Option<NiveauPublie>,
    pub steps: Vec<EtapeNommee>,
    pub discrepancies: Vec<Ecart>,
}

impl CheminTess {
    /// Les étapes effectives, dans l'ordre.
    pub fn effectives(&self) -> impl Iterator<Item = &EtapeNommee> {
        self.steps.iter().filter(|e| e.etape.effective)
    }
}

/// Détail d'une étape éteinte parce que rien ne tourne.
pub const DETAIL_PAS_EN_SERVICE: &str = "Requested but not running";
/// Détail d'une étape qui tourne sans être attendue.
pub const DETAIL_HORS_RESOLUTION: &str = "Running although the chain does not ask for it";
/// Détail d'un traitement que la sonde voit sans pouvoir l'attribuer.
pub const DETAIL_NON_ATTRIBUE: &str = "Processing not attributed to a plugin";
/// Détail d'un greffon décoché parce qu'il n'agit pas sur une sortie réseau
/// (marque `inert_on_network` de T02, décision D2).
pub const DETAIL_INERTE_RESEAU: &str = "No effect on a network output";

/// Rang d'un greffon dans l'ordre des moteurs d'avant l'exécuteur
/// (`apply_local_dsp`, `StreamingDsp::process`) : égaliseur, convolution,
/// crossfeed, tiers, repli mono, réaffectation.
pub fn rang_historique(id: &str, settings: &Value) -> u8 {
    match id {
        "equalizer-graphic" | "equalizer-parametric" | "equalizer-headphone" => 0,
        // Une correction mesurée sans RI est l'égaliseur de pièce (T02, D1).
        "room-correction" if settings.get("ir_path").is_none() => 0,
        "convolution" | "room-correction" => 1,
        "crossfeed" | "crosstalk" => 2,
        _ if id.starts_with("crossfeed-") => 2,
        "mono-downmix" => 4,
        "channel-remap" => 5,
        _ => 3,
    }
}

fn nom_fixe(code: CodeEtape) -> &'static str {
    match code {
        CodeEtape::Source => "Source",
        CodeEtape::Headroom => "Headroom",
        CodeEtape::Plugin => "Plugin",
        CodeEtape::Upsampling => "Upsampling",
        CodeEtape::Volume => "Volume control",
        // A4 corrigée (Q9) : une ligne « Dither » finale en PCM.
        CodeEtape::Quantization => "Dither",
        CodeEtape::DsdModulator => "DSD modulator",
        CodeEtape::Output => "Output",
        CodeEtape::Unknown => "Unknown",
    }
}

fn texte(v: &Value, cles: &[&str]) -> Option<String> {
    cles.iter()
        .find_map(|c| v.get(*c).and_then(Value::as_str))
        .map(str::to_owned)
}

fn nom_de_fichier(chemin: &str) -> String {
    chemin
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(chemin)
        .to_owned()
}

/// Les paramètres propres à un greffon (maquette A4 : « room_96k.wav »,
/// « 31 bands · Jazz »), lus dans ses réglages. Rien n'est inventé : une clé
/// absente n'est pas publiée.
pub fn params_du_greffon(id: &str, settings: &Value) -> Map<String, Value> {
    let mut p = Map::new();
    match id {
        "equalizer-graphic" | "equalizer-parametric" | "equalizer-headphone" => {
            if let Some(n) = settings
                .get("bands")
                .and_then(Value::as_array)
                .map(Vec::len)
            {
                p.insert("bands".into(), json!(n));
            }
            if let Some(n) = texte(settings, &["preset", "preset_name", "name", "profile"]) {
                p.insert("preset".into(), json!(n));
            }
        }
        "convolution" | "room-correction" => {
            if let Some(f) = texte(settings, &["ir_path", "filter_set", "filter"]) {
                p.insert("filter".into(), json!(nom_de_fichier(&f)));
            }
            if settings.get("ir_path").is_none() && id == "room-correction" {
                if let Some(n) = settings
                    .get("bands")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                {
                    p.insert("bands".into(), json!(n));
                }
            }
        }
        _ if id == "crossfeed" || id == "crosstalk" || id.starts_with("crossfeed-") => {
            if let Some(n) = texte(settings, &["preset", "level"]) {
                p.insert("preset".into(), json!(n));
            }
        }
        _ => {}
    }
    p
}

fn etape_vide(code: CodeEtape) -> EtapeResolue {
    EtapeResolue {
        code,
        plugin_id: None,
        fixed: true,
        requested: false,
        effective: false,
        reason: None,
        detail: None,
        params: None,
        latency_frames: None,
    }
}

fn eteindre_pas_en_service(e: &mut EtapeResolue, ecarts: &mut Vec<Ecart>) {
    e.effective = false;
    e.reason = Some(MotifEtape::NotInService);
    e.detail = Some(DETAIL_PAS_EN_SERVICE.into());
    ecarts.push(Ecart {
        code: e.code,
        plugin_id: e.plugin_id.clone(),
        kind: SensEcart::RequestedNotRunning,
    });
}

fn allumer_hors_resolution(e: &mut EtapeResolue, ecarts: &mut Vec<Ecart>) {
    e.effective = true;
    e.reason = None;
    e.detail = Some(DETAIL_HORS_RESOLUTION.into());
    ecarts.push(Ecart {
        code: e.code,
        plugin_id: e.plugin_id.clone(),
        kind: SensEcart::RunningNotRequested,
    });
}

fn fusionner_params(e: &mut EtapeResolue, ajout: Map<String, Value>) {
    if ajout.is_empty() {
        return;
    }
    match e.params.as_mut().and_then(Value::as_object_mut) {
        Some(o) => o.extend(ajout),
        None => e.params = Some(Value::Object(ajout)),
    }
}

/// Rapproche la résolution de ce qui tourne et rend le bloc A4.
///
/// - `chaine` : la chaîne résolue (réglages des greffons, upsampling, dither) ;
/// - `chemin_intact` : le verdict du reste du chemin (transport, source) ;
///   `Some(false)` interdit le badge « Bit-perfect » ;
/// - `niveau` : le bilan T07 d'un étage EN SERVICE, sinon `None`.
pub fn chemin_du_signal(
    chaine: &ChaineTess,
    catalogue: &Catalogue,
    resolution: Resolution,
    en_service: &EnService,
    chemin_intact: Option<bool>,
    niveau: Option<NiveauPublie>,
) -> CheminTess {
    let hors_tess = resolution.badge == Badge::OutsideTess;
    let mut ecarts = Vec::new();

    let mut source = None;
    let mut headroom = None;
    let mut greffons: Vec<EtapeResolue> = Vec::new();
    let mut upsampling = None;
    let mut volume = None;
    let mut sortie = None;
    for e in resolution.etapes {
        match e.code {
            CodeEtape::Source => source = Some(e),
            CodeEtape::Headroom => headroom = Some(e),
            CodeEtape::Plugin => greffons.push(e),
            CodeEtape::Upsampling => upsampling = Some(e),
            CodeEtape::Volume => volume = Some(e),
            CodeEtape::Output => sortie = Some(e),
            // L'étape finale est recalculée après le rapprochement.
            CodeEtape::Quantization | CodeEtape::DsdModulator | CodeEtape::Unknown => {}
        }
    }
    let mut source = source.unwrap_or_else(|| etape_vide(CodeEtape::Source));
    source.effective = true;
    let mut headroom = headroom.unwrap_or_else(|| etape_vide(CodeEtape::Headroom));
    let mut upsampling = upsampling.unwrap_or_else(|| etape_vide(CodeEtape::Upsampling));
    let mut volume = volume.unwrap_or_else(|| etape_vide(CodeEtape::Volume));
    let mut sortie = sortie.unwrap_or_else(|| etape_vide(CodeEtape::Output));
    sortie.effective = true;

    // ── Greffons ────────────────────────────────────────────────────
    let reglages = |id: &str| -> Value {
        chaine
            .plugins
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.settings.clone())
            .unwrap_or(Value::Null)
    };
    for g in &mut greffons {
        let id = g.plugin_id.clone().unwrap_or_default();
        let tourne = match &en_service.greffons {
            GreffonsEnService::Exacts(v) => v.iter().any(|x| *x == id),
            GreffonsEnService::Sonde {
                actif,
                hors_service,
            } => *actif && g.effective && !hors_service.contains(&id),
        };
        if g.effective && !tourne {
            eteindre_pas_en_service(g, &mut ecarts);
        } else if !g.effective && tourne {
            allumer_hors_resolution(g, &mut ecarts);
        }
        g.latency_frames = en_service
            .latences
            .iter()
            .find(|(x, _)| *x == id)
            .map(|(_, l)| *l)
            .or(g.latency_frames);
        let r = reglages(&id);
        fusionner_params(g, params_du_greffon(&id, &r));
        // D2 : décoché d'office sur le réseau, il le dit plutôt que de
        // passer pour un oubli.
        if !g.requested
            && g.detail.is_none()
            && r.get(MARQUE_MONO_INERTE).and_then(Value::as_bool) == Some(true)
        {
            g.detail = Some(DETAIL_INERTE_RESEAU.into());
        }
    }
    match &en_service.greffons {
        GreffonsEnService::Exacts(v) => {
            // Un étage en service que la chaîne ne nomme pas.
            for id in v {
                if greffons.iter().any(|g| g.plugin_id.as_deref() == Some(id)) {
                    continue;
                }
                let mut e = etape_vide(CodeEtape::Plugin);
                e.fixed = false;
                e.plugin_id = Some(id.clone());
                e.latency_frames = en_service
                    .latences
                    .iter()
                    .find(|(x, _)| x == id)
                    .map(|(_, l)| *l);
                allumer_hors_resolution(&mut e, &mut ecarts);
                greffons.push(e);
            }
        }
        GreffonsEnService::Sonde { actif: true, .. } if !greffons.iter().any(|g| g.effective) => {
            // La sonde voit un traitement que la résolution n'attribue à
            // aucun greffon : il est publié, jamais tu.
            let mut e = etape_vide(CodeEtape::Plugin);
            e.fixed = false;
            allumer_hors_resolution(&mut e, &mut ecarts);
            e.detail = Some(DETAIL_NON_ATTRIBUE.into());
            greffons.push(e);
        }
        GreffonsEnService::Sonde { .. } => {}
    }
    // Ordre réel : les effectifs dans l'ordre d'exécution, puis les
    // inactifs dans l'ordre de la chaîne (ils ne traitent rien).
    let rang = |g: &EtapeResolue| -> usize {
        let id = g.plugin_id.as_deref().unwrap_or("");
        match &en_service.greffons {
            GreffonsEnService::Exacts(v) => v.iter().position(|x| x == id).unwrap_or(usize::MAX),
            GreffonsEnService::Sonde { .. } if g.plugin_id.is_none() => 0,
            GreffonsEnService::Sonde { .. } => usize::from(rang_historique(id, &reglages(id))),
        }
    };
    let (mut actifs, inactifs): (Vec<_>, Vec<_>) = greffons.into_iter().partition(|g| g.effective);
    actifs.sort_by_key(|g| rang(g));
    let greffons_effectifs = !actifs.is_empty();

    // ── Étages fixes ────────────────────────────────────────────────
    let headroom_en_service = en_service.headroom_db.filter(|db| *db != 0.0);
    match headroom_en_service {
        Some(db) => {
            if !headroom.effective {
                allumer_hors_resolution(&mut headroom, &mut ecarts);
            }
            let mut p = Map::new();
            p.insert("db".into(), json!(db));
            fusionner_params(&mut headroom, p);
        }
        None if headroom.effective => eteindre_pas_en_service(&mut headroom, &mut ecarts),
        None => {}
    }
    if headroom.effective
        && let Some(n) = niveau
    {
        let mut p = Map::new();
        p.insert(
            "absorbed_by_plugins_db".into(),
            json!(n.absorbed_by_plugins_db),
        );
        fusionner_params(&mut headroom, p);
    }
    let headroom_db = headroom_en_service
        .filter(|_| headroom.effective)
        .unwrap_or(0.0);

    if upsampling.effective && !en_service.upsampling {
        eteindre_pas_en_service(&mut upsampling, &mut ecarts);
    } else if !upsampling.effective && en_service.upsampling {
        allumer_hors_resolution(&mut upsampling, &mut ecarts);
    }

    if volume.effective && !en_service.volume_logiciel {
        eteindre_pas_en_service(&mut volume, &mut ecarts);
    } else if !volume.effective && en_service.volume_logiciel {
        allumer_hors_resolution(&mut volume, &mut ecarts);
    }
    if volume.effective
        && let Some(n) = niveau
    {
        let mut p = Map::new();
        p.insert("unrendered_db".into(), json!(n.unrendered_db));
        fusionner_params(&mut volume, p);
    }

    // ── Étape finale, recalculée sur le réel ───────────────────────
    let modifie =
        headroom.effective || greffons_effectifs || upsampling.effective || volume.effective;
    let vers_dsd =
        upsampling.effective && matches!(chaine.fixed.upsampling, Upsampling::Dsd { .. });
    let finale = if vers_dsd {
        let mut f = etape_vide(CodeEtape::DsdModulator);
        f.requested = true;
        f.effective = true;
        f.params = upsampling.params.clone();
        f
    } else {
        let mut f = etape_vide(CodeEtape::Quantization);
        f.requested = true;
        f.effective = modifie;
        f.params = Some(json!({ "dither": chaine.fixed.quantization.dither }));
        if !modifie {
            f.reason = Some(MotifEtape::NothingToQuantize);
            f.detail = Some("Bit-perfect".into());
        } else if chaine.fixed.quantization.dither == Dither::Unknown {
            f.reason = Some(MotifEtape::UnknownValue);
        }
        f
    };

    let etat_volume = EtatVolume {
        control: resolution.volume.control,
        placement: volume.effective.then_some(if vers_dsd {
            PlacementVolume::BeforeDsdModulator
        } else {
            PlacementVolume::AtRender
        }),
        db: resolution.volume.db,
    };

    let badge = if hors_tess {
        if modifie {
            Badge::AlteredOutsideTess
        } else {
            Badge::OutsideTess
        }
    } else if modifie {
        Badge::ProcessedByTess
    } else if chemin_intact == Some(false) {
        Badge::AlteredOutsideTess
    } else {
        Badge::BitPerfect
    };

    let nommer = |e: EtapeResolue| -> EtapeNommee {
        let name = match (&e.code, e.plugin_id.as_deref()) {
            (CodeEtape::Plugin, Some(id)) => catalogue
                .get(id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.to_owned()),
            (CodeEtape::Plugin, None) => "DSP".to_owned(),
            (code, _) => nom_fixe(*code).to_owned(),
        };
        EtapeNommee { name, etape: e }
    };
    let niveau_publie = niveau.filter(|_| headroom.effective || volume.effective || modifie);
    let mut steps = Vec::with_capacity(actifs.len() + inactifs.len() + 6);
    steps.push(nommer(source));
    steps.push(nommer(headroom));
    steps.extend(actifs.into_iter().map(nommer));
    steps.extend(inactifs.into_iter().map(nommer));
    steps.push(nommer(upsampling));
    steps.push(nommer(volume));
    steps.push(nommer(finale));
    steps.push(nommer(sortie));

    CheminTess {
        badge,
        headroom_db,
        volume: etat_volume,
        level: niveau_publie,
        steps,
        discrepancies: ecarts,
    }
}

#[cfg(test)]
#[path = "chemin_tests.rs"]
mod tests;
