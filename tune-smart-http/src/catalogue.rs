//! Le CATALOGUE d'un service, quand une règle le demande — #4473.
//!
//! Bertrand, 19/09/2026 : *« Serait-il possible d'étendre les source = Qobuz à
//! l'intégralité du catalogue ? »*
//!
//! ## Ce qu'un service sait répondre, et ce qu'il ne sait pas
//!
//! Le contrat d'un connecteur (`tune_core::streaming::StreamingService`) offre
//! `search`, `get_artist_albums`, `get_artist_top_tracks`, `get_album_tracks`.
//! **Aucun service n'énumère son catalogue** : on peut chercher, pas
//! interroger. Une règle « source = Qobuz ET année = 2025 » n'a donc aucune
//! requête derrière elle, chez aucun service.
//!
//! En revanche « tout Coltrane chez Qobuz » est exactement `get_artist_albums`.
//!
//! ## Deux valeurs, deux sens — et l'ancienne ne change pas
//!
//! * `source = qobuz` — les **favoris** du profil. Gratuit, hors ligne,
//!   déterministe. Inchangé.
//! * `source = catalogue:qobuz` — le **catalogue** du service, borné à ce
//!   qu'une règle `artist` ou `album` nomme (arbitrage de Bertrand, 19/09).
//!
//! ## 🔴 Sans cible, on REFUSE — on ne rend ni tout ni rien
//!
//! Une règle `catalogue:<service>` sans règle artiste ni album n'a aucune
//! requête possible. Elle doit être refusée **explicitement**. C'est la leçon
//! de #4469 : soixante-six combinaisons rendaient la bibliothèque entière parce
//! qu'une règle non traduite valait « vrai pour tout ».
//!
//! ## La frontière de crate reste fermée
//!
//! Ce module ne connaît aucun service : il déclare **ce dont il a besoin**, et
//! `tune-server` — qui tient le registre — le fournit. `SmartHttpState` porte
//! un `Option<Arc<dyn CatalogueDistant>>`, `None` en test et partout où le
//! registre n'existe pas.
use serde_json::Value;

/// Le préfixe qui distingue le catalogue des favoris dans la valeur d'une
/// règle `source`.
pub const PREFIXE_CATALOGUE: &str = "catalogue:";

/// Un album tel qu'un service le rend. Les champs que la carte d'album de
/// l'écran sait afficher, et rien de plus.
#[derive(Debug, Clone, Default)]
pub struct AlbumDistant {
    pub service: String,
    pub source_id: String,
    pub title: String,
    pub artist: String,
    pub cover_url: Option<String>,
    pub year: Option<i64>,
}

/// Une piste telle qu'un service la rend.
#[derive(Debug, Clone, Default)]
pub struct PisteDistante {
    pub service: String,
    pub source_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_url: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Ce que le module des règles a besoin de demander à un service.
///
/// Les implémentations rendent une liste **vide** quand le service est inconnu,
/// déconnecté ou muet : une panne de réseau ne doit pas faire échouer la
/// résolution d'une collection qui a par ailleurs des albums locaux.
#[async_trait::async_trait]
pub trait CatalogueDistant: Send + Sync {
    async fn albums_par_artiste(&self, service: &str, nom: &str) -> Vec<AlbumDistant>;
    async fn albums_par_titre(&self, service: &str, titre: &str) -> Vec<AlbumDistant>;
    async fn pistes_par_artiste(&self, service: &str, nom: &str) -> Vec<PisteDistante>;
}

/// Ce qu'une règle `catalogue:<service>` peut faire chercher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cible {
    Artiste(String),
    Album(String),
}

fn valeur(regle: &Value) -> String {
    regle
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn champ(regle: &Value) -> String {
    regle
        .get("field")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// Le service nommé par une règle `source = catalogue:<service>`, s'il y en a.
///
/// Comparaison en minuscules : l'éditeur écrit `catalogue:qobuz`, un semis
/// pourrait écrire `Catalogue:Qobuz`.
pub fn service_du_catalogue(rules_json: &str) -> Option<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    rules.iter().find_map(|r| {
        if champ(r) != "source" {
            return None;
        }
        let v = valeur(r).to_lowercase();
        let nom = v.strip_prefix(PREFIXE_CATALOGUE)?.trim().to_string();
        (!nom.is_empty()).then_some(nom)
    })
}

/// L'artiste ou l'album que les règles nomment — la seule chose qu'un service
/// sache chercher.
///
/// L'artiste prime : « catalogue Qobuz, artiste Coltrane, album Blue Train »
/// se lit comme « chez cet artiste », et `get_artist_albums` répond mieux
/// qu'une recherche par titre.
///
/// Seule l'égalité compte. « Artiste **contient** Col » n'est pas une requête
/// qu'un service sait honorer : il chercherait « Col » et rendrait autre chose.
pub fn cible(rules_json: &str) -> Option<Cible> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let egalite = |r: &Value| {
        matches!(
            crate::regles_sql::normaliser_op(
                r.get("op")
                    .or_else(|| r.get("operator"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("contains"),
            ),
            "="
        )
    };
    let nomme = |quoi: &[&str]| -> Option<String> {
        rules.iter().find_map(|r| {
            let c = champ(r);
            if !quoi.contains(&c.as_str()) || !egalite(r) {
                return None;
            }
            let v = valeur(r);
            (!v.is_empty()).then_some(v)
        })
    };
    if let Some(a) = nomme(&["artist", "artist_name"]) {
        return Some(Cible::Artiste(a));
    }
    nomme(&["album", "album_title"]).map(Cible::Album)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAT: &str = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                          {"field":"artist","op":"equals","value":"John Coltrane"}]"#;

    #[test]
    fn le_catalogue_se_distingue_des_favoris() {
        assert_eq!(service_du_catalogue(CAT).as_deref(), Some("qobuz"));
        // 🔴 L'ancienne valeur ne change pas de sens : « qobuz » tout court
        // reste les FAVORIS, et ne doit surtout pas devenir le catalogue.
        assert!(service_du_catalogue(r#"[{"field":"source","op":"=","value":"qobuz"}]"#).is_none());
        assert!(service_du_catalogue(r#"[{"field":"source","op":"=","value":"local"}]"#).is_none());
        assert!(service_du_catalogue("[]").is_none());
    }

    #[test]
    fn la_graphie_du_prefixe_ne_compte_pas() {
        for v in ["catalogue:qobuz", "Catalogue:Qobuz", "CATALOGUE:QOBUZ"] {
            let r = format!(r#"[{{"field":"source","op":"=","value":"{v}"}}]"#);
            assert_eq!(service_du_catalogue(&r).as_deref(), Some("qobuz"), "{v}");
        }
    }

    #[test]
    fn un_prefixe_sans_service_ne_compte_pas() {
        assert!(
            service_du_catalogue(r#"[{"field":"source","op":"=","value":"catalogue:"}]"#).is_none()
        );
        assert!(
            service_du_catalogue(r#"[{"field":"source","op":"=","value":"catalogue:   "}]"#)
                .is_none()
        );
    }

    #[test]
    fn l_artiste_est_la_cible_et_prime_sur_l_album() {
        assert_eq!(
            cible(CAT),
            Some(Cible::Artiste("John Coltrane".into())),
            "l'artiste doit primer"
        );
        let deux = r#"[{"field":"artist","op":"=","value":"Coltrane"},
                       {"field":"album","op":"=","value":"Blue Train"}]"#;
        assert_eq!(cible(deux), Some(Cible::Artiste("Coltrane".into())));
        let seul_album = r#"[{"field":"album","op":"equals","value":"Blue Train"}]"#;
        assert_eq!(cible(seul_album), Some(Cible::Album("Blue Train".into())));
    }

    #[test]
    fn les_deux_graphies_du_champ_artiste_sont_accueillies() {
        for champ in ["artist", "artist_name"] {
            let r = format!(r#"[{{"field":"{champ}","op":"=","value":"Coltrane"}}]"#);
            assert_eq!(
                cible(&r),
                Some(Cible::Artiste("Coltrane".into())),
                "{champ}"
            );
        }
    }

    #[test]
    fn seule_l_egalite_est_une_cible() {
        // « artiste CONTIENT Col » n'est pas une requête qu'un service sait
        // honorer : il chercherait « Col » et rendrait autre chose. On refuse
        // plutôt que de deviner.
        for op in ["contains", "starts_with", "not_equals", "is_empty"] {
            let r = format!(r#"[{{"field":"artist","op":"{op}","value":"Coltrane"}}]"#);
            assert_eq!(cible(&r), None, "opérateur {op}");
        }
    }

    #[test]
    fn sans_cible_il_n_y_a_rien_a_chercher() {
        // C'est le cas qui doit REFUSER en amont : « catalogue Qobuz ET année
        // 2025 » n'a aucune requête possible.
        let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                    {"field":"year","op":"=","value":"2025"}]"#;
        assert_eq!(service_du_catalogue(r).as_deref(), Some("qobuz"));
        assert_eq!(cible(r), None);
    }
}
