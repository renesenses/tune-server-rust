//! Ce que Discogs rend, lu sans réseau.
//!
//! * `GET /users/{nom}/collection/folders/0/releases` — le dossier 0 est
//!   « All » : toute la collection, une entrée par EXEMPLAIRE possédé
//!   (`instance_id`) ; deux exemplaires du même disque font deux entrées.
//!   Chaque entrée porte `basic_information` : artistes, titre, année,
//!   labels et numéros de catalogue, formats, `master_id`.
//! * `GET /releases/{id}` — la fiche d'un disque, pour son CODE-BARRE
//!   (`identifiers[].type == "Barcode"`) : un appel de plus par disque,
//!   fait une seule fois (le code d'un pressage ne change pas).
//! * `GET /oauth/identity` — le nom du compte d'un jeton personnel.
//!
//! Un champ absent reste absent : rien ne s'invente.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Disques par page : le plafond de Discogs.
pub const PAR_PAGE: u32 = 100;
/// Au plus 200 pages, soit 20 000 exemplaires. Au-delà, la liste le dit
/// (`tronquee`) : une liste tronquée peut OMETTRE un disque, jamais en
/// inventer un.
pub const PAGES_MAX: u32 = 200;

/// La famille de support, pour les filtres de l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Famille {
    Vinyle,
    Cd,
    Sacd,
    Cassette,
    Autre,
}

impl Famille {
    pub const TOUTES: [Famille; 5] = [
        Famille::Vinyle,
        Famille::Cd,
        Famille::Sacd,
        Famille::Cassette,
        Famille::Autre,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Famille::Vinyle => "vinyle",
            Famille::Cd => "cd",
            Famille::Sacd => "sacd",
            Famille::Cassette => "cassette",
            Famille::Autre => "autre",
        }
    }

    pub fn depuis_code(code: &str) -> Option<Famille> {
        Famille::TOUTES.into_iter().find(|f| f.code() == code)
    }

    /// Un disque qui se copie dans la bibliothèque par le lecteur de CD.
    pub fn se_copie(self) -> bool {
        matches!(self, Famille::Cd | Famille::Sacd)
    }
}

/// La famille d'un nom de format Discogs (`Vinyl`, `CD`, `CDr`, `SACD`,
/// `Cassette`, `Box Set`…). `None` pour un contenant (`Box Set`, `All Media`) :
/// on regarde alors le format suivant.
fn famille_du_format(nom: &str) -> Option<Famille> {
    let n = nom.trim().to_ascii_lowercase();
    match n.as_str() {
        "box set" | "all media" => None,
        "vinyl" | "lathe cut" | "acetate" | "flexi-disc" | "shellac" => Some(Famille::Vinyle),
        "sacd" => Some(Famille::Sacd),
        "cd" | "cdr" | "cdv" | "minidisc" | "hdcd" | "blu-spec cd" | "shm-cd" => Some(Famille::Cd),
        "cassette" | "microcassette" | "dcc" | "8-track cartridge" => Some(Famille::Cassette),
        _ if n.contains("cd") => Some(Famille::Cd),
        _ => Some(Famille::Autre),
    }
}

/// Un exemplaire de la collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disque {
    pub instance_id: u64,
    pub release_id: u64,
    /// Le « master » Discogs (toutes les éditions d'un album) ; `None` pour un
    /// disque sans master.
    pub master_id: Option<u64>,
    /// Les artistes, sans le numéro de désambiguïsation de Discogs
    /// (« Nirvana (2) » → « Nirvana »).
    pub artistes: Vec<String>,
    /// L'affichage, joint comme Discogs le joint (`join`).
    pub artiste: String,
    pub titre: String,
    /// `None` quand Discogs dit `0` (année inconnue).
    pub annee: Option<i32>,
    pub labels: Vec<String>,
    pub numeros_catalogue: Vec<String>,
    /// Les noms de formats tels que Discogs les écrit (`Vinyl`, `CD`…).
    pub formats: Vec<String>,
    /// Leurs précisions (`LP`, `Album`, `Hybrid`, `Remastered`…).
    pub precisions: Vec<String>,
    pub famille: Famille,
    /// `date_added` de Discogs, telle quelle.
    pub ajoute_le: Option<String>,
}

impl Disque {
    pub fn lien_discogs(&self) -> String {
        format!("https://www.discogs.com/release/{}", self.release_id)
    }
}

/// « Nirvana (2) » → « Nirvana ». Discogs numérote les homonymes ; le
/// numéro n'appartient pas au nom.
pub fn nom_sans_numero(nom: &str) -> String {
    let t = nom.trim();
    if let Some(debut) = t.rfind(" (")
        && t.ends_with(')')
    {
        let dedans = &t[debut + 2..t.len() - 1];
        if !dedans.is_empty() && dedans.chars().all(|c| c.is_ascii_digit()) {
            return t[..debut].trim_end().to_string();
        }
    }
    t.to_string()
}

fn texte(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Un exemplaire de la liste, ou `None` s'il lui manque l'essentiel
/// (identifiants, titre).
pub fn disque_de_l_entree(e: &Value) -> Option<Disque> {
    let b = e.get("basic_information")?;
    let release_id = e
        .get("id")
        .and_then(Value::as_u64)
        .or_else(|| b.get("id").and_then(Value::as_u64))?;
    let instance_id = e.get("instance_id").and_then(Value::as_u64)?;
    let titre = texte(&b["title"])?;
    let mut artistes = Vec::new();
    let mut artiste = String::new();
    for a in b["artists"].as_array().into_iter().flatten() {
        let Some(nom) = texte(&a["name"]).map(|n| nom_sans_numero(&n)) else {
            continue;
        };
        artiste.push_str(&nom);
        if let Some(j) = a["join"].as_str().map(str::trim).filter(|j| !j.is_empty()) {
            if j == "," {
                artiste.push_str(", ");
            } else {
                artiste.push(' ');
                artiste.push_str(j);
                artiste.push(' ');
            }
        } else {
            artiste.push_str(", ");
        }
        artistes.push(nom);
    }
    let artiste = artiste
        .trim_end_matches(|c: char| c == ',' || c.is_whitespace())
        .to_string();
    let mut labels = Vec::new();
    let mut numeros = Vec::new();
    for l in b["labels"].as_array().into_iter().flatten() {
        if let Some(n) = texte(&l["name"]).map(|n| nom_sans_numero(&n))
            && !labels.contains(&n)
        {
            labels.push(n);
        }
        if let Some(c) = texte(&l["catno"]).filter(|c| !c.eq_ignore_ascii_case("none"))
            && !numeros.contains(&c)
        {
            numeros.push(c);
        }
    }
    let mut formats = Vec::new();
    let mut precisions = Vec::new();
    let mut famille = None;
    for f in b["formats"].as_array().into_iter().flatten() {
        let Some(nom) = texte(&f["name"]) else {
            continue;
        };
        if famille.is_none() {
            famille = famille_du_format(&nom);
        }
        formats.push(nom);
        for d in f["descriptions"].as_array().into_iter().flatten() {
            if let Some(d) = texte(d)
                && !precisions.contains(&d)
            {
                precisions.push(d);
            }
        }
    }
    Some(Disque {
        instance_id,
        release_id,
        master_id: b["master_id"].as_u64().filter(|m| *m > 0),
        artistes,
        artiste,
        titre,
        annee: b["year"]
            .as_i64()
            .filter(|a| *a > 0)
            .and_then(|a| i32::try_from(a).ok()),
        labels,
        numeros_catalogue: numeros,
        formats,
        precisions,
        famille: famille.unwrap_or(Famille::Autre),
        ajoute_le: texte(&e["date_added"]),
    })
}

/// Une page de la collection : ses disques, et le nombre de pages annoncé.
pub fn page_de_collection(page: &Value) -> (Vec<Disque>, Option<u32>) {
    let disques = page["releases"]
        .as_array()
        .map(|items| items.iter().filter_map(disque_de_l_entree).collect())
        .unwrap_or_default();
    let pages = page
        .pointer("/pagination/pages")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    (disques, pages)
}

/// Un code-barre comparable : les chiffres seuls, sans les zéros de tête
/// (l'UPC-A à 12 chiffres et l'EAN-13 qui le préfixe d'un 0 sont le même
/// code). `None` s'il reste moins de 6 chiffres : ce n'est pas un code.
pub fn code_barre_normalise(brut: &str) -> Option<String> {
    let chiffres: String = brut.chars().filter(char::is_ascii_digit).collect();
    let sans_zero = chiffres.trim_start_matches('0');
    (sans_zero.len() >= 6).then(|| sans_zero.to_string())
}

/// Les codes-barres d'une fiche `GET /releases/{id}` : leurs CHIFFRES, tels
/// qu'imprimés (zéros de tête compris — c'est ainsi qu'un service les
/// cherche), sans doublon, dans l'ordre de la fiche. La comparaison passe par
/// [`code_barre_normalise`].
pub fn codes_barres_de_la_fiche(fiche: &Value) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for i in fiche["identifiers"].as_array().into_iter().flatten() {
        let est_code = i["type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case("barcode"));
        if !est_code {
            continue;
        }
        let Some(brut) = i["value"].as_str() else {
            continue;
        };
        let Some(normal) = code_barre_normalise(brut) else {
            continue;
        };
        if codes
            .iter()
            .any(|c| code_barre_normalise(c).as_deref() == Some(normal.as_str()))
        {
            continue;
        }
        codes.push(brut.chars().filter(char::is_ascii_digit).collect());
    }
    codes
}

/// Le nom du compte d'un jeton personnel (`GET /oauth/identity`).
pub fn nom_de_l_identite(identite: &Value) -> Option<String> {
    texte(&identite["username"]).filter(|n| nom_utilisateur_valide(n))
}

/// Un nom d'utilisateur Discogs : lettres, chiffres, `.`, `_`, `-`, de 1 à 64
/// caractères. Il entre dans un CHEMIN d'URL : rien d'autre n'y passe.
pub fn nom_utilisateur_valide(nom: &str) -> bool {
    let n = nom.trim();
    !n.is_empty()
        && n.chars().count() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn le_numero_d_homonyme_n_appartient_pas_au_nom() {
        assert_eq!(nom_sans_numero("Nirvana (2)"), "Nirvana");
        assert_eq!(nom_sans_numero("Rick Astley"), "Rick Astley");
        // Une parenthèse qui n'est pas un numéro reste.
        assert_eq!(nom_sans_numero("Sunn O)))"), "Sunn O)))");
        assert_eq!(nom_sans_numero("Mono (Japan)"), "Mono (Japan)");
    }

    #[test]
    fn les_familles_de_support() {
        assert_eq!(famille_du_format("Vinyl"), Some(Famille::Vinyle));
        assert_eq!(famille_du_format("CD"), Some(Famille::Cd));
        assert_eq!(famille_du_format("CDr"), Some(Famille::Cd));
        assert_eq!(famille_du_format("SACD"), Some(Famille::Sacd));
        assert_eq!(famille_du_format("Cassette"), Some(Famille::Cassette));
        assert_eq!(famille_du_format("Box Set"), None);
        assert_eq!(famille_du_format("File"), Some(Famille::Autre));
    }

    #[test]
    fn un_coffret_prend_la_famille_de_son_contenu() {
        let e = json!({"id": 1, "instance_id": 9, "basic_information": {
            "id": 1, "title": "Coffret", "year": 0,
            "formats": [{"name": "Box Set", "qty": "1"}, {"name": "CD", "qty": "5", "descriptions": ["Album"]}],
            "artists": [{"name": "X", "join": ""}], "labels": []
        }});
        let d = disque_de_l_entree(&e).unwrap();
        assert_eq!(d.famille, Famille::Cd);
        assert_eq!(d.annee, None, "0 = année inconnue");
        assert_eq!(d.master_id, None);
    }

    #[test]
    fn les_codes_barres_se_comparent_sans_espaces_ni_zeros_de_tete() {
        assert_eq!(
            code_barre_normalise("5 012394 144777").as_deref(),
            Some("5012394144777")
        );
        assert_eq!(
            code_barre_normalise("0602547288221"),
            code_barre_normalise("602547288221")
        );
        assert_eq!(code_barre_normalise("LC 0316"), None);
        assert_eq!(code_barre_normalise(""), None);
    }

    #[test]
    fn le_nom_d_utilisateur_entre_dans_un_chemin_sans_rien_y_glisser() {
        assert!(nom_utilisateur_valide("bertrand_b209"));
        assert!(nom_utilisateur_valide("a.b-c"));
        assert!(!nom_utilisateur_valide(""));
        assert!(!nom_utilisateur_valide("x/../y"));
        assert!(!nom_utilisateur_valide("x?y=1"));
        assert!(!nom_utilisateur_valide(&"a".repeat(65)));
    }
}
