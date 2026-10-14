//! #6255 — le bruit suspect vu depuis un LECTEUR RÉSEAU.
//!
//! Le serveur n'entend pas le lecteur (DLNA/UPnP, OpenHome, Chromecast,
//! BluOS… tout ce qui tire un flux HTTP d'ici). Il juge ce qu'il ENVOIE, et
//! comment le lecteur va le lire.
//!
//! Cas du 13/10 (.18, `Lavf/58.45.100`) : WAV 44,1 kHz / 24 bits converti,
//! reprise `bytes=31958520-` servie à l'octet. (31958520 − 44) % 6 = 4 : le
//! lecteur, qui décode depuis le premier octet reçu, lit chaque échantillon à
//! cheval sur deux — bruit blanc fort à 120,8 s. Le détecteur de #6130 n'a
//! rien vu : il lit le PCM sur la VRAIE grille des trames, où c'est de la
//! musique.
//!
//! Ce module regarde chaque connexion WAV qui part :
//!
//! - **`reprise_hors_trame`** : l'offset demandé (`Range: bytes=N-`) ne tombe
//!   pas sur la grille des trames — que le serveur serve cet octet tel quel
//!   ou qu'il ait réaligné la reprise ;
//! - **`decodage_decale_probable`** : sur une courte fenêtre au début de la
//!   connexion et à chaque jonction (retenue → direct), le même bloc est
//!   décodé deux fois, (a) sur la vraie grille, (b) sur le début de la
//!   connexion comme le ferait un lecteur naïf. (b) ressemble à du bruit
//!   (autocorrélation d'ordre 1 proche de 0, niveau élevé) alors que (a)
//!   ressemble à de la musique ;
//! - **`signal_bruite`** : le bloc servi ressemble à du bruit même aligné ;
//! - **`entete_au_milieu`** : un en-tête `RIFF….WAVE` ailleurs qu'à l'octet 0
//!   du flux.
//!
//! Coût : aucune copie du flux. Deux accumulateurs (somme des carrés,
//! produits aux retards 1 et 2, passages par zéro) sur au plus
//! [`TRAMES_MAX`] trames par fenêtre, et une recherche de `RIFF` octet par
//! octet. Éteint par le réglage `network_noise_watch`
//! ([`tune_core::http::bruit_reseau`]). Rien ne bloque ni ne modifie l'envoi.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;
use tune_core::http::bruit_reseau::{self as registre, EvenementBruitReseau};
use tune_core::http::streamer::StreamSession;

/// En-tête WAV canonique (celui que la voie conversion émet).
pub(crate) const EN_TETE_CANONIQUE: u64 = 44;
/// Durée visée d'une fenêtre d'analyse.
const FENETRE_MS: u64 = 300;
/// Au plus ce nombre de trames par fenêtre (borne le coût en haute fréquence).
pub(crate) const TRAMES_MAX: usize = 16_384;
/// En deçà, pas de verdict : une sonde qui lit 4 Kio ne dit rien du signal.
const TRAMES_MIN: usize = 1_024;
/// Niveau efficace minimal d'un « bruit » (−16,5 dBFS). Un échantillon lu à
/// cheval prend un octet quelconque pour poids fort : environ −4,8 dBFS.
const RMS_DU_BRUIT: f64 = 0.15;
/// Autocorrélation normalisée maximale aux retards 1 et 2 d'un bruit. Un
/// bruit blanc est à 0 ; une musique, même aiguë, a |r1| ou |r2| ≥ 0,5.
const AUTOCORRELATION_DU_BRUIT: f64 = 0.30;
/// Taux de passages par zéro minimal d'un bruit (blanc : 0,5).
const PASSAGES_DU_BRUIT: f64 = 0.30;
/// Trame la plus large lue (8 canaux × 32 bits).
const TRAME_MAXI: usize = 64;

const REPRISE_HORS_TRAME: &str = "reprise_hors_trame";
const DECODAGE_DECALE_PROBABLE: &str = "decodage_decale_probable";
const SIGNAL_BRUITE: &str = "signal_bruite";
const ENTETE_AU_MILIEU: &str = "entete_au_milieu";
const CAUSES: [&str; 4] = [
    REPRISE_HORS_TRAME,
    DECODAGE_DECALE_PROBABLE,
    SIGNAL_BRUITE,
    ENTETE_AU_MILIEU,
];

/// Signal posé par le corps quand le flux change de source en cours de
/// connexion (retenue → direct) : la fenêtre suivante se juge à nouveau.
#[derive(Clone, Default)]
pub(crate) struct Jonction(Arc<AtomicBool>);

impl Jonction {
    pub(crate) fn signaler(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn prendre(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

/// L'offset `N` d'un `Range: bytes=N-` ou `bytes=N-M` (pas les suffixes).
pub(crate) fn offset_demande(range: &str) -> Option<u64> {
    let reste = range.trim().strip_prefix("bytes=")?;
    let (debut, _) = reste.split_once('-')?;
    debut.trim().parse().ok()
}

/// Le format PCM d'une connexion.
#[derive(Debug, Clone, Copy)]
struct Format {
    frequence: u32,
    canaux: usize,
    octets_par_echantillon: usize,
    trame: usize,
    pleine_echelle: f64,
}

impl Format {
    fn nouveau(frequence: u32, profondeur: u16, canaux: u16) -> Option<Self> {
        let octets_par_echantillon = match profondeur {
            16 => 2,
            24 => 3,
            32 => 4,
            _ => return None,
        };
        let canaux = usize::from(canaux);
        let trame = canaux * octets_par_echantillon;
        if frequence == 0 || canaux == 0 || trame > TRAME_MAXI {
            return None;
        }
        Some(Self {
            frequence,
            canaux,
            octets_par_echantillon,
            trame,
            pleine_echelle: f64::from(1u32 << (profondeur - 1)),
        })
    }

    fn octets_par_seconde(&self) -> f64 {
        f64::from(self.frequence) * self.trame as f64
    }

    fn trames_par_fenetre(&self) -> usize {
        ((u64::from(self.frequence) * FENETRE_MS / 1000) as usize).clamp(TRAMES_MIN, TRAMES_MAX)
    }
}

/// Ce que mesure un décodage d'une fenêtre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Mesure {
    pub trames: usize,
    pub rms: f64,
    /// Autocorrélation normalisée aux retards 1 et 2.
    pub r1: f64,
    pub r2: f64,
    pub taux_de_passages: f64,
}

impl Mesure {
    pub(crate) fn ressemble_a_du_bruit(&self) -> bool {
        self.rms >= RMS_DU_BRUIT
            && self.r1.abs() < AUTOCORRELATION_DU_BRUIT
            && self.r2.abs() < AUTOCORRELATION_DU_BRUIT
            && self.taux_de_passages >= PASSAGES_DU_BRUIT
    }

    fn texte(&self) -> String {
        format!(
            "rms_dbfs={:.1} r1={:.3} r2={:.3} passages={:.3} trames={}",
            20.0 * self.rms.max(1e-12).log10(),
            self.r1,
            self.r2,
            self.taux_de_passages,
            self.trames
        )
    }
}

/// Un décodage en flux, sans copie : il saute `a_sauter` octets, puis lit
/// des trames entières jusqu'à `cible`.
struct Decodeur {
    format: Format,
    a_sauter: usize,
    cible: usize,
    reste: [u8; TRAME_MAXI],
    n_reste: usize,
    trames: usize,
    somme_carres: f64,
    s1: f64,
    s2: f64,
    passages: u64,
    precedents: [(f64, f64); 8],
    positifs: [bool; 8],
}

impl Decodeur {
    fn nouveau(format: Format, a_sauter: usize, cible: usize) -> Self {
        Self {
            format,
            a_sauter,
            cible,
            reste: [0; TRAME_MAXI],
            n_reste: 0,
            trames: 0,
            somme_carres: 0.0,
            s1: 0.0,
            s2: 0.0,
            passages: 0,
            precedents: [(0.0, 0.0); 8],
            positifs: [false; 8],
        }
    }

    fn fini(&self) -> bool {
        self.trames >= self.cible
    }

    fn nourrir(&mut self, mut o: &[u8]) {
        if self.fini() {
            return;
        }
        if self.a_sauter > 0 {
            let s = self.a_sauter.min(o.len());
            o = &o[s..];
            self.a_sauter -= s;
        }
        let trame = self.format.trame;
        if self.n_reste > 0 {
            let manque = (trame - self.n_reste).min(o.len());
            self.reste[self.n_reste..self.n_reste + manque].copy_from_slice(&o[..manque]);
            self.n_reste += manque;
            o = &o[manque..];
            if self.n_reste < trame {
                return;
            }
            self.n_reste = 0;
            let t = self.reste;
            self.trame_lue(&t[..trame]);
        }
        let mut entieres = o.chunks_exact(trame);
        for t in &mut entieres {
            if self.fini() {
                return;
            }
            self.trame_lue(t);
        }
        let r = entieres.remainder();
        if !self.fini() && !r.is_empty() {
            self.reste[..r.len()].copy_from_slice(r);
            self.n_reste = r.len();
        }
    }

    fn echantillon(&self, o: &[u8]) -> f64 {
        let v = match self.format.octets_par_echantillon {
            2 => i32::from(i16::from_le_bytes([o[0], o[1]])),
            3 => i32::from_le_bytes([0, o[0], o[1], o[2]]) >> 8,
            _ => i32::from_le_bytes([o[0], o[1], o[2], o[3]]),
        };
        f64::from(v) / self.format.pleine_echelle
    }

    fn trame_lue(&mut self, t: &[u8]) {
        let ope = self.format.octets_par_echantillon;
        // Au-delà de 8 canaux, on ne mesure que les 8 premiers.
        for c in 0..self.format.canaux.min(8) {
            let x = self.echantillon(&t[c * ope..(c + 1) * ope]);
            self.somme_carres += x * x;
            let (x1, x2) = self.precedents[c];
            self.s1 += x * x1;
            self.s2 += x * x2;
            self.precedents[c] = (x, x1);
            let positif = x >= 0.0;
            if self.trames > 0 && positif != self.positifs[c] {
                self.passages += 1;
            }
            self.positifs[c] = positif;
        }
        self.trames += 1;
    }

    fn mesure(&self) -> Mesure {
        let canaux = self.format.canaux.min(8);
        let n = (self.trames * canaux).max(1) as f64;
        let energie = self.somme_carres.max(f64::MIN_POSITIVE);
        Mesure {
            trames: self.trames,
            rms: (self.somme_carres / n).sqrt(),
            r1: self.s1 / energie,
            r2: self.s2 / energie,
            taux_de_passages: self.passages as f64
                / (self.trames.saturating_sub(1) * canaux).max(1) as f64,
        }
    }
}

/// Une fenêtre d'analyse : les deux décodages du même bloc.
struct Fenetre {
    motif: &'static str,
    debut: u64,
    aligne: Decodeur,
    naif: Decodeur,
}

/// Octets à sauter, depuis la position `p`, pour tomber sur une grille dont
/// une trame commence à `origine`.
fn saut(p: u64, origine: u64, trame: u64) -> usize {
    if p < origine {
        (origine - p) as usize
    } else {
        ((trame - (p - origine) % trame) % trame) as usize
    }
}

/// L'observateur d'UNE connexion WAV vers un lecteur réseau.
pub(crate) struct ObservateurReseau {
    session: Arc<StreamSession>,
    voie: &'static str,
    agent: String,
    range: String,
    offset_demande: Option<u64>,
    debut_du_corps: u64,
    /// Position, dans le flux, du premier octet de PCM (fin de l'en-tête).
    en_tete: u64,
    format: Format,
    /// Position, dans le flux, du prochain octet observé.
    position: u64,
    fenetre: Option<Fenetre>,
    jonction: Option<Jonction>,
    /// Causes déjà signalées sur cette connexion (un WARN par cause).
    signalees: [bool; 4],
    /// Les derniers octets du morceau précédent, pour un `RIFF` à cheval.
    queue: [u8; 11],
    n_queue: usize,
}

impl ObservateurReseau {
    /// `None` si le détecteur est éteint ou si le flux n'est pas un PCM WAV
    /// lisible. `debut_du_corps` : la position, dans le flux, du premier
    /// octet que cette connexion envoie. `en_tete` : celle du premier octet
    /// de PCM.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn nouveau(
        session: &Arc<StreamSession>,
        voie: &'static str,
        agent: &str,
        range: &str,
        debut_du_corps: u64,
        en_tete: u64,
        jonction: Option<Jonction>,
    ) -> Option<Self> {
        Self::si_actif(
            registre::actif(),
            session,
            voie,
            agent,
            range,
            debut_du_corps,
            en_tete,
            jonction,
        )
    }

    /// [`Self::nouveau`], l'interrupteur passé en argument (les tests ne
    /// touchent pas l'interrupteur global, partagé par les tests voisins).
    #[allow(clippy::too_many_arguments)]
    fn si_actif(
        actif: bool,
        session: &Arc<StreamSession>,
        voie: &'static str,
        agent: &str,
        range: &str,
        debut_du_corps: u64,
        en_tete: u64,
        jonction: Option<Jonction>,
    ) -> Option<Self> {
        if !actif || session.info.format != "wav" {
            return None;
        }
        let format = Format::nouveau(
            session.info.sample_rate,
            session.info.bit_depth,
            session.info.channels,
        )?;
        let mut obs = Self {
            session: session.clone(),
            voie,
            agent: agent.to_string(),
            range: range.to_string(),
            offset_demande: offset_demande(range),
            debut_du_corps,
            en_tete,
            format,
            position: debut_du_corps,
            fenetre: None,
            jonction,
            signalees: [false; 4],
            queue: [0; 11],
            n_queue: 0,
        };
        obs.juger_la_reprise();
        obs.ouvrir_une_fenetre("debut_de_connexion");
        Some(obs)
    }

    fn trame(&self) -> u64 {
        self.format.trame as u64
    }

    /// Position en secondes de musique d'un octet du flux.
    fn secondes(&self, p: u64) -> f64 {
        p.saturating_sub(self.en_tete) as f64 / self.format.octets_par_seconde()
    }

    /// Où le lecteur naïf pose sa grille : au premier octet reçu, ou à la fin
    /// de l'en-tête s'il l'a reçu.
    fn origine_naive(&self) -> u64 {
        self.debut_du_corps.max(self.en_tete)
    }

    fn juger_la_reprise(&mut self) {
        let Some(n) = self.offset_demande else {
            return;
        };
        if n <= self.en_tete {
            return;
        }
        let phase = (n - self.en_tete) % self.trame();
        if phase == 0 {
            return;
        }
        let realignee = self.debut_du_corps != n;
        let detail = format!(
            "{} ; le lecteur qui decode depuis l'octet {n} lit chaque echantillon a cheval",
            if realignee {
                format!(
                    "reprise realignee par le serveur, servie depuis {}",
                    self.debut_du_corps
                )
            } else {
                "servie a l'octet demande".to_string()
            }
        );
        self.signaler(REPRISE_HORS_TRAME, phase, self.secondes(n), detail);
    }

    fn ouvrir_une_fenetre(&mut self, motif: &'static str) {
        let p = self.position;
        let t = self.trame();
        let cible = self.format.trames_par_fenetre();
        self.fenetre = Some(Fenetre {
            motif,
            debut: p.max(self.en_tete),
            aligne: Decodeur::nouveau(self.format, saut(p, self.en_tete, t), cible),
            naif: Decodeur::nouveau(self.format, saut(p, self.origine_naive(), t), cible),
        });
    }

    /// Un morceau du corps vient de partir vers le lecteur.
    pub(crate) fn observer(&mut self, octets: &[u8]) {
        if octets.is_empty() {
            return;
        }
        if self.jonction.as_ref().is_some_and(Jonction::prendre) {
            self.ouvrir_une_fenetre("jonction");
        }
        self.chercher_un_entete(octets);
        if let Some(f) = self.fenetre.as_mut() {
            f.aligne.nourrir(octets);
            f.naif.nourrir(octets);
            if f.aligne.fini() && f.naif.fini() {
                self.juger_la_fenetre();
            }
        }
        self.position += octets.len() as u64;
    }

    /// `RIFF` + 4 octets + `WAVE`, ailleurs qu'à l'octet 0 du flux.
    fn chercher_un_entete(&mut self, octets: &[u8]) {
        if self.signalees[3] {
            return;
        }
        const MOTIF: usize = 12;
        let est_entete = |w: &[u8]| &w[..4] == b"RIFF" && &w[8..12] == b"WAVE";
        // À cheval sur le morceau précédent : la queue + le début de celui-ci.
        let mut trouve: Option<u64> = None;
        if self.n_queue > 0 {
            let mut pont = [0u8; 22];
            let n_tete = (MOTIF - 1).min(octets.len());
            pont[..self.n_queue].copy_from_slice(&self.queue[..self.n_queue]);
            pont[self.n_queue..self.n_queue + n_tete].copy_from_slice(&octets[..n_tete]);
            let pont = &pont[..self.n_queue + n_tete];
            if pont.len() >= MOTIF
                && let Some(i) = (0..self.n_queue)
                    .find(|&i| i + MOTIF <= pont.len() && est_entete(&pont[i..i + MOTIF]))
            {
                trouve = Some(self.position - self.n_queue as u64 + i as u64);
            }
        }
        if trouve.is_none() && octets.len() >= MOTIF {
            let mut i = 0;
            while i + MOTIF <= octets.len() {
                if octets[i] == b'R' && est_entete(&octets[i..i + MOTIF]) {
                    let p = self.position + i as u64;
                    if p != 0 {
                        trouve = Some(p);
                        break;
                    }
                }
                i += 1;
            }
        }
        let garde = octets.len().min(MOTIF - 1);
        if garde == MOTIF - 1 {
            self.queue.copy_from_slice(&octets[octets.len() - garde..]);
            self.n_queue = garde;
        } else {
            // Morceau minuscule : la queue glisse.
            let mut tout = [0u8; 22];
            tout[..self.n_queue].copy_from_slice(&self.queue[..self.n_queue]);
            tout[self.n_queue..self.n_queue + garde].copy_from_slice(octets);
            let n = self.n_queue + garde;
            let k = n.min(MOTIF - 1);
            self.queue[..k].copy_from_slice(&tout[n - k..n]);
            self.n_queue = k;
        }
        if let Some(p) = trouve.filter(|p| *p != 0) {
            let phase = (p.saturating_sub(self.en_tete)) % self.trame();
            self.signaler(
                ENTETE_AU_MILIEU,
                phase,
                self.secondes(p),
                format!(
                    "en-tete RIFF/WAVE a l'octet {p} du flux ; le lecteur le joue comme 44 octets de PCM et perd la grille"
                ),
            );
        }
    }

    fn juger_la_fenetre(&mut self) {
        let Some(f) = self.fenetre.take() else {
            return;
        };
        if f.aligne.trames < TRAMES_MIN || f.naif.trames < TRAMES_MIN {
            return;
        }
        let a = f.aligne.mesure();
        let b = f.naif.mesure();
        let phase_naive = (self.origine_naive() - self.en_tete) % self.trame();
        let position = self.secondes(f.debut);
        if a.ressemble_a_du_bruit() {
            self.signaler(
                SIGNAL_BRUITE,
                phase_naive,
                position,
                format!("fenetre={} aligne[{}]", f.motif, a.texte()),
            );
        } else if phase_naive != 0 && b.ressemble_a_du_bruit() {
            self.signaler(
                DECODAGE_DECALE_PROBABLE,
                phase_naive,
                position,
                format!(
                    "fenetre={} aligne[{}] naif[{}]",
                    f.motif,
                    a.texte(),
                    b.texte()
                ),
            );
        }
    }

    fn signaler(&mut self, cause: &'static str, phase: u64, position_s: f64, detail: String) {
        let i = CAUSES.iter().position(|c| *c == cause).unwrap_or(0);
        if std::mem::replace(&mut self.signalees[i], true) {
            return;
        }
        let zone_id = self
            .session
            .contexte_de_conversion
            .get()
            .map_or(-1, |c| c.zone_id);
        let info = &self.session.info;
        let format = format!("{}/{}/{}", info.sample_rate, info.bit_depth, info.channels);
        warn!(
            cause = ?cause,
            stream_id = %self.session.id,
            voie = self.voie,
            zone_id,
            agent = %self.agent,
            range = %self.range,
            offset_demande = ?self.offset_demande,
            offset_servi = self.debut_du_corps,
            phase,
            trame = self.trame(),
            position_s = %format!("{position_s:.1}"),
            format = %format,
            detail = %detail,
            "bruit_suspect_reseau"
        );
        registre::noter(EvenementBruitReseau {
            horodatage: registre::maintenant(),
            cause,
            stream_id: self.session.id.clone(),
            voie: self.voie,
            zone_id,
            agent: self.agent.clone(),
            range: self.range.clone(),
            offset_demande: self.offset_demande,
            offset_servi: self.debut_du_corps,
            phase,
            trame: self.trame(),
            position_s: (position_s * 10.0).round() / 10.0,
            format,
            detail,
        });
    }
}

impl Drop for ObservateurReseau {
    /// Une connexion fermée en cours de fenêtre : la juger sur ce qu'elle a
    /// envoyé, si c'est assez long.
    fn drop(&mut self) {
        self.juger_la_fenetre();
    }
}

/// Le flux du corps, observé au passage. Sans observateur, rien ne change.
pub(crate) fn sur_le_flux<S, B, E>(
    flux: S,
    mut observateur: Option<ObservateurReseau>,
) -> impl futures_util::Stream<Item = Result<B, E>>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    futures_util::StreamExt::map(flux, move |morceau| {
        if let (Ok(o), Some(obs)) = (&morceau, observateur.as_mut()) {
            obs.observer(o.as_ref());
        }
        morceau
    })
}

/// Où commence le PCM d'un fichier WAV : la fin de l'en-tête du bloc `data`.
/// `None` si le début du fichier n'est pas un WAV lisible.
pub(crate) fn debut_des_donnees(tete: &[u8]) -> Option<u64> {
    if tete.len() < 12 || &tete[..4] != b"RIFF" || &tete[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12usize;
    while i + 8 <= tete.len() {
        let taille =
            u32::from_le_bytes([tete[i + 4], tete[i + 5], tete[i + 6], tete[i + 7]]) as usize;
        if &tete[i..i + 4] == b"data" {
            return Some((i + 8) as u64);
        }
        i = i.checked_add(8 + taille + (taille & 1))?;
    }
    None
}

/// Le corps d'un FICHIER WAV servi (`serve_file`), observé au passage.
/// Lit les 4 premiers Kio du fichier pour trouver le début du PCM ; tout
/// échec rend le corps tel quel.
pub(crate) async fn sur_le_fichier(
    corps: axum::body::Body,
    chemin: &str,
    session: &Arc<StreamSession>,
    req_headers: &axum::http::HeaderMap,
    debut_du_corps: u64,
) -> axum::body::Body {
    if !registre::actif() || session.info.format != "wav" {
        return corps;
    }
    let tete = {
        use tokio::io::AsyncReadExt as _;
        let mut tete = vec![0u8; 4096];
        let Ok(mut f) = tokio::fs::File::open(chemin).await else {
            return corps;
        };
        let mut n = 0;
        while n < tete.len() {
            match f.read(&mut tete[n..]).await {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
        tete.truncate(n);
        tete
    };
    let Some(en_tete) = debut_des_donnees(&tete) else {
        return corps;
    };
    let entete = |nom: &str| {
        req_headers
            .get(nom)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    let Some(obs) = ObservateurReseau::nouveau(
        session,
        "fichier",
        &entete("User-Agent"),
        &entete("Range"),
        debut_du_corps,
        en_tete,
        None,
    ) else {
        return corps;
    };
    axum::body::Body::from_stream(sur_le_flux(corps.into_data_stream(), Some(obs)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::http::streamer::StreamInfo;

    const SR: u32 = 44_100;

    /// Générateur pseudo-aléatoire déterministe (xorshift).
    pub(crate) struct Alea(pub u64);
    impl Alea {
        pub(crate) fn suivant(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        }
    }

    /// WAV 24 bits stéréo : en-tête canonique + `f(t)` sur `secondes`.
    pub(crate) fn wav24(secondes: f64, mut f: impl FnMut(f64) -> f64) -> Vec<u8> {
        let mut v = tune_core::audio::wav::build_wav_header(2, SR, 24).to_vec();
        let n = (secondes * f64::from(SR)) as usize;
        let pe = f64::from((1u32 << 23) - 1);
        for i in 0..n {
            let x = (f(i as f64 / f64::from(SR)).clamp(-1.0, 1.0) * pe) as i32;
            v.extend_from_slice(&x.to_le_bytes()[..3]);
            v.extend_from_slice(&x.to_le_bytes()[..3]);
        }
        v
    }

    /// Sinus à 440 Hz, −6 dBFS, et un bruit faible à −60 dBFS.
    pub(crate) fn musique24(secondes: f64) -> Vec<u8> {
        let mut a = Alea(0x6255);
        wav24(secondes, |t| {
            0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.001 * a.suivant()
        })
    }

    fn session24(id: &str) -> Arc<StreamSession> {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: SR,
            channels: 2,
            bit_depth: 24,
            ..StreamInfo::default()
        };
        Arc::new(StreamSession::new(id.into(), info, false, 4))
    }

    /// Les causes notées au registre pour ce flux.
    fn causes(stream_id: &str) -> Vec<&'static str> {
        registre::derniers()
            .into_iter()
            .filter(|e| e.stream_id == stream_id)
            .map(|e| e.cause)
            .collect()
    }

    /// Sert `flux[debut..]` en morceaux de `bloc` à un observateur neuf.
    fn servir(id: &str, flux: &[u8], debut: u64, range: &str, bloc: usize) -> Vec<&'static str> {
        let session = session24(id);
        let mut obs = ObservateurReseau::nouveau(
            &session,
            "conversion",
            "Lavf/58.45.100",
            range,
            debut,
            EN_TETE_CANONIQUE,
            None,
        )
        .expect("WAV 24 bits lisible");
        for b in flux[debut as usize..].chunks(bloc) {
            obs.observer(b);
        }
        drop(obs);
        causes(id)
    }

    #[test]
    fn l_offset_demande_se_lit_dans_le_range() {
        assert_eq!(offset_demande("bytes=31958520-"), Some(31_958_520));
        assert_eq!(offset_demande("bytes=0-999"), Some(0));
        assert_eq!(offset_demande("bytes=-500"), None);
        assert_eq!(offset_demande("-"), None);
    }

    #[test]
    fn la_phase_du_13_10_vaut_4() {
        assert_eq!((31_958_520u64 - 44) % 6, 4);
    }

    #[test]
    fn une_musique_servie_depuis_44_ne_leve_rien() {
        let flux = musique24(2.0);
        assert!(servir("u-44", &flux, 44, "bytes=44-", 32_768).is_empty());
        assert!(servir("u-0", &flux, 0, "bytes=0-", 1_000).is_empty());
        // Reprise ALIGNÉE au milieu : rien non plus.
        let n = 44 + 6 * 20_000;
        assert!(servir("u-aligne", &flux, n, &format!("bytes={n}-"), 32_768).is_empty());
    }

    #[test]
    fn une_reprise_de_phase_4_leve_reprise_et_decodage_decale() {
        let flux = musique24(2.0);
        let n = 44 + 6 * 20_000 + 4;
        let c = servir("u-phase4", &flux, n, &format!("bytes={n}-"), 32_768);
        assert_eq!(
            c,
            vec![REPRISE_HORS_TRAME, DECODAGE_DECALE_PROBABLE],
            "{c:?}"
        );
        let e = registre::derniers()
            .into_iter()
            .find(|e| e.stream_id == "u-phase4")
            .unwrap();
        assert_eq!((e.phase, e.trame, e.offset_servi), (4, 6, n));
        assert!((e.position_s - 0.5).abs() < 0.1, "{e:?}");
    }

    #[test]
    fn une_reprise_realignee_par_le_serveur_se_signale_encore_sans_decodage_decale() {
        // Après le correctif de la reprise : `bytes=N-` demandé hors trame,
        // servi depuis la trame juste avant.
        let flux = musique24(2.0);
        let n = 44 + 6 * 20_000 + 4;
        let c = servir("u-realigne", &flux, n - 4, &format!("bytes={n}-"), 32_768);
        assert_eq!(c, vec![REPRISE_HORS_TRAME], "{c:?}");
        let e = registre::derniers()
            .into_iter()
            .find(|e| e.stream_id == "u-realigne")
            .unwrap();
        assert!(e.detail.contains("realignee"), "{e:?}");
    }

    #[test]
    fn un_entete_au_milieu_se_signale() {
        let musique = musique24(1.0);
        let mut flux = musique.clone();
        // Un second WAV collé au milieu, à une position quelconque.
        flux.extend_from_slice(&musique);
        assert_eq!(
            servir("u-entete", &flux, 0, "bytes=0-", 32_768),
            vec![ENTETE_AU_MILIEU]
        );
        // À cheval sur deux morceaux : même verdict.
        let coupe = musique.len() + 5;
        let session = session24("u-entete-cheval");
        let mut obs = ObservateurReseau::nouveau(
            &session,
            "conversion",
            "-",
            "-",
            0,
            EN_TETE_CANONIQUE,
            None,
        )
        .unwrap();
        obs.observer(&flux[..coupe]);
        obs.observer(&flux[coupe..]);
        drop(obs);
        assert_eq!(causes("u-entete-cheval"), vec![ENTETE_AU_MILIEU]);
    }

    #[test]
    fn un_bruit_servi_aligne_leve_signal_bruite() {
        let mut a = Alea(7);
        let flux = wav24(1.0, |_| 0.8 * a.suivant());
        assert_eq!(
            servir("u-bruit", &flux, 0, "bytes=0-", 32_768),
            vec![SIGNAL_BRUITE]
        );
    }

    #[test]
    fn une_jonction_ouvre_une_nouvelle_fenetre() {
        // Musique, puis à la jonction un PCM qui part hors de sa grille (le
        // direct reprend un octet trop loin) : le bloc aligné y est du bruit.
        let musique = musique24(1.0);
        let session = session24("u-jonction");
        let jonction = Jonction::default();
        let mut obs = ObservateurReseau::nouveau(
            &session,
            "conversion",
            "-",
            "bytes=0-",
            0,
            EN_TETE_CANONIQUE,
            Some(jonction.clone()),
        )
        .unwrap();
        obs.observer(&musique);
        assert!(causes("u-jonction").is_empty());
        jonction.signaler();
        obs.observer(&musique[45..]);
        drop(obs);
        assert_eq!(causes("u-jonction"), vec![SIGNAL_BRUITE]);
    }

    #[test]
    fn une_sonde_trop_courte_ne_juge_rien() {
        let flux = musique24(1.0);
        let n = 44 + 4;
        // 4 Kio seulement : la reprise hors trame se dit, le signal non.
        let session = session24("u-sonde");
        let mut obs = ObservateurReseau::nouveau(
            &session,
            "conversion",
            "-",
            &format!("bytes={n}-"),
            n,
            EN_TETE_CANONIQUE,
            None,
        )
        .unwrap();
        obs.observer(&flux[n as usize..n as usize + 4096]);
        drop(obs);
        assert_eq!(causes("u-sonde"), vec![REPRISE_HORS_TRAME]);
    }

    #[test]
    fn le_debut_des_donnees_se_lit_apres_un_bloc_list() {
        let h = tune_core::audio::wav::build_wav_header(2, SR, 24);
        assert_eq!(debut_des_donnees(&h), Some(44));
        // fmt (24) puis LIST de 10 octets (pair), puis data.
        let mut v = h[..36].to_vec();
        v.extend_from_slice(b"LIST");
        v.extend_from_slice(&10u32.to_le_bytes());
        v.extend_from_slice(&[0; 10]);
        v.extend_from_slice(&h[36..44]);
        assert_eq!(debut_des_donnees(&v), Some(62));
        assert_eq!(debut_des_donnees(b"ID3\x04 pas un wav"), None);
    }

    #[test]
    fn le_detecteur_eteint_ne_cree_aucun_observateur() {
        let eteint = ObservateurReseau::si_actif(
            false,
            &session24("u-eteint"),
            "conversion",
            "-",
            "bytes=50-",
            50,
            EN_TETE_CANONIQUE,
            None,
        )
        .is_none();
        assert!(eteint);
        assert!(causes("u-eteint").is_empty());
    }
}
