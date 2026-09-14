//! Phase 2 du chantier `unifier-serveurs-upnp-et-bibliotheque` — indexer **une**
//! source UPnP, choisie à la main, dans `tracks` / `albums` / `artists`.
//!
//! # La décision technique de ce lot : la clé d'identité
//!
//! Le document de chantier proposait `source_id = '<udn>|<objectid>'`. **C'est
//! faux, et c'est mesuré.** Relevé le 14/09/2026 contre Asset UPnP
//! (`192.168.1.41:26125`), la même piste « Wonderwall » vue sous deux axes de
//! navigation :
//!
//! ```text
//! sous Album              id = d6120941636376083059-co4E8D6A18CD1AC698
//! sous Genre > Pop-Rock   id = d6120941636376083059-co679729C874689A62
//! ```
//!
//! L'`ObjectID` d'une piste porte l'identifiant de **son conteneur parent** :
//! il change d'un axe à l'autre. Indexer la racine d'un serveur produirait donc
//! autant de lignes que d'axes de navigation pour un seul fichier.
//!
//! **Et l'URL de `res` ne sauve pas la mise** — c'est la mesure que la phase 0
//! n'avait pas faite. Elle porte le MÊME identifiant contextuel :
//!
//! ```text
//! .../content/c2/b16/f44100/d6120941636376083059-co4E8D6A18CD1AC698.flac
//! .../content/c2/b16/f44100/d6120941636376083059-co679729C874689A62.flac
//! ```
//!
//! S'ajoute, côté Tune, l'instabilité déjà documentée : l'`ObjectID` y vaut
//! `track/<rowid>` (`upnp_server.rs`) et un rescan complet réattribue les
//! `tracks.id` « in walk order » (`db/sqlite.rs`). L'URL de `res` porte le même
//! `id`, donc le même défaut. Ni l'un ni l'autre n'est une identité.
//!
//! ## Ce qui est retenu : un CONDENSAT DU CONTENU ANNONCÉ
//!
//! `source_id = '<udn>|<condensat>'`, le condensat étant calculé sur le
//! quintuplet que le DIDL publie déjà, **sans une seconde requête** :
//!
//! | composante | pourquoi elle est là |
//! |---|---|
//! | `dc:title` | ce que l'auditeur reconnaît |
//! | `upnp:artist` / `dc:creator` | départage deux homonymes |
//! | `upnp:album` | départage deux éditions |
//! | `res@duration`, à la seconde | départage un extrait d'une piste entière |
//! | `res@size`, en octets | le discriminant le plus fin dont on dispose |
//!
//! Les cinq valeurs sont **identiques d'un axe à l'autre** (vérifié : la taille
//! `31911291` de Wonderwall ne bouge pas), donc le condensat dédoublonne. Elles
//! ne dépendent d'aucun `rowid`, donc il survit à une réindexation du serveur
//! distant — le scénario que la phase 4 devra affronter.
//!
//! ## Ce que cette clé ne fait PAS, et qu'il faut dire
//!
//! - **Deux encodages du même enregistrement font deux lignes.** Asset publie
//!   « 1- The edge » en `mp3` (4 788 505 o) et en `flac` (17 140 575 o) : deux
//!   tailles, deux condensats, deux pistes indexées. C'est le comportement
//!   voulu ici — ce sont bien deux fichiers distincts chez le voisin — mais
//!   l'écran en montrera deux.
//! - **Deux fichiers réellement distincts aux mêmes étiquettes, même durée et
//!   même taille se replient en une seule ligne.** Mesuré : « 2- Metti una sera
//!   a cena » existe deux fois sur Asset, à 8 105 765 octets chacune. La
//!   seconde n'entre pas. Un serveur qui ne publie pas `res@size` (la taille
//!   vaut alors 0) élargit ce repli.
//! - Elle ne rapproche **rien** d'une piste locale : D1 tranche que le
//!   rapprochement local ↔ distant est un MARQUAGE, en phase 5.
//!
//! Le condensat est un **FNV-1a 64 bits écrit ici en toutes lettres**, pas le
//! `DefaultHasher` de la bibliothèque standard : celui-ci ne promet aucune
//! stabilité d'une version de Rust à l'autre, et une clé qui change au
//! prochain compilateur n'est pas une clé.
//!
//! # Ce que cette passe écrit, et ce qu'elle n'écrit jamais
//!
//! Purement **additive** : `INSERT` ou `UPDATE` de l'instantané, **aucun
//! `DELETE`**, jamais — la réconciliation et ses quatre gardes de purge sont la
//! phase 4. Relancer l'indexation deux fois de suite ne crée pas de doublon et
//! ne supprime rien.
//!
//! Les lignes posées portent `source = 'upnp'` et `file_path = NULL`. Elles
//! sont donc hors de portée du scan local, qui ne connaît que les chemins :
//! `adopter_en_local` (`db/track_repo.rs`) ne s'applique qu'aux lignes que la
//! carte des chemins a retrouvées (`a_adopter`, `routes/system/scan.rs`), et une
//! ligne sans chemin n'y figure pas. Le témoin
//! `une_source_upnp_s_indexe_sans_doublon_et_sans_rien_supprimer` exige
//! explicitement que zéro ligne `upnp` porte un `file_path`.
//!
//! # L'instantané d'affichage
//!
//! Doctrine de `streaming_item_tags` (`db/migrations.rs`) : l'écran se rend
//! depuis ce qui est en base, et n'interroge jamais le serveur distant. Le
//! titre, l'artiste, l'album, la durée, le format et la résolution vivent donc
//! dans `tracks` ; l'URL de pochette et l'**URL de lecture** dans
//! `track_metadata`, sous des clés préfixées `upnp_`.
//!
//! L'URL de lecture n'est pas dans `source_id` **parce que `source_id` porte
//! l'identité**. C'est un écart assumé avec le chemin `radio`/`podcast`, où
//! `source_id` EST l'URL. Depuis la **phase 3**, l'orchestrateur le sait :
//! `resolve_direct_url_de_source` (`orchestrator/resolve_direct.rs`) lit
//! [`CLE_URL_DE_LECTURE`] dans l'instantané quand la demande ne nomme aucune
//! URL. Les deux littéraux — celui écrit ici, celui relu là-bas — sont chacun
//! gardés par un témoin : s'ils divergeaient, plus aucune piste indexée ne
//! jouerait, et rien d'autre ne rougirait.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::{Json, extract::Query};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::orchestrator::verdict_upnp::SortieD4;

use crate::state::AppState;

/// La source d'une piste indexée. Même vocabulaire que `qobuz` / `tidal` /
/// `radio` — et c'est celui que l'orchestrateur aiguille déjà vers
/// `resolve_direct_url` (`orchestrator/commun.rs`).
pub const SOURCE_UPNP: &str = "upnp";

/// L'URL de lecture annoncée par `res`, rangée dans `track_metadata`.
pub const CLE_URL_DE_LECTURE: &str = "upnp_res_url";
/// L'`ObjectID` sous lequel la piste a été VUE. Contextuel — conservé pour le
/// diagnostic, jamais pour l'identité.
pub const CLE_OBJECT_ID: &str = "upnp_object_id";
/// L'UDN du serveur d'origine — ce que D1bis veut afficher en infobulle.
pub const CLE_SERVEUR: &str = "upnp_serveur";
/// Le nom convivial du serveur, au moment de l'indexation.
pub const CLE_SERVEUR_NOM: &str = "upnp_serveur_nom";
/// L'URL de pochette annoncée par `upnp:albumArtURI`.
pub const CLE_POCHETTE: &str = "upnp_cover_url";

/// Profondeur de descente par défaut. Asset range ses pistes sous
/// `Album > [All Albums] > <album>` : trois niveaux depuis la racine. Six
/// laisse de la marge sans autoriser une descente sans fin.
const PROFONDEUR_DEFAUT: u32 = 6;
/// Plafond de conteneurs visités. Un serveur qui publie douze axes de
/// navigation en démultiplie le nombre ; sans borne, indexer la racine d'un
/// gros catalogue ne se termine pas dans un temps utile.
const CONTENEURS_DEFAUT: usize = 1_000;
/// Plafond de pistes retenues. 22 331 pistes se parcourent en 11,6 s (mesure du
/// 13/09) ; 50 000 est large et reste un refus CHIFFRÉ plutôt qu'un blocage.
const PISTES_DEFAUT: usize = 50_000;

#[derive(Debug, Deserialize, Default)]
pub struct DemandeIndexation {
    /// Le conteneur par lequel commencer. `0` = la racine du serveur.
    pub conteneur: Option<String>,
    pub profondeur_max: Option<u32>,
    pub max_conteneurs: Option<usize>,
    pub max_pistes: Option<usize>,
}

/// Condensat FNV-1a 64 bits, écrit en toutes lettres.
///
/// `std::collections::hash_map::DefaultHasher` ne promet PAS la même valeur
/// d'une version de Rust à l'autre (sa documentation le dit). Une clé
/// d'identité persistée en base doit être stable pour toujours : elle est donc
/// calculée ici, par un algorithme figé et vérifié par un vecteur de test.
fn fnv1a64(octets: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for o in octets {
        h ^= *o as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Normalise un morceau d'étiquette : bords rognés, casse repliée, suites
/// d'espaces réduites à une seule.
///
/// Le repli de casse s'arrête là : pas de pliage d'accents, pas de retrait de
/// ponctuation. C'est exactement la normalisation que le dépôt applique déjà au
/// rapprochement de doublons (`LOWER()` seul, `routes/library/duplicates.rs`) —
/// en faire plus ici replierait deux pistes voisines sans que rien d'autre dans
/// le produit ne le fasse.
fn normaliser(texte: &str) -> String {
    let mut sortie = String::with_capacity(texte.len());
    let mut espace_en_attente = false;
    for c in texte.trim().chars() {
        if c.is_whitespace() {
            espace_en_attente = !sortie.is_empty();
            continue;
        }
        if espace_en_attente {
            sortie.push(' ');
            espace_en_attente = false;
        }
        for minuscule in c.to_lowercase() {
            sortie.push(minuscule);
        }
    }
    sortie
}

/// **La clé d'identité d'une piste distante.**
///
/// `'<udn>|<16 hexadécimaux>'`. Voir l'en-tête du module pour la mesure qui a
/// écarté l'`ObjectID` et l'URL de `res`.
///
/// La durée est arrondie à la **seconde** : Asset annonce `0:04:18.000`, un
/// Tune `0:04:42.773`, et deux serveurs qui décrivent le même fichier ne
/// tombent pas d'accord à la milliseconde. Une durée ou une taille absente vaut
/// `0` — ce qui élargit le repli, et c'est dit dans la réponse de la route.
pub fn cle_d_identite(
    udn: &str,
    titre: &str,
    artiste: Option<&str>,
    album: Option<&str>,
    duree_ms: Option<u64>,
    taille_octets: Option<u64>,
) -> String {
    // Le séparateur `\u{1f}` (UNIT SEPARATOR) ne peut pas apparaître dans une
    // étiquette lue d'un XML : sans lui, un titre finissant par le nom de
    // l'artiste suivant produirait le même condensat qu'un autre découpage.
    let matiere = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        normaliser(titre),
        normaliser(artiste.unwrap_or_default()),
        normaliser(album.unwrap_or_default()),
        duree_ms.unwrap_or(0) / 1_000,
        taille_octets.unwrap_or(0),
    );
    format!("{udn}|{:016x}", fnv1a64(matiere.as_bytes()))
}

/// La clé d'identité d'un ALBUM distant : même condensat, sur (titre d'album,
/// artiste). Un album n'a ni durée ni taille.
pub fn cle_d_identite_album(udn: &str, titre: &str, artiste: Option<&str>) -> String {
    let matiere = format!(
        "album\u{1f}{}\u{1f}{}",
        normaliser(titre),
        normaliser(artiste.unwrap_or_default()),
    );
    format!("{udn}|{:016x}", fnv1a64(matiere.as_bytes()))
}

/// Ce que l'indexation retient d'un item DIDL.
#[derive(Debug, Clone)]
struct PisteDistante {
    object_id: String,
    titre: String,
    artiste: Option<String>,
    album: Option<String>,
    url_de_lecture: Option<String>,
    pochette: Option<String>,
    duree_ms: Option<u64>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    channels: Option<i32>,
    taille: Option<u64>,
    protocol_info: Option<String>,
}

fn texte(v: &Value, cle: &str) -> Option<String> {
    v.get(cle)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn entier(v: &Value, cle: &str) -> Option<u64> {
    v.get(cle).and_then(serde_json::Value::as_u64)
}

impl PisteDistante {
    fn depuis_item(item: &Value) -> Option<Self> {
        let titre = texte(item, "title")?;
        Some(Self {
            object_id: texte(item, "id").unwrap_or_default(),
            titre,
            artiste: texte(item, "artist"),
            album: texte(item, "album"),
            url_de_lecture: texte(item, "res_url"),
            pochette: texte(item, "album_art_uri"),
            duree_ms: entier(item, "duration_ms"),
            sample_rate: entier(item, "sample_rate").map(|v| v as i32),
            bit_depth: entier(item, "bit_depth").map(|v| v as i32),
            channels: entier(item, "channels").map(|v| v as i32),
            taille: entier(item, "size"),
            protocol_info: texte(item, "protocol_info"),
        })
    }

    /// Le format lisible, déduit du `protocolInfo` — `audio/x-flac` → `flac`.
    /// Rien d'inventé : `None` quand le serveur ne dit rien.
    fn format(&self) -> Option<String> {
        let mime = self.protocol_info.as_deref()?.split(':').nth(2)?.trim();
        let sous_type = mime.rsplit('/').next()?.trim();
        if sous_type.is_empty() {
            return None;
        }
        Some(sous_type.trim_start_matches("x-").to_lowercase())
    }
}

/// Le bilan d'une passe d'indexation.
#[derive(Debug, Default)]
struct Bilan {
    conteneurs_visites: usize,
    items_vus: usize,
    pistes_distinctes: usize,
    ajoutees: usize,
    mises_a_jour: usize,
    sans_url: usize,
    sans_taille: usize,
    albums_ajoutes: usize,
    plafond_atteint: Option<&'static str>,
    erreurs: Vec<String>,
}

/// `POST /api/v1/network/media-servers/{id}/indexer`
///
/// Indexe UNE source, celle dont l'identifiant est passé dans le chemin. Rien
/// n'est périodique, rien n'est automatique : la phase 2 est un geste explicite.
pub async fn indexer_une_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(demande): Query<DemandeIndexation>,
) -> Json<Value> {
    let serveurs = state.media_servers.lock().await;
    let Some(ms) = serveurs.get(&id).cloned() else {
        return Json(json!({
            "indexe": false,
            "raison": "serveur inconnu",
            "detail": format!(
                "aucun serveur multimédia enregistré sous « {id} » — \
                 la liste vit dans GET /api/v1/network/media-servers"
            ),
        }));
    };
    drop(serveurs);

    let conteneur = demande.conteneur.clone().unwrap_or_else(|| "0".into());
    let profondeur_max = demande.profondeur_max.unwrap_or(PROFONDEUR_DEFAUT);
    let max_conteneurs = demande.max_conteneurs.unwrap_or(CONTENEURS_DEFAUT);
    let max_pistes = demande.max_pistes.unwrap_or(PISTES_DEFAUT);

    let debut = std::time::Instant::now();
    let (pistes, mut bilan) = recolter(
        &ms.content_directory_url,
        &ms.name,
        &ms.id,
        &conteneur,
        profondeur_max,
        max_conteneurs,
        max_pistes,
    )
    .await;

    let parcours_ms = debut.elapsed().as_millis() as u64;
    ecrire(&state, &ms.id, &ms.name, &pistes, &mut bilan);

    Json(json!({
        "indexe": true,
        "serveur": { "id": ms.id, "nom": ms.name, "adresse": format!("{}:{}", ms.host, ms.port) },
        "conteneur": conteneur,
        "cle_d_identite": "condensat de (titre, artiste, album, durée à la seconde, res@size)",
        "parcours": {
            "conteneurs_visites": bilan.conteneurs_visites,
            "items_vus": bilan.items_vus,
            "duree_ms": parcours_ms,
            "plafond_atteint": bilan.plafond_atteint,
        },
        "pistes": {
            "distinctes": bilan.pistes_distinctes,
            "ajoutees": bilan.ajoutees,
            "mises_a_jour": bilan.mises_a_jour,
            "ecartees_sans_url_de_lecture": bilan.sans_url,
            "sans_res_size": bilan.sans_taille,
        },
        "albums_ajoutes": bilan.albums_ajoutes,
        "supprimees": 0,
        "erreurs": bilan.erreurs,
        // Ne jamais faire semblant : ce que cette passe NE fait pas.
        "reserves": reserves(&bilan),
    }))
}

/// Ce que la passe ne fait pas, dit dans sa propre réponse.
fn reserves(bilan: &Bilan) -> Vec<String> {
    let mut dites = vec![
        "passe purement additive : aucune ligne n'est supprimée, \
         la réconciliation est la phase 4"
            .to_string(),
        "aucun rapprochement avec la bibliothèque locale : un album présent \
         des deux côtés apparaît deux fois (D1, marquage en phase 5)"
            .to_string(),
    ];
    // D4, tranchée par Bertrand le 14/09 : jouable partout, défauts assumés et
    // DITS. Depuis la phase 3, une ligne indexée SE JOUE — la lecture retrouve
    // son URL dans l'instantané, pas dans `source_id`. Ce qui reste à dire,
    // ce sont les dégradations, sortie par sortie.
    //
    // Elles ne sont pas recopiées ici : c'est la MÊME table que celle où
    // l'orchestrateur puise son refus OAAT, et que les routes de lecture
    // rendent dans leur champ `avertissements`. Trois listes écrites à la main
    // auraient divergé — ici, corriger une dégradation la fait disparaître des
    // trois endroits d'un coup.
    for sortie in [
        SortieD4::Reseau,
        SortieD4::Navigateur,
        SortieD4::Locale,
        SortieD4::Oaat,
    ] {
        for degradation in sortie.degradations() {
            dites.push(format!("sortie {} — {degradation}", sortie.nom()));
        }
    }
    if bilan.sans_taille > 0 {
        dites.push(format!(
            "{} piste(s) sans `res@size` : leur clé d'identité repose sur les \
             seules étiquettes et la durée, elle replie davantage",
            bilan.sans_taille
        ));
    }
    if let Some(plafond) = bilan.plafond_atteint {
        dites.push(format!(
            "plafond « {plafond} » atteint : le parcours s'est arrêté avant la \
             fin du catalogue — relancer sur un conteneur plus précis"
        ));
    }
    dites
}

/// Descente en largeur depuis le conteneur choisi, bornée de trois façons.
#[allow(clippy::too_many_arguments)]
async fn recolter(
    cd_url: &str,
    nom: &str,
    udn: &str,
    racine: &str,
    profondeur_max: u32,
    max_conteneurs: usize,
    max_pistes: usize,
) -> (Vec<PisteDistante>, Bilan) {
    let mut bilan = Bilan::default();
    let mut retenues: HashMap<String, PisteDistante> = HashMap::new();
    let mut vus: HashSet<String> = HashSet::new();
    let mut file: Vec<(String, u32)> = vec![(racine.to_string(), 0)];
    vus.insert(racine.to_string());

    while let Some((conteneur, profondeur)) = file.pop() {
        if bilan.conteneurs_visites >= max_conteneurs {
            bilan.plafond_atteint = Some("conteneurs");
            break;
        }
        bilan.conteneurs_visites += 1;

        let (sous_conteneurs, items, _total) =
            super::network::parcourir_les_enfants(cd_url, nom, &conteneur).await;
        bilan.items_vus += items.len();

        for item in &items {
            let Some(piste) = PisteDistante::depuis_item(item) else {
                continue;
            };
            if piste.url_de_lecture.is_none() {
                // Une piste sans URL de lecture n'est pas jouable : l'indexer
                // serait promettre un écran qui ne joue pas.
                bilan.sans_url += 1;
                continue;
            }
            if piste.taille.is_none() {
                bilan.sans_taille += 1;
            }
            let cle = cle_d_identite(
                udn,
                &piste.titre,
                piste.artiste.as_deref(),
                piste.album.as_deref(),
                piste.duree_ms,
                piste.taille,
            );
            // `entry` : la PREMIÈRE vue gagne. Les suivantes sont le même
            // fichier sous un autre axe de navigation — c'est précisément ce
            // que la clé d'identité est là pour replier.
            retenues.entry(cle).or_insert(piste);
            if retenues.len() >= max_pistes {
                bilan.plafond_atteint = Some("pistes");
                break;
            }
        }
        if bilan.plafond_atteint.is_some() {
            break;
        }

        if profondeur >= profondeur_max {
            if !sous_conteneurs.is_empty() {
                bilan.plafond_atteint.get_or_insert("profondeur");
            }
            continue;
        }
        for sous in &sous_conteneurs {
            let Some(sous_id) = texte(sous, "id") else {
                continue;
            };
            if vus.insert(sous_id.clone()) {
                file.push((sous_id, profondeur + 1));
            }
        }
    }

    bilan.pistes_distinctes = retenues.len();
    (retenues.into_values().collect(), bilan)
}

/// Écrit l'instantané. **Aucun `DELETE`.**
fn ecrire(
    state: &AppState,
    udn: &str,
    nom_du_serveur: &str,
    pistes: &[PisteDistante],
    bilan: &mut Bilan,
) {
    let pistes_repo = TrackRepo::with_backend(state.backend.clone());
    let albums_repo = AlbumRepo::with_backend(state.backend.clone());
    let artistes_repo = ArtistRepo::with_backend(state.backend.clone());
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());

    // Mémoire d'une seule passe : un album distant n'est résolu qu'une fois,
    // quel que soit le nombre de ses pistes.
    let mut albums_vus: HashMap<String, i64> = HashMap::new();

    for piste in pistes {
        let artiste_id = piste.artiste.as_deref().and_then(|nom| {
            artistes_repo
                .get_or_create(nom, None, None)
                .ok()
                .and_then(|a| a.id)
        });

        let album_id = match piste.album.as_deref() {
            None => None,
            Some(titre_album) => {
                let cle_album = cle_d_identite_album(udn, titre_album, piste.artiste.as_deref());
                match albums_vus.get(&cle_album) {
                    Some(id) => Some(*id),
                    None => {
                        let trouve = album_existant(state, &cle_album);
                        let id = match trouve {
                            Some(id) => Some(id),
                            None => {
                                let mut album = Album::new(titre_album.to_string());
                                album.artist_id = artiste_id;
                                album.source = SOURCE_UPNP.to_string();
                                album.source_id = Some(cle_album.clone());
                                // `cover_path` porte ici une URL, comme pour
                                // toute source distante : l'instantané doit
                                // pouvoir s'afficher serveur éteint.
                                album.cover_path = piste.pochette.clone();
                                match albums_repo.create(&album) {
                                    Ok(id) => {
                                        bilan.albums_ajoutes += 1;
                                        Some(id)
                                    }
                                    Err(e) => {
                                        bilan
                                            .erreurs
                                            .push(format!("album « {titre_album} » : {e}"));
                                        None
                                    }
                                }
                            }
                        };
                        if let Some(id) = id {
                            albums_vus.insert(cle_album, id);
                        }
                        id
                    }
                }
            }
        };

        let cle = cle_d_identite(
            udn,
            &piste.titre,
            piste.artiste.as_deref(),
            piste.album.as_deref(),
            piste.duree_ms,
            piste.taille,
        );

        let mut ligne = Track::new(piste.titre.clone());
        ligne.album_id = album_id;
        ligne.artist_id = artiste_id;
        ligne.artist_name = piste.artiste.clone();
        ligne.album_title = piste.album.clone();
        ligne.duration_ms = piste.duree_ms.unwrap_or(0) as i64;
        ligne.file_path = None;
        ligne.format = piste.format();
        ligne.sample_rate = piste.sample_rate;
        ligne.bit_depth = piste.bit_depth;
        // `channels` est un `i32` NON optionnel dont `to_json` dérive une
        // pastille AFFICHÉE. Un serveur muet sur `nrAudioChannels` ne prouve
        // rien : on retombe sur le défaut du modèle (2) plutôt que d'affirmer
        // un canal qu'on n'a pas lu — et la pastille reste ce qu'elle est
        // aujourd'hui pour toute piste sans preuve. Le rendre optionnel est un
        // prérequis nommé par le chantier, il n'est PAS fait ici.
        if let Some(ch) = piste.channels {
            ligne.channels = ch;
        }
        ligne.file_size = piste.taille.map(|t| t as i64);
        ligne.source = SOURCE_UPNP.to_string();
        ligne.source_id = Some(cle.clone());

        let existante = piste_existante(state, &cle);
        let id = match existante {
            Some(id) => {
                ligne.id = Some(id);
                match pistes_repo.update(&ligne) {
                    Ok(()) => {
                        bilan.mises_a_jour += 1;
                        Some(id)
                    }
                    Err(e) => {
                        bilan
                            .erreurs
                            .push(format!("piste « {} » : {e}", piste.titre));
                        None
                    }
                }
            }
            None => match pistes_repo.create(&ligne) {
                Ok(id) => {
                    bilan.ajoutees += 1;
                    Some(id)
                }
                Err(e) => {
                    bilan
                        .erreurs
                        .push(format!("piste « {} » : {e}", piste.titre));
                    None
                }
            },
        };

        let Some(id) = id else { continue };
        let mut instantane: HashMap<String, String> = HashMap::new();
        if let Some(url) = &piste.url_de_lecture {
            instantane.insert(CLE_URL_DE_LECTURE.to_string(), url.clone());
        }
        if let Some(pochette) = &piste.pochette {
            instantane.insert(CLE_POCHETTE.to_string(), pochette.clone());
        }
        instantane.insert(CLE_OBJECT_ID.to_string(), piste.object_id.clone());
        instantane.insert(CLE_SERVEUR.to_string(), udn.to_string());
        instantane.insert(CLE_SERVEUR_NOM.to_string(), nom_du_serveur.to_string());
        if let Err(e) = meta_repo.set_batch(id, &instantane) {
            bilan
                .erreurs
                .push(format!("instantané de « {} » : {e}", piste.titre));
        }
    }
}

fn piste_existante(state: &AppState, cle: &str) -> Option<i64> {
    state
        .backend
        .query_one(
            "SELECT id FROM tracks WHERE source = ? AND source_id = ?",
            &[&SOURCE_UPNP as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
}

fn album_existant(state: &AppState, cle: &str) -> Option<i64> {
    state
        .backend
        .query_one(
            "SELECT id FROM albums WHERE source = ? AND source_id = ?",
            &[&SOURCE_UPNP as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le vecteur de référence du FNV-1a 64 bits. Si un jour quelqu'un
    /// « optimise » la fonction, toutes les clés déjà en base deviendraient
    /// fausses en silence. C'est ici que ça rougit.
    #[test]
    fn le_condensat_est_fige() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// **La mesure du 14/09, rejouée.** Deux `ObjectID` différents, la même
    /// piste : la clé doit être la même.
    #[test]
    fn deux_axes_de_navigation_donnent_la_meme_cle() {
        let udn = "uuid:258FC2D5-E2C3-B734-0-123456789abc";
        // Sous « Album » :  d6120941636376083059-co4E8D6A18CD1AC698
        // Sous « Genre »  : d6120941636376083059-co679729C874689A62
        // Les deux annoncent le même titre, artiste, album, durée et taille.
        let sous_album = cle_d_identite(
            udn,
            "Wonderwall",
            Some("Oasis"),
            Some("(What's the Story) Morning Glory?"),
            Some(258_000),
            Some(31_911_291),
        );
        let sous_genre = cle_d_identite(
            udn,
            "Wonderwall",
            Some("Oasis"),
            Some("(What's the Story) Morning Glory?"),
            Some(258_000),
            Some(31_911_291),
        );
        assert_eq!(
            sous_album, sous_genre,
            "la clé doit être indépendante de l'axe de navigation"
        );
    }

    /// Le témoin n'aurait aucune portée si la clé ne SÉPARAIT rien : deux
    /// encodages du même enregistrement, mesurés sur Asset (« 1- The edge »,
    /// 4 788 505 o en mp3 contre 17 140 575 o en flac), restent deux pistes.
    #[test]
    fn deux_encodages_restent_deux_pistes() {
        let udn = "uuid:x";
        let mp3 = cle_d_identite(
            udn,
            "1- The edge",
            None,
            None,
            Some(199_000),
            Some(4_788_505),
        );
        let flac = cle_d_identite(
            udn,
            "1- The edge",
            None,
            None,
            Some(199_000),
            Some(17_140_575),
        );
        assert_ne!(mp3, flac);
    }

    #[test]
    fn la_cle_porte_le_serveur() {
        let a = cle_d_identite("uuid:a", "T", None, None, Some(1_000), Some(10));
        let b = cle_d_identite("uuid:b", "T", None, None, Some(1_000), Some(10));
        assert_ne!(a, b, "deux serveurs ne partagent pas une identité de piste");
        assert!(a.starts_with("uuid:a|"));
    }

    /// La milliseconde ne doit pas départager : Asset annonce `0:04:18.000` là
    /// où un Tune annonce des millièmes.
    #[test]
    fn la_duree_est_arrondie_a_la_seconde() {
        let rond = cle_d_identite("u", "T", None, None, Some(258_000), Some(7));
        let presque = cle_d_identite("u", "T", None, None, Some(258_773), Some(7));
        assert_eq!(rond, presque);
    }

    #[test]
    fn la_normalisation_replie_la_casse_et_les_espaces() {
        assert_eq!(normaliser("  Le   Grand  Bleu "), "le grand bleu");
        assert_eq!(
            cle_d_identite("u", "Wonderwall", None, None, None, None),
            cle_d_identite("u", "  WONDERWALL  ", None, None, None, None)
        );
    }

    /// Le séparateur d'unité interdit le glissement d'un champ sur l'autre.
    #[test]
    fn les_champs_ne_glissent_pas_l_un_dans_l_autre() {
        let a = cle_d_identite("u", "AB", Some(""), None, None, None);
        let b = cle_d_identite("u", "A", Some("B"), None, None, None);
        assert_ne!(a, b);
    }

    #[test]
    fn le_format_vient_du_protocol_info_ou_de_rien() {
        let mut p = PisteDistante {
            object_id: "x".into(),
            titre: "T".into(),
            artiste: None,
            album: None,
            url_de_lecture: None,
            pochette: None,
            duree_ms: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            taille: None,
            protocol_info: Some("http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".into()),
        };
        assert_eq!(p.format().as_deref(), Some("flac"));
        p.protocol_info = None;
        assert_eq!(p.format(), None, "rien n'est inventé quand rien n'est dit");
    }
}
