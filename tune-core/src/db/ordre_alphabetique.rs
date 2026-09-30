//! L'ordre alphabétique des listes paginées de la bibliothèque (#4956, suite
//! décidée par Bertrand le 29/09/2026) : artistes, albums par titre ou par
//! artiste, listes de lecture.
//!
//! La clé est celle du serveur média, `comparer_alphabetique`
//! ([`crate::upnp_server::cle_alphabetique`]) : signes de tête ignorés, casse
//! et accents ignorés, nombres par leur valeur, ex æquo départagés par le
//! texte brut. Un rayon se lit donc dans le même ordre sur un lecteur DLNA et
//! dans le client web, sur SQLite comme sur PostgreSQL.
//!
//! # Pourquoi en Rust, et pas en SQL
//!
//! `ORDER BY LOWER(…)` ne replie que l'ASCII sur SQLite et suit la collation
//! de la base sur PostgreSQL ; aucune expression SQL portable ne rend cette
//! clé. Deux façons de la servir ont été mesurées le 29/09/2026 sur Shrek
//! (build `--release`, 100 000 pistes, 10 000 artistes, 10 000 albums) :
//!
//! - **A**, une colonne de clé calculée et indexée : migration SQLite ET
//!   PostgreSQL, et une clé à tenir juste sur CHAQUE chemin d'écriture (scan,
//!   édition, fusion d'artistes, enrichissement du nom de tri…), à recalculer
//!   entièrement si la règle change. Page d'albums par titre : 18 ms en SQL
//!   sur SQLite, 58 ms sur PostgreSQL ; remplissage 1,3 s / 5,4 s.
//! - **B**, retenue : le tri en mémoire de lignes ÉTROITES (identifiant et
//!   texte de la clé), puis la découpe. Route entière `GET /library/albums?
//!   sort=title`, page à l'offset 5 000 : 26 ms sur SQLite, 139 ms sur
//!   PostgreSQL (lecture étroite 94 ms + tri 11 ms) ; l'ancien `ORDER BY
//!   LOWER(a.title)` coûtait 17 ms / 64 ms en SQL seul. Aucune clé ne peut
//!   être périmée, et c'est la fonction même du serveur média qui trie.
//!
//! L'ordre rendu est TOTAL (clé, texte brut, puis identifiant) : la même
//! requête rend le même ordre, deux pages ne se recouvrent pas et aucune ne
//! perd d'élément.

/// La page `[offset, offset + limit)` d'une liste déjà triée, avec la
/// sémantique de `LIMIT … OFFSET …` de SQLite : `limit` négatif = sans
/// borne, `offset` négatif = depuis le début.
pub(crate) fn tranche<T>(items: Vec<T>, limit: i64, offset: i64) -> Vec<T> {
    let debut = usize::try_from(offset.max(0)).unwrap_or(usize::MAX);
    let nombre = if limit < 0 {
        usize::MAX
    } else {
        usize::try_from(limit).unwrap_or(usize::MAX)
    };
    items.into_iter().skip(debut).take(nombre).collect()
}

/// Les identifiants d'une page, par listes SQL de 5 000 au plus : ils sont
/// inscrits sans marqueur (entiers issus de la base), et une liste bornée
/// reste loin de la longueur maximale d'une requête SQLite.
pub(crate) fn listes_d_ids(ids: &[i64]) -> impl Iterator<Item = String> + '_ {
    ids.chunks(5000)
        .map(|lot| lot.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_tranche_suit_limit_et_offset_de_sqlite() {
        let v: Vec<i32> = (0..10).collect();
        assert_eq!(tranche(v.clone(), 3, 0), vec![0, 1, 2]);
        assert_eq!(tranche(v.clone(), 3, 8), vec![8, 9]);
        assert_eq!(tranche(v.clone(), 3, 10), Vec::<i32>::new());
        assert_eq!(tranche(v.clone(), -1, 7), vec![7, 8, 9]);
        assert_eq!(tranche(v.clone(), 2, -5), vec![0, 1]);
        assert_eq!(tranche(v, 0, 0), Vec::<i32>::new());
    }

    #[test]
    fn les_listes_d_ids_sont_bornees() {
        let ids: Vec<i64> = (1..=12_001).collect();
        let listes: Vec<String> = listes_d_ids(&ids).collect();
        assert_eq!(listes.len(), 3);
        assert!(listes[0].starts_with("1,2,3,"));
        assert!(listes[2].ends_with("12001"));
    }
}
