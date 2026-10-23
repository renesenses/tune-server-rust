//! Ce que coûte un appel, et le plafond mensuel.
//!
//! # La table des prix
//!
//! Elle ne porte que des tarifs DOCUMENTÉS. Les prix sont ceux de l'API
//! Anthropic en accès direct (« first-party »), en dollars par million de
//! jetons, **relevés le 12/10/2026** dans la référence de l'API Claude (table
//! des modèles mise en cache le 06/10/2026). Pour Claude Haiku 5.5, c'est le
//! tarif des requêtes de moins de 100 000 jetons d'entrée — toutes les
//! nôtres : au-delà, Anthropic facture 0,50 $ / 2,50 $.
//!
//! Les prix Gemini viennent de la page officielle des tarifs de Google (voir
//! [`TARIFS_GEMINI`], source et date du relevé).
//!
//! Aucun prix n'est inventé pour les autres fournisseurs : pour un point
//! d'accès compatible OpenAI, l'utilisateur saisit lui-même les deux prix de
//! son modèle (`prix_entree_usd_mtok`, `prix_sortie_usd_mtok`). Sans eux,
//! l'appel est REFUSÉ (`tarif_inconnu`) plutôt que compté pour zéro : un
//! plafond qui ne compte rien ne plafonne rien. Ollama tourne chez
//! l'utilisateur : son coût est nul.
//!
//! # Dollars et euros
//!
//! Les fournisseurs facturent en dollars, le plafond se règle en euros. Le
//! taux de conversion est un réglage (`taux_usd_eur`) ; par défaut il vaut 1,
//! ce qui SURESTIME la dépense tant que le dollar vaut moins d'un euro : le
//! plafond est atteint un peu plus tôt, jamais plus tard. C'est un choix de
//! prudence, pas un taux relevé.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// D'où vient une table de prix, et quand elle a été relevée — montré à
/// l'écran à côté du tarif (étape 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Releve {
    /// `AAAA-MM-JJ` : le jour où la table a été relevée.
    pub releve_le: &'static str,
    /// La page relevée, quand elle est connue.
    pub source_url: Option<&'static str>,
    /// `AAAA-MM-JJ` : le fournisseur annonce un changement de prix à cette
    /// date ; la table est à relever avant.
    pub a_relever_avant: Option<&'static str>,
}

/// Le relevé de [`TARIFS_ANTHROPIC`].
pub const RELEVE_ANTHROPIC: Releve = Releve {
    releve_le: "2026-10-12",
    source_url: None,
    a_relever_avant: None,
};

/// Le relevé de [`TARIFS_GEMINI`] : Google annonce des prix à peu près
/// doublés au 01/01/2027.
pub const RELEVE_GEMINI: Releve = Releve {
    releve_le: "2026-10-12",
    source_url: Some("https://ai.google.dev/gemini-api/docs/pricing"),
    a_relever_avant: Some("2027-01-01"),
};

impl Releve {
    /// La table est-elle à relever au jour `aujourd_hui` (`AAAA-MM-JJ`) ?
    /// Les dates ISO se comparent comme des textes.
    pub fn a_relever(&self, aujourd_hui: &str) -> bool {
        self.a_relever_avant.is_some_and(|d| aujourd_hui >= d)
    }
}

/// Le jour courant, à l'heure locale du serveur.
pub fn jour_courant() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// La part du plafond au-delà de laquelle l'écran prévient (80 %).
pub const SEUIL_ALERTE_POURCENT: u64 = 80;

/// La dépense du mois a-t-elle atteint [`SEUIL_ALERTE_POURCENT`] du plafond ?
/// Un plafond nul est atteint d'emblée : l'alerte vaut aussi.
pub fn alerte(micro_eur: u64, plafond_micro_eur: u64) -> bool {
    (micro_eur as u128) * 100 >= (plafond_micro_eur as u128) * (SEUIL_ALERTE_POURCENT as u128)
}

/// Le modèle Anthropic par défaut.
pub const MODELE_ANTHROPIC_PAR_DEFAUT: &str = "claude-haiku-5-5";
/// Le modèle Anthropic proposé pour le classique difficile.
pub const MODELE_ANTHROPIC_CLASSIQUE: &str = "claude-sonnet-5-5";

/// Le modèle Gemini par défaut : celui que Google recommande pour un projet
/// nouveau, « fastest, most cost-effective » (page des modèles,
/// <https://ai.google.dev/gemini-api/docs/models>, relevée le 12/10/2026 :
/// statut Stable).
pub const MODELE_GEMINI_PAR_DEFAUT: &str = "gemini-3.5-flash-lite";

// ---------------------------------------------------------------------------
// Effort de réflexion (décision de Bertrand du 23/10/2026)
// ---------------------------------------------------------------------------

/// L'effort demandé pour les usages COURTS ([`usage_court`]).
///
/// Claude Haiku 5.5 et Sonnet 5.5 réfléchissent par défaut (réflexion
/// adaptative, effort `medium` pour Haiku 5.5, `high` pour Sonnet 5.5), et
/// la réflexion se facture comme de la sortie. Pour une réponse brève en JSON
/// fermé, `low` suffit et coûte moins.
pub const EFFORT_COURT: &str = "low";

/// Les usages courts : une réponse brève, au schéma fermé. Les suggestions
/// d'album, de pistes et les lots gardent l'effort par défaut du modèle.
pub const USAGES_COURTS: &[&str] = &["test", "playlist", "traduction", "requete"];

pub fn usage_court(usage: &str) -> bool {
    USAGES_COURTS.contains(&usage)
}

/// Les modèles Anthropic qui ACCEPTENT `output_config.effort`, relevés le
/// 23/10/2026 dans la référence de l'API Claude (table « Thinking &
/// Effort ») : Haiku 5.5, Sonnet 5.5 et 5, Sonnet 4.6, Opus 4.5 à 5.5, Fable 5
/// et 5.1. **Haiku 4.5 et Sonnet 4.5 le refusent par un 400** (le site l'a
/// appris à ses dépens) : la liste est FERMÉE, un modèle inconnu n'en reçoit
/// pas — il garde son effort par défaut plutôt que d'échouer.
pub const MODELES_AVEC_EFFORT: &[&str] = &[
    "claude-haiku-5-5",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-opus-4-5",
    "claude-fable-5-1",
    "claude-fable-5",
];

/// `modele` accepte-t-il `output_config.effort` ? Le nom exact, ou suivi d'un
/// suffixe daté (`claude-opus-4-5-20251101`).
pub fn accepte_effort(modele: &str) -> bool {
    let m = modele.trim();
    MODELES_AVEC_EFFORT.iter().any(|p| {
        m == *p
            || m.strip_prefix(p)
                .and_then(|reste| reste.strip_prefix('-'))
                .is_some_and(|date| !date.is_empty() && date.chars().all(|c| c.is_ascii_digit()))
    })
}

// ---------------------------------------------------------------------------
// Estimations avant l'appel
// ---------------------------------------------------------------------------

/// Jetons d'entrée ESTIMÉS d'un texte : un jeton pour trois caractères. C'est
/// prudent (l'anglais compte plutôt quatre caractères par jeton, le français
/// un peu moins) : l'estimation surestime, jamais l'inverse.
pub fn estimer_jetons(texte: &str) -> u64 {
    (texte.chars().count() as u64).div_ceil(3)
}

/// Le coût ESTIMÉ d'un appel avant son envoi, en micro-euros : l'entrée
/// estimée et la sortie au plus (`max_sortie`). Montré à l'écran avec
/// l'aperçu de ce qui part.
pub fn estimer_micro_eur(tarif: Tarif, entree: &str, max_sortie: u32, taux_usd_eur: f64) -> u64 {
    cout_micro_eur(
        tarif,
        estimer_jetons(entree),
        u64::from(max_sortie),
        taux_usd_eur,
    )
}

/// Le contexte demandé à Ollama, au moins.
pub const NUM_CTX_MIN: u32 = 4096;
/// Le contexte demandé à Ollama, au plus.
pub const NUM_CTX_MAX: u32 = 65_536;

/// Le `num_ctx` à envoyer à Ollama pour une requête dont l'entrée fait
/// `caracteres_entree` caractères et la sortie `max_sortie` jetons au plus.
///
/// Sans lui, Ollama tronque SANS ERREUR le début d'un contexte plus long que
/// son défaut (2 048 ou 4 096 jetons selon les versions) — la consigne et le
/// schéma, justement. Arrondi au kilo-jeton supérieur, borné à
/// [`NUM_CTX_MIN`]..[`NUM_CTX_MAX`].
pub fn num_ctx_ollama(caracteres_entree: usize, max_sortie: u32) -> u32 {
    let besoin = (caracteres_entree as u64).div_ceil(3) + u64::from(max_sortie) + 256;
    let arrondi = besoin.div_ceil(1024) * 1024;
    arrondi.clamp(u64::from(NUM_CTX_MIN), u64::from(NUM_CTX_MAX)) as u32
}

/// Un tarif documenté.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tarif {
    pub entree_usd_mtok: f64,
    pub sortie_usd_mtok: f64,
}

/// Les tarifs connus, par identifiant de modèle Anthropic (relevé du
/// 12/10/2026, voir l'en-tête du module).
pub const TARIFS_ANTHROPIC: &[(&str, Tarif)] = &[
    (
        "claude-haiku-5-5",
        Tarif {
            entree_usd_mtok: 0.10,
            sortie_usd_mtok: 0.50,
        },
    ),
    (
        "claude-sonnet-5-5",
        Tarif {
            entree_usd_mtok: 2.00,
            sortie_usd_mtok: 10.00,
        },
    ),
];

/// Les tarifs Gemini (palier payant, texte, ≤ 200 000 jetons d'entrée), en
/// dollars par million de jetons. Source : <https://ai.google.dev/gemini-api/docs/pricing>,
/// page mise à jour le 09/10/2026, **relevée le 12/10/2026**. La page annonce
/// des prix « à peu près doublés » à partir du 01/01/2027 : la table sera à
/// relever avant cette date.
pub const TARIFS_GEMINI: &[(&str, Tarif)] = &[
    (
        "gemini-3.5-flash-lite",
        Tarif {
            entree_usd_mtok: 0.30,
            sortie_usd_mtok: 2.50,
        },
    ),
    (
        "gemini-3.1-flash-lite",
        Tarif {
            entree_usd_mtok: 0.25,
            sortie_usd_mtok: 1.50,
        },
    ),
];

pub fn tarif_gemini(modele: &str) -> Option<Tarif> {
    TARIFS_GEMINI
        .iter()
        .find(|(m, _)| *m == modele.trim())
        .map(|(_, t)| *t)
}

/// Le tarif d'un modèle Anthropic, s'il est dans la table.
pub fn tarif_anthropic(modele: &str) -> Option<Tarif> {
    TARIFS_ANTHROPIC
        .iter()
        .find(|(m, _)| *m == modele.trim())
        .map(|(_, t)| *t)
}

/// Le coût d'un appel, en MICRO-euros, arrondi au micro-euro supérieur.
pub fn cout_micro_eur(tarif: Tarif, entree: u64, sortie: u64, taux_usd_eur: f64) -> u64 {
    let usd = (entree as f64) * tarif.entree_usd_mtok / 1_000_000.0
        + (sortie as f64) * tarif.sortie_usd_mtok / 1_000_000.0;
    let micro = usd * taux_usd_eur * 1_000_000.0;
    if micro <= 0.0 { 0 } else { micro.ceil() as u64 }
}

/// La consommation du mois en cours.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consommation {
    /// `AAAA-MM` : le mois que ces compteurs décrivent.
    pub mois: String,
    pub micro_eur: u64,
    pub appels: u64,
    pub jetons_entree: u64,
    pub jetons_sortie: u64,
    /// Réponses servies par le cache, sans appel ni coût.
    pub depuis_cache: u64,
    /// Étape 3 : les mêmes compteurs, par usage (`playlist`, `album`,
    /// `pistes`, `test`, `requete`, `traduction`). Absent d'une consommation
    /// rangée avant l'étape 3 : vide, les totaux restent justes.
    #[serde(default)]
    pub par_usage: BTreeMap<String, CompteUsage>,
}

/// Les compteurs d'un usage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompteUsage {
    pub micro_eur: u64,
    pub appels: u64,
    pub jetons_entree: u64,
    pub jetons_sortie: u64,
    pub depuis_cache: u64,
}

impl Consommation {
    /// Compte un appel, au total et pour son usage.
    pub fn ajouter_appel(&mut self, usage: &str, micro_eur: u64, entree: u64, sortie: u64) {
        self.appels += 1;
        self.jetons_entree += entree;
        self.jetons_sortie += sortie;
        self.micro_eur += micro_eur;
        let u = self.par_usage.entry(usage.to_string()).or_default();
        u.appels += 1;
        u.jetons_entree += entree;
        u.jetons_sortie += sortie;
        u.micro_eur += micro_eur;
    }

    /// Compte une réponse servie par le cache.
    pub fn ajouter_cache(&mut self, usage: &str) {
        self.depuis_cache += 1;
        self.par_usage
            .entry(usage.to_string())
            .or_default()
            .depuis_cache += 1;
    }

    /// Les compteurs, remis à zéro si `mois` n'est plus le leur.
    pub fn pour_le_mois(self, mois: &str) -> Self {
        if self.mois == mois {
            self
        } else {
            Self {
                mois: mois.to_string(),
                ..Self::default()
            }
        }
    }
}

/// Le mois courant, à l'heure locale du serveur.
pub fn mois_courant() -> String {
    chrono::Local::now().format("%Y-%m").to_string()
}

/// Des euros saisis (« 2,50 » ou « 2.5 ») en micro-euros. `None` si illisible
/// ou négatif.
pub fn euros_en_micro(texte: &str) -> Option<u64> {
    let v: f64 = texte.trim().replace(',', ".").parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * 1_000_000.0).round() as u64)
}

pub fn micro_en_euros(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_modeles_par_defaut_ont_un_tarif() {
        assert!(tarif_anthropic(MODELE_ANTHROPIC_PAR_DEFAUT).is_some());
        assert!(tarif_anthropic(MODELE_ANTHROPIC_CLASSIQUE).is_some());
        assert!(tarif_anthropic("claude-inconnu").is_none());
        assert!(tarif_gemini(MODELE_GEMINI_PAR_DEFAUT).is_some());
    }

    #[test]
    fn un_million_de_jetons_haiku_coute_ce_que_dit_la_table() {
        let t = tarif_anthropic("claude-haiku-5-5").unwrap();
        // 1 M en entrée (0,10 $) + 1 M en sortie (0,50 $) = 0,60 € au taux 1.
        assert_eq!(cout_micro_eur(t, 1_000_000, 1_000_000, 1.0), 600_000);
        // Le moindre jeton coûte au moins un micro-euro : rien n'est gratuit
        // par arrondi.
        assert_eq!(cout_micro_eur(t, 1, 0, 1.0), 1);
    }

    #[test]
    fn la_consommation_repart_de_zero_chaque_mois() {
        let c = Consommation {
            mois: "2026-09".into(),
            micro_eur: 42,
            ..Default::default()
        };
        assert_eq!(c.clone().pour_le_mois("2026-09").micro_eur, 42);
        assert_eq!(c.pour_le_mois("2026-10").micro_eur, 0);
    }

    #[test]
    fn la_consommation_se_detaille_par_usage() {
        let mut c = Consommation::default();
        c.ajouter_appel("playlist", 100, 10, 5);
        c.ajouter_appel("album", 300, 30, 15);
        c.ajouter_appel("album", 200, 20, 10);
        c.ajouter_cache("album");
        assert_eq!((c.micro_eur, c.appels, c.depuis_cache), (600, 3, 1));
        let a = &c.par_usage["album"];
        assert_eq!(
            (a.micro_eur, a.appels, a.jetons_entree, a.depuis_cache),
            (500, 2, 50, 1)
        );
        assert_eq!(c.par_usage["playlist"].micro_eur, 100);
        // La somme des usages est le total.
        let somme: u64 = c.par_usage.values().map(|u| u.micro_eur).sum();
        assert_eq!(somme, c.micro_eur);
    }

    #[test]
    fn une_consommation_d_avant_l_etape_3_se_relit() {
        let brut = r#"{"mois":"2026-10","micro_eur":42,"appels":1,"jetons_entree":3,"jetons_sortie":4,"depuis_cache":0}"#;
        let c: Consommation = serde_json::from_str(brut).unwrap();
        assert_eq!(c.micro_eur, 42);
        assert!(c.par_usage.is_empty());
    }

    #[test]
    fn l_alerte_part_a_80_pour_cent_du_plafond() {
        assert!(!alerte(1_599_999, 2_000_000));
        assert!(alerte(1_600_000, 2_000_000));
        assert!(alerte(2_500_000, 2_000_000));
        assert!(alerte(0, 0), "plafond nul : atteint d'emblée");
        assert!(!alerte(0, 2_000_000));
        assert!(!alerte(u64::MAX / 200, u64::MAX), "aucun débordement");
    }

    #[test]
    fn la_table_gemini_est_a_relever_au_1er_janvier_2027() {
        assert!(!RELEVE_GEMINI.a_relever("2026-12-31"));
        assert!(RELEVE_GEMINI.a_relever("2027-01-01"));
        assert!(!RELEVE_ANTHROPIC.a_relever("2030-01-01"));
        assert_eq!(jour_courant().len(), 10);
    }

    #[test]
    fn l_effort_ne_part_qu_aux_modeles_qui_l_acceptent() {
        assert!(accepte_effort("claude-haiku-5-5"));
        assert!(accepte_effort("claude-sonnet-5-5"));
        assert!(accepte_effort(" claude-opus-4-5-20251101 "));
        // Le 400 que le site a reçu : Haiku 4.5 refuse `effort`.
        assert!(!accepte_effort("claude-haiku-4-5"));
        assert!(!accepte_effort("claude-haiku-4-5-20251001"));
        assert!(!accepte_effort("claude-sonnet-4-5"));
        assert!(!accepte_effort("claude-inconnu"));
        assert!(!accepte_effort("claude-sonnet-5-5x"));
        assert!(!accepte_effort(""));
        assert!(usage_court("playlist") && usage_court("test"));
        assert!(!usage_court("album") && !usage_court("lot"));
    }

    #[test]
    fn le_contexte_ollama_couvre_l_entree_et_la_sortie() {
        assert_eq!(num_ctx_ollama(0, 0), NUM_CTX_MIN);
        // 60 000 caractères ≈ 20 000 jetons + 4 096 de sortie.
        let n = num_ctx_ollama(60_000, 4096);
        assert!(n >= 20_000 + 4096, "{n}");
        assert_eq!(n % 1024, 0);
        assert_eq!(num_ctx_ollama(10_000_000, 4096), NUM_CTX_MAX);
    }

    #[test]
    fn l_estimation_surestime_plutot_qu_elle_ne_sous_estime() {
        assert_eq!(estimer_jetons(""), 0);
        assert_eq!(estimer_jetons("abcd"), 2);
        let t = tarif_anthropic(MODELE_ANTHROPIC_PAR_DEFAUT).unwrap();
        // 3 000 caractères = 1 000 jetons (0,0001 $) + 1 000 de sortie (0,0005 $).
        assert_eq!(estimer_micro_eur(t, &"x".repeat(3000), 1000, 1.0), 600);
    }

    #[test]
    fn les_euros_se_lisent_avec_virgule_ou_point() {
        assert_eq!(euros_en_micro("2,50"), Some(2_500_000));
        assert_eq!(euros_en_micro("2.5"), Some(2_500_000));
        assert_eq!(euros_en_micro("-1"), None);
        assert_eq!(euros_en_micro("beaucoup"), None);
    }
}
