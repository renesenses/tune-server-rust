//! Le signal joué vers la zone : un `Read` PCM qui obéit, pendant qu'il
//! joue, aux consignes de l'analyseur — salve d'essai à tel niveau, balayage
//! complet à tel niveau, fin — et dit à quelle trame du flux chaque segment
//! est parti.
//!
//! Pourquoi un générateur piloté et non un fichier préparé : la montée de
//! niveau prudente (`tune_calibration::level::LevelPolicy`) DÉCIDE après
//! chaque salve, d'après ce que le micro a capté. Le signal suivant n'existe
//! donc pas avant la décision. Entre deux consignes, le générateur rend du
//! silence : la session de la zone reste alimentée, rien ne craque.
//!
//! Le plafond de niveau joué est tenu ICI, au dernier étage : quelle que soit
//! la consigne, aucune amplitude ne dépasse `plafond` (−6 dBFS par défaut,
//! non négociable, note §5 risque 5). Une consigne au-dessus est écrêtée et
//! comptée (`consignes_ecretees`), ce que les essais vérifient.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tune_calibration::fft;
use tune_calibration::sweep::{self, Sweep, SweepSpec};
use tune_core::source_pcm::FormatPcm;

/// Les voies de la zone qui reçoivent le signal (l'autre reste muette).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voies {
    Gauche,
    Droite,
    LesDeux,
}

impl Voies {
    pub fn depuis(nom: &str) -> Option<Self> {
        match nom {
            "gauche" | "left" | "l" => Some(Self::Gauche),
            "droite" | "right" | "r" => Some(Self::Droite),
            "deux" | "les_deux" | "both" | "lr" => Some(Self::LesDeux),
            _ => None,
        }
    }

    pub fn nom(self) -> &'static str {
        match self {
            Self::Gauche => "gauche",
            Self::Droite => "droite",
            Self::LesDeux => "deux",
        }
    }
}

/// Ce que l'analyseur demande au générateur.
#[derive(Debug, Clone, PartialEq)]
pub enum Consigne {
    /// Une salve d'essai (balayage court) à `niveau_dbfs` crête.
    Salve { niveau_dbfs: f64 },
    /// Le balayage complet à `niveau_dbfs` crête.
    Balayage { niveau_dbfs: f64 },
    /// Plus rien à jouer : le flux se termine.
    Fin,
}

/// Un segment parti dans le flux.
#[derive(Debug, Clone, PartialEq)]
pub struct Emission {
    pub consigne: Consigne,
    /// Trame du flux où le segment commence.
    pub debut_trame: u64,
    /// Longueur du segment, en trames.
    pub trames: u64,
    /// Niveau réellement joué (après plafond), dBFS crête.
    pub niveau_dbfs: f64,
}

/// Paramètres du déroulé : durées, format, plafond.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub format: FormatPcm,
    /// Balayage de la salve d'essai (court).
    pub salve: SweepSpec,
    /// Balayage complet.
    pub balayage: SweepSpec,
    /// Silence de garde après chaque segment (s) : la queue de la pièce et la
    /// latence de la sortie doivent tenir dedans.
    pub garde_s: f64,
    /// Nombre maximal de salves (la montée de niveau est bornée).
    pub salves_max: usize,
    /// Niveau joué maximal, dBFS crête. −6 dBFS : non négociable.
    pub plafond_dbfs: f64,
    pub voies: Voies,
}

impl Plan {
    pub fn defaut(frequence: u32, duree_balayage_s: f64, voies: Voies) -> Self {
        let mut balayage = SweepSpec::room_default(frequence);
        balayage.duration_s = duree_balayage_s;
        let mut salve = SweepSpec::room_default(frequence);
        salve.duration_s = 1.0;
        salve.f_start = 20.0;
        Self {
            format: FormatPcm {
                frequence,
                canaux: 2,
                bits: 24,
            },
            salve,
            balayage,
            garde_s: 2.5,
            salves_max: 6,
            plafond_dbfs: -6.0,
            voies,
        }
    }

    fn trames(spec: &SweepSpec, garde_s: f64) -> u64 {
        spec.len_samples() as u64 + (garde_s * spec.sample_rate as f64).round() as u64
    }

    /// Longueur maximale du flux, en trames : silence d'amorce, toutes les
    /// salves, le balayage et leurs gardes. C'est le `Content-Length` promis.
    pub fn trames_max(&self) -> u64 {
        let amorce = (0.5 * self.format.frequence as f64) as u64;
        amorce
            + self.salves_max as u64 * Self::trames(&self.salve, self.garde_s)
            + Self::trames(&self.balayage, self.garde_s)
    }

    pub fn octets_max(&self) -> u64 {
        self.trames_max() * self.format.octets_par_trame()
    }

    pub fn duree_max_ms(&self) -> u64 {
        self.trames_max() * 1000 / self.format.frequence as u64
    }
}

struct EtatDeroule {
    en_attente: Option<Consigne>,
    emissions: Vec<Emission>,
    /// Le générateur a été ouvert par la zone (le flux est servi).
    ouvert: bool,
    /// Trames rendues par le générateur jusqu'ici.
    trames_rendues: u64,
    fini: bool,
}

/// L'état partagé entre l'analyseur (qui donne les consignes) et le
/// générateur (qui les joue et le dit).
pub struct Deroule {
    pub plan: Plan,
    etat: Mutex<EtatDeroule>,
    signal: Condvar,
    annule: AtomicBool,
    pub consignes_ecretees: AtomicU64,
    /// Balayages pré-calculés à amplitude 1 ; l'amplitude s'applique à la
    /// copie, au moment de jouer.
    salve_unitaire: Sweep,
    balayage_unitaire: Sweep,
}

impl Deroule {
    pub fn new(plan: Plan) -> Arc<Self> {
        let salve_unitaire = sweep::generate(&SweepSpec {
            amplitude: 1.0,
            ..plan.salve.clone()
        });
        let balayage_unitaire = sweep::generate(&SweepSpec {
            amplitude: 1.0,
            ..plan.balayage.clone()
        });
        Arc::new(Self {
            plan,
            etat: Mutex::new(EtatDeroule {
                en_attente: None,
                emissions: Vec::new(),
                ouvert: false,
                trames_rendues: 0,
                fini: false,
            }),
            signal: Condvar::new(),
            annule: AtomicBool::new(false),
            consignes_ecretees: AtomicU64::new(0),
            salve_unitaire,
            balayage_unitaire,
        })
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, EtatDeroule> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Le balayage de la salve, à l'amplitude jouée : c'est lui qu'il faut
    /// déconvoluer (le filtre inverse ne dépend pas de l'amplitude).
    pub fn salve(&self) -> &Sweep {
        &self.salve_unitaire
    }

    pub fn balayage(&self) -> &Sweep {
        &self.balayage_unitaire
    }

    /// Donne une consigne ; la précédente non encore prise est remplacée.
    pub fn consigner(&self, c: Consigne) {
        let mut e = self.verrou();
        e.en_attente = Some(c);
        drop(e);
        self.signal.notify_all();
    }

    pub fn annuler(&self) {
        self.annule.store(true, Ordering::SeqCst);
        self.signal.notify_all();
    }

    pub fn annulee(&self) -> bool {
        self.annule.load(Ordering::SeqCst)
    }

    pub fn ouvert(&self) -> bool {
        self.verrou().ouvert
    }

    pub fn fini(&self) -> bool {
        self.verrou().fini
    }

    pub fn trames_rendues(&self) -> u64 {
        self.verrou().trames_rendues
    }

    pub fn emissions(&self) -> Vec<Emission> {
        self.verrou().emissions.clone()
    }

    /// Attend que la zone ait ouvert le flux.
    pub fn attendre_ouverture(&self, delai: Duration) -> bool {
        let echeance = Instant::now() + delai;
        let mut e = self.verrou();
        while !e.ouvert {
            if self.annulee() {
                return false;
            }
            let reste = echeance.saturating_duration_since(Instant::now());
            if reste.is_zero() {
                return false;
            }
            e = self
                .signal
                .wait_timeout(e, reste)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }

    /// Attend la `n`-ième émission (à partir de 0).
    pub fn attendre_emission(&self, n: usize, delai: Duration) -> Option<Emission> {
        let echeance = Instant::now() + delai;
        let mut e = self.verrou();
        loop {
            if let Some(em) = e.emissions.get(n) {
                return Some(em.clone());
            }
            if self.annulee() || e.fini {
                return None;
            }
            let reste = echeance.saturating_duration_since(Instant::now());
            if reste.is_zero() {
                return None;
            }
            e = self
                .signal
                .wait_timeout(e, reste)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Le niveau effectivement joué pour une consigne : jamais au-dessus du
    /// plafond.
    fn plafonner(&self, niveau_dbfs: f64) -> f64 {
        if niveau_dbfs > self.plan.plafond_dbfs + 1e-9 {
            self.consignes_ecretees.fetch_add(1, Ordering::Relaxed);
            self.plan.plafond_dbfs
        } else {
            niveau_dbfs
        }
    }
}

/// Le segment en cours de rendu.
struct Segment {
    echantillons: Arc<Vec<f64>>,
    position: usize,
    /// Trames de garde (silence) qui suivent.
    garde: u64,
}

/// Le `Read` servi à la pompe de l'orchestrateur.
pub struct Generateur {
    deroule: Arc<Deroule>,
    segment: Option<Segment>,
    trames_rendues: u64,
    trames_max: u64,
    reste: Vec<u8>,
}

impl Generateur {
    pub fn new(deroule: Arc<Deroule>) -> Self {
        let trames_max = deroule.plan.trames_max();
        {
            let mut e = deroule.verrou();
            e.ouvert = true;
        }
        deroule.signal.notify_all();
        Self {
            deroule,
            segment: None,
            trames_rendues: 0,
            trames_max,
            reste: Vec::new(),
        }
    }

    /// Prend la consigne en attente, s'il y en a une, et ouvre son segment.
    fn prendre_consigne(&mut self) -> bool {
        let mut e = self.deroule.verrou();
        let Some(c) = e.en_attente.take() else {
            return false;
        };
        let fs = self.deroule.plan.format.frequence as f64;
        let garde = (self.deroule.plan.garde_s * fs).round() as u64;
        let (source, niveau) = match &c {
            Consigne::Salve { niveau_dbfs } => (&self.deroule.salve_unitaire, *niveau_dbfs),
            Consigne::Balayage { niveau_dbfs } => (&self.deroule.balayage_unitaire, *niveau_dbfs),
            Consigne::Fin => {
                e.fini = true;
                drop(e);
                self.deroule.signal.notify_all();
                return true;
            }
        };
        drop(e);
        let niveau = self.deroule.plafonner(niveau);
        let amp = fft::from_db(niveau);
        let echantillons: Vec<f64> = source.signal.iter().map(|v| v * amp).collect();
        let trames = echantillons.len() as u64;
        let mut e = self.deroule.verrou();
        e.emissions.push(Emission {
            consigne: c,
            debut_trame: self.trames_rendues,
            trames,
            niveau_dbfs: niveau,
        });
        drop(e);
        self.deroule.signal.notify_all();
        self.segment = Some(Segment {
            echantillons: Arc::new(echantillons),
            position: 0,
            garde,
        });
        true
    }

    fn ecrire_trame(&self, sortie: &mut Vec<u8>, v: f64) {
        let plan = &self.deroule.plan;
        let v = v.clamp(-1.0, 1.0);
        let (g, d) = match plan.voies {
            Voies::Gauche => (v, 0.0),
            Voies::Droite => (0.0, v),
            Voies::LesDeux => (v, v),
        };
        for x in [g, d].into_iter().take(plan.format.canaux as usize) {
            match plan.format.bits {
                16 => sortie.extend_from_slice(&((x * 32_767.0).round() as i16).to_le_bytes()),
                _ => {
                    let n = (x * 8_388_607.0).round() as i32;
                    sortie.extend_from_slice(&n.to_le_bytes()[..3]);
                }
            }
        }
    }

    /// Produit `n` trames au plus, en octets.
    fn produire(&mut self, n: usize) -> Vec<u8> {
        let mut sortie =
            Vec::with_capacity(n * self.deroule.plan.format.octets_par_trame() as usize);
        let mut produites = 0usize;
        while produites < n {
            if self.trames_rendues >= self.trames_max || self.deroule.annulee() {
                break;
            }
            if self.segment.is_none() {
                let _ = self.prendre_consigne();
            }
            if self.deroule.fini() && self.segment.is_none() {
                break;
            }
            let v = match self.segment.as_mut() {
                Some(s) if s.position < s.echantillons.len() => {
                    let v = s.echantillons[s.position];
                    s.position += 1;
                    v
                }
                Some(s) if s.garde > 0 => {
                    s.garde -= 1;
                    0.0
                }
                Some(_) => {
                    self.segment = None;
                    continue;
                }
                None => 0.0,
            };
            self.ecrire_trame(&mut sortie, v);
            self.trames_rendues += 1;
            produites += 1;
        }
        {
            let mut e = self.deroule.verrou();
            e.trames_rendues = self.trames_rendues;
        }
        sortie
    }
}

impl Read for Generateur {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.reste.is_empty() {
            // Des blocs de 10 ms : une consigne est prise au bloc suivant.
            let bloc = (self.deroule.plan.format.frequence / 100).max(1) as usize;
            self.reste = self.produire(bloc);
            if self.reste.is_empty() {
                let mut e = self.deroule.verrou();
                e.fini = true;
                drop(e);
                self.deroule.signal.notify_all();
                return Ok(0);
            }
        }
        let n = buf.len().min(self.reste.len());
        buf[..n].copy_from_slice(&self.reste[..n]);
        self.reste.drain(..n);
        Ok(n)
    }
}

impl Drop for Generateur {
    fn drop(&mut self) {
        let mut e = self.deroule.verrou();
        e.fini = true;
        drop(e);
        self.deroule.signal.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        let mut p = Plan::defaut(48_000, 2.0, Voies::LesDeux);
        p.garde_s = 0.1;
        p
    }

    fn tout_lire(g: &mut Generateur) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = g.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn crete_24(octets: &[u8]) -> f64 {
        octets
            .chunks_exact(3)
            .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f64 / 8_388_608.0)
            .fold(0.0f64, |m, v| m.max(v.abs()))
    }

    /// Une consigne au-dessus du plafond est jouée AU plafond, et comptée.
    #[test]
    fn le_plafond_de_niveau_joue_tient_au_dernier_etage() {
        let d = Deroule::new(plan());
        d.consigner(Consigne::Salve { niveau_dbfs: 0.0 });
        let mut g = Generateur::new(d.clone());
        let mut buf = vec![0u8; 6 * 48_000 * 2];
        let mut lu = 0;
        while lu < buf.len() {
            let n = g.read(&mut buf[lu..]).unwrap();
            if n == 0 {
                break;
            }
            lu += n;
        }
        let crete = crete_24(&buf[..lu]);
        assert!(
            fft::to_db(crete) <= -6.0 + 0.01,
            "crête jouée {:.2} dBFS au-dessus du plafond",
            fft::to_db(crete)
        );
        assert_eq!(d.consignes_ecretees.load(Ordering::Relaxed), 1);
        let em = d.emissions();
        assert_eq!(em.len(), 1);
        assert_eq!(em[0].niveau_dbfs, -6.0);
    }

    /// Sans consigne : du silence, et le flux ne s'arrête qu'à `Fin` ou à la
    /// longueur maximale promise.
    #[test]
    fn sans_consigne_le_generateur_rend_du_silence_puis_finit_sur_fin() {
        let d = Deroule::new(plan());
        let mut g = Generateur::new(d.clone());
        assert!(d.ouvert());
        let mut buf = vec![0u8; 48_000 * 6];
        g.read_exact(&mut buf).unwrap();
        assert!(buf.iter().all(|b| *b == 0));
        d.consigner(Consigne::Fin);
        let reste = tout_lire(&mut g);
        assert!(reste.len() < 48_000 * 6 * 2, "le flux ne finit pas");
        assert!(d.fini());
    }

    /// Une voie seule : l'autre reste exactement nulle.
    #[test]
    fn la_voie_droite_seule_laisse_la_gauche_muette() {
        let mut p = plan();
        p.voies = Voies::Droite;
        let d = Deroule::new(p);
        d.consigner(Consigne::Salve { niveau_dbfs: -20.0 });
        let mut g = Generateur::new(d.clone());
        let mut buf = vec![0u8; 6 * 48_000];
        g.read_exact(&mut buf).unwrap();
        let gauche: Vec<u8> = buf.chunks_exact(6).flat_map(|t| t[..3].to_vec()).collect();
        let droite: Vec<u8> = buf.chunks_exact(6).flat_map(|t| t[3..].to_vec()).collect();
        assert_eq!(crete_24(&gauche), 0.0);
        assert!(crete_24(&droite) > 0.05);
    }

    /// L'émission dit où le segment commence dans le flux.
    #[test]
    fn l_emission_porte_la_trame_de_depart() {
        let d = Deroule::new(plan());
        let mut g = Generateur::new(d.clone());
        let mut buf = vec![0u8; 6 * 4_800];
        g.read_exact(&mut buf).unwrap();
        d.consigner(Consigne::Salve { niveau_dbfs: -30.0 });
        g.read_exact(&mut buf).unwrap();
        let em = d.attendre_emission(0, Duration::from_secs(1)).unwrap();
        assert_eq!(em.debut_trame, 4_800);
        assert_eq!(em.trames, 48_000);
        assert_eq!(em.niveau_dbfs, -30.0);
    }

    #[test]
    fn la_longueur_promise_est_une_borne() {
        let p = plan();
        let d = Deroule::new(p.clone());
        d.consigner(Consigne::Balayage { niveau_dbfs: -12.0 });
        let mut g = Generateur::new(d.clone());
        let tout = tout_lire(&mut g);
        assert!(tout.len() as u64 <= p.octets_max());
        assert_eq!(d.trames_rendues(), p.trames_max());
    }
}
