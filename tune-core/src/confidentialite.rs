//! Nettoyage des données personnelles avant qu'un rapport ou un journal quitte
//! la machine (#5124).
//!
//! Le rapport de bogue part sur un forum PUBLIC, le diagnostic d'un ticket de
//! support est republié par le miroir, et un journal exporté finit collé dans un
//! fil. Tous reprennent tels quels des textes que Tune n'a pas écrits : le nom
//! qu'annonce un serveur UPnP/DLNA (qui peut contenir une adresse électronique
//! ou un nom de machine), les chemins des répertoires musicaux, les réponses
//! des services de streaming écrites dans le journal.
//!
//! Ce module est la SEULE fonction de nettoyage. Les trois sorties l'appellent :
//! le rapport de bogue (`generate_bug_report`, donc aussi le fil de forum et le
//! markdown joint aux tickets), l'export des journaux (`/system/logs`) et le
//! relais des tickets de support (`cloud::support`, champs `system` et `logs`).
//!
//! Ce qui est masqué :
//! - les adresses électroniques, en clair ou encodées (`%40`) → `<email>` ;
//! - les identifiants d'une URL (`scheme://user:pass@hôte`) ;
//! - les chemins personnels `/Users/<nom>`, `/home/<nom>`, `C:\Users\<nom>` → `~` ;
//! - les valeurs des clés secrètes (`token`, `secret`, `password`, `sig`,
//!   `hmac`, `serial`, `username`…) écrites `clé=valeur`, `clé: valeur` ou en
//!   JSON, les en-têtes `Bearer`/`Basic`, les JWT → [`MASQUE`] ;
//! - la partie propre à l'appareil des adresses MAC : le préfixe constructeur
//!   (OUI) reste, c'est lui qui identifie le matériel ;
//! - le nom de l'utilisateur local du système, là où il apparaît en toutes
//!   lettres (nom de machine « MacBook-de-<nom> », nom de serveur…).
//!
//! Ce qui reste, parce que c'est le diagnostic : versions, OS, modèles, type de
//! serveur, adresses IP du réseau local, ports, compteurs.
//!
//! La fonction est idempotente : nettoyer deux fois ne change rien, ce qui
//! permet de l'appliquer à chaque porte de sortie sans se soucier de l'ordre.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

pub use crate::secrets::MASQUE;

/// Ce qui remplace une adresse électronique.
pub const MASQUE_EMAIL: &str = "<email>";

/// Ce qui remplace le nom de l'utilisateur local.
pub const MASQUE_UTILISATEUR: &str = "<utilisateur>";

/// Noms de clés personnels qui ne sont pas des secrets au sens de
/// [`crate::secrets::est_secret`] (ils ne le sont pas pour `/system/config`,
/// où l'écran les lit), mais qui ne doivent pas quitter la machine.
const FRAGMENTS_PERSONNELS: &[&str] = &[
    "username",
    "user_name",
    "email",
    "e-mail",
    "serial",
    "cookie",
    "authorization",
    "hmac",
    "signature",
];

/// Noms exacts, trop courts pour être cherchés comme fragments.
const NOMS_PERSONNELS: &[&str] = &[
    "sig",
    "request_sig",
    "login",
    "mail",
    "pwd",
    "user",
    "identity",
];

/// Cette clé porte-t-elle une valeur qui ne doit pas sortir de la machine ?
pub fn est_personnel(cle: &str) -> bool {
    if crate::secrets::est_secret(cle) {
        return true;
    }
    let cle = cle.to_ascii_lowercase();
    NOMS_PERSONNELS.contains(&cle.as_str()) || FRAGMENTS_PERSONNELS.iter().any(|f| cle.contains(f))
}

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif de confidentialité valide")
}

/// `scheme://utilisateur:motdepasse@` — avant l'adresse électronique, qui
/// sinon prendrait `motdepasse@hôte.fr` pour une adresse.
static RE_URL_IDENTIFIANTS: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@"));

/// JSON Web Token : trois segments base64url dont les deux premiers
/// commencent par `eyJ` (`{"`).
static RE_JWT: LazyLock<Regex> =
    LazyLock::new(|| re(r"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]*"));

/// `Authorization: Bearer <jeton>` et `Basic <b64>`.
/// Seul un mot qui a l'air d'un jeton (un chiffre, un `=` de remplissage, ou
/// 20 caractères et plus) est masqué : « Bearer tokens are… » reste lisible.
static RE_PORTEUR: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(bearer|basic)\s+([A-Za-z0-9._~+/=-]{6,})"));

/// Paramètres d'URL sensibles, quel que soit leur nom de clé ailleurs.
static RE_PARAMETRE_URL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)([?&](?:code|state|key|token|sig|hmac|auth|access_token|refresh_token|user_auth_token|app_secret|request_sig|password|session|identity)=)[^&\s#]+",
    )
});

/// `clé=valeur`, `clé: valeur`, `"clé":"valeur"`, `clé=Some("valeur")`. La
/// clé est jugée par [`est_personnel`] dans la fermeture : une seule liste.
static RE_CLE_VALEUR: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)\b([a-z][a-z0-9_.-]*)(["']?[ \t]*[:=][ \t]*(?:Some\([ \t]*)?["']?)([^\s"',;&(){}\[\]<>]+)"#,
    )
});

/// Adresse électronique, `@` en clair ou encodé.
static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)[a-z0-9._+-]+(?:@|%40)[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,}\b"));

/// `C:\Users\<nom>` (séparateurs simples, doublés par un `Debug`, ou `/`).
/// Un nom Windows peut contenir des espaces : il n'en prend que s'il est suivi
/// d'un séparateur, et jamais de virgule, pour ne pas manger la suite d'une
/// liste.
static RE_CHEMIN_WINDOWS: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?i)\b[a-z]:[\\/]+users[\\/]+(?:([^\\/\r\n"'<>|:*?,;]+)([\\/])|([^\\/\s"'<>|:*?,;]+))"#)
});

/// `/Users/<nom>`, `/home/<nom>`, `/var/home/<nom>` (Silverblue).
static RE_CHEMIN_UNIX: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:/var)?/(?:home|Users)/([^/\s"'<>,;:)]+)"#));

/// Adresse MAC, `:` ou `-` (deux motifs : `regex` n'a pas de référence
/// arrière pour exiger le même séparateur partout).
static RE_MAC_DEUX_POINTS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b([0-9A-Fa-f]{2}):([0-9A-Fa-f]{2}):([0-9A-Fa-f]{2}):[0-9A-Fa-f]{2}:[0-9A-Fa-f]{2}:[0-9A-Fa-f]{2}\b",
    )
});
static RE_MAC_TIRETS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b([0-9A-Fa-f]{2})-([0-9A-Fa-f]{2})-([0-9A-Fa-f]{2})-[0-9A-Fa-f]{2}-[0-9A-Fa-f]{2}-[0-9A-Fa-f]{2}\b",
    )
});

/// Noms de compte génériques : les masquer effacerait des mots du diagnostic
/// (« tune », « music », « fedora ») sans rien protéger.
const COMPTES_GENERIQUES: &[&str] = &[
    "root",
    "admin",
    "administrator",
    "administrateur",
    "user",
    "users",
    "guest",
    "default",
    "public",
    "shared",
    "system",
    "nobody",
    "daemon",
    "tune",
    "music",
    "audio",
    "media",
    "pi",
    "ubuntu",
    "debian",
    "fedora",
    "docker",
    "runner",
    "www-data",
];

/// Le nom de l'utilisateur local, s'il vaut la peine d'être masqué.
fn utilisateur_local() -> Option<&'static str> {
    static NOM: LazyLock<Option<String>> = LazyLock::new(|| {
        let depuis_env = ["USER", "USERNAME", "LOGNAME"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()));
        let depuis_home = || {
            ["HOME", "USERPROFILE"].iter().find_map(|v| {
                let chemin = std::env::var(v).ok()?;
                let nom = chemin
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()?;
                Some(nom.to_string())
            })
        };
        depuis_env
            .or_else(depuis_home)
            .filter(|n| utilisateur_a_masquer(n))
    });
    NOM.as_deref()
}

fn utilisateur_a_masquer(nom: &str) -> bool {
    let nom = nom.trim();
    nom.chars().count() >= 3 && !COMPTES_GENERIQUES.contains(&nom.to_lowercase().as_str())
}

/// Nettoie un texte avant qu'il quitte la machine.
///
/// C'est LA fonction que toute sortie appelle. Le nom de l'utilisateur local
/// est lu dans l'environnement du processus.
pub fn anonymiser(texte: &str) -> String {
    anonymiser_avec(texte, utilisateur_local())
}

/// [`anonymiser`], le nom d'utilisateur local donné explicitement — pour les
/// épreuves, qui ne doivent pas dépendre du compte qui les exécute.
pub fn anonymiser_avec(texte: &str, utilisateur: Option<&str>) -> String {
    let t = RE_URL_IDENTIFIANTS.replace_all(texte, format!("${{1}}{MASQUE}@"));
    let t = RE_JWT.replace_all(&t, MASQUE);
    let t = RE_PORTEUR.replace_all(&t, |c: &Captures| {
        let jeton = &c[2];
        if jeton.len() >= 20 || jeton.chars().any(|ch| ch.is_ascii_digit() || ch == '=') {
            format!("{} {MASQUE}", &c[1])
        } else {
            c[0].to_string()
        }
    });
    let t = RE_PARAMETRE_URL.replace_all(&t, format!("${{1}}{MASQUE}"));
    let t = RE_CLE_VALEUR.replace_all(&t, |c: &Captures| {
        if est_personnel(&c[1]) {
            format!("{}{}{MASQUE}", &c[1], &c[2])
        } else {
            c[0].to_string()
        }
    });
    let t = RE_EMAIL.replace_all(&t, MASQUE_EMAIL);
    let t = RE_CHEMIN_WINDOWS.replace_all(&t, |c: &Captures| {
        // Le séparateur qui suivait le nom est gardé : `~\Music`.
        format!("~{}", c.get(2).map_or("", |m| m.as_str()))
    });
    let t = RE_CHEMIN_UNIX.replace_all(&t, |c: &Captures| {
        // `/Users/Shared` n'est le dossier de personne.
        if c[1].eq_ignore_ascii_case("shared") {
            c[0].to_string()
        } else {
            "~".to_string()
        }
    });
    let t = RE_MAC_DEUX_POINTS.replace_all(&t, "$1:$2:$3:xx:xx:xx");
    let t = RE_MAC_TIRETS.replace_all(&t, "$1-$2-$3-xx-xx-xx");
    let mut t = t.into_owned();
    if let Some(nom) = utilisateur
        .map(str::trim)
        .filter(|n| utilisateur_a_masquer(n))
    {
        let motif = format!(r"(?i)\b{}\b", regex::escape(nom));
        if let Ok(re_nom) = Regex::new(&motif) {
            t = re_nom.replace_all(&t, MASQUE_UTILISATEUR).into_owned();
        }
    }
    t
}

/// Nettoie, en place, un document JSON avant qu'il quitte la machine.
///
/// Toute chaîne passe par [`anonymiser`] ; toute valeur scalaire rangée sous
/// une clé personnelle ([`est_personnel`]) est remplacée par [`MASQUE`], quel
/// que soit son contenu — `"username": "jdupont"` n'a rien qu'une expression
/// reconnaisse. Les booléens et `null` restent : « authentifié, oui/non »
/// est du diagnostic, et ne dit rien de personne.
pub fn anonymiser_json(valeur: &mut Value) {
    anonymiser_json_avec(valeur, utilisateur_local());
}

/// [`anonymiser_json`], le nom d'utilisateur local donné explicitement.
pub fn anonymiser_json_avec(valeur: &mut Value, utilisateur: Option<&str>) {
    match valeur {
        Value::String(s) => *s = anonymiser_avec(s, utilisateur),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|v| anonymiser_json_avec(v, utilisateur)),
        Value::Object(carte) => {
            for (cle, v) in carte.iter_mut() {
                if est_personnel(cle) && matches!(v, Value::String(_) | Value::Number(_)) {
                    *v = Value::String(MASQUE.to_string());
                } else {
                    anonymiser_json_avec(v, utilisateur);
                }
            }
        }
        _ => {}
    }
}

/// Un champ texte qui porte peut-être du JSON (la fiche système d'un ticket
/// multipart arrive sérialisée) : nettoyé comme JSON s'il en est, comme texte
/// sinon — dans les deux cas par la même fonction.
pub fn anonymiser_champ(texte: &str) -> String {
    match serde_json::from_str::<Value>(texte) {
        Ok(mut v) if v.is_object() || v.is_array() => {
            anonymiser_json(&mut v);
            v.to_string()
        }
        _ => anonymiser(texte),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nettoie(t: &str) -> String {
        anonymiser_avec(t, None)
    }

    #[test]
    fn adresse_electronique_dans_un_nom_de_serveur() {
        let sortie =
            nettoie("  - MinimServer [jean.dupont@exemple.fr] — 192.168.1.20:9790 — joignable");
        assert!(!sortie.contains("jean.dupont"), "{sortie}");
        assert!(!sortie.contains("exemple.fr"), "{sortie}");
        assert!(
            sortie.contains("MinimServer"),
            "le type de serveur doit rester : {sortie}"
        );
        assert!(
            sortie.contains("192.168.1.20:9790"),
            "l'adresse LAN doit rester : {sortie}"
        );
        assert!(sortie.contains(MASQUE_EMAIL), "{sortie}");
    }

    #[test]
    fn adresse_encodee_dans_une_url() {
        let sortie = nettoie("GET /search?q=jean.dupont%40exemple.fr");
        assert!(!sortie.contains("jean.dupont"), "{sortie}");
    }

    #[test]
    fn chemins_personnels_ramenes_a_tilde() {
        assert_eq!(nettoie("/Users/jdupont/Music/a.flac"), "~/Music/a.flac");
        assert_eq!(nettoie("/home/jdupont/Musique"), "~/Musique");
        assert_eq!(nettoie("/var/home/jdupont/x"), "~/x");
        assert_eq!(
            nettoie(r"C:\Users\Jean Pierre\Music\a.flac"),
            r"~\Music\a.flac"
        );
        assert_eq!(nettoie(r"C:\\Users\\jdupont\\Music"), r"~\\Music");
        assert_eq!(nettoie("D:/Users/jdupont/Music"), "~/Music");
        assert_eq!(
            nettoie("Music dirs: /home/jdupont/Music, /mnt/nas/Musique"),
            "Music dirs: ~/Music, /mnt/nas/Musique"
        );
        assert_eq!(nettoie("/Users/Shared/Music"), "/Users/Shared/Music");
        assert_eq!(nettoie("/var/log/tune.log"), "/var/log/tune.log");
    }

    #[test]
    fn jetons_et_mots_de_passe() {
        let journal = concat!(
            r#"INFO tidal_token_exchange_success body={"access_token":"eyJhbGciOiJ.eyJzdWIiOjE.c2lnbmF0dXJl","refresh_token":"rt-Zx81","user":{"email":"a@b.fr"}}"#,
            "\n",
            "INFO qobuz_get_file_url track_id=42 sig=0f1e2d3c4b5a",
            "\n",
            "WARN http GET https://api.qobuz.com/file?user_auth_token=UAT999&format_id=27",
            "\n",
            "Authorization: Bearer abcdef0123456789",
            "\n",
            "deezer_authenticated_token username=Some(\"jdupont\")",
            "\n",
            "smb://jdupont:motdepasse@nas.local/Musique",
            "\n",
            "Cookie: identity=7%09BANDCAMP",
        );
        let sortie = nettoie(journal);
        for fuite in [
            "eyJhbGciOiJ",
            "rt-Zx81",
            "0f1e2d3c4b5a",
            "UAT999",
            "abcdef0123456789",
            "jdupont",
            "motdepasse",
            "BANDCAMP",
            "a@b.fr",
        ] {
            assert!(
                !sortie.contains(fuite),
                "« {fuite} » sort encore :\n{sortie}"
            );
        }
        // Le diagnostic reste.
        for garde in [
            "tidal_token_exchange_success",
            "track_id=42",
            "format_id=27",
            "nas.local",
        ] {
            assert!(sortie.contains(garde), "« {garde} » a disparu :\n{sortie}");
        }
    }

    #[test]
    fn mac_garde_le_constructeur() {
        assert_eq!(nettoie("mac=00:1A:2B:3C:4D:5E"), "mac=00:1A:2B:xx:xx:xx");
        assert_eq!(nettoie("00-1a-2b-3c-4d-5e"), "00-1a-2b-xx-xx-xx");
        // Une heure n'est pas une MAC.
        assert_eq!(nettoie("12:34:56"), "12:34:56");
    }

    #[test]
    fn nom_d_utilisateur_local() {
        let sortie = anonymiser_avec("Serveur Plex (MacBook-Pro-de-Jdupont)", Some("jdupont"));
        assert_eq!(sortie, "Serveur Plex (MacBook-Pro-de-<utilisateur>)");
        // Un compte générique n'efface pas un mot du diagnostic.
        assert_eq!(anonymiser_avec("Tune music", Some("tune")), "Tune music");
    }

    #[test]
    fn diagnostic_intact() {
        let texte = "**Version**: 0.9.165 (engine: rust)\n**Platform**: linux (x86_64)\n\
                     - Tracks: 1200\n- Serveurs multimedia: 1\n  - Asset UPnP — 192.168.1.5:26125 — joignable\n\
                     - enrich_on_scan: true\n- qobuz: enabled, authenticated";
        assert_eq!(nettoie(texte), texte);
    }

    #[test]
    fn idempotent() {
        let une = nettoie("x@y.fr /home/jdupont token=abc 00:11:22:33:44:55");
        assert_eq!(nettoie(&une), une);
    }

    #[test]
    fn json_cles_personnelles_et_chaines() {
        let mut fiche = json!({
            "server": { "version": "0.9.165", "os": "macos" },
            "library": { "music_dirs": ["/Users/jdupont/Music"] },
            "streaming_services": [
                { "name": "qobuz", "authenticated": true, "username": "jdupont", "subscription": "Studio" }
            ],
            "network": { "media_servers": [
                { "name": "jean.dupont@exemple.fr: MinimServer", "host": "192.168.1.20", "port": 9790 }
            ]},
            "settings": { "license_key": "TUNE-1234", "enrich_on_scan": true },
            "serial_number": 123456,
        });
        anonymiser_json_avec(&mut fiche, None);
        let texte = fiche.to_string();
        for fuite in [
            "jdupont",
            "jean.dupont",
            "exemple.fr",
            "TUNE-1234",
            "123456",
        ] {
            assert!(!texte.contains(fuite), "« {fuite} » sort encore : {texte}");
        }
        assert_eq!(fiche["server"]["version"], "0.9.165");
        assert_eq!(fiche["library"]["music_dirs"][0], "~/Music");
        assert_eq!(fiche["streaming_services"][0]["authenticated"], true);
        assert_eq!(fiche["streaming_services"][0]["subscription"], "Studio");
        assert_eq!(fiche["network"]["media_servers"][0]["host"], "192.168.1.20");
        assert_eq!(fiche["network"]["media_servers"][0]["port"], 9790);
        assert_eq!(fiche["settings"]["enrich_on_scan"], true);
    }

    #[test]
    fn champ_json_serialise() {
        let sortie = anonymiser_champ(r#"{"media_servers":[{"name":"x@y.fr"}],"username":"z"}"#);
        assert!(
            !sortie.contains("x@y.fr") && !sortie.contains("\"z\""),
            "{sortie}"
        );
        assert_eq!(anonymiser_champ("hors JSON a@b.fr"), "hors JSON <email>");
    }
}
