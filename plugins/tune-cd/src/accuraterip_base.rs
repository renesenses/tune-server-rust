//! La vérification AccurateRip RÉELLE (#8522) : l'identifiant AccurateRip du
//! disque, la base `accuraterip.com`, le décalage du lecteur et le verdict
//! par piste. Les CRC eux-mêmes sont ceux de `extraction::accuraterip`.
//!
//! ## L'identifiant du disque
//!
//! Algorithme de whipper (`whipper/image/table.py`, `getAccurateRipIds` et
//! `getCDDBDiscId`), le même qu'EAC et CUETools :
//!
//! * `id1` : somme des débuts (LBA, sans le pré-gap de 150) des pistes AUDIO,
//!   plus la fin du programme audio ;
//! * `id2` : somme de `max(début, 1) × numéro` des pistes audio, plus
//!   `fin × (nombre de pistes audio + 1)` ;
//! * l'identifiant CDDB (freedb) : `((Σ chiffres(secondes de chaque début,
//!   pré-gap compris) mod 255) << 24) | (durée en secondes << 8) | pistes`,
//!   sur TOUTES les pistes et la vraie fin du disque.
//!
//! L'adresse : `http://www.accuraterip.com/accuraterip/a/b/c/dBAR-NNN-id1-id2-cddb.bin`,
//! où `a`, `b`, `c` sont les trois derniers chiffres hexadécimaux de `id1`
//! (le dernier d'abord) et `NNN` le nombre de pistes audio.
//!
//! MESURÉ le 11/11/2026 : des TOC prises dans MusicBrainz donnent des
//! adresses qui répondent 200 (dont `dBAR-012-00151845-00c504b0-a70de90c`,
//! gardée en fixture), y compris un disque dont la première piste ne
//! commence pas à 0.
//!
//! ## La réponse
//!
//! Format de whipper (`whipper/common/accurip.py`) : une suite de blocs, un
//! par pressage ; chaque bloc commence par 13 octets — nombre de pistes
//! (u8), `id1`, `id2`, `cddb` (u32 petit-boutistes) — puis 9 octets par
//! piste : confiance (u8), CRC (u32), CRC de la trame 450 (u32). Un CRC v1 ou
//! v2 peut figurer dans n'importe quel bloc : on compare les deux. 404 : le
//! disque n'est pas dans la base.
//!
//! ## Usage de la base
//!
//! Un seul appel par disque : la réponse (trouvée ou absente) est gardée
//! pour la vie du greffon. User-Agent « Tune », aucun en-tête ni paramètre
//! qui dise qui écoute. Base injoignable : verdict « non vérifié », jamais
//! un échec d'extraction ; elle sera redemandée au disque suivant.
//!
//! ## Le décalage du lecteur
//!
//! Un lecteur lit les échantillons décalés d'un nombre fixe d'échantillons
//! stéréo (sa « read offset »). Le CRC se calcule sur la piste décalée
//! d'autant : `[début × 588 + décalage, fin × 588 + décalage)`, des zéros
//! hors du programme audio. Réglage `cd_decalage_lecteur` (0 par défaut) ;
//! sur un disque entier en mémoire, le décalage se DÉTECTE en comparant le
//! CRC v1 d'une piste sur toute la plage `±DECALAGE_MAX` (calcul glissant,
//! [`crc_v1_glissants`]) aux CRC de la base. Limite : seul le v1 glisse (le
//! v2 n'est pas linéaire) ; un disque que la base ne connaît qu'en v2 ne
//! permet pas la détection — le réglage manuel reste alors la voie.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::extraction::accuraterip::{CalculAccurateRip, TRAMES_SAUTEES};
use crate::lecteur::ErreurCd;
use crate::toc::{OCTETS_PAR_SECTEUR, PREGAP, SECTEURS_PAR_SECONDE, TRAMES_PAR_SECTEUR, Toc};

/// Clé de réglage du décalage du lecteur, en échantillons stéréo.
pub const CLE_DECALAGE: &str = "cd_decalage_lecteur";
/// Plage du décalage, réglé ou détecté : les 5 secteurs que l'algorithme
/// saute aux bords du disque.
pub const DECALAGE_MAX: i32 = TRAMES_SAUTEES as i32;
/// Une réponse plus grosse n'est pas une réponse de la base.
pub const TAILLE_MAX: usize = 1024 * 1024;
const TRAMES: i64 = TRAMES_PAR_SECTEUR as i64;

// ─── L'identifiant ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdentifiantAr {
    pub pistes_audio: u8,
    pub id1: u32,
    pub id2: u32,
    pub cddb: u32,
}

fn somme_des_chiffres(mut n: u32) -> u32 {
    let mut s = 0;
    while n > 0 {
        s += n % 10;
        n /= 10;
    }
    s
}

impl IdentifiantAr {
    pub fn depuis_toc(toc: &Toc) -> Self {
        let (mut id1, mut id2, mut n) = (0u32, 0u32, 0u8);
        for p in toc.pistes_audio() {
            id1 = id1.wrapping_add(p.debut);
            id2 = id2.wrapping_add(p.debut.max(1).wrapping_mul(p.numero as u32));
            n += 1;
        }
        let (_, fin_audio) = toc.fin_audio();
        id1 = id1.wrapping_add(fin_audio);
        id2 = id2.wrapping_add(fin_audio.wrapping_mul(n as u32 + 1));
        let secondes = |lba: u32| (lba + PREGAP) / SECTEURS_PAR_SECONDE as u32;
        let chiffres: u32 = toc
            .pistes
            .iter()
            .map(|p| somme_des_chiffres(secondes(p.debut)))
            .sum();
        let premiere = toc.pistes.first().map(|p| p.debut).unwrap_or(0);
        let duree = secondes(toc.fin).saturating_sub(secondes(premiere));
        let cddb =
            ((chiffres % 255) << 24) | ((duree & 0xFFFF) << 8) | (toc.pistes.len() as u32 & 0xFF);
        Self {
            pistes_audio: n,
            id1,
            id2,
            cddb,
        }
    }

    /// `dBAR-NNN-id1-id2-cddb` : le nom du fichier de la base.
    pub fn nom(&self) -> String {
        format!(
            "dBAR-{:03}-{:08x}-{:08x}-{:08x}",
            self.pistes_audio, self.id1, self.id2, self.cddb
        )
    }

    pub fn url(&self) -> String {
        let h = format!("{:08x}", self.id1);
        let c: Vec<char> = h.chars().collect();
        format!(
            "http://www.accuraterip.com/accuraterip/{}/{}/{}/{}.bin",
            c[7],
            c[6],
            c[5],
            self.nom()
        )
    }
}

// ─── La réponse ───────────────────────────────────────────────────────────

/// Une soumission pour une piste, dans un pressage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntreeAr {
    pub confiance: u8,
    pub crc: u32,
    pub crc450: u32,
}

/// Les pressages de la réponse, chacun avec ses entrées par piste. Un bloc
/// d'un autre disque est écarté ; une réponse tronquée ou vide est une
/// erreur (la base n'a pas répondu ce qu'on attendait).
pub fn analyser(octets: &[u8], id: &IdentifiantAr) -> Result<Vec<Vec<EntreeAr>>, String> {
    let u32_a = |o: &[u8], i: usize| u32::from_le_bytes([o[i], o[i + 1], o[i + 2], o[i + 3]]);
    let mut pressages = Vec::new();
    let mut i = 0usize;
    while i < octets.len() {
        if octets.len() - i < 13 {
            return Err(format!("réponse tronquée à l'octet {i}"));
        }
        let n = octets[i] as usize;
        let (id1, id2, cddb) = (
            u32_a(octets, i + 1),
            u32_a(octets, i + 5),
            u32_a(octets, i + 9),
        );
        i += 13;
        if octets.len() - i < 9 * n {
            return Err(format!("bloc tronqué à l'octet {i}"));
        }
        let entrees: Vec<EntreeAr> = (0..n)
            .map(|k| {
                let j = i + 9 * k;
                EntreeAr {
                    confiance: octets[j],
                    crc: u32_a(octets, j + 1),
                    crc450: u32_a(octets, j + 5),
                }
            })
            .collect();
        i += 9 * n;
        if (n as u8, id1, id2, cddb) == (id.pistes_audio, id.id1, id.id2, id.cddb) {
            pressages.push(entrees);
        } else {
            tracing::warn!(id = %id.nom(), "accuraterip_bloc_d_un_autre_disque_ecarte");
        }
    }
    if pressages.is_empty() {
        return Err("aucun bloc pour ce disque".into());
    }
    Ok(pressages)
}

/// Ce que la base a dit du disque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Trouvee(Vec<Vec<EntreeAr>>),
    /// 404 : personne n'a soumis ce disque.
    Absente,
    /// Hors ligne, délai dépassé, réponse illisible.
    Injoignable(String),
}

impl Base {
    pub fn etat(&self) -> &'static str {
        match self {
            Base::Trouvee(_) => "trouvee",
            Base::Absente => "absente",
            Base::Injoignable(_) => "injoignable",
        }
    }

    /// Les CRC connus de la piste d'index `index` (audio, à partir de 0).
    pub fn crcs(&self, index: usize) -> Vec<u32> {
        match self {
            Base::Trouvee(p) => p
                .iter()
                .filter_map(|b| b.get(index))
                .filter(|e| e.crc != 0)
                .map(|e| e.crc)
                .collect(),
            _ => Vec::new(),
        }
    }
}

// ─── Le verdict ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatutAr {
    /// Un CRC (v2 d'abord, puis v1) figure dans la base.
    Verifie,
    /// La base ne connaît pas ce disque (ou pas cette piste).
    Absent,
    /// La base connaît la piste, avec d'autres CRC.
    Different,
    /// La base n'a pas pu être consultée.
    NonVerifie,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerdictPiste {
    pub piste: u8,
    pub statut: StatutAr,
    /// `v2` ou `v1` : la version du CRC trouvé.
    pub version: Option<&'static str>,
    /// Vérifié : les soumissions qui ont ce CRC. Différent : toutes celles
    /// de la piste.
    pub confiance: u32,
    /// Les CRC calculés, au décalage appliqué.
    pub v1: String,
    pub v2: String,
    pub decalage: i32,
}

pub fn juger(
    base: &Base,
    index: usize,
    piste: u8,
    (v1, v2): (u32, u32),
    decalage: i32,
) -> VerdictPiste {
    let mut v = VerdictPiste {
        piste,
        statut: StatutAr::NonVerifie,
        version: None,
        confiance: 0,
        v1: format!("{v1:08x}"),
        v2: format!("{v2:08x}"),
        decalage,
    };
    match base {
        Base::Injoignable(_) => {}
        Base::Absente => v.statut = StatutAr::Absent,
        Base::Trouvee(p) => {
            let entrees: Vec<&EntreeAr> = p
                .iter()
                .filter_map(|b| b.get(index))
                .filter(|e| e.crc != 0)
                .collect();
            let confiance = |crc: u32| -> u32 {
                entrees
                    .iter()
                    .filter(|e| e.crc == crc)
                    .map(|e| e.confiance as u32)
                    .sum()
            };
            let (c2, c1) = (confiance(v2), confiance(v1));
            if c2 > 0 {
                (v.statut, v.version, v.confiance) = (StatutAr::Verifie, Some("v2"), c2);
            } else if c1 > 0 {
                (v.statut, v.version, v.confiance) = (StatutAr::Verifie, Some("v1"), c1);
            } else if entrees.is_empty() {
                v.statut = StatutAr::Absent;
            } else {
                v.statut = StatutAr::Different;
                v.confiance = entrees.iter().map(|e| e.confiance as u32).sum();
            }
        }
    }
    v
}

/// La vérification d'un disque : ce que l'écran et `/etat` montrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationAr {
    /// `dBAR-NNN-id1-id2-cddb`.
    pub identifiant: String,
    /// `trouvee`, `absente` ou `injoignable`.
    pub base: &'static str,
    pub decalage: i32,
    /// Le décalage vient de la détection, pas du réglage.
    pub decalage_detecte: bool,
    pub pistes: Vec<VerdictPiste>,
}

/// La valeur de la balise `ACCURATERIPRESULT` (et la ligne du journal),
/// en anglais comme celles d'EAC et de dBpoweramp que d'autres logiciels
/// savent lire.
pub fn texte_de_balise(v: &VerdictPiste) -> String {
    let etat = match v.statut {
        StatutAr::Verifie => format!(
            "Accurate ({}, confidence {})",
            v.version.unwrap_or("v1"),
            v.confiance
        ),
        StatutAr::Absent => "Not present in database".into(),
        StatutAr::Different => format!("Not accurate (database confidence {})", v.confiance),
        StatutAr::NonVerifie => "Not verified (database unreachable)".into(),
    };
    format!(
        "AccurateRip: {etat} [v1 {} v2 {}] offset {:+}",
        v.v1, v.v2, v.decalage
    )
}

// ─── La base ──────────────────────────────────────────────────────────────

/// D'où viennent les réponses : `accuraterip.com` en production, un témoin
/// en test. `Ok(None)` : 404.
#[async_trait]
pub trait SourceAr: Send + Sync {
    async fn telecharger(&self, url: &str) -> Result<Option<Vec<u8>>, String>;
}

pub struct AccurateRipCom;

/// Aucune base : toujours injoignable (tests, et repli).
pub struct HorsLigne;

#[async_trait]
impl SourceAr for HorsLigne {
    async fn telecharger(&self, _: &str) -> Result<Option<Vec<u8>>, String> {
        Err("hors ligne".into())
    }
}

#[async_trait]
impl SourceAr for AccurateRipCom {
    async fn telecharger(&self, url: &str) -> Result<Option<Vec<u8>>, String> {
        let r = tune_core::http::client::shared()
            .get(url)
            .header(reqwest::header::USER_AGENT, "Tune")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(format!("HTTP {}", r.status()));
        }
        if r.content_length().is_some_and(|l| l as usize > TAILLE_MAX) {
            return Err("réponse trop grosse".into());
        }
        let o = r.bytes().await.map_err(|e| e.to_string())?;
        if o.len() > TAILLE_MAX {
            return Err("réponse trop grosse".into());
        }
        Ok(Some(o.to_vec()))
    }
}

/// La base, avec sa mémoire : un seul appel par disque.
pub struct BaseAr {
    source: Arc<dyn SourceAr>,
    cache: tokio::sync::Mutex<HashMap<IdentifiantAr, Base>>,
    appels: AtomicU32,
}

impl BaseAr {
    pub fn new(source: Arc<dyn SourceAr>) -> Self {
        Self {
            source,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            appels: AtomicU32::new(0),
        }
    }

    pub fn production() -> Self {
        Self::new(Arc::new(AccurateRipCom))
    }

    /// La base par défaut : `accuraterip.com`, sauf dans les tests unitaires
    /// du greffon, qui ne sortent jamais sur le réseau (« injoignable »).
    pub fn par_defaut() -> Self {
        #[cfg(test)]
        {
            Self::new(Arc::new(HorsLigne))
        }
        #[cfg(not(test))]
        {
            Self::production()
        }
    }

    /// Appels faits à la source (témoins).
    pub fn appels(&self) -> u32 {
        self.appels.load(Ordering::SeqCst)
    }

    /// Ce que la base dit du disque. Le verrou est tenu pendant l'appel :
    /// deux demandes simultanées du même disque n'en font qu'un.
    pub async fn consulter(&self, id: &IdentifiantAr) -> Base {
        let mut cache = self.cache.lock().await;
        if let Some(b) = cache.get(id) {
            return b.clone();
        }
        self.appels.fetch_add(1, Ordering::SeqCst);
        let base = match self.source.telecharger(&id.url()).await {
            Ok(Some(o)) => match analyser(&o, id) {
                Ok(p) => Base::Trouvee(p),
                Err(e) => Base::Injoignable(e),
            },
            Ok(None) => Base::Absente,
            Err(e) => Base::Injoignable(e),
        };
        match &base {
            Base::Injoignable(e) => {
                tracing::warn!(id = %id.nom(), erreur = %e, "accuraterip_base_injoignable")
            }
            b => {
                tracing::info!(id = %id.nom(), base = b.etat(), "accuraterip_base_consultee");
                cache.insert(*id, b.clone());
            }
        }
        base
    }
}

/// Le décalage réglé (`cd_decalage_lecteur`), borné à `±DECALAGE_MAX` ;
/// 0 sans réglage.
pub fn decalage_regle(backend: Arc<dyn tune_core::db::backend::DbBackend>) -> i32 {
    tune_core::db::settings_repo::SettingsRepo::with_backend(backend)
        .get(CLE_DECALAGE)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .map(|d| d.clamp(-DECALAGE_MAX, DECALAGE_MAX))
        .unwrap_or(0)
}

// ─── Les CRC au décalage du lecteur ───────────────────────────────────────

/// Une lecture de secteurs : `(lba, nombre, sortie de nombre × 2 352 octets)`.
pub type LireSecteurs<'a> = dyn FnMut(u32, u32, &mut [u8]) -> Result<(), ErreurCd> + 'a;

/// Les `n` trames à partir de la trame ABSOLUE `depuis` (peut être
/// négative), en octets ; des zéros hors de `[debut_audio, fin_audio)`
/// (secteurs).
pub fn lire_trames(
    lire: &mut LireSecteurs<'_>,
    depuis: i64,
    n: u64,
    (debut_audio, fin_audio): (u32, u32),
) -> Result<Vec<u8>, ErreurCd> {
    let mut sortie = vec![0u8; n as usize * 4];
    if n == 0 {
        return Ok(sortie);
    }
    let s0 = depuis.div_euclid(TRAMES).max(debut_audio as i64);
    let s1 = (depuis + n as i64 + TRAMES - 1)
        .div_euclid(TRAMES)
        .min(fin_audio as i64);
    if s0 >= s1 {
        return Ok(sortie);
    }
    let mut tampon = vec![0u8; (s1 - s0) as usize * OCTETS_PAR_SECTEUR];
    lire(s0 as u32, (s1 - s0) as u32, &mut tampon)?;
    // Intersection de [depuis, depuis + n) et [s0 × 588, s1 × 588).
    let a = depuis.max(s0 * TRAMES);
    let b = (depuis + n as i64).min(s1 * TRAMES);
    if a < b {
        let src = ((a - s0 * TRAMES) * 4) as usize;
        let dst = ((a - depuis) * 4) as usize;
        let k = ((b - a) * 4) as usize;
        sortie[dst..dst + k].copy_from_slice(&tampon[src..src + k]);
    }
    Ok(sortie)
}

/// Les CRC v1 et v2 de la piste `[debut, fin)` (secteurs) lue au décalage
/// `decalage` : relit tout par `lire` (la mémoire, jamais le lecteur).
pub fn crc_decale(
    lire: &mut LireSecteurs<'_>,
    (debut, fin): (u32, u32),
    audio: (u32, u32),
    decalage: i32,
    (premiere, derniere): (bool, bool),
) -> Result<(u32, u32), ErreurCd> {
    let trames = (fin - debut) as u64 * TRAMES_PAR_SECTEUR;
    let mut calcul = CalculAccurateRip::new(trames as u32, premiere, derniere);
    let depart = debut as i64 * TRAMES + decalage as i64;
    let pas = 24 * TRAMES_PAR_SECTEUR;
    let mut fait = 0u64;
    while fait < trames {
        let k = pas.min(trames - fait);
        calcul.ajouter(&lire_trames(lire, depart + fait as i64, k, audio)?);
        fait += k;
    }
    Ok(calcul.resultat())
}

/// Le calcul AU FIL de l'extraction, au décalage du lecteur : la piste est
/// lue une fois, comme avant ; seules les trames qui débordent (avant le
/// début si le décalage est négatif, après la fin s'il est positif) sont
/// lues en plus, par [`CalculDecale::avant`] et [`CalculDecale::apres`].
pub struct CalculDecale {
    calcul: CalculAccurateRip,
    decalage: i32,
    /// Octets de la piste à ignorer encore (décalage positif).
    a_sauter: usize,
}

impl CalculDecale {
    pub fn new(trames: u32, (premiere, derniere): (bool, bool), decalage: i32) -> Self {
        Self {
            calcul: CalculAccurateRip::new(trames, premiere, derniere),
            decalage,
            a_sauter: decalage.max(0) as usize * 4,
        }
    }

    /// Avant la piste : les `|décalage|` trames qui la précèdent.
    pub fn avant(
        &mut self,
        lire: &mut LireSecteurs<'_>,
        debut: u32,
        audio: (u32, u32),
    ) -> Result<(), ErreurCd> {
        if self.decalage < 0 {
            let o = self.decalage as i64;
            let t = lire_trames(lire, debut as i64 * TRAMES + o, (-o) as u64, audio)?;
            self.calcul.ajouter(&t);
        }
        Ok(())
    }

    /// Un bloc de la piste, dans l'ordre. Au-delà de la fin décalée, le
    /// calcul ignore les trames (position hors de la piste).
    pub fn piste(&mut self, bloc: &[u8]) {
        let saut = self.a_sauter.min(bloc.len());
        self.a_sauter -= saut;
        self.calcul.ajouter(&bloc[saut..]);
    }

    /// Après la piste : les `décalage` trames qui la suivent.
    pub fn apres(
        &mut self,
        lire: &mut LireSecteurs<'_>,
        fin: u32,
        audio: (u32, u32),
    ) -> Result<(), ErreurCd> {
        if self.decalage > 0 {
            let t = lire_trames(lire, fin as i64 * TRAMES, self.decalage as u64, audio)?;
            self.calcul.ajouter(&t);
        }
        Ok(())
    }

    pub fn resultat(&self) -> (u32, u32) {
        self.calcul.resultat()
    }
}

/// Le CRC v1 de la piste pour CHAQUE décalage de `-r` à `+r`, en un passage
/// glissant. `x` : les trames (mots 32 bits) de `[début − r, fin + r)`,
/// des zéros hors du programme audio ; `trames` : la longueur de la piste.
///
/// Avec `y_o(i) = x[r + o + i − 1]` et `C(o) = Σ_{i=a..b} i·y_o(i)`,
/// `S(o) = Σ_{i=a..b} y_o(i)` : `C(o+1) = C(o) − S(o) − (a−1)·y_o(a) +
/// b·y_o(b+1)` et `S(o+1) = S(o) − y_o(a) + y_o(b+1)`, modulo 2³².
pub fn crc_v1_glissants(
    x: &[u32],
    r: usize,
    trames: usize,
    (premiere, derniere): (bool, bool),
) -> Vec<u32> {
    let a = if premiere { TRAMES_SAUTEES as usize } else { 1 };
    let b = if derniere {
        trames.saturating_sub(TRAMES_SAUTEES as usize)
    } else {
        trames
    };
    let mut sortie = Vec::with_capacity(2 * r + 1);
    if a > b || x.len() < trames + 2 * r {
        return sortie;
    }
    // o = −r : y(i) = x[i − 1].
    let (mut c, mut s) = (0u32, 0u32);
    for i in a..=b {
        let y = x[i - 1];
        c = c.wrapping_add(y.wrapping_mul(i as u32));
        s = s.wrapping_add(y);
    }
    for o in 0..=2 * r {
        sortie.push(c);
        if o == 2 * r {
            break;
        }
        // o est ici l'index (décalage + r).
        let ya = x[o + a - 1];
        let yb = x[o + b];
        c = c
            .wrapping_sub(s)
            .wrapping_sub(ya.wrapping_mul(a as u32 - 1))
            .wrapping_add(yb.wrapping_mul(b as u32));
        s = s.wrapping_sub(ya).wrapping_add(yb);
    }
    sortie
}

#[cfg(test)]
pub(crate) mod tests_communs {
    use super::IdentifiantAr;

    /// Une réponse construite à la main : un bloc par pressage.
    pub fn fixture(id: &IdentifiantAr, pressages: &[&[(u8, u32, u32)]]) -> Vec<u8> {
        let mut o = Vec::new();
        for p in pressages {
            o.push(p.len() as u8);
            o.extend(id.id1.to_le_bytes());
            o.extend(id.id2.to_le_bytes());
            o.extend(id.cddb.to_le_bytes());
            for (c, crc, c450) in p.iter() {
                o.push(*c);
                o.extend(crc.to_le_bytes());
                o.extend(c450.to_le_bytes());
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::tests_communs::fixture;
    use super::*;
    use crate::toc::PisteToc;

    fn toc(debuts: &[u32], fin: u32) -> Toc {
        Toc::nouvelle(
            debuts
                .iter()
                .enumerate()
                .map(|(i, d)| PisteToc {
                    numero: i as u8 + 1,
                    debut: *d,
                    audio: true,
                })
                .collect(),
            fin,
        )
        .unwrap()
    }

    /// RÉFÉRENCE RÉELLE : la TOC MusicBrainz du disque
    /// `2y0jFMwYQP7gb9iSUKagmoT6DIw-` (Nirvana, Nevermind, 12 pistes) et la
    /// réponse de la base, téléchargée le 11/11/2026 (16 pressages) : notre
    /// identifiant est celui de la base, la réponse se lit en entier.
    #[test]
    fn identifiant_et_reponse_d_un_vrai_disque() {
        const DEBUTS: [u32; 12] = [
            150, 22_767, 41_887, 58_317, 72_102, 91_375, 104_652, 115_380, 132_165, 143_932,
            159_870, 174_597,
        ];
        let t = toc(&DEBUTS.map(|d| d - PREGAP), 267_225 - PREGAP);
        let id = IdentifiantAr::depuis_toc(&t);
        assert_eq!(id.nom(), "dBAR-012-00151845-00c504b0-a70de90c");
        assert_eq!(
            id.url(),
            "http://www.accuraterip.com/accuraterip/5/4/8/dBAR-012-00151845-00c504b0-a70de90c.bin"
        );
        let octets = include_bytes!("../tests/fixtures/dBAR-012-00151845-00c504b0-a70de90c.bin");
        let p = analyser(octets, &id).unwrap();
        assert_eq!(p.len(), 16);
        assert!(p.iter().all(|b| b.len() == 12));
        assert_eq!(
            p[0][0],
            EntreeAr {
                confiance: 19,
                crc: 0x279c_f130,
                crc450: p[0][0].crc450
            }
        );
        assert_eq!(p[1][2].crc, 0x0950_29f8);
        // Contre-épreuve : un autre identifiant n'y trouve aucun bloc.
        let autre = IdentifiantAr {
            id1: id.id1 ^ 1,
            ..id
        };
        assert!(analyser(octets, &autre).is_err());
    }

    /// Le premier début non nul compte tel quel (`max(début, 1)`) ; une
    /// piste de DONNÉES finale n'entre ni dans id1/id2 (la fin audio la
    /// remplace) ni dans le nombre de pistes audio, mais dans le CDDB.
    #[test]
    fn identifiant_d_un_cd_enrichi() {
        let mut pistes: Vec<PisteToc> = [0u32, 10_000]
            .iter()
            .enumerate()
            .map(|(i, d)| PisteToc {
                numero: i as u8 + 1,
                debut: *d,
                audio: true,
            })
            .collect();
        pistes.push(PisteToc {
            numero: 3,
            debut: 40_000,
            audio: false,
        });
        let t = Toc::nouvelle(pistes, 50_000).unwrap();
        let id = IdentifiantAr::depuis_toc(&t);
        let fin_audio = 40_000 - 11_400;
        assert_eq!(id.pistes_audio, 2);
        assert_eq!(id.id1, 10_000 + fin_audio);
        assert_eq!(id.id2, 1 + 10_000 * 2 + fin_audio * 3);
        assert_eq!(id.cddb & 0xFF, 3, "le CDDB compte la piste de données");
        assert_eq!((id.cddb >> 8) & 0xFFFF, (50_150 / 75) - 2);
    }

    /// Une réponse tronquée n'est pas une réponse.
    #[test]
    fn une_reponse_tronquee_est_refusee() {
        let id = IdentifiantAr {
            pistes_audio: 2,
            id1: 1,
            id2: 2,
            cddb: 3,
        };
        let mut o = fixture(&id, &[&[(5, 0xAA, 0), (5, 0xBB, 0)]]);
        assert_eq!(analyser(&o, &id).unwrap().len(), 1);
        o.pop();
        assert!(analyser(&o, &id).is_err());
        assert!(analyser(&[], &id).is_err());
    }

    #[test]
    fn verdicts_v2_v1_absent_different_non_verifie() {
        let id = IdentifiantAr {
            pistes_audio: 2,
            id1: 1,
            id2: 2,
            cddb: 3,
        };
        let o = fixture(
            &id,
            &[
                &[(12, 0x1111, 0), (4, 0x2222, 0)],
                &[(3, 0x1111, 0), (0, 0, 0)],
                &[(7, 0xAAAA, 0), (2, 0x9999, 0)],
            ],
        );
        let b = Base::Trouvee(analyser(&o, &id).unwrap());
        // v2 dans deux pressages : confiance 15.
        let v = juger(&b, 0, 1, (0x5555, 0x1111), 6);
        assert_eq!(
            (v.statut, v.version, v.confiance, v.decalage),
            (StatutAr::Verifie, Some("v2"), 15, 6)
        );
        // v1 seulement.
        let v = juger(&b, 0, 1, (0xAAAA, 0x5555), 0);
        assert_eq!(
            (v.statut, v.version, v.confiance),
            (StatutAr::Verifie, Some("v1"), 7)
        );
        // Piste connue, autre CRC : différent, confiance totale.
        let v = juger(&b, 1, 2, (0x7777, 0x8888), 0);
        assert_eq!((v.statut, v.confiance), (StatutAr::Different, 6));
        assert_eq!(v.v1, "00007777");
        // Une piste au-delà de la base : absente.
        assert_eq!(juger(&b, 5, 6, (1, 2), 0).statut, StatutAr::Absent);
        assert_eq!(
            juger(&Base::Absente, 0, 1, (1, 2), 0).statut,
            StatutAr::Absent
        );
        assert_eq!(
            juger(&Base::Injoignable("x".into()), 0, 1, (0x1111, 0x1111), 0).statut,
            StatutAr::NonVerifie
        );
        assert_eq!(
            texte_de_balise(&juger(&b, 0, 1, (0x5555, 0x1111), 6)),
            "AccurateRip: Accurate (v2, confidence 15) [v1 00005555 v2 00001111] offset +6"
        );
    }

    struct SourceTemoin(Result<Option<Vec<u8>>, String>);
    #[async_trait]
    impl SourceAr for SourceTemoin {
        async fn telecharger(&self, url: &str) -> Result<Option<Vec<u8>>, String> {
            assert!(url.starts_with("http://www.accuraterip.com/accuraterip/"));
            self.0.clone()
        }
    }

    /// Un seul appel par disque, trouvé ou absent ; l'injoignable se
    /// redemande.
    #[tokio::test]
    async fn la_base_est_consultee_une_fois_par_disque() {
        let id = IdentifiantAr {
            pistes_audio: 1,
            id1: 0x123,
            id2: 2,
            cddb: 3,
        };
        let b = BaseAr::new(Arc::new(SourceTemoin(Ok(Some(fixture(
            &id,
            &[&[(1, 9, 0)]],
        ))))));
        assert!(matches!(b.consulter(&id).await, Base::Trouvee(_)));
        assert!(matches!(b.consulter(&id).await, Base::Trouvee(_)));
        assert_eq!(b.appels(), 1);
        let b = BaseAr::new(Arc::new(SourceTemoin(Ok(None))));
        assert_eq!(b.consulter(&id).await, Base::Absente);
        assert_eq!(b.consulter(&id).await, Base::Absente);
        assert_eq!(b.appels(), 1);
        let b = BaseAr::new(Arc::new(SourceTemoin(Err("hors ligne".into()))));
        assert!(matches!(b.consulter(&id).await, Base::Injoignable(_)));
        assert!(matches!(b.consulter(&id).await, Base::Injoignable(_)));
        assert_eq!(b.appels(), 2, "l'injoignable n'est pas gardé");
        // Une page HTML servie en 200 n'est pas une réponse.
        let b = BaseAr::new(Arc::new(SourceTemoin(Ok(Some(
            b"<!DOCTYPE html>".to_vec(),
        )))));
        assert!(matches!(b.consulter(&id).await, Base::Injoignable(_)));
    }

    /// Un « disque » de trames pseudo-aléatoires, lisible secteur par secteur.
    fn disque(secteurs: u32) -> Vec<u8> {
        (0..secteurs as usize * OCTETS_PAR_SECTEUR)
            .map(|i| ((i as u64).wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    fn lecteur(d: &[u8]) -> impl FnMut(u32, u32, &mut [u8]) -> Result<(), ErreurCd> + '_ {
        move |lba, n, s| {
            let a = lba as usize * OCTETS_PAR_SECTEUR;
            s.copy_from_slice(&d[a..a + n as usize * OCTETS_PAR_SECTEUR]);
            Ok(())
        }
    }

    /// Référence d'un seul tenant : les trames décalées, zéros hors disque.
    fn crc_reference(
        d: &[u8],
        debut: u32,
        fin: u32,
        decalage: i32,
        bords: (bool, bool),
    ) -> (u32, u32) {
        let trames = (fin - debut) as i64 * 588;
        let mut pcm = Vec::new();
        for k in 0..trames {
            let t = debut as i64 * 588 + decalage as i64 + k;
            let i = t * 4;
            if i < 0 || i as usize + 4 > d.len() {
                pcm.extend([0u8; 4]);
            } else {
                pcm.extend(&d[i as usize..i as usize + 4]);
            }
        }
        let mut c = CalculAccurateRip::new(trames as u32, bords.0, bords.1);
        c.ajouter(&pcm);
        c.resultat()
    }

    /// Le CRC décalé — relu en entier, ou au fil de la lecture avec les
    /// bords lus en plus — vaut la référence, pour des décalages positifs,
    /// négatifs et nuls, au milieu comme aux bords du disque.
    #[test]
    fn le_crc_au_decalage_du_lecteur() {
        let d = disque(300);
        let audio = (0, 300);
        for (debut, fin, bords) in [
            (100, 200, (false, false)),
            (0, 100, (true, false)),
            (200, 300, (false, true)),
        ] {
            for decalage in [0, 6, -6, 667, -1_164, 2_940, -2_940] {
                let attendu = crc_reference(&d, debut, fin, decalage, bords);
                let mut lire = lecteur(&d);
                assert_eq!(
                    crc_decale(&mut lire, (debut, fin), audio, decalage, bords).unwrap(),
                    attendu,
                    "relu : piste {debut}..{fin}, décalage {decalage}"
                );
                let mut c = CalculDecale::new((fin - debut) * 588, bords, decalage);
                c.avant(&mut lire, debut, audio).unwrap();
                for s in (debut..fin).step_by(24) {
                    let n = (fin - s).min(24);
                    let a = s as usize * OCTETS_PAR_SECTEUR;
                    c.piste(&d[a..a + n as usize * OCTETS_PAR_SECTEUR]);
                }
                c.apres(&mut lire, fin, audio).unwrap();
                assert_eq!(
                    c.resultat(),
                    attendu,
                    "au fil : piste {debut}..{fin}, décalage {decalage}"
                );
            }
        }
        // Contre-épreuve : le décalage change bien le CRC.
        assert_ne!(
            crc_reference(&d, 100, 200, 0, (false, false)),
            crc_reference(&d, 100, 200, 6, (false, false))
        );
    }

    /// Le calcul glissant vaut le calcul direct à chaque décalage.
    #[test]
    fn les_crc_glissants_valent_le_calcul_direct() {
        let d = disque(80);
        let r = 700usize;
        for (debut, fin, bords) in [(20u32, 60u32, (false, false)), (20, 60, (true, true))] {
            let trames = ((fin - debut) * 588) as usize;
            let x: Vec<u32> = (0..trames + 2 * r)
                .map(|k| {
                    let t = debut as i64 * 588 - r as i64 + k as i64;
                    let i = (t * 4) as usize;
                    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
                })
                .collect();
            let g = crc_v1_glissants(&x, r, trames, bords);
            assert_eq!(g.len(), 2 * r + 1);
            for o in [-700i32, -699, -1, 0, 1, 6, 333, 700] {
                assert_eq!(
                    g[(o + r as i32) as usize],
                    crc_reference(&d, debut, fin, o, bords).0,
                    "décalage {o}"
                );
            }
        }
    }
}
