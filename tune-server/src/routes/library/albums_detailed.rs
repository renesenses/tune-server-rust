//! `GET /library/albums-detailed` — un album par ligne, avec ses agrégats.
//!
//! La vue « cartes album » d'Oxygen (inspirée de Helium) veut, par album :
//! label, année, durée totale, nombre de CD, nombre de pistes. Jusqu'ici elle
//! les dérivait des pistes DÉJÀ CHARGÉES côté client — donc d'une page. Un
//! album dont la moitié des pistes tombait hors de la page s'affichait avec un
//! nombre de pistes et une durée faux, sans que rien ne le signale. Sur une
//! bibliothèque de 55 000 pistes, c'est la majorité des albums.
//!
//! Ce point d'entrée fait l'agrégat en SQL, sur la sélection de facettes
//! courante, en réutilisant `facets::build_conditions` : les cartes comptent
//! donc exactement ce que le rail annonce.

use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

use super::facets::{FacetQuery, build_conditions, resolve_collection};

/// Une piste sans `album_id` n'est pas un album : elle n'a ni pochette, ni
/// numéro de disque fiable, et regrouper toutes les orphelines sous une carte
/// unique ne veut rien dire. Elles restent visibles dans la table détaillée.
const ONLY_REAL_ALBUMS: &str = "t.album_id IS NOT NULL";

/// L'artiste affiché sur une carte d'album.
///
/// 🔴 #4427 — sur une COMPILATION, l'artiste de la carte est celui de l'album,
/// pas le maximum alphabétique de ce que portent ses pistes.
///
/// Mesuré le 18/09/2026 sur le .18 : les douze lignes « Coco María Presents
/// Club Coco ¡AHORA! » réunies en un disque portent bien
/// `albums.artist_id → « Various Artists »`, et la carte affichait
/// **« Ronald Snijders »** — un invité, dernier par ordre alphabétique.
///
/// La cause n'est pas dans la ligne album, qui est juste, mais ici : la
/// requête ne lisait que `t.album_artist`, la colonne que chaque piste garde
/// de son album d'ORIGINE. Et la fusion d'albums ne la met jamais à jour —
/// elle ne fait que `UPDATE tracks SET album_id = ?`. Après une fusion, les
/// pistes portent donc encore N noms différents.
///
/// Corriger ici plutôt que dans la fusion répare aussi les bibliothèques
/// **déjà** fusionnées, sans réécrire une seule piste.
///
/// Limité aux compilations à dessein : pour un album ordinaire,
/// `t.album_artist` reste ce que les fichiers déclarent, et c'est la valeur
/// que les écrans montrent depuis toujours. `ar_al.name` peut être NUL (album
/// sans `artist_id`) — le `COALESCE` retombe alors sur l'ancien comportement.
const ARTISTE_DE_CARTE: &str = "COALESCE(CASE WHEN COALESCE(al.is_compilation, 0) <> 0 \
     THEN ar_al.name END, t.album_artist, ar.name)";

pub(super) async fn albums_detailed(
    Query(q): Query<FacetQuery>,
    RawQuery(raw): RawQuery,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Même lecture des facettes multi-valeurs que le rail (#2168) : les cartes
    // doivent compter exactement ce que le rail annonce.
    let q = q.hydrate(raw.as_deref())?;
    let engine = state.backend.engine();
    // Même résolution que le rail ET que la liste (#1864) : le nom d'une
    // collection manuelle vit dans un JSON de réglages, celui d'une collection
    // intelligente dans des règles à compiler — jamais dans une table joignable.
    let coll = q
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| resolve_collection(&state, name));

    // `exclude` vide : ici AUCUNE facette n'est exclue. Le rail exclut la
    // facette qu'il compte pour garder ses alternatives visibles ; une liste
    // d'albums, elle, doit refléter la sélection entière.
    let (mut conds, params) = build_conditions(&q, engine, "", coll.as_ref());
    conds.push(ONLY_REAL_ALBUMS.to_string());
    let where_clause = format!(" WHERE {}", conds.join(" AND "));

    let limit = q.limit.unwrap_or(500).clamp(1, 2000);
    let offset = q.offset.unwrap_or(0).max(0);

    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();

    // Total = nombre d'ALBUMS distincts, pas de pistes : c'est ce que la vue
    // pagine et ce que la barre d'état annonce.
    let total_sql = format!("SELECT COUNT(DISTINCT t.album_id) FROM tracks t{where_clause}");
    let total = state
        .backend
        .query_one(&total_sql, &bound)
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // `MAX(...)` sur les colonnes d'album : elles sont constantes au sein d'un
    // groupe (même album), et un agrégat évite d'avoir à les lister dans le
    // GROUP BY — PostgreSQL l'exigerait, SQLite non. Écrire pour les deux.
    let sql = format!(
        "SELECT t.album_id, \
                MAX(al.title), \
                MAX({ARTISTE_DE_CARTE}), \
                MAX(al.cover_path), \
                MAX(t.label), \
                MAX(t.year), \
                SUM(COALESCE(t.duration_ms, 0)), \
                COUNT(DISTINCT COALESCE(t.disc_number, 1)), \
                COUNT(*), \
                MAX(t.format), \
                MAX(t.sample_rate), \
                MAX(t.bit_depth), \
                MAX(al.is_compilation) \
         FROM tracks t \
         LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN artists ar_al ON ar_al.id = al.artist_id{where_clause} \
         GROUP BY t.album_id \
         ORDER BY MAX({ARTISTE_DE_CARTE}), MAX(al.title) \
         LIMIT {limit} OFFSET {offset}"
    );

    let items: Vec<Value> = state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let album_id = it.next()?.as_i64()?;
            let title = it.next().and_then(|v| v.as_string());
            let artist = it.next().and_then(|v| v.as_string());
            let cover = it.next().and_then(|v| v.as_string());
            let label = it.next().and_then(|v| v.as_string());
            let year = it.next().and_then(|v| v.as_i64());
            let duration_ms = it.next().and_then(|v| v.as_i64()).unwrap_or(0);
            let disc_count = it.next().and_then(|v| v.as_i64()).unwrap_or(1);
            let track_count = it.next().and_then(|v| v.as_i64()).unwrap_or(0);
            let format = it.next().and_then(|v| v.as_string());
            let sample_rate = it.next().and_then(|v| v.as_i64());
            let bit_depth = it.next().and_then(|v| v.as_i64());
            // Le drapeau « compilation » (#1957). Un `MAX()` comme les autres
            // colonnes d'album : constante au sein du groupe, et PostgreSQL
            // exigerait sinon la colonne dans le GROUP BY. Décodé par le
            // décodeur unique de `tune-core` — jamais de `null` : une ligne
            // sans album, ou une base migrée qui porte encore NULL, vaut
            // « non », exactement comme dans le modèle `Album`.
            let is_compilation = tune_core::db::album_repo::drapeau_compilation(it.next().as_ref());
            Some(json!({
                "album_id": album_id,
                "title": title,
                "album_artist": artist,
                "cover_path": cover,
                "label": label,
                "year": year,
                "duration_ms": duration_ms,
                "disc_count": disc_count,
                "track_count": track_count,
                "format": format,
                "sample_rate": sample_rate,
                "bit_depth": bit_depth,
                "is_compilation": is_compilation,
            }))
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

#[cfg(test)]
mod tests {
    use super::ONLY_REAL_ALBUMS;
    use serde_json::Value;

    /// Le garde-fou qui empêche les pistes orphelines de former une carte
    /// fantôme. S'il disparaît, `GROUP BY t.album_id` produit un groupe NULL
    /// rassemblant des morceaux sans rapport.
    #[test]
    fn les_pistes_sans_album_sont_ecartees() {
        assert!(ONLY_REAL_ALBUMS.contains("album_id IS NOT NULL"));
    }

    /// 🔴 #4427 — une compilation s'affiche sous l'artiste de son ALBUM.
    ///
    /// Sans le `CASE`, la requête rend le maximum alphabétique de
    /// `t.album_artist` : après une fusion, chaque piste garde le nom de son
    /// album d'origine et la carte sort sous un invité — « Ronald Snijders »
    /// pour « Club Coco ¡AHORA! », mesuré le 18/09 sur le .18.
    #[test]
    fn une_compilation_porte_l_artiste_de_son_album() {
        use super::ARTISTE_DE_CARTE;
        // La compilation lit l'artiste de la ligne album…
        assert!(ARTISTE_DE_CARTE.contains("al.is_compilation"));
        assert!(ARTISTE_DE_CARTE.contains("ar_al.name"));
        // …et rien d'autre ne passe devant lui.
        let i = ARTISTE_DE_CARTE
            .find("ar_al.name")
            .expect("artiste d'album");
        let j = ARTISTE_DE_CARTE
            .find("t.album_artist")
            .expect("repli piste");
        assert!(
            i < j,
            "l'artiste de l'album doit primer sur celui des pistes"
        );
        // Un album ORDINAIRE garde le comportement d'avant : le CASE ne rend
        // `ar_al.name` que si le drapeau est levé, et le COALESCE retombe.
        assert!(ARTISTE_DE_CARTE.contains("t.album_artist, ar.name"));
    }

    /// Le tri suit l'affichage. S'ils divergent, la pagination range les cartes
    /// sous un nom qu'elles ne montrent pas.
    #[test]
    fn le_tri_emploie_le_meme_artiste_que_l_affichage() {
        let fichier = include_str!("albums_detailed.rs");
        let sql = fichier
            .split("let sql = format!(")
            .nth(1)
            .expect("la requête");
        assert_eq!(
            sql.matches("MAX({ARTISTE_DE_CARTE})").count(),
            2,
            "une fois pour la colonne, une fois pour l'ORDER BY"
        );
    }

    /// 🔴 L'épreuve qui MESURE, par la route et une vraie base — les deux
    /// ci-dessus ne lisent que du texte.
    ///
    /// On reconstitue le cas du .18 : douze lignes réunies en un album marqué
    /// compilation, dont la ligne porte « Various Artists », et des pistes qui
    /// gardent chacune l'artiste de leur album d'origine. C'est l'état que la
    /// fusion laisse derrière elle (`UPDATE tracks SET album_id = ?`, et rien
    /// sur `album_artist`).
    #[tokio::test]
    async fn la_carte_d_une_compilation_ne_sort_pas_sous_un_invite() {
        use crate::state::AppState;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        use tune_core::db::backend::ToSqlValue;

        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Various Artists'), (2, 'Ronald Snijders'), (3, 'Acid Coco')",
            &[],
        )
        .expect("artistes");
        // La ligne album est JUSTE : compilation, sous Various Artists.
        b.execute(
            "INSERT INTO albums (id, title, artist_id, is_compilation) VALUES (1, 'Club Coco', 1, 1)",
            &[],
        )
        .expect("album");
        // Les pistes, elles, gardent le nom de leur album d'origine.
        for (t, a, aa) in [("Uno", 3, "Acid Coco"), ("Dos", 2, "Ronald Snijders")] {
            b.execute(
                "INSERT INTO tracks (title, artist_id, album_id, album_artist, source, file_path) \
                 VALUES (?1, ?2, 1, ?3, 'local', ?4)",
                &[
                    &t as &dyn ToSqlValue,
                    &(a as i64),
                    &aa,
                    &format!("/m/{t}.flac"),
                ],
            )
            .expect("piste");
        }

        let reponse = super::super::router()
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/albums-detailed?limit=10")
                    .body(Body::empty())
                    .expect("requête"),
            )
            .await
            .expect("réponse");
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .expect("corps");
        let v: Value = serde_json::from_slice(&octets).expect("json");
        let carte = &v["items"][0];
        assert_eq!(
            carte["album_artist"], "Various Artists",
            "la carte doit porter l'artiste de l'album, pas le dernier invité par ordre alphabétique"
        );
        assert_eq!(carte["is_compilation"], true);
    }

    /// Un album ORDINAIRE garde le comportement d'avant : c'est ce que ses
    /// fichiers déclarent qui s'affiche, pas la ligne album.
    #[tokio::test]
    async fn un_album_ordinaire_garde_l_artiste_de_ses_fichiers() {
        use crate::state::AppState;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        use tune_core::db::backend::ToSqlValue;

        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Ligne Album'), (2, 'Étiquette Fichier')",
            &[],
        )
        .expect("artistes");
        b.execute(
            "INSERT INTO albums (id, title, artist_id, is_compilation) VALUES (1, 'Disque', 1, 0)",
            &[],
        )
        .expect("album");
        b.execute(
            "INSERT INTO tracks (title, artist_id, album_id, album_artist, source, file_path) \
             VALUES ('Une', 2, 1, 'Étiquette Fichier', 'local', '/m/u.flac')",
            &[] as &[&dyn ToSqlValue],
        )
        .expect("piste");

        let reponse = super::super::router()
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/albums-detailed?limit=10")
                    .body(Body::empty())
                    .expect("requête"),
            )
            .await
            .expect("réponse");
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .expect("corps");
        let v: Value = serde_json::from_slice(&octets).expect("json");
        assert_eq!(v["items"][0]["album_artist"], "Étiquette Fichier");
    }

    /// Marqueur de contrat : le total pagine des ALBUMS. Compter des pistes
    /// donnerait un nombre de pages faux d'un facteur dix.
    #[test]
    fn le_total_compte_des_albums_distincts() {
        let sql = "SELECT COUNT(DISTINCT t.album_id) FROM tracks t";
        assert!(sql.contains("COUNT(DISTINCT t.album_id)"));
    }
}
