//! #6287 — un langage de requête TEXTUEL pour les règles intelligentes.
//!
//! Décision de Bertrand (13/10), idée reprise d'Aède : « pistes où X joue du
//! piano », « favoris jamais écoutés », plages de valeurs, combinaisons
//! booléennes. Une syntaxe optionnelle pour les avancés, **traduite vers les
//! mêmes règles que l'éditeur visuel** : chaque critère d'une requête devient
//! une règle `{field, op, value}` que `build_smart_query_rapport` traduit
//! comme il traduit celles de l'éditeur. Ce module n'écrit AUCUN SQL de
//! critère : il ne fait que lire le texte, et combiner les conditions que le
//! moteur des playlists lui rend.
//!
//! # Grammaire
//!
//! ```text
//! requete   := ou
//! ou        := et ( OU et )*                    OU  : « ou », « or », « | »
//! et        := unaire ( [ET] unaire )*          ET  : « et », « and », « & », ou rien
//! unaire    := NON unaire | atome               NON : « non », « not », « ! »
//! atome     := "(" ou ")" | credit | motcle | critere
//! credit    := ("credit" | "crédit") "(" cle sep valeur ("," cle sep valeur)* ")"
//!              cle : artiste | role | instrument
//! motcle    := "favori" | "jamais_ecoute"
//! critere   := champ op valeur
//! op        := ":" | "=" | "!=" | ">" | ">=" | "<" | "<="
//! valeur    := mot | "chaîne entre guillemets" | bas..haut
//! ```
//!
//! Deux termes juxtaposés sont joints par ET, et ET lie plus fort que OU :
//! `a b ou c` se lit `(a ET b) OU c`.
//!
//! # Exemples
//!
//! ```text
//! credit(artiste: "Bill Evans", instrument: piano)
//! favori jamais_ecoute
//! genre:jazz annee:1955..1965 non credit(role: producer)
//! (compositeur:Bach ou compositeur:Haendel) duree>10min
//! ecoutes>=5 derniere_ecoute<2025-01-01
//! ```
//!
//! # Ce que `:` veut dire
//!
//! Sur un texte, « contient » ; sur un nombre, « égal » ; sur une date, la
//! date elle-même n'a pas de sens : `:30j` y veut dire « dans les 30 derniers
//! jours », et `:jamais` « jamais ». `champ:vide` cherche l'absence de valeur.
//!
//! # La négation
//!
//! `non a` est le complément EXACT de `a` : une piste que `a` ne retient pas
//! — y compris parce que la valeur est absente — passe `non a`. Le SQL de `a`
//! peut valoir NULL (une comparaison sur une colonne vide) ; `NOT NULL` vaut
//! NULL et la piste tomberait des deux côtés. D'où `NOT COALESCE((a), 1 = 0)`,
//! la même leçon que « ne contient pas » (#6250).
//!
//! # Vers l'éditeur visuel
//!
//! Une requête qui n'est qu'une suite de critères joints par ET (ou seulement
//! par OU) s'écrit telle quelle en règles d'éditeur : `rules` + `match_mode`.
//! Une requête imbriquée ou niée ne le peut pas — l'éditeur n'a qu'un niveau
//! —, et `regles_editeur` vaut alors `None` : la requête reste exécutable,
//! elle n'a simplement pas de forme visuelle.

use serde_json::{Value, json};

use crate::criteres::{CRITERES, Famille};
use crate::regles_sql;
use crate::smart_playlists::build_smart_query_rapport;
use crate::smart_refs::RefCtx;

/// Une requête lue : un arbre de critères.
#[derive(Debug, Clone, PartialEq)]
pub enum Noeud {
    /// Une règle de l'éditeur, telle quelle.
    Regle(Value),
    Et(Vec<Noeud>),
    Ou(Vec<Noeud>),
    Non(Box<Noeud>),
}

/// Une requête qui ne se lit pas, et où.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErreurDeRequete {
    /// Position, en CARACTÈRES depuis le début de la requête.
    pub position: usize,
    pub message: String,
}

impl std::fmt::Display for ErreurDeRequete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (caractère {})", self.message, self.position + 1)
    }
}

fn erreur(position: usize, message: impl Into<String>) -> ErreurDeRequete {
    ErreurDeRequete {
        position,
        message: message.into(),
    }
}

// ── Lecture des jetons ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Jeton {
    Mot(String),
    /// Une chaîne entre guillemets : jamais un mot-clé, jamais un opérateur.
    Chaine(String),
    Op(&'static str),
    ParOuvrante,
    ParFermante,
    Virgule,
    /// `!` seul, ou `-` en tête de terme : la négation.
    Non,
}

/// Les caractères qui terminent un mot.
fn coupe_un_mot(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '(' | ')' | ',' | ':' | '=' | '!' | '<' | '>' | '"' | '|' | '&'
        )
}

fn lire_les_jetons(texte: &str) -> Result<Vec<(usize, Jeton)>, ErreurDeRequete> {
    let car: Vec<char> = texte.chars().collect();
    let mut jetons = Vec::new();
    let mut i = 0;
    while i < car.len() {
        let c = car[i];
        let debut = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let deux = car.get(i + 1).copied();
        let jeton = match c {
            '(' => {
                i += 1;
                Jeton::ParOuvrante
            }
            ')' => {
                i += 1;
                Jeton::ParFermante
            }
            ',' => {
                i += 1;
                Jeton::Virgule
            }
            '|' => {
                i += if deux == Some('|') { 2 } else { 1 };
                Jeton::Mot("ou".into())
            }
            '&' => {
                i += if deux == Some('&') { 2 } else { 1 };
                Jeton::Mot("et".into())
            }
            ':' => {
                i += 1;
                Jeton::Op(":")
            }
            '=' => {
                i += if deux == Some('=') { 2 } else { 1 };
                Jeton::Op("=")
            }
            '!' if deux == Some('=') => {
                i += 2;
                Jeton::Op("!=")
            }
            '!' => {
                i += 1;
                Jeton::Non
            }
            '>' | '<' => {
                let egal = deux == Some('=');
                i += if egal { 2 } else { 1 };
                Jeton::Op(match (c, egal) {
                    ('>', true) => ">=",
                    ('>', false) => ">",
                    ('<', true) => "<=",
                    _ => "<",
                })
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    match car.get(i) {
                        None => return Err(erreur(debut, "guillemet ouvert jamais fermé")),
                        Some('\\') if matches!(car.get(i + 1), Some('"' | '\\')) => {
                            s.push(car[i + 1]);
                            i += 2;
                        }
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some(&x) => {
                            s.push(x);
                            i += 1;
                        }
                    }
                }
                Jeton::Chaine(s)
            }
            // `-genre:rock` : négation d'un terme. Un `-` au milieu d'un mot
            // (`jazz-rock`, `2024-01-01`) n'est pas lu ici, il fait partie du
            // mot.
            '-' if deux.is_some_and(|d| !d.is_whitespace() && !d.is_ascii_digit()) => {
                i += 1;
                Jeton::Non
            }
            _ => {
                let mut s = String::new();
                while i < car.len() {
                    // `4:30` (minutes:secondes) : dans un mot qui commence par
                    // un chiffre, un `:` suivi d'un chiffre reste dans le mot.
                    let dans_une_duree = car[i] == ':'
                        && s.starts_with(|c: char| c.is_ascii_digit())
                        && car.get(i + 1).is_some_and(char::is_ascii_digit);
                    if coupe_un_mot(car[i]) && !dans_une_duree {
                        break;
                    }
                    s.push(car[i]);
                    i += 1;
                }
                Jeton::Mot(s)
            }
        };
        jetons.push((debut, jeton));
    }
    Ok(jetons)
}

// ── Les noms ───────────────────────────────────────────────────────────────

/// Retire les accents et met en minuscules, pour lire `année` comme `annee`.
fn simplifier(s: &str) -> String {
    crate::smart_playlists::strip_accents(&s.to_lowercase()).replace('-', "_")
}

/// Le champ de règle (niveau PISTE) que nomme `nom`, en français ou sous son
/// nom de règle.
fn champ_de(nom: &str) -> Option<&'static str> {
    let simple = simplifier(nom);
    let alias: &[(&str, &str)] = &[
        ("artiste", "artist"),
        ("interprete", "track_artist"),
        ("artiste_album", "album_artist"),
        ("compositeur", "composer"),
        ("dossier", "folder"),
        ("annee", "year"),
        ("annee_originale", "original_year"),
        ("frequence", "sample_rate"),
        ("bits", "bit_depth"),
        ("pistes", "track_count"),
        ("duree", "duration"),
        ("numero", "track_number"),
        ("disque", "disc_number"),
        ("note_album", "rating"),
        ("note", "track_rating"),
        ("pochette", "cover_path"),
        ("ajout", "added_at"),
        ("ajoute", "added_at"),
        ("ecoutes", "play_count"),
        ("derniere_ecoute", "last_played_at"),
        ("etiquette", "tag"),
        ("titre", "title"),
        ("commentaire", "comments"),
        ("ia", "ai_generated"),
        ("hires", "hires_verdict"),
        ("favori", "favorite"),
        ("collection", "in_collection"),
        ("playlist", "in_playlist"),
    ];
    if let Some((_, champ)) = alias.iter().find(|(a, _)| *a == simple) {
        return Some(champ);
    }
    CRITERES
        .iter()
        .find_map(|c| c.piste.filter(|p| *p == simple))
}

fn famille_de(champ: &str) -> Option<Famille> {
    CRITERES
        .iter()
        .find(|c| c.piste == Some(champ))
        .map(|c| c.famille)
}

// ── Les valeurs ────────────────────────────────────────────────────────────

fn nombre(s: &str) -> Option<i64> {
    s.trim().parse().ok()
}

/// Une durée en millisecondes : `90` ou `90s` (secondes), `5min`, `1h`,
/// `2:30` (minutes:secondes). Les durées des règles sont en millisecondes
/// (`t.duration_ms`).
fn duree_ms(s: &str) -> Option<i64> {
    let s = simplifier(s);
    if let Some((m, sec)) = s.split_once(':') {
        return nombre(m)?
            .checked_mul(60)?
            .checked_add(nombre(sec)?)?
            .checked_mul(1000);
    }
    let (chiffres, unite): (String, String) = s.chars().partition(|c| c.is_ascii_digit());
    let n: i64 = chiffres.parse().ok()?;
    let facteur = match unite.as_str() {
        "" | "s" | "sec" => 1,
        "min" | "m" | "mn" => 60,
        "h" => 3600,
        _ => return None,
    };
    // Les chiffres d'abord, l'unité ensuite : `5min`, jamais `min5`.
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    n.checked_mul(facteur)?.checked_mul(1000)
}

/// `30j`, `30d`, `30` : un nombre de jours.
fn jours(s: &str) -> Option<i64> {
    let s = simplifier(s);
    let s = s
        .strip_suffix("jours")
        .or_else(|| s.strip_suffix('j'))
        .or_else(|| s.strip_suffix('d'))
        .unwrap_or(&s);
    nombre(s).filter(|n| *n > 0)
}

fn est_vide(v: &str) -> bool {
    matches!(
        simplifier(v).as_str(),
        "vide" | "empty" | "aucun" | "aucune"
    )
}

fn est_jamais(v: &str) -> bool {
    matches!(simplifier(v).as_str(), "jamais" | "never")
}

/// Une plage `bas..haut`.
fn plage(v: &str) -> Option<(&str, &str)> {
    v.split_once("..")
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
}

/// La règle d'éditeur d'un critère `champ op valeur`.
fn regle(
    champ: &'static str,
    famille: Famille,
    op: &str,
    valeur: &str,
    position: usize,
) -> Result<Value, ErreurDeRequete> {
    let r = |op: &str, value: Value| Ok(json!({"field": champ, "op": op, "value": value}));
    let refus = |pourquoi: &str| {
        Err(erreur(
            position,
            format!("{champ} {op} {valeur} : {pourquoi}"),
        ))
    };
    let nombre_de = |v: &str| -> Option<i64> {
        if champ == "duration" {
            duree_ms(v)
        } else {
            nombre(v)
        }
    };
    match famille {
        Famille::Texte => match op {
            ":" | "=" if est_vide(valeur) => r("is_null", Value::Null),
            "!=" if est_vide(valeur) => r("is_not_null", Value::Null),
            ":" => r("contains", json!(valeur)),
            "=" | "!=" => r(op, json!(valeur)),
            _ => refus("un texte se compare par « : », « = » ou « != »"),
        },
        Famille::Nombre | Famille::Compte => {
            if let Some((bas, haut)) = plage(valeur) {
                return match (nombre_de(bas), nombre_de(haut), op) {
                    (Some(a), Some(b), ":" | "=") => r("between", json!([a.min(b), a.max(b)])),
                    (Some(_), Some(_), _) => refus("une plage se donne par « : » ou « = »"),
                    _ => refus("bornes illisibles"),
                };
            }
            let Some(n) = nombre_de(valeur) else {
                return refus("nombre attendu");
            };
            let op = if op == ":" { "=" } else { op };
            if famille == Famille::Compte && op == "!=" {
                return refus("« différent » n'existe pas pour un compte d'écoutes");
            }
            // Le compte d'écoutes : « moins de » s'écrit « entre 0 et … ». Le
            // `<` du moteur ne compte que les pistes qui ont AU MOINS une
            // ligne d'écoute ; une piste jamais écoutée en a zéro, elle a
            // donc moins de N écoutes. « Entre », lui, garde les jamais
            // écoutées quand la borne basse vaut 0.
            match (famille, op) {
                (Famille::Compte, "<=") => r("between", json!([0, n.max(0)])),
                (Famille::Compte, "<") if n <= 0 => {
                    Ok(json!({"field": champ, "op": "=", "value": -1}))
                }
                (Famille::Compte, "<") => r("between", json!([0, n - 1])),
                _ => r(op, json!(n)),
            }
        }
        Famille::Horodatage => {
            if matches!(op, ":" | "=") && est_jamais(valeur) {
                return r("is_null", Value::Null);
            }
            if let Some((bas, haut)) = plage(valeur) {
                return r("between", json!([bas, haut]));
            }
            match op {
                ":" | "=" => match jours(valeur) {
                    Some(n) => r(regles_sql::DANS_LES_DERNIERS_JOURS, json!(n)),
                    None => refus("« :30j » (les 30 derniers jours), « :jamais » ou une plage"),
                },
                ">" => r(">", json!(valeur)),
                "<" => r("<", json!(valeur)),
                // Le moteur ne connaît que les comparaisons strictes : `>=`
                // rabattu sur `>` perdrait la borne sans le dire.
                _ => {
                    refus("une date se compare par « > », « < » (strictes), une plage ou « :30j »")
                }
            }
        }
        Famille::Source => match op {
            ":" | "=" => r("=", json!(simplifier(valeur))),
            "!=" => r("!=", json!(simplifier(valeur))),
            _ => refus("une source se compare par « : » ou « != »"),
        },
        Famille::Repertoire => match op {
            ":" => r("contains", json!(valeur)),
            "=" => r("starts_with", json!(valeur)),
            _ => refus("un dossier se compare par « : » (contient) ou « = » (commence par)"),
        },
        Famille::Vide => match (op, est_vide(valeur)) {
            (":" | "=", true) => r("is_null", Value::Null),
            ("!=", true) => r("is_not_null", Value::Null),
            _ => refus("seul « :vide » ou « !=vide » a un sens ici"),
        },
        Famille::Etiquette => {
            // Étape 1 : par identifiant. Le nom d'une étiquette se résoudra en
            // base à l'étape suivante — voir la PR.
            let Some(id) = nombre(valeur) else {
                return refus("une étiquette se donne par son identifiant numérique");
            };
            match op {
                ":" | "=" => r("is", json!(id.to_string())),
                "!=" => r("is_not", json!(id.to_string())),
                _ => refus("une étiquette se compare par « : » ou « != »"),
            }
        }
        Famille::Favori => {
            let quoi = match simplifier(valeur).as_str() {
                "piste" | "titre" | "track" => "track",
                "album" => "album",
                "artiste" | "artist" => "artist",
                _ => return refus("« piste », « album » ou « artiste »"),
            };
            match op {
                ":" | "=" => r("is", json!(quoi)),
                "!=" => r("is_not", json!(quoi)),
                _ => refus("un favori se compare par « : » ou « != »"),
            }
        }
        Famille::RefCollection | Famille::RefPlaylist => match op {
            ":" | "=" => r("in", json!(valeur)),
            "!=" => r("not_in", json!(valeur)),
            _ => refus("une référence se compare par « : » ou « != »"),
        },
        Famille::Marquage => {
            let oui = match simplifier(valeur).as_str() {
                "oui" | "vrai" | "true" | "yes" => true,
                "non" | "faux" | "false" | "no" => false,
                _ => return refus("« oui » ou « non »"),
            };
            match op {
                ":" | "=" => r(if oui { "is_true" } else { "is_false" }, Value::Null),
                "!=" => r(if oui { "is_false" } else { "is_true" }, Value::Null),
                _ => refus("« :oui » ou « :non »"),
            }
        }
        Famille::Credit => refus("un crédit s'écrit credit(artiste: …, role: …, instrument: …)"),
        // #6285 — `hires:verifie`, `hires!=douteux`.
        Famille::VerdictHiRes => {
            let verdict = match simplifier(valeur).as_str() {
                "verifie" => "verifie",
                "conforme" => "conforme",
                "douteux" => "douteux",
                "non_analyse" => "non_analyse",
                _ => return refus("« verifie », « conforme », « douteux » ou « non_analyse »"),
            };
            match op {
                ":" | "=" => r("=", json!(verdict)),
                "!=" => r("!=", json!(verdict)),
                _ => refus("un verdict hi-res se compare par « : » ou « != »"),
            }
        }
    }
}

// ── L'analyse ──────────────────────────────────────────────────────────────

struct Analyse {
    jetons: Vec<(usize, Jeton)>,
    i: usize,
    fin: usize,
    /// Parenthèses et négations ouvertes : l'analyse et la compilation
    /// descendent d'un appel par niveau.
    profondeur: usize,
}

/// La plus longue requête acceptée, en caractères.
pub const LONGUEUR_MAX: usize = 2000;
/// Le plus d'imbrications acceptées (parenthèses et négations cumulées).
pub const PROFONDEUR_MAX: usize = 32;

fn est_mot(j: Option<&Jeton>, mots: &[&str]) -> bool {
    matches!(j, Some(Jeton::Mot(m)) if mots.contains(&simplifier(m).as_str()))
}

const MOTS_OU: &[&str] = &["ou", "or"];
const MOTS_ET: &[&str] = &["et", "and"];
const MOTS_NON: &[&str] = &["non", "not"];

impl Analyse {
    fn voir(&self) -> Option<&Jeton> {
        self.jetons.get(self.i).map(|(_, j)| j)
    }

    fn position(&self) -> usize {
        self.jetons.get(self.i).map_or(self.fin, |(p, _)| *p)
    }

    fn descendre(&mut self) -> Result<(), ErreurDeRequete> {
        self.profondeur += 1;
        if self.profondeur > PROFONDEUR_MAX {
            return Err(erreur(
                self.position(),
                format!("plus de {PROFONDEUR_MAX} parenthèses ou négations imbriquées"),
            ));
        }
        Ok(())
    }

    fn prendre(&mut self) -> Option<(usize, Jeton)> {
        let j = self.jetons.get(self.i).cloned();
        self.i += 1;
        j
    }

    fn ou(&mut self) -> Result<Noeud, ErreurDeRequete> {
        let mut termes = vec![self.et()?];
        while est_mot(self.voir(), MOTS_OU) {
            self.i += 1;
            termes.push(self.et()?);
        }
        Ok(if termes.len() == 1 {
            termes.remove(0)
        } else {
            Noeud::Ou(termes)
        })
    }

    /// Un terme peut-il commencer ici ? (ce qui rend le ET implicite)
    fn debut_de_terme(&self) -> bool {
        match self.voir() {
            Some(Jeton::ParOuvrante | Jeton::Non | Jeton::Chaine(_)) => true,
            Some(Jeton::Mot(_)) => !est_mot(self.voir(), MOTS_OU),
            _ => false,
        }
    }

    fn et(&mut self) -> Result<Noeud, ErreurDeRequete> {
        let mut termes = vec![self.unaire()?];
        loop {
            if est_mot(self.voir(), MOTS_ET) {
                self.i += 1;
                termes.push(self.unaire()?);
            } else if self.debut_de_terme() {
                termes.push(self.unaire()?);
            } else {
                break;
            }
        }
        Ok(if termes.len() == 1 {
            termes.remove(0)
        } else {
            Noeud::Et(termes)
        })
    }

    fn unaire(&mut self) -> Result<Noeud, ErreurDeRequete> {
        if matches!(self.voir(), Some(Jeton::Non)) || est_mot(self.voir(), MOTS_NON) {
            self.i += 1;
            self.descendre()?;
            let n = self.unaire()?;
            self.profondeur -= 1;
            return Ok(Noeud::Non(Box::new(n)));
        }
        self.atome()
    }

    fn atome(&mut self) -> Result<Noeud, ErreurDeRequete> {
        let position = self.position();
        match self.prendre() {
            Some((_, Jeton::ParOuvrante)) => {
                self.descendre()?;
                let dedans = self.ou()?;
                self.profondeur -= 1;
                match self.prendre() {
                    Some((_, Jeton::ParFermante)) => Ok(dedans),
                    _ => Err(erreur(position, "parenthèse ouverte jamais fermée")),
                }
            }
            Some((_, Jeton::Mot(m))) => self.critere(position, m),
            Some((_, Jeton::Chaine(s))) => Err(erreur(
                position,
                format!(
                    "« {s} » seul ne dit pas quoi chercher : écrivez par exemple titre:\"{s}\""
                ),
            )),
            Some((p, j)) => Err(erreur(p, format!("terme attendu, trouvé {}", decrire(&j)))),
            None => Err(erreur(position, "la requête s'arrête au milieu d'un terme")),
        }
    }

    fn valeur(&mut self, apres: usize) -> Result<String, ErreurDeRequete> {
        match self.prendre() {
            Some((_, Jeton::Mot(m) | Jeton::Chaine(m))) => Ok(m),
            Some((p, j)) => Err(erreur(
                p,
                format!("valeur attendue, trouvé {}", decrire(&j)),
            )),
            None => Err(erreur(apres, "valeur attendue en fin de requête")),
        }
    }

    fn critere(&mut self, position: usize, nom: String) -> Result<Noeud, ErreurDeRequete> {
        let simple = simplifier(&nom);
        let suit_un_op = matches!(self.voir(), Some(Jeton::Op(_)));
        // Mots-clés seuls.
        if !suit_un_op {
            match simple.as_str() {
                "favori" | "favoris" | "favorite" => {
                    return Ok(Noeud::Regle(
                        json!({"field": "favorite", "op": "is", "value": "track"}),
                    ));
                }
                "jamais_ecoute" | "jamais_ecoutes" | "jamais_ecoutee" | "jamais_ecoutees"
                | "never_played" => {
                    return Ok(Noeud::Regle(
                        json!({"field": "last_played_at", "op": "is_null", "value": null}),
                    ));
                }
                "credit" | "credits" if matches!(self.voir(), Some(Jeton::ParOuvrante)) => {
                    return self.credit(position);
                }
                _ => {}
            }
            return Err(erreur(
                position,
                format!("« {nom} » : critère attendu, de la forme champ:valeur"),
            ));
        }
        let Some(champ) = champ_de(&nom) else {
            return Err(erreur(position, format!("champ inconnu : « {nom} »")));
        };
        let Some(famille) = famille_de(champ) else {
            return Err(erreur(position, format!("champ inconnu : « {nom} »")));
        };
        let Some((pos_op, Jeton::Op(op))) = self.prendre() else {
            unreachable!("un opérateur suit, vérifié plus haut")
        };
        let pos_valeur = self.position();
        let valeur = self.valeur(pos_op + op.chars().count())?;
        regle(champ, famille, op, &valeur, pos_valeur).map(Noeud::Regle)
    }

    fn credit(&mut self, position: usize) -> Result<Noeud, ErreurDeRequete> {
        self.i += 1; // la parenthèse
        let mut objet = serde_json::Map::new();
        loop {
            let pos_cle = self.position();
            let cle = match self.prendre() {
                Some((_, Jeton::Mot(m))) => m,
                Some((_, Jeton::ParFermante)) if objet.is_empty() => {
                    return Err(erreur(pos_cle, "un crédit vide ne cherche rien"));
                }
                _ => return Err(erreur(pos_cle, "artiste, role ou instrument attendu")),
            };
            let cle_regle = match simplifier(&cle).as_str() {
                "artiste" | "artist" | "artist_name" | "nom" => "artist_name",
                "role" => "role",
                "instrument" => "instrument",
                _ => {
                    return Err(erreur(
                        pos_cle,
                        format!("« {cle} » : un crédit se cherche par artiste, role ou instrument"),
                    ));
                }
            };
            match self.prendre() {
                Some((_, Jeton::Op(":" | "="))) => {}
                _ => {
                    return Err(erreur(
                        pos_cle,
                        format!("« {cle} » attend « : » puis sa valeur"),
                    ));
                }
            }
            let valeur = self.valeur(pos_cle)?;
            objet.insert(cle_regle.into(), json!(valeur));
            match self.prendre() {
                Some((_, Jeton::Virgule)) => continue,
                Some((_, Jeton::ParFermante)) => break,
                _ => {
                    return Err(erreur(
                        position,
                        "crédit mal fermé : « , » ou « ) » attendu",
                    ));
                }
            }
        }
        Ok(Noeud::Regle(
            json!({"field": "credit", "op": "has", "value": Value::Object(objet)}),
        ))
    }
}

fn decrire(j: &Jeton) -> String {
    match j {
        Jeton::Mot(m) => format!("« {m} »"),
        Jeton::Chaine(s) => format!("« \"{s}\" »"),
        Jeton::Op(o) => format!("« {o} »"),
        Jeton::ParOuvrante => "« ( »".into(),
        Jeton::ParFermante => "« ) »".into(),
        Jeton::Virgule => "« , »".into(),
        Jeton::Non => "une négation".into(),
    }
}

/// Lit une requête. Une requête vide est une erreur : elle ne dit pas quoi
/// chercher, et rendre la bibliothèque entière serait la faute de #1231.
pub fn analyser(texte: &str) -> Result<Noeud, ErreurDeRequete> {
    if texte.chars().count() > LONGUEUR_MAX {
        return Err(erreur(
            LONGUEUR_MAX,
            format!("requête trop longue : {LONGUEUR_MAX} caractères au plus"),
        ));
    }
    let jetons = lire_les_jetons(texte)?;
    if jetons.is_empty() {
        return Err(erreur(0, "requête vide"));
    }
    let mut a = Analyse {
        jetons,
        i: 0,
        fin: texte.chars().count(),
        profondeur: 0,
    };
    let arbre = a.ou()?;
    if let Some((p, j)) = a.prendre() {
        return Err(erreur(p, format!("{} inattendu ici", decrire(&j))));
    }
    Ok(arbre)
}

// ── Vers l'éditeur, vers le SQL ────────────────────────────────────────────

/// Les règles d'éditeur équivalentes, quand la requête en a une forme :
/// `(rules, match_mode)`.
pub fn regles_editeur(arbre: &Noeud) -> Option<(Vec<Value>, &'static str)> {
    let feuilles = |termes: &[Noeud]| -> Option<Vec<Value>> {
        termes
            .iter()
            .map(|t| match t {
                Noeud::Regle(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    };
    match arbre {
        Noeud::Regle(r) => Some((vec![r.clone()], "all")),
        Noeud::Et(t) => feuilles(t).map(|r| (r, "all")),
        Noeud::Ou(t) => feuilles(t).map(|r| (r, "any")),
        Noeud::Non(_) => None,
    }
}

/// Une requête compilée.
#[derive(Debug, Clone)]
pub struct RequeteCompilee {
    /// La condition, sans `WHERE`, sur les alias du moteur des playlists
    /// (`t`, `al`, `ar`).
    pub condition: String,
    /// Les critères que le moteur n'a pas su traduire (`champ op`) : ils
    /// valent FAUX, et l'appelant doit le dire (#4467).
    pub refusees: Vec<String>,
    /// Ceux d'entre eux qui sont sous une négation. FAUX nié vaut VRAI : la
    /// requête rendrait toute la bibliothèque, l'appelant doit la refuser.
    pub refusees_sous_non: Vec<String>,
}

/// Compile l'arbre : chaque règle par le moteur des playlists, les
/// combinaisons ici.
pub fn compiler(arbre: &Noeud, ctx: &RefCtx) -> RequeteCompilee {
    let mut refus = Refus::default();
    let condition = condition_de(arbre, ctx, &mut refus, false);
    RequeteCompilee {
        condition,
        refusees: refus.toutes,
        refusees_sous_non: refus.sous_non,
    }
}

#[derive(Default)]
struct Refus {
    toutes: Vec<String>,
    sous_non: Vec<String>,
}

fn condition_de(noeud: &Noeud, ctx: &RefCtx, refus: &mut Refus, sous_non: bool) -> String {
    match noeud {
        Noeud::Regle(r) => {
            let regles = Value::Array(vec![r.clone()]).to_string();
            let (w, _o, _l, refusees) =
                build_smart_query_rapport(&regles, "all", "title", "asc", None, ctx);
            if sous_non {
                refus.sous_non.extend(refusees.iter().cloned());
            }
            refus.toutes.extend(refusees);
            match w.trim().strip_prefix("WHERE ") {
                Some(c) => format!("({c})"),
                // Une règle sans condition ne retient rien : jamais « tout ».
                None => format!("({})", regles_sql::FAUX),
            }
        }
        Noeud::Et(t) => joindre(t, " AND ", ctx, refus, sous_non),
        Noeud::Ou(t) => joindre(t, " OR ", ctx, refus, sous_non),
        Noeud::Non(n) => format!(
            "(NOT COALESCE({}, {}))",
            condition_de(n, ctx, refus, true),
            regles_sql::FAUX
        ),
    }
}

fn joindre(
    termes: &[Noeud],
    liant: &str,
    ctx: &RefCtx,
    refus: &mut Refus,
    sous_non: bool,
) -> String {
    let parties: Vec<String> = termes
        .iter()
        .map(|t| condition_de(t, ctx, refus, sous_non))
        .collect();
    format!("({})", parties.join(liant))
}

#[cfg(test)]
#[path = "requete_texte_tests.rs"]
mod tests;
