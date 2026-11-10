//! Les artistes d'une piste, UN PAR LIEN (#8327).
//!
//! Une piste ne porte qu'une chaîne d'artiste : « A & B », « A feat. C ».
//! Les clients qui voulaient un lien par artiste la découpaient eux-mêmes,
//! puis retrouvaient chaque morceau par son NOM — d'où des liens vers un
//! homonyme, ou vers « Belle » et « Sebastian » pour Belle & Sebastian.
//!
//! Ce module rend la liste structurée `artists: [{name, id?, id_service?,
//! role}]`, avec la règle de découpage du dépôt
//! ([`super::artist_split`]) et une règle de PRUDENCE :
//!
//! - le découpage FORT (« ; », « feat. », « ft. », « featuring », « with »,
//!   « vs. ») est toujours appliqué : ces marques ne se trouvent pas dans un
//!   nom d'artiste. Ce qui suit « feat. »/« with » est `featured`, le reste
//!   `main` ;
//! - le découpage RISQUÉ (« & », « , », « and ») n'est appliqué que si
//!   CHAQUE morceau est confirmé par `confirme` (un artiste de bibliothèque
//!   de ce nom exact). Sans confirmation, le morceau reste entier : « Belle
//!   & Sebastian » n'est pas coupé, et un service qui nomme son artiste
//!   « A & B » désigne bien UNE fiche, la sienne ;
//! - un identifiant n'est jamais deviné : `id`/`id_service` de la piste ne
//!   vont qu'à un artiste UNIQUE dont le nom est la chaîne entière ; les
//!   autres viennent de `confirme`, ou restent absents.

use serde::{Deserialize, Serialize};

use super::artist_split::{analyze_artist_credit, is_allowlisted, replace_ci};
use crate::db::engine::fold_diacritics;

/// Le rôle d'un artiste dans le crédit de la piste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleDArtiste {
    /// Artiste principal (avant toute marque d'invitation).
    Main,
    /// Invité : ce qui suit « feat. », « ft. », « featuring », « with ».
    Featured,
}

/// Un artiste de la piste, tel que les clients en font un lien.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtisteDePiste {
    pub name: String,
    /// Identifiant de BIBLIOTHÈQUE (`artists.id`), quand il est sûr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Identifiant de l'artiste CHEZ LE SERVICE de la piste, quand il est sûr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_service: Option<String>,
    pub role: RoleDArtiste,
}

/// Marques fortes et rôle de ce qui les SUIT.
const MARQUES_FORTES: &[(&str, RoleDArtiste)] = &[
    (" featuring ", RoleDArtiste::Featured),
    (" feat. ", RoleDArtiste::Featured),
    (" feat ", RoleDArtiste::Featured),
    (" ft. ", RoleDArtiste::Featured),
    (" ft ", RoleDArtiste::Featured),
    (" with ", RoleDArtiste::Featured),
    (" vs. ", RoleDArtiste::Main),
    (" vs ", RoleDArtiste::Main),
];

const SEP_PRINCIPAL: char = '\u{1}';
const SEP_INVITE: char = '\u{2}';

fn cle(nom: &str) -> String {
    fold_diacritics(nom.trim()).to_lowercase()
}

/// « (feat. B) » laisse « A (» et « B) » : on ôte les parenthèses orphelines.
fn nettoyer(morceau: &str) -> String {
    let mut s = morceau.trim();
    loop {
        let avant = s;
        if s.ends_with('(') || s.ends_with('[') {
            s = s[..s.len() - 1].trim_end();
        }
        if (s.ends_with(')') && !s.contains('(')) || (s.ends_with(']') && !s.contains('[')) {
            s = s[..s.len() - 1].trim_end();
        }
        if (s.starts_with('(') && !s.contains(')')) || (s.starts_with('[') && !s.contains(']')) {
            s = s[1..].trim_start();
        }
        if s == avant {
            break;
        }
    }
    s.to_string()
}

/// Le découpage FORT, avec le rôle de chaque morceau.
fn morceaux_forts(credit: &str) -> Vec<(String, RoleDArtiste)> {
    let mut travail = credit.replace(';', &SEP_PRINCIPAL.to_string());
    // « (feat. X) » : la parenthèse ouvrante colle à la marque.
    travail = replace_ci(&travail, "(feat. ", " feat. ");
    travail = replace_ci(&travail, "(ft. ", " ft. ");
    travail = replace_ci(&travail, "(featuring ", " featuring ");
    for (marque, role) in MARQUES_FORTES {
        let sep = match role {
            RoleDArtiste::Main => SEP_PRINCIPAL,
            RoleDArtiste::Featured => SEP_INVITE,
        };
        travail = replace_ci(&travail, marque, &sep.to_string());
    }
    let mut out = Vec::new();
    let mut role = RoleDArtiste::Main;
    let mut courant = String::new();
    for c in travail.chars() {
        if c == SEP_PRINCIPAL || c == SEP_INVITE {
            out.push((nettoyer(&courant), role));
            courant.clear();
            // Un invité le reste jusqu'à la fin : « A feat. B; C » → C invité.
            if c == SEP_INVITE {
                role = RoleDArtiste::Featured;
            }
        } else {
            courant.push(c);
        }
    }
    out.push((nettoyer(&courant), role));
    out.retain(|(n, _)| !n.is_empty());
    out
}

/// La liste des artistes de la piste dont le crédit est `credit`.
///
/// - `id` / `id_service` : les identifiants que la piste porte pour son
///   artiste (bibliothèque, service). Ils ne vont qu'à un artiste unique dont
///   le nom est le crédit entier.
/// - `confirme` : rend l'identifiant de bibliothèque d'un artiste de ce nom
///   exact, ou `None`. Un découpage « & »/« , » n'a lieu que si TOUS les
///   morceaux sont confirmés. `&|_| None` hors base : seul le découpage fort
///   s'applique alors.
///
/// Rend une liste vide pour un crédit vide.
pub fn artistes_de_piste(
    credit: &str,
    id: Option<i64>,
    id_service: Option<&str>,
    confirme: &dyn Fn(&str) -> Option<i64>,
) -> Vec<ArtisteDePiste> {
    let credit = credit.trim();
    if credit.is_empty() {
        return Vec::new();
    }
    let id_service = id_service
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let entier = |role| ArtisteDePiste {
        name: credit.to_string(),
        id: id.or_else(|| confirme(credit)),
        id_service: id_service.clone(),
        role,
    };
    if is_allowlisted(credit, &[])
        || credit.eq_ignore_ascii_case("various artists")
        || credit.eq_ignore_ascii_case("va")
    {
        return vec![entier(RoleDArtiste::Main)];
    }

    let mut out: Vec<ArtisteDePiste> = Vec::new();
    for (morceau, role) in morceaux_forts(credit) {
        let parties = if is_allowlisted(&morceau, &[]) {
            vec![morceau.clone()]
        } else {
            analyze_artist_credit(&morceau, &[], true).tokens
        };
        let ids: Vec<Option<i64>> = parties.iter().map(|p| confirme(p)).collect();
        if parties.len() > 1 && ids.iter().all(Option::is_some) {
            for (nom, id) in parties.into_iter().zip(ids) {
                out.push(ArtisteDePiste {
                    name: nom,
                    id,
                    id_service: None,
                    role,
                });
            }
        } else {
            let id = confirme(&morceau);
            out.push(ArtisteDePiste {
                name: morceau,
                id,
                id_service: None,
                role,
            });
        }
    }

    // Dédoublonnage (casse et accents ignorés), dans l'ordre du crédit.
    let mut vus: Vec<String> = Vec::new();
    out.retain(|a| {
        let k = cle(&a.name);
        if vus.contains(&k) {
            false
        } else {
            vus.push(k);
            true
        }
    });

    // Rien n'a été coupé : c'est l'artiste de la piste, avec SES identifiants.
    if out.len() == 1 && cle(&out[0].name) == cle(credit) {
        return vec![entier(out[0].role)];
    }
    out
}

/// La même liste, sans base : découpage fort seulement (voir
/// [`artistes_de_piste`]).
pub fn artistes_de_piste_sans_base(
    credit: &str,
    id: Option<i64>,
    id_service: Option<&str>,
) -> Vec<ArtisteDePiste> {
    artistes_de_piste(credit, id, id_service, &|_| None)
}

/// Sérialise `piste` (par sa sérialisation dérivée) suivie de la clé
/// `artists`, absente quand la liste est vide.
///
/// Les types qui l'emploient dérivent `Serialize`/`Deserialize` avec
/// `#[serde(remote = "Self")]` : la forme dérivée reste la référence, et la
/// lecture d'un JSON ancien ou nouveau ne change pas (la clé `artists` y est
/// ignorée).
#[derive(Serialize)]
pub(crate) struct AvecArtistes<B: Serialize> {
    #[serde(flatten)]
    pub piste: B,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<ArtisteDePiste>,
}

/// Branche une forme dérivée `remote = "Self"` sur `Serialize`/`Deserialize`,
/// avec la clé `artists` calculée par `$artistes`.
macro_rules! serialiser_avec_artistes {
    ($type:ty, $brut:ident, |$p:ident| $artistes:expr) => {
        struct $brut<'a>(&'a $type);
        impl serde::Serialize for $brut<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                <$type>::serialize(self.0, s)
            }
        }
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let $p = self;
                serde::Serialize::serialize(
                    &$crate::metadata::artistes_de_piste::AvecArtistes {
                        piste: $brut(self),
                        artists: $artistes,
                    },
                    s,
                )
            }
        }
        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <$type>::deserialize(d)
            }
        }
    };
}
pub(crate) use serialiser_avec_artistes;

#[cfg(test)]
mod tests {
    use super::*;

    fn noms(v: &[ArtisteDePiste]) -> Vec<(&str, RoleDArtiste)> {
        v.iter().map(|a| (a.name.as_str(), a.role)).collect()
    }

    #[test]
    fn credit_simple_garde_ses_identifiants() {
        let v = artistes_de_piste_sans_base("Radiohead", Some(7), Some("q-1"));
        assert_eq!(
            v,
            vec![ArtisteDePiste {
                name: "Radiohead".into(),
                id: Some(7),
                id_service: Some("q-1".into()),
                role: RoleDArtiste::Main,
            }]
        );
    }

    #[test]
    fn credit_vide_rend_une_liste_vide() {
        assert!(artistes_de_piste_sans_base("  ", Some(1), None).is_empty());
    }

    #[test]
    fn feat_coupe_et_marque_l_invite_sans_preter_l_identifiant() {
        let v =
            artistes_de_piste_sans_base("Daft Punk feat. Pharrell Williams", Some(3), Some("x"));
        assert_eq!(
            noms(&v),
            vec![
                ("Daft Punk", RoleDArtiste::Main),
                ("Pharrell Williams", RoleDArtiste::Featured)
            ]
        );
        assert!(v.iter().all(|a| a.id.is_none() && a.id_service.is_none()));
    }

    #[test]
    fn feat_entre_parentheses() {
        let v = artistes_de_piste_sans_base("A (feat. B)", None, None);
        assert_eq!(
            noms(&v),
            vec![("A", RoleDArtiste::Main), ("B", RoleDArtiste::Featured)]
        );
    }

    #[test]
    fn esperluette_sans_confirmation_reste_entiere() {
        let v = artistes_de_piste_sans_base("Belle & Sebastian", Some(9), Some("s-9"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Belle & Sebastian");
        assert_eq!(v[0].id, Some(9));
        assert_eq!(v[0].id_service.as_deref(), Some("s-9"));
    }

    #[test]
    fn esperluette_confirmee_coupe_avec_les_identifiants_de_bibliotheque() {
        let confirme = |n: &str| match n {
            "Miles Davis" => Some(1),
            "John Coltrane" => Some(2),
            _ => None,
        };
        let v = artistes_de_piste("Miles Davis & John Coltrane", Some(40), None, &confirme);
        assert_eq!(
            v.iter()
                .map(|a| (a.name.as_str(), a.id))
                .collect::<Vec<_>>(),
            vec![("Miles Davis", Some(1)), ("John Coltrane", Some(2))]
        );
        assert!(v.iter().all(|a| a.role == RoleDArtiste::Main));
    }

    #[test]
    fn esperluette_a_moitie_confirmee_reste_entiere() {
        let confirme = |n: &str| (n == "Belle").then_some(1);
        let v = artistes_de_piste("Belle & Sebastian", Some(5), None, &confirme);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, Some(5));
    }

    #[test]
    fn nom_de_groupe_connu_jamais_coupe() {
        let confirme = |_: &str| Some(1);
        let v = artistes_de_piste("Simon & Garfunkel", Some(8), None, &confirme);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, Some(8));
    }

    #[test]
    fn point_virgule_et_feat_ensemble() {
        let v = artistes_de_piste_sans_base("A; B feat. C", None, None);
        assert_eq!(
            noms(&v),
            vec![
                ("A", RoleDArtiste::Main),
                ("B", RoleDArtiste::Main),
                ("C", RoleDArtiste::Featured)
            ]
        );
    }

    #[test]
    fn serialisation_du_role() {
        let a = ArtisteDePiste {
            name: "C".into(),
            id: None,
            id_service: None,
            role: RoleDArtiste::Featured,
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"name": "C", "role": "featured"})
        );
    }
}
