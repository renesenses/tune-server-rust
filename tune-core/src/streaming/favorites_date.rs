//! La date de mise en favori, telle que le service la donne (#3489).
//!
//! # Le défaut
//!
//! Didier (« Gros Bidon »), forum 1666, 04/09/2026 : « Voici l'écran Favoris,
//! trié sur Album, Qobuz, Date d'ajout : l'ordre n'est pas respecté. Je change
//! le sens du tri avec la petite flèche : rien ne change. "Par défaut" et
//! "Date d'ajout" affichaient la même chose. »
//!
//! Les trois symptômes n'en font qu'un : `GET /streaming/{service}/favorites/
//! {type}` ne transportait AUCUNE date, sous aucun nom. Le trieur du client lit
//! `favorite_added_at` puis `created_at` ; les deux absents, la clé de tri vaut
//! la chaîne vide pour toutes les entrées, le comparateur rend `Equal` partout
//! et le court-circuit « pas de date des deux côtés » est atteint avant même
//! qu'on regarde le sens.
//!
//! # Ce que ce module fait, et surtout ce qu'il refuse de faire
//!
//! Il ramène à UNE forme les dates que les services écrivent chacun à leur
//! façon : Qobuz un entier d'époque (`favorited_at`), Tidal une chaîne datée
//! avec fuseau collé (`created`, `2019-04-18T09:53:31.000+0000`).
//!
//! Il rend `None` dès qu'il ne reconnaît pas la valeur, et l'appelant n'ajoute
//! alors aucune clé. C'est la consigne explicite de l'issue : « si l'une des
//! deux ne la donne pas, mieux vaut ne rien émettre pour elle que d'inventer
//! une valeur — le client sait déjà traiter l'absence, c'est la promesse muette
//! qui pose problème ». Une date fausse se trie, et se trie mal, sans que rien
//! ne le dise ; une date absente se voit.
//!
//! # La forme rendue
//!
//! `%Y-%m-%dT%H:%M:%SZ`, en UTC — celle que `listen_history.listened_at` porte
//! déjà en base (`strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`). Elle a la propriété
//! qui compte ici : à longueur constante, l'ordre lexicographique EST l'ordre
//! chronologique, donc le trieur du client range juste sans rien parser.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

/// La forme rendue : UTC, à la seconde, triable telle quelle.
const FORMAT_SORTIE: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Au-delà de cette valeur, un entier ne peut plus être des secondes d'époque
/// (ce serait l'an 5138) : c'est donc des millisecondes.
const PLANCHER_MILLISECONDES: i64 = 100_000_000_000;

/// Les formats datés que l'on sait lire, du plus précis au plus permissif.
/// `%z` avale `+0000` (Tidal), `%:z` avale `+00:00`, et les deux derniers sont
/// des dates nues que l'on lit en UTC faute de mieux.
const FORMATS_AVEC_FUSEAU: &[&str] = &["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%.f%:z"];
const FORMATS_NUS: &[&str] = &["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"];

/// La date de mise en favori d'un élément brut, cherchée sous les noms donnés
/// dans l'ordre, ou `None` si aucun ne porte une valeur lisible.
///
/// Plusieurs noms parce que chaque service a le sien et qu'aucun n'est un
/// standard : `favorited_at` chez Qobuz, `created` dans l'enveloppe de Tidal.
/// Le premier nom qui rend une date lisible gagne ; un nom présent mais
/// illisible ne bloque pas les suivants.
pub fn date_de_mise_en_favori(brut: &Value, cles: &[&str]) -> Option<String> {
    cles.iter()
        .filter_map(|cle| brut.get(*cle))
        .find_map(normaliser)
}

/// Ramène une valeur JSON à la forme de sortie, ou rend `None`.
pub fn normaliser(valeur: &Value) -> Option<String> {
    match valeur {
        Value::Number(n) => n.as_i64().and_then(depuis_epoque),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Un service peut sérialiser son entier d'époque en chaîne ; c'est
            // la même donnée, on ne la perd pas pour un guillemet.
            if let Ok(n) = s.parse::<i64>() {
                return depuis_epoque(n);
            }
            depuis_texte(s)
        }
        _ => None,
    }
}

/// Secondes — ou millisecondes — depuis l'époque.
///
/// Zéro et le négatif sont refusés : un `favorited_at` à 0 est le défaut d'un
/// champ jamais rempli, pas le 1er janvier 1970. L'émettre ferait remonter en
/// tête de tri, dans un sens, tout ce que le service n'a pas daté.
fn depuis_epoque(brut: i64) -> Option<String> {
    if brut <= 0 {
        return None;
    }
    let secondes = if brut >= PLANCHER_MILLISECONDES {
        brut / 1000
    } else {
        brut
    };
    Utc.timestamp_opt(secondes, 0)
        .single()
        .map(|d| d.format(FORMAT_SORTIE).to_string())
}

/// Une date écrite en toutes lettres, avec ou sans fuseau.
fn depuis_texte(s: &str) -> Option<String> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc).format(FORMAT_SORTIE).to_string());
    }
    for format in FORMATS_AVEC_FUSEAU {
        if let Ok(d) = DateTime::parse_from_str(s, format) {
            return Some(d.with_timezone(&Utc).format(FORMAT_SORTIE).to_string());
        }
    }
    for format in FORMATS_NUS {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, format) {
            return Some(d.and_utc().format(FORMAT_SORTIE).to_string());
        }
    }
    None
}

/// Pose `created_at` sur un élément déjà sérialisé, si et seulement si le brut
/// porte une date lisible.
///
/// La greffe se fait sur le JSON et non dans `StreamTrack` / `StreamAlbum` /
/// `StreamArtist` : ces trois structures sont construites à quatre-vingt-onze
/// endroits (recherche, détail d'album, radio, DJ automatique, greffons…) où
/// la notion de « date de mise en favori » n'a aucun sens. Un champ de plus y
/// serait `None` partout sauf sur deux routes, et il faudrait l'écrire partout
/// quand même.
pub fn greffer_created_at(element: &mut Value, brut: &Value, cles: &[&str]) {
    if let (Some(objet), Some(date)) = (element.as_object_mut(), date_de_mise_en_favori(brut, cles))
    {
        objet.insert("created_at".into(), Value::String(date));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// La forme EXACTE que Qobuz met sur chaque favori : un entier d'époque.
    /// C'est la première moitié du défaut de Didier — cette valeur existait
    /// dans la réponse amont et n'atteignait jamais le client.
    #[test]
    fn la_date_de_qobuz_est_un_entier_d_epoque() {
        let brut = json!({"id": 999, "title": "Time Out", "favorited_at": 1_700_000_000});
        assert_eq!(
            date_de_mise_en_favori(&brut, &["favorited_at"]).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
    }

    /// La forme EXACTE que Tidal met sur l'ENVELOPPE : un fuseau collé, sans
    /// deux-points, que `parse_from_rfc3339` refuse.
    #[test]
    fn la_date_de_tidal_a_son_fuseau_colle() {
        let enveloppe = json!({"created": "2019-04-18T09:53:31.000+0000", "item": {"id": 1}});
        assert_eq!(
            date_de_mise_en_favori(&enveloppe, &["created"]).as_deref(),
            Some("2019-04-18T09:53:31Z"),
            "un fuseau ecrit `+0000` doit etre lu comme les autres"
        );
    }

    /// Un fuseau qui n'est pas UTC est ramené à UTC, sans quoi deux favoris
    /// posés à la même seconde depuis deux pays se rangeraient dans le
    /// désordre.
    #[test]
    fn un_fuseau_decale_est_ramene_a_utc() {
        for ecriture in [
            "2019-04-18T11:53:31.000+02:00",
            "2019-04-18T11:53:31+02:00",
            "2019-04-18T11:53:31.000+0200",
        ] {
            assert_eq!(
                normaliser(&json!(ecriture)).as_deref(),
                Some("2019-04-18T09:53:31Z"),
                "ecriture: {ecriture}"
            );
        }
    }

    /// Les autres écritures que l'on croise : `Z`, date nue, et la forme de la
    /// table locale `streaming_favorites.created_at`.
    #[test]
    fn les_ecritures_courantes_sont_lues() {
        for (ecriture, attendu) in [
            ("2019-04-18T09:53:31Z", "2019-04-18T09:53:31Z"),
            ("2019-04-18T09:53:31", "2019-04-18T09:53:31Z"),
            ("2019-04-18 09:53:31", "2019-04-18T09:53:31Z"),
            ("1700000000", "2023-11-14T22:13:20Z"),
        ] {
            assert_eq!(
                normaliser(&json!(ecriture)).as_deref(),
                Some(attendu),
                "ecriture: {ecriture}"
            );
        }
        // Millisecondes : la même seconde, pas l'an 55 000.
        assert_eq!(
            normaliser(&json!(1_700_000_000_000i64)).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
    }

    /// La règle de l'issue, mot pour mot : « mieux vaut ne rien émettre pour
    /// elle que d'inventer une valeur ». Rien de ce qui suit n'est une date,
    /// et rien de ce qui suit ne doit en produire une.
    #[test]
    fn ce_qui_n_est_pas_une_date_ne_rend_rien() {
        for valeur in [
            json!(null),
            json!(0),
            json!(-1),
            json!(true),
            json!(""),
            json!("   "),
            json!("hier"),
            json!("2019-13-45"),
            json!({"created": "2019-04-18T09:53:31Z"}),
            json!([1, 2]),
        ] {
            assert!(
                normaliser(&valeur).is_none(),
                "ne doit rien rendre : {valeur}"
            );
        }
    }

    /// Un zéro n'est pas le 1er janvier 1970 : c'est un champ jamais rempli.
    /// L'émettre ferait remonter en tête de tri, dans un sens, tout ce que le
    /// service n'a pas daté — exactement le genre de colonne fausse que
    /// l'issue refuse.
    #[test]
    fn un_zero_n_est_pas_une_date() {
        assert!(normaliser(&json!(0)).is_none());
        assert!(normaliser(&json!("0")).is_none());
    }

    /// Le premier nom qui rend une date lisible gagne, et un nom présent mais
    /// illisible ne bloque pas les suivants.
    #[test]
    fn les_noms_sont_essayes_dans_l_ordre() {
        let brut = json!({"created": "n'importe quoi", "favorited_at": 1_700_000_000});
        assert_eq!(
            date_de_mise_en_favori(&brut, &["created", "favorited_at"]).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert!(date_de_mise_en_favori(&brut, &["absent"]).is_none());
    }

    /// La greffe pose `created_at` — et ne pose RIEN quand il n'y a pas de
    /// date. Une clé à `null` obligerait chaque client à distinguer « pas de
    /// date » de « date nulle » ; l'absence se teste toute seule.
    #[test]
    fn la_greffe_ajoute_la_cle_ou_ne_touche_a_rien() {
        let mut avec = json!({"source_id": "1", "title": "Time Out"});
        greffer_created_at(
            &mut avec,
            &json!({"favorited_at": 1_700_000_000}),
            &["favorited_at"],
        );
        assert_eq!(avec["created_at"], json!("2023-11-14T22:13:20Z"));

        let mut sans = json!({"source_id": "2", "title": "Kind of Blue"});
        greffer_created_at(&mut sans, &json!({"favorited_at": null}), &["favorited_at"]);
        assert!(
            sans.get("created_at").is_none(),
            "pas de date lisible = pas de cle, et surtout pas une cle a null"
        );
    }

    /// Ce pour quoi la forme de sortie a été choisie : à longueur constante,
    /// l'ordre du texte EST l'ordre du temps. C'est ce qui permet au trieur du
    /// client de ranger sans rien parser — et c'est ce qui manquait à Didier.
    #[test]
    fn la_forme_rendue_se_trie_comme_du_texte() {
        let mut dates: Vec<String> = [1_700_000_000i64, 1_500_000_000, 1_900_000_000]
            .iter()
            .map(|t| normaliser(&json!(t)).expect("date lisible"))
            .collect();
        dates.sort();
        assert_eq!(
            dates,
            vec![
                normaliser(&json!(1_500_000_000i64)).unwrap(),
                normaliser(&json!(1_700_000_000i64)).unwrap(),
                normaliser(&json!(1_900_000_000i64)).unwrap(),
            ]
        );
    }
}
