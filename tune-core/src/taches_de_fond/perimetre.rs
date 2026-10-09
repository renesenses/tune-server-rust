//! Le PÉRIMÈTRE des analyses de fond (#5593).
//!
//! Tades (1.0.0-rc1, 528 352 pistes, quatre racines dont un partage réseau)
//! trouvait la tâche « sans fin » : « je n'ai pas besoin que mon NAS soit
//! analysé ». Ce module permet d'**exclure des racines de bibliothèque** des
//! passes qui DÉCODENT : ReplayGain, plage dynamique, empreintes et CLAP.
//!
//! Un réglage, écrit par `PATCH /system/config` et publié par
//! `GET /system/config` :
//!
//! | Clé                                   | Valeur                     | Vide          |
//! |---------------------------------------|----------------------------|---------------|
//! | `background_analysis_excluded_roots`  | tableau de racines exclues | rien d'exclu  |
//!
//! Le réglage vit dans `settings`, à côté de `music_dirs` qui porte les racines
//! elles-mêmes : aucune table, donc aucune migration. Une racine retirée de
//! `music_dirs` laisse au pire une entrée qui ne correspond plus à aucun
//! chemin, sans effet.
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
//! Exclure ne JETTE rien : les valeurs déjà mesurées sur la racine restent en
//! base et servent à la lecture. Réinclure la racine reprend là où elle en
//! était.
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
//! ou un `_` dans un nom de dossier serait un joker. Le préfixe porte le
//! séparateur : exclure `/music` ne doit pas exclure `/music2`. Et les deux
//! côtés sont comparés séparateurs NORMALISÉS (`\` → `/`) : une base Windows
//! porte l'un ou l'autre selon le chemin d'arrivée de la piste (scan,
//! surveillance, import), à l'intérieur de la racine comme après elle.
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Les racines de bibliothèque exclues des passes qui décodent.
pub const CLE_RACINES_EXCLUES: &str = "background_analysis_excluded_roots";

/// Borne du nombre d'entrées d'une liste : une clause de mille préfixes
/// coûterait à chaque `COUNT(*)` de la page État, pour un réglage qui en compte
/// une poignée.
pub const ENTREES_MAX: usize = 200;

/// Borne de longueur d'une entrée, en caractères.
pub const LONGUEUR_MAX: usize = 4096;

/// Lit une liste de chaînes en base. Absente, illisible ou d'un autre type :
/// vide — c'est-à-dire « rien d'exclu », le comportement d'avant le réglage.
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

/// Un chemin, séparateurs normalisés : `\` devient `/`.
fn normaliser_les_separateurs(chemin: &str) -> String {
    chemin.replace('\\', "/")
}

/// `" AND NOT (…)"` : écarte les pistes dont `expr` (un chemin) est l'une des
/// `racines` ou se trouve dessous. Vide si aucune racine.
///
/// Les deux côtés sont comparés séparateurs normalisés (`\` → `/`) : une base
/// Windows porte l'un ou l'autre, y compris À L'INTÉRIEUR de la racine
/// (`Z:\NAS` réglé, `Z:/NAS/f.flac` en base). La racine est prise sans son
/// séparateur final ; `/` seul devient le préfixe vide suivi d'un séparateur :
/// tout chemin absolu.
///
/// La casse compte : la racine réglée vient de `music_dirs`, la même chaîne
/// que le scan a mise en tête des chemins.
pub fn clause_hors_racines(expr: &str, racines: &[String]) -> String {
    match termes_des_racines(expr, racines) {
        Some(termes) => format!(" AND NOT ({termes})"),
        None => String::new(),
    }
}

/// `" AND (…)"` : l'inverse exact de [`clause_hors_racines`], sur les MÊMES
/// termes — les pistes dont `expr` est l'une des `racines` ou se trouve
/// dessous. Vide si aucune racine.
///
/// Fil 2157 : une piste d'une racine exclue n'est candidate d'aucune passe,
/// mais elle reste « sans plage dynamique ». La compter à part, avec le texte
/// même qui l'écarte, est la seule façon qu'une jauge ne l'attende pas.
pub fn clause_dans_les_racines(expr: &str, racines: &[String]) -> String {
    match termes_des_racines(expr, racines) {
        Some(termes) => format!(" AND ({termes})"),
        None => String::new(),
    }
}

/// Les termes partagés par [`clause_hors_racines`] et
/// [`clause_dans_les_racines`], joints par `OR`. `None` si aucune racine.
fn termes_des_racines(expr: &str, racines: &[String]) -> Option<String> {
    let chemin = format!("REPLACE(COALESCE({expr}, ''), '\\', '/')");
    let termes: Vec<String> = racines
        .iter()
        .filter_map(|r| {
            let r = r.trim();
            if r.is_empty() {
                return None;
            }
            let normale = normaliser_les_separateurs(r);
            let base = normale.trim_end_matches('/');
            let n = base.chars().count() + 1;
            let mut t = format!(
                "substr({chemin}, 1, {n}) = {}",
                litteral(&format!("{base}/"))
            );
            if !base.is_empty() {
                t = format!("{chemin} = {} OR {t}", litteral(base));
            }
            Some(format!("({t})"))
        })
        .collect();
    if termes.is_empty() {
        return None;
    }
    Some(termes.join(" OR "))
}

/// La clause du périmètre pour ReplayGain, plage dynamique et empreintes : les
/// racines exclues, sur `t.file_path`.
pub fn clause_decodage(backend: &Arc<dyn DbBackend>) -> String {
    clause_hors_racines("t.file_path", &racines_exclues(backend))
}

/// L'inverse de [`clause_decodage`] : `" AND (…)"` sur les pistes des racines
/// exclues, vide si aucune racine n'est exclue.
pub fn clause_hors_perimetre_decodage(backend: &Arc<dyn DbBackend>) -> String {
    clause_dans_les_racines("t.file_path", &racines_exclues(backend))
}

/// La clause du périmètre du CLAP : les racines exclues, sur le chemin
/// OUVRABLE (le support d'une piste de feuille CUE compte).
pub fn clause_clap(backend: &Arc<dyn DbBackend>) -> String {
    clause_hors_racines(
        crate::db::track_repo::sql::CHEMIN_OUVRABLE,
        &racines_exclues(backend),
    )
}

#[cfg(test)]
#[path = "perimetre_tests.rs"]
mod tests;
