//! Étape 2 — ce que l'HÔTE emprunte à l'assistant : son coffre, son plafond,
//! son fournisseur.
//!
//! Deux usages anciens vivent hors du greffon et y restent :
//!
//! - l'ancien assistant conversationnel `POST /ai/query`
//!   (`tune-server/src/routes/ai.rs`), qui pilote la lecture par des OUTILS
//!   (tool use) — écrit pour l'API Messages d'Anthropic ; étape 3 : traduit
//!   pour tous les fournisseurs de l'assistant par [`crate::dialogue`] ;
//! - la traduction des requêtes d'ambiance (`tune_core::ai::translate`),
//!   appelée par la recherche acoustique.
//!
//! Ils lisaient une clé EN CLAIR (`anthropic_api_key`…) et ne connaissaient
//! aucun plafond. Ils passent désormais par une [`Passerelle`] : la clé vient
//! du coffre (après [`crate::migration`]), chaque appel est compté au tarif de
//! la table et refusé plafond atteint, sous LE verrou de consommation du
//! processus ([`crate::appel::verrou_de_consommation`]) — celui que tient
//! aussi le greffon.
//!
//! La passerelle n'a pas de cache : la traduction a le sien
//! (`ambiance_query_translations`), et une conversation ne se rejoue pas.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;

use crate::appel::{self, ErreurAppel, Moyens};
use crate::coffre::Coffre;
use crate::fournisseur::{self, Fabrique, Famille};
use crate::migration;
use crate::reglages::{self, Reglages};

/// L'usage compté pour l'ancien assistant conversationnel.
pub const USAGE_REQUETE: &str = "requete";
/// L'usage compté pour la traduction des ambiances.
pub const USAGE_TRADUCTION: &str = "traduction";

#[derive(Clone)]
pub struct Passerelle {
    backend: Arc<dyn DbBackend>,
    coffre: Arc<Coffre>,
    fabrique: Fabrique,
    verrou: Arc<tokio::sync::Mutex<()>>,
}

impl Passerelle {
    /// La passerelle de production : la clé maîtresse dans `dossier_secret`
    /// (le dossier de la base, comme pour le greffon), les vrais fournisseurs.
    pub fn new(backend: Arc<dyn DbBackend>, dossier_secret: &Path) -> Self {
        Self::avec_fabrique(backend, dossier_secret, fournisseur::fabrique_reelle())
    }

    /// Pour les essais : un faux fournisseur.
    pub fn avec_fabrique(
        backend: Arc<dyn DbBackend>,
        dossier_secret: &Path,
        fabrique: Fabrique,
    ) -> Self {
        Self {
            backend,
            coffre: Arc::new(crate::coffre::du_greffon(dossier_secret)),
            fabrique,
            verrou: appel::verrou_de_consommation(),
        }
    }

    fn moyens(&self) -> Moyens<'_> {
        Moyens {
            backend: &self.backend,
            coffre: &self.coffre,
            fabrique: &self.fabrique,
            verrou: &self.verrou,
            cache: None,
        }
    }

    /// Migre les clés héritées vers le coffre. Un échec est journalisé (sans
    /// la clé) et laisse la clé en clair à sa place : il sera retenté.
    ///
    /// Puis fait passer au coffre partagé ce que l'ancienne clé du greffon a
    /// scellé ([`migration::passer_au_coffre_partage`]) ; un échec y laisse
    /// l'ancienne valeur, que l'ancienne clé ouvre toujours.
    pub fn migrer(&self) -> migration::Rapport {
        let rapport = match migration::migrer(&self.backend, &self.coffre) {
            Ok(r) => r,
            Err(e) => {
                warn!(erreur = %e, "assistant_ia_migration_echouee");
                migration::Rapport::default()
            }
        };
        if let Err(e) = migration::passer_au_coffre_partage(&self.backend, &self.coffre) {
            warn!(erreur = %e, "assistant_ia_passage_au_coffre_partage_echoue");
        }
        rapport
    }

    /// L'assistant peut-il faire partir un appel (fournisseur, modèle, clé
    /// qui s'ouvre, tarif connu) ? Le plafond n'est pas regardé ici.
    pub fn pret(&self) -> bool {
        let r = Reglages::charger(&self.backend);
        let modele = r.modele_effectif(false);
        r.famille.is_some()
            && !modele.is_empty()
            && appel::acces_et_tarif(&self.backend, &self.coffre, &r, &modele).is_ok()
    }

    /// L'écran Ambiance peut-il promettre une traduction ? Oui si l'assistant
    /// est prêt, ou si une clé héritée attend sa migration (elle aura lieu au
    /// premier appel). Lecture seule : rien n'est migré ici.
    pub fn traduction_disponible(&self) -> bool {
        self.pret() || migration::reste_une_cle_heritee(&self.backend)
    }

    /// Traduit une requête d'ambiance par le fournisseur de l'assistant.
    /// `None` (l'appelant garde la requête brute) si l'assistant n'est pas
    /// prêt, plafond atteint, ou en cas d'échec.
    pub async fn traduire(&self, consigne: &str, requete: &str) -> Option<String> {
        self.migrer();
        match appel::appeler_avec(
            &self.moyens(),
            USAGE_TRADUCTION,
            false,
            consigne,
            requete,
            100,
            None,
            false,
        )
        .await
        {
            Ok(r) => Some(r.texte),
            Err(e) => {
                info!(code = e.code(), "assistant_ia_traduction_non_faite");
                None
            }
        }
    }

    /// L'ancien assistant peut-il dialoguer ? Étape 3 : avec tout
    /// fournisseur de l'assistant prêt (Anthropic, compatible OpenAI, Gemini,
    /// Ollama) — le dialogue à outils est traduit par [`crate::dialogue`].
    pub fn dialogue_pret(&self) -> bool {
        self.pret()
    }

    /// Le dialogue avec Anthropic seulement (étape 2). Gardé pour qui le
    /// demande explicitement ; l'hôte passe par [`Self::dialogue_pret`].
    pub fn anthropic_pret(&self) -> bool {
        Reglages::charger(&self.backend).famille == Some(Famille::Anthropic) && self.pret()
    }

    /// Où en est la dépense du mois face au plafond (étape 3), pour la
    /// réponse de `/ai/query`.
    pub fn etat_plafond(&self) -> Value {
        appel::etat_plafond(&self.backend)
    }

    /// Un tour de dialogue de l'ancien assistant, chez le fournisseur de
    /// l'assistant QUEL QU'IL SOIT, avec ses outils, au tarif et sous le
    /// plafond de l'assistant, compté pour l'usage [`USAGE_REQUETE`].
    ///
    /// `messages` et `outils` sont dans la forme de l'API Messages
    /// d'Anthropic, et la réponse aussi (`content` en blocs `text` /
    /// `tool_use`, `stop_reason`) : l'hôte n'a qu'un dialogue à écrire.
    pub async fn discuter(
        &self,
        systeme: &str,
        messages: &Value,
        outils: &Value,
        max_jetons: u32,
    ) -> Result<Value, ErreurAppel> {
        self.discuter_compte(systeme, messages, outils, max_jetons)
            .await
            .map(|(v, _)| v)
    }

    /// [`Self::discuter`], avec le coût EXACT du tour en micro-euros : celui
    /// qui vient d'être compté pour l'usage [`USAGE_REQUETE`] (même tarif
    /// daté, même taux). L'hôte en fait la somme pour le `cout_eur` d'une
    /// question.
    pub async fn discuter_compte(
        &self,
        systeme: &str,
        messages: &Value,
        outils: &Value,
        max_jetons: u32,
    ) -> Result<(Value, u64), ErreurAppel> {
        use crate::dialogue::{self, Dialecte};
        let famille = Reglages::charger(&self.backend)
            .famille
            .ok_or(ErreurAppel::NonConfigure)?;
        match famille {
            Famille::Anthropic => {
                self.discuter_anthropic_compte(systeme, messages, outils, max_jetons)
                    .await
            }
            Famille::CompatibleOpenai | Famille::Gemini => {
                let (v, cout) = self
                    .tour(
                        |acces, modele| {
                            let mut corps = json!({
                                "model": modele,
                                "messages": dialogue::messages_vers(Dialecte::Openai, systeme, messages),
                            });
                            let fonctions = dialogue::outils_en_fonctions(outils);
                            if !fonctions.is_empty() {
                                corps["tools"] = json!(fonctions);
                            }
                            // Pas de plafond de sortie : voir `CompatibleOpenai`
                            // dans `fournisseur` (clé refusée par certains
                            // points d'accès). La sortie est comptée sur `usage`.
                            let mut req = fournisseur::client()
                                .post(format!("{}/chat/completions", fournisseur::base(acces)))
                                .timeout(fournisseur::DELAI)
                                .json(&corps);
                            if let Some(cle) = acces.cle.as_deref() {
                                req = req.bearer_auth(cle);
                            }
                            req
                        },
                        |v| {
                            let u = v.get("usage").cloned().unwrap_or(Value::Null);
                            (
                                fournisseur::jetons(&u, "prompt_tokens"),
                                fournisseur::jetons(&u, "completion_tokens"),
                            )
                        },
                    )
                    .await?;
                Ok((
                    dialogue::reponse_depuis(
                        v.pointer("/choices/0/message").unwrap_or(&Value::Null),
                    ),
                    cout,
                ))
            }
            Famille::Ollama => {
                let (v, cout) = self
                    .tour(
                        |acces, modele| {
                            let mut corps = json!({
                                "model": modele,
                                "stream": false,
                                "messages": dialogue::messages_vers(Dialecte::Ollama, systeme, messages),
                            });
                            let fonctions = dialogue::outils_en_fonctions(outils);
                            if !fonctions.is_empty() {
                                corps["tools"] = json!(fonctions);
                            }
                            fournisseur::client()
                                .post(format!("{}/api/chat", fournisseur::base(acces)))
                                .timeout(fournisseur::DELAI)
                                .json(&corps)
                        },
                        |v| {
                            (
                                fournisseur::jetons(v, "prompt_eval_count"),
                                fournisseur::jetons(v, "eval_count"),
                            )
                        },
                    )
                    .await?;
                Ok((dialogue::reponse_depuis(&v["message"]), cout))
            }
        }
    }

    /// Un tour de dialogue avec l'API Messages d'Anthropic. Rend la réponse
    /// brute (`content`, `stop_reason`, `usage`).
    pub async fn discuter_anthropic(
        &self,
        systeme: &str,
        messages: &Value,
        outils: &Value,
        max_jetons: u32,
    ) -> Result<Value, ErreurAppel> {
        self.discuter_anthropic_compte(systeme, messages, outils, max_jetons)
            .await
            .map(|(v, _)| v)
    }

    /// [`Self::discuter_anthropic`], avec le coût exact du tour (micro-euros).
    async fn discuter_anthropic_compte(
        &self,
        systeme: &str,
        messages: &Value,
        outils: &Value,
        max_jetons: u32,
    ) -> Result<(Value, u64), ErreurAppel> {
        if Reglages::charger(&self.backend).famille != Some(Famille::Anthropic) {
            return Err(ErreurAppel::NonConfigure);
        }
        self.tour(
            |acces, modele| {
                let mut corps = json!({
                    "model": modele,
                    "max_tokens": max_jetons,
                    "system": systeme,
                    "messages": messages,
                });
                if outils.as_array().is_some_and(|o| !o.is_empty()) {
                    corps["tools"] = outils.clone();
                }
                fournisseur::entetes_anthropic(
                    fournisseur::client().post(format!("{}/v1/messages", fournisseur::base(acces))),
                    acces,
                )
                .timeout(fournisseur::DELAI)
                .json(&corps)
            },
            |v| {
                let u = v.get("usage").cloned().unwrap_or(Value::Null);
                (
                    fournisseur::jetons(&u, "input_tokens")
                        + fournisseur::jetons(&u, "cache_creation_input_tokens")
                        + fournisseur::jetons(&u, "cache_read_input_tokens"),
                    fournisseur::jetons(&u, "output_tokens"),
                )
            },
        )
        .await
    }

    /// Le chemin commun d'un tour : clé et tarif, plafond sous LE verrou,
    /// la requête (`construire`), puis le comptage de SES jetons (`jetons`)
    /// pour l'usage [`USAGE_REQUETE`], dont le coût (micro-euros) est rendu
    /// avec la réponse. Seuls la famille et le statut HTTP sont journalisés,
    /// jamais la requête ni la clé.
    async fn tour(
        &self,
        construire: impl FnOnce(&fournisseur::Acces, &str) -> reqwest::RequestBuilder,
        jetons: impl FnOnce(&Value) -> (u64, u64),
    ) -> Result<(Value, u64), ErreurAppel> {
        let r = Reglages::charger(&self.backend);
        let famille = r.famille.ok_or(ErreurAppel::NonConfigure)?;
        let modele = r.modele_effectif(false);
        if modele.is_empty() {
            return Err(ErreurAppel::NonConfigure);
        }
        let (acces, tarif) = appel::acces_et_tarif(&self.backend, &self.coffre, &r, &modele)?;

        let _verrou = self.verrou.lock().await;
        let mut c = reglages::charger_consommation(&self.backend);
        if c.micro_eur >= r.plafond_micro_eur {
            info!(usage = USAGE_REQUETE, "assistant_ia_plafond_atteint");
            return Err(ErreurAppel::PlafondAtteint);
        }
        let rep = construire(&acces, &modele)
            .send()
            .await
            .map_err(|e| ErreurAppel::Fournisseur(fournisseur::erreur_reseau(famille, &e)))?;
        let statut = rep.status().as_u16();
        let texte = rep
            .text()
            .await
            .map_err(|e| ErreurAppel::Fournisseur(fournisseur::erreur_reseau(famille, &e)))?;
        if !(200..300).contains(&statut) {
            return Err(ErreurAppel::Fournisseur(fournisseur::erreur_de_statut(
                famille, statut, &texte,
            )));
        }
        let v: Value = serde_json::from_str(&texte).map_err(|_| {
            ErreurAppel::Fournisseur(fournisseur::ErreurFournisseur::corps_illisible(statut))
        })?;
        let (entree, sortie) = jetons(&v);
        let cout = appel::compter(
            &self.backend,
            &mut c,
            &r,
            tarif,
            USAGE_REQUETE,
            &modele,
            entree,
            sortie,
        );
        Ok((v, cout))
    }
}
