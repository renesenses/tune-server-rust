//! Journal JSON Lines (cahier § 3.2) : une ligne par essai.
//!
//! Aucune adresse IP, ni en clair ni hachée ; aucune position. On garde la
//! famille d'adresse. [`contient_une_adresse_ip`] est la garde : le client
//! refuse d'écrire une ligne qu'elle signale, et les messages d'erreur venus
//! d'iroh passent par [`caviarder`] avant d'entrer dans la ligne.

use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub use crate::protocole::InfoServeur;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Essai {
    pub v: u32,
    pub campagne: String,
    pub testeur: String,
    pub essai: String,
    pub debut: String,
    pub client: InfoClient,
    pub serveur: Option<InfoServeur>,
    pub relais: String,
    pub chemin_final: Option<String>,
    pub famille_direct: Option<String>,
    pub t_connecte_ms: Option<u64>,
    pub t_premier_octet_ms: Option<u64>,
    pub t_direct_ms: Option<u64>,
    pub debit_max_mbps: Option<f64>,
    pub flux: Vec<Flux>,
    pub rtt: Option<Rtt>,
    pub batterie: Option<Batterie>,
    pub erreur: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InfoClient {
    pub os: String,
    pub plateforme: String,
    pub reseau: String,
    pub operateur: String,
    pub partage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Flux {
    pub format: String,
    pub duree_s: u64,
    pub debit_moyen_mbps: f64,
    pub sous_debits: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Quantiles {
    pub p50: f64,
    pub p95: f64,
    /// Toujours rempli par le client ; absent de la ligne d'exemple du § 3.2
    /// côté relais, d'où la tolérance à la lecture.
    #[serde(default)]
    pub p99: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rtt {
    pub relais: Option<Quantiles>,
    pub direct: Option<Quantiles>,
    pub gigue_ms: Option<f64>,
    pub pertes_ping: f64,
}

/// Phase 2 seulement (mobile) ; toujours `null` en CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Batterie {
    pub batterie_debut: Option<f64>,
    pub batterie_fin: Option<f64>,
    pub duree_s: Option<u64>,
    pub ecran: Option<String>,
}

/// Un pseudonyme, un nom de campagne : `[a-z0-9_-]`, 1 à 32 caractères. Ils
/// entrent dans le nom du fichier ; on ferme la porte aux noms en clair, aux
/// espaces et aux `/`.
pub fn identifiant_valide(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// `p2p-mesure-<campagne>-<testeur>-<date>.jsonl` (cahier § 3.2).
pub fn nom_du_journal(campagne: &str, testeur: &str, date: &str) -> String {
    format!("p2p-mesure-{campagne}-{testeur}-{date}.jsonl")
}

/// Ajoute la ligne de l'essai au journal, après vérification qu'elle ne
/// contient aucune adresse IP.
pub fn ajouter(dossier: &Path, essai: &Essai) -> Result<PathBuf> {
    let date = essai.debut.get(..10).unwrap_or("0000-00-00");
    let chemin = dossier.join(nom_du_journal(&essai.campagne, &essai.testeur, date));
    let ligne = serde_json::to_string(essai)?;
    if let Some(trouvee) = contient_une_adresse_ip(&ligne) {
        bail!("ligne refusée : elle contient une adresse IP ({trouvee})");
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&chemin)?;
    writeln!(f, "{ligne}")?;
    Ok(chemin)
}

fn caractere_d_adresse(c: char) -> bool {
    c.is_ascii_hexdigit() || c == ':' || c == '.' || c == '%'
}

/// Les jetons d'un texte qui pourraient être une adresse : suites de
/// chiffres hexadécimaux, `:`, `.` et `%` (zone IPv6).
fn jetons(texte: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut debut = None;
    let mut sortie = Vec::new();
    for (i, c) in texte.char_indices() {
        match (caractere_d_adresse(c), debut) {
            (true, None) => debut = Some(i),
            (false, Some(d)) => {
                sortie.push((d, &texte[d..i]));
                debut = None;
            }
            _ => {}
        }
    }
    if let Some(d) = debut {
        sortie.push((d, &texte[d..]));
    }
    sortie.into_iter()
}

/// Adresses IP trouvées dans un jeton : le jeton entier en IPv6 (zone ôtée),
/// sinon chaque morceau entre `:` en IPv4 (forme `1.2.3.4:5678`).
fn adresses_du_jeton(jeton: &str) -> Vec<(usize, usize)> {
    let j = jeton.trim_matches('.');
    let decalage = jeton.find(j).unwrap_or(0);
    let sans_zone = j.split('%').next().unwrap_or(j);
    if sans_zone.contains(':') && sans_zone.parse::<Ipv6Addr>().is_ok() {
        return vec![(decalage, decalage + j.len())];
    }
    let mut trouves = Vec::new();
    let mut pos = 0;
    for morceau in j.split(':') {
        let m = morceau.trim_matches('.');
        if m.parse::<Ipv4Addr>().is_ok() {
            let d = decalage + pos + morceau.find(m).unwrap_or(0);
            trouves.push((d, d + m.len()));
        }
        pos += morceau.len() + 1;
    }
    trouves
}

/// La première adresse IPv4 ou IPv6 trouvée dans le texte, s'il y en a une.
pub fn contient_une_adresse_ip(texte: &str) -> Option<String> {
    jetons(texte).find_map(|(d, j)| {
        adresses_du_jeton(j)
            .first()
            .map(|(a, b)| texte[d + a..d + b].to_string())
    })
}

/// Remplace toute adresse IP du texte par `<adresse>`.
pub fn caviarder(texte: &str) -> String {
    let mut plages: Vec<(usize, usize)> = jetons(texte)
        .flat_map(|(d, j)| {
            adresses_du_jeton(j)
                .into_iter()
                .map(move |(a, b)| (d + a, d + b))
        })
        .collect();
    plages.sort();
    let mut sortie = String::with_capacity(texte.len());
    let mut curseur = 0;
    for (a, b) in plages {
        if a < curseur {
            continue;
        }
        sortie.push_str(&texte[curseur..a]);
        sortie.push_str("<adresse>");
        curseur = b;
    }
    sortie.push_str(&texte[curseur..]);
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_garde_trouve_les_adresses() {
        for t in [
            "x 127.0.0.1 y",
            "\"192.168.1.10:5000\"",
            "connexion à [::1]:4433 refusée",
            "fe80::1%en0",
            "2a01:cb00:1234::1",
            "::ffff:100.64.0.1",
            "relais 100.64.12.3.",
        ] {
            assert!(contient_une_adresse_ip(t).is_some(), "non vue : {t}");
        }
    }

    #[test]
    fn la_garde_ne_crie_pas_au_loup() {
        // Les valeurs légitimes d'une ligne d'exemple du § 3.2.
        for t in [
            "\"debut\":\"2026-11-03T18:42:10Z\"",
            "\"debit_max_mbps\":38.2",
            "\"format\":\"24/96\"",
            "\"famille\":\"ipv4+ipv6\"",
            "\"essai\":\"7f3a9c0e5b2d4a61\"",
            "\"pertes_ping\":0.002",
            "\"version\":\"1.3.0\"",
        ] {
            assert_eq!(contient_une_adresse_ip(t), None, "faux positif : {t}");
        }
    }

    #[test]
    fn caviarder_ote_les_adresses() {
        let t = caviarder("échec vers 192.168.1.10:5000 et [2a01:cb00::1]:443, fin");
        assert_eq!(t, "échec vers <adresse>:5000 et [<adresse>]:443, fin");
        assert_eq!(contient_une_adresse_ip(&t), None);
    }

    #[test]
    fn identifiants() {
        assert!(identifiant_valide("t03"));
        assert!(identifiant_valide("c1"));
        assert!(!identifiant_valide("Jean Dupont"));
        assert!(!identifiant_valide("../x"));
        assert!(!identifiant_valide(""));
    }
}
