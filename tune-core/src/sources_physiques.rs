//! Le registre commun des SOURCES PHYSIQUES (#5065, étape 1).
//!
//! Un lecteur de CD, une entrée audio USB, une entrée virtuelle, une capture
//! HDMI : des sources qui existent sur la machine du serveur et qu'on
//! sélectionne « comme sur un amplificateur ». Chaque greffon natif déclare
//! les siennes ici, dans une forme commune ; le cœur les agrège et les sert :
//!
//! * `GET /api/v1/sources` — la liste ([`RegistreSources::lister`]) ;
//! * l'événement du bus `sources.changed`, portant la liste COMPLÈTE, émis à
//!   chaque changement RÉEL (apparition, disparition, changement d'état ou de
//!   détail) — une réinscription à l'identique n'émet rien ;
//! * `POST /api/v1/sources/{id}/jouer` — délégué au greffon propriétaire par
//!   son [`JoueurSource`].
//!
//! Le registre vit en mémoire : une source n'existe que tant que son greffon
//! tourne et la déclare. Rien n'est persisté.
//!
//! ## Identifiants
//!
//! L'`id` est unique dans tout le registre (`cd`, `entree:yeti-x`) : c'est lui
//! que la route `/sources/{id}/jouer` désigne. Un greffon ne peut ni écraser
//! ni retirer la source d'un autre greffon : l'inscription d'un `id` déjà
//! tenu par un autre greffon est refusée.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event_bus::EventBus;
use crate::event_types::EventType;

/// La nature de la source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeSource {
    Cd,
    Entree,
    Virtuelle,
    Hdmi,
}

/// Ce que la source fait en ce moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EtatSource {
    /// Un disque est dans le lecteur.
    Disque,
    /// Le lecteur est là, sans disque.
    Vide,
    /// L'entrée reçoit un signal.
    Signal,
    /// L'entrée est ouverte, sans signal.
    Silence,
    /// Le système refuse l'accès au périphérique (micro sous macOS…).
    AutorisationRefusee,
    /// La plateforme du serveur ne sait pas lire cette source.
    NonPrisEnCharge,
    /// Présente mais inutilisable pour une autre raison.
    Indisponible,
}

/// Une source, telle que `GET /api/v1/sources` la rend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    #[serde(rename = "type")]
    pub genre: TypeSource,
    /// Le nom du greffon propriétaire.
    pub greffon: String,
    pub nom: String,
    pub etat: EtatSource,
    /// Le détail propre au type (album d'un CD, fréquence d'une entrée…).
    /// Toujours un objet JSON, `{}` s'il n'y a rien à dire.
    pub detail: Value,
}

/// `POST /api/v1/sources/{id}/jouer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandeJouer {
    pub zone_id: i64,
    /// Pour un CD : la piste de départ (la première si absente).
    #[serde(default)]
    pub piste: Option<u32>,
}

/// Le refus d'un greffon : statut HTTP, motif stable, message lisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusSource {
    pub statut: u16,
    pub motif: String,
    pub message: String,
}

/// Ce qu'un greffon inscrit pour savoir JOUER ses sources.
#[async_trait]
pub trait JoueurSource: Send + Sync {
    /// Joue la source `id` sur la zone demandée. Rend le corps JSON de la
    /// réponse, ou le refus.
    async fn jouer(&self, id: &str, demande: DemandeJouer) -> Result<Value, RefusSource>;
}

/// Pourquoi `jouer` n'a pas été délégué, ou ce que le greffon a refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurJouer {
    /// Aucune source de cet identifiant (→ 404).
    Inconnue,
    /// La source existe, son greffon n'a inscrit aucun joueur (→ 409).
    NonJouable { greffon: String },
    /// Le greffon a refusé.
    Refus(RefusSource),
}

/// L'inscription est refusée : l'`id` appartient à un autre greffon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifiantPris {
    pub id: String,
    pub proprietaire: String,
}

struct Inscrite {
    source: Source,
    joueur: Option<Arc<dyn JoueurSource>>,
}

/// Le registre. Partagé (`Arc`) entre l'orchestrateur, les greffons et les
/// routes.
#[derive(Default)]
pub struct RegistreSources {
    sources: Mutex<BTreeMap<String, Inscrite>>,
    bus: RwLock<Option<Arc<EventBus>>>,
}

impl RegistreSources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Branche le bus sur lequel partira `sources.changed`. Sans bus (témoins
    /// de l'orchestrateur), le registre fonctionne et n'émet rien.
    pub fn brancher_bus(&self, bus: Arc<EventBus>) {
        if let Ok(mut b) = self.bus.write() {
            *b = Some(bus);
        }
    }

    /// Inscrit la source, ou la met à jour si son greffon la tient déjà.
    /// `joueur` remplace le précédent. Émet `sources.changed` si la source
    /// est nouvelle ou différente. Rend `Ok(true)` quand il y a eu changement.
    pub fn inscrire(
        &self,
        source: Source,
        joueur: Option<Arc<dyn JoueurSource>>,
    ) -> Result<bool, IdentifiantPris> {
        let Ok(mut m) = self.sources.lock() else {
            return Ok(false);
        };
        if let Some(existante) = m.get_mut(&source.id) {
            if existante.source.greffon != source.greffon {
                return Err(IdentifiantPris {
                    id: source.id,
                    proprietaire: existante.source.greffon.clone(),
                });
            }
            existante.joueur = joueur;
            if existante.source == source {
                return Ok(false);
            }
            existante.source = source;
        } else {
            m.insert(source.id.clone(), Inscrite { source, joueur });
        }
        self.annoncer(&m);
        Ok(true)
    }

    /// Met à jour une source DÉJÀ inscrite par ce greffon, en gardant son
    /// joueur. Rend `false` si elle n'existe pas (ou n'est pas à lui) ou si
    /// rien ne change.
    pub fn mettre_a_jour(&self, source: Source) -> bool {
        let Ok(mut m) = self.sources.lock() else {
            return false;
        };
        let Some(existante) = m.get_mut(&source.id) else {
            return false;
        };
        if existante.source.greffon != source.greffon || existante.source == source {
            return false;
        }
        existante.source = source;
        self.annoncer(&m);
        true
    }

    /// Retire la source `id` si elle appartient à `greffon`.
    pub fn retirer(&self, greffon: &str, id: &str) -> bool {
        let Ok(mut m) = self.sources.lock() else {
            return false;
        };
        if m.get(id).is_none_or(|e| e.source.greffon != greffon) {
            return false;
        }
        m.remove(id);
        self.annoncer(&m);
        true
    }

    /// Retire toutes les sources de `greffon` (arrêt du greffon). Un seul
    /// `sources.changed`, et aucun s'il n'en avait pas.
    pub fn retirer_greffon(&self, greffon: &str) -> usize {
        let Ok(mut m) = self.sources.lock() else {
            return 0;
        };
        let avant = m.len();
        m.retain(|_, e| e.source.greffon != greffon);
        let retirees = avant - m.len();
        if retirees > 0 {
            self.annoncer(&m);
        }
        retirees
    }

    /// Toutes les sources, triées par identifiant.
    pub fn lister(&self) -> Vec<Source> {
        self.sources
            .lock()
            .map(|m| Self::liste(&m))
            .unwrap_or_default()
    }

    pub fn source(&self, id: &str) -> Option<Source> {
        self.sources
            .lock()
            .ok()
            .and_then(|m| m.get(id).map(|e| e.source.clone()))
    }

    /// Délègue la lecture de `id` au greffon propriétaire.
    pub async fn jouer(&self, id: &str, demande: DemandeJouer) -> Result<Value, ErreurJouer> {
        let joueur = {
            let m = self.sources.lock().map_err(|_| ErreurJouer::Inconnue)?;
            let e = m.get(id).ok_or(ErreurJouer::Inconnue)?;
            e.joueur.clone().ok_or_else(|| ErreurJouer::NonJouable {
                greffon: e.source.greffon.clone(),
            })?
        };
        joueur.jouer(id, demande).await.map_err(ErreurJouer::Refus)
    }

    fn liste(m: &BTreeMap<String, Inscrite>) -> Vec<Source> {
        m.values().map(|e| e.source.clone()).collect()
    }

    /// Émis SOUS le verrou : deux changements concurrents partent dans
    /// l'ordre où ils ont été appliqués, et le dernier événement reçu dit
    /// toujours l'état courant.
    fn annoncer(&self, m: &BTreeMap<String, Inscrite>) {
        let bus = self.bus.read().ok().and_then(|b| b.clone());
        if let Some(bus) = bus {
            let liste = serde_json::to_value(Self::liste(m)).unwrap_or(Value::Array(vec![]));
            bus.emit_typed(EventType::SourcesChanged, liste);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::broadcast::error::TryRecvError;

    fn source(greffon: &str, id: &str, etat: EtatSource) -> Source {
        Source {
            id: id.into(),
            genre: TypeSource::Entree,
            greffon: greffon.into(),
            nom: format!("Source {id}"),
            etat,
            detail: json!({}),
        }
    }

    /// Les `sources.changed` reçus jusqu'ici.
    fn recus(rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>) -> Vec<Value> {
        let mut v = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) if e.event_type == "sources.changed" => v.push(e.data),
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return v,
                Err(TryRecvError::Lagged(_)) => {}
            }
        }
    }

    struct JoueurTemoin;
    #[async_trait]
    impl JoueurSource for JoueurTemoin {
        async fn jouer(&self, id: &str, d: DemandeJouer) -> Result<Value, RefusSource> {
            if d.zone_id < 0 {
                return Err(RefusSource {
                    statut: 409,
                    motif: "zone_refusee".into(),
                    message: "non".into(),
                });
            }
            Ok(json!({ "id": id, "zone_id": d.zone_id, "piste": d.piste }))
        }
    }

    #[test]
    fn la_forme_json_est_celle_du_contrat() {
        let s = Source {
            id: "cd".into(),
            genre: TypeSource::Cd,
            greffon: "cd".into(),
            nom: "Genesis — A Trick of the Tail".into(),
            etat: EtatSource::NonPrisEnCharge,
            detail: json!({}),
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({ "id": "cd", "type": "cd", "greffon": "cd",
                    "nom": "Genesis — A Trick of the Tail",
                    "etat": "non_pris_en_charge", "detail": {} })
        );
        assert_eq!(
            serde_json::to_value(EtatSource::AutorisationRefusee).unwrap(),
            "autorisation_refusee"
        );
    }

    /// Témoin : un greffon factice inscrit, met à jour puis retire sa
    /// source ; la liste suit, et `sources.changed` part UNE fois par vrai
    /// changement — jamais pour une réinscription à l'identique.
    #[test]
    fn un_evenement_par_vrai_changement() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let r = RegistreSources::new();
        r.brancher_bus(bus);

        assert_eq!(
            r.inscrire(source("factice", "entree:a", EtatSource::Silence), None),
            Ok(true)
        );
        assert_eq!(r.lister().len(), 1);
        let e = recus(&mut rx);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0][0]["etat"], "silence");

        // Réinscription et mise à jour à l'identique : rien.
        assert_eq!(
            r.inscrire(source("factice", "entree:a", EtatSource::Silence), None),
            Ok(false)
        );
        assert!(!r.mettre_a_jour(source("factice", "entree:a", EtatSource::Silence)));
        assert!(recus(&mut rx).is_empty());

        // Vrai changement d'état.
        assert!(r.mettre_a_jour(source("factice", "entree:a", EtatSource::Signal)));
        let e = recus(&mut rx);
        assert_eq!(e.len(), 1);
        assert_eq!(
            e[0],
            json!([source("factice", "entree:a", EtatSource::Signal)])
        );
        assert_eq!(r.lister()[0].etat, EtatSource::Signal);

        // Une seconde source : l'événement porte la liste COMPLÈTE.
        r.inscrire(source("factice", "entree:b", EtatSource::Signal), None)
            .unwrap();
        let e = recus(&mut rx);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].as_array().unwrap().len(), 2);

        // Retrait : la liste suit.
        assert!(r.retirer("factice", "entree:a"));
        assert!(!r.retirer("factice", "entree:a"));
        let e = recus(&mut rx);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0][0]["id"], "entree:b");
        assert_eq!(r.retirer_greffon("factice"), 1);
        assert_eq!(r.retirer_greffon("factice"), 0);
        let e = recus(&mut rx);
        assert_eq!(e, vec![json!([])]);
        assert!(r.lister().is_empty());
    }

    #[test]
    fn un_greffon_ne_touche_pas_la_source_d_un_autre() {
        let r = RegistreSources::new();
        r.inscrire(source("a", "x", EtatSource::Signal), None)
            .unwrap();
        assert_eq!(
            r.inscrire(source("b", "x", EtatSource::Vide), None),
            Err(IdentifiantPris {
                id: "x".into(),
                proprietaire: "a".into()
            })
        );
        assert!(!r.mettre_a_jour(source("b", "x", EtatSource::Vide)));
        assert!(!r.retirer("b", "x"));
        assert_eq!(r.retirer_greffon("b"), 0);
        assert_eq!(r.lister()[0].greffon, "a");
    }

    #[tokio::test]
    async fn jouer_delegue_au_greffon_ou_dit_pourquoi_pas() {
        let r = RegistreSources::new();
        let d = DemandeJouer {
            zone_id: 3,
            piste: Some(2),
        };
        assert_eq!(
            r.jouer("absente", d.clone()).await,
            Err(ErreurJouer::Inconnue)
        );

        r.inscrire(source("muet", "m", EtatSource::Signal), None)
            .unwrap();
        assert_eq!(
            r.jouer("m", d.clone()).await,
            Err(ErreurJouer::NonJouable {
                greffon: "muet".into()
            })
        );

        r.inscrire(
            source("factice", "f", EtatSource::Signal),
            Some(Arc::new(JoueurTemoin)),
        )
        .unwrap();
        assert_eq!(
            r.jouer("f", d).await,
            Ok(json!({ "id": "f", "zone_id": 3, "piste": 2 }))
        );
        // Une mise à jour garde le joueur.
        r.mettre_a_jour(source("factice", "f", EtatSource::Silence));
        let refus = r
            .jouer(
                "f",
                DemandeJouer {
                    zone_id: -1,
                    piste: None,
                },
            )
            .await;
        assert!(matches!(
            refus,
            Err(ErreurJouer::Refus(RefusSource { statut: 409, .. }))
        ));
    }
}
