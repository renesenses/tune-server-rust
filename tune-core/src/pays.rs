//! Le code ISO 3166-1 alpha-2 d'un nom de pays, tel qu'une station le porte
//! en clair (« Royaume-Uni », « États-Unis »).
//!
//! Déplacée de `tune-server/src/routes/radios_libelles.rs` (#8452) : Cover+
//! en a besoin dans `tune-core` pour choisir la boutique iTunes de la
//! station. La route la réexporte telle quelle.

/// Le code ISO 3166-1 alpha-2 d'un nom de pays, français ou anglais.
///
/// La reconnaissance est insensible à la casse et aux espaces de bord. Elle
/// couvre les pays du catalogue livré, puis les voisins qu'un ajout manuel
/// écrit le plus souvent. Un nom inconnu ne rend rien : mieux vaut pas de code
/// qu'un code faux, le client retombant alors sur `country`.
pub fn code_pays(nom: &str) -> Option<&'static str> {
    let n = nom.trim().to_lowercase();
    let code = match n.as_str() {
        // Le catalogue livré (annuaire mozaiklabs + semis Radio France).
        "france" => "FR",
        "royaume-uni" | "royaume uni" | "united kingdom" | "grande-bretagne" | "great britain"
        | "uk" => "GB",
        "états-unis"
        | "etats-unis"
        | "états unis"
        | "etats unis"
        | "united states"
        | "united states of america"
        | "usa" => "US",
        "pays-bas" | "pays bas" | "netherlands" | "the netherlands" | "holland" => "NL",
        "suisse" | "switzerland" | "schweiz" | "svizzera" => "CH",
        "japon" | "japan" => "JP",
        "canada" => "CA",
        "belgique" | "belgium" | "belgië" | "belgie" => "BE",
        // Les voisins les plus probables d'un ajout à la main.
        "allemagne" | "germany" | "deutschland" => "DE",
        "italie" | "italy" | "italia" => "IT",
        "espagne" | "spain" | "españa" | "espana" => "ES",
        "portugal" => "PT",
        "autriche" | "austria" | "österreich" | "osterreich" => "AT",
        "irlande" | "ireland" => "IE",
        "suède" | "suede" | "sweden" | "sverige" => "SE",
        "norvège" | "norvege" | "norway" | "norge" => "NO",
        "danemark" | "denmark" | "danmark" => "DK",
        "finlande" | "finland" | "suomi" => "FI",
        "islande" | "iceland" => "IS",
        "pologne" | "poland" | "polska" => "PL",
        "roumanie" | "romania" | "românia" => "RO",
        "hongrie" | "hungary" => "HU",
        "tchéquie" | "tchequie" | "czechia" | "czech republic" => "CZ",
        "grèce" | "grece" | "greece" => "GR",
        "luxembourg" => "LU",
        "australie" | "australia" => "AU",
        "nouvelle-zélande" | "nouvelle-zelande" | "new zealand" => "NZ",
        "brésil" | "bresil" | "brazil" | "brasil" => "BR",
        "argentine" | "argentina" => "AR",
        "mexique" | "mexico" => "MX",
        "chine" | "china" => "CN",
        "corée du sud" | "coree du sud" | "south korea" | "korea" => "KR",
        "russie" | "russia" => "RU",
        _ => return None,
    };
    Some(code)
}
