//! Tune Circle T3 (#5326) : tenir à jour les rayons partagés.
//!
//! Décision de Bertrand du 28/09/2026 : envoi **à chaque changement** d'une
//! étiquette ou d'une collection partagée, et un battement de 5 minutes en
//! filet. Trois déclencheurs, un seul chemin ([`Pousseur::repousser`]) :
//!
//! * **le battement** (5 min) : la liste des ensembles cochés est relue chez
//!   le cloud (`GET /sets`, source de vérité — un ensemble décoché ailleurs,
//!   un cercle supprimé n'y sont plus), chacun est résolu, et n'est repoussé
//!   que si son empreinte diffère de celle que le cloud a gardée ;
//! * **le bus** : `library.updated` et `library.scan.completed` (la
//!   bibliothèque a changé, une collection peut avoir gagné un album) réveillent
//!   un tour complet, après 10 s de calme pour grouper une rafale ;
//! * **la définition** : toutes les 30 s, sans réseau, chaque ensemble connu
//!   est relu localement — une étiquette résolue (quelques lignes), une
//!   collection seulement par son nom et ses règles. Si l'un a changé, un
//!   tour complet part (et relit `GET /sets` : rien n'est poussé de mémoire).
//!   Aucun évènement du bus ne dit « étiquette posée » ni
//!   « règle modifiée » : c'est ce tour qui tient la promesse « à chaque
//!   changement » pour eux.
//!
//! Ce qui est gardé en mémoire, et seulement là : la liste des ensembles de CE
//! serveur (cercle, genre, identifiant local, profil, empreinte poussée). Elle
//! n'est jamais servie à personne ; elle ne sert qu'à savoir quoi relire.
//! Chaque envoi part par le relais, avec la session SSO relue à chaque appel :
//! un contact révoqué ou un cercle supprimé sont jugés par le cloud seul.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::ensembles::{self, GENRE_COLLECTION, GENRE_ETIQUETTE, Hote};
use crate::relais::{Issue, Relais};

/// Le filet : un tour complet au plus tard toutes les 5 minutes.
pub const BATTEMENT: Duration = Duration::from_secs(300);
/// La relecture locale des définitions.
pub const TOUR_DES_DEFINITIONS: Duration = Duration::from_secs(30);
/// Le calme attendu après un évènement du bus avant le tour complet.
pub const CALME_APRES_EVENEMENT: Duration = Duration::from_secs(10);

/// Les évènements du bus qui réveillent un tour complet.
pub const EVENEMENTS_QUI_REVEILLENT: [&str; 2] = ["library.updated", "library.scan.completed"];

/// Un ensemble de CE serveur, tel que le cloud l'a gardé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsembleConnu {
    pub circle_id: i64,
    pub kind: String,
    pub source_id: i64,
    pub profile_id: Option<i64>,
    /// L'empreinte du dernier envoi, telle que le cloud la rend.
    pub digest: Option<String>,
    /// Pour une collection : l'empreinte de sa définition au dernier tour.
    pub definition: Option<String>,
}

/// Ce qu'a donné un passage, pour le journal et les bancs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bilan {
    pub examines: usize,
    pub envoyes: usize,
    pub retires: usize,
    pub echecs: usize,
}

pub struct Pousseur {
    relais: Arc<Relais>,
    hote: Arc<dyn Hote>,
    reveil: Notify,
    connus: Mutex<Vec<EnsembleConnu>>,
}

fn entier(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Les ensembles de `GET /sets` (liste nue ou `{ "data": […] }`) qui
/// appartiennent au serveur `server_id`.
pub fn ensembles_du_serveur(corps: &[u8], server_id: &str) -> Option<Vec<EnsembleConnu>> {
    let v: Value = serde_json::from_slice(corps).ok()?;
    let liste = v.as_array().or_else(|| v.get("data")?.as_array())?;
    Some(
        liste
            .iter()
            .filter(|e| e["server_id"].as_str() == Some(server_id))
            .filter_map(|e| {
                let kind = e["kind"].as_str()?.to_string();
                if !ensembles::genre_valide(&kind) {
                    return None;
                }
                Some(EnsembleConnu {
                    circle_id: entier(&e["circle_id"])?,
                    kind,
                    source_id: entier(&e["source_id"])?,
                    profile_id: entier(&e["profile_id"]),
                    digest: e["digest"].as_str().map(str::to_string),
                    definition: None,
                })
            })
            .collect(),
    )
}

impl Pousseur {
    pub fn new(relais: Arc<Relais>, hote: Arc<dyn Hote>) -> Self {
        Self {
            relais,
            hote,
            reveil: Notify::new(),
            connus: Mutex::new(Vec::new()),
        }
    }

    pub fn relais(&self) -> &Arc<Relais> {
        &self.relais
    }

    pub fn hote(&self) -> &Arc<dyn Hote> {
        &self.hote
    }

    /// Réveille un tour complet (évènement du bus). Ne bloque jamais.
    pub fn reveiller(&self) {
        self.reveil.notify_one();
    }

    /// Les ensembles connus, pour les bancs.
    pub fn connus(&self) -> Vec<EnsembleConnu> {
        self.connus.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn server_id(&self) -> Option<String> {
        self.relais
            .reglages()
            .get("server_id")
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Note (ou remplace) un ensemble après un envoi réussi.
    pub fn noter(&self, e: EnsembleConnu) {
        if let Ok(mut connus) = self.connus.lock() {
            connus.retain(|c| {
                !(c.circle_id == e.circle_id && c.kind == e.kind && c.source_id == e.source_id)
            });
            connus.push(e);
        }
    }

    /// Oublie un ensemble décoché.
    pub fn oublier(&self, circle_id: i64, kind: &str, source_id: i64) {
        if let Ok(mut connus) = self.connus.lock() {
            connus.retain(|c| {
                !(c.circle_id == circle_id && c.kind == kind && c.source_id == source_id)
            });
        }
    }

    /// Résout l'ensemble et le pousse si son contenu diffère de `digest`.
    /// Rend l'ensemble tel qu'il faut désormais le connaître, `None` s'il est
    /// retiré (l'étiquette ou la collection n'existe plus) ou en échec.
    async fn repousser(&self, e: &EnsembleConnu, bilan: &mut Bilan) -> Option<EnsembleConnu> {
        bilan.examines += 1;
        let backend = self.relais.backend().clone();
        // Une étiquette est globale au serveur : le profil ne la change pas,
        // il est seulement rendu tel que le cloud l'a gardé.
        let profil = match (e.kind.as_str(), e.profile_id) {
            (GENRE_ETIQUETTE, p) => p,
            (GENRE_COLLECTION, Some(p)) => Some(p),
            // Sans le profil qui l'a cochée, la collection ne se résout pas :
            // on n'en devine pas un autre. Le dernier envoi reste.
            _ => {
                debug!(kind = %e.kind, "circle_ensemble_sans_profil");
                return Some(e.clone());
            }
        };
        let mut connu = e.clone();
        if e.kind == GENRE_COLLECTION {
            connu.definition = ensembles::definition_de_collection(&backend, e.source_id);
        }
        let membres = match ensembles::resoudre(
            &backend,
            &*self.hote,
            &e.kind,
            e.source_id,
            profil.unwrap_or_default(),
        )
        .await
        {
            Ok(Some(m)) => m,
            Ok(None) => {
                // Supprimée en local : ce qui n'existe plus ne reste pas
                // partagé avec son ancien contenu.
                let id = e.circle_id.to_string();
                let source = e.source_id.to_string();
                let issue = self
                    .relais
                    .appeler(
                        "DELETE /circles/{id}/sets/{kind}/{source_id}",
                        Method::DELETE,
                        &["circles", &id, "sets", &e.kind, &source],
                        None,
                    )
                    .await;
                if reussie_ou_absente(&issue) {
                    bilan.retires += 1;
                    return None;
                }
                bilan.echecs += 1;
                return Some(e.clone());
            }
            Err(motif) => {
                warn!(kind = %e.kind, error = %motif, "circle_ensemble_non_resolu");
                bilan.echecs += 1;
                return Some(connu);
            }
        };
        let server_id = self.server_id();
        let corps = ensembles::corps_du_partage(&membres, server_id.as_deref(), profil);
        let digest = corps["digest"].as_str().map(str::to_string);
        if digest.is_some() && digest == e.digest {
            return Some(connu);
        }
        let id = e.circle_id.to_string();
        let source = e.source_id.to_string();
        let issue = self
            .relais
            .appeler(
                "PUT /circles/{id}/sets/{kind}/{source_id}",
                Method::PUT,
                &["circles", &id, "sets", &e.kind, &source],
                Some(&corps),
            )
            .await;
        match issue {
            Issue::Reponse { statut, .. } if (200..300).contains(&statut) => {
                bilan.envoyes += 1;
                connu.digest = digest;
                Some(connu)
            }
            // Cercle supprimé, partage de bibliothèque coupé ou déplacé : le
            // cloud ne le garde plus, on l'oublie aussi.
            Issue::Reponse {
                statut: 404 | 409, ..
            } => {
                bilan.retires += 1;
                None
            }
            autre => {
                debug!(issue = ?statut_de(&autre), "circle_ensemble_non_envoye");
                bilan.echecs += 1;
                Some(e.clone())
            }
        }
    }

    /// Le tour complet : relit chez le cloud les ensembles de CE serveur,
    /// résout chacun et pousse ce qui a changé.
    pub async fn tour_complet(&self) -> Bilan {
        let mut bilan = Bilan::default();
        let Some(server_id) = self.server_id() else {
            return bilan;
        };
        let issue = self
            .relais
            .appeler("GET /sets", Method::GET, &["sets"], None)
            .await;
        let Issue::Reponse {
            statut: 200, corps, ..
        } = issue
        else {
            return bilan;
        };
        let Some(liste) = ensembles_du_serveur(&corps, &server_id) else {
            return bilan;
        };
        let mut gardes = Vec::with_capacity(liste.len());
        for e in &liste {
            if let Some(k) = self.repousser(e, &mut bilan).await {
                gardes.push(k);
            }
        }
        if let Ok(mut connus) = self.connus.lock() {
            *connus = gardes;
        }
        if bilan.envoyes + bilan.retires + bilan.echecs > 0 {
            info!(
                examines = bilan.examines,
                envoyes = bilan.envoyes,
                retires = bilan.retires,
                echecs = bilan.echecs,
                "circle_ensembles_tour_complet"
            );
        }
        bilan
    }

    /// Le tour des définitions, sans réseau tant que rien n'a changé : une
    /// étiquette est résolue et comparée à l'empreinte envoyée ; une
    /// collection n'est comparée que par son nom et ses règles.
    ///
    /// Il ne POUSSE rien lui-même. Dès qu'un ensemble connu a changé, il passe
    /// la main au tour complet, qui relit `GET /sets` d'abord : c'est la seule
    /// vérité sur ce qui est coché. Couper le partage d'un cercle, le déplacer
    /// ou délier le serveur y SUPPRIME ses ensembles ; un envoi fait de
    /// mémoire les recréerait (le `PUT` crée ce qu'il ne trouve pas) et
    /// recocherait, au rallumage, des cases que personne n'a cochées.
    pub async fn tour_des_definitions(&self) -> Bilan {
        let backend = self.relais.backend().clone();
        let a_change = |e: &EnsembleConnu| match e.kind.as_str() {
            GENRE_ETIQUETTE => match ensembles::resoudre_etiquette(&backend, e.source_id) {
                Ok(Some(m)) => Some(ensembles::empreinte(&ensembles::contenu(&m))) != e.digest,
                Ok(None) => true,
                Err(_) => false,
            },
            GENRE_COLLECTION => {
                ensembles::definition_de_collection(&backend, e.source_id) != e.definition
            }
            _ => false,
        };
        if self.connus().iter().any(a_change) {
            self.tour_complet().await
        } else {
            Bilan::default()
        }
    }

    /// La boucle de fond. Premier tour complet une minute après le départ.
    pub async fn tourner(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut dernier_complet = tokio::time::Instant::now();
        self.tour_complet().await;
        loop {
            let reveille = tokio::select! {
                _ = tokio::time::sleep(TOUR_DES_DEFINITIONS) => false,
                _ = self.reveil.notified() => true,
            };
            if reveille {
                tokio::time::sleep(CALME_APRES_EVENEMENT).await;
            }
            if reveille || dernier_complet.elapsed() >= BATTEMENT {
                dernier_complet = tokio::time::Instant::now();
                self.tour_complet().await;
            } else {
                self.tour_des_definitions().await;
            }
        }
    }
}

fn reussie_ou_absente(issue: &Issue) -> bool {
    matches!(issue, Issue::Reponse { statut, .. } if (200..300).contains(statut) || *statut == 404)
}

fn statut_de(issue: &Issue) -> Option<u16> {
    match issue {
        Issue::Reponse { statut, .. } => Some(*statut),
        Issue::Indisponible { statut_amont } => *statut_amont,
        Issue::NonConnecte => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seuls_les_ensembles_de_ce_serveur_sont_repris() {
        let corps = serde_json::json!([
            { "id": 1, "circle_id": 3, "kind": "tag", "source_id": 5, "server_id": "moi",
              "profile_id": 2, "digest": "v1:aa" },
            { "id": 2, "circle_id": 3, "kind": "tag", "source_id": 6, "server_id": "autre" },
            { "id": 3, "circle_id": 4, "kind": "playlist", "source_id": 7, "server_id": "moi" }
        ])
        .to_string();
        let l = ensembles_du_serveur(corps.as_bytes(), "moi").unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].source_id, 5);
        assert_eq!(l[0].profile_id, Some(2));
        assert_eq!(l[0].digest.as_deref(), Some("v1:aa"));
    }
}
