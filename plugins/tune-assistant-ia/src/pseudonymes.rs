//! La pseudonymisation STRICTE de ce qui part chez un fournisseur tiers
//! (décision de Bertrand du 23/10/2026, étude #6834 § 1.4).
//!
//! `tune_core::confidentialite::anonymiser` a été écrit pour le forum : il
//! masque les jetons, les clés, le nom d'utilisateur et les chemins
//! personnels, mais il GARDE volontairement les adresses IP du réseau local
//! et les noms que les appareils annoncent — ils servent au diagnostic sur le
//! forum. Pour un fournisseur tiers, cette passe-ci s'ajoute (elle tourne
//! APRÈS `anonymiser`) et remplace :
//!
//! - les noms CONNUS — zones, appareils, hôtes, titres, artistes, albums —
//!   que l'hôte fournit ([`Pseudonymiseur::noms`]), par `<zone-1>`,
//!   `<appareil-2>`… ;
//! - les adresses IPv4 et IPv6 par `<ip-1>`… (le même pseudonyme pour la
//!   même adresse : le modèle voit que deux lignes parlent du même appareil) ;
//! - les adresses MAC (`<mac-1>`), les UUID et UDN (`<id-1>`), les courriels
//!   (`<courriel>`), les noms d'hôtes en `.local`/`.lan`/`.home` (`<hote-1>`) ;
//! - les chemins de fichiers par `<chemin>` suivi de leur seule extension ;
//! - les valeurs des paramètres d'URL (`?token=<masqué>`) et toute longue
//!   suite alphanumérique (`<jeton>`) — une signature au nom inhabituel ;
//! - les valeurs des champs de journal qui portent un titre (`title=`,
//!   `artist=`, `album=`, `track=`, `name=`…) par `<texte>`.
//!
//! La table de correspondance reste SUR LE SERVEUR : [`Pseudonymiseur::retablir`]
//! rend les vrais noms dans la réponse montrée à l'écran. Rien n'est
//! « garanti » : une donnée que la passe ne connaît pas (un nom de réseau
//! Wi-Fi au milieu d'une phrase) peut passer — d'où l'aperçu montré avant
//! CHAQUE envoi.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

/// Les catégories de noms connus, et le préfixe de leur pseudonyme.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("zone", "zone"),
    ("appareil", "appareil"),
    ("hote", "hote"),
    ("piste", "piste"),
    ("artiste", "artiste"),
    ("album", "album"),
];

/// Un nom plus court n'est pas remplacé à l'aveugle (« TV », « Hi ») : il
/// toucherait des mots ordinaires. Il reste couvert par les autres passes.
pub const NOM_MIN: usize = 3;

#[derive(Clone, Default)]
pub struct Pseudonymiseur {
    /// Vrai nom (minuscules) → pseudonyme.
    connus: Vec<(String, String)>,
    /// Pseudonyme → vrai texte, pour rétablir.
    table: BTreeMap<String, String>,
    compteurs: BTreeMap<&'static str, usize>,
    /// Valeur repérée (IP, MAC…) → pseudonyme.
    vus: BTreeMap<String, String>,
}

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif valide")
}

macro_rules! motif {
    ($nom:ident, $m:expr) => {
        fn $nom() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| re($m))
        }
    };
}

motif!(
    re_courriel,
    r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"
);
motif!(
    re_uuid,
    r"(?i)\b(?:uuid:)?[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"
);
motif!(re_mac, r"(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b");
motif!(re_ipv4, r"\b(?:\d{1,3}\.){3}\d{1,3}\b");
// IPv6 : la forme pleine (8 groupes) ou compressée (avec `::`). Une heure
// `11:48:38` n'en est pas une : ni huit groupes, ni `::`.
motif!(
    re_ipv6,
    r"(?i)\b(?:(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}|(?:[0-9a-f]{1,4}:)*[0-9a-f]{1,4}::(?:[0-9a-f]{1,4}:)*[0-9a-f]{1,4}|::(?:[0-9a-f]{1,4}:)*[0-9a-f]{1,4})(?:%[0-9a-z]+)?\b"
);
motif!(
    re_hote,
    r"(?i)\b[a-z0-9][a-z0-9-]*(?:\.[a-z0-9-]+)*\.(?:local|lan|home|internal|localdomain|fritz\.box)\b"
);
// Un chemin : une racine connue, des dossiers (espaces permis, chaque dossier
// finit par un séparateur), puis un dernier élément sans espace. Masquer trop
// vaut mieux que pas assez.
motif!(
    re_chemin,
    r#"(?:~/(?:[^/\n"'<>|=]*/)*|/(?:home|Users|mnt|media|srv|var|volume\d*|Volumes|data|music|Music|share|nas|opt|tmp|root)\b(?:[^/\n"'<>|=]*/)*|[A-Za-z]:\\(?:[^\\\n"'<>|=]*\\)*|\\\\(?:[^\\\n"'<>|=]*\\)*)[^\s"'<>|,;)\]]*"#
);
motif!(re_param_url, r"([?&][A-Za-z0-9_.-]+=)[^&\s\x22']+");
motif!(
    re_champ_titre,
    r#"(?i)\b(title|artist|album|track|song|name|album_title|artist_name|track_title|zone_name|device_name|friendly_name)(\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s,;]+)"#
);
motif!(re_jeton, r"\b[A-Za-z0-9_\-]{32,}\b");

impl Pseudonymiseur {
    pub fn new() -> Self {
        Self::default()
    }

    fn suivant(&mut self, prefixe: &'static str) -> String {
        let n = self.compteurs.entry(prefixe).or_insert(0);
        *n += 1;
        format!("<{prefixe}-{n}>")
    }

    /// Déclare des noms connus d'une catégorie ([`CATEGORIES`]).
    pub fn noms(&mut self, categorie: &str, noms: &[String]) {
        let Some((_, prefixe)) = CATEGORIES.iter().find(|(c, _)| *c == categorie) else {
            return;
        };
        for nom in noms {
            let nom = nom.trim();
            if nom.chars().count() < NOM_MIN {
                continue;
            }
            let bas = nom.to_lowercase();
            if self.connus.iter().any(|(n, _)| *n == bas) {
                continue;
            }
            let p = self.suivant(prefixe);
            self.table.insert(p.clone(), nom.to_string());
            self.connus.push((bas, p));
        }
        // Les plus longs d'abord : « Salon TV » avant « Salon ».
        self.connus
            .sort_by_key(|x| std::cmp::Reverse(x.0.chars().count()));
    }

    fn remplacer_motif(&mut self, texte: &str, motif: &Regex, prefixe: &'static str) -> String {
        let mut sortie = String::with_capacity(texte.len());
        let mut dernier = 0;
        for m in motif.find_iter(texte) {
            sortie.push_str(&texte[dernier..m.start()]);
            let valeur = m.as_str().to_string();
            let p = match self.vus.get(&valeur) {
                Some(p) => p.clone(),
                None => {
                    let p = self.suivant(prefixe);
                    self.table.insert(p.clone(), valeur.clone());
                    self.vus.insert(valeur, p.clone());
                    p
                }
            };
            sortie.push_str(&p);
            dernier = m.end();
        }
        sortie.push_str(&texte[dernier..]);
        sortie
    }

    /// Remplace les noms connus, sans tenir compte de la casse.
    fn remplacer_noms(&self, texte: &str) -> String {
        let mut t = texte.to_string();
        for (bas, p) in &self.connus {
            let motif = Regex::new(&format!("(?i){}", regex::escape(bas)));
            if let Ok(m) = motif {
                t = m.replace_all(&t, regex::NoExpand(p)).into_owned();
            }
        }
        t
    }

    /// La passe stricte, sur un texte déjà passé par `anonymiser`.
    pub fn passer(&mut self, texte: &str) -> String {
        // Les chemins d'abord : un nom connu au milieu d'un chemin ne doit pas
        // le couper en deux (le reste du chemin partirait).
        let t = re_courriel().replace_all(texte, "<courriel>").into_owned();
        let t = re_chemin()
            .replace_all(&t, |c: &regex::Captures| {
                let brut = &c[0];
                let nom = brut.rsplit(['/', '\\']).next().unwrap_or("");
                match nom.rsplit_once('.') {
                    Some((_, ext))
                        if !ext.is_empty()
                            && ext.len() <= 5
                            && ext.chars().all(|x| x.is_ascii_alphanumeric()) =>
                    {
                        format!("<chemin>.{ext}")
                    }
                    _ => "<chemin>".to_string(),
                }
            })
            .into_owned();
        let t = self.remplacer_noms(&t);
        let t = re_champ_titre()
            .replace_all(&t, |c: &regex::Captures| {
                let v = &c[3];
                // Une valeur déjà pseudonymisée reste lisible : `<zone-1>`.
                if v.trim_matches(|x| x == '"' || x == '\'').starts_with('<') {
                    c[0].to_string()
                } else {
                    format!("{}{}<texte>", &c[1], &c[2])
                }
            })
            .into_owned();
        let t = re_param_url().replace_all(&t, "${1}<masqué>").into_owned();
        let t = self.remplacer_motif(&t, re_uuid(), "id");
        let t = self.remplacer_motif(&t, re_mac(), "mac");
        let t = self.remplacer_motif(&t, re_ipv4(), "ip");
        let t = self.remplacer_motif(&t, re_ipv6(), "ip");
        let t = self.remplacer_motif(&t, re_hote(), "hote");
        // Une longue suite qui mêle lettres ET chiffres : un jeton. Un nom
        // d'évènement du journal (`ssdp_startup_zone_hidden_skipping`) n'en
        // est pas un, et il sert d'indice au diagnostic.
        re_jeton()
            .replace_all(&t, |c: &regex::Captures| {
                let m = &c[0];
                if m.chars().any(|x| x.is_ascii_digit())
                    && m.chars().any(|x| x.is_ascii_alphabetic())
                {
                    "<jeton>".to_string()
                } else {
                    m.to_string()
                }
            })
            .into_owned()
    }

    /// Les vrais noms, rétablis dans une réponse du modèle, pour l'écran.
    pub fn retablir(&self, texte: &str) -> String {
        let mut t = texte.to_string();
        // Les plus longs pseudonymes d'abord : `<ip-12>` avant `<ip-1>`.
        let mut cles: Vec<&String> = self.table.keys().collect();
        cles.sort_by_key(|k| std::cmp::Reverse(k.len()));
        for k in cles {
            t = t.replace(k.as_str(), &self.table[k]);
        }
        t
    }

    /// Combien de remplacements la table connaît (pour l'écran).
    pub fn taille(&self) -> usize {
        self.table.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Pseudonymiseur {
        let mut p = Pseudonymiseur::new();
        p.noms("zone", &["Salon".into(), "Chambre de Léa".into()]);
        p.noms("appareil", &["Naim Uniti Atom".into()]);
        p.noms("piste", &["So What".into()]);
        p
    }

    #[test]
    fn aucune_ip_ni_nom_connu_ne_reste() {
        let mut p = p();
        let ligne = "2026-10-23 WARN dlna: Naim Uniti Atom (192.168.1.42) ne répond pas pour la zone Salon \
                     — fe80::1c2b:3aff:fe4d:5e6f%eth0, mac 00:1A:2B:3C:4D:5E, uuid:4d696e69-444c-164e-9d41-b827eb54e2a1, \
                     hôte nas-maison.local, piste « So What », chambre de léa";
        let s = p.passer(ligne);
        for interdit in [
            "192.168",
            "Naim",
            "Salon",
            "fe80",
            "00:1A",
            "4d696e69",
            "nas-maison",
            "So What",
            "léa",
        ] {
            assert!(
                !s.contains(interdit),
                "« {interdit} » part chez le fournisseur : {s}"
            );
        }
        assert!(s.contains("<ip-1>"), "{s}");
        // Rétablir rend les vrais noms à l'écran.
        let r = p.retablir(&s);
        assert!(
            r.contains("192.168.1.42") && r.contains("Naim Uniti Atom"),
            "{r}"
        );
    }

    #[test]
    fn une_heure_n_est_pas_une_adresse() {
        let mut p = Pseudonymiseur::new();
        assert_eq!(
            p.passer("2026-10-23T11:48:38Z WARN zone stopped"),
            "2026-10-23T11:48:38Z WARN zone stopped"
        );
    }

    #[test]
    fn la_meme_adresse_garde_le_meme_pseudonyme() {
        let mut p = Pseudonymiseur::new();
        let s = p.passer("de 10.0.0.5 vers 10.0.0.9 puis 10.0.0.5");
        assert_eq!(s, "de <ip-1> vers <ip-2> puis <ip-1>");
    }

    #[test]
    fn chemins_titres_et_jetons_sont_masques() {
        let mut p = Pseudonymiseur::new();
        let s = p.passer(
            "open /home/bertrand/Musique/Miles Davis/Kind of Blue/01.flac failed; \
             title=\"Blue in Green\" artist=Coltrane url=https://x.example/s?sig=abc123&token=zzz \
             jeton ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd",
        );
        for interdit in [
            "bertrand",
            "Musique",
            "Miles",
            "Kind of Blue",
            "Blue in Green",
            "Coltrane",
            "abc123",
            "zzz",
            "ABCDEFGHIJ",
        ] {
            assert!(!s.contains(interdit), "« {interdit} » part : {s}");
        }
        assert!(s.contains("<chemin>.flac"), "{s}");
        let w = p.passer(r"C:\Users\Léa\Ma musique\a.flac introuvable");
        assert!(!w.contains("Léa") && !w.contains("musique"), "{w}");
    }

    #[test]
    fn un_evenement_du_journal_n_est_pas_un_jeton() {
        let mut p = Pseudonymiseur::new();
        assert_eq!(
            p.passer("ERROR ssdp_startup_zone_hidden_skipping"),
            "ERROR ssdp_startup_zone_hidden_skipping"
        );
    }

    #[test]
    fn un_nom_trop_court_n_est_pas_remplace_partout() {
        let mut p = Pseudonymiseur::new();
        p.noms("zone", &["TV".into()]);
        assert_eq!(p.passer("TV et TVA"), "TV et TVA");
    }
}
