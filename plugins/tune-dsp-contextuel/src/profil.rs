//! Le moteur de profils : des règles (année, label, support, genre, mesures)
//! → une liste de traitements avec leurs réglages, leur raison affichable,
//! leur priorité et leur droit. Résolution PURE et déterministe
//! ([`resoudre`]), puis choix de l'utilisateur (« toujours pour cet album /
//! ce label / jamais »).
//!
//! Contrat : `coordination/dsp-contextuels/contrat-moteur.md`, §2 et §3.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::analyse::{MesuresFichier, seuils};
use crate::traitements::reserve::{ContrainteDeMarge, reserve_pour_la_crete};
use crate::traitements::{Canal, GenreTraitement, OrigineDsd, Reglages};

/// Le support d'origine d'un album.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Cd,
    Vinyle,
    Cassette,
    Numerique,
    Dsd,
    #[default]
    Inconnu,
}

/// D'où vient l'indicateur de pré-accentuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePreaccentuation {
    Toc,
    Cue,
}

/// Qui règle le volume de la zone. Le loudness n'a de sens qu'en volume
/// LOGICIEL : ailleurs, Tune ne connaît pas le niveau d'écoute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceDuVolume {
    Logiciel,
    Materiel,
    Dac,
    LecteurReseau,
    Fixe,
    #[default]
    Inconnue,
}

impl SourceDuVolume {
    pub fn code(self) -> &'static str {
        match self {
            Self::Logiciel => "logiciel",
            Self::Materiel => "materiel",
            Self::Dac => "dac",
            Self::LecteurReseau => "lecteur_reseau",
            Self::Fixe => "fixe",
            Self::Inconnue => "inconnue",
        }
    }
}

/// Les droits ouverts par la licence de l'utilisateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DroitsDisponibles {
    pub premium: bool,
}

/// Tout ce que la résolution sait d'un album.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContexteAlbum {
    pub album_id: i64,
    pub annee: Option<u16>,
    pub label: Option<String>,
    pub support: Support,
    pub genres: Vec<String>,
    pub mono_declare: bool,
    pub preaccentuation: Option<SourcePreaccentuation>,
    pub origine_dsd: OrigineDsd,
    pub mesures: Option<MesuresFichier>,
    /// Mode Pur de la zone : rien d'actif.
    pub mode_pur: bool,
    pub droits: DroitsDisponibles,
    /// Qui règle le volume de la zone (loudness).
    pub volume: SourceDuVolume,
}

/// Un label comparé sans casse ni espaces de bord.
pub fn normaliser_label(l: &str) -> String {
    l.trim().to_lowercase()
}

/// Une condition d'une règle. Une condition de MESURE sur un contexte sans
/// mesures est fausse : on ne devine jamais.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "si", rename_all = "snake_case")]
pub enum Condition {
    AnneeEntre { min: Option<u16>, max: Option<u16> },
    Label { un_de: Vec<String> },
    Support { support: Support },
    Genre { contient: String },
    Preaccentuation,
    OrigineDsd { origine: OrigineDsd },
    MonoDeclare,
    DoubleMono,
    PolariteInversee,
    ContinuAuDessusDe { dbfs: f64 },
    EcartLrAuDessusDe { db: f64 },
    CreteVraieAuDessusDe { dbtp: f64 },
    DrSous { dr: f64 },
    Toujours,
}

impl Condition {
    pub fn vraie(&self, c: &ContexteAlbum) -> bool {
        let m = c.mesures.as_ref();
        match self {
            Self::AnneeEntre { min, max } => c
                .annee
                .is_some_and(|a| min.is_none_or(|mn| a >= mn) && max.is_none_or(|mx| a <= mx)),
            Self::Label { un_de } => c.label.as_deref().is_some_and(|l| {
                let l = normaliser_label(l);
                un_de.iter().any(|x| normaliser_label(x) == l)
            }),
            Self::Support { support } => c.support == *support,
            Self::Genre { contient } => {
                let cherche = contient.to_lowercase();
                c.genres.iter().any(|g| g.to_lowercase().contains(&cherche))
            }
            Self::Preaccentuation => c.preaccentuation.is_some(),
            Self::OrigineDsd { origine } => c.origine_dsd == *origine,
            Self::MonoDeclare => c.mono_declare,
            Self::DoubleMono => m.is_some_and(|m| m.double_mono),
            Self::PolariteInversee => m.is_some_and(|m| m.polarite_inversee),
            Self::ContinuAuDessusDe { dbfs } => m.is_some_and(|m| m.continu_max_dbfs() > *dbfs),
            Self::EcartLrAuDessusDe { db } => m
                .and_then(|m| m.ecart_lr_db)
                .is_some_and(|e| e.abs() >= *db),
            Self::CreteVraieAuDessusDe { dbtp } => m.is_some_and(|m| m.crete_vraie_dbtp > *dbtp),
            Self::DrSous { dr } => m.and_then(|m| m.dr).is_some_and(|d| d < *dr),
            Self::Toujours => true,
        }
    }
}

/// Le droit qu'une règle exige.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "droit", rename_all = "snake_case")]
pub enum Droit {
    /// Vague 1.
    #[default]
    Gratuit,
    /// Vagues 2 et 3 (« Profils sonores ») : vérifié côté serveur par
    /// `exiger_le_droit` au branchement ; `fonction` nomme la fonction
    /// Premium.
    Premium { fonction: String },
}

impl Droit {
    pub fn ouvert(&self, d: DroitsDisponibles) -> bool {
        match self {
            Self::Gratuit => true,
            Self::Premium { .. } => d.premium,
        }
    }
}

/// Une raison affichable : une clé i18n et ses paramètres bruts (le client
/// compose la phrase et formate les nombres).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Raison {
    pub cle: String,
    pub params: BTreeMap<String, Value>,
}

impl Raison {
    pub fn new(cle: &str) -> Self {
        Self {
            cle: cle.to_string(),
            params: BTreeMap::new(),
        }
    }

    pub fn avec(mut self, nom: &str, valeur: Value) -> Self {
        self.params.insert(nom.to_string(), valeur);
        self
    }
}

pub const CLE_CHOIX_UTILISATEUR: &str = "dsp_contextuel.raison.choix_utilisateur";

/// Une règle d'un profil.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regle {
    /// Stable : clé des journaux et du départage.
    pub id: String,
    /// ET logique ; une liste vide ne s'applique jamais.
    pub conditions: Vec<Condition>,
    pub reglages: Reglages,
    /// Clé i18n de la raison ; ses paramètres sont tirés du contexte.
    pub raison: String,
    pub priorite: i32,
    #[serde(default)]
    pub droit: Droit,
    #[serde(default)]
    pub actif_par_defaut: bool,
}

impl Regle {
    pub fn s_applique(&self, c: &ContexteAlbum) -> bool {
        !self.conditions.is_empty() && self.conditions.iter().all(|x| x.vraie(c))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profil {
    pub id: String,
    pub regles: Vec<Regle>,
}

/// La portée d'un choix de l'utilisateur, du plus précis au plus large.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "portee", content = "valeur", rename_all = "snake_case")]
pub enum Portee {
    Album(i64),
    Label(String),
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Toujours,
    Jamais,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoixUtilisateur {
    pub portee: Portee,
    pub genre: GenreTraitement,
    pub decision: Decision,
}

/// Où les choix de l'utilisateur sont gardés. La table SQL viendra au
/// branchement ; [`DepotEnMemoire`] sert aux tests.
pub trait DepotDeChoix {
    /// Les choix qui peuvent concerner cet album : les siens, ceux de son
    /// label, les globaux.
    fn choix_pour(&self, album_id: i64, label: Option<&str>) -> Vec<ChoixUtilisateur>;
    /// Enregistre un choix ; il remplace celui de même portée et même genre.
    fn enregistrer(&mut self, choix: ChoixUtilisateur);
    /// Oublie le choix de cette portée pour ce genre.
    fn oublier(&mut self, portee: &Portee, genre: GenreTraitement);
}

#[derive(Debug, Default)]
pub struct DepotEnMemoire {
    choix: HashMap<(Portee, GenreTraitement), Decision>,
}

fn portee_normalisee(p: &Portee) -> Portee {
    match p {
        Portee::Label(l) => Portee::Label(normaliser_label(l)),
        autre => autre.clone(),
    }
}

impl DepotDeChoix for DepotEnMemoire {
    fn choix_pour(&self, album_id: i64, label: Option<&str>) -> Vec<ChoixUtilisateur> {
        let label = label.map(normaliser_label);
        let mut v: Vec<ChoixUtilisateur> = self
            .choix
            .iter()
            .filter(|((p, _), _)| match p {
                Portee::Album(a) => *a == album_id,
                Portee::Label(l) => label.as_deref() == Some(l.as_str()),
                Portee::Global => true,
            })
            .map(|((p, g), d)| ChoixUtilisateur {
                portee: p.clone(),
                genre: *g,
                decision: *d,
            })
            .collect();
        v.sort_by_key(|c| (c.genre, rang(&c.portee)));
        v
    }

    fn enregistrer(&mut self, c: ChoixUtilisateur) {
        self.choix
            .insert((portee_normalisee(&c.portee), c.genre), c.decision);
    }

    fn oublier(&mut self, portee: &Portee, genre: GenreTraitement) {
        self.choix.remove(&(portee_normalisee(portee), genre));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Etat {
    Actif,
    Propose,
    Refuse,
    /// Demandé, mais sans effet possible ici (loudness sans volume
    /// logiciel) : aucun étage n'est construit, la raison dit pourquoi.
    Neutre,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrigineDuChoix {
    Defaut,
    Album,
    Label,
    Global,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraitementResolu {
    pub genre: GenreTraitement,
    pub reglages: Reglages,
    pub etat: Etat,
    /// `None` : ajouté par un choix « toujours » sans règle.
    pub regle_id: Option<String>,
    pub raison: Raison,
    pub droit: Droit,
    /// Le droit manque : proposé seulement.
    pub droit_requis: bool,
    pub origine_du_choix: OrigineDuChoix,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfilResolu {
    pub album_id: i64,
    pub profil_id: String,
    /// Dans l'ordre canonique de la chaîne.
    pub traitements: Vec<TraitementResolu>,
}

impl ProfilResolu {
    pub fn actifs(&self) -> impl Iterator<Item = &TraitementResolu> {
        self.traitements.iter().filter(|t| t.etat == Etat::Actif)
    }

    /// Ce que ce profil demande à l'autorité de marge UNIQUE de TESS : la
    /// crête vraie (anti-écrêtage actif) et le gain positif maximal des
    /// étages actifs (plafond du loudness).
    pub fn contrainte_de_marge(&self) -> ContrainteDeMarge {
        let mut c = ContrainteDeMarge::default();
        for t in self.actifs() {
            match t.reglages {
                Reglages::Reserve {
                    crete_vraie_dbtp,
                    marge_db,
                } => {
                    c.crete_vraie_dbtp = crete_vraie_dbtp;
                    c.reserve_crete_db = reserve_pour_la_crete(crete_vraie_dbtp, marge_db);
                }
                Reglages::Loudness { palier, .. } => {
                    c.gain_positif_max_db = c.gain_positif_max_db.max(palier.plafond_db());
                }
                _ => {}
            }
        }
        c
    }
}

pub const CLE_LOUDNESS_SANS_VOLUME_LOGICIEL: &str =
    "dsp_contextuel.raison.loudness_sans_volume_logiciel";

fn rang(p: &Portee) -> u8 {
    match p {
        Portee::Album(_) => 0,
        Portee::Label(_) => 1,
        Portee::Global => 2,
    }
}

/// Complète les réglages tirés des mesures (écart L/R, crête vraie).
fn renseigner(reglages: Reglages, c: &ContexteAlbum) -> Reglages {
    let m = c.mesures.as_ref();
    match reglages {
        Reglages::Equilibre { .. } => Reglages::Equilibre {
            ecart_db: m.and_then(|m| m.ecart_lr_db).unwrap_or(0.0),
        },
        Reglages::Reserve { marge_db, .. } => Reglages::Reserve {
            crete_vraie_dbtp: m.map(|m| m.crete_vraie_dbtp),
            marge_db,
        },
        autre => autre,
    }
}

fn arrondi(x: f64, decimales: i32) -> Value {
    let k = 10f64.powi(decimales);
    json!((x * k).round() / k)
}

/// Les paramètres d'une raison, tirés du contexte et des réglages.
fn raison(cle: &str, r: &Reglages, c: &ContexteAlbum) -> Raison {
    let m = c.mesures.as_ref();
    let base = Raison::new(cle);
    match *r {
        Reglages::Desaccentuation => match c.preaccentuation {
            Some(SourcePreaccentuation::Toc) => base.avec("source", json!("toc")),
            Some(SourcePreaccentuation::Cue) => base.avec("source", json!("cue")),
            None => base,
        },
        Reglages::UltrasoniqueDsd { coupure_hz } => {
            base.avec("coupure_khz", arrondi(coupure_hz / 1000.0, 1))
        }
        Reglages::Polarite { .. } => match m.and_then(|m| m.correlation) {
            Some(x) => base.avec("correlation", arrondi(x, 2)),
            None => base,
        },
        Reglages::SuppressionContinu { .. } => match m {
            Some(m) => base.avec("dbfs", arrondi(m.continu_max_dbfs(), 1)),
            None => base,
        },
        Reglages::Equilibre { ecart_db } => base.avec("ecart_db", arrondi(ecart_db.abs(), 1)).avec(
            "canal",
            json!(if ecart_db > 0.0 {
                Canal::Gauche
            } else {
                Canal::Droit
            }),
        ),
        Reglages::Reserve {
            crete_vraie_dbtp: Some(crete),
            marge_db,
        } => base.avec("crete_dbtp", arrondi(crete, 1)).avec(
            "reserve_db",
            arrondi(reserve_pour_la_crete(Some(crete), marge_db), 1),
        ),
        Reglages::Reserve { .. } => base,
        Reglages::Rumble { coupure_hz, .. } => base.avec("coupure_hz", arrondi(coupure_hz, 0)),
        Reglages::Loudness { .. } | Reglages::Mono => base,
    }
}

/// Résout un profil pour un album. Pure et déterministe : même profil, même
/// contexte, mêmes choix ⇒ même résultat, quel que soit l'ordre des règles
/// et des choix.
pub fn resoudre(
    profil: &Profil,
    contexte: &ContexteAlbum,
    choix: &[ChoixUtilisateur],
) -> ProfilResolu {
    // 1-2. Règles vraies, départagées par genre : priorité, puis id.
    let mut retenues: BTreeMap<GenreTraitement, &Regle> = BTreeMap::new();
    for r in profil.regles.iter().filter(|r| r.s_applique(contexte)) {
        let g = r.reglages.genre();
        let remplace = match retenues.get(&g) {
            None => true,
            Some(e) => {
                (r.priorite, std::cmp::Reverse(&r.id)) > (e.priorite, std::cmp::Reverse(&e.id))
            }
        };
        if remplace {
            retenues.insert(g, r);
        }
    }

    // 3. Choix de l'utilisateur, du plus précis au plus large.
    let label = contexte.label.as_deref().map(normaliser_label);
    let mut decisions: BTreeMap<GenreTraitement, (u8, Decision)> = BTreeMap::new();
    for ch in choix {
        let concerne = match &ch.portee {
            Portee::Album(a) => *a == contexte.album_id,
            Portee::Label(l) => label.as_deref() == Some(normaliser_label(l).as_str()),
            Portee::Global => true,
        };
        if !concerne {
            continue;
        }
        let r = rang(&ch.portee);
        let garde = decisions
            .get(&ch.genre)
            // À portée égale, « jamais » l'emporte : jamais de surprise.
            .is_none_or(|&(r0, d0)| r < r0 || (r == r0 && d0 == Decision::Toujours));
        if garde {
            decisions.insert(ch.genre, (r, ch.decision));
        }
    }

    let mut traitements = Vec::new();
    for genre in GenreTraitement::TOUS {
        let regle = retenues.get(&genre).copied();
        let decision = decisions.get(&genre).copied();
        if regle.is_none() && !matches!(decision, Some((_, Decision::Toujours))) {
            continue;
        }
        let origine_du_choix = match decision.map(|d| d.0) {
            None => OrigineDuChoix::Defaut,
            Some(0) => OrigineDuChoix::Album,
            Some(1) => OrigineDuChoix::Label,
            Some(_) => OrigineDuChoix::Global,
        };
        let (reglages, regle_id, droit, raison_) = match regle {
            Some(r) => {
                let reg = renseigner(r.reglages, contexte);
                (
                    reg,
                    Some(r.id.clone()),
                    r.droit.clone(),
                    raison(&r.raison, &reg, contexte),
                )
            }
            None => {
                let reg = renseigner(Reglages::par_defaut(genre), contexte);
                let portee = match origine_du_choix {
                    OrigineDuChoix::Album => "album",
                    OrigineDuChoix::Label => "label",
                    _ => "global",
                };
                (
                    reg,
                    None,
                    Droit::Gratuit,
                    Raison::new(CLE_CHOIX_UTILISATEUR).avec("portee", json!(portee)),
                )
            }
        };
        let mut etat = match decision {
            Some((_, Decision::Jamais)) => Etat::Refuse,
            Some((_, Decision::Toujours)) => Etat::Actif,
            None if regle.is_some_and(|r| r.actif_par_defaut) => Etat::Actif,
            None => Etat::Propose,
        };
        // 5. Garde-fous.
        let droit_requis = !droit.ouvert(contexte.droits);
        if etat == Etat::Actif && (contexte.mode_pur || droit_requis) {
            etat = Etat::Propose;
        }
        let mut raison_ = raison_;
        if genre == GenreTraitement::Loudness
            && etat != Etat::Refuse
            && contexte.volume != SourceDuVolume::Logiciel
        {
            etat = Etat::Neutre;
            raison_ = Raison::new(CLE_LOUDNESS_SANS_VOLUME_LOGICIEL)
                .avec("volume", json!(contexte.volume.code()));
        }
        if genre == GenreTraitement::UltrasoniqueDsd
            && contexte.origine_dsd != OrigineDsd::ConvertieEnPcm
        {
            continue;
        }
        traitements.push(TraitementResolu {
            genre,
            reglages,
            etat,
            regle_id,
            raison: raison_,
            droit,
            droit_requis,
            origine_du_choix,
        });
    }

    ProfilResolu {
        album_id: contexte.album_id,
        profil_id: profil.id.clone(),
        traitements,
    }
}

/// Le profil de la vague 1 (corrections techniques, gratuites). Seule la
/// désaccentuation est active par défaut (décision de Bertrand, 25/11).
pub fn profil_vague_1() -> Profil {
    use crate::traitements::{loudness, reserve, rumble, ultrasonique};
    fn regle(id: &str, conditions: Vec<Condition>, reglages: Reglages, priorite: i32) -> Regle {
        Regle {
            id: id.to_string(),
            conditions,
            reglages,
            raison: format!("dsp_contextuel.raison.{id}"),
            priorite,
            droit: Droit::Gratuit,
            actif_par_defaut: false,
        }
    }
    let mut desaccentuation = regle(
        "desaccentuation_cd",
        vec![Condition::Preaccentuation],
        Reglages::Desaccentuation,
        100,
    );
    desaccentuation.actif_par_defaut = true;
    Profil {
        id: "vague_1".to_string(),
        regles: vec![
            desaccentuation,
            regle(
                "ultrasonique_dsd",
                vec![Condition::OrigineDsd {
                    origine: OrigineDsd::ConvertieEnPcm,
                }],
                Reglages::UltrasoniqueDsd {
                    coupure_hz: ultrasonique::COUPURE_PAR_DEFAUT_HZ,
                },
                100,
            ),
            regle(
                "mono_declare",
                vec![Condition::MonoDeclare],
                Reglages::Mono,
                20,
            ),
            regle(
                "double_mono",
                vec![Condition::DoubleMono],
                Reglages::Mono,
                10,
            ),
            regle(
                "polarite_relative",
                vec![Condition::PolariteInversee],
                Reglages::Polarite {
                    canal: Canal::Droit,
                },
                100,
            ),
            regle(
                "continu",
                vec![Condition::ContinuAuDessusDe {
                    dbfs: seuils::CONTINU_DBFS,
                }],
                Reglages::SuppressionContinu {
                    coupure_hz: crate::traitements::continu::COUPURE_HZ,
                },
                100,
            ),
            regle(
                "equilibre",
                vec![Condition::EcartLrAuDessusDe {
                    db: seuils::ECART_LR_DB,
                }],
                Reglages::Equilibre { ecart_db: 0.0 },
                100,
            ),
            // Le volume n'est connu qu'à la lecture : toujours proposé.
            regle(
                "loudness",
                vec![Condition::Toujours],
                Reglages::Loudness {
                    reference_phon: loudness::REFERENCE_PHON,
                    palier: loudness::PalierLoudness::Standard,
                },
                100,
            ),
            regle(
                "reserve",
                vec![Condition::CreteVraieAuDessusDe { dbtp: 0.0 }],
                Reglages::Reserve {
                    crete_vraie_dbtp: None,
                    marge_db: reserve::MARGE_PAR_DEFAUT_DB,
                },
                100,
            ),
            regle(
                "rumble",
                vec![Condition::Support {
                    support: Support::Vinyle,
                }],
                Reglages::Rumble {
                    coupure_hz: rumble::COUPURE_PAR_DEFAUT_HZ,
                    voile: false,
                },
                100,
            ),
        ],
    }
}

#[cfg(test)]
mod tests;
