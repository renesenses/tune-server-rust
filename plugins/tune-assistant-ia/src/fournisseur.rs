//! Les fournisseurs de modèles, derrière UNE interface.
//!
//! Trois familles, une implémentation chacune :
//!
//! - **Anthropic** (Claude) : `POST {base}/v1/messages` ;
//! - **compatible OpenAI** : `POST {base}/chat/completions` — OpenAI, Mistral
//!   et tout point d'accès qui parle ce dialecte ;
//! - **Google Gemini** : un PRÉRÉGLAGE de la famille compatible OpenAI —
//!   l'adresse et le modèle sont posés, l'utilisateur ne colle que sa clé
//!   Google AI Studio (voir [`Famille::Gemini`]) ;
//! - **Ollama**, en local : `POST {base}/api/chat`, sans clé.
//!
//! # La clé ne sort d'ici que dans l'en-tête de la requête
//!
//! Aucune ligne de ce module ne journalise une requête, un en-tête ou le corps
//! d'une réponse d'erreur : seulement le statut HTTP et la famille. Une erreur
//! de `reqwest` est réduite à sa nature (délai, connexion) avant de remonter,
//! parce que son texte peut contenir l'URL — et un point d'accès « compatible »
//! mal configuré peut porter un jeton dans son URL.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

/// La famille d'un fournisseur.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Famille {
    Anthropic,
    CompatibleOpenai,
    /// Google Gemini, par son point d'accès compatible OpenAI.
    ///
    /// Relevé le 12/10/2026 dans la documentation officielle
    /// (<https://ai.google.dev/gemini-api/docs/openai>, mise à jour du
    /// 02/09/2026) : adresse `https://generativelanguage.googleapis.com/v1beta/openai/`,
    /// clé en `Authorization: Bearer <clé>`, modèle nommé sans préfixe
    /// `models/`. La page dit cette compatibilité « encore en bêta » et ne
    /// documente pas `response_format: json_object` : il n'est donc PAS
    /// envoyé à Gemini. Ce n'est pas une perte de garantie — aucune famille
    /// n'est crue sur parole : la réponse est extraite et VALIDÉE côté serveur
    /// contre le schéma (`regles::valider`, `suggestions::evaluer`), une
    /// réponse hors schéma rend `reponse_invalide`. D'où le choix du
    /// préréglage plutôt que d'une implémentation de `generateContent`.
    Gemini,
    Ollama,
}

impl Famille {
    pub fn depuis(texte: &str) -> Option<Self> {
        match texte.trim() {
            "anthropic" => Some(Self::Anthropic),
            "compatible_openai" | "openai" => Some(Self::CompatibleOpenai),
            "gemini" => Some(Self::Gemini),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }

    pub fn nom(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::CompatibleOpenai => "compatible_openai",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
        }
    }

    /// L'adresse par défaut du fournisseur.
    pub fn url_par_defaut(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::CompatibleOpenai => "https://api.openai.com/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
            Self::Ollama => "http://127.0.0.1:11434",
        }
    }

    /// Le modèle par défaut. Hors Anthropic, c'est l'utilisateur qui choisit.
    pub fn modele_par_defaut(self) -> &'static str {
        match self {
            Self::Anthropic => crate::couts::MODELE_ANTHROPIC_PAR_DEFAUT,
            Self::Gemini => crate::couts::MODELE_GEMINI_PAR_DEFAUT,
            Self::CompatibleOpenai | Self::Ollama => "",
        }
    }

    /// Ollama tourne chez l'utilisateur et ne demande pas de clé.
    pub fn exige_une_cle(self) -> bool {
        !matches!(self, Self::Ollama)
    }
}

/// Ce que l'on demande au modèle : une consigne et un message.
#[derive(Clone, Debug)]
pub struct Demande {
    pub modele: String,
    pub systeme: String,
    pub message: String,
    /// Plafond de jetons de SORTIE de cet appel.
    pub max_jetons: u32,
    /// Demander une réponse JSON quand le fournisseur sait la garantir.
    pub json: bool,
}

/// Ce que le modèle rend, et ce que l'appel a coûté en jetons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reponse {
    pub texte: String,
    pub jetons_entree: u64,
    pub jetons_sortie: u64,
}

/// Pourquoi un appel a échoué, dans les mots que l'écran traduit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErreurFournisseur {
    /// 401/403 : la clé est fausse, révoquée, ou sans droit.
    CleRefusee,
    /// 429, crédit épuisé : le compte du fournisseur ne suit plus.
    Quota,
    /// 5xx, 529 : le fournisseur est surchargé ou en panne.
    Surcharge,
    /// Le modèle demandé n'existe pas chez ce fournisseur.
    ModeleInconnu,
    /// Le serveur du fournisseur est injoignable (DNS, délai, connexion).
    Reseau,
    /// Le modèle a refusé de répondre.
    Refus,
    /// Toute autre réponse : le statut, sans le corps.
    Autre(u16),
}

impl ErreurFournisseur {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CleRefusee => "cle_refusee",
            Self::Quota => "quota",
            Self::Surcharge => "surcharge",
            Self::ModeleInconnu => "modele_inconnu",
            Self::Reseau => "reseau",
            Self::Refus => "refus",
            Self::Autre(_) => "erreur_fournisseur",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CleRefusee => "Clé refusée par le fournisseur.".into(),
            Self::Quota => "Quota ou crédit du fournisseur épuisé.".into(),
            Self::Surcharge => "Fournisseur surchargé ou indisponible, réessayez plus tard.".into(),
            Self::ModeleInconnu => "Ce modèle n'existe pas chez ce fournisseur.".into(),
            Self::Reseau => "Fournisseur injoignable (réseau).".into(),
            Self::Refus => "Le modèle a refusé de répondre.".into(),
            Self::Autre(s) => format!("Le fournisseur a répondu {s}."),
        }
    }
}

/// L'interface commune.
#[async_trait]
pub trait Fournisseur: Send + Sync {
    async fn completer(&self, demande: &Demande) -> Result<Reponse, ErreurFournisseur>;
}

/// De quoi construire un fournisseur : sa famille, son adresse, sa clé.
#[derive(Clone)]
pub struct Acces {
    pub famille: Famille,
    pub url_base: String,
    pub cle: Option<String>,
}

// La clé ne s'imprime jamais, même dans un `{:?}` de débogage.
impl std::fmt::Debug for Acces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acces")
            .field("famille", &self.famille)
            .field("url_base", &self.url_base)
            .field("cle", &self.cle.as_ref().map(|_| "********"))
            .finish()
    }
}

/// Fabrique de fournisseurs : la vraie en production, une fausse en test.
pub type Fabrique = std::sync::Arc<dyn Fn(Acces) -> Box<dyn Fournisseur> + Send + Sync>;

/// La fabrique de production.
pub fn fabrique_reelle() -> Fabrique {
    std::sync::Arc::new(|acces: Acces| -> Box<dyn Fournisseur> {
        match acces.famille {
            Famille::Anthropic => Box::new(Anthropic { acces }),
            Famille::CompatibleOpenai | Famille::Gemini => Box::new(CompatibleOpenai { acces }),
            Famille::Ollama => Box::new(Ollama { acces }),
        }
    })
}

pub(crate) const DELAI: Duration = Duration::from_secs(90);

pub(crate) fn client() -> &'static reqwest::Client {
    tune_core::http::client::shared()
}

/// Une erreur de transport, réduite à sa nature : son texte peut porter l'URL.
pub(crate) fn erreur_reseau(famille: Famille, e: &reqwest::Error) -> ErreurFournisseur {
    let nature = if e.is_timeout() {
        "delai"
    } else if e.is_connect() {
        "connexion"
    } else {
        "transport"
    };
    warn!(
        fournisseur = famille.nom(),
        nature, "assistant_ia_fournisseur_injoignable"
    );
    ErreurFournisseur::Reseau
}

/// Un statut d'échec, traduit. `corps` ne sert qu'à reconnaître un crédit
/// épuisé ou un modèle inconnu ; il n'est jamais journalisé.
pub(crate) fn erreur_de_statut(famille: Famille, statut: u16, corps: &str) -> ErreurFournisseur {
    warn!(
        fournisseur = famille.nom(),
        statut, "assistant_ia_fournisseur_erreur"
    );
    let bas = corps.to_ascii_lowercase();
    match statut {
        401 | 403 => ErreurFournisseur::CleRefusee,
        402 | 429 => ErreurFournisseur::Quota,
        404 => ErreurFournisseur::ModeleInconnu,
        400 if bas.contains("credit balance") || bas.contains("insufficient_quota") => {
            ErreurFournisseur::Quota
        }
        400 if bas.contains("model")
            && (bas.contains("not found") || bas.contains("not_found")) =>
        {
            ErreurFournisseur::ModeleInconnu
        }
        500..=599 => ErreurFournisseur::Surcharge,
        s => ErreurFournisseur::Autre(s),
    }
}

pub(crate) fn base(acces: &Acces) -> String {
    let b = acces.url_base.trim();
    let b = if b.is_empty() {
        acces.famille.url_par_defaut()
    } else {
        b
    };
    b.trim_end_matches('/').to_string()
}

pub(crate) fn jetons(v: &Value, cle: &str) -> u64 {
    v.get(cle).and_then(Value::as_u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

struct Anthropic {
    acces: Acces,
}

#[async_trait]
impl Fournisseur for Anthropic {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        let corps = json!({
            "model": d.modele,
            "max_tokens": d.max_jetons,
            "system": d.systeme,
            "messages": [{ "role": "user", "content": d.message }],
        });
        let rep = client()
            .post(format!("{}/v1/messages", base(&self.acces)))
            .header("x-api-key", self.acces.cle.as_deref().unwrap_or(""))
            .header("anthropic-version", "2023-06-01")
            .timeout(DELAI)
            .json(&corps)
            .send()
            .await
            .map_err(|e| erreur_reseau(Famille::Anthropic, &e))?;
        let statut = rep.status().as_u16();
        let texte = rep
            .text()
            .await
            .map_err(|e| erreur_reseau(Famille::Anthropic, &e))?;
        if !(200..300).contains(&statut) {
            return Err(erreur_de_statut(Famille::Anthropic, statut, &texte));
        }
        let v: Value =
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::Autre(statut))?;
        if v.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            return Err(ErreurFournisseur::Refus);
        }
        // Seuls les blocs `text` portent la réponse ; un bloc `thinking`
        // (réflexion adaptative des modèles récents) est ignoré.
        let sortie: String = v
            .get("content")
            .and_then(Value::as_array)
            .map(|blocs| {
                blocs
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        Ok(Reponse {
            texte: sortie,
            jetons_entree: jetons(&usage, "input_tokens")
                + jetons(&usage, "cache_creation_input_tokens")
                + jetons(&usage, "cache_read_input_tokens"),
            jetons_sortie: jetons(&usage, "output_tokens"),
        })
    }
}

// ---------------------------------------------------------------------------
// Compatible OpenAI (OpenAI, Mistral…)
// ---------------------------------------------------------------------------

struct CompatibleOpenai {
    acces: Acces,
}

#[async_trait]
impl Fournisseur for CompatibleOpenai {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        // Pas de plafond de sortie dans le corps : la clé `max_tokens` est
        // refusée par les modèles récents d'OpenAI, et `max_completion_tokens`
        // par d'autres points d'accès « compatibles ». La consigne demande une
        // réponse courte ; le coût réel est compté sur `usage`.
        let mut corps = json!({
            "model": d.modele,
            "messages": [
                { "role": "system", "content": d.systeme },
                { "role": "user", "content": d.message },
            ],
        });
        // Pas pour Gemini : sa page de compatibilité ne documente pas ce mode
        // (voir [`Famille::Gemini`]).
        if d.json && self.acces.famille != Famille::Gemini {
            corps["response_format"] = json!({ "type": "json_object" });
        }
        let famille = self.acces.famille;
        let mut req = client()
            .post(format!("{}/chat/completions", base(&self.acces)))
            .timeout(DELAI)
            .json(&corps);
        if let Some(cle) = self.acces.cle.as_deref() {
            req = req.bearer_auth(cle);
        }
        let rep = req.send().await.map_err(|e| erreur_reseau(famille, &e))?;
        let statut = rep.status().as_u16();
        let texte = rep.text().await.map_err(|e| erreur_reseau(famille, &e))?;
        if !(200..300).contains(&statut) {
            return Err(erreur_de_statut(famille, statut, &texte));
        }
        let v: Value =
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::Autre(statut))?;
        let sortie = v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        Ok(Reponse {
            texte: sortie,
            jetons_entree: jetons(&usage, "prompt_tokens"),
            jetons_sortie: jetons(&usage, "completion_tokens"),
        })
    }
}

// ---------------------------------------------------------------------------
// Ollama (local)
// ---------------------------------------------------------------------------

struct Ollama {
    acces: Acces,
}

#[async_trait]
impl Fournisseur for Ollama {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        let mut corps = json!({
            "model": d.modele,
            "stream": false,
            "messages": [
                { "role": "system", "content": d.systeme },
                { "role": "user", "content": d.message },
            ],
        });
        if d.json {
            corps["format"] = json!("json");
        }
        let rep = client()
            .post(format!("{}/api/chat", base(&self.acces)))
            .timeout(DELAI)
            .json(&corps)
            .send()
            .await
            .map_err(|e| erreur_reseau(Famille::Ollama, &e))?;
        let statut = rep.status().as_u16();
        let texte = rep
            .text()
            .await
            .map_err(|e| erreur_reseau(Famille::Ollama, &e))?;
        if !(200..300).contains(&statut) {
            return Err(erreur_de_statut(Famille::Ollama, statut, &texte));
        }
        let v: Value =
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::Autre(statut))?;
        Ok(Reponse {
            texte: v
                .pointer("/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            jetons_entree: jetons(&v, "prompt_eval_count"),
            jetons_sortie: jetons(&v, "eval_count"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_statuts_se_traduisent() {
        let f = Famille::Anthropic;
        assert_eq!(erreur_de_statut(f, 401, ""), ErreurFournisseur::CleRefusee);
        assert_eq!(erreur_de_statut(f, 403, ""), ErreurFournisseur::CleRefusee);
        assert_eq!(erreur_de_statut(f, 429, ""), ErreurFournisseur::Quota);
        assert_eq!(
            erreur_de_statut(f, 400, "Your credit balance is too low"),
            ErreurFournisseur::Quota
        );
        assert_eq!(erreur_de_statut(f, 529, ""), ErreurFournisseur::Surcharge);
        assert_eq!(
            erreur_de_statut(f, 404, ""),
            ErreurFournisseur::ModeleInconnu
        );
        assert_eq!(erreur_de_statut(f, 418, ""), ErreurFournisseur::Autre(418));
    }

    #[test]
    fn le_debogage_d_un_acces_ne_montre_pas_la_cle() {
        let a = Acces {
            famille: Famille::Anthropic,
            url_base: String::new(),
            cle: Some("sk-ant-secret-123456".into()),
        };
        let vu = format!("{a:?}");
        assert!(!vu.contains("secret"), "{vu}");
    }
}
