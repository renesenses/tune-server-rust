//! #6709 — les modes propres de la pièce, et les filtres qui les ATTÉNUENT.
//!
//! Proposition de Tades (fil 2364), go de Bertrand du 21/10. À partir des
//! dimensions de la pièce, de la place de l'auditeur et de celle des enceintes,
//! ce module calcule les ondes stationnaires et propose 5 à 10 filtres en
//! cloche entre 20 et 250 Hz.
//!
//! ## Il n'y a aucun DSP ici
//!
//! Comme [`super::autoeq`], ce module ne fait que des NOMBRES : des
//! [`EqBandSpec`] de type `peak`. L'écran les montre, l'auditeur les retouche,
//! puis ils partent par `POST /zones/{id}/eq`, le même chemin que l'égaliseur
//! paramétrique — même cascade, même préampli automatique, même fondu de
//! bascule (#5215). Rien n'est appliqué par ce module, et rien ne l'est sans
//! le geste de l'auditeur.
//!
//! ## Zone sensible : des ATTÉNUATIONS seulement
//!
//! Une bosse de mode se corrige en creusant, jamais en remplissant un creux :
//! un creux de mode est une annulation acoustique, et le pousser coûte de la
//! marge sans le combler. L'issue fixe donc les bornes, et ce module les tient
//! par construction :
//!
//! - gain de **−3 à −20 dB**, jamais positif ([`ATTENUATION_MIN_DB`],
//!   [`ATTENUATION_MAX_DB`]). Le gain n'est jamais CALCULÉ comme un nombre
//!   signé : c'est l'opposé d'une atténuation bornée ([`FiltreDeMode::gain_db`]) ;
//! - Q de **4 à 20** ([`Q_MIN`], [`Q_MAX`]) ;
//! - **5 à 10** filtres ([`FILTRES_MIN`], [`FILTRES_MAX`]), entre
//!   [`FREQUENCE_MIN_HZ`] et [`FREQUENCE_MAX_HZ`].
//!
//! Une entrée absurde (dimension nulle, négative, énorme, non finie, auditeur
//! hors de la pièce) est REFUSÉE ([`ErreurModes`]), jamais bornée en silence :
//! une pièce rabotée à 30 m donnerait des filtres faux sans le dire.
//!
//! ## Le modèle
//!
//! Pièce rectangulaire à parois rigides. Les modes sont
//!
//! ```text
//! f(nx, ny, nz) = c/2 · sqrt((nx/L)² + (ny/W)² + (nz/H)²),  c = 343 m/s
//! ```
//!
//! avec `L` la longueur (du mur des enceintes au mur arrière), `W` la largeur
//! et `H` la hauteur. Les trois familles sont comptées : **axiaux** (un seul
//! indice non nul), **tangentiels** (deux) et **obliques** (trois). Les modes
//! non axiaux portent moins d'énergie : −3 dB pour un tangentiel, −6 dB pour
//! un oblique (Morse ; Everest, *Master Handbook of Acoustics*). Les ignorer
//! laisserait de côté le tangentiel (1,1,0) d'une pièce presque carrée, qui
//! peut tomber entre deux axiaux et s'entendre autant qu'eux ; les compter à
//! plein en ferait sortir des dizaines au-dessus de 150 Hz qui ne dominent
//! jamais.
//!
//! **Pondération à la place d'écoute.** La pression d'un mode vaut
//! `cos(nx·π·x/L)·cos(ny·π·y/W)·cos(nz·π·z/H)`. Son couplage est le produit de
//! cette forme à l'enceinte (l'excitation) et à l'auditeur (l'écoute), en
//! valeur absolue : un auditeur sur un nœud n'entend pas le mode, une enceinte
//! sur un nœud ne l'excite pas. La place latérale des enceintes n'est pas une
//! entrée (deux enceintes, de part et d'autre) : leur facteur de largeur est
//! pris à 1, le cas le plus défavorable — on ne retire jamais un mode sur une
//! hypothèse qu'on n'a pas.
//!
//! **Largeur d'un mode.** La bande passante à mi-puissance d'un mode vaut
//! `Δf = 2,2 / RT60` ; le Q proposé est `f / Δf`, borné à [`Q_MIN`]..[`Q_MAX`].
//! Avec le RT60 par défaut de 0,4 s, Δf ≈ 5,5 Hz et un mode à 37,5 Hz reçoit
//! Q ≈ 6,8 — le « Q = 7 » de l'exemple de l'issue. Deux modes plus proches que
//! Δf ne font qu'une bosse : ils sont regroupés en un filtre, élargi d'autant.
//!
//! **Atténuation.** `3 + 9·s` dB, où `s` ∈ [0, 1] est le couplage pondéré : un
//! mode axial pleinement couplé reçoit −12 dB (l'exemple de l'issue), un mode
//! à peine couplé −3. La proposition s'arrête à −12 ([`ATTENUATION_PROPOSEE_MAX_DB`]) :
//! c'est un calcul sans mesure, et creuser de −20 dB sur la foi d'un modèle de
//! parois rigides serait plus risqué que de laisser l'auditeur descendre à la
//! main, jusqu'à −20, ce qu'il entend.

use serde::{Deserialize, Serialize};

use super::eq::EqBandSpec;

/// Vitesse du son dans l'air à 20 °C, m/s.
pub const VITESSE_DU_SON_M_S: f64 = 343.0;
/// Fréquence la plus haute traitée : au-delà, les modes se recouvrent.
pub const FREQUENCE_MAX_HZ: f64 = 250.0;
/// Fréquence la plus basse proposée : en dessous, une cloche ne sert plus à
/// rien (les enceintes n'y descendent pas, ou si peu).
pub const FREQUENCE_MIN_HZ: f64 = 20.0;
/// La plus petite atténuation d'un filtre proposé, en dB (positif).
pub const ATTENUATION_MIN_DB: f64 = 3.0;
/// La plus grande atténuation qu'un filtre de mode peut porter, en dB.
pub const ATTENUATION_MAX_DB: f64 = 20.0;
/// La plus grande atténuation que le CALCUL propose (voir le module).
pub const ATTENUATION_PROPOSEE_MAX_DB: f64 = 12.0;
/// Bornes du Q d'un filtre de mode.
pub const Q_MIN: f64 = 4.0;
pub const Q_MAX: f64 = 20.0;
/// Bornes et défaut du nombre de filtres demandés.
pub const FILTRES_MIN: usize = 5;
pub const FILTRES_MAX: usize = 10;
pub const FILTRES_DEFAUT: usize = 8;
/// Temps de réverbération supposé quand l'auditeur ne le donne pas : une
/// pièce de vie meublée.
pub const RT60_DEFAUT_S: f64 = 0.4;
pub const RT60_MIN_S: f64 = 0.2;
pub const RT60_MAX_S: f64 = 1.5;
/// Bornes d'une longueur ou d'une largeur de pièce, en mètres.
pub const COTE_MIN_M: f64 = 1.5;
pub const COTE_MAX_M: f64 = 30.0;
/// Bornes d'une hauteur sous plafond, en mètres.
pub const HAUTEUR_MIN_M: f64 = 1.8;
pub const HAUTEUR_MAX_M: f64 = 10.0;
/// Distance minimale d'un point (oreilles, enceinte) à une paroi, en mètres.
pub const ECART_A_LA_PAROI_MIN_M: f64 = 0.05;
/// L'auditeur est au moins à cette distance du plan des enceintes.
pub const ECART_ENCEINTE_AUDITEUR_MIN_M: f64 = 0.5;
/// Hauteur des oreilles d'un auditeur assis, quand elle n'est pas donnée.
pub const HAUTEUR_DES_OREILLES_DEFAUT_M: f64 = 1.1;
/// Hauteur du point d'émission (le haut-parleur de grave), par défaut.
pub const HAUTEUR_DE_L_ENCEINTE_DEFAUT_M: f64 = 0.8;
/// Couplage pondéré en deçà duquel un mode n'est pas proposé (−20 dB).
pub const SEUIL_DE_COUPLAGE: f64 = 0.1;
/// Pondération d'un mode tangentiel (−3 dB) et d'un oblique (−6 dB).
pub const POIDS_TANGENTIEL: f64 = 0.707_945_784_384_137_9;
pub const POIDS_OBLIQUE: f64 = 0.501_187_233_627_272_2;

/// Ce que l'auditeur décrit de sa pièce. Les noms sont ceux de l'API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Piece {
    /// Longueur, du mur des enceintes au mur arrière, m.
    pub length_m: f64,
    pub width_m: f64,
    pub height_m: f64,
    /// Distance entre le mur arrière et les oreilles, m.
    pub listener_from_back_wall_m: f64,
    /// Distance entre le mur avant et le point d'émission, m.
    pub speaker_from_front_wall_m: f64,
    /// Distance des oreilles au mur latéral ; défaut : le milieu de la pièce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_from_side_wall_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_height_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_height_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rt60_s: Option<f64>,
    /// Nombre de filtres voulus, de 5 à 10 ; défaut 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
}

/// Une entrée refusée. `champ` est le nom de l'API.
#[derive(Debug, Clone, PartialEq)]
pub enum ErreurModes {
    /// Une valeur non finie (NaN, infini).
    NonFini { champ: &'static str },
    /// Une valeur hors de ses bornes.
    HorsBornes {
        champ: &'static str,
        valeur: f64,
        min: f64,
        max: f64,
    },
    /// L'auditeur n'est pas devant les enceintes, ou trop près d'elles.
    PositionsIncompatibles { ecart_m: f64 },
}

impl ErreurModes {
    /// Clé stable, à traduire côté client.
    pub fn cle(&self) -> &'static str {
        match self {
            Self::NonFini { .. } => "room_value_not_finite",
            Self::HorsBornes { .. } => "room_value_out_of_bounds",
            Self::PositionsIncompatibles { .. } => "room_positions_incompatible",
        }
    }

    /// Le champ fautif, quand il y en a un.
    pub fn champ(&self) -> Option<&'static str> {
        match self {
            Self::NonFini { champ } | Self::HorsBornes { champ, .. } => Some(champ),
            Self::PositionsIncompatibles { .. } => None,
        }
    }
}

impl std::fmt::Display for ErreurModes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFini { champ } => write!(f, "{champ} : valeur non finie"),
            Self::HorsBornes {
                champ,
                valeur,
                min,
                max,
            } => write!(f, "{champ} = {valeur} : attendu entre {min} et {max}"),
            Self::PositionsIncompatibles { ecart_m } => write!(
                f,
                "l'auditeur doit être devant les enceintes, à {ECART_ENCEINTE_AUDITEUR_MIN_M} m \
                 au moins de leur plan (écart calculé : {ecart_m:.2} m)"
            ),
        }
    }
}

impl std::error::Error for ErreurModes {}

/// Famille d'un mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NatureDuMode {
    Axial,
    Tangential,
    Oblique,
}

impl NatureDuMode {
    fn depuis(nx: u32, ny: u32, nz: u32) -> Self {
        match [nx, ny, nz].iter().filter(|n| **n > 0).count() {
            1 => Self::Axial,
            2 => Self::Tangential,
            _ => Self::Oblique,
        }
    }

    /// Pondération d'énergie de la famille, en amplitude.
    pub fn poids(self) -> f64 {
        match self {
            Self::Axial => 1.0,
            Self::Tangential => POIDS_TANGENTIEL,
            Self::Oblique => POIDS_OBLIQUE,
        }
    }
}

/// Un mode propre de la pièce.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mode {
    /// Les indices (nx, ny, nz).
    pub n: [u32; 3],
    pub freq_hz: f64,
    pub nature: NatureDuMode,
    /// Couplage enceinte → auditeur, pondéré par la famille, dans [0, 1].
    pub couplage: f64,
}

/// Un filtre proposé. Le gain n'est PAS un champ : il se lit par
/// [`Self::gain_db`], qui ne peut pas être positif.
#[derive(Debug, Clone, PartialEq)]
pub struct FiltreDeMode {
    pub freq_hz: f64,
    /// Atténuation, en dB POSITIFS, toujours dans
    /// [`ATTENUATION_MIN_DB`]..=[`ATTENUATION_MAX_DB`].
    attenuation_db: f64,
    pub q: f64,
    /// Les modes que ce filtre couvre.
    pub modes: Vec<[u32; 3]>,
}

impl FiltreDeMode {
    /// Un filtre de `attenuation_db` dB d'atténuation. La valeur est bornée
    /// ici, au seul endroit où un filtre se construit : une atténuation
    /// négative, nulle ou non finie devient [`ATTENUATION_MIN_DB`], une trop
    /// grande [`ATTENUATION_MAX_DB`].
    pub fn new(freq_hz: f64, attenuation_db: f64, q: f64, modes: Vec<[u32; 3]>) -> Self {
        let attenuation_db = if attenuation_db.is_finite() {
            attenuation_db.clamp(ATTENUATION_MIN_DB, ATTENUATION_MAX_DB)
        } else {
            ATTENUATION_MIN_DB
        };
        let q = if q.is_finite() {
            q.clamp(Q_MIN, Q_MAX)
        } else {
            Q_MIN
        };
        Self {
            freq_hz,
            attenuation_db,
            q,
            modes,
        }
    }

    /// Le gain du filtre, en dB : toujours entre −20 et −3.
    pub fn gain_db(&self) -> f64 {
        -self.attenuation_db
    }

    pub fn attenuation_db(&self) -> f64 {
        self.attenuation_db
    }

    /// La bande d'égaliseur, sur tous les canaux.
    pub fn bande(&self) -> EqBandSpec {
        EqBandSpec {
            freq: self.freq_hz,
            gain: self.gain_db(),
            q: self.q,
            band_type: "peak".into(),
            channel: None,
        }
    }
}

/// Les positions retenues, en mètres, dans le repère de la pièce : `x` depuis
/// le mur des enceintes, `y` depuis un mur latéral, `z` depuis le sol.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Point {
    pub x_m: f64,
    pub y_m: Option<f64>,
    pub z_m: f64,
}

/// Une pièce lue, contrôlée et complétée de ses défauts.
#[derive(Debug, Clone, PartialEq)]
pub struct PieceValidee {
    pub longueur_m: f64,
    pub largeur_m: f64,
    pub hauteur_m: f64,
    pub auditeur: Point,
    /// `y_m` est `None` : la place latérale des enceintes n'est pas une entrée.
    pub enceinte: Point,
    pub rt60_s: f64,
    pub nombre: usize,
}

/// Ce que le calcul rend.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposition {
    pub piece: PieceValidee,
    /// Tous les modes entre [`FREQUENCE_MIN_HZ`] et [`FREQUENCE_MAX_HZ`], par
    /// fréquence croissante.
    pub modes: Vec<Mode>,
    /// Les filtres proposés, par fréquence croissante.
    pub filtres: Vec<FiltreDeMode>,
}

impl Proposition {
    pub fn bandes(&self) -> Vec<EqBandSpec> {
        self.filtres.iter().map(FiltreDeMode::bande).collect()
    }
}

fn fini(champ: &'static str, v: f64) -> Result<f64, ErreurModes> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(ErreurModes::NonFini { champ })
    }
}

fn borne(champ: &'static str, v: f64, min: f64, max: f64) -> Result<f64, ErreurModes> {
    let v = fini(champ, v)?;
    if v < min || v > max {
        return Err(ErreurModes::HorsBornes {
            champ,
            valeur: v,
            min,
            max,
        });
    }
    Ok(v)
}

/// Une position le long d'un côté : à [`ECART_A_LA_PAROI_MIN_M`] au moins de
/// chaque paroi.
fn dans_le_cote(champ: &'static str, v: f64, cote: f64) -> Result<f64, ErreurModes> {
    borne(
        champ,
        v,
        ECART_A_LA_PAROI_MIN_M,
        cote - ECART_A_LA_PAROI_MIN_M,
    )
}

impl Piece {
    /// Contrôle chaque champ et complète les défauts. Rien n'est borné en
    /// silence : une valeur hors de ses bornes est refusée.
    pub fn valider(&self) -> Result<PieceValidee, ErreurModes> {
        let l = borne("length_m", self.length_m, COTE_MIN_M, COTE_MAX_M)?;
        let w = borne("width_m", self.width_m, COTE_MIN_M, COTE_MAX_M)?;
        let h = borne("height_m", self.height_m, HAUTEUR_MIN_M, HAUTEUR_MAX_M)?;
        let arriere = dans_le_cote(
            "listener_from_back_wall_m",
            self.listener_from_back_wall_m,
            l,
        )?;
        let avant = dans_le_cote(
            "speaker_from_front_wall_m",
            self.speaker_from_front_wall_m,
            l,
        )?;
        let x_auditeur = l - arriere;
        let ecart = x_auditeur - avant;
        if ecart < ECART_ENCEINTE_AUDITEUR_MIN_M {
            return Err(ErreurModes::PositionsIncompatibles { ecart_m: ecart });
        }
        let y_auditeur = match self.listener_from_side_wall_m {
            Some(y) => dans_le_cote("listener_from_side_wall_m", y, w)?,
            None => w / 2.0,
        };
        let z_auditeur = match self.listener_height_m {
            Some(z) => dans_le_cote("listener_height_m", z, h)?,
            None => HAUTEUR_DES_OREILLES_DEFAUT_M.min(h - ECART_A_LA_PAROI_MIN_M),
        };
        let z_enceinte = match self.speaker_height_m {
            Some(z) => dans_le_cote("speaker_height_m", z, h)?,
            None => HAUTEUR_DE_L_ENCEINTE_DEFAUT_M.min(h - ECART_A_LA_PAROI_MIN_M),
        };
        let rt60_s = match self.rt60_s {
            Some(t) => borne("rt60_s", t, RT60_MIN_S, RT60_MAX_S)?,
            None => RT60_DEFAUT_S,
        };
        let nombre = match self.count {
            Some(n) if (FILTRES_MIN as i64..=FILTRES_MAX as i64).contains(&n) => n as usize,
            Some(n) => {
                return Err(ErreurModes::HorsBornes {
                    champ: "count",
                    valeur: n as f64,
                    min: FILTRES_MIN as f64,
                    max: FILTRES_MAX as f64,
                });
            }
            None => FILTRES_DEFAUT,
        };
        Ok(PieceValidee {
            longueur_m: l,
            largeur_m: w,
            hauteur_m: h,
            auditeur: Point {
                x_m: x_auditeur,
                y_m: Some(y_auditeur),
                z_m: z_auditeur,
            },
            enceinte: Point {
                x_m: avant,
                y_m: None,
                z_m: z_enceinte,
            },
            rt60_s,
            nombre,
        })
    }
}

/// `f = c/2 · sqrt((nx/L)² + (ny/W)² + (nz/H)²)`.
pub fn frequence_du_mode(l: f64, w: f64, h: f64, n: [u32; 3]) -> f64 {
    let [nx, ny, nz] = n.map(f64::from);
    VITESSE_DU_SON_M_S / 2.0 * ((nx / l).powi(2) + (ny / w).powi(2) + (nz / h).powi(2)).sqrt()
}

/// La forme de pression d'un mode le long d'un côté, en `x` sur `cote`.
fn forme(n: u32, x: f64, cote: f64) -> f64 {
    (f64::from(n) * std::f64::consts::PI * x / cote).cos()
}

/// Tous les modes de la pièce entre [`FREQUENCE_MIN_HZ`] et
/// [`FREQUENCE_MAX_HZ`], avec leur couplage, par fréquence croissante.
pub fn modes_propres(p: &PieceValidee) -> Vec<Mode> {
    let (l, w, h) = (p.longueur_m, p.largeur_m, p.hauteur_m);
    // n/côté ≤ 2·fmax/c : au-delà, l'indice seul dépasse déjà la borne.
    let rang = |cote: f64| (2.0 * FREQUENCE_MAX_HZ * cote / VITESSE_DU_SON_M_S).floor() as u32;
    let mut modes = Vec::new();
    for nx in 0..=rang(l) {
        for ny in 0..=rang(w) {
            for nz in 0..=rang(h) {
                if nx == 0 && ny == 0 && nz == 0 {
                    continue;
                }
                let n = [nx, ny, nz];
                let f = frequence_du_mode(l, w, h, n);
                if !(FREQUENCE_MIN_HZ..=FREQUENCE_MAX_HZ).contains(&f) {
                    continue;
                }
                let nature = NatureDuMode::depuis(nx, ny, nz);
                let a = &p.auditeur;
                let e = &p.enceinte;
                let a_l_ecoute = forme(nx, a.x_m, l)
                    * a.y_m.map_or(1.0, |y| forme(ny, y, w))
                    * forme(nz, a.z_m, h);
                // Place latérale des enceintes inconnue : facteur 1.
                let a_l_emission = forme(nx, e.x_m, l) * forme(nz, e.z_m, h);
                let couplage = (a_l_ecoute * a_l_emission).abs() * nature.poids();
                modes.push(Mode {
                    n,
                    freq_hz: f,
                    nature,
                    couplage,
                });
            }
        }
    }
    modes.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    modes
}

/// Bande passante à mi-puissance d'un mode, Hz.
fn largeur_du_mode(rt60_s: f64) -> f64 {
    2.2 / rt60_s
}

/// Le calcul entier : contrôle, modes, regroupement, choix des filtres.
pub fn proposer(piece: &Piece) -> Result<Proposition, ErreurModes> {
    let p = piece.valider()?;
    let modes = modes_propres(&p);
    let largeur = largeur_du_mode(p.rt60_s);

    // Regroupement des modes couplés : deux modes plus proches que la
    // largeur d'un mode ne font qu'une bosse.
    struct Groupe {
        energie: f64,
        somme_f: f64,
        f_min: f64,
        f_max: f64,
        modes: Vec<[u32; 3]>,
    }
    let mut groupes: Vec<Groupe> = Vec::new();
    for m in modes.iter().filter(|m| m.couplage >= SEUIL_DE_COUPLAGE) {
        let e = m.couplage * m.couplage;
        match groupes.last_mut() {
            Some(g) if m.freq_hz - g.somme_f / g.energie < largeur => {
                g.energie += e;
                g.somme_f += e * m.freq_hz;
                g.f_max = m.freq_hz;
                g.modes.push(m.n);
            }
            _ => groupes.push(Groupe {
                energie: e,
                somme_f: e * m.freq_hz,
                f_min: m.freq_hz,
                f_max: m.freq_hz,
                modes: vec![m.n],
            }),
        }
    }
    groupes.sort_by(|a, b| b.energie.total_cmp(&a.energie));
    groupes.truncate(p.nombre);

    let mut filtres: Vec<FiltreDeMode> = groupes
        .into_iter()
        .map(|g| {
            let f = g.somme_f / g.energie;
            let s = g.energie.sqrt().min(1.0);
            let attenuation =
                ATTENUATION_MIN_DB + (ATTENUATION_PROPOSEE_MAX_DB - ATTENUATION_MIN_DB) * s;
            let q = f / (largeur + (g.f_max - g.f_min));
            FiltreDeMode::new(f, attenuation, q, g.modes)
        })
        .collect();
    filtres.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));

    Ok(Proposition {
        piece: p,
        modes,
        filtres,
    })
}

#[cfg(test)]
#[path = "modes_de_piece_tests.rs"]
mod tests;
