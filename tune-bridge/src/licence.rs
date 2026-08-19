//! Le relais demande au cloud si un serveur a le droit de s'enregistrer.
//!
//! Le Cloud Relay est une fonction **premium**, et rien ne le verifiait ici :
//! le controle vivait dans `POST /cloud/bridge/enable`, cote SERVEUR —
//! c'est-a-dire sur la porte que l'utilisateur tient. Le relais acceptait
//! quiconque se presentait, avec pour seule limite un plafond de cent
//! serveurs. Et comme l'adresse du relais est un defaut code en dur dans le
//! client, tout Tune dont on active le pont s'y enregistre, licence ou pas.
//!
//! La verification s'appuie sur `GET /api/v1/bridge/eligible/{server_id}`
//! (site-mozaiklabs), qui repond d'apres `licenses.server_id` et `tier`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{info, warn};

/// Base du cloud. Surchargeable pour un environnement de recette.
pub const CLOUD_BASE_ENV: &str = "TUNE_CLOUD_BASE_URL";
/// Jeton de service. **Sans lui, aucune verification n'est possible.**
pub const CLOUD_TOKEN_ENV: &str = "TUNE_CLOUD_SERVICE_TOKEN";

const BASE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";

/// Duree de validite d'un verdict favorable.
///
/// Six heures : assez long pour ne pas marteler le cloud a chaque
/// reconnexion — un serveur derriere une connexion instable se reconnecte
/// souvent —, assez court pour qu'une licence expiree cesse d'ouvrir la porte
/// dans la journee.
const TTL_FAVORABLE: Duration = Duration::from_secs(6 * 3600);

/// Duree de validite d'un refus.
///
/// Quinze minutes seulement : quelqu'un qui vient d'acheter une licence ne
/// doit pas attendre six heures pour que le relais s'en apercoive. Un refus
/// coute peu a reverifier ; un accord indu coute plus cher.
const TTL_REFUS: Duration = Duration::from_secs(15 * 60);

/// Fenetre pendant laquelle un verdict favorable perime sert encore de secours
/// quand le cloud ne repond pas.
///
/// Sept jours. C'est le compromis central de ce module : refuser en cas de
/// panne du cloud couperait TOUS les utilisateurs premium des que
/// mozaiklabs.fr tousse ; accepter ouvrirait une porte qu'il suffirait
/// d'attendre. On accepte donc ceux qu'on a DEJA valides recemment, et on
/// refuse les inconnus.
const GRACE_PANNE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Autorise,
    Refuse(String),
}

#[derive(Clone)]
struct Entree {
    autorise: bool,
    /// Motif du refus, tel que le cloud l'a formule.
    motif: String,
    pose_a: Instant,
}

impl Entree {
    fn frais(&self) -> bool {
        let ttl = if self.autorise {
            TTL_FAVORABLE
        } else {
            TTL_REFUS
        };
        self.pose_a.elapsed() < ttl
    }

    fn utilisable_en_secours(&self) -> bool {
        self.autorise && self.pose_a.elapsed() < GRACE_PANNE
    }
}

/// Verificateur d'eligibilite, avec son cache.
pub struct Licences {
    cache: DashMap<String, Entree>,
    client: reqwest::Client,
    base: String,
    jeton: Option<String>,
}

impl Licences {
    pub fn depuis_environnement() -> Arc<Self> {
        let base = std::env::var(CLOUD_BASE_ENV)
            .ok()
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| BASE_PAR_DEFAUT.to_string());
        let jeton = std::env::var(CLOUD_TOKEN_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        if jeton.is_none() {
            // Dit HAUT et fort, parce que le relais tourne alors sans controle.
            // Refuser tout le monde serait pire : un deploiement qui oublie la
            // variable mettrait tous les utilisateurs premium dehors, et le
            // symptome ne pointerait pas vers la cause.
            warn!(
                "bridge_eligibilite_non_appliquee — {CLOUD_TOKEN_ENV} absent : \
                 tout serveur qui se presente sera accepte, premium ou non"
            );
        } else {
            info!(base = %base, "bridge_eligibilite_active");
        }

        Arc::new(Self {
            cache: DashMap::new(),
            client: reqwest::Client::builder()
                // Court : ce contrôle est sur le chemin d'un enregistrement.
                // Mieux vaut basculer sur le cache de secours que faire
                // attendre un serveur qui se reconnecte.
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            base,
            jeton,
        })
    }

    /// Ce serveur peut-il s'enregistrer ?
    pub async fn verifier(&self, server_id: &str) -> Verdict {
        // Sans jeton de service, aucune verification n'est possible : on laisse
        // passer plutot que de tout bloquer. L'avertissement au demarrage dit
        // que le relais tourne sans controle.
        let Some(jeton) = self.jeton.as_deref() else {
            return Verdict::Autorise;
        };

        if let Some(e) = self.cache.get(server_id)
            && e.frais()
        {
            return if e.autorise {
                Verdict::Autorise
            } else {
                Verdict::Refuse(e.motif.clone())
            };
        }

        let url = format!("{}/api/v1/bridge/eligible/{}", self.base, server_id);
        let reponse = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {jeton}"))
            .send()
            .await;

        match reponse {
            Ok(r) if r.status().is_success() => {
                let corps: serde_json::Value = r.json().await.unwrap_or_default();
                let autorise = corps
                    .get("eligible")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let motif = corps
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("not_eligible")
                    .to_string();
                self.cache.insert(
                    server_id.to_string(),
                    Entree {
                        autorise,
                        motif: motif.clone(),
                        pose_a: Instant::now(),
                    },
                );
                if autorise {
                    Verdict::Autorise
                } else {
                    Verdict::Refuse(motif)
                }
            }
            // Cloud joignable mais fâché (500, 401…) : on ne sait pas, donc on
            // applique la même règle qu'une panne — le secours, ou le refus.
            Ok(r) => {
                warn!(status = %r.status(), "bridge_eligibilite_reponse_inattendue");
                self.secours(server_id)
            }
            Err(e) => {
                warn!(error = %e, "bridge_eligibilite_cloud_injoignable");
                self.secours(server_id)
            }
        }
    }

    /// Cloud indisponible : on accepte ceux qu'on a déjà validés récemment, et
    /// on refuse les inconnus. Une panne ne doit couper personne d'installé,
    /// ni laisser entrer qui que ce soit de nouveau.
    fn secours(&self, server_id: &str) -> Verdict {
        match self.cache.get(server_id) {
            Some(e) if e.utilisable_en_secours() => {
                info!(server_id, "bridge_eligibilite_secours_cache");
                Verdict::Autorise
            }
            _ => Verdict::Refuse("cloud_unreachable".to_string()),
        }
    }

    /// Pose une entrée de cache — pour les tests.
    #[cfg(test)]
    fn poser(&self, server_id: &str, autorise: bool, age: Duration) {
        self.cache.insert(
            server_id.to_string(),
            Entree {
                autorise,
                motif: "test".into(),
                pose_a: Instant::now() - age,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sans_jeton() -> Licences {
        Licences {
            cache: DashMap::new(),
            client: reqwest::Client::new(),
            base: "http://exemple.invalide".into(),
            jeton: None,
        }
    }

    fn avec_jeton() -> Licences {
        Licences {
            cache: DashMap::new(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .unwrap(),
            // Domaine reserve, garanti injoignable : le test porte sur le
            // comportement en panne, pas sur le reseau.
            base: "http://cloud.invalide".into(),
            jeton: Some("jeton".into()),
        }
    }

    /// Un deploiement qui oublie la variable ne doit pas mettre dehors tous les
    /// utilisateurs premium : le symptome ne pointerait pas vers la cause.
    #[tokio::test]
    async fn sans_jeton_de_service_tout_le_monde_passe() {
        assert_eq!(sans_jeton().verifier("abc").await, Verdict::Autorise);
    }

    /// Le cœur du compromis : une panne du cloud ne coupe pas ceux qu'on a
    /// déjà validés.
    #[tokio::test]
    async fn en_panne_un_serveur_deja_valide_passe() {
        let l = avec_jeton();
        l.poser("connu", true, Duration::from_secs(3 * 24 * 3600));
        assert_eq!(l.verifier("connu").await, Verdict::Autorise);
    }

    /// …mais elle ne laisse pas entrer un inconnu. Sinon il suffirait
    /// d'attendre une panne.
    #[tokio::test]
    async fn en_panne_un_inconnu_est_refuse() {
        let l = avec_jeton();
        assert_eq!(
            l.verifier("inconnu").await,
            Verdict::Refuse("cloud_unreachable".into())
        );
    }

    /// Un accord trop vieux ne sert plus de secours : sept jours, pas plus.
    #[tokio::test]
    async fn en_panne_un_accord_trop_vieux_ne_secourt_plus() {
        let l = avec_jeton();
        l.poser("vieux", true, Duration::from_secs(8 * 24 * 3600));
        assert_eq!(
            l.verifier("vieux").await,
            Verdict::Refuse("cloud_unreachable".into())
        );
    }

    /// Un refus n'est JAMAIS secouru : le cache de secours ne vaut que pour
    /// les accords.
    #[tokio::test]
    async fn en_panne_un_refus_reste_un_refus() {
        let l = avec_jeton();
        l.poser("refuse", false, Duration::from_secs(60));
        // Frais : le refus en cache repond directement.
        assert!(matches!(l.verifier("refuse").await, Verdict::Refuse(_)));
    }

    #[test]
    fn un_accord_frais_le_reste_six_heures() {
        let e = Entree {
            autorise: true,
            motif: String::new(),
            pose_a: Instant::now() - Duration::from_secs(5 * 3600),
        };
        assert!(e.frais());
        let vieux = Entree {
            pose_a: Instant::now() - Duration::from_secs(7 * 3600),
            ..e.clone()
        };
        assert!(!vieux.frais());
    }

    /// Quelqu'un qui vient d'acheter une licence ne doit pas attendre six
    /// heures : un refus se reverifie au bout de quinze minutes.
    #[test]
    fn un_refus_se_reverifie_en_quinze_minutes() {
        let e = Entree {
            autorise: false,
            motif: "not_premium".into(),
            pose_a: Instant::now() - Duration::from_secs(10 * 60),
        };
        assert!(e.frais());
        let perime = Entree {
            pose_a: Instant::now() - Duration::from_secs(20 * 60),
            ..e.clone()
        };
        assert!(!perime.frais());
    }
}
