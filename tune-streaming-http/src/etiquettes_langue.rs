//! Les rubriques éditoriales s'affichent dans la langue de la requête.
//!
//! Qobuz rend le libellé de ses catégories sous forme d'objet **multilingue**
//! — `name_json = {"fr": "Histoires de labels", "en": "Label Stories"}`. Le
//! client Tune n'en gardait qu'une, toujours la même :
//! `tune-core/src/streaming/qobuz.rs:2519`, `obj.get("fr")` d'abord, sans
//! condition et pour tout le monde. L'anglais était dans la même réponse, et
//! jeté. Un testeur roumain, sur un compte « Qobuz UK », lisait donc
//! « Histoires de labels », « Nouveautés », « Dans le casque de… ».
//!
//! Rien n'est envoyé à Qobuz qui demande du français : la requête ne porte ni
//! `zone`, ni `lang`, ni `Accept-Language` (`qobuz.rs:606-625`). Le choix était
//! entièrement le nôtre, et il est désormais fait ici, au plus près de
//! l'affichage, d'après l'`Accept-Language` de la requête — l'en-tête dans
//! lequel le client web met la langue RÉELLEMENT choisie dans l'application,
//! et non celle du navigateur (voir `tune-server/src/i18n.rs`).
//!
//! `name` garde sa forme de chaîne : ce sont sa VALEUR et elle seule qui suit
//! la langue. Le faisceau complet part à côté, sous `name_i18n`, pour un
//! client qui préfère choisir lui-même.

use axum::http::HeaderMap;
use serde_json::Value;

/// Les langues de la requête, de la plus souhaitée à la moins, réduites à leur
/// base en minuscules (`fr-FR` → `fr`).
///
/// L'ordre suit les facteurs de qualité (`;q=`), comme le veut la norme : un
/// `Accept-Language: ro;q=0.8, fr;q=0.2` demande d'abord du roumain. À qualité
/// égale, l'ordre d'écriture est conservé.
pub fn langues_demandees(headers: &HeaderMap) -> Vec<String> {
    let Some(brut) = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
    else {
        return Vec::new();
    };
    let mut pesees: Vec<(usize, f32, String)> = brut
        .split(',')
        .enumerate()
        .filter_map(|(rang, morceau)| {
            let mut parties = morceau.split(';');
            let etiquette = parties.next()?.trim();
            if etiquette.is_empty() || etiquette == "*" {
                return None;
            }
            let qualite = parties
                .find_map(|p| p.trim().strip_prefix("q=").map(str::to_string))
                .and_then(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            let base = etiquette.split('-').next()?.trim().to_lowercase();
            (!base.is_empty()).then_some((rang, qualite, base))
        })
        .collect();
    // Tri stable : la qualité décroissante d'abord, l'ordre d'écriture ensuite.
    pesees.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut vues: Vec<String> = Vec::with_capacity(pesees.len());
    for (_, _, base) in pesees {
        if !vues.contains(&base) {
            vues.push(base);
        }
    }
    vues
}

/// Le libellé d'un faisceau, choisi d'après les langues demandées.
///
/// L'ordre : la première langue demandée que le faisceau porte, puis
/// l'anglais — langue de recours d'un catalogue international, et non le
/// français, qui n'est que la langue du studio —, puis le libellé déjà en
/// place.
fn meilleur_libelle(
    faisceau: &serde_json::Map<String, Value>,
    langues: &[String],
) -> Option<String> {
    for langue in langues {
        if let Some(v) = faisceau.get(langue).and_then(Value::as_str) {
            return Some(v.to_string());
        }
    }
    faisceau
        .get("en")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Réécrire le `name` de tout objet porteur d'un `name_i18n`, en profondeur.
///
/// En profondeur, parce que les rangées de `featured-playlists/by-tag`
/// portent le faisceau au premier niveau mais que la forme peut se nicher :
/// mieux vaut une descente complète qu'un chemin codé en dur qu'un jour on
/// oublie de suivre.
pub fn localiser(corps: &mut Value, langues: &[String]) {
    match corps {
        Value::Array(elements) => {
            for element in elements {
                localiser(element, langues);
            }
        }
        Value::Object(objet) => {
            if let Some(libelle) = objet
                .get("name_i18n")
                .and_then(Value::as_object)
                .and_then(|f| meilleur_libelle(f, langues))
            {
                objet.insert("name".into(), Value::String(libelle));
            }
            for (_, valeur) in objet.iter_mut() {
                localiser(valeur, langues);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{langues_demandees, localiser};
    use axum::http::HeaderMap;
    use serde_json::json;

    fn entetes(accept: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("accept-language", accept.parse().unwrap());
        h
    }

    #[test]
    fn la_langue_est_reduite_a_sa_base_et_triee_par_qualite() {
        assert_eq!(langues_demandees(&entetes("ro")), vec!["ro"]);
        assert_eq!(
            langues_demandees(&entetes("ro-RO,ro;q=0.9,en;q=0.8")),
            vec!["ro", "en"]
        );
        assert_eq!(
            langues_demandees(&entetes("fr;q=0.2, ro;q=0.8")),
            vec!["ro", "fr"],
            "la qualité l'emporte sur l'ordre d'écriture"
        );
        assert!(langues_demandees(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn le_libelle_suit_la_langue_demandee() {
        let bundle = json!([{
            "id": "label",
            "name": "Histoires de labels",
            "name_i18n": {"fr": "Histoires de labels", "en": "Label Stories"},
        }]);

        let mut en_roumain = bundle.clone();
        localiser(&mut en_roumain, &langues_demandees(&entetes("ro")));
        assert_eq!(
            en_roumain[0]["name"], "Label Stories",
            "le roumain n'existe pas chez Qobuz : recours à l'anglais, pas au français"
        );

        let mut en_francais = bundle.clone();
        localiser(&mut en_francais, &langues_demandees(&entetes("fr-FR,fr")));
        assert_eq!(
            en_francais[0]["name"], "Histoires de labels",
            "un lecteur francophone garde son libellé"
        );
    }

    #[test]
    fn sans_faisceau_le_libelle_ne_bouge_pas() {
        // Contre-épreuve : la fonction ne fabrique rien. Un service qui ne
        // sert pas d'objet multilingue garde son libellé mot pour mot.
        let mut corps = json!([{"id": "mood", "name": "Humeurs"}]);
        localiser(&mut corps, &langues_demandees(&entetes("ro")));
        assert_eq!(corps[0]["name"], "Humeurs");
    }

    #[test]
    fn les_rangees_imbriquees_sont_localisees_elles_aussi() {
        let mut corps = json!([{
            "id": "new",
            "name": "Nouveautés",
            "name_i18n": {"fr": "Nouveautés", "en": "New Releases"},
            "playlists": [{"id": "1", "name": "Une playlist"}],
        }]);
        localiser(&mut corps, &langues_demandees(&entetes("ro")));
        assert_eq!(corps[0]["name"], "New Releases");
        assert_eq!(
            corps[0]["playlists"][0]["name"], "Une playlist",
            "le nom propre d'une playlist n'est pas un libellé de rubrique"
        );
    }
}
