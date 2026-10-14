//! #6171 — les **canaux de la carte** qu'une zone ASIO utilise.
//!
//! Jean-François (fil 2018, rc3) : « une carte pouvant contenir plusieurs
//! sorties, il faudrait pouvoir mapper la sortie audio avec les entrées de la
//! carte ». Ses deux cas : un casque sur les canaux 3-4 d'une carte à quatre
//! sorties ; un 9.1.6 dont les canaux ne sont ni consécutifs ni dans l'ordre
//! du fichier, et un casque en 17-18 après les seize canaux du 9.1.6.
//!
//! # Le réglage
//!
//! `zone_{id}_output_channels` : un tableau JSON de numéros de canaux de la
//! carte, **à partir de 1** comme sur la face avant et dans le panneau du
//! pilote. Le canal `i` de la piste (0 = gauche, 1 = droite, puis l'ordre du
//! fichier) sort sur le canal `numeros[i]` de la carte. `[3, 4]` met une piste
//! stéréo sur le casque ; `[1, 2, 5, 6, 3, 4, 7, 8]` réordonne un 7.1. Absent,
//! `null` ou `[]` : le comportement d'avant, les premiers canaux de la carte.
//!
//! # Ce qu'ASIO permet, et ce que Tune en fait
//!
//! `ASIOCreateBuffers` reçoit une liste d'`ASIOBufferInfo`, chacun portant un
//! `channelNum` arbitraire : le SDK permet d'ouvrir un sous-ensemble de canaux.
//! Mais cpal (vendorisé dans `vendor/cpal`, `vendor/asio-sys`) ne l'expose pas :
//! `prepare_buffer_infos` crée toujours les canaux `0..n`, et `StreamConfig`
//! ne porte qu'un nombre de canaux. Plutôt que de modifier la pile cpal, Tune
//! ouvre la carte sur `max(numeros)` canaux — la **largeur** — et place chaque
//! canal de la piste à sa position dans le rappel ; les canaux de la largeur
//! que la zone ne vise pas reçoivent des zéros. Aucun mot n'est modifié : la
//! route native reste bit-perfect.
//!
//! Conséquence assumée : un pilote ASIO n'a qu'un client à la fois (et Tune
//! sérialise déjà l'ouverture, `ASIO_DEVICE_LOCK`). Deux zones sur la même
//! carte, l'une sur 1-2 et l'autre sur 3-4, ne peuvent pas jouer EN MÊME
//! TEMPS ; elles se relaient. Jouer les deux à la fois demanderait un mélangeur
//! commun à toutes les zones d'un pilote — un autre chantier.
//!
//! Tout ici est pur et se teste sous Linux ; le rappel ASIO
//! (`asio_exclusive.rs`, Windows + `asio`) n'appelle que [`remplir_le_tampon`].
use serde_json::Value;

/// Borne des numéros de canaux : bien au-delà des plus grosses cartes ASIO
/// (MADI : 64 à 128 canaux, Dante : 512 annoncés par certains pilotes).
pub const NUMERO_MAX: u16 = 512;

/// Trames traitées par passe dans le rappel : le tampon intermédiaire est
/// alloué UNE fois, à l'ouverture, jamais dans le rappel temps réel.
pub const TRAMES_PAR_PASSE: usize = 1024;

/// La clé du réglage d'une zone.
pub fn cle_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_output_channels")
}

/// Pourquoi un réglage est refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurDeCanaux {
    /// Ni un tableau, ni `null`.
    Forme,
    /// Un élément n'est pas un entier de 1 à [`NUMERO_MAX`].
    HorsBorne(String),
    /// Le même canal de la carte apparaît deux fois.
    Doublon(u16),
}

impl std::fmt::Display for ErreurDeCanaux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forme => write!(f, "un tableau de numéros de canaux est attendu"),
            Self::HorsBorne(v) => {
                write!(f, "canal {v} : un entier de 1 à {NUMERO_MAX} est attendu")
            }
            Self::Doublon(n) => write!(f, "le canal {n} apparaît deux fois"),
        }
    }
}

/// Les canaux de la carte visés par une zone, dans l'ordre des canaux de la
/// piste. Stockés à partir de 0 ; publiés et saisis à partir de 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanauxDeCarte {
    indices: Vec<u16>,
}

impl CanauxDeCarte {
    /// Depuis des numéros à partir de 1. Vide : `None` (rien de réglé).
    pub fn depuis_numeros(numeros: &[u16]) -> Result<Option<Self>, ErreurDeCanaux> {
        let mut indices = Vec::with_capacity(numeros.len());
        for &n in numeros {
            if n == 0 || n > NUMERO_MAX {
                return Err(ErreurDeCanaux::HorsBorne(n.to_string()));
            }
            if indices.contains(&(n - 1)) {
                return Err(ErreurDeCanaux::Doublon(n));
            }
            indices.push(n - 1);
        }
        Ok((!indices.is_empty()).then_some(Self { indices }))
    }

    /// Depuis la valeur JSON du réglage (ou du corps d'une requête). `null`
    /// et `[]` valent « rien de réglé ».
    pub fn depuis_json(valeur: &Value) -> Result<Option<Self>, ErreurDeCanaux> {
        let elements = match valeur {
            Value::Null => return Ok(None),
            Value::Array(a) => a,
            _ => return Err(ErreurDeCanaux::Forme),
        };
        let mut numeros = Vec::with_capacity(elements.len());
        for e in elements {
            let n = e
                .as_u64()
                .filter(|n| (1..=u64::from(NUMERO_MAX)).contains(n))
                .ok_or_else(|| ErreurDeCanaux::HorsBorne(e.to_string()))?;
            numeros.push(n as u16);
        }
        Self::depuis_numeros(&numeros)
    }

    /// Le réglage d'une zone, tel qu'en base. Illisible : `None`, le
    /// comportement d'avant — la route d'écriture refuse ce qui ne se relit pas.
    pub fn lire(
        backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> Option<Self> {
        let brut = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .get(&cle_zone(zone_id))
            .ok()
            .flatten()?;
        let valeur = serde_json::from_str::<Value>(&brut).ok()?;
        Self::depuis_json(&valeur).ok().flatten()
    }

    /// Les numéros, à partir de 1 — la forme publiée.
    pub fn numeros(&self) -> Vec<u16> {
        self.indices.iter().map(|i| i + 1).collect()
    }

    /// Les index, à partir de 0 — ce que voit le rappel.
    pub fn indices(&self) -> &[u16] {
        &self.indices
    }

    /// Le nombre de canaux à ouvrir sur la carte : le plus grand numéro visé.
    pub fn largeur(&self) -> u16 {
        self.indices.iter().copied().max().map_or(0, |m| m + 1)
    }
}

/// Où va chaque canal de la piste dans une trame de la carte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDeCanaux {
    /// Canaux par trame dans l'anneau : ceux de la piste.
    canaux_source: usize,
    /// Canaux par trame ouverts sur la carte.
    largeur: usize,
    /// `cible[c]` : la position du canal `c` de la piste dans la trame de la
    /// carte.
    cible: Vec<usize>,
}

impl PlanDeCanaux {
    pub fn canaux_source(&self) -> usize {
        self.canaux_source
    }
    pub fn largeur(&self) -> usize {
        self.largeur
    }
    pub fn cible(&self) -> &[usize] {
        &self.cible
    }
}

/// Le plan d'une ouverture : `None` quand il n'y a rien à faire — aucun
/// réglage, ou un réglage qui vaut les premiers canaux dans l'ordre (`[1, 2]`
/// pour une piste stéréo) : le chemin d'avant, au mot près.
///
/// Refusé, avec un message que l'écran affiche tel quel, quand la piste a plus
/// de canaux que la zone n'en vise (Tune ne jette pas un canal en silence) ou
/// quand la carte n'a pas assez de sorties.
pub fn planifier(
    canaux: Option<&CanauxDeCarte>,
    canaux_source: u16,
    sorties_de_la_carte: Option<u16>,
) -> Result<Option<PlanDeCanaux>, String> {
    let Some(canaux) = canaux else {
        return Ok(None);
    };
    let n = usize::from(canaux_source);
    if n == 0 {
        return Ok(None);
    }
    let visees = canaux.indices();
    if n > visees.len() {
        return Err(format!(
            "la piste a {n} canaux, mais la zone n'envoie que {} canaux à la carte \
             (canaux {}) : ajoutez des canaux de sortie ou retirez le réglage",
            visees.len(),
            liste(&canaux.numeros()),
        ));
    }
    let cible: Vec<usize> = visees[..n].iter().map(|&i| usize::from(i)).collect();
    let largeur = cible.iter().copied().max().map_or(0, |m| m + 1);
    if let Some(sorties) = sorties_de_la_carte {
        if largeur > usize::from(sorties) {
            return Err(format!(
                "la zone vise le canal {largeur} de la carte, qui n'a que {sorties} sorties"
            ));
        }
    }
    if largeur == n && cible.iter().enumerate().all(|(c, &p)| c == p) {
        return Ok(None);
    }
    Ok(Some(PlanDeCanaux {
        canaux_source: n,
        largeur,
        cible,
    }))
}

fn liste(numeros: &[u16]) -> String {
    numeros
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// La dispersion d'un rappel : le plan et le tampon intermédiaire, alloué à
/// l'ouverture.
pub struct Dispersion<T> {
    plan: PlanDeCanaux,
    tampon: Vec<T>,
}

impl<T: Copy> Dispersion<T> {
    pub fn new(plan: PlanDeCanaux, zero: T) -> Self {
        let tampon = vec![zero; TRAMES_PAR_PASSE * plan.canaux_source];
        Self { plan, tampon }
    }

    /// Remplit `data` (trames de la carte, `largeur` mots chacune) à partir
    /// de ce que `tirer` rend (trames de la piste). Rend `(lus, voulus)` en
    /// mots de la piste : `lus < voulus` est une famine de l'anneau.
    pub fn rendre(
        &mut self,
        data: &mut [T],
        zero: T,
        mut tirer: impl FnMut(&mut [T]) -> usize,
    ) -> (usize, usize) {
        let largeur = self.plan.largeur;
        let n = self.plan.canaux_source;
        data.fill(zero);
        if largeur == 0 || n == 0 {
            return (0, 0);
        }
        let trames = data.len() / largeur;
        let (mut lus, mut voulus) = (0, 0);
        let mut faites = 0;
        while faites < trames {
            let passe = (trames - faites).min(TRAMES_PAR_PASSE);
            let mots = passe * n;
            let tampon = &mut self.tampon[..mots];
            let lu = tirer(tampon).min(mots);
            tampon[lu..].fill(zero);
            lus += lu;
            voulus += mots;
            for t in 0..passe {
                let source = &tampon[t * n..(t + 1) * n];
                let trame = &mut data[(faites + t) * largeur..(faites + t + 1) * largeur];
                for (c, &p) in self.plan.cible.iter().enumerate() {
                    trame[p] = source[c];
                }
            }
            faites += passe;
        }
        (lus, voulus)
    }
}

/// Ce que fait chaque rappel ASIO : sans dispersion, `tirer` remplit `data`
/// directement et le reste est mis à zéro (le chemin d'avant) ; avec, la
/// dispersion place les canaux. Rend `true` sur une famine de l'anneau.
#[inline]
pub fn remplir_le_tampon<T: Copy>(
    data: &mut [T],
    dispersion: &mut Option<Dispersion<T>>,
    zero: T,
    mut tirer: impl FnMut(&mut [T]) -> usize,
) -> bool {
    match dispersion {
        Some(d) => {
            let (lus, voulus) = d.rendre(data, zero, tirer);
            lus < voulus
        }
        None => {
            let lu = tirer(data).min(data.len());
            data[lu..].fill(zero);
            lu < data.len()
        }
    }
}

#[cfg(test)]
#[path = "canaux_de_carte_6171_tests.rs"]
mod tests;
