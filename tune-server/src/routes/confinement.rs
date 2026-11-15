//! Sécurité — confinement des chemins venus d'un client.
//!
//! Toute route qui reçoit un chemin du client (lecture OU écriture) le passe
//! ici. La règle est celle de `tune_core::chemin_sur` : comparaison lexicale,
//! les deux séparateurs, tout `..` refusé (forme encodée comprise), puis
//! appartenance segment par segment à l'une des racines autorisées. Les deux
//! côtés sont repliés en NFC : un chemin renvoyé tel que le disque l'a fourni
//! (NFD sur un partage SMB) reste sous sa racine configurée en NFC.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::AppError;
use crate::state::AppState;
use tune_core::chemin_sur::{RefusChemin, racine_contenante};

/// Les dossiers de musique effectifs : le réglage `music_dirs`, ou la
/// configuration de démarrage quand le réglage est absent ou vide — le même
/// repli que `GET /library/browse/dir`.
pub(crate) fn racines_musicales(state: &AppState) -> Vec<String> {
    let reglees = crate::routes::system::get_music_dirs_list(&state.backend);
    let brutes = if reglees.is_empty() {
        state.config.music_dirs.clone()
    } else {
        reglees
    };
    brutes
        .into_iter()
        .map(|d| tune_core::scanner::walker::normalize_path(&d))
        .filter(|d| !d.is_empty())
        .collect()
}

/// Un chemin de travail du serveur, en texte, pour la liste des racines.
pub(crate) fn texte(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `chemin` est-il sous l'une des `racines` ? Sinon, le refus nommé.
///
/// Le chemin est vérifié TEL QU'IL SERA OUVERT : pas de rognage. Un
/// `"  /musique/x"` rogné passait la garde puis s'ouvrait relativement au
/// dossier courant du service ; il est désormais refusé (relatif). Seule la
/// forme Unicode est repliée, pour la comparaison.
pub(crate) fn sous_les_racines(chemin: &str, racines: &[String]) -> Result<(), RefusChemin> {
    let nfc: String = chemin.nfc().collect();
    let racines_nfc: Vec<String> = racines.iter().map(|r| r.nfc().collect()).collect();
    racine_contenante(&nfc, &racines_nfc).map(|_| ())
}

/// Le message d'un refus, avec le nom du champ fautif.
pub(crate) fn message(champ: &str, refus: RefusChemin) -> String {
    match refus {
        RefusChemin::Remontee => format!("{champ} must not contain '..'"),
        RefusChemin::Vide => format!("{champ} must not be empty"),
        RefusChemin::Relatif | RefusChemin::Absolu => format!("{champ} must be an absolute path"),
        RefusChemin::HorsRacines => {
            format!("{champ} is outside the directories this server allows for it")
        }
    }
}

/// Même vérification, rendue en `400` pour les routes à `AppError`.
pub(crate) fn exiger(champ: &str, chemin: &str, racines: &[String]) -> Result<(), AppError> {
    sous_les_racines(chemin, racines).map_err(|r| {
        tracing::warn!(champ, chemin, refus = ?r, "chemin_client_refuse");
        AppError::bad_request(message(champ, r))
    })
}

/// Le dossier de sortie d'une extraction (CD, SACD).
///
/// Sans `demande`, rien ne change : le réglage `cle_reglage` (posé par
/// l'administrateur dans la configuration), sinon le dossier de travail
/// `etiquette`. Avec une `demande` du client, elle doit tomber dans un
/// dossier de musique, dans le dossier d'extraction configuré, ou dans le
/// dossier de travail par défaut — tout le reste est refusé.
pub(crate) fn dossier_de_sortie(
    state: &AppState,
    demande: Option<String>,
    cle_reglage: &str,
    etiquette: &str,
) -> Result<String, AppError> {
    let reglage = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(cle_reglage)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty());
    let defaut = texte(&tune_core::chemins_de_travail::racine_de_travail(etiquette));
    let Some(demande) = demande.filter(|s| !s.trim().is_empty()) else {
        return Ok(reglage.unwrap_or(defaut));
    };
    let mut racines = racines_musicales(state);
    racines.extend(reglage);
    racines.push(defaut);
    exiger("output_dir", &demande, &racines)?;
    Ok(demande)
}

/// Un SEGMENT de nom fourni par le client (identifiant, nom de greffon…)
/// joint sous `base` : refuse séparateurs, `..`, chemin absolu.
pub(crate) fn segment_sous(base: &Path, segment: &str) -> Result<PathBuf, RefusChemin> {
    if segment.contains(['/', '\\']) {
        return Err(RefusChemin::Remontee);
    }
    tune_core::chemin_sur::joindre_sous(base, segment)
}

/// Banc commun des tests de confinement des routes : un état de serveur
/// dont l'unique dossier de musique est `musique`, et un dossier voisin
/// `dehors` (avec un fichier) qu'aucune route ne doit atteindre.
#[cfg(test)]
pub(crate) struct Banc {
    pub state: AppState,
    pub musique: PathBuf,
    pub dehors: PathBuf,
    _dossier: tempfile::TempDir,
}

#[cfg(test)]
impl Banc {
    /// `{musique}/../dehors{suffixe}` : lexicalement « sous » la musique pour
    /// un test de préfixe, réellement dans `dehors`.
    pub fn remontee(&self, suffixe: &str) -> String {
        format!("{}/../dehors{suffixe}", self.musique.display())
    }
    pub fn dedans(&self, suffixe: &str) -> String {
        format!("{}{suffixe}", self.musique.display())
    }
}

#[cfg(test)]
pub(crate) fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let musique = dossier.path().join("musique");
    let dehors = dossier.path().join("dehors");
    std::fs::create_dir_all(musique.join("Album")).unwrap();
    std::fs::create_dir_all(&dehors).unwrap();
    std::fs::write(dehors.join("secret.flac"), b"pas pour vous").unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_str().unwrap()]).unwrap(),
        )
        .unwrap();
    Banc {
        state,
        musique,
        dehors,
        _dossier: dossier,
    }
}

/// Le statut HTTP d'un résultat de route, `Ok` ou `Err`.
#[cfg(test)]
pub(crate) fn statut<T: axum::response::IntoResponse, E: axum::response::IntoResponse>(
    r: Result<T, E>,
) -> axum::http::StatusCode {
    match r {
        Ok(v) => v.into_response().status(),
        Err(e) => e.into_response().status(),
    }
}

#[cfg(test)]
#[path = "confinement_routeur_tests.rs"]
mod confinement_routeur_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_et_nfd_tombent_sous_la_meme_racine() {
        let racine = vec!["/mnt/CDThèque".nfc().collect::<String>()];
        let nfd: String = "/mnt/CDThèque/Album".nfd().collect();
        assert!(sous_les_racines(&nfd, &racine).is_ok());
        assert_eq!(
            sous_les_racines("/mnt/CDThèque/../etc", &racine),
            Err(RefusChemin::Remontee)
        );
    }

    /// N1 — la valeur vérifiée est celle qu'on ouvre : un espace en tête
    /// ferait ouvrir un chemin RELATIF au dossier courant du service.
    #[test]
    fn un_chemin_precede_d_espaces_n_est_pas_sous_la_racine() {
        let racine = vec!["/musique".to_string()];
        assert!(sous_les_racines("/musique/Album", &racine).is_ok());
        for c in ["  /musique/Album", "\t/musique/Album", " /musique"] {
            assert_eq!(
                sous_les_racines(c, &racine),
                Err(RefusChemin::Relatif),
                "{c:?}"
            );
        }
    }

    #[test]
    fn un_segment_ne_porte_ni_separateur_ni_remontee() {
        let base = Path::new("/srv/plugins");
        assert_eq!(segment_sous(base, "eq-pro").unwrap(), base.join("eq-pro"));
        for s in ["..", "../x", "a/b", r"a\b", "/etc", "%2e%2e"] {
            assert!(segment_sous(base, s).is_err(), "{s}");
        }
    }
}
