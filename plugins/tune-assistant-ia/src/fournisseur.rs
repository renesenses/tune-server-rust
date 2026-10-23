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
    /// L'effort de réflexion à demander (`output_config.effort`), pour un
    /// usage court ([`crate::couts::usage_court`]). Envoyé SEULEMENT aux
    /// modèles qui l'acceptent ([`crate::couts::accepte_effort`]) : Haiku 4.5
    /// le refuse par un 400. `None` : l'effort par défaut du modèle.
    pub effort: Option<&'static str>,
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
    /// #6730 — Anthropic : une clé d'ORGANISATION, rattachée à aucun espace de
    /// travail, exige l'en-tête `anthropic-workspace-id` (400 « not scoped to
    /// a workspace »). Une clé créée dans un espace de travail suffit.
    CleSansEspaceDeTravail,
    /// Toute autre réponse : le statut, et le motif que le fournisseur a
    /// donné (`error.message`), tronqué et expurgé de tout ce qui ressemble à
    /// une clé ([`motif_du_fournisseur`]). `None` : le corps n'en portait pas.
    Autre { statut: u16, motif: Option<String> },
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
            Self::CleSansEspaceDeTravail => "cle_sans_espace_de_travail",
            Self::Autre { .. } => "erreur_fournisseur",
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
            Self::CleSansEspaceDeTravail => "Cette clé Anthropic n'est rattachée à aucun espace de travail : renseignez l'identifiant d'espace de travail dans les réglages de l'assistant, ou créez une clé dans un espace de travail (console.anthropic.com › API keys).".into(),
            Self::Autre {
                statut,
                motif: Some(m),
            } => format!("Le fournisseur a répondu {statut} : « {m} »"),
            Self::Autre { statut, motif: None } => format!("Le fournisseur a répondu {statut}."),
        }
    }

    /// Le motif du fournisseur, déjà expurgé, pour le corps de la réponse.
    pub fn motif(&self) -> Option<&str> {
        match self {
            Self::Autre { motif, .. } => motif.as_deref(),
            _ => None,
        }
    }

    /// Une réponse 2xx dont le corps n'est pas du JSON lisible.
    pub(crate) fn corps_illisible(statut: u16) -> Self {
        Self::Autre {
            statut,
            motif: None,
        }
    }
}

/// Longueur maximale du motif rendu à l'écran et au journal.
const MOTIF_MAX: usize = 300;

/// #6730 — le motif lisible d'une réponse d'erreur : `error.message`
/// (Anthropic, compatible OpenAI, Gemini), `[0].error.message` (Gemini sous
/// forme de liste), ou `error` quand c'est une chaîne (Ollama). Tronqué à
/// [`MOTIF_MAX`] caractères, et tout mot qui ressemble à une clé d'API est
/// remplacé par `[masqué]` : certains fournisseurs recopient un fragment de
/// la clé dans leur message (« Incorrect API key provided: sk-… »). Le corps
/// brut, lui, n'est jamais rendu ni journalisé.
pub(crate) fn motif_du_fournisseur(corps: &str) -> Option<String> {
    let v: Value = serde_json::from_str(corps).ok()?;
    let v = match &v {
        Value::Array(a) => a.first()?.clone(),
        _ => v,
    };
    let brut = match v.get("error")? {
        Value::String(s) => s.clone(),
        e => e.get("message")?.as_str()?.to_string(),
    };
    let expurge: Vec<String> = brut
        .split_whitespace()
        .map(|mot| {
            if ressemble_a_une_cle(mot) {
                "[masqué]".to_string()
            } else {
                mot.to_string()
            }
        })
        .collect();
    let texte = expurge.join(" ");
    let texte = texte.trim();
    if texte.is_empty() {
        return None;
    }
    Some(if texte.chars().count() > MOTIF_MAX {
        format!("{}…", texte.chars().take(MOTIF_MAX).collect::<String>())
    } else {
        texte.to_string()
    })
}

/// Un mot qui pourrait être (un fragment de) clé : préfixe connu (`sk-`,
/// `sk_`, `AIza`, `gsk_`, `xai-`), ou longue suite de 24 caractères ou plus
/// mêlant lettres et chiffres sans espace.
fn ressemble_a_une_cle(mot: &str) -> bool {
    let m = mot.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    if ["sk-", "sk_", "AIza", "gsk_", "xai-"]
        .iter()
        .any(|p| m.starts_with(p))
    {
        return true;
    }
    m.len() >= 24
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && m.chars().any(|c| c.is_ascii_digit())
        && m.chars().any(|c| c.is_ascii_alphabetic())
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
    /// #6730 — Anthropic : l'identifiant d'espace de travail, envoyé dans
    /// l'en-tête `anthropic-workspace-id` quand il est renseigné (clé
    /// d'organisation). Ce n'est pas un secret : la console l'affiche, et il
    /// n'ouvre rien sans la clé. Ignoré des autres fournisseurs.
    pub espace_de_travail: Option<String>,
}

// La clé ne s'imprime jamais, même dans un `{:?}` de débogage.
impl std::fmt::Debug for Acces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acces")
            .field("famille", &self.famille)
            .field("url_base", &self.url_base)
            .field("cle", &self.cle.as_ref().map(|_| "********"))
            .field("espace_de_travail", &self.espace_de_travail)
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

/// L'en-tête d'Anthropic qui désigne l'espace de travail d'une clé
/// d'organisation (#6730).
pub const ENTETE_ESPACE_DE_TRAVAIL: &str = "anthropic-workspace-id";

/// #6730 — les en-têtes d'authentification d'une requête Anthropic : la clé,
/// la version, et l'espace de travail s'il est renseigné. Un seul endroit
/// pour les deux chemins (fournisseur et passerelle de `/ai/query`).
pub(crate) fn entetes_anthropic(
    requete: reqwest::RequestBuilder,
    acces: &Acces,
) -> reqwest::RequestBuilder {
    let requete = requete
        .header("x-api-key", acces.cle.as_deref().unwrap_or(""))
        .header("anthropic-version", "2023-06-01");
    match acces.espace_de_travail.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => requete.header(ENTETE_ESPACE_DE_TRAVAIL, id),
        _ => requete,
    }
}

/// #6730 — un identifiant d'espace de travail acceptable : non vide,
/// 128 caractères au plus, lettres, chiffres, `_` et `-` seulement (la
/// console rend `wrkspc_…`). Tout autre caractère pourrait casser l'en-tête.
pub fn espace_de_travail_valide(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

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

/// Un statut d'échec, traduit. Le corps brut n'est jamais journalisé : seul
/// son motif expurgé l'est ([`motif_du_fournisseur`], #6730).
pub(crate) fn erreur_de_statut(famille: Famille, statut: u16, corps: &str) -> ErreurFournisseur {
    let motif = motif_du_fournisseur(corps);
    warn!(
        fournisseur = famille.nom(),
        statut,
        motif = motif.as_deref().unwrap_or("-"),
        "assistant_ia_fournisseur_erreur"
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
        400 if famille == Famille::Anthropic && bas.contains("not scoped to a workspace") => {
            ErreurFournisseur::CleSansEspaceDeTravail
        }
        500..=599 => ErreurFournisseur::Surcharge,
        statut => ErreurFournisseur::Autre { statut, motif },
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

/// Pose `output_config.effort` sur un corps de l'API Messages, si un effort
/// est demandé ET que `modele` l'accepte. Sinon, le corps reste tel quel.
pub(crate) fn poser_effort(corps: &mut Value, modele: &str, effort: Option<&str>) {
    if let Some(e) = effort
        && crate::couts::accepte_effort(modele)
    {
        corps["output_config"] = json!({ "effort": e });
    }
}

/// Pose `options.num_ctx` sur un corps d'Ollama (`/api/chat`), d'après la
/// taille de ce qu'il porte déjà (`messages`, `tools`) et la sortie attendue
/// ([`crate::couts::num_ctx_ollama`]).
pub(crate) fn poser_num_ctx(corps: &mut Value, max_sortie: u32) {
    let taille = corps
        .get("messages")
        .map(|m| m.to_string().len())
        .unwrap_or(0)
        + corps.get("tools").map(|t| t.to_string().len()).unwrap_or(0);
    corps["options"] = json!({ "num_ctx": crate::couts::num_ctx_ollama(taille, max_sortie) });
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
        let mut corps = json!({
            "model": d.modele,
            "max_tokens": d.max_jetons,
            "system": d.systeme,
            "messages": [{ "role": "user", "content": d.message }],
        });
        poser_effort(&mut corps, &d.modele, d.effort);
        let rep = entetes_anthropic(
            client().post(format!("{}/v1/messages", base(&self.acces))),
            &self.acces,
        )
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
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::corps_illisible(statut))?;
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
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::corps_illisible(statut))?;
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
        poser_num_ctx(&mut corps, d.max_jetons);
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
            serde_json::from_str(&texte).map_err(|_| ErreurFournisseur::corps_illisible(statut))?;
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
        assert_eq!(
            erreur_de_statut(f, 418, ""),
            ErreurFournisseur::Autre {
                statut: 418,
                motif: None
            }
        );
    }

    /// #6730 — la réponse réelle d'Anthropic à une clé d'organisation
    /// (relevée par curl le 22/10/2026 sur le .18).
    const SANS_ESPACE: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header with the ID of the workspace to use."}}"#;

    #[test]
    fn une_cle_sans_espace_de_travail_est_nommee_6730() {
        let e = erreur_de_statut(Famille::Anthropic, 400, SANS_ESPACE);
        assert_eq!(
            e,
            ErreurFournisseur::CleSansEspaceDeTravail,
            "#6730 : le 400 « not scoped to a workspace » est retombé en « Autre »"
        );
        assert_eq!(e.code(), "cle_sans_espace_de_travail");
        assert!(e.message().contains("espace de travail"), "{}", e.message());
    }

    #[test]
    fn un_400_inconnu_garde_le_motif_du_fournisseur_6730() {
        // Compatible OpenAI.
        let e = erreur_de_statut(
            Famille::CompatibleOpenai,
            400,
            r#"{"error":{"message":"Unsupported parameter: 'temperature'","type":"invalid_request_error"}}"#,
        );
        assert_eq!(
            e.motif(),
            Some("Unsupported parameter: 'temperature'"),
            "#6730 : le motif du fournisseur est perdu"
        );
        assert!(
            e.message().contains("Unsupported parameter"),
            "{}",
            e.message()
        );
        // Gemini, réponse en liste.
        let e = erreur_de_statut(
            Famille::Gemini,
            400,
            r#"[{"error":{"code":400,"message":"API key not valid for this project","status":"INVALID_ARGUMENT"}}]"#,
        );
        assert_eq!(e.motif(), Some("API key not valid for this project"));
        // Ollama : `error` est une chaîne.
        let e = erreur_de_statut(Famille::Ollama, 400, r#"{"error":"invalid format"}"#);
        assert_eq!(e.motif(), Some("invalid format"));
        // Un corps qui n'est pas du JSON : pas de motif, pas de corps brut.
        let e = erreur_de_statut(Famille::Anthropic, 400, "<html>Bad Request</html>");
        assert_eq!(e.motif(), None);
    }

    #[test]
    fn le_motif_ne_porte_jamais_la_cle_et_reste_court_6730() {
        let corps = r#"{"error":{"message":"Incorrect API key provided: sk-proj-AbCdEf0123456789. You can find your key at https://platform.openai.com.","type":"invalid_request_error"}}"#;
        let m = motif_du_fournisseur(corps).unwrap();
        assert!(
            !m.contains("sk-proj") && !m.contains("0123456789"),
            "#6730 : un fragment de clé part dans le motif : {m}"
        );
        assert!(m.contains("[masqué]"), "{m}");
        let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "mot ".repeat(400));
        let m = motif_du_fournisseur(&long).unwrap();
        assert!(m.chars().count() <= MOTIF_MAX + 1, "{}", m.chars().count());
        let m =
            motif_du_fournisseur(r#"{"error":{"message":"clé AIzaSyD-abcdefghijklmnop rejetée"}}"#)
                .unwrap();
        assert!(
            !m.contains("AIza"),
            "#6730 : une clé Gemini part dans le motif : {m}"
        );
    }

    #[test]
    fn l_effort_n_est_pose_que_pour_un_modele_qui_l_accepte() {
        let mut c = json!({"model": "claude-haiku-5-5"});
        poser_effort(&mut c, "claude-haiku-5-5", Some("low"));
        assert_eq!(c["output_config"]["effort"], "low");
        let mut c = json!({"model": "claude-haiku-4-5"});
        poser_effort(&mut c, "claude-haiku-4-5", Some("low"));
        assert!(
            c.get("output_config").is_none(),
            "Haiku 4.5 refuse `effort` par un 400 : {c}"
        );
        let mut c = json!({});
        poser_effort(&mut c, "claude-haiku-5-5", None);
        assert!(c.get("output_config").is_none(), "{c}");
    }

    #[test]
    fn ollama_recoit_un_contexte_a_la_taille_de_la_demande() {
        let long = "x".repeat(60_000);
        let mut c = json!({"messages": [{"role": "user", "content": long}]});
        poser_num_ctx(&mut c, 4096);
        let n = c["options"]["num_ctx"].as_u64().unwrap();
        assert!(n >= 20_000 + 4096, "contexte trop court : {n}");
    }

    #[test]
    fn le_debogage_d_un_acces_ne_montre_pas_la_cle() {
        let a = Acces {
            famille: Famille::Anthropic,
            url_base: String::new(),
            cle: Some("sk-ant-secret-123456".into()),
            espace_de_travail: Some("wrkspc_01Essai".into()),
        };
        let vu = format!("{a:?}");
        assert!(!vu.contains("secret"), "{vu}");
    }
}
