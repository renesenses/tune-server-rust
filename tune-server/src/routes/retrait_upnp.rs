//! Retrait d'une bibliothèque UPnP **par serveur média** (#4624).
//!
//! Jean Valjean (fil forum 1869) a indexé son serveur MusicBee « pour voir »,
//! et rien ne retire ce qui est entré : « Indexer » et « Suspendre la
//! synchronisation » existent, « Retirer » n'existe pas. Le seul chemin qui
//! supprime — `confirm` dans `synchronisation_upnp.rs` — exige un Browse
//! COMPLET du serveur distant, donc un serveur ALLUMÉ : il est inaccessible
//! précisément dans le cas où l'on veut retirer.
//!
//! ## La borne retenue, et pourquoi ce n'est pas l'appartenance
//!
//! `remove_missing` borne ses candidats au jeu `upnp_library_members` de la
//! source. Une **indexation ponctuelle** (bouton « Indexer ») n'écrit AUCUNE
//! ligne dans cette table : ces deux tables ne sont alimentées que par
//! l'abonnement. Une borne par appartenance ne retirerait donc rien dans le
//! cas du testeur.
//!
//! Mesuré en lecture seule sur le .18 le 22/09/2026 (SQLite `tune_v2.db`) :
//!
//! ```text
//! sqlite> SELECT source, COUNT(*) FROM tracks GROUP BY source;
//! local|46939
//! upnp|179
//! sqlite> SELECT id, substr(source_id,1,80) FROM tracks WHERE source='upnp' LIMIT 1;
//! 46878|uuid:258FC2D5-E2C3-B734-0-123456789abc|49ad2f0775435943
//! sqlite> SELECT COUNT(*) FROM tracks t WHERE t.source='upnp'
//!    ...>   AND t.id NOT IN (SELECT track_id FROM upnp_library_members);
//! 1
//! ```
//!
//! Le lien vers le serveur média EXISTE donc déjà, et il est indépendant de
//! l'abonnement : `tracks.source = 'upnp'` et `tracks.source_id` **préfixé par
//! `<udn>|`**. Rien n'est à ajouter au scan. C'est la borne de ce module — la
//! même que la seconde moitié de la garde de `remove_missing`, sans la
//! première.
//!
//! ## Les trois décisions de Bertrand (22/09/2026)
//!
//! 1. le retrait se fait **par serveur média**, pas par source abonnée : une
//!    seule action retire tout ce qui vient de ce serveur ;
//! 2. favoris et playlists **ne bloquent pas** — contrairement à
//!    `remove_missing`, qui refuse en silence — mais l'aperçu **annonce les
//!    nombres** et le client confirme ;
//! 3. l'action vit **dans la ligne du serveur**, écran Serveurs multimédia.
//!
//! Le retrait emporte aussi la ligne `upnp_library_sources` du serveur (point 4
//! de l'issue) : sans cela la boucle de `synchronisation_upnp::start` ramène
//! les pistes au plus tard une heure après.
//!
//! Aucun fichier distant n'est touché : ce module n'écrit que dans la base.

use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn erreur(code: StatusCode, message: impl ToString) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error": message.to_string()})))
}
fn interne(e: impl ToString) -> (StatusCode, Json<Value>) {
    erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
}

/// Le préfixe d'identité d'un serveur dans `source_id`. `indexation_upnp`
/// écrit `<udn>|<empreinte>` ; `remove_missing` lit le même préfixe.
fn prefixe(udn: &str) -> String {
    format!("{udn}|")
}

/// `source = 'upnp'` ET `source_id` commençant par `<udn>|`.
///
/// `substr(col, 1, n)` plutôt que `LIKE` : un UDN peut contenir `_`, qui est un
/// joker `LIKE`. La longueur est un entier calculé en Rust, jamais une entrée
/// utilisateur ; le préfixe lui-même reste un paramètre lié.
fn borne(alias: &str, prefixe: &str) -> String {
    let n = prefixe.chars().count();
    format!("{alias}.source = 'upnp' AND substr({alias}.source_id, 1, {n}) = ?")
}

/// La même expression de liens utilisateur que `synchronisation_upnp`, mais
/// pour COMPTER au lieu d'interdire.
fn lien_favori(alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM favorites f WHERE
            (f.item_type = 'track' AND f.item_id = CAST({alias}.id AS TEXT))
            OR (f.item_type = 'album' AND f.item_id = CAST({alias}.album_id AS TEXT)))"
    )
}
fn lien_playlist(alias: &str) -> String {
    format!("EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.track_id = {alias}.id)")
}

/// Ce que l'écran annonce avant de demander confirmation.
#[derive(Debug, PartialEq, Eq)]
pub struct Apercu {
    pub pistes: i64,
    pub albums: i64,
    pub favoris: i64,
    pub playlists: i64,
    pub sources: i64,
}

fn compte(state: &AppState, sql: &str, prefixe: &str) -> Result<i64, String> {
    let params: [&dyn ToSqlValue; 1] = [&prefixe];
    Ok(state
        .backend
        .query_one(sql, &params)?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0))
}

/// Compte, sans rien écrire, ce qu'un retrait emporterait.
pub fn apercu(state: &AppState, udn: &str) -> Result<Apercu, String> {
    let p = prefixe(udn);
    let piste = borne("t", &p);
    let album = borne("a", &p);
    let pistes = compte(
        state,
        &format!("SELECT COUNT(*) FROM tracks t WHERE {piste}"),
        &p,
    )?;
    let favoris = compte(
        state,
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {piste} AND {}",
            lien_favori("t")
        ),
        &p,
    )?;
    let playlists = compte(
        state,
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {piste} AND {}",
            lien_playlist("t")
        ),
        &p,
    )?;
    let albums = compte(
        state,
        &format!("SELECT COUNT(*) FROM albums a WHERE {album}"),
        &p,
    )?;
    let params: [&dyn ToSqlValue; 1] = [&udn];
    let sources = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM upnp_library_sources WHERE udn = ?",
            &params,
        )?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    Ok(Apercu {
        pistes,
        albums,
        favoris,
        playlists,
        sources,
    })
}

fn public(a: &Apercu) -> Value {
    json!({"pistes": a.pistes, "albums": a.albums, "favoris": a.favoris,
        "playlists": a.playlists, "sources": a.sources})
}

/// Retire toute la bibliothèque venue de `udn`.
///
/// `attendu` est le nombre de pistes que le client a **affiché** à
/// l'utilisateur : s'il ne correspond plus, on refuse plutôt que de supprimer
/// autre chose que ce qui a été confirmé. C'est la garde de `confirm`, reprise
/// telle quelle.
pub fn retirer(state: &AppState, udn: &str, attendu: Option<i64>) -> Result<Apercu, String> {
    let p = prefixe(udn);
    let vu = apercu(state, udn)?;
    if let Some(attendu) = attendu
        && attendu != vu.pistes
    {
        return Err(format!(
            "Le nombre a changé : {} pistes à retirer, {attendu} annoncées. Relisez le compte avant de confirmer.",
            vu.pistes
        ));
    }
    let mut retirees = 0i64;
    let retirees_ref = &mut retirees;
    let udn_owned = udn.to_string();
    state.backend.write_tx(&mut |tx| {
        let params: [&dyn ToSqlValue; 1] = [&p];
        let ids: Vec<i64> = tx
            .query_many(
                &format!("SELECT t.id FROM tracks t WHERE {}", borne("t", &p)),
                &params,
            )?
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        for lot in ids.chunks(500) {
            // Construit exclusivement depuis des i64 : jamais une entrée
            // utilisateur. Même précaution que `liens_utilisateur_sql`.
            let liste = lot.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let textes = lot
                .iter()
                .map(|id| format!("'{id}'"))
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                &format!("DELETE FROM upnp_library_members WHERE track_id IN ({liste})"),
                &[],
            )?;
            tx.execute(
                &format!("DELETE FROM playlist_tracks WHERE track_id IN ({liste})"),
                &[],
            )?;
            // `favorites` ne porte AUCUNE clé étrangère (ni SQLite ni PG) :
            // sans ce ménage, un favori survivrait à sa piste.
            tx.execute(
                &format!(
                    "DELETE FROM favorites WHERE item_type = 'track' AND CAST(item_id AS TEXT) IN ({textes})"
                ),
                &[],
            )?;
            // La cascade existe sur un schéma neuf mais manque sur les bases
            // migrées (cf. `TrackRepo::delete`).
            let _ = tx.execute(
                &format!("DELETE FROM queue_items WHERE track_id IN ({liste})"),
                &[],
            );
            *retirees_ref +=
                tx.execute(&format!("DELETE FROM tracks WHERE id IN ({liste})"), &[])? as i64;
        }
        // Seuls les albums DISTANTS de CE serveur, et seulement une fois vides.
        // Un album local n'est jamais touché.
        let albums: Vec<i64> = tx
            .query_many(
                &format!(
                    "SELECT a.id FROM albums a WHERE {} AND NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.album_id = a.id)",
                    borne("a", &p)
                ),
                &params,
            )?
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        for lot in albums.chunks(500) {
            let liste = lot.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let textes = lot
                .iter()
                .map(|id| format!("'{id}'"))
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                &format!(
                    "DELETE FROM favorites WHERE item_type = 'album' AND CAST(item_id AS TEXT) IN ({textes})"
                ),
                &[],
            )?;
            tx.execute(
                &format!("DELETE FROM albums WHERE id IN ({liste}) AND source = 'upnp'"),
                &[],
            )?;
        }
        // Sans ce dernier retrait, la boucle de `synchronisation_upnp::start`
        // ramène tout au plus tard une heure après : l'abonnement survivrait au
        // retrait. La cascade emporte `upnp_library_members`.
        let params: [&dyn ToSqlValue; 1] = [&udn_owned];
        tx.execute(
            "DELETE FROM upnp_library_members WHERE source_key IN (SELECT source_key FROM upnp_library_sources WHERE udn = ?)",
            &params,
        )?;
        tx.execute("DELETE FROM upnp_library_sources WHERE udn = ?", &params)?;
        Ok(())
    })?;
    Ok(Apercu {
        pistes: retirees,
        ..vu
    })
}

/// `GET /network/media-servers/{id}/bibliotheque` — ne modifie rien.
pub async fn apercu_du_retrait(
    State(state): State<AppState>,
    Path(udn): Path<String>,
) -> ApiResult {
    Ok(Json(public(&apercu(&state, &udn).map_err(interne)?)))
}

#[derive(Deserialize)]
pub struct Confirmation {
    /// Le nombre de pistes que l'écran a annoncé à l'utilisateur.
    pistes: Option<i64>,
}

/// `DELETE /network/media-servers/{id}/bibliotheque?pistes=N`.
///
/// Fonctionne **serveur distant éteint** : aucun Browse, aucune sortie réseau.
pub async fn retrait_de_la_bibliotheque(
    State(state): State<AppState>,
    Path(udn): Path<String>,
    Query(confirmation): Query<Confirmation>,
) -> ApiResult {
    let _garde = state.upnp_index_lock.try_lock().map_err(|_| {
        erreur(
            StatusCode::CONFLICT,
            "Une indexation est en cours. Réessayez après sa fin.",
        )
    })?;
    let vu = apercu(&state, &udn).map_err(interne)?;
    if vu.pistes == 0 && vu.albums == 0 && vu.sources == 0 {
        return Err(erreur(
            StatusCode::NOT_FOUND,
            "Aucune piste de ce serveur dans la bibliothèque",
        ));
    }
    match retirer(&state, &udn, confirmation.pistes) {
        Ok(fait) => Ok(Json(public(&fait))),
        // Un compte qui a bougé est un conflit, pas une panne : le client
        // relit l'aperçu et redemande confirmation.
        Err(e) if e.starts_with("Le nombre a changé") => Err(erreur(StatusCode::CONFLICT, e)),
        Err(e) => Err(interne(e)),
    }
}

#[cfg(test)]
mod retrait_upnp_tests {
    use super::*;
    use tune_core::db::{
        album_repo::AlbumRepo,
        models::{Album, Track},
        playlist_repo::PlaylistRepo,
        track_repo::TrackRepo,
    };

    const A: &str = "uuid:serveur-A";
    const B: &str = "uuid:serveur-B";

    fn piste(state: &AppState, titre: &str, source: &str, source_id: Option<&str>) -> i64 {
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let mut track = Track::new(titre.into());
        track.source = source.into();
        track.source_id = source_id.map(str::to_string);
        if source == "local" {
            track.file_path = Some(format!("/musique/{titre}.flac"));
        }
        tracks.create(&track).unwrap()
    }

    fn album(state: &AppState, titre: &str, source_id: &str) -> i64 {
        let albums = AlbumRepo::with_backend(state.backend.clone());
        let mut album = Album::new(titre.into());
        album.source = "upnp".into();
        album.source_id = Some(source_id.into());
        albums.create(&album).unwrap()
    }

    /// Le cœur de #4624 : le retrait est borné au SERVEUR, il n'épargne rien de
    /// ce serveur et il ne touche rien d'un autre — ni du local.
    #[test]
    fn retrait_upnp_borne_au_serveur_epargne_lautre_serveur_et_le_local() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let a1 = piste(&state, "A1", "upnp", Some(&format!("{A}|1")));
        let a2 = piste(&state, "A2", "upnp", Some(&format!("{A}|2")));
        let b1 = piste(&state, "B1", "upnp", Some(&format!("{B}|1")));
        let locale = piste(&state, "Locale", "local", None);

        assert_eq!(apercu(&state, A).unwrap().pistes, 2, "l'aperçu compte 2");
        let fait = retirer(&state, A, Some(2)).unwrap();
        assert_eq!(fait.pistes, 2, "deux pistes retirées");

        assert!(tracks.get(a1).unwrap().is_none(), "A1 doit partir");
        assert!(tracks.get(a2).unwrap().is_none(), "A2 doit partir");
        assert!(
            tracks.get(b1).unwrap().is_some(),
            "la piste de l'AUTRE serveur reste"
        );
        assert!(
            tracks.get(locale).unwrap().is_some(),
            "la bibliothèque locale reste intacte"
        );
    }

    /// Le cas exact de Jean Valjean : indexation ponctuelle, donc AUCUNE ligne
    /// d'appartenance. Une borne par `upnp_library_members` ne retirerait rien ;
    /// celle-ci retire.
    #[test]
    fn retrait_upnp_retire_une_piste_sans_ligne_dappartenance() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let ponctuelle = piste(&state, "Ponctuelle", "upnp", Some(&format!("{A}|1")));
        assert_eq!(
            state
                .backend
                .query_one("SELECT COUNT(*) FROM upnp_library_members", &[])
                .unwrap()
                .unwrap()[0]
                .as_i64(),
            Some(0),
            "témoin : aucune appartenance, comme après une indexation ponctuelle"
        );
        assert_eq!(retirer(&state, A, None).unwrap().pistes, 1);
        assert!(
            tracks.get(ponctuelle).unwrap().is_none(),
            "une piste sans appartenance doit pouvoir être retirée"
        );
    }

    /// Décision 2 de Bertrand : favoris et playlists sont ANNONCÉS, ils ne
    /// bloquent pas. Et ils ne laissent pas d'orphelin derrière eux.
    #[test]
    fn retrait_upnp_annonce_favoris_et_playlists_puis_retire_sans_orphelin() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let aid = album(&state, "Album distant", &format!("{A}|album"));
        let favorite = piste(&state, "Favorite", "upnp", Some(&format!("{A}|1")));
        let enliste = piste(&state, "Enlistée", "upnp", Some(&format!("{A}|2")));
        let simple = piste(&state, "Simple", "upnp", Some(&format!("{A}|3")));
        for id in [favorite, enliste, simple] {
            let mut t = tracks.get(id).unwrap().unwrap();
            t.album_id = Some(aid);
            tracks.update(&t).unwrap();
        }
        state
            .backend
            .execute(
                "INSERT INTO favorites (profile_id,item_type,item_id) VALUES (1,'track',?)",
                &[&favorite.to_string()],
            )
            .unwrap();
        let playlists = PlaylistRepo::with_backend(state.backend.clone());
        let pid = playlists.create("Garder", None, 1).unwrap();
        playlists.add_tracks(pid, &[enliste], None).unwrap();

        let vu = apercu(&state, A).unwrap();
        assert_eq!(vu.pistes, 3, "trois pistes annoncées");
        assert_eq!(vu.favoris, 1, "un favori annoncé");
        assert_eq!(vu.playlists, 1, "une piste de playlist annoncée");
        assert_eq!(vu.albums, 1, "un album annoncé");

        assert_eq!(
            retirer(&state, A, Some(3)).unwrap().pistes,
            3,
            "un favori ou une playlist ne bloque PAS le retrait"
        );
        assert!(tracks.get(favorite).unwrap().is_none());
        assert!(tracks.get(enliste).unwrap().is_none());
        assert_eq!(
            state
                .backend
                .query_one("SELECT COUNT(*) FROM favorites", &[])
                .unwrap()
                .unwrap()[0]
                .as_i64(),
            Some(0),
            "aucun favori orphelin ne survit à sa piste"
        );
        assert_eq!(
            state
                .backend
                .query_one("SELECT COUNT(*) FROM playlist_tracks", &[])
                .unwrap()
                .unwrap()[0]
                .as_i64(),
            Some(0),
            "aucune ligne de playlist orpheline ne survit à sa piste"
        );
        assert_eq!(
            state
                .backend
                .query_one("SELECT COUNT(*) FROM albums", &[])
                .unwrap()
                .unwrap()[0]
                .as_i64(),
            Some(0),
            "l'album distant vidé part avec ses pistes"
        );
    }

    /// Point 4 de l'issue : sans cela, la boucle horaire ramène tout.
    #[test]
    fn retrait_upnp_emporte_labonnement_et_ses_appartenances() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let id = piste(&state, "Abonnée", "upnp", Some(&format!("{A}|1")));
        state.backend.execute(
            "INSERT INTO upnp_library_sources (source_key,udn,container,state_json) VALUES ('s',?,'0','{}')",
            &[&A],
        ).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO upnp_library_members (source_key,track_id,generation) VALUES ('s',?,'g')",
                &[&id],
            )
            .unwrap();
        retirer(&state, A, None).unwrap();
        for table in ["upnp_library_sources", "upnp_library_members"] {
            assert_eq!(
                state
                    .backend
                    .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
                    .unwrap()
                    .unwrap()[0]
                    .as_i64(),
                Some(0),
                "{table} doit être vidée, sinon la synchronisation ramène tout"
            );
        }
    }

    /// Ne jamais supprimer autre chose que ce qui a été confirmé à l'écran.
    #[test]
    fn retrait_upnp_refuse_un_compte_perime() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let id = piste(&state, "A1", "upnp", Some(&format!("{A}|1")));
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let erreur = retirer(&state, A, Some(7)).unwrap_err();
        assert!(
            erreur.starts_with("Le nombre a changé"),
            "message attendu, reçu : {erreur}"
        );
        assert!(
            tracks.get(id).unwrap().is_some(),
            "un compte périmé ne supprime rien"
        );
    }

    /// Un UDN portant `_` est un motif `LIKE` : la borne ne doit pas déborder.
    #[test]
    fn retrait_upnp_un_underscore_dans_ludn_ne_deborde_pas() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let voisine = piste(&state, "Voisine", "upnp", Some("uuid:aXb|1"));
        let ciblee = piste(&state, "Ciblée", "upnp", Some("uuid:a_b|1"));
        assert_eq!(apercu(&state, "uuid:a_b").unwrap().pistes, 1);
        retirer(&state, "uuid:a_b", Some(1)).unwrap();
        assert!(tracks.get(ciblee).unwrap().is_none());
        assert!(
            tracks.get(voisine).unwrap().is_some(),
            "`_` est un joker LIKE : la borne ne doit pas l'interpréter"
        );
    }
}
