//! L'arbre des CLÉS d'une réponse brute de l'API Qobuz, sans ses valeurs (#5530).
//!
//! # Pourquoi cette sonde existe
//!
//! Qobuz a annoncé le 24/09/2026 un marquage « contenu généré par IA » au
//! niveau de la sortie. Personne n'a pu établir s'il passe par l'API JSON que
//! Tune consomme, ni sous quel nom : le mapping (`map_album`, `map_track`) lit
//! champ par champ et ignore tout le reste, et la réponse brute n'a jamais pu
//! être lue hors de Tune — elle exige la session Qobuz du serveur.
//!
//! La route d'administration `GET /streaming/qobuz/debug/raw-keys` fait donc
//! l'appel elle-même et rend CE QUE CETTE FONCTION EN GARDE : la forme du
//! document, jamais son contenu.
//!
//! # Ce qui sort, ce qui ne sort pas
//!
//! - Chaque clé, avec le ou les types rencontrés (`booleen`, `nombre`,
//!   `chaine`, `objet`, `tableau`, `null`).
//! - Les tableaux sont FUSIONNÉS : l'arbre d'un élément est l'union des clés
//!   de tous les éléments. Un marquage présent sur une seule piste d'un album
//!   apparaît donc quand même.
//! - Une VALEUR ne sort que dans deux cas :
//!   1. c'est un booléen ;
//!   2. la clé — ou une clé parente — contient l'un des fragments de
//!      [`FRAGMENTS_NOTABLES`] (`ai`, `generat`, `label`, `tag`, `flag`…).
//!      L'héritage est voulu : `"badges": ["ai-generated"]` n'a aucune clé
//!      au niveau de la chaîne, c'est la clé du tableau qui la rend lisible.
//! - Une clé sensible ([`FRAGMENTS_SENSIBLES`] : jeton, secret, e-mail, URL,
//!   utilisateur…) ne rend JAMAIS de valeur, pas même un booléen, et coupe
//!   l'héritage pour tout ce qu'elle contient.
//! - Une chaîne qui ressemble à une URL, à une adresse e-mail ou à un jeton
//!   est masquée même sous une clé notable : la clé `label` n'a pas à faire
//!   sortir une URL d'image.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// Fragments de nom de clé dont la VALEUR est rendue. Comparaison sur le nom
/// en minuscules, par sous-chaîne : c'est large exprès — on ne connaît pas le
/// nom du champ cherché (`is_ai`, `aiGenerated`, `ai_content`…).
pub const FRAGMENTS_NOTABLES: &[&str] = &[
    "ai",
    "generat",
    "artificial",
    "label",
    "tag",
    "flag",
    "badge",
];

/// Fragments de nom de clé qui interdisent toute valeur, et priment sur
/// [`FRAGMENTS_NOTABLES`] (`email` contient `ai`, `avatar_url` contient `ta`…).
pub const FRAGMENTS_SENSIBLES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "session",
    "signature",
    "auth",
    "email",
    "mail",
    "login",
    "user",
    "url",
    "uri",
    "href",
    "link",
    "key",
];

/// Nombre de valeurs distinctes gardées par clé : de quoi voir `true`/`false`
/// ou quelques libellés, pas de quoi recopier un catalogue.
const VALEURS_MAX: usize = 12;

/// Longueur au-delà de laquelle une chaîne rendue est tronquée.
const LONGUEUR_CHAINE_MAX: usize = 120;

fn cle_notable(cle: &str) -> bool {
    let cle = cle.to_ascii_lowercase();
    FRAGMENTS_NOTABLES.iter().any(|f| cle.contains(f))
}

fn cle_sensible(cle: &str) -> bool {
    let cle = cle.to_ascii_lowercase();
    FRAGMENTS_SENSIBLES.iter().any(|f| cle.contains(f))
}

/// Une chaîne qui ne doit pas sortir, quelle que soit sa clé.
fn chaine_sensible(s: &str) -> bool {
    let bas = s.to_ascii_lowercase();
    if bas.contains("://") || bas.starts_with("www.") || bas.contains('@') {
        return true;
    }
    // Un long mot sans espace fait de caractères de jeton : identifiant
    // opaque, clé, empreinte. Un libellé humain a des espaces ou reste court.
    s.len() >= 24
        && !s.contains(' ')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '+' | '/'))
}

/// Où l'on se trouve dans l'arbre : ce qui décide si une valeur peut sortir.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Regime {
    /// Types seulement ; booléens rendus.
    Ordinaire,
    /// Sous une clé notable : valeurs rendues (sauf chaîne sensible).
    Notable,
    /// Sous une clé sensible : rien, pas même les booléens.
    Sensible,
}

impl Regime {
    fn pour_cle(self, cle: &str) -> Self {
        if self == Regime::Sensible || cle_sensible(cle) {
            Regime::Sensible
        } else if self == Regime::Notable || cle_notable(cle) {
            Regime::Notable
        } else {
            Regime::Ordinaire
        }
    }
}

#[derive(Default)]
struct Noeud {
    types: Vec<&'static str>,
    valeurs: Vec<Value>,
    valeurs_masquees: bool,
    cles: BTreeMap<String, Noeud>,
    elements: Option<Box<Noeud>>,
    longueur_max: usize,
}

impl Noeud {
    fn ajouter_type(&mut self, t: &'static str) {
        if !self.types.contains(&t) {
            self.types.push(t);
        }
    }

    fn ajouter_valeur(&mut self, v: Value) {
        if !self.valeurs.contains(&v) && self.valeurs.len() < VALEURS_MAX {
            self.valeurs.push(v);
        }
    }

    fn absorber(&mut self, v: &Value, regime: Regime) {
        match v {
            Value::Null => self.ajouter_type("null"),
            Value::Bool(b) => {
                self.ajouter_type("booleen");
                if regime == Regime::Sensible {
                    self.valeurs_masquees = true;
                } else {
                    self.ajouter_valeur(Value::Bool(*b));
                }
            }
            Value::Number(n) => {
                self.ajouter_type("nombre");
                if regime == Regime::Notable {
                    self.ajouter_valeur(Value::Number(n.clone()));
                }
            }
            Value::String(s) => {
                self.ajouter_type("chaine");
                if regime == Regime::Notable {
                    if chaine_sensible(s) {
                        self.valeurs_masquees = true;
                    } else {
                        let rendu: String = s.chars().take(LONGUEUR_CHAINE_MAX).collect();
                        self.ajouter_valeur(Value::String(rendu));
                    }
                }
            }
            Value::Object(m) => {
                self.ajouter_type("objet");
                for (cle, enfant) in m {
                    self.cles
                        .entry(cle.clone())
                        .or_default()
                        .absorber(enfant, regime.pour_cle(cle));
                }
            }
            Value::Array(a) => {
                self.ajouter_type("tableau");
                self.longueur_max = self.longueur_max.max(a.len());
                let elements = self.elements.get_or_insert_with(Default::default);
                for e in a {
                    elements.absorber(e, regime);
                }
            }
        }
    }

    fn rendre(&self) -> Value {
        let mut o = Map::new();
        let types = if self.types.len() == 1 {
            Value::String(self.types[0].to_string())
        } else {
            json!(self.types)
        };
        o.insert("type".into(), types);
        if !self.valeurs.is_empty() {
            o.insert("valeurs".into(), Value::Array(self.valeurs.clone()));
        }
        if self.valeurs_masquees {
            o.insert("masque".into(), Value::Bool(true));
        }
        if !self.cles.is_empty() {
            let cles: Map<String, Value> = self
                .cles
                .iter()
                .map(|(k, n)| (k.clone(), n.rendre()))
                .collect();
            o.insert("cles".into(), Value::Object(cles));
        }
        if let Some(e) = &self.elements {
            o.insert("longueur_max".into(), json!(self.longueur_max));
            if !e.types.is_empty() {
                o.insert("elements".into(), e.rendre());
            }
        }
        Value::Object(o)
    }

    fn chemins_notables(&self, chemin: &str, sortie: &mut Vec<String>) {
        for (cle, n) in &self.cles {
            let ici = if chemin.is_empty() {
                cle.clone()
            } else {
                format!("{chemin}.{cle}")
            };
            if cle_notable(cle) && !cle_sensible(cle) {
                sortie.push(ici.clone());
            }
            n.chemins_notables(&ici, sortie);
        }
        if let Some(e) = &self.elements {
            e.chemins_notables(&format!("{chemin}[]"), sortie);
        }
    }
}

/// L'arbre des clés de `document`, avec les seules valeurs autorisées
/// (voir l'en-tête du module).
pub fn arbre_des_cles(document: &Value) -> Value {
    let mut racine = Noeud::default();
    racine.absorber(document, Regime::Ordinaire);
    racine.rendre()
}

/// Les chemins (`tracks.items[].parental_warning`) des clés dont le NOM
/// contient un fragment notable — de quoi lire en une ligne si un candidat
/// au marquage IA existe, sans parcourir l'arbre.
pub fn chemins_notables(document: &Value) -> Vec<String> {
    let mut racine = Noeud::default();
    racine.absorber(document, Regime::Ordinaire);
    let mut sortie = Vec::new();
    racine.chemins_notables("", &mut sortie);
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une réponse `album/get` FACTICE, bâtie pour piéger la sonde : chaque
    /// valeur sensible porte la chaîne `FUITE`, qui ne doit jamais ressortir.
    fn album_factice() -> Value {
        json!({
            "id": "FUITE-id-album-0825646254385",
            "title": "FUITE titre",
            "maximum_bit_depth": 24,
            "hires": true,
            "streamable": false,
            "is_ai_generated": true,
            "ai": { "generated": true, "score": 0.97, "source": "declared",
                    "contact": "FUITE@example.com" },
            // Sous des clés NOTABLES dont le nom n'est pas sensible : seul le
            // contrôle de la chaîne elle-même (URL, e-mail) les retient.
            "label": { "id": 1234, "name": "Label Factice", "slug": "label-factice",
                       "image": "https://static.qobuz.com/label/FUITE.jpg" },
            "image": { "large": "https://static.qobuz.com/FUITE.jpg" },
            "badges": ["ai-generated", "hires"],
            "user_auth_token": "FUITE-jeton-utilisateur",
            "user": { "id": 987654, "email": "FUITE@example.com", "is_admin": true },
            "tracks": {
                "total": 2,
                "items": [
                    { "id": 111, "title": "FUITE piste 1", "parental_warning": false },
                    { "id": 222, "title": "FUITE piste 2", "parental_warning": true,
                      "ai_flag": "suspected" }
                ]
            },
            "genre": { "name": "FUITE genre", "path": [1, 2] },
            "label_url": "https://www.qobuz.com/label/FUITE",
            "tag_opaque": "Zm9vYmFyRlVJVEVGVUlURUZVSVRFRlVJVEU="
        })
    }

    #[test]
    fn aucune_valeur_sensible_ne_sort() {
        let arbre = arbre_des_cles(&album_factice());
        let texte = arbre.to_string();
        assert!(!texte.contains("FUITE"), "fuite dans : {texte}");
        assert!(!texte.contains("http"), "URL dans : {texte}");
        assert!(!texte.contains('@'), "e-mail dans : {texte}");
        assert!(!texte.contains("987654"), "id utilisateur dans : {texte}");
        assert!(!texte.contains("0825646254385"));
        // Les nombres ordinaires ne sortent pas non plus.
        assert!(!texte.contains("111") && !texte.contains("222"));
        assert!(
            arbre["cles"]["genre"]["cles"]["path"]["elements"]
                .get("valeurs")
                .is_none()
        );
        // Mais les CLÉS, elles, sont toutes là, avec leur type.
        assert_eq!(arbre["cles"]["title"]["type"], "chaine");
        assert_eq!(arbre["cles"]["maximum_bit_depth"]["type"], "nombre");
        assert_eq!(arbre["cles"]["user_auth_token"]["type"], "chaine");
        assert_eq!(arbre["cles"]["user"]["cles"]["email"]["type"], "chaine");
        assert_eq!(arbre["cles"]["tracks"]["cles"]["items"]["type"], "tableau");
    }

    #[test]
    fn les_cles_ai_et_flag_gardent_leur_valeur() {
        let arbre = arbre_des_cles(&album_factice());
        let c = &arbre["cles"];
        assert_eq!(c["is_ai_generated"]["valeurs"], json!([true]));
        assert_eq!(c["ai"]["cles"]["generated"]["valeurs"], json!([true]));
        // Héritage : `score` et `source` n'ont pas de fragment notable, mais
        // vivent sous `ai`.
        assert_eq!(c["ai"]["cles"]["score"]["valeurs"], json!([0.97]));
        assert_eq!(c["ai"]["cles"]["source"]["valeurs"], json!(["declared"]));
        assert_eq!(
            c["label"]["cles"]["name"]["valeurs"],
            json!(["Label Factice"])
        );
        // L'identifiant du LABEL (catalogue, pas utilisateur) sort par héritage.
        assert_eq!(c["label"]["cles"]["id"]["valeurs"], json!([1234]));
        assert_eq!(
            c["badges"]["elements"]["valeurs"],
            json!(["ai-generated", "hires"])
        );
        let items = &c["tracks"]["cles"]["items"]["elements"]["cles"];
        assert_eq!(items["ai_flag"]["valeurs"], json!(["suspected"]));
        // Booléens ordinaires : valeur rendue, et fusionnée sur les éléments.
        assert_eq!(c["hires"]["valeurs"], json!([true]));
        assert_eq!(items["parental_warning"]["valeurs"], json!([false, true]));
    }

    #[test]
    fn une_cle_sensible_masque_meme_un_booleen_et_coupe_l_heritage() {
        let arbre = arbre_des_cles(&album_factice());
        let user = &arbre["cles"]["user"];
        assert!(user["cles"]["is_admin"].get("valeurs").is_none());
        assert_eq!(user["cles"]["is_admin"]["masque"], true);
        // `label_url` contient `label` ET `url` : le sensible gagne.
        assert!(arbre["cles"]["label_url"].get("valeurs").is_none());
        // Une chaîne-jeton sous une clé notable est masquée.
        assert!(arbre["cles"]["tag_opaque"].get("valeurs").is_none());
        assert_eq!(arbre["cles"]["tag_opaque"]["masque"], true);
    }

    #[test]
    fn les_chemins_notables_sont_listes() {
        let chemins = chemins_notables(&album_factice());
        for attendu in [
            "is_ai_generated",
            "ai",
            "ai.generated",
            "label",
            "badges",
            "tracks.items[].ai_flag",
            "tag_opaque",
        ] {
            assert!(
                chemins.iter().any(|c| c == attendu),
                "{attendu} absent de {chemins:?}"
            );
        }
        assert!(!chemins.iter().any(|c| c.contains("label_url")));
        assert!(!chemins.iter().any(|c| c.contains("email")));
    }

    #[test]
    fn les_valeurs_sont_bornees() {
        let items: Vec<Value> = (0..50).map(|i| json!({ "tag": format!("t{i}") })).collect();
        let arbre = arbre_des_cles(&json!({ "items": items }));
        let valeurs = arbre["cles"]["items"]["elements"]["cles"]["tag"]["valeurs"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(valeurs, VALEURS_MAX);
        assert_eq!(arbre["cles"]["items"]["longueur_max"], 50);
    }
}
