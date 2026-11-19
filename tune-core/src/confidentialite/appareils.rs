//! Les noms d'appareils, de zones et de profils dans ce qui sort de la machine
//! (#9140).
//!
//! Un appareil réseau annonce un nom choisi par son propriétaire : « Prénom’s
//! MacBook », « iPhone de Prénom », « Chambre de Prénom ». Le journal le recopie
//! tel quel (`<nom> volume=0.5`, `<nom> id=airplay-…`, `<nom> host=…`), et le
//! rapport de bogue part sur un forum PUBLIC.
//!
//! Deux passes, dans cet ordre :
//!
//! 1. les noms CONNUS — appareils et serveurs multimédia découverts depuis le
//!    démarrage (AirPlay, Chromecast, DLNA/UPnP, Sonos, OpenHome…), zones,
//!    profils, nom annoncé du serveur — retenus par [`Registre::retenir`], sont
//!    remplacés par un jeton stable, `[appareil 3]`, `[zone 2]`, `[profil 1]` :
//!    le même nom garde le même jeton dans tout le rapport, et d'un rapport à
//!    l'autre. Un nom qui n'est que le matériel (« Marantz NR1711 », « Sonos
//!    One ») ou un mot générique (« Main », « USB DAC », « Tune ») n'est pas
//!    retenu : c'est du diagnostic. Au-delà de [`NUMEROTES_MAX`] noms, un nom
//!    reste retenu et masqué, sous le jeton sans numéro (`[appareil]`) ;
//! 2. en filet de sécurité, les noms personnels reconnus à leur FORME, que la
//!    découverte n'a pas vus (appareil Bluetooth, téléphone de passage) :
//!    « X’s MacBook », « Jean Paul’s iPhone », « iPhone de X », « JBL Flip 5 de
//!    X », « MacBook-Pro-de-x », « Jean-Pierres-MacBook-Pro » (mDNS) —
//!    apostrophe droite, typographique, `&apos;`, `&#39;`, échappements DNS-SD
//!    (`\226\128\153`, `\032`). La forme exige un mot d'appareil : « Monkey’s
//!    Audio » ou « Kathy’s Waltz » restent. Ces noms-là ne sont PAS retenus
//!    dans la liste du serveur : leur jeton ne vaut que pour la séance de
//!    nettoyage en cours (un rapport), qui les numérote à la suite.
//!
//! Les jetons ne sont jamais repris par l'une ou l'autre passe : nettoyer deux
//! fois ne change rien.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, RwLock};

use regex::{Captures, Regex};

/// Ce que désigne un nom retenu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sorte {
    Appareil,
    Zone,
    Profil,
}

impl Sorte {
    fn mot(self) -> &'static str {
        match self {
            Sorte::Appareil => "appareil",
            Sorte::Zone => "zone",
            Sorte::Profil => "profil",
        }
    }

    fn indice(self) -> usize {
        match self {
            Sorte::Appareil => 0,
            Sorte::Zone => 1,
            Sorte::Profil => 2,
        }
    }
}

/// Au-delà, un nom retenu sort sous le jeton sans numéro (`[appareil]`) :
/// il reste masqué, seul le numéro manque.
pub const NUMEROTES_MAX: usize = 512;

/// Plafond dur de la liste : la découverte ne voit pas autant d'appareils sur
/// un réseau domestique ; au-delà, seul le filet par motifs protège encore.
const NOMS_MAX: usize = 8192;

/// Un nom plus court toucherait des mots ordinaires (« TV », « Hi »).
const NOM_MIN: usize = 3;

/// Une zone se nomme souvent d'un mot court et générique (« Bar », « Hi-Fi »).
const ZONE_MIN: usize = 4;

/// Noms qui ne désignent personne : les masquer effacerait du diagnostic.
const NOMS_GENERIQUES: &[&str] = &[
    "tune",
    "tune server",
    "tune-server",
    "airplay",
    "airplay 2",
    "chromecast",
    "sonos",
    "dlna",
    "upnp",
    "upnp renderer",
    "media renderer",
    "renderer",
    "openhome",
    "bluetooth",
    "local",
    "localhost",
    "default",
    "défaut",
    "par défaut",
    "system default",
    "main",
    "test",
    "studio",
    "usb",
    "usb dac",
    "usb audio",
    "dac",
    "hdmi",
    "spdif",
    "s/pdif",
    "toslink",
    "optical",
    "optique",
    "line out",
    "sortie analogique",
    "built-in output",
    "speaker",
    "speakers",
    "unknown",
    "inconnu",
    "output",
    "sortie",
    "audio",
    "music",
    "musique",
    "headphones",
    "casque",
    "tidal",
    "qobuz",
    "spotify",
    "spotify connect",
    "deezer",
    "roon",
    "plex",
    "youtube",
    "minimserver",
    "minidlna",
];

/// Mots qui, devant « ’s » ou après « de », ne sont pas un propriétaire.
const PROPRIETAIRES_EXCLUS: &[&str] = &[
    // anglais : contractions, marques, services
    "let", "it", "that", "what", "there", "here", "who", "where", "how", "he", "she", "tune",
    "apple", "google", "amazon", "sonos", "samsung", "device", "zone", "server", "player",
    "spotify", "tidal", "qobuz", "deezer", "youtube", "roon", "plex", "today", "user",
    // français : articles et possessifs
    "la", "le", "les", "un", "une", "mon", "ma", "mes", "ton", "ta", "tes", "sa", "son", "ses",
    "notre", "votre", "leur", "ce", "cet", "cette",
];

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif de confidentialité valide")
}

/// L'apostrophe sous toutes ses écritures : droite, typographique, entités
/// HTML, échappement DNS-SD des octets UTF-8 de `’` (`\226\128\153`, simple
/// ou doublé par un `Debug`), `\'`.
const APOSTROPHE: &str = r"(?:['’ʼ‘]|&apos;|&#0*39;|&#[xX]0*27;|&#0*8217;|&rsquo;|\\{1,2}226\\{1,2}128\\{1,2}153|\\{1,2}')";

/// Un blanc entre deux mots, `\032` (espace échappé par DNS-SD) compris.
const BLANC: &str = r"(?:[ \t]|\\{1,2}032)";

/// Séparateur de mots d'un nom d'appareil ou d'hôte.
const SEP: &str = r"(?:[ \t-]|\\{1,2}032)";

/// Appareils dont le nom suffit à reconnaître le motif, quelle que soit la
/// casse (les noms d'hôte mDNS passent souvent en minuscules).
const DISTINCTIFS: &str = r"(?i:macbook(?:(?:[ \t-]|\\{1,2}032)(?:pro|air))?|imac(?:(?:[ \t-]|\\{1,2}032)pro)?|iphone|ipad|ipod(?:(?:[ \t-]|\\{1,2}032)touch)?|homepod(?:(?:[ \t-]|\\{1,2}032)mini)?|airpods(?:(?:[ \t-]|\\{1,2}032)(?:pro|max))?|apple(?:[ \t-]|\\{1,2}032)tv|apple(?:[ \t-]|\\{1,2}032)watch|mac(?:[ \t-]|\\{1,2}032)(?:mini|studio|pro)|galaxy|chromebook|xperia|thinkpad)";

/// Mots d'appareil courants, reconnus seulement avec leur majuscule : « mac de
/// l'appareil » ou « portable de la cuisine » ne sont pas des noms.
const GENERIQUES: &str = r"(?:Mac|PC|TV|Echo|Pixel|Kindle|Surface|Nest|Sonos|Bose|JBL|Marshall|Beats|Xbox|PlayStation|Ordinateur|Portable|Téléphone|Telephone|Tablette|Enceinte|Laptop|Phone|Speaker|Watch)";

/// Ce qui suit le mot d'appareil dans « X’s <appareil> » : modèle (« S21 »,
/// « 15 », « Pro », « Dot »), jamais un mot quelconque.
const SUFFIXE: &str = r"(?:(?:[ \t-]|\\{1,2}032)(?:[0-9A-Za-z]*[0-9][0-9A-Za-z]*|Pro|Max|Plus|Ultra|Air|mini|Mini|Tab|Note|FE|Lite|Dot|Studio|Flip|Charge|Go|One|Move|Era|Roam|Beam|Arc))*";

/// Ce qui suit le mot d'appareil dans « <appareil> de X » : le « de » borne,
/// un mot à majuscule y est un modèle (« JBL Flip 5 de X »).
const SUFFIXE_DE: &str =
    r"(?:(?:[ \t-]|\\{1,2}032)(?:\p{Lu}\p{L}*|[0-9A-Za-z]*[0-9][0-9A-Za-z]*|mini))*";

/// Le propriétaire devant « ’s » : « Jean », « jean », « Jean-Pierre »,
/// « Jean Paul ».
fn proprietaire_possessif() -> String {
    format!(
        r"(?P<proprio>(?:\p{{Lu}}\p{{Ll}}+{BLANC})?[\p{{L}}][\p{{L}}\p{{M}}]*(?:-[\p{{L}}][\p{{L}}\p{{M}}]*)*)"
    )
}

/// « Jean’s MacBook Pro », « jean's iPhone 15 », « Jean Paul’s Echo Dot ».
static RE_POSSESSIF: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"{}{APOSTROPHE}s{SEP}+(?:{DISTINCTIFS}|{GENERIQUES})(?-u:\b){SUFFIXE}",
        proprietaire_possessif()
    ))
});

/// « iPhone de Jean », « Galaxy S21 de Jean Paul », « MacBook-Pro-de-jean »,
/// « JBL Flip 5 de Jean », « iPad d’Anne », « Mac von Jan ».
static RE_APPAREIL_DE: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"(?-u:\b)(?:(?P<distinctif>{DISTINCTIFS})|{GENERIQUES})(?-u:\b){SUFFIXE_DE}{SEP}+(?:(?:de|du|di|da|von|van){SEP}+|d{APOSTROPHE})(?P<proprio>[\p{{L}}][\p{{L}}\p{{M}}]+(?:-[\p{{L}}][\p{{L}}\p{{M}}]+)*(?:{BLANC}\p{{Lu}}\p{{Ll}}+)?)"
    ))
});

/// Nom d'hôte mDNS où macOS a retiré l'apostrophe : « Jeans-MacBook-Pro »,
/// « Jean-Pierres-MacBook-Pro ».
static RE_HOTE_MDNS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?P<proprio>[\p{L}][\p{L}\p{M}]+(?:-[\p{L}][\p{L}\p{M}]+)*?)s-(?i:macbook(?:-(?:pro|air))?|imac|iphone|ipad|mac-(?:mini|studio|pro)|homepod)(?-u:\b)(?:-[0-9]+)?",
    )
});

/// Les écritures d'apostrophe et d'espace que [`cle`] ramène à `'` et ` `.
static RE_ECHAPPEMENTS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"&apos;|&#0*39;|&#[xX]0*27;|&#0*8217;|&rsquo;|\\{1,2}226\\{1,2}128\\{1,2}153|\\{1,2}'|(\\{1,2}032)",
    )
});

/// Un jeton déjà posé : jamais repris.
const MOTIF_JETON: &str = r"\[(?:appareil|zone|profil)(?: [0-9]+)?\]";

/// La clé d'un nom : casse, apostrophes, échappements et blancs ramenés à une
/// seule forme, comme les compare le motif de la passe 1.
fn cle(nom: &str) -> String {
    let decode =
        RE_ECHAPPEMENTS.replace_all(
            nom.trim(),
            |c: &Captures| {
                if c.get(1).is_some() { " " } else { "'" }
            },
        );
    let mut sortie = String::with_capacity(decode.len());
    let mut blanc = false;
    for c in decode.trim().chars() {
        if c.is_whitespace() {
            if !blanc {
                sortie.push(' ');
            }
            blanc = true;
            continue;
        }
        blanc = false;
        if ['’', 'ʼ', '‘'].contains(&c) {
            sortie.push('\'');
        } else {
            sortie.extend(c.to_lowercase());
        }
    }
    sortie
}

/// Le motif d'un nom connu : apostrophes, échappements et blancs
/// indifférents. Les bornes de mot sont vérifiées à la main par
/// [`Seance::masquer_connus`] : une borne Unicode (`\b`) prive le moteur de son
/// automate rapide dès qu'un caractère non ASCII paraît (×30 sur un journal
/// accentué).
fn motif_du_nom(nom: &str) -> String {
    let nom = cle(nom);
    let mut m = String::new();
    for c in nom.chars() {
        match c {
            ' ' => m.push_str(r"(?:\s|\\{1,2}032)+"),
            '\'' => m.push_str(APOSTROPHE),
            _ => m.push_str(&regex::escape(&c.to_string())),
        }
    }
    m
}

fn lettre_ou_chiffre(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn mots(texte: &str) -> Vec<String> {
    cle(texte)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

/// Le nom n'est-il que le matériel ? Vrai si chacun de ses mots est un mot de
/// l'un des `materiels` (« fabricant modèle ») : « Marantz NR1711 »,
/// « NR1711 », « Sonos One » face à `Sonos` + `One`.
pub fn est_nom_de_materiel(nom: &str, materiels: &[String]) -> bool {
    let du_nom = mots(nom);
    !du_nom.is_empty()
        && materiels.iter().any(|m| {
            let du_materiel = mots(m);
            du_nom.iter().all(|x| du_materiel.contains(x))
        })
}

/// Les morceaux d'un nom qui ne sont pas le matériel : « MinimServer [x] »
/// face à `MinimWorld MinimServer` rend `["x"]`, « Chambre - Sonos Play:1 »
/// face à `Sonos Play:1` rend `["Chambre", ":1"]`. Le matériel reste lisible,
/// seul le reste est retenu — morceau par morceau, pour que chacun soit
/// retrouvé tel qu'il est écrit.
fn morceaux_hors_materiel(nom: &str, materiels: &[String]) -> Vec<String> {
    let mut mots_materiel: Vec<String> = materiels
        .iter()
        .flat_map(|m| {
            m.split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().count() >= 3)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    mots_materiel.sort();
    mots_materiel.dedup();
    let trim = |m: &str| {
        m.trim_matches(|c: char| c.is_whitespace() || "-–—:_|,;·()[]{}\"'«»".contains(c))
            .to_string()
    };
    if mots_materiel.is_empty() {
        return vec![trim(nom)];
    }
    let alternatives: Vec<String> = mots_materiel.iter().map(|w| regex::escape(w)).collect();
    let Ok(motif) = Regex::new(&format!(r"(?i)\b(?:{})\b", alternatives.join("|"))) else {
        return vec![trim(nom)];
    };
    motif
        .split(nom)
        .map(trim)
        .filter(|m| !m.is_empty())
        .collect()
}

/// Ce nom mérite-t-il un jeton ?
fn nom_a_masquer(sorte: Sorte, nom: &str, materiels: &[String]) -> bool {
    let nom = nom.trim();
    let min = if sorte == Sorte::Zone {
        ZONE_MIN
    } else {
        NOM_MIN
    };
    if nom.chars().count() < min || !nom.chars().any(char::is_alphabetic) {
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
    if c.starts_with('[') || c.starts_with('<') {
        return false;
    }
    !est_nom_de_materiel(nom, materiels)
}

fn proprietaire_exclu(proprio: &str) -> bool {
    let dernier = proprio.rsplit([' ', '-']).next().unwrap_or(proprio);
    PROPRIETAIRES_EXCLUS.contains(&dernier.to_lowercase().as_str())
        || PROPRIETAIRES_EXCLUS.contains(&proprio.to_lowercase().as_str())
}

/// Ce que lit une séance de nettoyage : figé, partagé sans copie.
#[derive(Default)]
struct Instantane {
    /// Clé ([`cle`]) → jeton.
    jetons: HashMap<String, String>,
    /// La passe 1.
    motif: Option<Regex>,
    /// Le dernier numéro donné, par sorte.
    compteurs: [usize; 3],
}

/// Les noms retenus et leurs jetons.
#[derive(Default)]
pub struct Registre {
    jetons: HashMap<String, String>,
    /// Les noms déjà examinés, retenus ou non (bornée à [`NOMS_MAX`]) : la
    /// réannonce d'un appareil ne refait pas le travail.
    vus: HashSet<String>,
    noms: Vec<String>,
    compteurs: [usize; 3],
    /// Reconstruit à la première séance après un nom retenu.
    instantane: Option<Arc<Instantane>>,
}

impl Registre {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ce nom est-il déjà retenu ? (Lecture seule : la découverte l'appelle à
    /// chaque réannonce.)
    pub fn connait(&self, nom: &str) -> bool {
        let c = cle(nom);
        self.jetons.contains_key(&c) || self.vus.contains(&c)
    }

    /// Retient un nom ; rend son jeton (celui de son premier morceau quand le
    /// nom mêle matériel et nom personnel), ou `None` s'il ne mérite pas
    /// d'être masqué (`materiels` : les « fabricant modèle » auxquels le
    /// comparer). Un nom déjà retenu garde son jeton.
    pub fn retenir(&mut self, sorte: Sorte, nom: &str, materiels: &[String]) -> Option<String> {
        let c = cle(nom);
        if let Some(j) = self.jetons.get(&c) {
            return Some(j.clone());
        }
        if self.vus.len() < NOMS_MAX {
            self.vus.insert(c);
        }
        if !nom_a_masquer(sorte, nom, materiels) {
            return None;
        }
        let mut premier = None;
        for morceau in morceaux_hors_materiel(nom, materiels) {
            if let Some(j) = self.retenir_un(sorte, &morceau) {
                premier.get_or_insert(j);
            }
        }
        premier
    }

    fn retenir_un(&mut self, sorte: Sorte, nom: &str) -> Option<String> {
        let c = cle(nom);
        if let Some(j) = self.jetons.get(&c) {
            return Some(j.clone());
        }
        if !nom_a_masquer(sorte, nom, &[]) || self.noms.len() >= NOMS_MAX {
            return None;
        }
        let i = sorte.indice();
        let j = if self.compteurs[i] < NUMEROTES_MAX {
            self.compteurs[i] += 1;
            format!("[{} {}]", sorte.mot(), self.compteurs[i])
        } else {
            format!("[{}]", sorte.mot())
        };
        self.jetons.insert(c, j.clone());
        self.noms.push(nom.trim().to_string());
        self.instantane = None;
        Some(j)
    }

    fn instantane(&mut self) -> Arc<Instantane> {
        if let Some(i) = &self.instantane {
            return i.clone();
        }
        let motif = (!self.noms.is_empty()).then(|| {
            // Les plus longs d'abord : « Salon TV » avant « Salon ».
            let mut noms: Vec<&String> = self.noms.iter().collect();
            noms.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
            let alternatives: Vec<String> = noms.iter().map(|n| motif_du_nom(n)).collect();
            // Le jeton en tête : à sa position, c'est lui qui l'emporte, et
            // aucun nom ne peut commencer au milieu.
            Regex::new(&format!("(?i){MOTIF_JETON}|{}", alternatives.join("|"))).ok()
        });
        let i = Arc::new(Instantane {
            jetons: self.jetons.clone(),
            motif: motif.flatten(),
            compteurs: self.compteurs,
        });
        self.instantane = Some(i.clone());
        i
    }

    /// Une séance de nettoyage : les noms retenus à cet instant, plus les noms
    /// que le filet trouvera, numérotés à la suite pour cette séance seule.
    pub fn seance(&mut self) -> Seance {
        let instantane = self.instantane();
        Seance {
            suivant: instantane.compteurs[Sorte::Appareil.indice()],
            instantane,
            locaux: HashMap::new(),
        }
    }
}

/// Un nettoyage — un rapport, un export : le même nom, le même jeton.
pub struct Seance {
    instantane: Arc<Instantane>,
    /// Noms trouvés par le filet pendant cette séance → jeton.
    locaux: HashMap<String, String>,
    suivant: usize,
}

impl Seance {
    /// Une séance sans aucun nom retenu (le filet seul).
    pub fn vide() -> Self {
        Registre::new().seance()
    }

    /// Passe 1 : les noms retenus.
    fn masquer_connus(&self, texte: &str) -> String {
        let Some(motif) = &self.instantane.motif else {
            return texte.to_string();
        };
        let mut sortie = String::with_capacity(texte.len());
        let mut dernier = 0;
        for m in motif.find_iter(texte) {
            let tout = m.as_str();
            if tout.starts_with('[') {
                continue;
            }
            // Bornes de mot : « Salon » ne touche pas « Salonnière ».
            let avant = texte[..m.start()].chars().next_back();
            let apres = texte[m.end()..].chars().next();
            let debut_mot = lettre_ou_chiffre(tout.chars().next());
            let fin_mot = lettre_ou_chiffre(tout.chars().next_back());
            if (debut_mot && lettre_ou_chiffre(avant)) || (fin_mot && lettre_ou_chiffre(apres)) {
                continue;
            }
            sortie.push_str(&texte[dernier..m.start()]);
            sortie.push_str(
                self.instantane
                    .jetons
                    .get(&cle(tout))
                    .map_or("[appareil]", String::as_str),
            );
            dernier = m.end();
        }
        sortie.push_str(&texte[dernier..]);
        sortie
    }

    fn jeton_local(&mut self, nom: &str) -> String {
        let c = cle(nom);
        if let Some(j) = self
            .instantane
            .jetons
            .get(&c)
            .or_else(|| self.locaux.get(&c))
        {
            return j.clone();
        }
        self.suivant += 1;
        let j = format!("[appareil {}]", self.suivant);
        self.locaux.insert(c, j.clone());
        j
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
            sortie.push_str(&self.jeton_local(nom));
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
        let t = self.masquer_motif(&t, &RE_POSSESSIF, exclu);
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

/// La liste du serveur : remplie par la découverte et les portes de sortie,
/// lue par [`super::anonymiser`].
static REGISTRE: LazyLock<RwLock<Registre>> = LazyLock::new(|| RwLock::new(Registre::new()));

/// Retient un nom dans la liste du serveur.
pub(super) fn retenir(sorte: Sorte, nom: &str, materiels: &[String]) {
    // La réannonce d'un appareil connu ne prend pas le verrou en écriture.
    let connu = match REGISTRE.read() {
        Ok(r) => r.connait(nom),
        Err(e) => e.into_inner().connait(nom),
    };
    if connu {
        return;
    }
    let mut r = REGISTRE.write().unwrap_or_else(|e| e.into_inner());
    r.retenir(sorte, nom, materiels);
}

/// Une séance sur la liste du serveur. Le verrou n'est tenu que le temps de
/// prendre l'instantané, jamais pendant le nettoyage.
pub(super) fn seance() -> Seance {
    REGISTRE.write().unwrap_or_else(|e| e.into_inner()).seance()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retenu(reg: &mut Registre, sorte: Sorte, nom: &str) -> Option<String> {
        reg.retenir(sorte, nom, &[])
    }

    #[test]
    fn nom_connu_jeton_stable_dans_tout_le_texte() {
        let mut reg = Registre::new();
        assert_eq!(
            retenu(&mut reg, Sorte::Appareil, "Alicia’s Salon Box").as_deref(),
            Some("[appareil 1]")
        );
        assert_eq!(
            retenu(&mut reg, Sorte::Appareil, "Bureau Naim").as_deref(),
            Some("[appareil 2]")
        );
        assert_eq!(
            retenu(&mut reg, Sorte::Zone, "Chambre de Zélie").as_deref(),
            Some("[zone 1]")
        );
        assert_eq!(
            retenu(&mut reg, Sorte::Profil, "Octavine").as_deref(),
            Some("[profil 1]")
        );
        let journal = "INFO airplay: Alicia’s Salon Box volume=0.5\n\
                       INFO airplay: Alicia's Salon Box id=airplay-a0b1c2d3e4f5\n\
                       INFO mdns: Alicia\\226\\128\\153s\\032Salon\\032Box._raop._tcp.local.\n\
                       INFO html: Alicia&apos;s Salon Box\n\
                       WARN dlna: bureau naim host=192.168.1.20 injoignable\n\
                       INFO zone Chambre de Zélie: lecture profil=Octavine\n\
                       INFO Alicia’s  Salon Box";
        let sortie = reg.seance().masquer(journal);
        for fuite in ["Alicia", "Naim", "naim", "Zélie", "Octavine"] {
            assert!(
                !sortie.contains(fuite),
                "« {fuite} » sort encore : {sortie}"
            );
        }
        assert_eq!(sortie.matches("[appareil 1]").count(), 5, "{sortie}");
        for garde in [
            "[appareil 1] volume=0.5",
            "[appareil 1] id=airplay-a0b1c2d3e4f5",
            "[appareil 1]._raop._tcp.local.",
            "[appareil 2] host=192.168.1.20 injoignable",
            "zone [zone 1]: lecture profil=[profil 1]",
        ] {
            assert!(sortie.contains(garde), "« {garde} » a disparu : {sortie}");
        }
        // Idempotent.
        assert_eq!(reg.seance().masquer(&sortie), sortie);
    }

    #[test]
    fn le_plus_long_l_emporte_et_les_bornes_de_mot_tiennent() {
        let mut reg = Registre::new();
        retenu(&mut reg, Sorte::Zone, "Salon");
        retenu(&mut reg, Sorte::Appareil, "Salon TV");
        let sortie = reg
            .seance()
            .masquer("Salon TV joue ; Salon attend ; Salonnière intacte");
        assert_eq!(
            sortie,
            "[appareil 1] joue ; [zone 1] attend ; Salonnière intacte"
        );
    }

    #[test]
    fn noms_generiques_courts_et_materiels_ignores() {
        let mut reg = Registre::new();
        for n in [
            "TV",
            "Tune",
            "AirPlay",
            "Zone 31",
            "  ",
            "1234",
            "[appareil 4]",
            "USB DAC",
        ] {
            assert_eq!(retenu(&mut reg, Sorte::Appareil, n), None, "{n}");
        }
        for n in ["Main", "Test", "Studio", "Bar", "Qobuz"] {
            assert_eq!(retenu(&mut reg, Sorte::Zone, n), None, "{n}");
        }
        let materiels = ["Marantz NR1711".to_string(), "Sonos One".to_string()];
        for n in ["Marantz NR1711", "NR1711", "nr1711 marantz", "Sonos One"] {
            assert_eq!(reg.retenir(Sorte::Appareil, n, &materiels), None, "{n}");
            assert_eq!(reg.retenir(Sorte::Zone, n, &materiels), None, "{n}");
        }
        // Un vrai nom à côté du matériel, lui, est retenu.
        assert!(
            reg.retenir(Sorte::Appareil, "Marantz de Wilfried", &materiels)
                .is_some()
        );
        let texte = "Tune AirPlay: Zone 31 TV ok, Main, Marantz NR1711 USB DAC";
        assert_eq!(reg.seance().masquer(texte), texte);
    }

    #[test]
    fn seul_le_morceau_personnel_d_un_nom_est_retenu() {
        let mut reg = Registre::new();
        let minim = ["MinimWorld MinimServer".to_string()];
        reg.retenir(Sorte::Appareil, "MinimServer [Rosalinde-NAS]", &minim);
        let sonos = ["Sonos Play:1".to_string()];
        reg.retenir(Sorte::Zone, "Mansarde de Philibert - Sonos Play:1", &sonos);
        let sortie = reg.seance().masquer(
            "- MinimServer [Rosalinde-NAS] — 192.168.1.20:9790\n\
             - Mansarde de Philibert - Sonos Play:1 (dlna)\n\
             INFO Mansarde de Philibert volume=0.3",
        );
        assert!(
            !sortie.contains("Rosalinde") && !sortie.contains("Philibert"),
            "{sortie}"
        );
        for garde in [
            "- MinimServer [[appareil 1]] — 192.168.1.20:9790",
            "- [zone 1] - Sonos Play:1 (dlna)",
            "INFO [zone 1] volume=0.3",
        ] {
            assert!(sortie.contains(garde), "« {garde} » a disparu : {sortie}");
        }
    }

    #[test]
    fn au_dela_du_plafond_le_nom_reste_masque() {
        let mut reg = Registre::new();
        for i in 0..NUMEROTES_MAX {
            reg.retenir(Sorte::Appareil, &format!("Enceinte numero {i}"), &[]);
        }
        assert_eq!(
            reg.retenir(Sorte::Appareil, "Grenier d’Ondine", &[])
                .as_deref(),
            Some("[appareil]")
        );
        let sortie = reg.seance().masquer("Grenier d’Ondine volume=0.4");
        assert_eq!(sortie, "[appareil] volume=0.4");
    }

    #[test]
    fn filet_formes_vues_dans_les_journaux() {
        let journal = "INFO raop: Alicia’s MacBook Pro volume=0.5\n\
                       INFO raop: Alicia’s MacBook Pro id=airplay-a0b1c2xxxxxx\n\
                       INFO raop: Alicia’s MacBook Pro host=[adresse]\n\
                       INFO raop: Alicia’s MacBook Pro\n\
                       INFO mdns: Bastiens-MacBook-Air.local. résolu\n\
                       INFO mdns: Jean-Hilaires-MacBook-Pro.local. résolu\n\
                       INFO cast: iPhone de Céleste connecté\n\
                       INFO cast: Galaxy S21 de Dorian-Paul volume=12\n\
                       INFO mdns: MacBook-Pro-de-eloise.local\n\
                       INFO bt: iPad d’Anouk appairé\n\
                       INFO bt: Fabrice's iPhone 15 192.168.1.30:7000\n\
                       INFO bt: Jean Gontran’s iPhone appairé\n\
                       INFO bt: JBL Flip 5 de Hortense appairé\n\
                       INFO html: Ignace&#39;s iPad\n\
                       INFO mdns: Kilian\\226\\128\\153s\\032iPhone._airplay._tcp.local.\n\
                       INFO sonos: Gaëlle’s Sonos joue";
        let mut s = Seance::vide();
        let sortie = s.masquer(journal);
        for fuite in [
            "Alicia", "Bastien", "Hilaire", "Céleste", "Dorian", "Paul", "eloise", "Anouk",
            "Fabrice", "Gontran", "Hortense", "Ignace", "Kilian", "Gaëlle",
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
            "._airplay._tcp.local.",
            " joue",
        ] {
            assert!(sortie.contains(garde), "« {garde} » a disparu : {sortie}");
        }
        assert_eq!(s.masquer(&sortie), sortie, "pas idempotent");
        // Le filet ne remplit pas la liste du serveur.
        let mut reg = Registre::new();
        let _ = reg.seance().masquer(journal);
        assert!(reg.noms.is_empty());
    }

    /// Contre-épreuve : des phrases de journal ordinaires ne bougent pas.
    #[test]
    fn contre_epreuve_texte_ordinaire_intact() {
        let mut s = Seance::vide();
        for texte in [
            "let's go: the zone's volume is 0.5",
            "Let’s Go",
            "adresse mac de l'appareil inconnue",
            "Portable de la cuisine",
            "PC de bureau",
            "Tune’s Server démarré",
            "Apple’s AirPlay 2 receiver",
            "Monkey's Audio decoder",
            "Kathy's Waltz (Dave Brubeck)",
            "TIDAL's API returned 429",
            "Spotify's Connect session",
            "MacBook Pro Speakers (CoreAudio) 44100 Hz",
            "iPhone 15 connecté via AirPlay 2",
            "Galaxy S21 volume=12",
            "Mac mini 192.168.1.4",
            "INFO tune_core::outputs::airplay: session ouverte en 12 ms",
        ] {
            assert_eq!(s.masquer(texte), texte, "modifié à tort");
        }
    }

    /// Mesure : 1 000 lignes non ASCII, 300 noms retenus.
    #[test]
    fn mille_lignes_non_ascii() {
        let mut reg = Registre::new();
        for i in 0..300 {
            reg.retenir(Sorte::Appareil, &format!("Pièce n°{i} de Zoéline"), &[]);
        }
        let ligne = "2026-11-19T10:00:00Z INFO tune_core::outputs::airplay: « Pièce n°42 de Zoéline » \
                     volume=0.5 — Ünïcödé ✓ Gaëlle’s iPhone host=192.168.1.20\n";
        let journal = ligne.repeat(1000);
        let debut = std::time::Instant::now();
        let sortie = reg.seance().masquer(&journal);
        let duree = debut.elapsed();
        eprintln!("1 000 lignes, 300 noms : {duree:?}");
        assert!(!sortie.contains("Zoéline") && !sortie.contains("Gaëlle"));
        // Large : la machine des épreuves est chargée, et c'est un binaire de
        // mise au point. Avant l'automate rapide : 33 s.
        assert!(duree < std::time::Duration::from_secs(8), "{duree:?}");
    }
}
