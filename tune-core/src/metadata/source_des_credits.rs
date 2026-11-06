//! D'où vient un crédit de piste : `track_credits.source` (#7765).
//!
//! Chaque écrivain de `track_credits` pose sa source et ne purge que SES
//! lignes et les lignes de source inconnue (NULL, antérieures à la colonne).
//! Sans elle, la passe MusicBrainz effaçait tous les crédits d'une piste avant
//! d'écrire — y compris ceux lus dans les fichiers (#7766).
//!
//! Les passes qui cherchent les pistes « sans crédit » ignorent les lignes
//! `balises` : des crédits lus dans les fichiers ne disent pas qu'une piste a
//! déjà été enrichie. Le filtre vit ICI, une seule fois, pour que les quatre
//! requêtes et leurs jumeaux PostgreSQL exécutent exactement le même texte.

/// Saisi par l'utilisateur. Réservée : aucune route de saisie dans ce lot.
pub const MANUEL: &str = "manuel";
/// Lu sur MusicBrainz (passe par disque ou par enregistrement).
pub const MUSICBRAINZ: &str = "musicbrainz";
/// Lu dans les balises des fichiers (#7766).
pub const BALISES: &str = "balises";
/// Proposition Discogs acceptée.
pub const DISCOGS: &str = "discogs";
/// Import depuis Roon.
pub const ROON: &str = "roon";

/// Toutes les valeurs admises, NULL mis à part (ligne ancienne, inconnue).
pub const TOUTES: [&str; 5] = [MANUEL, MUSICBRAINZ, BALISES, DISCOGS, ROON];

/// Une ligne de `track_credits` aliasée `tc` qui COMPTE comme crédit : toute
/// ligne sauf celles lues dans les balises. NULL compte : une ligne ancienne
/// vient d'une passe d'enrichissement.
pub const COMPTE_COMME_CREDIT: &str = "(tc.source IS NULL OR tc.source <> 'balises')";

/// La piste `t` n'a aucun crédit hors balises — le filtre des passes
/// « à faire » qui lisent `tracks t`.
pub const PISTE_SANS_CREDIT: &str = "NOT EXISTS (SELECT 1 FROM track_credits tc \
     WHERE tc.track_id = t.id AND (tc.source IS NULL OR tc.source <> 'balises'))";

/// Les crédits hors balises d'UNE piste, liée en premier paramètre.
pub const CREDITS_D_UNE_PISTE: &str = "SELECT COUNT(*) FROM track_credits tc \
     WHERE tc.track_id = ? AND (tc.source IS NULL OR tc.source <> 'balises')";

#[cfg(test)]
mod tests {
    use super::*;

    /// Les trois textes portent le même filtre : un seul à corriger.
    #[test]
    fn les_requetes_partagent_le_meme_filtre() {
        assert!(PISTE_SANS_CREDIT.contains(COMPTE_COMME_CREDIT));
        assert!(CREDITS_D_UNE_PISTE.contains(COMPTE_COMME_CREDIT));
        assert!(COMPTE_COMME_CREDIT.contains(&format!("'{BALISES}'")));
    }
}
