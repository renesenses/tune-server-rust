//! L'analyseur de la grammaire de requêtes (#7848) : le texte tapé devient un
//! arbre [`Expr`], ou une [`ErreurDeRequete`] qui dit OÙ et ce qui était
//! attendu.
//!
//! ```text
//! requete  := ou FIN
//! ou       := et ( "OR" et )*
//! et       := unaire ( "AND"? unaire )*        ET implicite entre deux conditions
//! unaire   := ( "NOT" | "-" ) unaire | primaire
//! primaire := "(" ou ")" | champ ":" valeur | "loved" | terme | "texte entre guillemets"
//! valeur   := ( ">=" | "<=" | ">" | "<" | "=" )? atome | atome? ".." atome?
//! ```
//!
//! `OR`, `AND` et `NOT` s'écrivent en capitales : en minuscules, ce sont des
//! mots cherchés comme les autres. Un champ inconnu est une erreur, pas un
//! terme : `rating:5` mal orthographié ne doit pas devenir une recherche
//! silencieuse du texte « ratng:5 ». Les positions sont des indices de
//! caractères (pas d'octets), comptés à partir de 0.

use serde::Serialize;

/// Longueur maximale d'une requête, en caractères.
pub const LONGUEUR_MAX: usize = 2_000;
/// Profondeur maximale d'imbrication (parenthèses et négations).
pub const PROFONDEUR_MAX: usize = 32;
/// Nombre maximal de conditions élémentaires dans une requête.
pub const CONDITIONS_MAX: usize = 100;

/// Une requête mal formée : la position (indice de caractère) et ce qui était
/// attendu à cet endroit. Rendue telle quelle au client, jamais en 500.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErreurDeRequete {
    pub position: usize,
    pub attendu: String,
    pub message: String,
}

impl std::fmt::Display for ErreurDeRequete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (position {}, attendu : {})",
            self.message, self.position, self.attendu
        )
    }
}

impl std::error::Error for ErreurDeRequete {}

fn erreur(position: usize, attendu: &str, message: impl Into<String>) -> ErreurDeRequete {
    ErreurDeRequete {
        position,
        attendu: attendu.to_string(),
        message: message.into(),
    }
}

/// Champs texte d'une piste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChampTexte {
    Artiste,
    ArtisteAlbum,
    Album,
    Titre,
    Genre,
    Format,
    Label,
}

/// Champs numériques d'une piste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChampNombre {
    Annee,
    Frequence,
    Resolution,
    Canaux,
    /// En secondes dans la requête.
    Duree,
    NumeroDePiste,
    NumeroDeDisque,
    /// La note du profil : celle de l'album, à défaut celle des balises.
    Note,
}

/// Rôles de crédit, lus dans `track_credits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Compositeur,
    Chef,
    Interprete,
    Producteur,
    Arrangeur,
    Ingenieur,
    Mixeur,
    Remixeur,
    Parolier,
}

impl Role {
    /// Les valeurs de `track_credits.role` que couvre ce champ. Les écrivains
    /// rangent le parolier sous `writer` (`credits_mb::role_canonique`) et la
    /// voix sous `vocal` : les deux sont repris ici.
    pub fn valeurs_en_base(self) -> &'static [&'static str] {
        match self {
            Role::Compositeur => &["composer"],
            Role::Chef => &["conductor"],
            Role::Interprete => &["performer", "vocal"],
            Role::Producteur => &["producer"],
            Role::Arrangeur => &["arranger"],
            Role::Ingenieur => &["engineer"],
            Role::Mixeur => &["mixer"],
            Role::Remixeur => &["remixer"],
            Role::Parolier => &["lyricist", "writer"],
        }
    }
}

/// Le champ d'une condition `champ:valeur`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Champ {
    Texte(ChampTexte),
    Nombre(ChampNombre),
    Role(Role),
    Aime,
}

/// Les noms de champs reconnus, dans l'ordre où l'erreur les propose.
const CHAMPS: &[(&str, Champ)] = &[
    ("artist", Champ::Texte(ChampTexte::Artiste)),
    ("albumartist", Champ::Texte(ChampTexte::ArtisteAlbum)),
    ("album", Champ::Texte(ChampTexte::Album)),
    ("title", Champ::Texte(ChampTexte::Titre)),
    ("genre", Champ::Texte(ChampTexte::Genre)),
    ("format", Champ::Texte(ChampTexte::Format)),
    ("label", Champ::Texte(ChampTexte::Label)),
    ("year", Champ::Nombre(ChampNombre::Annee)),
    ("sample_rate", Champ::Nombre(ChampNombre::Frequence)),
    ("bit_depth", Champ::Nombre(ChampNombre::Resolution)),
    ("channels", Champ::Nombre(ChampNombre::Canaux)),
    ("duration", Champ::Nombre(ChampNombre::Duree)),
    ("track", Champ::Nombre(ChampNombre::NumeroDePiste)),
    ("disc", Champ::Nombre(ChampNombre::NumeroDeDisque)),
    ("rating", Champ::Nombre(ChampNombre::Note)),
    ("loved", Champ::Aime),
    ("composer", Champ::Role(Role::Compositeur)),
    ("conductor", Champ::Role(Role::Chef)),
    ("performer", Champ::Role(Role::Interprete)),
    ("producer", Champ::Role(Role::Producteur)),
    ("arranger", Champ::Role(Role::Arrangeur)),
    ("engineer", Champ::Role(Role::Ingenieur)),
    ("mixer", Champ::Role(Role::Mixeur)),
    ("remixer", Champ::Role(Role::Remixeur)),
    ("lyricist", Champ::Role(Role::Parolier)),
];

fn champ_nomme(nom: &str) -> Option<Champ> {
    let nom = nom.to_ascii_lowercase();
    CHAMPS.iter().find(|(n, _)| *n == nom).map(|(_, c)| *c)
}

fn liste_des_champs() -> String {
    CHAMPS
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Comparaison numérique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparaison {
    Egal,
    Plus,
    PlusOuEgal,
    Moins,
    MoinsOuEgal,
}

/// Ce que la condition vérifie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    /// Le texte contient la valeur (casse et accents ignorés).
    Contient(String),
    /// Le texte est la valeur (casse et accents ignorés).
    Est(String),
    Compare(Comparaison, i64),
    /// Bornes incluses ; une borne absente est ouverte.
    Plage(Option<i64>, Option<i64>),
    Booleen(bool),
}

/// L'arbre d'une requête.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Mot ou phrase libre : titre, artiste ou album le contient.
    Terme(String),
    Condition {
        champ: Champ,
        test: Test,
    },
    Et(Vec<Expr>),
    Ou(Vec<Expr>),
    Non(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Jeton {
    Ouvrante,
    Fermante,
    Mot(String),
    Guillemets(String),
}

#[derive(Debug, Clone)]
struct Lu {
    jeton: Jeton,
    debut: usize,
    fin: usize,
}

fn decouper(texte: &[char]) -> Result<Vec<Lu>, ErreurDeRequete> {
    let mut lus = Vec::new();
    let mut i = 0;
    while i < texte.len() {
        let c = texte[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' || c == ')' {
            let jeton = if c == '(' {
                Jeton::Ouvrante
            } else {
                Jeton::Fermante
            };
            lus.push(Lu {
                jeton,
                debut: i,
                fin: i + 1,
            });
            i += 1;
        } else if c == '"' {
            let debut = i;
            let mut contenu = String::new();
            i += 1;
            loop {
                match texte.get(i) {
                    None => {
                        return Err(erreur(debut, "\" fermant", "guillemet ouvert jamais fermé"));
                    }
                    Some('"') => break,
                    Some('\\') if texte.get(i + 1) == Some(&'"') => {
                        contenu.push('"');
                        i += 2;
                    }
                    Some(&autre) => {
                        contenu.push(autre);
                        i += 1;
                    }
                }
            }
            i += 1;
            lus.push(Lu {
                jeton: Jeton::Guillemets(contenu),
                debut,
                fin: i,
            });
        } else {
            let debut = i;
            while i < texte.len() && !texte[i].is_whitespace() && !"()\"".contains(texte[i]) {
                i += 1;
            }
            lus.push(Lu {
                jeton: Jeton::Mot(texte[debut..i].iter().collect()),
                debut,
                fin: i,
            });
        }
    }
    Ok(lus)
}

struct Analyseur {
    lus: Vec<Lu>,
    i: usize,
    longueur: usize,
    conditions: usize,
}

/// Analyse une requête. Une requête vide (ou faite de blancs) est une erreur :
/// « tout » n'a pas besoin de requête.
pub fn analyser(requete: &str) -> Result<Expr, ErreurDeRequete> {
    let texte: Vec<char> = requete.chars().collect();
    if texte.len() > LONGUEUR_MAX {
        return Err(erreur(
            LONGUEUR_MAX,
            "une requête plus courte",
            format!("requête trop longue (plus de {LONGUEUR_MAX} caractères)"),
        ));
    }
    let lus = decouper(&texte)?;
    let mut a = Analyseur {
        lus,
        i: 0,
        longueur: texte.len(),
        conditions: 0,
    };
    if a.lus.is_empty() {
        return Err(erreur(0, "une condition", "requête vide"));
    }
    let expr = a.ou(0)?;
    if let Some(lu) = a.lus.get(a.i) {
        let message = if lu.jeton == Jeton::Fermante {
            "parenthèse fermante sans ouvrante"
        } else {
            "texte inattendu"
        };
        return Err(erreur(lu.debut, "la fin de la requête", message));
    }
    Ok(expr)
}

fn est_mot(lu: Option<&Lu>, mot: &str) -> bool {
    matches!(lu, Some(Lu { jeton: Jeton::Mot(m), .. }) if m == mot)
}

impl Analyseur {
    fn courant(&self) -> Option<&Lu> {
        self.lus.get(self.i)
    }

    fn position_courante(&self) -> usize {
        self.courant().map(|l| l.debut).unwrap_or(self.longueur)
    }

    fn ou(&mut self, profondeur: usize) -> Result<Expr, ErreurDeRequete> {
        let mut membres = vec![self.et(profondeur)?];
        while est_mot(self.courant(), "OR") {
            self.i += 1;
            membres.push(self.et(profondeur)?);
        }
        Ok(if membres.len() == 1 {
            membres.pop().unwrap()
        } else {
            Expr::Ou(membres)
        })
    }

    fn et(&mut self, profondeur: usize) -> Result<Expr, ErreurDeRequete> {
        let mut membres = vec![self.unaire(profondeur)?];
        loop {
            match self.courant() {
                None => break,
                Some(Lu {
                    jeton: Jeton::Fermante,
                    ..
                }) => break,
                lu if est_mot(lu, "OR") => break,
                lu if est_mot(lu, "AND") => {
                    self.i += 1;
                    membres.push(self.unaire(profondeur)?);
                }
                _ => membres.push(self.unaire(profondeur)?),
            }
        }
        Ok(if membres.len() == 1 {
            membres.pop().unwrap()
        } else {
            Expr::Et(membres)
        })
    }

    fn unaire(&mut self, profondeur: usize) -> Result<Expr, ErreurDeRequete> {
        if profondeur >= PROFONDEUR_MAX {
            return Err(erreur(
                self.position_courante(),
                "une requête moins imbriquée",
                format!("plus de {PROFONDEUR_MAX} niveaux d'imbrication"),
            ));
        }
        if est_mot(self.courant(), "NOT") {
            self.i += 1;
            return Ok(Expr::Non(Box::new(self.unaire(profondeur + 1)?)));
        }
        // `-x` : le signe moins colle à ce qu'il nie. Seul, il nie le jeton
        // suivant (`-"live at"`, `-(a OR b)`).
        if let Some(Lu {
            jeton: Jeton::Mot(m),
            debut,
            fin,
        }) = self.courant().cloned()
            && m.starts_with('-')
        {
            if m == "-" {
                self.i += 1;
            } else {
                self.lus[self.i] = Lu {
                    jeton: Jeton::Mot(m[1..].to_string()),
                    debut: debut + 1,
                    fin,
                };
            }
            return Ok(Expr::Non(Box::new(self.unaire(profondeur + 1)?)));
        }
        self.primaire(profondeur)
    }

    fn compter_une_condition(&mut self, position: usize) -> Result<(), ErreurDeRequete> {
        self.conditions += 1;
        if self.conditions > CONDITIONS_MAX {
            return Err(erreur(
                position,
                "moins de conditions",
                format!("plus de {CONDITIONS_MAX} conditions"),
            ));
        }
        Ok(())
    }

    fn primaire(&mut self, profondeur: usize) -> Result<Expr, ErreurDeRequete> {
        let Some(lu) = self.courant().cloned() else {
            return Err(erreur(
                self.longueur,
                "une condition",
                "la requête s'arrête au milieu",
            ));
        };
        match lu.jeton.clone() {
            Jeton::Ouvrante => {
                self.i += 1;
                let dedans = self.ou(profondeur + 1)?;
                match self.courant() {
                    Some(Lu {
                        jeton: Jeton::Fermante,
                        ..
                    }) => {
                        self.i += 1;
                        Ok(dedans)
                    }
                    _ => Err(erreur(
                        self.position_courante(),
                        ")",
                        format!("parenthèse ouverte en position {} jamais fermée", lu.debut),
                    )),
                }
            }
            Jeton::Fermante => Err(erreur(
                lu.debut,
                "une condition",
                "parenthèse fermante inattendue",
            )),
            Jeton::Guillemets(phrase) => {
                self.i += 1;
                self.compter_une_condition(lu.debut)?;
                Ok(Expr::Terme(phrase))
            }
            Jeton::Mot(mot) => {
                if mot == "OR" || mot == "AND" {
                    return Err(erreur(
                        lu.debut,
                        "une condition",
                        format!("{mot} sans condition devant ou derrière"),
                    ));
                }
                self.i += 1;
                self.compter_une_condition(lu.debut)?;
                if mot.eq_ignore_ascii_case("loved") {
                    return Ok(Expr::Condition {
                        champ: Champ::Aime,
                        test: Test::Booleen(true),
                    });
                }
                match decouper_champ(&mot) {
                    Some((nom, reste)) => self.condition(nom, reste, &lu),
                    None => Ok(Expr::Terme(mot)),
                }
            }
        }
    }

    /// `nom:reste` ; quand `reste` n'a pas de valeur (`artist:` ou
    /// `artist:=`), elle est dans les guillemets collés qui suivent.
    fn condition(&mut self, nom: &str, reste: &str, lu: &Lu) -> Result<Expr, ErreurDeRequete> {
        let Some(champ) = champ_nomme(nom) else {
            return Err(erreur(
                lu.debut,
                &liste_des_champs(),
                format!(
                    "champ inconnu « {nom} » (pour chercher ce texte, mettez-le entre guillemets)"
                ),
            ));
        };
        let debut_valeur = lu.debut + nom.chars().count() + 1;
        let (comparaison, valeur) = separer_comparaison(reste);
        let debut_valeur = debut_valeur + reste.chars().count() - valeur.chars().count();
        let (valeur, entre_guillemets) = if valeur.is_empty() {
            match self.courant().cloned() {
                Some(Lu {
                    jeton: Jeton::Guillemets(phrase),
                    debut,
                    ..
                }) if debut == lu.fin => {
                    self.i += 1;
                    (phrase, true)
                }
                _ => {
                    return Err(erreur(
                        debut_valeur,
                        "une valeur",
                        format!("aucune valeur après « {nom}: »"),
                    ));
                }
            }
        } else {
            (valeur.to_string(), false)
        };
        let test = match champ {
            Champ::Texte(_) | Champ::Role(_) => match comparaison {
                None => Test::Contient(valeur),
                Some(Comparaison::Egal) => Test::Est(valeur),
                Some(_) => {
                    return Err(erreur(
                        lu.debut + nom.chars().count() + 1,
                        "« : » ou « := »",
                        format!("« {nom} » est un texte : il ne se compare pas avec < ou >"),
                    ));
                }
            },
            Champ::Aime => match valeur.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" if comparaison.is_none() => Test::Booleen(true),
                "false" | "no" | "0" if comparaison.is_none() => Test::Booleen(false),
                _ => {
                    return Err(erreur(debut_valeur, "true ou false", "valeur de « loved »"));
                }
            },
            Champ::Nombre(n) => {
                if entre_guillemets {
                    return Err(erreur(debut_valeur, "un nombre", "nombre entre guillemets"));
                }
                test_numerique(n, comparaison, &valeur, debut_valeur)?
            }
        };
        Ok(Expr::Condition { champ, test })
    }
}

/// `artist:miles` → (`artist`, `miles`). Le nom commence par une lettre et
/// ne contient que des lettres ASCII et `_` : `12:30` reste un terme.
fn decouper_champ(mot: &str) -> Option<(&str, &str)> {
    let (nom, reste) = mot.split_once(':')?;
    let mut lettres = nom.chars();
    let premiere = lettres.next()?;
    if premiere.is_ascii_alphabetic() && lettres.all(|c| c.is_ascii_alphabetic() || c == '_') {
        Some((nom, reste))
    } else {
        None
    }
}

fn separer_comparaison(valeur: &str) -> (Option<Comparaison>, &str) {
    for (signe, c) in [
        (">=", Comparaison::PlusOuEgal),
        ("<=", Comparaison::MoinsOuEgal),
        (">", Comparaison::Plus),
        ("<", Comparaison::Moins),
        ("=", Comparaison::Egal),
    ] {
        if let Some(reste) = valeur.strip_prefix(signe) {
            return (Some(c), reste);
        }
    }
    (None, valeur)
}

fn test_numerique(
    champ: ChampNombre,
    comparaison: Option<Comparaison>,
    valeur: &str,
    debut: usize,
) -> Result<Test, ErreurDeRequete> {
    if let Some((bas, haut)) = valeur.split_once("..") {
        if comparaison.is_some() {
            return Err(erreur(
                debut,
                "une plage a..b sans comparateur",
                "comparateur devant une plage",
            ));
        }
        let lire_borne = |texte: &str, position: usize| {
            if texte.is_empty() {
                Ok(None)
            } else {
                nombre(champ, texte, position).map(Some)
            }
        };
        let bas_lu = lire_borne(bas, debut)?;
        let haut_lu = lire_borne(haut, debut + bas.chars().count() + 2)?;
        if bas_lu.is_none() && haut_lu.is_none() {
            return Err(erreur(debut, "au moins une borne", "plage sans borne"));
        }
        return Ok(Test::Plage(bas_lu, haut_lu));
    }
    let n = nombre(champ, valeur, debut)?;
    Ok(Test::Compare(comparaison.unwrap_or(Comparaison::Egal), n))
}

/// Un nombre entier ; la fréquence s'écrit aussi en kHz (`96k`, `44.1k`).
fn nombre(champ: ChampNombre, texte: &str, position: usize) -> Result<i64, ErreurDeRequete> {
    let invalide = || {
        erreur(
            position,
            "un nombre entier",
            format!("« {texte} » n'est pas un nombre"),
        )
    };
    if champ == ChampNombre::Frequence
        && let Some(khz) = texte
            .strip_suffix("khz")
            .or_else(|| texte.strip_suffix("kHz"))
            .or_else(|| texte.strip_suffix('k'))
    {
        let f: f64 = khz.parse().map_err(|_| invalide())?;
        if !f.is_finite() || !(0.0..=10_000.0).contains(&f) {
            return Err(invalide());
        }
        return Ok((f * 1000.0).round() as i64);
    }
    texte.parse::<i64>().map_err(|_| invalide())
}
