//! Les noms d'appareils et de zones dans ce qui sort de la machine.
//!
//! Un appareil réseau annonce un nom choisi par son propriétaire : « Prénom’s
//! MacBook », « iPhone de Prénom », « Chambre de Prénom ». Le journal le recopie
//! tel quel (`<nom> volume=0.5`, `<nom> id=airplay-…`, `<nom> host=…`), et le
//! rapport de bogue part sur un forum PUBLIC.
//!
//! Deux passes, dans cet ordre :
//!
//! 1. les noms CONNUS — appareils découverts (AirPlay, Chromecast, DLNA/UPnP,
//!    Sonos, OpenHome…) et zones, retenus par la porte de sortie juste avant
//!    de nettoyer ([`NomsDAppareils::retenir`]) — sont remplacés par un jeton
//!    stable, `[appareil 3]` ou `[zone 2]` : le même nom garde le même jeton
//!    dans tout le rapport, le diagnostic reste lisible (« [appareil 3] a
//!    décroché ») sans dire à qui il appartient ;
//! 2. en filet de sécurité, les motifs de noms personnels que la découverte
//!    n'a pas vus (appareil Bluetooth, téléphone de passage, journal ancien) :
//!    « X’s MacBook », « iPhone de X », « Galaxy S21 de X », « MacBook-Pro-de-X »,
//!    « Xs-MacBook-Pro » (nom d'hôte mDNS), apostrophe droite ou typographique.
//!    Le nom trouvé est retenu à son tour : il reçoit lui aussi un jeton stable.
//!
//! Les jetons ne sont jamais repris par l'une ou l'autre passe : nettoyer deux
//! fois ne change rien.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use regex::{Captures, Regex};

/// Ce que désigne un nom retenu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sorte {
    Appareil,
    Zone,
}

impl Sorte {
    fn mot(self) -> &'static str {
        match self {
            Sorte::Appareil => "appareil",
            Sorte::Zone => "zone",
        }
    }
}

/// Au-delà, un nom n'est plus retenu (le jeton générique le remplace) : la
/// liste ne grandit pas sans fin sur un serveur qui tourne des mois.
const NOMS_MAX: usize = 512;

/// Un nom plus court toucherait des mots ordinaires (« TV », « Hi »).
const NOM_MIN: usize = 3;

/// Le jeton d'un nom trouvé par motif quand la liste est pleine.
const JETON_GENERIQUE: &str = "[appareil]";

/// Noms qui ne désignent personne : les masquer effacerait du diagnostic.
const NOMS_GENERIQUES: &[&str] = &[
    "tune",
    "tune server",
    "tune-server",
    "airplay",
    "chromecast",
    "sonos",
    "dlna",
    "upnp",
    "openhome",
    "bluetooth",
    "local",
    "localhost",
    "default",
    "défaut",
    "system default",
    "speaker",
    "speakers",
    "renderer",
    "media renderer",
    "unknown",
    "inconnu",
    "output",
    "sortie",
    "audio",
    "music",
    "musique",
    "headphones",
    "casque",
];

/// Mots qui, devant « ’s » ou après « de », ne sont pas un propriétaire.
const PROPRIETAIRES_EXCLUS: &[&str] = &[
    // anglais : contractions et marques
    "let", "it", "that", "what", "there", "here", "who", "where", "how", "he", "she", "tune",
    "apple", "google", "amazon", "sonos", "samsung", "device", "zone", "server", "player",
    // français : articles et possessifs
    "la", "le", "les", "un", "une", "mon", "ma", "mes", "ton", "ta", "tes", "sa", "son", "ses",
    "notre", "votre", "leur", "ce", "cet", "cette",
];

const APOSTROPHES: &[char] = &['\'', '’', 'ʼ', '‘'];

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif de confidentialité valide")
}

/// Appareils dont le nom suffit à reconnaître le motif, quelle que soit la
/// casse (les noms d'hôte mDNS passent souvent en minuscules).
const DISTINCTIFS: &str = r"(?i:macbook(?:[ -](?:pro|air))?|imac(?:[ -]pro)?|iphone|ipad|ipod(?:[ -]touch)?|homepod(?:[ -]mini)?|airpods(?:[ -](?:pro|max))?|apple[ -]tv|apple[ -]watch|mac[ -](?:mini|studio|pro)|galaxy|chromebook|xperia|thinkpad)";

/// Mots d'appareil courants, reconnus seulement avec leur majuscule : « mac de
/// l'appareil » ou « portable de la cuisine » ne sont pas des noms.
const GENERIQUES: &str = r"(?:Mac|PC|TV|Echo|Pixel|Kindle|Surface|Ordinateur|Portable|Téléphone|Telephone|Tablette|Enceinte|Laptop|Phone|Speaker|Watch)";

/// Ce qui suit le mot d'appareil : modèle (« S21 », « 15 », « Pro », « Dot »).
const SUFFIXE: &str = r"(?:[ -](?:[0-9A-Za-z]*[0-9][0-9A-Za-z]*|Pro|Max|Plus|Ultra|Air|mini|Mini|Tab|Note|FE|Lite|Dot|Studio))*";

/// Le propriétaire, toute casse : « Jean », « jean », « Jean-Pierre ».
const PROPRIETAIRE: &str = r"(?P<proprio>[\p{L}][\p{L}\p{M}]+(?:-[\p{L}][\p{L}\p{M}]+)?)";

/// Le propriétaire avec sa majuscule.
const PROPRIETAIRE_MAJ: &str = r"(?P<proprio>\p{Lu}[\p{L}\p{M}]*(?:-\p{Lu}[\p{L}\p{M}]*)?)";

const APOSTROPHE: &str = "['’ʼ‘]";

/// « Jean’s MacBook Pro », « jean's iPhone 15 », « Léa’s Echo Dot ».
static RE_POSSESSIF_APPAREIL: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"\b{PROPRIETAIRE}{APOSTROPHE}s[ \t-]+(?:{DISTINCTIFS}|{GENERIQUES})\b{SUFFIXE}"
    ))
});

/// « Jean’s Living Room », « Léa’s Bose » : un possessif suivi d'un nom propre.
static RE_POSSESSIF_NOM: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"\b{PROPRIETAIRE_MAJ}{APOSTROPHE}s[ \t]+(?:\p{{Lu}}|i\p{{Lu}}|\p{{N}})[\p{{L}}\p{{N}}]*(?:[ -](?:\p{{Lu}}|\p{{N}})[\p{{L}}\p{{N}}]*){{0,3}}"
    ))
});

/// « iPhone de Jean », « Galaxy S21 de Jean », « MacBook-Pro-de-jean »,
/// « Ordinateur de Jean », « iPad d’Anne », « Mac von Jan ».
static RE_APPAREIL_DE: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"\b(?:(?P<distinctif>{DISTINCTIFS})|{GENERIQUES})\b{SUFFIXE}[ \t-]+(?:(?:de|du|di|da|von|van)[ \t-]+|d{APOSTROPHE}){PROPRIETAIRE}"
    ))
});

/// Nom d'hôte mDNS où macOS a retiré l'apostrophe : « Jeans-MacBook-Pro ».
static RE_HOTE_MDNS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b(?P<proprio>[\p{L}][\p{L}\p{M}]+)s-(?i:macbook(?:-(?:pro|air))?|imac|iphone|ipad|mac-(?:mini|studio|pro)|homepod)\b(?:-[0-9]+)?",
    )
});

/// Un jeton déjà posé : jamais repris.
const MOTIF_JETON: &str = r"\[(?:appareil|zone)(?: [0-9]+)?\]";

/// La clé d'un nom : casse, apostrophes et blancs ramenés à une seule forme,
/// comme les compare le motif de la passe 1.
fn cle(nom: &str) -> String {
    let mut sortie = String::with_capacity(nom.len());
    let mut blanc = false;
    for c in nom.trim().chars() {
        if c.is_whitespace() {
            if !blanc {
                sortie.push(' ');
            }
            blanc = true;
            continue;
        }
        blanc = false;
        if APOSTROPHES.contains(&c) {
            sortie.push('\'');
        } else {
            sortie.extend(c.to_lowercase());
        }
    }
    sortie
}

/// Le motif d'un nom connu : apostrophes et blancs indifférents, bornes de
/// mot là où le nom commence ou finit par une lettre ou un chiffre.
fn motif_du_nom(nom: &str) -> String {
    let nom = nom.trim();
    let mut m = String::new();
    if nom.chars().next().is_some_and(char::is_alphanumeric) {
        m.push_str(r"\b");
    }
    let mut blanc = false;
    for c in nom.chars() {
        if c.is_whitespace() {
            if !blanc {
                m.push_str(r"\s+");
            }
            blanc = true;
            continue;
        }
        blanc = false;
        if APOSTROPHES.contains(&c) {
            m.push_str(APOSTROPHE);
        } else {
            m.push_str(&regex::escape(&c.to_string()));
        }
    }
    if nom.chars().last().is_some_and(char::is_alphanumeric) {
        m.push_str(r"\b");
    }
    m
}

/// Ce nom mérite-t-il un jeton ?
fn nom_a_masquer(nom: &str) -> bool {
    let nom = nom.trim();
    if nom.chars().count() < NOM_MIN || !nom.chars().any(char::is_alphabetic) {
        return false;
    }
    let c = cle(nom);
    if NOMS_GENERIQUES.contains(&c.as_str()) {
        return false;
    }
    // Le nom par défaut d'une zone (« Zone 31 ») ne dit rien de personne.
    if let Some(reste) = c.strip_prefix("zone ")
        && reste.chars().all(|ch| ch.is_ascii_digit())
    {
        return false;
    }
    // Un jeton, un masque d'une autre passe : rien à protéger.
    !(c.starts_with('[') || c.starts_with('<'))
}

fn proprietaire_exclu(proprio: &str) -> bool {
    PROPRIETAIRES_EXCLUS.contains(&proprio.to_lowercase().as_str())
}

/// Les noms retenus et leurs jetons.
#[derive(Default)]
pub struct NomsDAppareils {
    /// Clé ([`cle`]) → jeton.
    jetons: HashMap<String, String>,
    /// Les noms bruts, dans l'ordre où ils ont été retenus.
    noms: Vec<String>,
    appareils: usize,
    zones: usize,
    /// La passe 1, reconstruite à chaque nom retenu.
    motif: Option<Regex>,
}

impl NomsDAppareils {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retient un nom ; rend son jeton, ou `None` s'il ne mérite pas d'être
    /// masqué. Un nom déjà retenu garde son jeton, quelle que soit la sorte
    /// demandée la seconde fois.
    pub fn retenir(&mut self, sorte: Sorte, nom: &str) -> Option<String> {
        if !nom_a_masquer(nom) {
            return None;
        }
        Some(self.jeton(sorte, nom))
    }

    fn jeton(&mut self, sorte: Sorte, nom: &str) -> String {
        let c = cle(nom);
        if let Some(j) = self.jetons.get(&c) {
            return j.clone();
        }
        if self.noms.len() >= NOMS_MAX {
            return format!("[{}]", sorte.mot());
        }
        let n = match sorte {
            Sorte::Appareil => {
                self.appareils += 1;
                self.appareils
            }
            Sorte::Zone => {
                self.zones += 1;
                self.zones
            }
        };
        let j = format!("[{} {n}]", sorte.mot());
        self.jetons.insert(c, j.clone());
        self.noms.push(nom.trim().to_string());
        self.motif = None;
        j
    }

    fn motif(&mut self) -> Option<&Regex> {
        if self.motif.is_none() && !self.noms.is_empty() {
            // Les plus longs d'abord : « Salon TV » avant « Salon ».
            let mut noms: Vec<&String> = self.noms.iter().collect();
            noms.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
            let alternatives: Vec<String> = noms.iter().map(|n| motif_du_nom(n)).collect();
            // Le jeton en tête : à sa position, c'est lui qui l'emporte, et
            // aucun nom ne peut commencer au milieu.
            let motif = format!("(?i){MOTIF_JETON}|{}", alternatives.join("|"));
            self.motif = Regex::new(&motif).ok();
        }
        self.motif.as_ref()
    }

    /// Passe 1 : les noms retenus.
    fn masquer_connus(&mut self, texte: &str) -> String {
        let jetons = self.jetons.clone();
        let Some(motif) = self.motif() else {
            return texte.to_string();
        };
        motif
            .replace_all(texte, |c: &Captures| {
                let tout = &c[0];
                if tout.starts_with('[') {
                    return tout.to_string();
                }
                jetons
                    .get(&cle(tout))
                    .cloned()
                    .unwrap_or_else(|| JETON_GENERIQUE.to_string())
            })
            .into_owned()
    }

    /// Passe 2 : un motif, son jeton. Le dernier mot d'un modèle (« 192 »)
    /// collé à un `.` ou `:` est le début d'une adresse, pas du nom : il reste.
    fn masquer_motif(
        &mut self,
        texte: &str,
        motif: &Regex,
        garder: impl Fn(&Captures) -> bool,
    ) -> String {
        let mut sortie = String::with_capacity(texte.len());
        let mut dernier = 0;
        for c in motif.captures_iter(texte) {
            let m = c.get(0).expect("correspondance");
            sortie.push_str(&texte[dernier..m.start()]);
            dernier = m.end();
            if garder(&c) {
                sortie.push_str(m.as_str());
                continue;
            }
            let mut nom = m.as_str();
            let mut reste = "";
            if texte[m.end()..].starts_with(['.', ':'])
                && let Some(coupe) = nom.rfind([' ', '-'])
                && nom[coupe + 1..].chars().any(|ch| ch.is_ascii_digit())
            {
                reste = &nom[coupe..];
                nom = &nom[..coupe];
            }
            sortie.push_str(&self.jeton(Sorte::Appareil, nom));
            sortie.push_str(reste);
        }
        sortie.push_str(&texte[dernier..]);
        sortie
    }

    /// Les deux passes.
    pub fn masquer(&mut self, texte: &str) -> String {
        let t = self.masquer_connus(texte);
        let exclu = |c: &Captures| {
            c.name("proprio")
                .is_some_and(|p| proprietaire_exclu(p.as_str()))
        };
        let t = self.masquer_motif(&t, &RE_POSSESSIF_APPAREIL, exclu);
        let t = self.masquer_motif(&t, &RE_POSSESSIF_NOM, exclu);
        let t = self.masquer_motif(&t, &RE_APPAREIL_DE, |c: &Captures| {
            let Some(p) = c.name("proprio").map(|p| p.as_str()) else {
                return true;
            };
            // Un mot d'appareil courant ne compte qu'avec un propriétaire
            // écrit comme un prénom : « Portable de la cuisine » reste.
            let generique = c.name("distinctif").is_none();
            proprietaire_exclu(p) || (generique && !p.starts_with(char::is_uppercase))
        });
        self.masquer_motif(&t, &RE_HOTE_MDNS, exclu)
    }
}

/// La liste du serveur : remplie par les portes de sortie, lue par
/// [`super::anonymiser`].
pub(super) static NOMS: LazyLock<RwLock<NomsDAppareils>> =
    LazyLock::new(|| RwLock::new(NomsDAppareils::new()));

#[cfg(test)]
mod tests {
    use super::*;

    fn passe(reg: &mut NomsDAppareils, t: &str) -> String {
        reg.masquer(t)
    }

    #[test]
    fn nom_connu_jeton_stable_dans_tout_le_texte() {
        let mut reg = NomsDAppareils::new();
        assert_eq!(
            reg.retenir(Sorte::Appareil, "Alicia’s MacBook").as_deref(),
            Some("[appareil 1]")
        );
        assert_eq!(
            reg.retenir(Sorte::Appareil, "Bureau Naim").as_deref(),
            Some("[appareil 2]")
        );
        assert_eq!(
            reg.retenir(Sorte::Zone, "Chambre de Zélie").as_deref(),
            Some("[zone 1]")
        );
        let journal = "INFO airplay: Alicia’s MacBook volume=0.5\n\
                       INFO airplay: Alicia's MacBook id=airplay-a0b1c2d3e4f5\n\
                       WARN dlna: bureau naim host=192.168.1.20 injoignable\n\
                       INFO zone Chambre de Zélie: lecture\n\
                       INFO Alicia’s  MacBook";
        let sortie = passe(&mut reg, journal);
        for fuite in ["Alicia", "Naim", "naim", "Zélie"] {
            assert!(
                !sortie.contains(fuite),
                "« {fuite} » sort encore : {sortie}"
            );
        }
        assert_eq!(sortie.matches("[appareil 1]").count(), 3, "{sortie}");
        assert!(sortie.contains("[appareil 1] volume=0.5"), "{sortie}");
        assert!(
            sortie.contains("[appareil 1] id=airplay-a0b1c2d3e4f5"),
            "{sortie}"
        );
        assert!(
            sortie.contains("[appareil 2] host=192.168.1.20 injoignable"),
            "{sortie}"
        );
        assert!(sortie.contains("zone [zone 1]: lecture"), "{sortie}");
        // Idempotent.
        assert_eq!(passe(&mut reg, &sortie), sortie);
    }

    #[test]
    fn le_plus_long_l_emporte_et_les_bornes_de_mot_tiennent() {
        let mut reg = NomsDAppareils::new();
        reg.retenir(Sorte::Zone, "Salon");
        reg.retenir(Sorte::Appareil, "Salon TV");
        let sortie = passe(
            &mut reg,
            "Salon TV joue ; Salon attend ; Salonnière intacte",
        );
        assert_eq!(
            sortie,
            "[appareil 1] joue ; [zone 1] attend ; Salonnière intacte"
        );
    }

    #[test]
    fn noms_generiques_et_courts_ignores() {
        let mut reg = NomsDAppareils::new();
        for n in [
            "TV",
            "Tune",
            "AirPlay",
            "Zone 31",
            "  ",
            "1234",
            "[appareil 4]",
        ] {
            assert_eq!(reg.retenir(Sorte::Appareil, n), None, "{n}");
        }
        let texte = "Tune AirPlay: Zone 31 TV ok";
        assert_eq!(passe(&mut reg, texte), texte);
    }

    #[test]
    fn filet_formes_vues_dans_les_journaux() {
        let mut reg = NomsDAppareils::new();
        let journal = "INFO raop: Alicia’s MacBook Pro volume=0.5\n\
                       INFO raop: Alicia’s MacBook Pro id=airplay-a0b1c2xxxxxx\n\
                       INFO raop: Alicia’s MacBook Pro host=[adresse]\n\
                       INFO raop: Alicia’s MacBook Pro\n\
                       INFO mdns: Bastiens-MacBook-Air.local. résolu\n\
                       INFO cast: iPhone de Céleste connecté\n\
                       INFO cast: Galaxy S21 de Dorian-Paul volume=12\n\
                       INFO mdns: MacBook-Pro-de-eloise.local\n\
                       INFO bt: iPad d’Anouk appairé\n\
                       INFO bt: Fabrice's iPhone 15 192.168.1.30:7000\n\
                       INFO sonos: Gaëlle’s Living Room joue";
        let sortie = passe(&mut reg, journal);
        for fuite in [
            "Alicia", "Bastien", "Céleste", "Dorian", "Paul", "eloise", "Anouk", "Fabrice",
            "Gaëlle",
        ] {
            assert!(
                !sortie.contains(fuite),
                "« {fuite} » sort encore : {sortie}"
            );
        }
        // Le même appareil, le même jeton, sur les quatre formes.
        assert_eq!(sortie.matches("[appareil 1]").count(), 4, "{sortie}");
        for garde in [
            "[appareil 1] volume=0.5",
            "[appareil 1] id=airplay-a0b1c2xxxxxx",
            "[appareil 1] host=[adresse]",
            ".local. résolu",
            " connecté",
            " volume=12",
            " appairé",
            "192.168.1.30:7000",
            " joue",
        ] {
            assert!(sortie.contains(garde), "« {garde} » a disparu : {sortie}");
        }
        assert_eq!(passe(&mut reg, &sortie), sortie, "pas idempotent");
    }

    /// Contre-épreuve : des phrases de journal ordinaires ne bougent pas.
    #[test]
    fn contre_epreuve_texte_ordinaire_intact() {
        let mut reg = NomsDAppareils::new();
        for texte in [
            "let's go: the zone's volume is 0.5",
            "Let’s Go",
            "adresse mac de l'appareil inconnue",
            "Portable de la cuisine",
            "PC de bureau",
            "Tune’s Server démarré",
            "Apple’s AirPlay 2 receiver",
            "MacBook Pro Speakers (CoreAudio) 44100 Hz",
            "iPhone 15 connecté via AirPlay 2",
            "Galaxy S21 volume=12",
            "Mac mini 192.168.1.4",
            "INFO tune_core::outputs::airplay: session ouverte en 12 ms",
        ] {
            assert_eq!(passe(&mut reg, texte), texte, "modifié à tort");
        }
    }
}
