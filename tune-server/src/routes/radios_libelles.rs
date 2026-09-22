//! Le genre et le pays d'une station voyagent en CLÉ et en CODE, plus en clair.
//!
//! Le semis du catalogue de radios écrit le genre et le pays en français —
//! `tune-core/src/db/migrations.rs` (« Éclectique », « Chanson française »,
//! « Généraliste »…) et `tune-core/migrations/radios/annuaire_*.sql`
//! (« Royaume-Uni », « États-Unis », « Pays-Bas », « Suisse », « Japon »). Ces
//! deux colonnes partaient telles quelles dans `GET /radios` : un testeur
//! roumain voyait des pastilles de genre et des pays en français, au milieu
//! d'une interface roumaine.
//!
//! Le remède suit la convention du dépôt : ce qui est stable voyage, ce qui est
//! lisible se calcule à l'affichage.
//!
//! * `country_code` — le code ISO 3166-1 alpha-2 du pays (`GB`, `US`, `NL`…).
//!   Un code, pas un nom : le client le rend avec `Intl.DisplayNames` dans SA
//!   langue, sans table à tenir, et pour n'importe quel pays.
//! * `genre_key` — une clé stable (`radio.genre.eclectic`) ; le genre musical
//!   n'a pas de norme ISO, donc c'est une clé du catalogue de traduction.
//! * `genre_label` — le genre déjà traduit dans la langue de la requête, pour
//!   qu'un client puisse l'afficher sans embarquer les 10 langues.
//!
//! Les trois champs sont AJOUTÉS : `genre` et `country` gardent mot pour mot
//! leur valeur d'avant, `docs/contrat-web.json` les cite toujours et le client
//! publié ne voit aucune différence.
//!
//! Aucune table n'est modifiée en base : la reconnaissance se fait à
//! l'affichage, sur la valeur stockée. Une station ajoutée à la main par un
//! utilisateur, avec un genre à lui, reste donc intacte — elle n'obtient
//! simplement pas de clé, et le client retombe sur `genre`.

use serde_json::Value;

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

/// La clé de traduction d'un genre de station, ou rien si le genre n'est pas
/// l'un de ceux du catalogue livré.
///
/// Les clés vivent dans `i18n_server.json`, sous `radio.genre.*`, dans les dix
/// langues de l'interface.
pub fn cle_genre(genre: &str) -> Option<&'static str> {
    let g = genre.trim().to_lowercase();
    let cle = match g.as_str() {
        "éclectique" | "eclectique" | "eclectic" => "radio.genre.eclectic",
        "chanson française" | "chanson francaise" | "french chanson" => "radio.genre.frenchSong",
        "classique" | "classical" => "radio.genre.classical",
        "contemporaine" | "contemporary" => "radio.genre.contemporary",
        "culture" => "radio.genre.culture",
        "généraliste" | "generaliste" | "generalist" => "radio.genre.generalist",
        "électronique" | "electronique" | "electronic" | "electro" => "radio.genre.electronic",
        "groove" => "radio.genre.groove",
        "hip-hop" | "hip hop" | "hiphop" => "radio.genre.hipHop",
        "jazz" => "radio.genre.jazz",
        "metal" | "métal" => "radio.genre.metal",
        "monde" | "world" | "world music" => "radio.genre.world",
        "pop" => "radio.genre.pop",
        "reggae" => "radio.genre.reggae",
        "rock" => "radio.genre.rock",
        "blues" => "radio.genre.blues",
        "soul" => "radio.genre.soul",
        "funk" => "radio.genre.funk",
        "folk" => "radio.genre.folk",
        "ambient" | "ambiant" => "radio.genre.ambient",
        _ => return None,
    };
    Some(cle)
}

/// Ajouter `country_code`, `genre_key` et `genre_label` à UNE station.
///
/// Les champs existants ne sont jamais réécrits. Les nouveaux ne sont posés que
/// lorsqu'ils sont connus : une station dont le genre est libre garde
/// simplement son `genre`, sans clé, et le client l'affiche tel quel.
pub fn enrichir_station(station: &mut Value, lang: &str) {
    let Some(objet) = station.as_object_mut() else {
        return;
    };
    if let Some(code) = objet
        .get("country")
        .and_then(Value::as_str)
        .and_then(code_pays)
    {
        objet.insert("country_code".into(), Value::String(code.into()));
    }
    if let Some(cle) = objet
        .get("genre")
        .and_then(Value::as_str)
        .and_then(cle_genre)
    {
        let libelle = crate::i18n::t(lang, cle);
        objet.insert("genre_key".into(), Value::String(cle.into()));
        objet.insert("genre_label".into(), Value::String(libelle));
    }
}

/// Le même travail sur ce que rend une route : un objet station, un tableau de
/// stations, ou un objet qui en porte un sous `items` ou `radio`.
///
/// Une seule fonction pour tous les points de sortie de `routes/radios.rs` :
/// un point oublié est exactement le défaut que cette fiche corrige.
pub fn enrichir(corps: &mut Value, lang: &str) {
    match corps {
        Value::Array(stations) => {
            for station in stations {
                enrichir_station(station, lang);
            }
        }
        Value::Object(objet) => {
            if objet.contains_key("stream_url") || objet.contains_key("url") {
                enrichir_station(corps, lang);
                return;
            }
            for cle in ["items", "radio", "stations", "suggestions"] {
                if let Some(sous) = objet.get_mut(cle) {
                    enrichir(sous, lang);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{cle_genre, code_pays, enrichir};
    use serde_json::json;

    #[test]
    fn les_pays_du_catalogue_livre_ont_tous_un_code() {
        for (nom, attendu) in [
            ("France", "FR"),
            ("Royaume-Uni", "GB"),
            ("États-Unis", "US"),
            ("Pays-Bas", "NL"),
            ("Suisse", "CH"),
            ("Japon", "JP"),
            ("Canada", "CA"),
            ("Belgique", "BE"),
        ] {
            assert_eq!(code_pays(nom), Some(attendu), "pays {nom}");
        }
    }

    #[test]
    fn un_pays_inconnu_ne_rend_aucun_code() {
        // Contre-épreuve : la table ne devine pas. Sans ça, une table qui
        // renverrait toujours « FR » passerait l'essai précédent.
        assert_eq!(code_pays("Sylvanie"), None);
        assert_eq!(code_pays(""), None);
    }

    #[test]
    fn les_genres_du_catalogue_livre_ont_tous_une_cle() {
        for (genre, attendue) in [
            ("Éclectique", "radio.genre.eclectic"),
            ("Chanson française", "radio.genre.frenchSong"),
            ("Classique", "radio.genre.classical"),
            ("Contemporaine", "radio.genre.contemporary"),
            ("Culture", "radio.genre.culture"),
            ("Généraliste", "radio.genre.generalist"),
            ("Électronique", "radio.genre.electronic"),
            ("Monde", "radio.genre.world"),
            ("Hip-Hop", "radio.genre.hipHop"),
        ] {
            assert_eq!(cle_genre(genre), Some(attendue), "genre {genre}");
        }
    }

    #[test]
    fn un_genre_libre_reste_sans_cle() {
        assert_eq!(cle_genre("Fanfare de quartier"), None);
    }

    #[test]
    fn enrichir_pose_les_champs_sans_toucher_aux_anciens() {
        let mut corps = json!([{
            "id": 1,
            "name": "Linn Classical",
            "stream_url": "http://radio.linn.co.uk:8004/autodj",
            "country": "Royaume-Uni",
            "genre": "Classique",
        }]);
        enrichir(&mut corps, "ro");
        let station = &corps[0];
        assert_eq!(station["country"], "Royaume-Uni", "`country` intact");
        assert_eq!(station["genre"], "Classique", "`genre` intact");
        assert_eq!(station["country_code"], "GB");
        assert_eq!(station["genre_key"], "radio.genre.classical");
        assert_eq!(station["genre_label"], "Clasică");
    }

    #[test]
    fn une_station_au_genre_libre_ne_gagne_pas_de_cle() {
        let mut corps = json!({
            "id": 7,
            "name": "Ma radio",
            "stream_url": "https://exemple.invalid/flux",
            "genre": "Fanfare de quartier",
            "country": "Sylvanie",
        });
        enrichir(&mut corps, "ro");
        assert!(corps.get("genre_key").is_none());
        assert!(corps.get("genre_label").is_none());
        assert!(corps.get("country_code").is_none());
        assert_eq!(corps["genre"], "Fanfare de quartier");
    }
}
