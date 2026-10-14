//! #6130 — instrumentation de la voie « conversion » : de quoi CAPTURER la
//! prochaine occurrence d'un bruit blanc qu'on ne sait pas reproduire.
//!
//! Zone 10 du .18 (Eversolo DMP-A8, `Lavf/58.45.100`, 11/10 vers 07:06 UTC) :
//! un Qobuz FLAC 44,1/16 converti en WAV chunké, étages DSP et greffon natif
//! tiers actifs, joué comme un FORT bruit blanc. Environ une fois tous les
//! deux jours ; les octets servis étaient justes en longueur, et le rejeu de
//! trois `bytes=0-` façon Lavf est vert (`bruit_trois_connexions_b209bruit`).
//!
//! Ce module n'agit pas sur le flux. Il REGARDE les octets qui partent :
//!
//! - une ligne INFO par connexion, `conversion_connexion_contexte` : les 44
//!   premiers octets réellement envoyés en hexadécimal, l'en-tête WAV qu'ils
//!   portent décodé, le format de la session, et le contexte posé par
//!   l'orchestrateur (source, chaîne d'étages, piste précédente) ;
//! - un WARN au plus par FLUX, `bruit_suspect`, quand les blocs PCM qui
//!   sortent ressemblent à du bruit plein échelle soutenu : niveau élevé et
//!   spectre plat, approché par le taux de passages par zéro et confirmé par
//!   l'autocorrélation aux retards 1 et 2 (nulle pour un bruit blanc, jamais
//!   pour un sinus, si aigu soit-il).
//!
//! Il ne coupe et ne modifie RIEN. Une coupure de protection serait une
//! fonction à part.
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::{info, warn};
use tune_core::http::streamer::StreamSession;

/// Octets du bloc incriminé recopiés dans le WARN.
pub(crate) const OCTETS_DU_BLOC_JOURNALISES: usize = 1024;
/// Longueur d'un en-tête WAV canonique.
const ENTETE: usize = 44;

/// Fenêtre d'analyse : 100 ms de trames.
const FENETRES_PAR_SECONDE: u32 = 10;
/// Fenêtres bruyantes CONSÉCUTIVES avant de signaler : 2 s. Une caisse
/// claire, un souffle entre deux plages ne tiennent pas deux secondes à ce
/// niveau-là.
const FENETRES_SOUTENUES: u32 = 20;
/// Niveau efficace minimal d'une fenêtre (−20 dBFS).
const RMS_PLANCHER: f64 = 0.10;
/// Niveau efficace « élevé » (−14 dBFS). Un bruit blanc plein échelle est à
/// −4,8 dBFS (uniforme) ; une musique très compressée reste vers −8 dBFS,
/// mais avec un taux de passages par zéro bien plus bas.
const RMS_ELEVE: f64 = 0.20;
/// Crête « plein échelle ».
const CRETE_ELEVEE: f64 = 0.95;
/// Taux de passages par zéro minimal (par échantillon). Bruit blanc : 0,5.
/// Un sinus à 1 kHz sous 44,1 kHz : 0,045 ; la musique : sous 0,15 hors
/// transitoires. Prudent : 0,30.
const TAUX_DE_PASSAGES_MINIMAL: f64 = 0.30;
/// Autocorrélation normalisée maximale aux retards 1 et 2. Le taux de
/// passages par zéro seul confond un bruit blanc et un sinus aigu (15 kHz
/// sous 44,1 kHz : 0,68). L'autocorrélation les sépare : nulle pour un bruit
/// blanc, alors qu'un sinus a toujours |r1| ou |r2| ≥ 0,5 (si
/// |cos θ| < 0,5, alors cos 2θ < −0,5).
const AUTOCORRELATION_MAXIMALE: f64 = 0.30;

/// Hexadécimal compact, deux chiffres par octet, sans séparateur.
pub(crate) fn hex(octets: &[u8]) -> String {
    let mut s = String::with_capacity(octets.len() * 2);
    for o in octets {
        let _ = write!(s, "{o:02x}");
    }
    s
}

/// Les paramètres d'un en-tête WAV canonique, tels que le renderer les lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnteteWav {
    pub format: u16,
    pub canaux: u16,
    pub frequence: u32,
    pub octets_par_seconde: u32,
    pub alignement: u16,
    pub profondeur: u16,
    pub taille_riff: u32,
    /// `None` si le bloc `data` ne suit pas directement `fmt ` (octet 36).
    pub taille_data: Option<u32>,
}

impl EnteteWav {
    /// `None` si les octets ne commencent pas par `RIFF….WAVEfmt `.
    pub(crate) fn decoder(o: &[u8]) -> Option<Self> {
        if o.len() < ENTETE || &o[..4] != b"RIFF" || &o[8..12] != b"WAVE" || &o[12..16] != b"fmt " {
            return None;
        }
        let u16_ = |i: usize| u16::from_le_bytes([o[i], o[i + 1]]);
        let u32_ = |i: usize| u32::from_le_bytes([o[i], o[i + 1], o[i + 2], o[i + 3]]);
        Some(Self {
            format: u16_(20),
            canaux: u16_(22),
            frequence: u32_(24),
            octets_par_seconde: u32_(28),
            alignement: u16_(32),
            profondeur: u16_(34),
            taille_riff: u32_(4),
            taille_data: (&o[36..40] == b"data").then(|| u32_(40)),
        })
    }

    /// L'en-tête est-il cohérent avec lui-même (débit et alignement déduits
    /// de la fréquence, de la profondeur et des canaux) ?
    fn coherent(&self) -> bool {
        let trame = u32::from(self.canaux) * u32::from(self.profondeur / 8);
        u32::from(self.alignement) == trame && self.octets_par_seconde == self.frequence * trame
    }
}

impl std::fmt::Display for EnteteWav {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frequence={} profondeur={} canaux={} format={} octets_par_s={} alignement={} \
             taille_riff={} taille_data={} coherent={}",
            self.frequence,
            self.profondeur,
            self.canaux,
            self.format,
            self.octets_par_seconde,
            self.alignement,
            self.taille_riff,
            self.taille_data
                .map_or_else(|| "absente".to_string(), |t| t.to_string()),
            self.coherent(),
        )
    }
}

/// Ce que mesurait la fenêtre qui a fait déclencher le détecteur.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Mesure {
    pub rms: f64,
    pub crete: f64,
    pub taux_de_passages: f64,
    /// Autocorrélation normalisée aux retards 1 et 2.
    pub autocorrelation: (f64, f64),
    /// Durée bruyante soutenue au déclenchement, en millisecondes.
    pub soutenu_ms: u64,
    /// Position, DANS LE FLUX, du premier octet de la fenêtre déclenchante.
    pub position: u64,
}

impl Mesure {
    pub(crate) fn rms_dbfs(&self) -> f64 {
        20.0 * self.rms.max(1e-12).log10()
    }
}

/// Le détecteur de « bruit plein échelle » sur le PCM entier signé
/// petit-boutiste (16, 24 ou 32 bits) d'un WAV.
///
/// Il lit les octets À LEUR POSITION DANS LE FLUX — celle que le renderer
/// leur prête : l'en-tête (44 octets) est sauté, et la grille de trames part
/// de l'octet 44. Un PCM servi décalé d'un octet s'y lit donc comme le
/// renderer l'entend : en bruit.
pub(crate) struct DetecteurDeBruit {
    octets_par_echantillon: usize,
    canaux: usize,
    trame: usize,
    pleine_echelle: f64,
    trames_par_fenetre: usize,
    /// Position dans le flux du prochain octet reçu.
    position: u64,
    /// Fin de trame entamée par le bloc précédent.
    reste: Vec<u8>,
    // ── La fenêtre courante ──
    debut_fenetre: u64,
    trames: usize,
    somme_carres: f64,
    crete: f64,
    passages: u64,
    signe_precedent: Vec<bool>,
    /// Les deux échantillons précédents de chaque canal.
    precedents: Vec<(f64, f64)>,
    somme_retard_1: f64,
    somme_retard_2: f64,
    // ── La durée soutenue ──
    fenetres_bruyantes: u32,
    debut_du_bruit: u64,
}

impl DetecteurDeBruit {
    /// `None` si le format n'est pas un PCM entier que le détecteur sait lire.
    pub(crate) fn nouveau(
        frequence: u32,
        profondeur: u16,
        canaux: u16,
        debut_du_corps: u64,
    ) -> Option<Self> {
        let octets_par_echantillon = match profondeur {
            16 => 2,
            24 => 3,
            32 => 4,
            _ => return None,
        };
        if frequence == 0 || canaux == 0 {
            return None;
        }
        let canaux = usize::from(canaux);
        Some(Self {
            octets_par_echantillon,
            canaux,
            trame: canaux * octets_par_echantillon,
            pleine_echelle: f64::from(1u32 << (profondeur - 1)),
            trames_par_fenetre: (frequence / FENETRES_PAR_SECONDE).max(1) as usize,
            position: debut_du_corps,
            reste: Vec::new(),
            debut_fenetre: 0,
            trames: 0,
            somme_carres: 0.0,
            crete: 0.0,
            passages: 0,
            signe_precedent: vec![false; canaux],
            precedents: vec![(0.0, 0.0); canaux],
            somme_retard_1: 0.0,
            somme_retard_2: 0.0,
            fenetres_bruyantes: 0,
            debut_du_bruit: 0,
        })
    }

    /// Observe les octets suivants du flux. Rend la mesure de la fenêtre qui
    /// fait franchir le seuil soutenu, une fois ; ensuite le détecteur
    /// continue de compter mais ne rend plus rien tant que le bruit dure.
    pub(crate) fn observer(&mut self, mut octets: &[u8]) -> Option<Mesure> {
        // L'en-tête n'est pas du PCM.
        if self.position < ENTETE as u64 {
            let saut = ((ENTETE as u64 - self.position) as usize).min(octets.len());
            octets = &octets[saut..];
            self.position += saut as u64;
            if self.position < ENTETE as u64 {
                return None;
            }
        }
        // Une connexion qui part au milieu d'une trame : se caler sur la
        // grille du renderer avant de lire un échantillon.
        if self.reste.is_empty() {
            let phase = ((self.position - ENTETE as u64) % self.trame as u64) as usize;
            if phase != 0 {
                let saut = (self.trame - phase).min(octets.len());
                octets = &octets[saut..];
                self.position += saut as u64;
            }
        }
        let mut declenche = None;
        if !self.reste.is_empty() {
            let manque = (self.trame - self.reste.len()).min(octets.len());
            self.reste.extend_from_slice(&octets[..manque]);
            octets = &octets[manque..];
            self.position += manque as u64;
            if self.reste.len() == self.trame {
                let trame = std::mem::take(&mut self.reste);
                declenche = declenche.or(self.trame_lue(&trame));
            }
        }
        let entieres = octets.len() / self.trame * self.trame;
        for trame in octets[..entieres].chunks_exact(self.trame) {
            self.position += self.trame as u64;
            declenche = declenche.or(self.trame_lue(trame));
        }
        let reste = &octets[entieres..];
        self.reste.extend_from_slice(reste);
        self.position += reste.len() as u64;
        declenche
    }

    fn echantillon(&self, o: &[u8]) -> f64 {
        let v = match self.octets_par_echantillon {
            2 => i32::from(i16::from_le_bytes([o[0], o[1]])),
            3 => i32::from_le_bytes([0, o[0], o[1], o[2]]) >> 8,
            _ => i32::from_le_bytes([o[0], o[1], o[2], o[3]]),
        };
        f64::from(v) / self.pleine_echelle
    }

    fn trame_lue(&mut self, trame: &[u8]) -> Option<Mesure> {
        if self.trames == 0 {
            self.debut_fenetre = self.position - self.trame as u64;
        }
        for c in 0..self.canaux {
            let i = c * self.octets_par_echantillon;
            let x = self.echantillon(&trame[i..i + self.octets_par_echantillon]);
            self.somme_carres += x * x;
            self.crete = self.crete.max(x.abs());
            let positif = x >= 0.0;
            if self.trames > 0 && positif != self.signe_precedent[c] {
                self.passages += 1;
            }
            self.signe_precedent[c] = positif;
            let (x1, x2) = self.precedents[c];
            self.somme_retard_1 += x * x1;
            self.somme_retard_2 += x * x2;
            self.precedents[c] = (x, x1);
        }
        self.trames += 1;
        if self.trames < self.trames_par_fenetre {
            return None;
        }
        let echantillons = (self.trames * self.canaux) as f64;
        let rms = (self.somme_carres / echantillons).sqrt();
        let taux_de_passages =
            self.passages as f64 / ((self.trames - 1) * self.canaux).max(1) as f64;
        let crete = self.crete;
        let debut = self.debut_fenetre;
        let energie = self.somme_carres.max(f64::MIN_POSITIVE);
        let autocorrelation = (self.somme_retard_1 / energie, self.somme_retard_2 / energie);
        self.trames = 0;
        self.somme_carres = 0.0;
        self.crete = 0.0;
        self.passages = 0;
        self.somme_retard_1 = 0.0;
        self.somme_retard_2 = 0.0;

        let bruyante = rms >= RMS_PLANCHER
            && (rms >= RMS_ELEVE || crete >= CRETE_ELEVEE)
            && taux_de_passages >= TAUX_DE_PASSAGES_MINIMAL
            && autocorrelation.0.abs() < AUTOCORRELATION_MAXIMALE
            && autocorrelation.1.abs() < AUTOCORRELATION_MAXIMALE;
        if !bruyante {
            self.fenetres_bruyantes = 0;
            return None;
        }
        if self.fenetres_bruyantes == 0 {
            self.debut_du_bruit = debut;
        }
        self.fenetres_bruyantes = self.fenetres_bruyantes.saturating_add(1);
        (self.fenetres_bruyantes == FENETRES_SOUTENUES).then(|| Mesure {
            rms,
            crete,
            taux_de_passages,
            autocorrelation,
            soutenu_ms: u64::from(self.fenetres_bruyantes) * 1000 / u64::from(FENETRES_PAR_SECONDE),
            position: self.debut_du_bruit,
        })
    }
}

/// L'observateur d'UNE connexion de la voie conversion. Vit dans le bilan de
/// la connexion ; voit chaque morceau du corps au moment où il part.
pub(crate) struct ObservateurDeConversion {
    session: Arc<StreamSession>,
    stream_id: String,
    agent: String,
    range: String,
    debut_du_corps: u64,
    premiers: Vec<u8>,
    contexte_journalise: bool,
    detecteur: Option<DetecteurDeBruit>,
    envoyes: u64,
}

impl ObservateurDeConversion {
    /// `debut_du_corps` : la position, dans le flux, du premier octet que ce
    /// corps envoie (0 pour `bytes=0-`, N pour une reprise `bytes=N-`).
    /// `detecter` : le corps est-il du WAV sans métadonnées ICY entrelacées ?
    pub(crate) fn nouveau(
        session: Arc<StreamSession>,
        stream_id: &str,
        agent: &str,
        range: &str,
        debut_du_corps: u64,
        detecter: bool,
    ) -> Self {
        let detecteur = detecter
            .then(|| {
                DetecteurDeBruit::nouveau(
                    session.info.sample_rate,
                    session.info.bit_depth,
                    session.info.channels,
                    debut_du_corps,
                )
            })
            .flatten();
        Self {
            session,
            stream_id: stream_id.to_string(),
            agent: agent.to_string(),
            range: range.to_string(),
            debut_du_corps,
            premiers: Vec::with_capacity(ENTETE),
            contexte_journalise: false,
            detecteur,
            envoyes: 0,
        }
    }

    /// Un morceau du corps vient de partir vers le renderer.
    pub(crate) fn observer(&mut self, octets: &[u8]) {
        if self.premiers.len() < ENTETE {
            let n = (ENTETE - self.premiers.len()).min(octets.len());
            self.premiers.extend_from_slice(&octets[..n]);
            if self.premiers.len() == ENTETE {
                self.journaliser_le_contexte();
            }
        }
        let position_du_bloc = self.debut_du_corps + self.envoyes;
        self.envoyes += octets.len() as u64;
        let Some(detecteur) = self.detecteur.as_mut() else {
            return;
        };
        let Some(mesure) = detecteur.observer(octets) else {
            return;
        };
        // Un WARN au plus par FLUX. Ce flux-ci est déjà signalé : inutile de
        // continuer à mesurer sur cette connexion.
        self.detecteur = None;
        if self
            .session
            .bruit_suspect_signale
            .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let ctx = self.session.contexte_de_conversion.get();
        let entete = EnteteWav::decoder(&self.premiers);
        warn!(
            stream_id = %self.stream_id,
            agent = %self.agent,
            range = %self.range,
            zone_id = ctx.map_or(-1, |c| c.zone_id),
            source = %ctx.map_or("absent", |c| c.source.as_str()),
            chaine = %ctx.map_or("absent", |c| c.chaine.as_str()),
            precedente = %ctx.map_or("absent", |c| c.precedente.as_str()),
            session_format = %format_de_session(&self.session),
            entete_envoye = %entete.map_or_else(|| "absent".to_string(), |e| e.to_string()),
            entete_hex = %hex(&self.premiers),
            rms_dbfs = %format!("{:.1}", mesure.rms_dbfs()),
            crete = %format!("{:.3}", mesure.crete),
            taux_passages_zero = %format!("{:.3}", mesure.taux_de_passages),
            autocorrelation = %format!(
                "{:.3}/{:.3}",
                mesure.autocorrelation.0, mesure.autocorrelation.1
            ),
            soutenu_ms = mesure.soutenu_ms,
            debut_du_bruit = mesure.position,
            position_du_bloc,
            bloc_octets = octets.len(),
            bloc_hex = %hex(&octets[..octets.len().min(OCTETS_DU_BLOC_JOURNALISES)]),
            "bruit_suspect — le PCM qui part ressemble à du bruit plein échelle soutenu \
             (niveau élevé, spectre plat). Diagnostic seulement : rien n'est coupé ni \
             modifié (#6130)"
        );
    }

    /// La connexion se termine : si elle n'a pas atteint 44 octets, sa ligne
    /// de contexte part maintenant, avec ce qu'elle a envoyé.
    pub(crate) fn cloturer(&mut self) {
        if !self.contexte_journalise {
            self.journaliser_le_contexte();
        }
    }

    fn journaliser_le_contexte(&mut self) {
        self.contexte_journalise = true;
        let ctx = self.session.contexte_de_conversion.get();
        let entete = EnteteWav::decoder(&self.premiers);
        let i = &self.session.info;
        let conforme = entete.map(|e| {
            e.frequence == i.sample_rate && e.profondeur == i.bit_depth && e.canaux == i.channels
        });
        info!(
            stream_id = %self.stream_id,
            agent = %self.agent,
            range = %self.range,
            debut_du_corps = self.debut_du_corps,
            zone_id = ctx.map_or(-1, |c| c.zone_id),
            source = %ctx.map_or("absent", |c| c.source.as_str()),
            chaine = %ctx.map_or("absent", |c| c.chaine.as_str()),
            precedente = %ctx.map_or("absent", |c| c.precedente.as_str()),
            session_format = %format_de_session(&self.session),
            entete_envoye = %entete.map_or_else(|| "absent".to_string(), |e| e.to_string()),
            entete_conforme_a_la_session = ?conforme,
            premiers_octets = self.premiers.len(),
            entete_hex = %hex(&self.premiers),
            detecteur = self.detecteur.is_some(),
            "conversion_connexion_contexte"
        );
    }
}

fn format_de_session(s: &StreamSession) -> String {
    format!(
        "{} {}/{}/{}",
        s.info.format, s.info.sample_rate, s.info.bit_depth, s.info.channels
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Générateur pseudo-aléatoire déterministe (xorshift).
    struct Alea(u64);
    impl Alea {
        fn suivant(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        }
    }

    const SR: u32 = 44_100;

    /// PCM 16 bits stéréo depuis une fonction (temps en s, canal) → [-1, 1].
    fn pcm16(secondes: f64, f: impl FnMut(f64, usize) -> f64) -> Vec<u8> {
        pcm(secondes, 16, f)
    }

    fn pcm(secondes: f64, profondeur: u16, mut f: impl FnMut(f64, usize) -> f64) -> Vec<u8> {
        let n = (secondes * f64::from(SR)) as usize;
        let pe = f64::from((1u32 << (profondeur - 1)) - 1);
        let mut v = Vec::with_capacity(n * 2 * usize::from(profondeur / 8));
        for i in 0..n {
            let t = i as f64 / f64::from(SR);
            for c in 0..2 {
                let x = (f(t, c).clamp(-1.0, 1.0) * pe) as i32;
                match profondeur {
                    16 => v.extend_from_slice(&(x as i16).to_le_bytes()),
                    24 => v.extend_from_slice(&x.to_le_bytes()[..3]),
                    _ => v.extend_from_slice(&x.to_le_bytes()),
                }
            }
        }
        v
    }

    fn wav(profondeur: u16, pcm: &[u8]) -> Vec<u8> {
        let mut v = tune_core::audio::wav::build_wav_header(2, SR, profondeur).to_vec();
        v.extend_from_slice(pcm);
        v
    }

    /// Le flux entier passé au détecteur en blocs de `bloc` octets, comme le
    /// corps HTTP les envoie. Rend chaque déclenchement.
    fn declenchements(profondeur: u16, flux: &[u8], bloc: usize) -> Vec<Mesure> {
        let mut d = DetecteurDeBruit::nouveau(SR, profondeur, 2, 0).expect("format lisible");
        flux.chunks(bloc).filter_map(|b| d.observer(b)).collect()
    }

    fn bruit_blanc(niveau: f64) -> impl FnMut(f64, usize) -> f64 {
        let mut a = Alea(0x6130_6130_dead_beef);
        move |_, _| a.suivant() * niveau
    }

    #[test]
    fn le_bruit_blanc_plein_echelle_declenche_une_fois() {
        let flux = wav(16, &pcm16(5.0, bruit_blanc(1.0)));
        let d = declenchements(16, &flux, 65_536);
        assert_eq!(
            d.len(),
            1,
            "un seul déclenchement pour un bruit continu : {d:?}"
        );
        let m = d[0];
        assert!(m.taux_de_passages > 0.45, "{m:?}");
        assert!(m.rms_dbfs() > -6.0, "{m:?}");
        assert_eq!(m.soutenu_ms, 2_000);
        assert_eq!(m.position, 44, "le bruit commence au premier octet de PCM");
    }

    #[test]
    fn le_bruit_blanc_declenche_en_24_et_32_bits_et_par_petits_blocs() {
        for profondeur in [24u16, 32] {
            let flux = wav(profondeur, &pcm(3.0, profondeur, bruit_blanc(0.8)));
            // Blocs de 1 000 octets : des trames coupées en deux à chaque bloc.
            assert_eq!(
                declenchements(profondeur, &flux, 1_000).len(),
                1,
                "{profondeur} bits"
            );
        }
    }

    #[test]
    fn un_bruit_blanc_a_moins_6_dbfs_declenche_encore() {
        // −6 dBFS de crête, environ −10,8 dBFS efficaces.
        let flux = wav(16, &pcm16(3.0, bruit_blanc(0.5)));
        assert_eq!(declenchements(16, &flux, 65_536).len(), 1);
    }

    #[test]
    fn un_sinus_plein_echelle_ne_declenche_pas() {
        // Jusqu'aux aigus : 15 kHz passe par zéro plus souvent qu'un bruit
        // blanc ; c'est l'autocorrélation qui l'écarte.
        for f in [50.0, 1_000.0, 5_000.0, 11_025.0, 15_000.0, 20_000.0] {
            let flux = wav(
                16,
                &pcm16(5.0, |t, _| (2.0 * std::f64::consts::PI * f * t).sin()),
            );
            assert!(declenchements(16, &flux, 65_536).is_empty(), "sinus {f} Hz");
        }
    }

    #[test]
    fn une_musique_synthetique_forte_ne_declenche_pas() {
        // Un accord riche en harmoniques, très compressé (−9 dBFS efficaces),
        // une caisse claire de 80 ms toutes les 500 ms à pleine échelle, et
        // une cymbale continue à −26 dBFS : le pire de la musique moderne.
        let mut a = Alea(42);
        let fondamentales = [110.0, 138.6, 164.8, 220.0];
        let flux = wav(
            16,
            &pcm16(10.0, |t, c| {
                let mut x = 0.0;
                for (k, f0) in fondamentales.iter().enumerate() {
                    for h in 1..8 {
                        let phase = (k * 7 + h + c) as f64;
                        x += (2.0 * std::f64::consts::PI * f0 * h as f64 * t + phase).sin()
                            / h as f64;
                    }
                }
                let accord = (x / 4.0).tanh() * 0.6;
                let cymbale = a.suivant() * 0.05;
                let caisse = if (t % 0.5) < 0.08 {
                    a.suivant() * 0.9
                } else {
                    0.0
                };
                accord + cymbale + caisse
            }),
        );
        assert!(declenchements(16, &flux, 65_536).is_empty());
    }

    #[test]
    fn le_silence_et_un_souffle_faible_ne_declenchent_pas() {
        let silence = wav(16, &vec![0u8; 44_100 * 4 * 5]);
        assert!(declenchements(16, &silence, 65_536).is_empty());
        // Souffle de bande à −40 dBFS : plat, mais faible.
        let souffle = wav(16, &pcm16(5.0, bruit_blanc(0.01)));
        assert!(declenchements(16, &souffle, 65_536).is_empty());
    }

    #[test]
    fn un_porteur_dop_ne_declenche_pas() {
        // DoP 24 bits : marqueur 0x05 / 0xFA alterné dans l'octet de poids
        // fort, bits DSD dessous — le signe change à chaque échantillon, mais
        // le niveau reste vers −28 dBFS.
        let mut a = Alea(7);
        let mut v = Vec::new();
        for i in 0..(176_400 * 4) {
            let marqueur: u8 = if (i / 2) % 2 == 0 { 0x05 } else { 0xFA };
            let bits = (a.suivant() * 32_767.0) as i16 as u16;
            v.extend_from_slice(&[bits as u8, (bits >> 8) as u8, marqueur]);
        }
        let mut d = DetecteurDeBruit::nouveau(176_400, 24, 2, 0).unwrap();
        let flux = {
            let mut f = tune_core::audio::wav::build_wav_header(2, 176_400, 24).to_vec();
            f.extend_from_slice(&v);
            f
        };
        assert!(flux.chunks(65_536).all(|b| d.observer(b).is_none()));
    }

    #[test]
    fn un_pcm_decale_d_un_octet_se_lit_comme_le_renderer_l_entend() {
        // Une musique forte servie avec un octet de trop après l'en-tête :
        // poids forts et faibles inversés, le renderer entend du bruit.
        let musique = pcm16(5.0, |t, _| {
            0.7 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()
        });
        let mut flux = tune_core::audio::wav::build_wav_header(2, SR, 16).to_vec();
        flux.push(0);
        flux.extend_from_slice(&musique);
        assert_eq!(declenchements(16, &flux, 65_536).len(), 1);
    }

    #[test]
    fn une_reprise_au_milieu_d_une_trame_se_cale_sur_la_grille() {
        // `bytes=46-` : 2 octets dans la première trame. Le détecteur doit
        // sauter jusqu'à la trame suivante, pas lire à cheval.
        let sinus = wav(
            16,
            &pcm16(5.0, |t, _| (2.0 * std::f64::consts::PI * 440.0 * t).sin()),
        );
        let mut d = DetecteurDeBruit::nouveau(SR, 16, 2, 46).unwrap();
        assert!(sinus[46..].chunks(65_536).all(|b| d.observer(b).is_none()));
    }

    #[test]
    fn l_entete_decode_rend_les_parametres_exacts() {
        let h = tune_core::audio::wav::build_wav_header(2, 44_100, 16);
        let e = EnteteWav::decoder(&h).expect("en-tête");
        assert_eq!(
            (e.frequence, e.profondeur, e.canaux, e.format),
            (44_100, 16, 2, 1)
        );
        assert_eq!(e.octets_par_seconde, 176_400);
        assert_eq!(e.alignement, 4);
        assert!(e.coherent());
        let texte = e.to_string();
        assert!(
            texte.starts_with(
                "frequence=44100 profondeur=16 canaux=2 format=1 octets_par_s=176400 alignement=4"
            ),
            "{texte}"
        );
        assert!(EnteteWav::decoder(&h[..43]).is_none());
        assert!(EnteteWav::decoder(&[0u8; 44]).is_none());
        let h24 = tune_core::audio::wav::build_wav_header(2, 48_000, 24);
        let e24 = EnteteWav::decoder(&h24).unwrap();
        assert_eq!(
            (e24.frequence, e24.profondeur, e24.alignement),
            (48_000, 24, 6)
        );
    }

    #[test]
    fn un_en_tete_livre_par_petits_morceaux_ne_trouble_pas_le_detecteur() {
        let flux = wav(16, &pcm16(3.0, bruit_blanc(1.0)));
        let mut d = DetecteurDeBruit::nouveau(SR, 16, 2, 0).unwrap();
        // 10 + 10 + 10 + 14 octets d'en-tête, puis le PCM.
        let mut n = 0;
        for b in [&flux[..10], &flux[10..20], &flux[20..30], &flux[30..44]] {
            n += usize::from(d.observer(b).is_some());
        }
        n += flux[44..]
            .chunks(65_536)
            .filter(|b| d.observer(b).is_some())
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn l_hexadecimal_est_celui_des_octets() {
        assert_eq!(hex(b"RIFF"), "52494646");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
