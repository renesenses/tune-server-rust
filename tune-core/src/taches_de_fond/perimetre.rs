//! Le PÉRIMÈTRE des analyses de fond (#5593).
//!
//! Tades (1.0.0-rc1, 528 352 pistes, quatre racines dont un partage réseau)
//! trouvait la tâche « sans fin » et demandait deux choses :
//!
//! 1. **exclure des racines de bibliothèque** (« je n'ai pas besoin que mon NAS
//!    soit analysé ») des passes qui DÉCODENT : ReplayGain, plage dynamique,
//!    empreintes et CLAP ;
//! 2. **réserver le CLAP à certains genres** (« le CLAP ne serait utile que pour
//!    Jazz et Pop »).
//!
//! Deux réglages, écrits par `PATCH /system/config` et publiés par
//! `GET /system/config` :
//!
//! | Clé                                   | Valeur                     | Vide          |
//! |---------------------------------------|----------------------------|---------------|
//! | `background_analysis_excluded_roots`  | tableau de racines exclues | rien d'exclu  |
//! | `audio_embedding_genres`              | tableau de genres retenus  | tous les genres |
//!
//! # Une CLAUSE, ajoutée au prédicat partagé — jamais une seconde requête
//!
//! Chaque passe a UN prédicat, partagé entre sa sélection et le compteur de sa
//! jauge (`CANDIDATS_RG_WHERE`, `CANDIDATS_DR_WHERE`,
//! `CANDIDATS_EMPREINTE_WHERE`, `ELIGIBLE_WHERE`). Le périmètre s'y ajoute en
//! fin de texte, au même endroit pour les deux : une piste exclue n'est donc
//! ni sélectionnée — donc jamais décodée — ni comptée. Une jauge qui
//! continuerait de la compter annoncerait du travail qui ne se fera jamais.
//!
//! # Des littéraux, pas des `?`
//!
//! Les prédicats sont liés par position (`?` du seuil de report, du modèle, du
//! `LIMIT`). Ajouter des marqueurs décalerait tous les appelants, et leur
//! nombre varie avec le réglage. La clause porte donc ses valeurs en
//! LITTÉRAUX SQL, guillemets simples doublés : c'est la forme standard, comprise
//! à l'identique par SQLite et PostgreSQL (`standard_conforming_strings`, la
//! barre oblique inverse d'un chemin Windows n'y est pas un échappement), et la
//! traduction `?` → `$n` du moteur PostgreSQL saute l'intérieur des littéraux.
//!
//! Les racines se comparent par PRÉFIXE avec `substr`, pas avec `LIKE` : un `%`
//! ou un `_` dans un nom de dossier serait un joker. Et le préfixe porte le
//! séparateur : exclure `/music` ne doit pas exclure `/music2`.
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Les racines de bibliothèque exclues des passes qui décodent.
pub const CLE_RACINES_EXCLUES: &str = "background_analysis_excluded_roots";

/// Les genres auxquels l'analyse CLAP est réservée. Vide : tous.
pub const CLE_GENRES_CLAP: &str = "audio_embedding_genres";

/// Borne du nombre d'entrées d'une liste : une clause de mille préfixes
/// coûterait à chaque `COUNT(*)` de la page État, pour un réglage qui en compte
/// une poignée.
pub const ENTREES_MAX: usize = 200;

/// Borne de longueur d'une entrée, en caractères.
pub const LONGUEUR_MAX: usize = 4096;

/// Lit une liste de chaînes en base. Absente, illisible ou d'un autre type :
/// vide — c'est-à-dire « rien d'exclu » / « tous les genres », le comportement
/// d'avant le réglage.
fn liste(backend: &Arc<dyn DbBackend>, cle: &str) -> Vec<String> {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(cle)
        .ok()
        .flatten()
        .unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&brut)
        .ok()
        .and_then(|v| normaliser(&v).ok())
        .unwrap_or_default()
}

/// Les racines exclues, telles que réglées.
pub fn racines_exclues(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_RACINES_EXCLUES)
}

/// Les genres retenus pour le CLAP, tels que réglés. Vide : tous.
pub fn genres_clap(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_CLAP)
}

/// La forme unique d'une liste du périmètre : un tableau de chaînes, rognées,
/// sans vide ni doublon, dans l'ordre reçu. `null` vaut la liste vide.
///
/// Sert à `PATCH /system/config`, qui REFUSE ce qu'elle refuse (400) au lieu de
/// laisser en base une valeur qui retomberait en silence sur « rien d'exclu ».
pub fn normaliser(valeur: &serde_json::Value) -> Result<Vec<String>, String> {
    let elements = match valeur {
        serde_json::Value::Null => return Ok(Vec::new()),
        serde_json::Value::Array(a) => a,
        autre => return Err(format!("un tableau de chaînes est attendu, reçu {autre}")),
    };
    if elements.len() > ENTREES_MAX {
        return Err(format!(
            "{} entrées, au plus {ENTREES_MAX} sont admises",
            elements.len()
        ));
    }
    let mut vues = std::collections::HashSet::new();
    let mut sortie = Vec::with_capacity(elements.len());
    for e in elements {
        let Some(s) = e.as_str() else {
            return Err(format!("entrée {e} : une chaîne est attendue"));
        };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if s.chars().count() > LONGUEUR_MAX {
            return Err(format!("entrée de plus de {LONGUEUR_MAX} caractères"));
        }
        if vues.insert(s.to_string()) {
            sortie.push(s.to_string());
        }
    }
    Ok(sortie)
}

/// Un littéral SQL : guillemets simples doublés. Voir l'en-tête du module.
fn litteral(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `" AND NOT (…)"` : écarte les pistes dont `expr` (un chemin) est l'une des
/// `racines` ou se trouve dessous. Vide si aucune racine.
///
/// La racine est prise sans son séparateur final, puis comparée suivie de `/`
/// OU de `\` : une base Windows porte l'un ou l'autre selon le chemin
/// d'arrivée de la piste (scan, surveillance, import). `/` seul comme racine
/// devient le préfixe vide suivi d'un séparateur : tout chemin absolu.
pub fn clause_hors_racines(expr: &str, racines: &[String]) -> String {
    let termes: Vec<String> = racines
        .iter()
        .filter_map(|r| {
            let base = r.trim().trim_end_matches(['/', '\\']);
            if base.is_empty() && r.trim().is_empty() {
                return None;
            }
            let n = base.chars().count() + 1;
            let mut t = format!(
                "substr(COALESCE({expr}, ''), 1, {n}) IN ({}, {})",
                litteral(&format!("{base}/")),
                litteral(&format!("{base}\\")),
            );
            if !base.is_empty() {
                t = format!("COALESCE({expr}, '') = {} OR {t}", litteral(base));
            }
            Some(format!("({t})"))
        })
        .collect();
    if termes.is_empty() {
        return String::new();
    }
    format!(" AND NOT ({})", termes.join(" OR "))
}

/// `" AND (…)"` : ne garde que les pistes portant l'un des `genres`, dans la
/// colonne `t.genre` (comparée en entier) OU dans le tableau JSON `t.genres` —
/// exactement l'union que teste le filtre Genre des facettes. Insensible à la
/// casse des deux côtés (`LOWER`), sur les deux moteurs. Vide si aucun genre.
///
/// Une piste SANS genre n'est donc pas retenue quand la liste est réglée :
/// c'est ce que l'écran du réglage dit à l'utilisateur.
pub fn clause_genres(genres: &[String]) -> String {
    if genres.is_empty() {
        return String::new();
    }
    let dans_la_colonne = genres
        .iter()
        .map(|g| format!("LOWER({})", litteral(g)))
        .collect::<Vec<_>>()
        .join(", ");
    let dans_le_tableau = genres
        .iter()
        .map(|g| {
            format!(
                "LOWER(COALESCE(t.genres, '')) LIKE LOWER({})",
                litteral(&format!("%\"{g}\"%"))
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(" AND (LOWER(COALESCE(t.genre, '')) IN ({dans_la_colonne}) OR {dans_le_tableau})")
}

/// La clause du périmètre pour ReplayGain, plage dynamique et empreintes : les
/// racines exclues, sur `t.file_path`.
pub fn clause_decodage(backend: &Arc<dyn DbBackend>) -> String {
    clause_hors_racines("t.file_path", &racines_exclues(backend))
}

/// La clause du périmètre du CLAP : les racines exclues, sur le chemin
/// OUVRABLE (le support d'une piste de feuille CUE compte), et les genres.
pub fn clause_clap(backend: &Arc<dyn DbBackend>) -> String {
    let mut c = clause_hors_racines(
        crate::db::track_repo::sql::CHEMIN_OUVRABLE,
        &racines_exclues(backend),
    );
    c.push_str(&clause_genres(&genres_clap(backend)));
    c
}

#[cfg(test)]
#[path = "perimetre_tests.rs"]
mod tests;
