//! Cache des comptes et des pochettes de la liste des collections
//! intelligentes — #5438.
//!
//! `GET /library/smart-collections` lance, PAR collection, deux
//! `COUNT(DISTINCT …)` sur toute la bibliothèque, plus la lecture de ses
//! premières pochettes. Chez Yves Corbat (58 359 pistes), chaque ouverture de
//! l'écran des collections les rejouait toutes. Or ces nombres ne bougent que
//! lorsque la bibliothèque bouge.
//!
//! # Ce qui est mis en cache, et ce qui ne l'est jamais
//!
//! Seules les collections dont TOUTES les règles portent sur des champs de la
//! bibliothèque elle-même ([`CHAMPS_DE_BIBLIOTHEQUE`] : genre, artiste,
//! format…) sont mises en cache. Ces champs ne changent qu'au scan, à
//! l'édition ou à l'enrichissement, et chacun de ces chemins l'annonce sur le
//! bus ([`EVENEMENTS_QUI_INVALIDENT`]).
//!
//! Une règle sur une date relative (« ajoutés depuis 90 jours »), l'écoute,
//! la note, les favoris, les étiquettes, une source de service, un catalogue
//! ou une autre collection n'est JAMAIS mise en cache : son résultat change
//! sans qu'aucun événement de bibliothèque ne le dise. Elle est recalculée à
//! chaque fois, comme avant.
//!
//! # L'invalidation
//!
//! Le cache tient son propre abonné au bus et le VIDE à chaque lecture : un
//! événement de [`EVENEMENTS_QUI_INVALIDENT`], ou un retard de l'abonné
//! (`Lagged` : des événements ont été perdus, on ne sait pas lesquels), fait
//! avancer la génération et oublie tout. Aucune tâche de fond n'est nécessaire,
//! et le cache ne peut pas manquer un événement émis avant la lecture.
//!
//! Une valeur calculée pendant qu'une invalidation passe n'est pas posée : la
//! génération est relue avant d'écrire.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast::{self, error::TryRecvError};
use tune_core::event_bus::TuneEvent;

/// Les événements après lesquels un compte de collection peut avoir changé.
pub const EVENEMENTS_QUI_INVALIDENT: [&str; 5] = [
    "library.updated",
    "library.scan.completed",
    "library.enrich.completed",
    "library.artwork.completed",
    "sources.changed",
];

/// Les champs de règle dont la valeur ne change qu'avec la bibliothèque.
///
/// Liste BLANCHE : un champ absent d'ici — ou inconnu — rend la collection
/// non cachable. Se tromper dans ce sens ne coûte qu'une requête.
pub const CHAMPS_DE_BIBLIOTHEQUE: [&str; 19] = [
    "genre",
    "artist",
    "artist_name",
    "album",
    "album_title",
    "title",
    "composer",
    "label",
    "format",
    "folder",
    "file_path",
    "year",
    "sample_rate",
    "bit_depth",
    "track_count",
    "duration",
    "track_number",
    "disc_number",
    "bpm",
];

/// Vrai si toutes les règles portent sur [`CHAMPS_DE_BIBLIOTHEQUE`].
///
/// Une liste de règles illisible, ou une règle sans champ, n'est pas cachable.
pub fn regles_cachables(rules_json: &str) -> bool {
    let Ok(Value::Array(regles)) = serde_json::from_str::<Value>(rules_json) else {
        return false;
    };
    regles.iter().all(|r| {
        r.get("field")
            .and_then(Value::as_str)
            .is_some_and(|f| CHAMPS_DE_BIBLIOTHEQUE.contains(&f))
    })
}

/// Ce qui désigne un résultat : la collection telle qu'elle est définie à cet
/// instant. Une règle modifiée change la clé — l'ancienne entrée n'est plus
/// jamais lue.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Cle {
    pub profile_id: i64,
    pub rules: String,
    pub match_mode: String,
    pub sort_by: String,
    pub sort_order: String,
    pub max_limit: Option<i64>,
}

/// Les champs calculés d'une ligne de la liste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comptes {
    pub album_count: i64,
    /// Absent quand le compte serait partiel (`partiel`, #4466) ou illisible.
    pub track_count: Option<i64>,
    /// Des albums de service entrent dans la collection : `track_count` ne
    /// couvrirait qu'une part du contenu (#4466).
    pub partiel: bool,
    /// Les pochettes de la mosaïque. `None` : le serveur ne sait pas les
    /// composer seul (règle de catalogue) — le client va les chercher.
    pub covers: Option<Vec<String>>,
}

struct Etat {
    abonne: broadcast::Receiver<TuneEvent>,
    generation: u64,
    entrees: HashMap<Cle, Comptes>,
}

pub struct CacheDesComptes {
    etat: Mutex<Etat>,
}

impl CacheDesComptes {
    /// `abonne` : un abonné au bus d'événements du serveur, pris à la
    /// construction de l'état.
    pub fn new(abonne: broadcast::Receiver<TuneEvent>) -> Self {
        Self {
            etat: Mutex::new(Etat {
                abonne,
                generation: 0,
                entrees: HashMap::new(),
            }),
        }
    }

    /// Vide l'abonné ; oublie tout si la bibliothèque a pu changer.
    fn rattraper(etat: &mut Etat) {
        loop {
            match etat.abonne.try_recv() {
                Ok(ev) => {
                    if EVENEMENTS_QUI_INVALIDENT.contains(&ev.event_type.as_str()) {
                        etat.generation += 1;
                        etat.entrees.clear();
                    }
                }
                Err(TryRecvError::Lagged(_)) => {
                    etat.generation += 1;
                    etat.entrees.clear();
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            }
        }
    }

    /// La valeur en cache, et la génération à repasser à [`Self::poser`].
    pub fn lire(&self, cle: &Cle) -> (Option<Comptes>, u64) {
        let mut etat = self.etat.lock().unwrap_or_else(|e| e.into_inner());
        Self::rattraper(&mut etat);
        (etat.entrees.get(cle).cloned(), etat.generation)
    }

    /// Pose une valeur calculée depuis la génération `generation` — sauf si une
    /// invalidation est passée entre-temps.
    pub fn poser(&self, cle: Cle, comptes: Comptes, generation: u64) {
        let mut etat = self.etat.lock().unwrap_or_else(|e| e.into_inner());
        Self::rattraper(&mut etat);
        if etat.generation == generation {
            etat.entrees.insert(cle, comptes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cle(rules: &str) -> Cle {
        Cle {
            profile_id: 1,
            rules: rules.into(),
            match_mode: "all".into(),
            sort_by: "title".into(),
            sort_order: "asc".into(),
            max_limit: None,
        }
    }

    fn comptes(n: i64) -> Comptes {
        Comptes {
            album_count: n,
            track_count: Some(n * 10),
            partiel: false,
            covers: Some(vec![]),
        }
    }

    #[test]
    fn seules_les_regles_de_bibliotheque_sont_cachables() {
        assert!(regles_cachables(
            r#"[{"field":"genre","operator":"contains","value":"jazz"}]"#
        ));
        assert!(regles_cachables(
            r#"[{"field":"sample_rate","op":">","value":"96000"},{"field":"format","op":"=","value":"dsd"}]"#
        ));
        // Une date RELATIVE, l'écoute, la note, une étiquette : jamais.
        assert!(!regles_cachables(
            r#"[{"field":"added_at","operator":"greater_than","value":"90d"}]"#
        ));
        assert!(!regles_cachables(
            r#"[{"field":"genre","op":"=","value":"x"},{"field":"play_count","op":">","value":"3"}]"#
        ));
        assert!(!regles_cachables(
            r#"[{"field":"rating","op":">=","value":"4"}]"#
        ));
        assert!(!regles_cachables(
            r#"[{"field":"tag","op":"is","value":"7"}]"#
        ));
        assert!(!regles_cachables(
            r#"[{"field":"source","op":"=","value":"qobuz"}]"#
        ));
        assert!(!regles_cachables(r#"[{"op":"=","value":"x"}]"#));
        assert!(!regles_cachables("pas du json"));
    }

    #[test]
    fn un_evenement_de_bibliotheque_oublie_tout_les_autres_non() {
        let (tx, rx) = broadcast::channel(16);
        let cache = CacheDesComptes::new(rx);
        let k = cle("[]");
        let (rien, g) = cache.lire(&k);
        assert!(rien.is_none());
        cache.poser(k.clone(), comptes(3), g);
        assert_eq!(cache.lire(&k).0, Some(comptes(3)));

        tx.send(TuneEvent {
            event_type: "zone.updated".into(),
            data: json!({}),
        })
        .unwrap();
        assert_eq!(
            cache.lire(&k).0,
            Some(comptes(3)),
            "un événement de zone ne touche rien"
        );

        tx.send(TuneEvent {
            event_type: "library.updated".into(),
            data: json!({}),
        })
        .unwrap();
        assert_eq!(cache.lire(&k).0, None, "library.updated invalide");
    }

    #[test]
    fn une_valeur_calculee_pendant_une_invalidation_n_est_pas_posee() {
        let (tx, rx) = broadcast::channel(16);
        let cache = CacheDesComptes::new(rx);
        let k = cle("[]");
        let (_, g) = cache.lire(&k);
        // Le scan finit PENDANT le calcul.
        tx.send(TuneEvent {
            event_type: "library.scan.completed".into(),
            data: json!({}),
        })
        .unwrap();
        cache.poser(k.clone(), comptes(3), g);
        assert_eq!(cache.lire(&k).0, None, "valeur d'avant le scan jetée");
    }

    #[test]
    fn un_abonne_en_retard_oublie_tout() {
        let (tx, rx) = broadcast::channel(2);
        let cache = CacheDesComptes::new(rx);
        let k = cle("[]");
        let (_, g) = cache.lire(&k);
        cache.poser(k.clone(), comptes(3), g);
        for _ in 0..5 {
            tx.send(TuneEvent {
                event_type: "zone.updated".into(),
                data: json!({}),
            })
            .unwrap();
        }
        assert_eq!(
            cache.lire(&k).0,
            None,
            "des événements perdus : on ne sait pas lesquels"
        );
    }
}
