//! #8761 — un jeu de réponses impulsionnelles par zone, une par cadence, et
//! le rééchantillonnage du filtre en repli.
//!
//! Avant : UNE réponse impulsionnelle par zone, et un refus dès que sa cadence
//! différait de celle du flux (« réexportez le filtre à N Hz ») — la piste
//! jouait alors SANS correction. Un testeur qui écoute du 44,1 kHz et du
//! 96 kHz devait choisir lequel des deux corriger (Begastor, forum-hifi,
//! 14/11 et 20/11).
//!
//! Maintenant, pour une cadence de flux donnée :
//!
//! 1. un fichier du jeu à la cadence EXACTE est pris tel quel ;
//! 2. sinon, si le réglage le permet (par défaut oui), le fichier le plus
//!    proche est RÉÉCHANTILLONNÉ, une fois, et le résultat est gardé en
//!    cache. « Le plus proche » préfère d'abord un rapport de cadences en
//!    puissance de deux (même famille : 44,1 → 88,2 → 176,4, ou 48 → 96 →
//!    192), puis le plus petit écart en octaves, puis, à écart égal, le
//!    fichier de cadence plus HAUTE (on descend plutôt que de monter : rien
//!    n'est inventé au-dessus du Nyquist du fichier d'origine) ;
//! 3. sinon (rééchantillonnage refusé par le réglage), le refus d'avant.
//!
//! Le rééchantillonneur est celui de toute la chaîne (`audio::resample`,
//! rubato sinc, fenêtre Blackman², noyau symétrique) : un FIR à phase
//! LINÉAIRE, donc un retard pur, sans IIR ni distorsion de phase. Le filtre
//! est complété de zéros de part et d'autre avant de passer au noyau, pour
//! que ni l'amorce ni la queue du noyau sinc ne soient tronquées ; l'amorce
//! ajoute un retard ENTIER de quelques centaines d'échantillons, le même sur
//! tous les canaux (voir [`marge_d_amorce`]). Les coefficients sont mis à
//! l'échelle `de / vers` : une RI discrète porte son gain dans la SOMME de
//! ses coefficients, et doubler la cadence double leur nombre.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// Les cadences qu'une sortie locale prépare à l'avance, en tâche de fond,
/// dès qu'un jeu est installé : un changement de cadence ENTRE deux pistes
/// enchaînées (gapless) ne doit pas payer le rééchantillonnage sur le fil du
/// producteur audio.
pub const CADENCES_USUELLES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Réglage : chemins des fichiers SUPPLÉMENTAIRES de la zone (tableau JSON de
/// chaînes), en plus du fichier principal `ir_path_{zone}`.
pub const CLE_JEU: &str = "ir_jeu_";
/// Réglage : `"false"` refuse le rééchantillonnage en repli (comportement
/// d'avant #8761). Absent ou toute autre valeur : autorisé.
pub const CLE_REECHANTILLONNAGE: &str = "ir_resample_";

/// Une réponse impulsionnelle du jeu, à sa cadence.
#[derive(Clone)]
pub(super) struct Variante {
    pub(super) taps: Arc<[Vec<f32>]>,
    pub(super) sample_rate: u32,
    pub(super) chemin: Option<String>,
    /// Identité du CONTENU pour le cache : chemin + taille + date de
    /// modification pour un fichier, un numéro unique pour une RI en mémoire.
    pub(super) cle: String,
}

static PROCHAIN_NUMERO: AtomicU64 = AtomicU64::new(1);

impl Variante {
    pub(super) fn en_memoire(taps: Arc<[Vec<f32>]>, sample_rate: u32) -> Self {
        Self {
            taps,
            sample_rate,
            chemin: None,
            cle: format!(
                "mem#{}",
                PROCHAIN_NUMERO.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    pub(super) fn depuis_fichier(chemin: &str) -> Result<Self, String> {
        let (taps, sample_rate) = Convolver::read_wav_ir(chemin)?;
        if sample_rate == 0 {
            return Err(format!("{chemin} : cadence nulle"));
        }
        if taps.is_empty() || taps.iter().any(Vec::is_empty) {
            return Err(format!("{chemin} : réponse impulsionnelle vide"));
        }
        let meta = std::fs::metadata(chemin).ok();
        let taille = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let date = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(Self {
            taps: taps.into(),
            sample_rate,
            chemin: Some(chemin.to_string()),
            cle: format!("{chemin}|{taille}|{date}"),
        })
    }
}

/// Le fichier retenu pour une cadence de flux, et pourquoi. C'est l'« état
/// effectif » que publient le journal, `GET /room-correction/ir/status` et le
/// chemin du signal.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChoixRi {
    /// La cadence du flux que le convolveur traite.
    pub cadence_flux: u32,
    /// La cadence du fichier retenu.
    pub cadence_fichier: u32,
    /// Le fichier retenu (`None` pour une RI construite en mémoire).
    pub chemin: Option<String>,
    /// Vrai si le fichier a été rééchantillonné de `cadence_fichier` vers
    /// `cadence_flux`.
    pub reechantillonne: bool,
    /// `exact` (fichier à la cadence du flux), `same_family` (rapport en
    /// puissance de deux) ou `nearest` (le plus proche, autre famille).
    pub raison: &'static str,
}

impl ChoixRi {
    /// Une phrase pour le journal et l'interface.
    pub fn decrire(&self) -> String {
        let khz = |r: u32| format!("{:.1} kHz", r as f64 / 1000.0).replace(".0 kHz", " kHz");
        if self.reechantillonne {
            format!(
                "filtre rééchantillonné de {} à {}",
                khz(self.cadence_fichier),
                khz(self.cadence_flux)
            )
        } else {
            format!("filtre à {}", khz(self.cadence_flux))
        }
    }
}

/// `true` si `a / b` ou `b / a` est une puissance de deux (1 compris).
fn rapport_en_puissance_de_deux(a: u32, b: u32) -> bool {
    let (grand, petit) = if a >= b { (a, b) } else { (b, a) };
    petit > 0 && grand.is_multiple_of(petit) && (grand / petit).is_power_of_two()
}

/// L'indice de la variante à employer pour `cible`, et la raison. Les
/// variantes de même cadence ont déjà été dédoublonnées par l'appelant.
pub(super) fn planifier(variantes: &[Variante], cible: u32) -> Option<(usize, &'static str)> {
    if let Some(i) = variantes.iter().position(|v| v.sample_rate == cible) {
        return Some((i, "exact"));
    }
    let cle = |v: &Variante| {
        let famille = rapport_en_puissance_de_deux(v.sample_rate, cible);
        let ecart = (v.sample_rate as f64 / cible as f64).log2().abs();
        // À écart égal, le fichier de cadence plus haute (descendre).
        (!famille, ecart, v.sample_rate < cible)
    };
    let (i, v) = variantes.iter().enumerate().min_by(|(_, a), (_, b)| {
        let (fa, ea, ha) = cle(a);
        let (fb, eb, hb) = cle(b);
        fa.cmp(&fb)
            .then(ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal))
            .then(ha.cmp(&hb))
    })?;
    let raison = if rapport_en_puissance_de_deux(v.sample_rate, cible) {
        "same_family"
    } else {
        "nearest"
    };
    Some((i, raison))
}

/// Zéros ajoutés en tête (et en queue) avant le noyau sinc, en trames
/// d'ENTRÉE : au moins la demi-longueur du noyau, arrondie à un multiple de
/// `de / pgcd(de, vers)` pour que le retard ajouté soit un nombre ENTIER de
/// trames de sortie (`marge · vers / de`). Rend `(marge_entree, marge_sortie)`.
pub fn marge_d_amorce(de: u32, vers: u32) -> (usize, usize) {
    fn pgcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    let g = pgcd(de as u64, vers as u64).max(1);
    let pas = (de as u64 / g) as usize;
    let demi_noyau = crate::audio::resample::parametres_sinc(de, vers).sinc_len / 2 + 2;
    let marge = demi_noyau.div_ceil(pas) * pas;
    let sortie = (marge as u64 * vers as u64 / de as u64) as usize;
    (marge, sortie)
}

/// Rééchantillonner une réponse impulsionnelle de `de` à `vers` Hz.
///
/// Rubato sinc (le rééchantillonneur de la chaîne, phase linéaire), sur le
/// filtre complété de [`marge_d_amorce`] zéros de chaque côté ; coefficients
/// mis à l'échelle `de / vers` pour conserver la réponse en fréquence. Le
/// résultat est retardé de `marge_d_amorce(de, vers).1` trames, sur tous les
/// canaux à l'identique.
pub fn reechantillonner_ri(taps: &[Vec<f32>], de: u32, vers: u32) -> Vec<Vec<f32>> {
    if de == vers || taps.is_empty() {
        return taps.to_vec();
    }
    let canaux = taps.len();
    let longueur = taps.iter().map(Vec::len).max().unwrap_or(0);
    let (marge, _) = marge_d_amorce(de, vers);
    let trames = marge + longueur + marge;
    let mut entrelace = vec![0.0f32; trames * canaux];
    for (c, canal) in taps.iter().enumerate() {
        for (i, &h) in canal.iter().enumerate() {
            entrelace[(marge + i) * canaux + c] = h;
        }
    }
    let sortie =
        crate::audio::resample::rubato_resample_track(&entrelace, de, vers, canaux as u16);
    let echelle = de as f32 / vers as f32;
    let n = sortie.len() / canaux;
    (0..canaux)
        .map(|c| (0..n).map(|i| sortie[i * canaux + c] * echelle).collect())
        .collect()
}

/// Le cache des RI rééchantillonnées, partagé par toutes les sorties : clé =
/// (identité du contenu, cadence visée). Borné : au-delà, on repart de zéro
/// (une RI de 65 536 prises stéréo pèse 0,5 Mo ; 32 entrées, 16 Mo au pire).
const CACHE_MAX: usize = 32;
type CleCache = (String, u32);
static CACHE: LazyLock<Mutex<HashMap<CleCache, Arc<[Vec<f32>]>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// La RI de `variante` à la cadence `vers`, depuis le cache ou calculée (le
/// calcul se fait HORS du verrou : un rééchantillonnage en cours ne bloque
/// pas un autre lecteur du cache).
pub(super) fn ri_a_la_cadence(variante: &Variante, vers: u32) -> Arc<[Vec<f32>]> {
    if variante.sample_rate == vers {
        return variante.taps.clone();
    }
    let cle = (variante.cle.clone(), vers);
    if let Some(ri) = CACHE.lock().ok().and_then(|c| c.get(&cle).cloned()) {
        return ri;
    }
    let debut = std::time::Instant::now();
    let ri: Arc<[Vec<f32>]> = reechantillonner_ri(&variante.taps, variante.sample_rate, vers).into();
    tracing::info!(
        chemin = variante.chemin.as_deref().unwrap_or("(mémoire)"),
        de = variante.sample_rate,
        vers,
        taps_avant = variante.taps.first().map(Vec::len).unwrap_or(0),
        taps_apres = ri.first().map(Vec::len).unwrap_or(0),
        ms = debut.elapsed().as_secs_f64() * 1000.0,
        "convolver_ir_reechantillonnee"
    );
    if let Ok(mut c) = CACHE.lock() {
        if c.len() >= CACHE_MAX && !c.contains_key(&cle) {
            c.clear();
        }
        // Un calcul concurrent a pu passer devant : garder le premier, pour
        // que tous les appelants partagent la même instance.
        return c.entry(cle).or_insert(ri).clone();
    }
    ri
}

/// L'état effectif par jeu (clé : le chemin du fichier principal), écrit à
/// chaque construction d'un moteur. Lu par `GET /room-correction/ir/status`.
static DERNIERS_CHOIX: LazyLock<Mutex<HashMap<String, ChoixRi>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Mémoriser le choix et le journaliser quand il CHANGE (une ligne par
/// changement de fichier ou de cadence, pas une par piste).
pub(super) fn noter_le_choix(cle_jeu: Option<&str>, choix: &ChoixRi) {
    let change = match cle_jeu {
        Some(cle) => DERNIERS_CHOIX
            .lock()
            .map(|mut m| m.insert(cle.to_string(), choix.clone()).as_ref() != Some(choix))
            .unwrap_or(true),
        None => true,
    };
    if change {
        tracing::info!(
            jeu = cle_jeu.unwrap_or("(mémoire)"),
            fichier = choix.chemin.as_deref().unwrap_or("(mémoire)"),
            cadence_flux = choix.cadence_flux,
            cadence_fichier = choix.cadence_fichier,
            reechantillonne = choix.reechantillonne,
            raison = choix.raison,
            "convolver_ir_choisie : {}",
            choix.decrire()
        );
    }
}

/// Le dernier choix connu pour le jeu dont `chemin_principal` est le fichier
/// principal (sortie locale ou flux réseau, le dernier des deux).
pub fn dernier_choix(chemin_principal: &str) -> Option<ChoixRi> {
    DERNIERS_CHOIX
        .lock()
        .ok()
        .and_then(|m| m.get(chemin_principal).cloned())
}

/// Les chemins du jeu d'une zone et son réglage de repli, lus dans les
/// réglages : le principal (`ir_path_{zone}`) en tête, puis les fichiers par
/// cadence (`ir_jeu_{zone}`). `None` sans fichier principal.
pub struct JeuDeZone {
    pub principal: String,
    pub supplementaires: Vec<String>,
    pub reechantillonner: bool,
}

impl JeuDeZone {
    pub fn lire(settings: &crate::db::settings_repo::SettingsRepo, zone_id: i64) -> Option<Self> {
        let principal = settings
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .filter(|p| !p.is_empty())?;
        Some(Self::avec_principal(settings, zone_id, principal))
    }

    /// Même lecture, avec un principal déjà connu (la garde PURE/licence de
    /// l'orchestrateur l'a lu).
    pub fn avec_principal(
        settings: &crate::db::settings_repo::SettingsRepo,
        zone_id: i64,
        principal: String,
    ) -> Self {
        let supplementaires = Self::lire_supplementaires(settings, zone_id);
        let reechantillonner = settings
            .get(&format!("{CLE_REECHANTILLONNAGE}{zone_id}"))
            .ok()
            .flatten()
            .is_none_or(|v| v.trim() != "false" && v.trim() != "0");
        Self {
            principal,
            supplementaires,
            reechantillonner,
        }
    }

    pub fn lire_supplementaires(
        settings: &crate::db::settings_repo::SettingsRepo,
        zone_id: i64,
    ) -> Vec<String> {
        settings
            .get(&format!("{CLE_JEU}{zone_id}"))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// Tous les chemins, principal en tête.
    pub fn chemins(&self) -> Vec<String> {
        std::iter::once(self.principal.clone())
            .chain(self.supplementaires.iter().cloned())
            .collect()
    }

    pub fn charger(&self) -> Result<ConvolverConfig, String> {
        ConvolverConfig::depuis_jeu(&self.chemins(), self.reechantillonner)
    }
}

#[cfg(test)]
#[path = "par_cadence_tests.rs"]
mod tests;
