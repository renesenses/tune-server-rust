//! Tune Circle, étape T3 (#5326) : les étiquettes et les collections
//! intelligentes partagées avec un cercle — les « rayons ».
//!
//! Le cloud ne sait pas évaluer une règle : elle porte sur des champs qu'il
//! n'a pas (notes, écoutes, date d'ajout, favoris de service). Le serveur du
//! propriétaire RÉSOUT donc chaque ensemble coché et pousse la liste de ses
//! membres :
//!
//! * les albums, pistes et artistes de la bibliothèque, par leur identifiant
//!   local — celui que `library_sync` pousse dans la copie en ligne
//!   (`remote_id` côté cloud) ;
//! * les objets de streaming comme **références** (décision de Bertrand du
//!   28/09/2026) : titre, artiste, album, identifiant de service — jamais une
//!   pochette, une adresse ou un chemin. Le contact les rejoue avec son propre
//!   service.
//!
//! Une étiquette est globale au serveur : ce module la résout lui-même, par la
//! base. Une collection intelligente se résout par profil, avec le moteur de
//! `tune-smart-http` : l'hôte l'injecte ([`Hote`]), sans copie de la logique.
//! Le profil qui la résout est celui qui l'a cochée (décision 2 du 28/09).

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::request::Parts;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tune_core::db::backend::{DbBackend, ToSqlValue};

/// `kind` d'une étiquette, dans le contrat du cloud.
pub const GENRE_ETIQUETTE: &str = "tag";
/// `kind` d'une collection intelligente, dans le contrat du cloud.
pub const GENRE_COLLECTION: &str = "smart_collection";

/// Les services dont le cloud connaît l'identifiant (`{service}_id`).
pub const SERVICES_A_IDENTIFIANT: [&str; 5] = ["qobuz", "tidal", "spotify", "deezer", "youtube"];

/// Longueur maximale d'un texte de référence (cloud : 300 caractères).
const TEXTE_MAX: usize = 300;
/// Longueur maximale du nom d'un ensemble (cloud : 200 caractères).
const NOM_MAX: usize = 200;

/// Ce que l'hôte fournit au greffon pour les rayons.
#[async_trait]
pub trait Hote: Send + Sync {
    /// Le profil actif de la requête, jugé comme toutes les routes du serveur
    /// (`X-Profile-Id` permis, appelant authentifié, profil global).
    async fn profil_actif(&self, parts: &mut Parts) -> i64;

    /// Les membres de la collection intelligente `id` pour `profile_id`, par
    /// le moteur de la vue. `Ok(None)` : elle n'existe pas. `Err` : la
    /// résolution a échoué, rien ne doit partir.
    async fn collection_intelligente(
        &self,
        id: i64,
        profile_id: i64,
    ) -> Result<Option<Membres>, String>;
}

/// Un objet de streaming partagé comme RÉFÉRENCE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// `track`, `album` ou `artist`.
    pub genre: String,
    pub titre: String,
    pub artiste: Option<String>,
    pub album: Option<String>,
    /// Le service tel que l'hôte le nomme (`qobuz`, `tidal`…).
    pub service: String,
    pub id_de_service: String,
}

/// Les membres résolus d'un ensemble.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Membres {
    pub nom: String,
    pub albums: Vec<i64>,
    pub pistes: Vec<i64>,
    pub artistes: Vec<i64>,
    pub references: Vec<Reference>,
}

impl Membres {
    /// Nombre d'éléments, tous types confondus.
    pub fn compte(&self) -> usize {
        self.albums.len() + self.pistes.len() + self.artistes.len() + self.references.len()
    }
}

/// Un `kind` que le contrat connaît.
pub fn genre_valide(kind: &str) -> bool {
    kind == GENRE_ETIQUETTE || kind == GENRE_COLLECTION
}

fn tronquer(texte: &str, max: usize) -> String {
    texte.trim().chars().take(max).collect()
}

fn texte_ou_rien(v: Option<&str>) -> Option<String> {
    v.map(|t| tronquer(t, TEXTE_MAX)).filter(|t| !t.is_empty())
}

/// La forme exacte d'un identifiant de service admis par le cloud :
/// `^[A-Za-z0-9._:-]{1,64}$`. Aucune barre, aucun espace : ni chemin ni adresse.
pub fn identifiant_de_service_valide(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Une référence, champ par champ, dans la liste blanche du contrat :
/// `type`, `title`, `artist_name`, `album_title`, `{service}_id`. `None` si
/// elle n'a pas de titre ou pas de type connu.
pub fn reference_en_json(r: &Reference) -> Option<Value> {
    if !matches!(r.genre.as_str(), "track" | "album" | "artist") {
        return None;
    }
    let titre = texte_ou_rien(Some(&r.titre))?;
    let mut objet = json!({ "type": r.genre, "title": titre });
    if let Some(a) = texte_ou_rien(r.artiste.as_deref()) {
        objet["artist_name"] = json!(a);
    }
    if let Some(a) = texte_ou_rien(r.album.as_deref()) {
        objet["album_title"] = json!(a);
    }
    let service = r.service.trim().to_ascii_lowercase();
    let id = r.id_de_service.trim();
    if SERVICES_A_IDENTIFIANT.contains(&service.as_str()) && identifiant_de_service_valide(id) {
        objet[format!("{service}_id")] = json!(id);
    }
    Some(objet)
}

fn trie_sans_doublon(mut ids: Vec<i64>) -> Vec<i64> {
    ids.retain(|i| *i > 0);
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Le contenu poussé, sans ce qui ne dépend pas de l'ensemble (`server_id`,
/// `profile_id`, `digest`) : c'est lui que l'empreinte résume.
pub fn contenu(m: &Membres) -> Value {
    let streaming: Vec<Value> = m.references.iter().filter_map(reference_en_json).collect();
    json!({
        "name": tronquer(&m.nom, NOM_MAX),
        "albums": trie_sans_doublon(m.albums.clone()),
        "tracks": trie_sans_doublon(m.pistes.clone()),
        "artists": trie_sans_doublon(m.artistes.clone()),
        "streaming": streaming,
    })
}

/// Empreinte opaque du contenu poussé : le battement ne repousse que si elle
/// change. `v1:` + SHA-256 en hexadécimal.
pub fn empreinte(contenu: &Value) -> String {
    let octets = Sha256::digest(contenu.to_string().as_bytes());
    let mut hex = String::with_capacity(3 + 64);
    hex.push_str("v1:");
    for o in octets {
        hex.push_str(&format!("{o:02x}"));
    }
    hex
}

/// Le corps du `PUT /circles/{id}/sets/{kind}/{source_id}` : le contenu, le
/// `server_id` de CE serveur, le profil qui résout et l'empreinte.
pub fn corps_du_partage(m: &Membres, server_id: Option<&str>, profile_id: Option<i64>) -> Value {
    let mut corps = contenu(m);
    let digest = empreinte(&corps);
    corps["server_id"] = json!(server_id);
    corps["profile_id"] = json!(profile_id);
    corps["digest"] = json!(digest);
    corps
}

// Étiquettes -----------------------------------------------------------------

/// Résout l'étiquette `id` par la base : ses albums, pistes et artistes de la
/// bibliothèque, et ses objets de streaming en références. Les autres types
/// étiquetables (collections, playlists, `smart_*`) ne sortent pas.
/// `Ok(None)` : l'étiquette n'existe pas.
pub fn resoudre_etiquette(
    backend: &Arc<dyn DbBackend>,
    id: i64,
) -> Result<Option<Membres>, String> {
    let Some(ligne) = backend.query_one(
        "SELECT name FROM tags WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    )?
    else {
        return Ok(None);
    };
    let mut m = Membres {
        nom: ligne
            .first()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        ..Membres::default()
    };
    for r in backend.query_many(
        "SELECT item_type, item_id FROM item_tags WHERE tag_id = ?",
        &[&id as &dyn ToSqlValue],
    )? {
        let genre = r.first().and_then(|v| v.as_string()).unwrap_or_default();
        let Some(item) = r.get(1).and_then(|v| v.as_i64()) else {
            continue;
        };
        match genre.as_str() {
            "album" => m.albums.push(item),
            "track" => m.pistes.push(item),
            "artist" => m.artistes.push(item),
            _ => {}
        }
    }
    for r in backend.query_many(
        "SELECT item_type, source, source_id, title, artist, album FROM streaming_item_tags \
         WHERE tag_id = ? ORDER BY created_at, source, source_id",
        &[&id as &dyn ToSqlValue],
    )? {
        let col = |i: usize| r.get(i).and_then(|v| v.as_string());
        let genre = col(0).unwrap_or_default();
        // Un artiste de service n'a pas de « titre » : son nom en tient lieu.
        let titre = col(3)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| (genre == "artist").then(|| col(4)).flatten())
            .unwrap_or_default();
        m.references.push(Reference {
            genre,
            titre,
            artiste: col(4),
            album: col(5),
            service: col(1).unwrap_or_default(),
            id_de_service: col(2).unwrap_or_default(),
        });
    }
    Ok(Some(m))
}

/// Les étiquettes du serveur : `(id, nom, nombre d'éléments partageables)`.
pub fn etiquettes_locales(backend: &Arc<dyn DbBackend>) -> Vec<(i64, String, i64)> {
    backend
        .query_many(
            "SELECT t.id, t.name, \
             (SELECT COUNT(*) FROM item_tags i WHERE i.tag_id = t.id \
                AND i.item_type IN ('album', 'track', 'artist')) \
             + (SELECT COUNT(*) FROM streaming_item_tags s WHERE s.tag_id = t.id \
                AND s.item_type IN ('album', 'track', 'artist')) \
             FROM tags t ORDER BY t.name",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            Some((
                r.first()?.as_i64()?,
                r.get(1)?.as_string()?,
                r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            ))
        })
        .collect()
}

/// Les collections intelligentes du serveur : `(id, nom)`.
pub fn collections_locales(backend: &Arc<dyn DbBackend>) -> Vec<(i64, String)> {
    backend
        .query_many("SELECT id, name FROM smart_collections ORDER BY name", &[])
        .unwrap_or_default()
        .iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
        .collect()
}

/// Ce qui DÉFINIT une collection intelligente : son nom et ses règles. Une
/// empreinte qui change dit « à résoudre de nouveau » sans résoudre.
pub fn definition_de_collection(backend: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
    let r = backend
        .query_one(
            "SELECT name, rules, match_mode, sort_by, sort_order, max_limit \
             FROM smart_collections WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok()??;
    let texte = |i: usize| r.get(i).and_then(|v| v.as_string()).unwrap_or_default();
    Some(empreinte(&json!([
        texte(0),
        texte(1),
        texte(2),
        texte(3),
        texte(4),
        r.get(5).and_then(|v| v.as_i64()),
    ])))
}

/// Résout un ensemble, quel que soit son genre.
pub async fn resoudre(
    backend: &Arc<dyn DbBackend>,
    hote: &dyn Hote,
    kind: &str,
    source_id: i64,
    profile_id: i64,
) -> Result<Option<Membres>, String> {
    match kind {
        GENRE_ETIQUETTE => resoudre_etiquette(backend, source_id),
        GENRE_COLLECTION => hote.collection_intelligente(source_id, profile_id).await,
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(service: &str, id: &str) -> Reference {
        Reference {
            genre: "track".into(),
            titre: "So What".into(),
            artiste: Some("Miles Davis".into()),
            album: None,
            service: service.into(),
            id_de_service: id.into(),
        }
    }

    #[test]
    fn une_reference_ne_porte_que_la_liste_blanche() {
        let v = reference_en_json(&reference("Qobuz", "123456")).unwrap();
        assert_eq!(
            v,
            json!({ "type": "track", "title": "So What", "artist_name": "Miles Davis",
                    "qobuz_id": "123456" })
        );
    }

    #[test]
    fn un_identifiant_qui_ressemble_a_un_chemin_ne_sort_pas() {
        for id in [
            "/home/moi/a.flac",
            "C:\\Musique\\a.flac",
            "https://x/y",
            "a b",
            "",
        ] {
            let v = reference_en_json(&reference("tidal", id)).unwrap();
            assert!(v.get("tidal_id").is_none(), "{id} : {v}");
        }
    }

    #[test]
    fn un_service_inconnu_garde_la_reference_sans_identifiant() {
        let v = reference_en_json(&reference("bandcamp", "abc")).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 3, "{v}");
    }

    #[test]
    fn sans_titre_ou_de_type_inconnu_rien_ne_part() {
        let mut r = reference("qobuz", "1");
        r.titre = "  ".into();
        assert!(reference_en_json(&r).is_none());
        let mut r = reference("qobuz", "1");
        r.genre = "playlist".into();
        assert!(reference_en_json(&r).is_none());
    }

    #[test]
    fn l_empreinte_ne_depend_pas_de_l_ordre_des_identifiants() {
        let a = Membres {
            nom: "Jazz".into(),
            albums: vec![3, 1, 2, 2],
            ..Membres::default()
        };
        let b = Membres {
            nom: "Jazz".into(),
            albums: vec![1, 2, 3],
            ..Membres::default()
        };
        assert_eq!(empreinte(&contenu(&a)), empreinte(&contenu(&b)));
        let c = Membres {
            nom: "Jazz".into(),
            albums: vec![1, 2],
            ..Membres::default()
        };
        assert_ne!(empreinte(&contenu(&a)), empreinte(&contenu(&c)));
    }
}
