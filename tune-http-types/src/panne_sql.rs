//! Une panne SQL ne doit jamais se présenter comme un résultat (#2861).
//!
//! ## Ce que `unwrap_or_default()` fait dans un gestionnaire de route
//!
//! `DbBackend::query_many` rend `Result<Vec<Vec<SqlValue>>, String>`, et
//! `backend.rs` ne journalise **rien** : l'erreur n'existe que dans ce
//! `Result`. Un gestionnaire qui écrit
//!
//! ```ignore
//! let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
//! ```
//!
//! échange donc l'erreur contre un `Vec` vide, qui devient un `200 []`. Côté
//! écran, une panne de base et une bibliothèque vide sont **le même octet**.
//! Côté journal, il ne reste rien du tout.
//!
//! Ce n'est pas une inquiétude de principe. La #2860 l'a mesuré : trois
//! erreurs PostgreSQL distinctes dans la même requête — `operator does not
//! exist: text = bigint`, un alias de `SELECT` dans un `HAVING`, une colonne
//! absente du `GROUP BY` — toutes avalées ici. Deux sections de l'écran
//! d'accueil sont restées vides pendant des mois sans une seule ligne de
//! journal. La section ne s'explique pas, elle disparaît.
//!
//! ## Pourquoi journaliser, et NON remonter l'erreur
//!
//! Transformer ces appels en `?` rendrait 500 là où l'écran s'affichait
//! dégradé : sur l'accueil, une seule requête fautive effacerait la page
//! entière au lieu d'une section. Le geste retenu garde donc **exactement** le
//! comportement HTTP d'avant — même défaut, même code, même corps — et
//! n'ajoute que la trace qui manquait. Aucune réponse ne change ; seul le
//! silence cesse.
//!
//! Là où l'utilisateur, lui, doit savoir, c'est un `match` explicite qui
//! convient, pas ce helper : voir `add_facet_favorite` dans `profiles.rs`, qui
//! distingue déjà une demande malformée (400) d'une panne (500).
//!
//! ## Pourquoi le lieu est automatique
//!
//! `#[track_caller]` fait porter à la trace le fichier et la ligne de
//! l'**appelant**, pas ceux de ce module. Aucune étiquette n'est à saisir sur
//! les dizaines de sites concernés, donc aucune ne peut être fausse ni
//! diverger du code après un déplacement.
//!
//! ## Pourquoi ce module vit ICI et non dans `tune-server::routes`
//!
//! Il y est né, mais les gestionnaires de routes ne vivent plus tous dans la
//! même caisse : `tune-smart-http` en emprunte autant que `tune-server`. Une
//! caisse extraite ne peut pas dépendre de `tune-server` — ce serait un cycle
//! — donc l'helper descend au point que **les deux** voient déjà,
//! `tune-http-types`, la caisse des contrats HTTP partagés.
//!
//! Le déplacement plutôt que la copie : deux implémentations divergeraient, et
//! une seule des deux porterait la trace. Le test
//! `les_deux_caisses_de_routes_voient_le_meme_helper` de `tune-smart-http`
//! verrouille l'accès depuis la seconde caisse, que la compilation de
//! `tune-server` seule ne prouve pas.

/// Rend le défaut comme `unwrap_or_default()`, mais laisse une trace.
///
/// Extension de `Result`, et non fonction enveloppante, pour que le site
/// d'appel change d'un seul jeton en fin de chaîne : la conversion d'un
/// gestionnaire ne réécrit pas son expression et ne peut donc pas en changer
/// le sens au passage.
pub trait OuDefautJournalise<T> {
    /// Le `Ok` tel quel ; sinon une trace `ERROR` nommant le lieu et l'erreur,
    /// puis `T::default()`.
    #[track_caller]
    fn ou_defaut_journalise(self) -> T;
}

impl<T, E> OuDefautJournalise<T> for Result<T, E>
where
    T: Default,
    E: std::fmt::Display,
{
    #[track_caller]
    fn ou_defaut_journalise(self) -> T {
        match self {
            Ok(valeur) => valeur,
            Err(erreur) => {
                let lieu = std::panic::Location::caller();
                tracing::error!(
                    fichier = lieu.file(),
                    ligne = lieu.line(),
                    erreur = %erreur,
                    "panne_sql_avalee : requete en echec, reponse degradee rendue a sa place"
                );
                T::default()
            }
        }
    }
}
