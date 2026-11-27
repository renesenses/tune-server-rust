//! rc15 — le suréchantillonnage PCM sur UPnP sans clic aux enchaînements ni
//! marche de niveau d'une piste à l'autre (relecture de #9565/#9568).
//!
//! **M1, continuité gapless.** Chaque piste est un fichier (ou un relais) à
//! part : l'état du filtre repartait de zéro à chaque frontière, avec une
//! queue calculée sur du silence et un départ sans passé, d'où un creux
//! jusqu'à −5,5 dB sous la crête sur 20 à 70 trames. Quand deux pistes
//! s'enchaînent SANS BLANC (même cadence, même canalisation, voisines dans
//! la file), chaque piste reçoit désormais le contexte de sa voisine :
//!
//! - **avant** : les dernières trames de la précédente passent d'abord par
//!   le noyau (sortie jetée) : l'état du filtre est celui qu'il aurait au
//!   bout de la précédente ;
//! - **après** : la queue est calculée sur les premières trames de la
//!   suivante, et non sur du silence.
//!
//! Le noyau étant linéaire et invariant, la concaténation des deux pistes est
//! alors, à la quantification près, le suréchantillonnage d'un seul flux.
//! Sans continuité (première piste, cadence ou canalisation différente,
//! recherche dans la piste, image CUE, fin de file), le bord est une
//! **rupture** : un fondu court et propre ([`DUREE_DU_FONDU_MS`]).
//!
//! **M2, niveau constant.** La réserve fixe du greffon (−3 dB) n'était pas
//! compensée en UPnP : une piste suréchantillonnée et une piste qui ne l'est
//! pas (refus, autre cadence) différaient de 3 dB, et la réserve variait avec
//! le filtre. Règle de Bertrand : gain ≤ 1, aucun clic, toute hausse rejoint
//! le niveau d'avant en environ 0,5 s sans le dépasser. D'où :
//!
//! - tout étage suréchantillonné sort au niveau de la ZONE
//!   ([`crate::audio::surechantillonnage::gain_de_zone`]) quel que soit son
//!   filtre ;
//! - sur une zone dont le lecteur se prête au suréchantillonnage, une piste
//!   qui n'est pas suréchantillonnée est jouée au MÊME niveau (étage de
//!   niveau seul) ;
//! - le passage d'un niveau à l'autre (premier suréchantillonnage, retour au
//!   niveau plein quand le réglage est coupé) est une rampe en cosinus :
//!   [`DUREE_DE_MONTEE_S`] pour une hausse, qui ne dépasse jamais sa cible,
//!   [`DUREE_DE_DESCENTE_S`] pour une baisse.

use crate::audio::surechantillonnage as su;

/// La durée du fondu d'un bord sans continuité.
pub const DUREE_DU_FONDU_MS: f64 = 3.0;
/// La durée d'une HAUSSE de niveau (règle de Bertrand : environ 0,5 s).
pub const DUREE_DE_MONTEE_S: f64 = 0.5;
/// La durée d'une BAISSE de niveau : assez longue pour ne pas cliquer.
pub const DUREE_DE_DESCENTE_S: f64 = 0.010;
/// Le contexte de chaque côté, en trames d'entrée, au-delà de la portée du
/// noyau (marge).
pub const MARGE_DE_CONTEXTE: usize = 16;

/// Un bord de piste.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Bord {
    /// Rien n'est su (radio, service, appelant d'avant) : comportement
    /// d'avant, queue sur du silence, aucun fondu.
    #[default]
    Inconnu,
    /// La voisine s'enchaîne sans blanc : ses trames (entrelacées, `f64`,
    /// à l'entrée de l'étage), du côté de la frontière.
    Continu(Vec<f64>),
    /// Pas de continuité : fondu court.
    Rupture,
}

/// Les deux bords d'une piste suréchantillonnée.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContexteDeJonction {
    pub avant: Bord,
    pub apres: Bord,
}

impl ContexteDeJonction {
    /// Ce que les bords changent au fichier servi, pour sa clé de cache.
    pub(crate) fn empreinte(&self, h: &mut sha2::Sha256) {
        use sha2::Digest;
        for (nom, b) in [(b"avant\0", &self.avant), (b"apres\0", &self.apres)] {
            h.update(nom);
            match b {
                Bord::Inconnu => h.update(b"i"),
                Bord::Rupture => h.update(b"r"),
                Bord::Continu(v) => {
                    h.update(b"c");
                    h.update((v.len() as u64).to_le_bytes());
                    for x in v {
                        h.update(x.to_le_bytes());
                    }
                }
            }
        }
    }
}

/// Le nombre de trames d'un fondu de bord à la cadence `cadence`.
pub fn trames_de_fondu(cadence: u32) -> usize {
    ((f64::from(cadence) * DUREE_DU_FONDU_MS / 1000.0).round() as usize).max(1)
}

/// Le gain de la trame `k` (0 = la plus proche du bord) d'un fondu de `f`
/// trames : cosinus surélevé, 0 au bord exclu, 1 au-delà.
pub fn gain_de_fondu(k: usize, f: usize) -> f64 {
    if k >= f || f == 0 {
        return 1.0;
    }
    let x = (k as f64 + 0.5) / f as f64;
    0.5 - 0.5 * (std::f64::consts::PI * x).cos()
}

/// Le contexte à demander de chaque côté pour un étage de portée `portee`
/// trames d'entrée.
pub fn trames_de_contexte(portee: usize) -> usize {
    portee + MARGE_DE_CONTEXTE
}

/// rc15 (M2) — une rampe de niveau sur la piste : de `depart` à `cible`
/// (gains linéaires de l'étage), en cosinus, puis constante. Monotone : une
/// hausse ne dépasse jamais `cible`, une baisse ne passe jamais sous elle.
#[derive(Debug, Clone, PartialEq)]
pub struct RampeDeNiveau {
    depart: f64,
    cible: f64,
    trames: usize,
    faites: usize,
    canaux: usize,
}

impl RampeDeNiveau {
    pub fn nouvelle(depart: f64, cible: f64, cadence: u32, canaux: usize) -> Self {
        let duree = if cible > depart {
            DUREE_DE_MONTEE_S
        } else {
            DUREE_DE_DESCENTE_S
        };
        let trames = if (cible - depart).abs() < 1e-12 {
            0
        } else {
            ((f64::from(cadence) * duree).round() as usize).max(1)
        };
        Self {
            depart,
            cible,
            trames,
            faites: 0,
            canaux: canaux.max(1),
        }
    }

    /// Le gain de la trame `i` de la piste.
    pub fn gain(&self, i: usize) -> f64 {
        if i >= self.trames {
            return self.cible;
        }
        let x = (i as f64 + 0.5) / self.trames as f64;
        let w = 0.5 - 0.5 * (std::f64::consts::PI * x).cos();
        self.depart + (self.cible - self.depart) * w
    }

    /// Neutre de bout en bout (gain 1, aucune rampe) ?
    pub fn est_neutre(&self) -> bool {
        self.trames == 0 && (self.cible - 1.0).abs() < 1e-12
    }

    /// Applique la rampe aux trames entrelacées `y`, à la suite des
    /// précédentes.
    pub fn appliquer(&mut self, y: &mut [f64]) {
        let c = self.canaux;
        if self.faites >= self.trames {
            if (self.cible - 1.0).abs() > 1e-12 {
                for v in y.iter_mut() {
                    *v *= self.cible;
                }
            }
            self.faites = self.faites.saturating_add(y.len() / c);
            return;
        }
        for trame in y.chunks_exact_mut(c) {
            let g = self.gain(self.faites);
            for v in trame {
                *v *= g;
            }
            self.faites += 1;
        }
    }
}

/// rc15 (M2) — le niveau d'une piste sur une zone réseau, en gain TOTAL
/// (réserve du suréchantillonnage comprise) : d'où elle part (le niveau de la
/// piste précédente) et où elle va.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NiveauPlanifie {
    pub depart: f64,
    pub cible: f64,
}

impl NiveauPlanifie {
    /// La rampe de l'étage de niveau, à la cadence de la SOURCE. `surech` :
    /// la piste est suréchantillonnée, l'étage du greffon la porte déjà au
    /// niveau de la zone ; la rampe exprime donc le reste. `None` : rien à
    /// faire (gain 1 de bout en bout).
    pub fn rampe(&self, cadence: u32, canaux: usize, surech: bool) -> Option<RampeDeNiveau> {
        let base = if surech { su::gain_de_zone() } else { 1.0 };
        let r = RampeDeNiveau::nouvelle(self.depart / base, self.cible / base, cadence, canaux);
        (!r.est_neutre()).then_some(r)
    }

    /// Pour la clé de cache.
    pub(crate) fn empreinte(&self, h: &mut sha2::Sha256) {
        use sha2::Digest;
        h.update(b"niveau\0");
        h.update(self.depart.to_le_bytes());
        h.update(self.cible.to_le_bytes());
    }

    /// Change-t-il le signal ?
    pub fn agit(&self) -> bool {
        (self.depart - 1.0).abs() > 1e-12 || (self.cible - 1.0).abs() > 1e-12
    }
}

fn niveaux() -> &'static std::sync::Mutex<std::collections::HashMap<i64, f64>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i64, f64>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// rc15 (M2) — le niveau total de la dernière piste résolue pour la zone.
pub fn dernier_niveau(zone_id: i64) -> Option<f64> {
    niveaux()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zone_id)
        .copied()
}

/// rc15 (M2) — retient le niveau total de la piste qui vient d'être résolue.
pub fn noter_le_niveau(zone_id: i64, niveau: f64) {
    niveaux()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(zone_id, niveau);
}

/// rc15 (M2) — le plan de niveau d'une piste : cible `cible` (gain total), à
/// partir du niveau de la piste précédente sur la zone (la cible elle-même si
/// rien n'est connu). Retient la cible pour la piste suivante.
pub fn planifier_le_niveau(zone_id: i64, cible: f64) -> NiveauPlanifie {
    let depart = dernier_niveau(zone_id).unwrap_or(cible);
    noter_le_niveau(zone_id, cible);
    NiveauPlanifie { depart, cible }
}

/// rc15 (M2) — la cible d'une piste d'une zone de la famille UPnP : le niveau
/// de la zone si son lecteur se prête au suréchantillonnage (`apte`) ou si la
/// piste est suréchantillonnée, le niveau plein sinon.
pub fn cible_de_la_piste(apte: bool, surech: bool) -> f64 {
    if apte || surech {
        su::gain_de_zone()
    } else {
        1.0
    }
}

fn aptitudes() -> &'static std::sync::Mutex<std::collections::HashMap<i64, bool>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i64, bool>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// rc15 (M2) — retient si le lecteur de la zone se prête au
/// suréchantillonnage (réglage actif, lecteur sondé qui annonce ses
/// cadences) : décidé par la décision réseau de chaque piste.
pub fn noter_l_aptitude(zone_id: i64, apte: bool) {
    aptitudes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(zone_id, apte);
}

/// rc15 (M2) — voir [`noter_l_aptitude`]. Faux tant que rien n'est décidé.
pub fn zone_apte(zone_id: i64) -> bool {
    aptitudes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&zone_id)
        .copied()
        .unwrap_or(false)
}

/// Une voisine dans la file, vue de la piste en cours.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Voisine {
    pub(crate) chemin: String,
    pub(crate) track_id: i64,
    pub(crate) duree_ms: i64,
}

/// rc15 (M1) — les voisines de la piste `track_id` dans la file de la zone :
/// la ligne qui la précède et celle qui la suit. La ligne de la piste est la
/// ligne en cours si c'est elle, sinon la première qui la porte APRÈS la
/// ligne en cours (piste suivante préparée pendant que l'autre joue), sinon
/// la première qui la porte.
pub(crate) fn voisines_dans_la_file(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
    track_id: i64,
) -> (Option<Voisine>, Option<Voisine>) {
    let Ok(mut file) =
        crate::db::play_queue_repo::PlayQueueRepo::with_backend(db.clone()).get_queue(zone_id)
    else {
        return (None, None);
    };
    file.sort_by_key(|q| q.position);
    let courante = file.iter().position(|q| q.is_current);
    let ici = match courante {
        Some(i) if file[i].track_id == track_id => Some(i),
        Some(i) => file
            .iter()
            .enumerate()
            .skip(i + 1)
            .find(|(_, q)| q.track_id == track_id)
            .map(|(k, _)| k),
        None => None,
    }
    .or_else(|| file.iter().position(|q| q.track_id == track_id));
    let Some(ici) = ici else {
        return (None, None);
    };
    let voisine = |k: usize| -> Option<Voisine> {
        let q = file.get(k)?;
        Some(Voisine {
            chemin: q.file_path.clone()?,
            track_id: q.track_id,
            duree_ms: q.duration_ms.unwrap_or(0),
        })
    };
    (ici.checked_sub(1).and_then(voisine), voisine(ici + 1))
}

/// Décode `trames` trames d'un bord d'une voisine (`fin` : les dernières,
/// sinon les premières), à `cadence` et `canaux`, en `f64` multipliées par
/// `gain`. `None` : décodage impossible ou format différent (le bord sera
/// une rupture). Bloquant.
pub(crate) fn decoder_un_bord(
    v: &Voisine,
    cadence: u32,
    canaux: u16,
    trames: usize,
    fin: bool,
    gain: f64,
) -> Option<Vec<f64>> {
    let duree_ctx = trames as f64 / f64::from(cadence.max(1));
    let (debut, duree) = if fin {
        let d = v.duree_ms as f64 / 1000.0;
        if d <= 0.0 {
            return None;
        }
        ((d - duree_ctx - 1.0).max(0.0), 0.0)
    } else {
        (0.0, duree_ctx + 0.5)
    };
    let dec = crate::audio::decode::decode_to_pcm_routee(
        &v.chemin,
        Some(cadence),
        Some(u32::from(canaux)),
        debut,
        duree,
    )
    .ok()?;
    if dec.sample_rate != cadence || dec.channels != u32::from(canaux) {
        return None;
    }
    let c = usize::from(canaux).max(1);
    let n = dec.samples_i32.len() / c;
    if n < trames {
        return None;
    }
    let echelle = 2f64.powi(i32::from(dec.bit_depth.clamp(8, 32)) - 1);
    let tranche = if fin {
        &dec.samples_i32[(n - trames) * c..n * c]
    } else {
        &dec.samples_i32[..trames * c]
    };
    Some(
        tranche
            .iter()
            .map(|s| f64::from(*s) / echelle * gain)
            .collect(),
    )
}

#[cfg(test)]
#[path = "jonction_surech_reseau_tests.rs"]
mod tests;
