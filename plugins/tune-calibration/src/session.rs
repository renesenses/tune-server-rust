//! C4 (#7632, lot TESS T10a) : la session de mesure à plusieurs positions.
//!
//! ```text
//! POST /zones/{z}/sessions            session : N positions prévues (3 à 9),
//!    │                                position centrale, pondération, rejet
//! POST /sessions/{s}/positions  ×N    une mesure C1 ordinaire par position
//!    │                                (le micro est déplacé entre deux)
//! GET  /sessions/{s}                  positions, verdicts, moyenne pondérée
//! POST /sessions/{s}/correction       C2 sur la moyenne (défauts spatiaux)
//! POST /comparaisons                  A/B avant/après (mesures ou sessions)
//! GET  /sessions/{s}/export           document versionné (v2)
//! POST /sessions/import               v2, ou v1 = résultat mono-position
//! ```
//!
//! **Format versionné** ([`VERSION_SESSION`]) :
//! * **v1** : le document d'une mesure mono-position tel que
//!   `GET /mesures/{id}/resultat` le rend depuis C1 (le [`Resultat`] plus
//!   `id`, `zone_id`, `entree`), sans champ `version`. Il se relit comme une
//!   session d'UNE position, sans perte : [`PositionSession::document_v1`]
//!   rend exactement le document lu.
//! * **v2** : [`Session`], avec `"version": 2`.
//! * Une version plus récente est refusée, nommément (`unsupported_version`),
//!   jamais lue à moitié.
//!
//! Une session vit en mémoire, comme les mesures ; c'est le client qui garde
//! le document exporté.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_calibration::analysis::LogResponse;
use tune_calibration::positions::{
    self, ComparaisonAB, ConfigPositions, MoyennePositions, POSITIONS_MAX, POSITIONS_MIN,
    VerdictPosition,
};

use crate::correction::{Cible, DemandeCorrection, Proposition, proposer_positions};
use crate::mesure::{Controleur, Demande, Etat, Resultat};

/// Version du format de session écrit par ce greffon.
pub const VERSION_SESSION: u32 = 2;
/// Au-delà, la plus ancienne session est oubliée.
pub const SESSIONS_GARDEES: usize = 32;

/// Une position d'une session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionSession {
    pub index: usize,
    /// « canapé gauche », « place d'écoute »… (facultatif).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etiquette: Option<String>,
    /// La mesure C1 qui porte cette position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesure_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entree: Option<String>,
    /// Le résultat de la mesure, une fois terminée.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resultat: Option<Resultat>,
}

impl PositionSession {
    /// Le document v1 (mono-position) de cette position : celui que
    /// `GET /mesures/{id}/resultat` rendait.
    pub fn document_v1(&self, zone_id: i64) -> Option<Value> {
        let r = self.resultat.as_ref()?;
        let mut v = serde_json::to_value(r).ok()?;
        let o = v.as_object_mut()?;
        if let Some(id) = &self.mesure_id {
            o.insert("id".into(), json!(id));
        }
        o.insert("zone_id".into(), json!(zone_id));
        if let Some(e) = &self.entree {
            o.insert("entree".into(), json!(e));
        }
        Some(v)
    }

    /// La réponse ramenée au niveau joué `niveau_ref_dbfs`. La montée de
    /// niveau par salves peut arrêter deux positions à des niveaux joués
    /// différents (6 dB d'un pas à l'autre) : sans ce recalage, une position
    /// paraîtrait aberrante pour un écart qui n'est pas dans la pièce. Le
    /// décalage est exactement 0 pour la position de référence.
    fn reponse(&self, niveau_ref_dbfs: f64) -> Option<LogResponse> {
        self.resultat.as_ref().map(|r| {
            let d = niveau_ref_dbfs - r.niveau_joue_dbfs;
            LogResponse {
                freqs: r.reponse.freqs_hz.clone(),
                magnitude_db: if d == 0.0 {
                    r.reponse.db.clone()
                } else {
                    r.reponse.db.iter().map(|x| x + d).collect()
                },
            }
        })
    }
}

/// Une session de mesure à plusieurs positions (format v2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: String,
    pub zone_id: i64,
    pub positions_prevues: usize,
    /// Index de la place d'écoute : prépondérante dans la moyenne.
    pub centrale: usize,
    #[serde(default)]
    pub config: ConfigPositions,
    pub positions: Vec<PositionSession>,
}

/// Pourquoi un document n'a pas été lu.
#[derive(Debug, Clone, PartialEq)]
pub enum ErreurLecture {
    /// Écrit par une version plus récente du greffon.
    VersionNonPrise(u64),
    Invalide(String),
}

impl Session {
    /// Lit un document v1 (résultat mono-position) ou v2.
    pub fn lire(doc: Value) -> Result<Self, ErreurLecture> {
        let version = match doc.get("version") {
            None => 1,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| ErreurLecture::Invalide("version : entier attendu".into()))?,
        };
        if version > VERSION_SESSION as u64 {
            return Err(ErreurLecture::VersionNonPrise(version));
        }
        let s = if version <= 1 {
            Self::depuis_v1(doc)?
        } else {
            let mut s: Session = serde_json::from_value(doc)
                .map_err(|e| ErreurLecture::Invalide(format!("session v2 illisible : {e}")))?;
            s.version = VERSION_SESSION;
            s
        };
        s.valider().map_err(ErreurLecture::Invalide)?;
        Ok(s)
    }

    /// Un résultat mono-position (format de C1) devient une session d'une
    /// position, sans rien perdre de ce qu'il portait.
    fn depuis_v1(doc: Value) -> Result<Self, ErreurLecture> {
        let mesure_id = doc.get("id").and_then(Value::as_str).map(str::to_string);
        let entree = doc
            .get("entree")
            .and_then(Value::as_str)
            .map(str::to_string);
        let zone_id = doc.get("zone_id").and_then(Value::as_i64).unwrap_or(0);
        let mut corps = doc;
        if let Some(o) = corps.as_object_mut() {
            o.remove("id");
            o.remove("zone_id");
            o.remove("entree");
            o.remove("version");
        }
        let resultat: Resultat = serde_json::from_value(corps).map_err(|e| {
            ErreurLecture::Invalide(format!("mesure mono-position (v1) illisible : {e}"))
        })?;
        Ok(Session {
            version: VERSION_SESSION,
            id: String::new(),
            zone_id,
            positions_prevues: 1,
            centrale: 0,
            config: ConfigPositions::default(),
            positions: vec![PositionSession {
                index: 0,
                etiquette: None,
                mesure_id,
                entree,
                resultat: Some(resultat),
            }],
        })
    }

    fn valider(&self) -> Result<(), String> {
        if self.positions_prevues == 0 || self.positions_prevues > POSITIONS_MAX {
            return Err(format!(
                "{} positions prévues : 1 à {POSITIONS_MAX}",
                self.positions_prevues
            ));
        }
        if self.centrale >= self.positions_prevues {
            return Err(format!(
                "position centrale {} : {} positions prévues",
                self.centrale, self.positions_prevues
            ));
        }
        if self.positions.len() > self.positions_prevues {
            return Err("plus de positions que prévu".into());
        }
        for (k, p) in self.positions.iter().enumerate() {
            if p.index >= self.positions_prevues
                || self.positions[..k].iter().any(|q| q.index == p.index)
            {
                return Err(format!(
                    "position {} : index invalide ou en double",
                    p.index
                ));
            }
        }
        self.config
            .ponderation
            .poids(self.positions_prevues, self.centrale)?;
        Ok(())
    }

    /// Les positions mesurées, dans l'ordre de leur index.
    fn mesurees(&self) -> Vec<&PositionSession> {
        let mut v: Vec<&PositionSession> = self
            .positions
            .iter()
            .filter(|p| p.resultat.is_some())
            .collect();
        v.sort_by_key(|p| p.index);
        v
    }

    /// La moyenne des positions mesurées : une session mono-position rend sa
    /// position ; une session multi-positions attend [`POSITIONS_MIN`]
    /// positions dont la centrale.
    pub fn moyenne(&self) -> Result<(MoyennePositions, Vec<usize>), String> {
        let mesurees = self.mesurees();
        if self.positions_prevues > 1 {
            if mesurees.len() < POSITIONS_MIN {
                return Err(format!(
                    "{} position(s) mesurée(s) sur {} : {POSITIONS_MIN} au moins pour une moyenne",
                    mesurees.len(),
                    self.positions_prevues
                ));
            }
            if !mesurees.iter().any(|p| p.index == self.centrale) {
                return Err(format!(
                    "la position centrale ({}) n'est pas encore mesurée",
                    self.centrale
                ));
            }
        } else if mesurees.is_empty() {
            return Err("aucune position mesurée".into());
        }
        let index: Vec<usize> = mesurees.iter().map(|p| p.index).collect();
        let centrale = index.iter().position(|i| *i == self.centrale).unwrap_or(0);
        let niveau_ref = mesurees[centrale]
            .resultat
            .as_ref()
            .map(|r| r.niveau_joue_dbfs)
            .unwrap_or(0.0);
        let reponses: Vec<LogResponse> = mesurees
            .iter()
            .filter_map(|p| p.reponse(niveau_ref))
            .collect();
        // Les poids sont donnés par position PRÉVUE : on garde ceux des mesurées.
        let tous = self
            .config
            .ponderation
            .poids(self.positions_prevues, self.centrale)?;
        let mut cfg = self.config.clone();
        cfg.ponderation.poids = Some(index.iter().map(|i| tous[*i]).collect());
        let mut m = positions::moyenne_positions(&reponses, centrale, &cfg)?;
        for (v, i) in m.verdicts.iter_mut().zip(&index) {
            v.index = *i;
        }
        Ok((m, index))
    }
}

/// Ce que `GET /sessions/{id}` rend : la session, l'état de chaque
/// position, et la moyenne si elle peut se calculer.
#[derive(Debug, Clone, Serialize)]
pub struct Vue {
    #[serde(flatten)]
    pub session: Session,
    pub etats: Vec<EtatPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moyenne: Option<VueMoyenne>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moyenne_indisponible: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EtatPosition {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesure_id: Option<String>,
    #[serde(flatten)]
    pub etat: Etat,
}

#[derive(Debug, Clone, Serialize)]
pub struct VueMoyenne {
    pub positions_retenues: usize,
    pub seuil_rejet_db: f64,
    pub verdicts: Vec<VerdictPosition>,
    pub freqs_hz: Vec<f64>,
    pub db: Vec<f64>,
    pub avertissements: Vec<String>,
}

/// Les sessions connues.
pub struct Magasin {
    entrees: Mutex<VecDeque<Session>>,
    compteur: AtomicU64,
}

impl Default for Magasin {
    fn default() -> Self {
        Self {
            entrees: Mutex::new(VecDeque::new()),
            compteur: AtomicU64::new(0),
        }
    }
}

impl Magasin {
    fn verrou(&self) -> std::sync::MutexGuard<'_, VecDeque<Session>> {
        self.entrees.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn nouvel_id(&self) -> String {
        format!(
            "s{}-{}",
            self.compteur.fetch_add(1, Ordering::Relaxed) + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        )
    }

    /// Range la session ; garde son id s'il est libre et sûr, sinon en donne un.
    pub fn ranger(&self, mut s: Session) -> String {
        let mut v = self.verrou();
        let sur = !s.id.is_empty()
            && s.id.len() <= 64
            && s.id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !sur || v.iter().any(|x| x.id == s.id) {
            s.id = self.nouvel_id();
        }
        let id = s.id.clone();
        v.push_back(s);
        while v.len() > SESSIONS_GARDEES {
            v.pop_front();
        }
        id
    }

    pub fn session(&self, id: &str) -> Option<Session> {
        self.verrou().iter().find(|s| s.id == id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        self.verrou().iter().map(|s| s.id.clone()).collect()
    }

    fn modifier<T>(&self, id: &str, f: impl FnOnce(&mut Session) -> T) -> Option<T> {
        self.verrou().iter_mut().find(|s| s.id == id).map(f)
    }
}

/// La demande de création d'une session.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DemandeSession {
    pub positions: usize,
    #[serde(default)]
    pub centrale: usize,
    #[serde(flatten)]
    pub config: ConfigPositions,
}

impl Controleur {
    /// Crée une session multi-positions sur la zone.
    pub fn creer_session(&self, zone_id: i64, d: DemandeSession) -> Result<String, String> {
        if !(POSITIONS_MIN..=POSITIONS_MAX).contains(&d.positions) {
            return Err(format!(
                "{} positions : entre {POSITIONS_MIN} et {POSITIONS_MAX}",
                d.positions
            ));
        }
        let s = Session {
            version: VERSION_SESSION,
            id: String::new(),
            zone_id,
            positions_prevues: d.positions,
            centrale: d.centrale,
            config: d.config,
            positions: Vec::new(),
        };
        s.valider()?;
        // Valide aussi le rejet, sur des courbes factices.
        let plate = LogResponse {
            freqs: vec![20.0, 40.0],
            magnitude_db: vec![0.0, 0.0],
        };
        let mut cfg = s.config.clone();
        cfg.ponderation = Default::default();
        positions::moyenne_positions(&vec![plate; POSITIONS_MIN], 0, &cfg)?;
        Ok(self.sessions.ranger(s))
    }

    /// Lance la mesure d'une position (la première libre si `index` est
    /// `None`). Une position déjà mesurée peut être refaite. Rend
    /// `(index, id de la mesure)`.
    pub fn mesurer_position(
        self: &Arc<Self>,
        session_id: &str,
        index: Option<usize>,
        etiquette: Option<String>,
        demande: Demande,
    ) -> Result<(usize, String), String> {
        self.synchroniser(session_id);
        let s = self
            .sessions
            .session(session_id)
            .ok_or_else(|| format!("aucune session « {session_id} »"))?;
        if demande.zone_id != s.zone_id {
            return Err(format!(
                "la session porte sur la zone {}, pas {}",
                s.zone_id, demande.zone_id
            ));
        }
        let occupee = |i: usize| {
            s.positions.iter().any(|p| {
                p.index == i
                    && (p.resultat.is_some()
                        || p.mesure_id
                            .as_deref()
                            .and_then(|m| self.mesure(m))
                            .is_some_and(|m| !m.etat().finale()))
            })
        };
        let index = match index {
            Some(i) if i < s.positions_prevues => i,
            Some(i) => {
                return Err(format!(
                    "position {i} : la session en prévoit {}",
                    s.positions_prevues
                ));
            }
            None => (0..s.positions_prevues)
                .find(|i| !occupee(*i))
                .ok_or("toutes les positions sont mesurées : préciser l'index à refaire")?,
        };
        let entree = demande.entree.clone();
        let mesure_id = self.lancer(demande)?;
        let etiquette = etiquette
            .map(|e| e.trim().chars().take(80).collect::<String>())
            .filter(|e| !e.is_empty());
        self.sessions.modifier(session_id, |s| {
            s.positions.retain(|p| p.index != index);
            s.positions.push(PositionSession {
                index,
                etiquette,
                mesure_id: Some(mesure_id.clone()),
                entree: Some(entree),
                resultat: None,
            });
            s.positions.sort_by_key(|p| p.index);
        });
        Ok((index, mesure_id))
    }

    /// Recopie dans la session les résultats des mesures terminées.
    pub fn synchroniser(&self, session_id: &str) {
        let Some(s) = self.sessions.session(session_id) else {
            return;
        };
        let a_copier: Vec<(usize, Resultat)> = s
            .positions
            .iter()
            .filter(|p| p.resultat.is_none())
            .filter_map(|p| {
                let m = self.mesure(p.mesure_id.as_deref()?)?;
                Some((p.index, (*m.resultat()?).clone()))
            })
            .collect();
        if a_copier.is_empty() {
            return;
        }
        self.sessions.modifier(session_id, |s| {
            for (i, r) in a_copier {
                if let Some(p) = s.positions.iter_mut().find(|p| p.index == i) {
                    p.resultat = Some(r);
                }
            }
        });
    }

    pub fn session(&self, id: &str) -> Option<Session> {
        self.synchroniser(id);
        self.sessions.session(id)
    }

    pub fn vue_session(&self, id: &str) -> Option<Vue> {
        let s = self.session(id)?;
        let etats = s
            .positions
            .iter()
            .map(|p| EtatPosition {
                index: p.index,
                mesure_id: p.mesure_id.clone(),
                etat: if p.resultat.is_some() {
                    Etat::Terminee
                } else {
                    p.mesure_id
                        .as_deref()
                        .and_then(|m| self.mesure(m))
                        .map(|m| m.etat())
                        .unwrap_or(Etat::Erreur {
                            message: "mesure oubliée (redémarrage ?) : refaire la position".into(),
                        })
                },
            })
            .collect();
        let (moyenne, moyenne_indisponible) = match s.moyenne() {
            Ok((m, _)) => (
                Some(VueMoyenne {
                    positions_retenues: m.retenues(),
                    seuil_rejet_db: m.seuil_applique_db,
                    verdicts: m.verdicts,
                    freqs_hz: m.reponse.freqs,
                    db: m.reponse.magnitude_db,
                    avertissements: m.avertissements,
                }),
                None,
            ),
            Err(e) => (None, Some(e)),
        };
        Some(Vue {
            session: s,
            etats,
            moyenne,
            moyenne_indisponible,
        })
    }

    /// Importe un document de session (v1 ou v2). Rend l'id et la version lue.
    pub fn importer_session(&self, doc: Value) -> Result<(String, u64), ErreurLecture> {
        let version = doc.get("version").and_then(Value::as_u64).unwrap_or(1);
        let s = Session::lire(doc)?;
        Ok((self.sessions.ranger(s), version))
    }

    /// La proposition C2 sur la moyenne de la session.
    pub fn proposer_session(
        &self,
        id: &str,
        demande: &DemandeCorrection,
    ) -> Result<(i64, Proposition), String> {
        let s = self
            .session(id)
            .ok_or_else(|| format!("aucune session « {id} »"))?;
        let (m, _) = s.moyenne()?;
        let mut p = proposer_positions(&m.reponse, demande, m.retenues())?;
        p.avertissements.extend(m.avertissements);
        for v in m.verdicts.iter().filter(|v| v.rejetee) {
            p.avertissements.push(format!(
                "position {} écartée : {}",
                v.index,
                v.motif.clone().unwrap_or_default()
            ));
        }
        Ok((s.zone_id, p))
    }

    /// La réponse d'une mesure terminée (`m…`) ou la moyenne d'une session
    /// (`s…`), et le nombre de positions qu'elle porte.
    pub fn reponse_de(&self, id: &str) -> Result<(LogResponse, usize), String> {
        if let Some(s) = self.session(id) {
            let (m, _) = s.moyenne()?;
            let n = m.retenues();
            return Ok((m.reponse, n));
        }
        let m = self
            .mesure(id)
            .ok_or_else(|| format!("aucune mesure ni session « {id} »"))?;
        let r = m
            .resultat()
            .ok_or_else(|| format!("la mesure « {id} » n'est pas terminée"))?;
        Ok((
            LogResponse {
                freqs: r.reponse.freqs_hz.clone(),
                magnitude_db: r.reponse.db.clone(),
            },
            1,
        ))
    }

    /// Comparaison A/B : la même zone avant et après correction.
    pub fn comparer(&self, d: &DemandeComparaison) -> Result<ComparaisonAB, String> {
        let (avant, _) = self.reponse_de(&d.avant)?;
        let (apres, _) = self.reponse_de(&d.apres)?;
        let f_lo = d.f_min_hz.unwrap_or(crate::correction::F_MIN_HZ);
        let f_hi = d.f_max_hz.unwrap_or(crate::correction::F_MAX_HZ);
        let cible = match &d.cible {
            Some(c) => {
                let t = c.courbe()?;
                Some(
                    avant
                        .freqs
                        .iter()
                        .map(|f| t.level_db(*f))
                        .collect::<Vec<_>>(),
                )
            }
            None => None,
        };
        positions::comparer_ab(&avant, &apres, f_lo, f_hi, cible.as_deref())
    }
}

/// Le corps de `POST /comparaisons`.
#[derive(Debug, Clone, Deserialize)]
pub struct DemandeComparaison {
    /// Id d'une mesure ou d'une session : AVANT correction.
    pub avant: String,
    /// Id d'une mesure ou d'une session : APRÈS correction.
    pub apres: String,
    pub f_min_hz: Option<f64>,
    pub f_max_hz: Option<f64>,
    pub cible: Option<Cible>,
}
