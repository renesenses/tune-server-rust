//! Ce que le greffon demande au système : lister les entrées, en démarrer une
//! au format natif, et relire sa fréquence nominale courante.
//!
//! Un trait, pour que le contrôleur, le lecteur et les routes se prouvent
//! sans matériel (`simule.rs`). L'implémentation réelle (`systeme.rs`, cpal)
//! n'existe qu'avec la feature `capture`.

use std::sync::Arc;

use serde::Serialize;
use tune_core::source_pcm::FormatPcm;

use crate::anneau::Anneau;

/// Une entrée audio telle que `/entrees` la décrit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DescriptionEntree {
    /// Le nom affiché, et la clé de `/jouer` (l'`id` est accepté aussi).
    pub nom: String,
    pub id: String,
    pub canaux: u16,
    /// Fréquences que le périphérique accepte (bornes des plages annoncées).
    pub frequences: Vec<u32>,
    /// Sa fréquence courante : celle que la capture prendra.
    pub frequence_courante: Option<u32>,
    /// Formats d'échantillons que le pilote rend (`f32`, `i16`, `i24`…).
    pub formats: Vec<String>,
    /// Profondeur du format PHYSIQUE, quand le système la dit (CoreAudio).
    pub bits_physiques: Option<u16>,
    /// Profondeur servie à la zone.
    pub bits_servis: u16,
    pub par_defaut: bool,
    /// Périphérique VIRTUEL (Loopback Audio, BlackHole…), quand le système
    /// le dit (transport CoreAudio `Virtual`) ; `None` : inconnu. Aucune
    /// entrée n'est écartée pour autant.
    pub virtuelle: Option<bool>,
}

/// Arrête une capture (le flux du pilote vit dans son propre fil).
pub trait Arret: Send {
    fn arreter(self: Box<Self>);
}

/// Une capture démarrée : son format servi, et de quoi l'arrêter.
pub struct CaptureDemarree {
    pub nom: String,
    /// #9070 — l'identifiant exact du périphérique ouvert (`alsa:hw:CARD=…`,
    /// `alsa:dsnoop:…`, `coreaudio:…`) : sous ALSA, plusieurs entrées portent
    /// le même nom, et seul l'identifiant dit quel PCM a été ouvert.
    pub id: String,
    pub format: FormatPcm,
    pub arret: Box<dyn Arret>,
}

/// #9070 — le PCM ALSA d'un identifiant cpal (`alsa:hw:CARD=U18dB,DEV=0`) :
/// son type d'accès (`hw`, `plughw`, `sysdefault`, `dsnoop`, `front`…) et
/// sa carte (`CARD`, `DEV`, 0 par défaut). `None` hors ALSA, ou pour un PCM
/// sans carte (`default`, `pipewire`, `pulse`).
fn pcm_alsa(id: &str) -> Option<(&str, (&str, &str))> {
    let pcm = id.strip_prefix("alsa:")?;
    let (acces, parametres) = pcm.split_once(':')?;
    let mut carte = None;
    let mut dev = "0";
    for p in parametres.split(',') {
        match p.split_once('=') {
            Some(("CARD", c)) => carte = Some(c),
            Some(("DEV", d)) => dev = d,
            _ => {}
        }
    }
    Some((acces, (carte?, dev)))
}

/// #9070 — le type d'accès ALSA d'une entrée (`hw`, `plughw`, `dsnoop`…),
/// à afficher à côté de son nom ; `None` hors ALSA.
pub fn acces_alsa(id: &str) -> Option<&str> {
    let pcm = id.strip_prefix("alsa:")?;
    Some(pcm.split_once(':').map(|(a, _)| a).unwrap_or(pcm))
}

/// #9070 — l'entrée matérielle conseillée pour une entrée ALSA :
/// `hw:CARD=X,DEV=Y` de la même carte. `None` hors ALSA ou sans carte.
pub fn entree_materielle_alsa(id: &str) -> Option<String> {
    let (_, (carte, dev)) = pcm_alsa(id)?;
    Some(format!("hw:CARD={carte},DEV={dev}"))
}

fn rang_materiel(acces: &str) -> Option<u8> {
    match acces {
        "hw" => Some(0),
        "plughw" => Some(1),
        _ => None,
    }
}

/// #9070 — parmi des entrées de MÊME nom (énumération brute de cpal, dans
/// son ordre), celle qu'une ouverture par le nom doit prendre : la même que
/// [`une_ligne_par_carte_alsa`] garde dans la liste — `hw:` d'une carte
/// nommée, puis `plughw:`, puis les mêmes par index de carte. Sans accès
/// matériel (ou hors ALSA), la première, comme avant. `None` : liste vide.
///
/// Sans elle, `/jouer` par le nom ouvrait la première entrée de ce nom, qui
/// pouvait ne pas être celle que la liste montrait (`id_systeme`).
pub fn choisir_parmi_homonymes(ids: &[String]) -> Option<usize> {
    let rang = |id: &str| -> u8 {
        match pcm_alsa(id) {
            Some((acces, (carte, _))) => {
                let par_index = carte.chars().all(|c| c.is_ascii_digit());
                match (rang_materiel(acces), par_index) {
                    (Some(r), false) => r,
                    (Some(r), true) => 2 + r,
                    (None, _) => 4,
                }
            }
            None => 4,
        }
    };
    (0..ids.len()).min_by_key(|&i| (rang(&ids[i]), i))
}

/// #9070 — sous ALSA, cpal énumère pour une même carte `sysdefault`,
/// `front`, `dsnoop`, `hw`, `plughw`… tous sous le MÊME nom (« Umik-1
/// Gain: 18dB, USB Audio ») : la liste montrait cinq lignes identiques, et
/// rien ne disait laquelle ouvrait quoi. Une seule ligne par carte et
/// périphérique : `hw:` (accès direct, format natif), sinon `plughw:`.
/// Une carte sans l'un ni l'autre garde toutes ses lignes ; les entrées hors
/// ALSA (CoreAudio, WASAPI) ou sans carte (`default`, `pipewire`) ne sont
/// pas touchées. Une entrée écartée s'ouvre encore par son identifiant.
pub fn une_ligne_par_carte_alsa(entrees: Vec<DescriptionEntree>) -> Vec<DescriptionEntree> {
    use std::collections::HashMap;
    // Pour chaque (carte, périphérique) : (rang, index) de la meilleure entrée
    // matérielle, et si l'une de ses lignes était l'entrée par défaut.
    let mut meilleures: HashMap<(String, String), (u8, usize)> = HashMap::new();
    for (i, e) in entrees.iter().enumerate() {
        if let Some((acces, (carte, dev))) = pcm_alsa(&e.id)
            && let Some(r) = rang_materiel(acces)
        {
            let cle = (carte.to_string(), dev.to_string());
            let m = meilleures.entry(cle).or_insert((r, i));
            if r < m.0 {
                *m = (r, i);
            }
        }
    }
    let mut defauts: Vec<(String, String)> = Vec::new();
    let mut gardees: Vec<DescriptionEntree> = Vec::new();
    for (i, e) in entrees.into_iter().enumerate() {
        let groupe = pcm_alsa(&e.id).map(|(_, (c, d))| (c.to_string(), d.to_string()));
        match groupe
            .as_ref()
            .and_then(|g| meilleures.get(g).map(|m| (g, m)))
        {
            Some((g, &(_, garde))) => {
                if e.par_defaut {
                    defauts.push(g.clone());
                }
                if i == garde {
                    gardees.push(e);
                }
            }
            None => gardees.push(e),
        }
    }
    let gardees: Vec<DescriptionEntree> = gardees
        .into_iter()
        .map(|mut e| {
            if let Some(g) = pcm_alsa(&e.id).map(|(_, (c, d))| (c.to_string(), d.to_string()))
                && defauts.contains(&g)
            {
                e.par_defaut = true;
            }
            e
        })
        .collect();
    // cpal ajoute aux indications ALSA un `hw:CARD=1,DEV=0` (index de la
    // carte) pour chaque carte physique : la même carte, sous le même nom,
    // que `hw:CARD=U18dB,DEV=0`. L'entrée par index s'efface devant celle par
    // nom quand les deux ont le même nom et le même périphérique.
    let nommees: Vec<(String, String)> = gardees
        .iter()
        .filter_map(|e| {
            let (_, (carte, dev)) = pcm_alsa(&e.id)?;
            (!carte.chars().all(|c| c.is_ascii_digit())).then(|| (e.nom.clone(), dev.to_string()))
        })
        .collect();
    gardees
        .into_iter()
        .filter(|e| match pcm_alsa(&e.id) {
            Some((_, (carte, dev))) if carte.chars().all(|c| c.is_ascii_digit()) => {
                !nommees.contains(&(e.nom.clone(), dev.to_string()))
            }
            _ => true,
        })
        .collect()
}

pub trait Peripheriques: Send + Sync {
    /// `coreaudio`, `alsa`, `wasapi`, `simule`, ou `aucune` (capture non
    /// compilée).
    fn pile(&self) -> &'static str;
    fn lister(&self) -> Result<Vec<DescriptionEntree>, String>;
    /// Le format servi SI l'on démarrait `entree` maintenant.
    fn format_natif(&self, entree: &str) -> Result<(String, FormatPcm), String>;
    /// Démarre la capture de `entree` à son format natif ; le rappel du
    /// pilote pousse dans `anneau`, qu'il ferme en `Fin::Erreur` si le
    /// périphérique faillit. `anneau` doit avoir été créé au format rendu par
    /// [`Self::format_natif`].
    fn demarrer(&self, entree: &str, anneau: Arc<Anneau>) -> Result<CaptureDemarree, String>;
    /// Sa fréquence nominale ACTUELLE (un S/PDIF peut en changer).
    fn frequence_courante(&self, entree: &str) -> Option<u32>;
}

/// Sans la feature `capture` : le greffon répond, et dit pourquoi il ne
/// capte rien.
pub struct AucunePile;

pub const CAPTURE_NON_COMPILEE: &str =
    "la capture audio n'est pas compilée dans ce serveur (feature local-audio absente)";

impl Peripheriques for AucunePile {
    fn pile(&self) -> &'static str {
        "aucune"
    }
    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn format_natif(&self, _: &str) -> Result<(String, FormatPcm), String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn demarrer(&self, _: &str, _: Arc<Anneau>) -> Result<CaptureDemarree, String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn frequence_courante(&self, _: &str) -> Option<u32> {
        None
    }
}

/// Les périphériques du système : cpal avec la feature `capture`.
pub fn du_systeme() -> Arc<dyn Peripheriques> {
    #[cfg(feature = "capture")]
    {
        Arc::new(crate::systeme::Systeme)
    }
    #[cfg(not(feature = "capture"))]
    {
        Arc::new(AucunePile)
    }
}

#[cfg(test)]
mod une_ligne_par_carte_9070 {
    use super::*;

    fn entree(id: &str, nom: &str, par_defaut: bool) -> DescriptionEntree {
        DescriptionEntree {
            nom: nom.into(),
            id: id.into(),
            canaux: 2,
            frequences: vec![48_000],
            frequence_courante: Some(48_000),
            formats: vec!["i24".into()],
            bits_physiques: None,
            bits_servis: 24,
            par_defaut,
            virtuelle: None,
        }
    }

    const UMIK: &str = "Umik-1  Gain: 18dB, USB Audio";

    /// L'énumération ALSA du testeur du fil 2770 : cinq PCM de la même
    /// carte sous le même nom, plus les PCM sans carte.
    #[test]
    fn une_carte_alsa_ne_montre_que_son_entree_hw() {
        let l = vec![
            entree("alsa:default", "Default ALSA Device", false),
            entree("alsa:pipewire", "PipeWire Sound Server", false),
            entree("alsa:sysdefault:CARD=U18dB", UMIK, true),
            entree("alsa:front:CARD=U18dB,DEV=0", UMIK, true),
            entree("alsa:dsnoop:CARD=U18dB,DEV=0", UMIK, true),
            entree("alsa:hw:CARD=U18dB,DEV=0", UMIK, false),
            entree("alsa:plughw:CARD=U18dB,DEV=0", UMIK, false),
            entree("alsa:usbstream:CARD=U18dB", "Umik-1  Gain: 18dB", false),
            // Ajoutées par cpal d'après l'index de la carte (8 lignes en tout).
            entree("alsa:hw:CARD=1,DEV=0", UMIK, false),
            entree("alsa:plughw:CARD=1,DEV=0", UMIK, false),
        ];
        let r = une_ligne_par_carte_alsa(l);
        let ids: Vec<&str> = r.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            ["alsa:default", "alsa:pipewire", "alsa:hw:CARD=U18dB,DEV=0"],
            "#9070 : une seule ligne par carte ALSA, l'accès matériel hw:"
        );
        assert!(r[2].par_defaut, "le défaut d'une ligne écartée passe à hw:");
        assert_eq!(r.iter().filter(|e| e.nom == UMIK).count(), 1);
    }

    /// Une carte que les indications ALSA ne nomment pas garde son entrée par
    /// index ; deux cartes de même nom (deux UMIK-1) gardent chacune la leur.
    #[test]
    fn l_entree_par_index_ne_s_efface_que_devant_la_meme_carte() {
        let r = une_ligne_par_carte_alsa(vec![
            entree("alsa:hw:CARD=2,DEV=0", "Carte sans indication", false),
            entree("alsa:plughw:CARD=2,DEV=0", "Carte sans indication", false),
            entree("alsa:hw:CARD=U18dB,DEV=0", UMIK, false),
            entree("alsa:hw:CARD=U18dB_1,DEV=0", UMIK, false),
        ]);
        let ids: Vec<&str> = r.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "alsa:hw:CARD=2,DEV=0",
                "alsa:hw:CARD=U18dB,DEV=0",
                "alsa:hw:CARD=U18dB_1,DEV=0"
            ]
        );
    }

    #[test]
    fn sans_hw_la_carte_garde_plughw() {
        let r = une_ligne_par_carte_alsa(vec![
            entree("alsa:dsnoop:CARD=X,DEV=0", "X", false),
            entree("alsa:plughw:CARD=X,DEV=0", "X", false),
            entree("alsa:hw:CARD=Y,DEV=1", "Y", false),
            entree("alsa:plughw:CARD=Y,DEV=1", "Y", false),
            entree("alsa:hw:CARD=Y,DEV=0", "Y0", false),
        ]);
        let ids: Vec<&str> = r.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "alsa:plughw:CARD=X,DEV=0",
                "alsa:hw:CARD=Y,DEV=1",
                "alsa:hw:CARD=Y,DEV=0"
            ]
        );
    }

    /// Une carte sans hw: ni plughw: énuméré garde toutes ses lignes ; les
    /// entrées hors ALSA (CoreAudio, WASAPI) ne sont jamais touchées, même
    /// de même nom.
    #[test]
    fn hors_alsa_rien_ne_change() {
        let l = vec![
            entree("alsa:dsnoop:CARD=Z,DEV=0", "Z", false),
            entree("alsa:sysdefault:CARD=Z", "Z", false),
            entree("coreaudio:BuiltInMicrophoneDevice", "Micro MacBook", true),
            entree("coreaudio:AppleUSBAudioEngine:miniDSP:Umik-1", UMIK, false),
            entree("wasapi:{0.0.1.00000000}.{abc}", UMIK, false),
            entree("Umik-1", UMIK, false),
        ];
        assert_eq!(une_ligne_par_carte_alsa(l.clone()), l);
    }

    /// `/jouer` par le nom ouvre l'entrée que la liste réduite montre.
    #[test]
    fn l_ouverture_par_le_nom_prend_l_entree_de_la_liste() {
        let brut: Vec<String> = [
            "alsa:sysdefault:CARD=U18dB",
            "alsa:front:CARD=U18dB,DEV=0",
            "alsa:dsnoop:CARD=U18dB,DEV=0",
            "alsa:plughw:CARD=U18dB,DEV=0",
            "alsa:hw:CARD=U18dB,DEV=0",
            "alsa:hw:CARD=1,DEV=0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let i = choisir_parmi_homonymes(&brut).unwrap();
        assert_eq!(brut[i], "alsa:hw:CARD=U18dB,DEV=0");
        let gardee: Vec<String> =
            une_ligne_par_carte_alsa(brut.iter().map(|id| entree(id, UMIK, false)).collect())
                .into_iter()
                .map(|e| e.id)
                .collect();
        assert_eq!(
            gardee,
            vec![brut[i].clone()],
            "liste et ouverture divergent"
        );
        // Hors ALSA, ou sans accès matériel : la première, comme avant.
        let autres: Vec<String> = vec!["coreaudio:A".into(), "coreaudio:B".into()];
        assert_eq!(choisir_parmi_homonymes(&autres), Some(0));
        let sans_hw: Vec<String> = vec![
            "alsa:dsnoop:CARD=Z,DEV=0".into(),
            "alsa:sysdefault:CARD=Z".into(),
        ];
        assert_eq!(choisir_parmi_homonymes(&sans_hw), Some(0));
        assert_eq!(choisir_parmi_homonymes(&[]), None);
    }

    #[test]
    fn acces_et_entree_materielle() {
        assert_eq!(acces_alsa("alsa:dsnoop:CARD=U18dB,DEV=0"), Some("dsnoop"));
        assert_eq!(acces_alsa("alsa:pipewire"), Some("pipewire"));
        assert_eq!(acces_alsa("coreaudio:X"), None);
        assert_eq!(
            entree_materielle_alsa("alsa:sysdefault:CARD=U18dB").as_deref(),
            Some("hw:CARD=U18dB,DEV=0")
        );
        assert_eq!(entree_materielle_alsa("alsa:default"), None);
        assert_eq!(entree_materielle_alsa("wasapi:x"), None);
    }
}
