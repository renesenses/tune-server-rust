//! La zone que visent les raccourcis globaux et les touches média (#8531).
//!
//! La page signale la zone qu'elle affiche (`/__widget/zone/<id>`, ou la
//! commande `select_zone` de l'interface embarquée). Tant qu'elle n'a rien
//! signalé — un client web plus ancien, ou la fenêtre pas encore chargée —
//! on ne vise plus la zone 1 au hasard : on prend la règle que l'interface
//! embarquée appliquait à son ouverture (`init()` de `src/app.js`) : la zone
//! qui joue, sinon une zone en pause, sinon la première.
//!
//! Fonction pure : `main.rs` va chercher `/api/v1/widget/data` et l'appelle.

use serde_json::Value;

/// Un titre que le serveur pose le temps de reprendre une lecture : ce n'est
/// pas encore un morceau (même filtre que `src/app.js`).
const EN_REPRISE: &str = "Recovering...";

fn joue_vraiment(z: &Value) -> bool {
    if z.get("state").and_then(Value::as_str) != Some("playing") {
        return false;
    }
    let Some(np) = z.get("now_playing").filter(|v| v.is_object()) else {
        return false;
    };
    let a_une_piste = np.get("track_id").is_some_and(|v| !v.is_null());
    let titre = np.get("title").and_then(Value::as_str).unwrap_or("");
    a_une_piste || (!titre.is_empty() && titre != EN_REPRISE)
}

fn id_de(z: &Value) -> Option<i64> {
    z.get("zone_id").and_then(Value::as_i64)
}

/// La zone par défaut d'après la réponse de `GET /api/v1/widget/data`.
///
/// Le serveur énumère ses zones dans un ordre qui n'est pas stable (une table
/// de hachage) : on les trie par identifiant, pour que « la première » soit
/// toujours la même d'un appui à l'autre.
pub fn zone_par_defaut(donnees: &Value) -> Option<i64> {
    let mut zones: Vec<&Value> = donnees
        .get("zones")?
        .as_array()?
        .iter()
        .filter(|z| id_de(z).is_some())
        .collect();
    zones.sort_by_key(|z| id_de(z));
    zones
        .iter()
        .find(|z| joue_vraiment(z))
        .or_else(|| {
            zones
                .iter()
                .find(|z| z.get("state").and_then(Value::as_str) == Some("paused"))
        })
        .or_else(|| zones.first())
        .and_then(|z| id_de(z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn la_zone_qui_joue_passe_avant_la_premiere() {
        let d = json!({"zones": [
            {"zone_id": 1, "state": "stopped"},
            {"zone_id": 4, "state": "paused"},
            {"zone_id": 7, "state": "playing", "now_playing": {"track_id": 12, "title": "So What"}},
        ]});
        assert_eq!(zone_par_defaut(&d), Some(7));
    }

    #[test]
    fn sans_lecture_la_zone_en_pause_puis_la_premiere() {
        let d = json!({"zones": [
            {"zone_id": 3, "state": "stopped"},
            {"zone_id": 5, "state": "paused"},
        ]});
        assert_eq!(zone_par_defaut(&d), Some(5));
        // Ordre de la réponse quelconque : la plus petite zone, toujours.
        let d = json!({"zones": [{"zone_id": 9, "state": "stopped"}, {"zone_id": 3, "state": "stopped"}]});
        assert_eq!(zone_par_defaut(&d), Some(3));
        let d = json!({"zones": [{"zone_id": 8, "state": "paused"}, {"zone_id": 5, "state": "paused"}]});
        assert_eq!(zone_par_defaut(&d), Some(5));
    }

    #[test]
    fn une_reprise_en_cours_ne_compte_pas_comme_une_lecture() {
        let d = json!({"zones": [
            {"zone_id": 2, "state": "playing", "now_playing": {"title": "Recovering..."}},
            {"zone_id": 6, "state": "paused"},
        ]});
        assert_eq!(zone_par_defaut(&d), Some(6));
    }

    #[test]
    fn rien_de_lisible_rien_de_choisi() {
        assert_eq!(zone_par_defaut(&json!({})), None);
        assert_eq!(zone_par_defaut(&json!({"zones": []})), None);
        assert_eq!(zone_par_defaut(&json!("Not Found")), None);
    }
}
