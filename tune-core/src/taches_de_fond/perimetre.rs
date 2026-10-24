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
//! | `background_analysis_included_dirs`   | dossiers auxquels les analyses se limitent | partout |
//! | `audio_embedding_genres_included`     | genres retenus pour le CLAP | tous les genres |
//! | `audio_embedding_genres_excluded`     | genres écartés du CLAP     | aucun écarté  |
//!
//! Les dossiers inclus (fil 2173 : « circonscrire l'analyse à un répertoire »)
//! sont l'inverse des racines exclues : réglés, seules les pistes sous l'un
//! d'eux sont analysées par les quatre passes. N'importe quel dossier, pas
//! seulement une racine de `music_dirs` ; comparé par le même préfixe que les
//! racines exclues. Une piste doit être DANS un dossier inclus et HORS des
//! racines exclues.
//!
//! Les deux listes de genres (#6198, Thierry, plus de 550 000 pistes) ne
//! touchent QUE la passe CLAP : ReplayGain, plage dynamique et empreintes
//! servent à la lecture de chaque piste, le CLAP seulement à la recherche par
//! ambiance. Une piste écartée n'est ni sélectionnée ni comptée : la passe ne
//! l'attend pas et se termine sans elle.
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

/// Les dossiers auxquels les passes qui décodent se limitent (fil 2173).
/// Vide : toute la bibliothèque.
pub const CLE_DOSSIERS_INCLUS: &str = "background_analysis_included_dirs";

/// Les genres auxquels la passe CLAP est réservée (#6198). Vide : tous.
pub const CLE_GENRES_CLAP_INCLUS: &str = "audio_embedding_genres_included";

/// Les genres que la passe CLAP n'analyse pas (#6198). Vide : aucun.
pub const CLE_GENRES_CLAP_EXCLUS: &str = "audio_embedding_genres_excluded";

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

/// Les dossiers inclus, tels que réglés. Vide : partout.
pub fn dossiers_inclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_DOSSIERS_INCLUS)
}

/// Les genres retenus pour le CLAP, tels que réglés. Vide : tous.
pub fn genres_clap_inclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_CLAP_INCLUS)
}

/// Les genres écartés du CLAP, tels que réglés. Vide : aucun.
pub fn genres_clap_exclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_CLAP_EXCLUS)
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

/// Le test « la piste porte l'un des `genres` » : la colonne `t.genre`
/// comparée en entier, OU le tableau JSON `t.genres` — l'union même que teste
/// le filtre Genre des facettes (`TrackRepo::conditions_du_filtre`).
/// Insensible à la casse des deux côtés (`LOWER`), sur les deux moteurs.
/// `COALESCE` partout : le test vaut vrai ou faux, jamais NULL, ce qui rend
/// son `NOT` sûr. `None` si la liste est vide.
///
/// Le motif `LIKE` ne porte que des genres sans joker : un `%` ou un `_` dans
/// un nom de genre élargirait la recherche au tableau. Ces genres-là ne sont
/// cherchés que dans la colonne, par égalité.
fn termes_des_genres(genres: &[String]) -> Option<String> {
    let genres: Vec<&str> = genres
        .iter()
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .collect();
    if genres.is_empty() {
        return None;
    }
    let dans_la_colonne = genres
        .iter()
        .map(|g| format!("LOWER({})", litteral(g)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut termes = vec![format!(
        "LOWER(COALESCE(t.genre, '')) IN ({dans_la_colonne})"
    )];
    termes.extend(genres.iter().filter(|g| !g.contains(['%', '_'])).map(|g| {
        format!(
            "LOWER(COALESCE(t.genres, '')) LIKE LOWER({})",
            litteral(&format!("%\"{g}\"%"))
        )
    }));
    Some(termes.join(" OR "))
}

/// `" AND (…)"` : ne garde que les pistes portant l'un des `genres`. Vide si
/// aucun genre. Une piste SANS genre n'est donc pas retenue quand la liste est
/// réglée : c'est ce que l'écran du réglage dit à l'utilisateur.
pub fn clause_genres_inclus(genres: &[String]) -> String {
    match termes_des_genres(genres) {
        Some(t) => format!(" AND ({t})"),
        None => String::new(),
    }
}

/// `" AND NOT (…)"` : écarte les pistes portant l'un des `genres`. Vide si
/// aucun genre. Une piste sans genre reste analysée.
pub fn clause_genres_exclus(genres: &[String]) -> String {
    match termes_des_genres(genres) {
        Some(t) => format!(" AND NOT ({t})"),
        None => String::new(),
    }
}

/// La clause du périmètre pour ReplayGain, plage dynamique et empreintes : les
/// racines exclues, sur `t.file_path`.
///
/// Fil 2173 — puis les dossiers inclus : réglés, seules les pistes sous l'un
/// d'eux restent.
pub fn clause_decodage(backend: &Arc<dyn DbBackend>) -> String {
    let mut c = clause_hors_racines("t.file_path", &racines_exclues(backend));
    c.push_str(&clause_dans_les_racines(
        "t.file_path",
        &dossiers_inclus(backend),
    ));
    c
}

/// L'inverse exact de [`clause_decodage`] : `" AND (…)"` sur les pistes
/// d'une racine exclue OU hors de tout dossier inclus. Vide si rien n'est
/// réglé.
pub fn clause_hors_perimetre_decodage(backend: &Arc<dyn DbBackend>) -> String {
    clause_hors_perimetre(
        termes_des_racines("t.file_path", &racines_exclues(backend)),
        termes_des_racines("t.file_path", &dossiers_inclus(backend)),
    )
}

/// `" AND (exclus OR NOT (inclus))"`, chaque moitié seulement si elle est
/// réglée. Vide si aucune ne l'est.
fn clause_hors_perimetre(exclus: Option<String>, inclus: Option<String>) -> String {
    match (exclus, inclus) {
        (None, None) => String::new(),
        (Some(e), None) => format!(" AND ({e})"),
        (None, Some(i)) => format!(" AND NOT ({i})"),
        (Some(e), Some(i)) => format!(" AND (({e}) OR NOT ({i}))"),
    }
}

/// La clause du périmètre du CLAP : les racines exclues, sur le chemin
/// OUVRABLE (le support d'une piste de feuille CUE compte), puis les genres
/// retenus et les genres écartés (#6198). Une piste doit passer les trois.
pub fn clause_clap(backend: &Arc<dyn DbBackend>) -> String {
    let mut c = clause_hors_racines(
        crate::db::track_repo::sql::CHEMIN_OUVRABLE,
        &racines_exclues(backend),
    );
    // Fil 2173 — les dossiers inclus, sur le même chemin ouvrable.
    c.push_str(&clause_dans_les_racines(
        crate::db::track_repo::sql::CHEMIN_OUVRABLE,
        &dossiers_inclus(backend),
    ));
    c.push_str(&clause_genres_inclus(&genres_clap_inclus(backend)));
    c.push_str(&clause_genres_exclus(&genres_clap_exclus(backend)));
    c
}

#[cfg(test)]
#[path = "perimetre_tests.rs"]
mod tests;
