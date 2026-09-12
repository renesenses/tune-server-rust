//! Le parseur des crédits MusicBrainz vit dans
//! [`tune_core::metadata::credits_mb`] depuis CRD-1 : les trois routes
//! d'enrichissement (`…/tracks/{id}/credits/enrich`,
//! `…/albums/{id}/credits/enrich`, `/library/enrich-credits`) et les passes de
//! fond à venir lisent la MÊME analyse. Ce module ne garde que ce qui est
//! propre au serveur : la clé d'avancement, et la ré-exportation que
//! `credits.rs` emprunte.

pub(super) use tune_core::metadata::credits_mb::{LigneCredit, lignes_credits};

/// Clé `settings` d'avancement de `POST /library/enrich-credits` (#2799).
///
/// Même forme et même cycle de vie que `enrich_all_status` : `running` au
/// lancement puis à chaque jalon, `done` à la fin normale. Elle vit ICI, dans
/// le module `pub(crate)`, parce que `startup.rs` la neutralise au démarrage —
/// sans quoi un arrêt en cours de passe laisse `running` en base pour toujours
/// et le bouton reste grisé (défaut #2002). La constante plutôt que le
/// littéral : renommer la clé d'un côté ne peut plus désynchroniser l'autre.
pub(crate) const REGLAGE_AVANCEMENT_CREDITS: &str = "enrich_credits_status";

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// CRD-1 : un seul parseur de crédits dans l'arbre. Le module cœur le
    /// définit, ce module ne fait que le ré-exporter, et l'ancien doublon
    /// `credit_enricher` n'existe plus. Lu dans les SOURCES : vaut quel que
    /// soit le jeu de features.
    #[test]
    fn un_seul_parseur_de_credits_dans_l_arbre() {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
        let coeur = std::fs::read_to_string(racine.join("../tune-core/src/metadata/credits_mb.rs"))
            .expect("le parseur vit dans tune-core");
        // Les noms sont assemblés à l'exécution : le texte de ce test ne doit
        // pas contenir lui-même la définition qu'il interdit.
        let ici = std::fs::read_to_string(racine.join("src/routes/library/credits_mb.rs")).unwrap();
        for nom in ["lignes_credits", "lignes_relations", "lignes_artist_credit"] {
            let definition = format!("fn {nom}(");
            assert!(
                coeur.contains(&definition),
                "{definition} doit être définie dans tune-core"
            );
            assert!(
                !ici.contains(&definition),
                "{definition} ne doit plus être définie côté routes"
            );
        }
        assert!(
            !racine
                .join("../tune-core/src/metadata/credit_enricher.rs")
                .exists(),
            "credit_enricher.rs est le doublon mort : il ne revient pas"
        );
    }
}
