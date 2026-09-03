//! Canon des libellés d'instruments MusicBrainz (#2799 §4).
//!
//! ## Une seule table, deux lecteurs
//!
//! Le canon sert aux **deux bouts** de la même intention :
//!
//! - à l'ÉCRITURE des crédits (`tune-server`, `routes/library/credits_mb.rs`),
//!   qui range « grand piano » sous `piano` avant d'insérer dans
//!   `track_credits` ;
//! - à la LECTURE, dans le compilateur de règles `credit` des Smart
//!   Collections (`tune-smart-http`, `smart_collections.rs`), qui doit chercher
//!   le MÊME canon.
//!
//! Deux normalisations différentes et une collection `instrument: Grand Piano`
//! ne retrouverait plus les lignes écrites `piano` : le défaut est exactement
//! celui que la #2799 corrige. D'où **une** table, ici.
//!
//! ## Pourquoi `tune-core` et non `tune-http-types`
//!
//! Ce n'est pas un contrat HTTP — aucun statut, aucun corps, aucune route. Ce
//! sont des données de bibliothèque. `tune-core` est le point le plus bas que
//! `tune-server` et `tune-smart-http` voient tous les deux, et il porte déjà le
//! reste du domaine MusicBrainz.

/// Familles d'instruments, par MOT ENTIER. Table FIGÉE (§4 de l'issue).
///
/// 🔴 L'ORDRE COMPTE, et la comparaison se fait par mot entier, jamais par
/// sous-chaîne :
/// - `bass drum` est une percussion, pas une basse → `drum` passe AVANT `bass` ;
/// - `bassoon` contient `bass` mais n'est pas une basse → d'où le mot entier.
///
/// Un libellé qui ne matche aucune famille ressort NORMALISÉ (minuscules,
/// espaces réduits) mais intact : on ne perd pas un instrument rare, on le
/// laisse tel quel.
const FAMILLES_INSTRUMENTS: &[(&str, &str)] = &[
    // Percussions d'abord — « bass drum », « snare drum ».
    ("drums", "drums"),
    ("drum", "drums"),
    ("percussion", "percussion"),
    ("vibraphone", "vibraphone"),
    // Basses (« bass guitar », « double bass », « electric bass »).
    ("bass", "bass"),
    // Claviers. `grand piano`, `acoustic piano`, `electric piano`, `upright
    // piano` — l'exemple même de l'issue — se rejoignent ici.
    ("piano", "piano"),
    ("rhodes", "piano"),
    ("fortepiano", "piano"),
    ("organ", "organ"),
    ("harpsichord", "harpsichord"),
    ("synthesizer", "synthesizer"),
    ("synthesiser", "synthesizer"),
    ("synth", "synthesizer"),
    ("keyboards", "keyboard"),
    ("keyboard", "keyboard"),
    // Cordes pincées / frottées.
    ("guitar", "guitar"),
    ("banjo", "banjo"),
    ("mandolin", "mandolin"),
    ("harp", "harp"),
    ("violin", "violin"),
    ("viola", "viola"),
    ("cello", "cello"),
    ("violoncello", "cello"),
    // Vents.
    ("saxophone", "saxophone"),
    ("sax", "saxophone"),
    ("trumpet", "trumpet"),
    ("cornet", "trumpet"),
    ("trombone", "trombone"),
    ("flute", "flute"),
    ("clarinet", "clarinet"),
    ("oboe", "oboe"),
    ("bassoon", "bassoon"),
    ("harmonica", "harmonica"),
    ("accordion", "accordion"),
    // Voix.
    ("vocals", "vocals"),
    ("vocal", "vocals"),
    ("voice", "vocals"),
    ("singing", "vocals"),
];

/// Minuscules, ponctuation en espaces, espaces réduits.
///
/// Publique parce que `credits_mb::est_qualificatif` compare des attributs
/// MusicBrainz à la même règle : normaliser autrement de son côté ferait
/// passer un qualificatif pour un instrument.
pub fn normaliser(brut: &str) -> String {
    let mut out = String::with_capacity(brut.len());
    let mut espace_en_attente = false;
    for c in brut.chars() {
        if c.is_alphanumeric() {
            if espace_en_attente && !out.is_empty() {
                out.push(' ');
            }
            espace_en_attente = false;
            out.extend(c.to_lowercase());
        } else {
            espace_en_attente = true;
        }
    }
    out
}

/// Ramène un libellé d'instrument MusicBrainz à son canon (§4 de l'issue).
///
/// C'est la MÊME fonction qui sert à l'écriture des crédits et au compilateur
/// de règles `credit` des Smart Collections : deux normalisations différentes
/// des deux côtés, et une collection `instrument: piano` ne retrouverait pas
/// les lignes écrites `grand piano`.
pub fn canoniser_instrument(brut: &str) -> String {
    let n = normaliser(brut);
    if n.is_empty() {
        return n;
    }
    for (mot, canon) in FAMILLES_INSTRUMENTS {
        if n.split(' ').any(|m| m == *mot) {
            return (*canon).to_string();
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::canoniser_instrument;

    #[test]
    fn piano_canonise_toutes_ses_variantes() {
        for v in [
            "piano",
            "grand piano",
            "acoustic piano",
            "electric piano",
            "upright piano",
            "Grand Piano",
        ] {
            assert_eq!(canoniser_instrument(v), "piano", "variante {v}");
        }
    }

    /// Le piège de la table : par SOUS-CHAÎNE, `bassoon` deviendrait une basse
    /// et `bass drum` aussi.
    #[test]
    fn mot_entier_et_ordre_de_la_table() {
        assert_eq!(canoniser_instrument("bassoon"), "bassoon");
        assert_eq!(canoniser_instrument("bass drum"), "drums");
        assert_eq!(canoniser_instrument("bass guitar"), "bass");
        assert_eq!(canoniser_instrument("double bass"), "bass");
        assert_eq!(canoniser_instrument("acoustic guitar"), "guitar");
        assert_eq!(canoniser_instrument("tenor saxophone"), "saxophone");
        assert_eq!(canoniser_instrument("background vocals"), "vocals");
    }

    /// Un instrument rare n'est pas perdu : il ressort normalisé, pas vidé.
    #[test]
    fn instrument_inconnu_survit_normalise() {
        assert_eq!(
            canoniser_instrument("  Ondes   Martenot "),
            "ondes martenot"
        );
    }
}
