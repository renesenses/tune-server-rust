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
use tune_core::db::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};

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
pub(crate) fn lien_favori(alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM favorites f WHERE
            (f.item_type = 'track' AND {piste})
            OR (f.item_type = 'album' AND {album}))",
        piste = super::synchronisation_upnp::favori_designe(&format!("{alias}.id")),
        album = super::synchronisation_upnp::favori_designe(&format!("{alias}.album_id")),
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

/// Une lecture, par la base ou dans une transaction : l'aperçu et le plan de
/// rattachement se calculent de la même façon des deux côtés.
pub(crate) trait Lire {
    fn un(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String>;
    fn tous(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String>;
}
pub(crate) struct ParLaBase<'a>(pub &'a dyn DbBackend);
struct DansLaTx<'a>(&'a dyn DbTxHandle);
impl Lire for ParLaBase<'_> {
    fn un(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String> {
        self.0.query_one(sql, p)
    }
    fn tous(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        self.0.query_many(sql, p)
    }
}
impl Lire for DansLaTx<'_> {
    fn un(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String> {
        self.0.query_one(sql, p)
    }
    fn tous(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        self.0.query_many(sql, p)
    }
}

fn compte(l: &dyn Lire, sql: &str, params: &[&dyn ToSqlValue]) -> Result<i64, String> {
    Ok(l.un(sql, params)?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0))
}

/// Compte, sans rien écrire, ce qu'un retrait emporterait.
pub fn apercu(state: &AppState, udn: &str) -> Result<Apercu, String> {
    apercu_par(&ParLaBase(state.backend.as_ref()), udn)
}

fn apercu_par(l: &dyn Lire, udn: &str) -> Result<Apercu, String> {
    let p = prefixe(udn);
    let pp: [&dyn ToSqlValue; 1] = [&p];
    let piste = borne("t", &p);
    let album = borne("a", &p);
    let pistes = compte(
        l,
        &format!("SELECT COUNT(*) FROM tracks t WHERE {piste}"),
        &pp,
    )?;
    let favoris = compte(
        l,
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {piste} AND {}",
            lien_favori("t")
        ),
        &pp,
    )?;
    let playlists = compte(
        l,
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {piste} AND {}",
            lien_playlist("t")
        ),
        &pp,
    )?;
    let albums = compte(
        l,
        &format!("SELECT COUNT(*) FROM albums a WHERE {album}"),
        &pp,
    )?;
    let pu: [&dyn ToSqlValue; 1] = [&udn];
    let sources = compte(
        l,
        "SELECT COUNT(*) FROM upnp_library_sources WHERE udn = ?",
        &pu,
    )?;
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
    state.backend.write_tx(&mut |tx| {
        retirees = retirer_dans(tx, udn)?;
        Ok(())
    })?;
    Ok(Apercu {
        pistes: retirees,
        ..vu
    })
}

/// Le retrait lui-même, dans la transaction de l'appelant. Rend le nombre de
/// pistes retirées.
fn retirer_dans(tx: &dyn DbTxHandle, udn: &str) -> Result<i64, String> {
    let p = prefixe(udn);
    let mut retirees = 0i64;
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
        retirees += tx.execute(&format!("DELETE FROM tracks WHERE id IN ({liste})"), &[])? as i64;
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
    let params: [&dyn ToSqlValue; 1] = [&udn];
    tx.execute(
        "DELETE FROM upnp_library_members WHERE source_key IN (SELECT source_key FROM upnp_library_sources WHERE udn = ?)",
        &params,
    )?;
    tx.execute("DELETE FROM upnp_library_sources WHERE udn = ?", &params)?;
    Ok(retirees)
}

/// Fil du forum 3028 — d'où l'on reconnaît une URL de NOTRE bibliothèque :
/// nos adresses (IPv4 et IPv6, `discovery_setup::nos_adresses`) et notre
/// port d'API. Calculé une fois, hors de la transaction.
#[derive(Clone, Debug)]
pub struct NotreAdresse {
    pub adresses: Vec<String>,
    pub port: u16,
    /// Les AUTRES serveurs connus du registre (`hôte`, `port`), ceux que
    /// `est_ce_serveur` ne reconnaît pas comme nous : un autre Tune du réseau
    /// y figure sous son UDN.
    pub autres: Vec<(String, u16)>,
}

impl NotreAdresse {
    pub fn de(state: &AppState) -> Self {
        let autres =
            tune_core::db::media_server_repo::MediaServerRepo::with_backend(state.backend.clone())
                .lister()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| {
                    let hote = s.host.clone().filter(|h| !h.is_empty())?;
                    let port = s.port.and_then(|p| u16::try_from(p).ok())?;
                    (!crate::discovery_setup::est_ce_serveur(
                        state,
                        &s.udn,
                        &hote,
                        port,
                        &s.location,
                    ))
                    .then_some((hote, port))
                })
                .collect();
        Self {
            adresses: crate::discovery_setup::nos_adresses(state.config.advertised_ip.as_deref()),
            port: state.config.port,
            autres,
        }
    }

    /// L'hôte d'une URL, sans les crochets d'une IPv6 (`Ipv6Addr::to_string`).
    fn hote_et_port(url: &reqwest::Url) -> Option<(&str, u16)> {
        let hote = url.host_str()?;
        let hote = hote.trim_start_matches('[').trim_end_matches(']');
        Some((hote, url.port_or_known_default()?))
    }

    /// L'URL vise-t-elle CE serveur (hôte ET port) ?
    fn designe_ce_serveur(&self, url: &reqwest::Url) -> bool {
        Self::hote_et_port(url).is_some_and(|(hote, port)| {
            port == self.port && self.adresses.iter().any(|a| a == hote)
        })
    }

    /// L'URL vise-t-elle un AUTRE serveur connu du registre ?
    fn designe_un_autre_serveur(&self, url: &reqwest::Url) -> bool {
        Self::hote_et_port(url)
            .is_some_and(|(hote, port)| self.autres.iter().any(|(h, p)| h == hote && *p == port))
    }
}

/// L'identifiant de piste que porte une URL de lecture d'un Tune
/// (`…/api/v1/library/tracks/<id>/audio`), quel que soit l'hôte.
fn id_d_une_url_de_tune(url: &reqwest::Url) -> Option<i64> {
    url.path()
        .strip_prefix(tune_core::upnp_server::API_PATH)?
        .strip_prefix("/library/tracks/")?
        .strip_suffix("/audio")?
        .parse()
        .ok()
        .filter(|id| *id > 0)
}

const PISTE_LOCALE: &str = "(l.source IS NULL OR l.source = 'local') AND l.file_path IS NOT NULL";

/// Ce qu'une piste porte pour être comparée : titre, artiste, album (sans
/// casse ni blancs de bord), durée, album en base.
#[derive(Clone, Debug)]
struct Signature {
    id: i64,
    titre: String,
    artiste: String,
    album: String,
    duree: i64,
    album_id: Option<i64>,
}

fn signatures(l: &dyn Lire, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Signature>, String> {
    let texte = |v: Option<&SqlValue>| {
        v.and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default()
    };
    Ok(l.tous(sql, p)?
        .iter()
        .filter_map(|r| {
            Some(Signature {
                id: r.first()?.as_i64()?,
                titre: texte(r.get(1)),
                artiste: texte(r.get(2)),
                album: texte(r.get(3)),
                duree: r.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                album_id: r.get(5).and_then(|v| v.as_i64()),
            })
        })
        .collect())
}

/// La signature est-elle assez complète pour fonder une équivalence ? Un titre
/// seul ne prouve rien : artiste, album et durée doivent être connus.
fn complete(s: &Signature) -> bool {
    !s.titre.is_empty() && !s.artiste.is_empty() && !s.album.is_empty() && s.duree > 0
}

fn concorde(d: &Signature, l: &Signature) -> bool {
    complete(l)
        && d.titre == l.titre
        && d.artiste == l.artiste
        && d.album == l.album
        && (d.duree - l.duree).abs() <= 2000
}

/// La piste LOCALE équivalente d'une piste distante, ou aucune.
///
/// - Signature distante incomplète (artiste, album ou durée inconnus) :
///   aucune. Le titre seul ne suffit pas.
/// - URL de lecture d'un Tune (`/library/tracks/<id>/audio`) :
///   - qui vise CE serveur (hôte et port) : la piste qu'elle nomme doit
///     concorder (titre, artiste, album, durée à 2 s), sinon aucune ;
///   - qui vise un AUTRE serveur connu du registre (un autre Tune, sous son
///     UDN) : aucune, même si une piste locale a les mêmes étiquettes ;
///   - dont l'hôte n'est pas reconnu (IP DHCP changée, port redirigé, nom
///     d'hôte au lieu de l'IP), le plus souvent nous-mêmes : l'identifiant
///     de l'URL est ignoré, et la règle des étiquettes ci-dessous s'applique.
/// - Sinon, une seule piste locale concordante. Deux ou plus : aucune.
fn equivalent(
    d: &Signature,
    url: Option<&str>,
    locales: &[Signature],
    nous: &NotreAdresse,
) -> Option<i64> {
    if !complete(d) {
        return None;
    }
    let url = url.and_then(|u| reqwest::Url::parse(u.trim()).ok());
    if let Some(url) = url.as_ref()
        && let Some(nomme) = id_d_une_url_de_tune(url)
    {
        if nous.designe_ce_serveur(url) {
            return locales
                .iter()
                .find(|l| l.id == nomme && concorde(d, l))
                .map(|l| l.id);
        }
        if nous.designe_un_autre_serveur(url) {
            return None;
        }
    }
    let mut candidates = locales.iter().filter(|l| concorde(d, l));
    match (candidates.next(), candidates.next()) {
        (Some(une), None) => Some(une.id),
        _ => None,
    }
}

/// Fil du forum 3028 — le plan d'un retrait : ce qui part, ce qui est
/// rattaché, ce qui n'a pas d'équivalent.
///
/// Un lien utilisateur (favori de piste ou d'album, entrée de playlist) porté
/// par une piste distante est REPORTÉ sur la piste locale équivalente avant le
/// retrait. Ce qui n'a pas d'équivalent est compté et annoncé. Le calcul tient
/// en un nombre FIXE de requêtes, quel que soit le nombre de pistes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `(nature, distant, local)` : `track` ou `album`.
    pub rattachements: Vec<(&'static str, i64, i64)>,
    /// Pistes et albums liés SANS équivalent local sûr.
    pub sans_equivalent: i64,
    /// Les pistes que le retrait emportera.
    pub pistes: Vec<i64>,
}

fn plan_par(l: &dyn Lire, udn: &str, nous: &NotreAdresse) -> Result<Plan, String> {
    let p = prefixe(udn);
    let pp: [&dyn ToSqlValue; 1] = [&p];
    let colonnes = |t: &str| {
        format!(
            "SELECT {t}.id, {t}.title, ar.name, al.title, {t}.duration_ms, {t}.album_id \
             FROM tracks {t} LEFT JOIN artists ar ON ar.id = {t}.artist_id \
             LEFT JOIN albums al ON al.id = {t}.album_id"
        )
    };
    let distantes = signatures(
        l,
        &format!("{} WHERE {} ORDER BY t.id", colonnes("t"), borne("t", &p)),
        &pp,
    )?;
    let locales = signatures(
        l,
        &format!(
            "{} WHERE {PISTE_LOCALE} AND lower(trim(l.title)) IN \
             (SELECT lower(trim(t.title)) FROM tracks t WHERE {})",
            colonnes("l"),
            borne("t", &p)
        ),
        &pp,
    )?;
    let urls: std::collections::HashMap<String, String> = l
        .tous(
            &format!(
                "SELECT CAST(m.track_id AS TEXT), m.value FROM track_metadata m \
                 WHERE m.key = 'upnp_res_url' AND CAST(m.track_id AS TEXT) IN \
                 (SELECT CAST(t.id AS TEXT) FROM tracks t WHERE {})",
                borne("t", &p)
            ),
            &pp,
        )?
        .iter()
        .filter_map(|r| Some((r.first()?.as_string()?, r.get(1)?.as_string()?)))
        .collect();
    let equivalents: std::collections::HashMap<i64, Option<i64>> = distantes
        .iter()
        .map(|d| {
            let url = urls.get(&d.id.to_string()).map(String::as_str);
            (d.id, equivalent(d, url, &locales, nous))
        })
        .collect();
    let album_local: std::collections::HashMap<i64, Option<i64>> =
        locales.iter().map(|s| (s.id, s.album_id)).collect();

    let mut plan = Plan {
        pistes: distantes.iter().map(|d| d.id).collect(),
        ..Plan::default()
    };
    let pistes_liees = ids(l.tous(
        &format!(
            "SELECT t.id FROM tracks t WHERE {} AND (EXISTS (SELECT 1 FROM favorites f \
             WHERE f.item_type = 'track' AND {}) OR {}) ORDER BY t.id",
            borne("t", &p),
            super::synchronisation_upnp::favori_designe("t.id"),
            lien_playlist("t"),
        ),
        &pp,
    )?);
    for piste in pistes_liees {
        match equivalents.get(&piste).copied().flatten() {
            Some(locale) => plan.rattachements.push(("track", piste, locale)),
            None => plan.sans_equivalent += 1,
        }
    }
    let albums_lies = ids(l.tous(
        &format!(
            "SELECT a.id FROM albums a WHERE {} AND EXISTS (SELECT 1 FROM favorites f \
             WHERE f.item_type = 'album' AND {}) ORDER BY a.id",
            borne("a", &p),
            super::synchronisation_upnp::favori_designe("a.id"),
        ),
        &pp,
    )?);
    for album in albums_lies {
        let pistes: Vec<&Signature> = distantes
            .iter()
            .filter(|d| d.album_id == Some(album))
            .collect();
        let cibles: Option<std::collections::BTreeSet<i64>> = pistes
            .iter()
            .map(|d| {
                equivalents
                    .get(&d.id)
                    .copied()
                    .flatten()
                    .and_then(|l| album_local.get(&l).copied().flatten())
            })
            .collect();
        match cibles {
            Some(c) if !pistes.is_empty() && c.len() == 1 => {
                let locale = *c.iter().next().expect("un élément");
                plan.rattachements.push(("album", album, locale));
            }
            _ => plan.sans_equivalent += 1,
        }
    }
    Ok(plan)
}

/// Reporte un favori `nature` de `ancien` sur `nouveau`, profil par profil,
/// sans créer de doublon : un profil qui a déjà `nouveau` en favori garde le
/// sien, et l'ancien part avec le retrait.
fn reporter_le_favori(
    tx: &dyn DbTxHandle,
    nature: &str,
    ancien: i64,
    nouveau: i64,
) -> Result<(), String> {
    let (ancien, nouveau_texte) = (ancien.to_string(), nouveau.to_string());
    let params: [&dyn ToSqlValue; 5] = [&nouveau, &nature, &ancien, &nature, &nouveau_texte];
    tx.execute(
        "UPDATE favorites SET item_id = ? WHERE item_type = ? AND CAST(item_id AS TEXT) = ? \
         AND NOT EXISTS (SELECT 1 FROM favorites g WHERE g.item_type = ? \
           AND CAST(g.item_id AS TEXT) = ? \
           AND CAST(g.profile_id AS TEXT) = CAST(favorites.profile_id AS TEXT))",
        &params,
    )?;
    Ok(())
}

fn ids(lignes: Vec<Vec<SqlValue>>) -> Vec<i64> {
    lignes
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// L'empreinte du plan : ce que la confirmation doit retrouver À L'IDENTIQUE
/// (pistes à retirer, rattachements, liens sans équivalent).
fn empreinte(plan: &Plan) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    plan.pistes.hash(&mut h);
    plan.rattachements.hash(&mut h);
    plan.sans_equivalent.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn plan_public(a: &Apercu, plan: &Plan) -> Value {
    let mut v = public(a);
    v["liens_rattaches"] = json!(plan.rattachements.len());
    v["liens_sans_equivalent"] = json!(plan.sans_equivalent);
    v["empreinte"] = json!(empreinte(plan));
    v
}

/// L'aperçu d'un retrait : une LECTURE simple, rien n'est écrit. Ce qu'elle
/// rend est ce que la confirmation devra retrouver.
pub fn preparer_le_retrait(
    db: &dyn DbBackend,
    udn: &str,
    nous: &NotreAdresse,
) -> Result<Value, String> {
    let l = ParLaBase(db);
    let apercu = apercu_par(&l, udn)?;
    Ok(plan_public(&apercu, &plan_par(&l, udn, nous)?))
}

/// Préfixe des refus de [`retirer_selon_le_plan`] qui sont des CONFLITS.
pub const PLAN_CHANGE: &str = "Le plan a changé";

/// Fil du forum 3028 — le retrait confirmé, en UNE transaction : relire le
/// plan, le comparer à l'aperçu annoncé (`annonce`) et au compte confirmé,
/// rattacher, puis retirer. Un écart (favori ajouté, piste apparue, compte
/// différent) annule tout : aucun lien n'est déplacé ni effacé sans avoir été
/// annoncé.
pub fn retirer_selon_le_plan(
    db: &dyn DbBackend,
    udn: &str,
    nous: &NotreAdresse,
    annonce: &Value,
    compte_confirme: i64,
) -> Result<Value, String> {
    let mut resultat = Value::Null;
    db.write_tx(&mut |tx| {
        let l = DansLaTx(tx);
        let apercu = apercu_par(&l, udn)?;
        let plan = plan_par(&l, udn, nous)?;
        let relu = plan_public(&apercu, &plan);
        if relu != *annonce || apercu.pistes != compte_confirme {
            return Err(format!(
                "{PLAN_CHANGE} : {} pistes à retirer, {compte_confirme} confirmées. Relisez l'aperçu avant de confirmer.",
                apercu.pistes
            ));
        }
        for &(nature, distant, local) in &plan.rattachements {
            reporter_le_favori(tx, nature, distant, local)?;
            if nature == "track" {
                let p: [&dyn ToSqlValue; 2] = [&local, &distant];
                tx.execute("UPDATE playlist_tracks SET track_id = ? WHERE track_id = ?", &p)?;
            }
        }
        let retirees = retirer_dans(tx, udn)?;
        resultat = plan_public(
            &Apercu {
                pistes: retirees,
                ..apercu
            },
            &plan,
        );
        Ok(())
    })?;
    Ok(resultat)
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
        (
            StatusCode::CONFLICT,
            Json(crate::message_api::corps_refus(
                "upnp_indexing_in_progress",
                "api.upnp.indexingInProgress",
                &[],
            )),
        )
    })?;
    // Fil du forum 3028 — sur CE Tune, chaque piste distante est le reflet
    // d'une piste locale : ce retrait-ci effacerait leurs favoris et leurs
    // entrées de playlist sans les rattacher. Il est refusé, et renvoie vers
    // l'action `remove` des sources de bibliothèque, qui rattache d'abord.
    if super::synchronisation_upnp::source_vise_ce_serveur(&state, &udn).await {
        return Err((
            StatusCode::CONFLICT,
            Json(crate::message_api::corps_refus(
                "this_server_use_remove",
                "api.upnp.thisServerUseRemove",
                &[],
            )),
        ));
    }
    let vu = apercu(&state, &udn).map_err(interne)?;
    if vu.pistes == 0 && vu.albums == 0 && vu.sources == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(crate::message_api::corps_refus(
                "upnp_no_tracks_from_server",
                "api.upnp.noTracksFromServer",
                &[],
            )),
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
