//! Le TYPE DE SORTIE d'un disque : album, EP ou single (#4767).
//!
//! FabienM demande que la page d'un artiste sépare « Albums principaux » et
//! « EP & singles ». Aucune table, aucun scan, aucun enrichissement de Tune ne
//! portait cette information : la page de Neil Young restait une liste unique
//! de 192 lignes.
//!
//! # Deux sources, et rien d'autre
//!
//! * **MusicBrainz**, via le `primary-type` du GROUPE DE SORTIE dont
//!   `albums.musicbrainz_release_group_id` porte déjà l'identifiant. C'est la
//!   source juste, et la seule pour un disque de la bibliothèque locale.
//! * **Le service**, pour un album de streaming : Qobuz l'annonce sous
//!   `release_type`, Tidal sous `type`. Ils parlent de LEUR catalogue, donc ils
//!   en sont l'autorité.
//!
//! # 🔴 L'inconnu est l'état normal, et il est explicite
//!
//! La couverture MBID mesurée est de **0,9 % sur le .18** et **88,4 % sur le
//! .15** : sur la plupart des disques, MusicBrainz ne répondra pas. Le type
//! reste alors `None`, la colonne reste NULLE, et le client lit « inconnu ».
//!
//! # Une règle de repli, et elle ne remplace jamais une réponse (#5616)
//!
//! La 0.9.169 refusait toute heuristique (« un classement faux est pire
//! qu'une section absente »). Bertrand revient sur ce choix le 05/10/2026
//! (fil 2096, puis 2143 de FabienM) : sur la bibliothèque locale, le type est
//! presque toujours inconnu, et la page d'un artiste mêlait ses singles à ses
//! albums. La règle est donc :
//!
//! 1. **Le type explicite gagne toujours** : `albums.release_type`, écrit par
//!    la balise du fichier au scan ([`depuis_valeurs_de_tag`] : `RELEASETYPE`
//!    et ses variantes, quand la colonne est vide), par MusicBrainz, par le
//!    service, ou à la main (édition de l'album). Il n'est jamais contredit.
//! 2. **À défaut**, [`type_deduit`] range le disque d'après ses PISTES :
//!    * single : de 1 à [`SINGLE_PISTES_MAX`] pistes et moins de
//!      [`SINGLE_DUREE_MAX_MS`] au total ;
//!    * EP : de [`EP_PISTES_MIN`] à [`EP_PISTES_MAX`] pistes et moins de
//!      [`EP_DUREE_MAX_MS`] au total ;
//!    * album sinon ;
//!    * et jamais single ni EP si une piste dure plus de [`PISTE_LONGUE_MS`]
//!      (10 min).
//! 3. **Compilations exclues** : un disque `is_compilation` n'est jamais
//!    déduit. Les albums live ne portent aucun drapeau dans Tune : un live
//!    typé par MusicBrainz reste un `album` (règle 1) ; un live sans type
//!    n'est pas reconnu comme tel, et la règle 2 s'y applique comme à tout
//!    disque.
//!
//! Le type DÉDUIT n'est jamais écrit dans `albums.release_type` : il est
//! calculé à la lecture et publié à part (`inferred_release_type`), pour que
//! la réponse explicite garde son statut et que la règle reste réversible.
//! Une durée de piste inconnue suspend la déduction : on ne compare pas une
//! somme partielle à un seuil.
//!
//! # Les types secondaires ne décident de rien
//!
//! Un groupe de sortie MusicBrainz porte un `primary-type` ET des
//! `secondary-types` (Live, Compilation, Soundtrack, Remix, Demo…). Les
//! seconds ne changent JAMAIS le premier : un album live est un `album`, un
//! album de remixes est un `album`. Pour « compilation », la colonne qui fait
//! foi reste `albums.is_compilation`, écrite par le scan d'après les tags
//! (#1957) — ce module ne la touche pas.
//!
//! Ils sont STOCKÉS à part, dans `albums.release_secondary_types`
//! (`live;compilation`, voir [`TYPES_SECONDAIRES`]), posés au scan depuis la
//! même balise que le primaire et sous la même règle : jamais par-dessus une
//! valeur déjà connue. Un seul sert à ranger : `live`, qui envoie le disque
//! dans la section « Live » de la fiche artiste ([`est_live`], décision de
//! Bertrand du 05/10/2026) — et là, il prime sur le primaire.

use serde_json::Value;
use tracing::{debug, info, warn};

/// Le vocabulaire, et il est celui de MusicBrainz.
///
/// Les cinq `primary-type` que MusicBrainz définit pour un groupe de sortie.
/// On n'en invente pas un sixième, et on n'en replie pas deux en un : le
/// client décide quoi mettre dans quelle section, pas ce module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDeSortie {
    Album,
    Ep,
    Single,
    /// Enregistrement de diffusion (radio, télévision).
    Broadcast,
    /// Le fourre-tout de MusicBrainz : livre audio, entretien, interlude…
    Autre,
}

impl TypeDeSortie {
    /// Le mot stocké dans `albums.release_type` et publié par les routes.
    pub fn as_str(self) -> &'static str {
        match self {
            TypeDeSortie::Album => "album",
            TypeDeSortie::Ep => "ep",
            TypeDeSortie::Single => "single",
            TypeDeSortie::Broadcast => "broadcast",
            TypeDeSortie::Autre => "other",
        }
    }

    /// Reconnaît un mot du vocabulaire, quelle que soit sa casse.
    ///
    /// Un mot INCONNU rend `None` — pas `Autre`. Les deux sont différents :
    /// `Autre` est ce que MusicBrainz appelle « Other », une réponse ; un mot
    /// non reconnu est une absence de réponse, et une absence ne se convertit
    /// pas en réponse.
    pub fn depuis_mot(brut: &str) -> Option<Self> {
        match brut.trim().to_lowercase().as_str() {
            "album" => Some(TypeDeSortie::Album),
            "ep" => Some(TypeDeSortie::Ep),
            "single" => Some(TypeDeSortie::Single),
            "broadcast" => Some(TypeDeSortie::Broadcast),
            "other" => Some(TypeDeSortie::Autre),
            _ => None,
        }
    }
}

/// Le type de sortie d'un GROUPE DE SORTIE MusicBrainz, tel que
/// `GET /ws/2/release-group/<mbid>?fmt=json` le rend.
///
/// Lit `primary-type`, et lui SEUL. `secondary-types` est délibérément ignoré
/// : un album live (`primary-type: Album`, `secondary-types: ["Live"]`) est un
/// album, pas autre chose. Laisser un type secondaire l'emporter classerait
/// toute la discographie live de Neil Young hors de « Albums principaux ».
///
/// `None` quand le champ est absent, vide ou inconnu de notre vocabulaire —
/// MusicBrainz est un wiki, et un groupe de sortie sans type existe.
pub fn depuis_groupe_musicbrainz(groupe: &Value) -> Option<TypeDeSortie> {
    let brut = groupe
        .get("primary-type")
        .and_then(Value::as_str)
        // MusicBrainz rend aussi `primary-type` sous ce nom dans certaines de
        // ses réponses de recherche. Même champ, même sens.
        .or_else(|| groupe.get("primary_type").and_then(Value::as_str))?;
    TypeDeSortie::depuis_mot(brut)
}

/// Le type qu'un SERVICE de streaming annonce pour un de SES albums.
///
/// Tidal écrit `ALBUM` / `EP` / `SINGLE` dans `type` ; Qobuz écrit `album` /
/// `ep` / `single` dans `release_type`. Même vocabulaire, autre casse — d'où
/// le passage par [`TypeDeSortie::depuis_mot`].
///
/// Un mot que le service ajouterait demain (Qobuz sert par exemple
/// `compilation` et `epMini` selon les catalogues) rend `None` : le type reste
/// inconnu plutôt que replié de force sur `album`. Une fausse certitude coûte
/// plus cher qu'une section muette.
pub fn depuis_service(brut: &str) -> Option<TypeDeSortie> {
    TypeDeSortie::depuis_mot(brut)
}

/// Ce que dit la balise de type d'un fichier (#5616, décision du 05/10/2026).
///
/// Picard écrit le type MusicBrainz du groupe de sortie dans les fichiers :
/// `RELEASETYPE` (Vorbis), `TXXX:MusicBrainz Album Type` (ID3v2),
/// `----:com.apple.iTunes:MusicBrainz Album Type` (MP4), `MUSICBRAINZ_ALBUMTYPE`
/// (APE). Lofty les rassemble sous `ItemKey::MusicBrainzReleaseType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeDuTag {
    /// Le type primaire, celui qui va dans `albums.release_type`.
    pub primaire: TypeDeSortie,
    /// La balise dit `live` (type secondaire MusicBrainz).
    pub live: bool,
    /// La balise dit `compilation` (type secondaire MusicBrainz).
    pub compilation: bool,
}

/// Lit les valeurs d'une balise de type de sortie.
///
/// Les valeurs peuvent venir en plusieurs champs (Vorbis : `RELEASETYPE=album`
/// puis `RELEASETYPE=live`) ou en un seul, séparées par `;`, `/`, `,` ou le
/// séparateur nul d'ID3v2.4 (`album; live`). La casse ne compte pas.
///
/// * Le premier mot PRIMAIRE (`album`, `ep`, `single`, `broadcast`, `other`)
///   est le type.
/// * `live` et `compilation` sont des types SECONDAIRES : ils ne changent pas
///   le primaire (`album;live` reste un album, comme chez MusicBrainz). Seuls,
///   sans primaire, ils désignent un album.
/// * Tout autre mot est ignoré. Rien de reconnu : `None`, le type reste
///   inconnu.
pub fn depuis_valeurs_de_tag<'a>(valeurs: impl IntoIterator<Item = &'a str>) -> Option<TypeDuTag> {
    let mut primaire = None;
    let mut live = false;
    let mut compilation = false;
    for valeur in valeurs {
        for mot in valeur.split([';', '/', ',', '\0']) {
            let mot = mot.trim().to_lowercase();
            match mot.as_str() {
                "live" => live = true,
                "compilation" => compilation = true,
                _ => {
                    if primaire.is_none() {
                        primaire = TypeDeSortie::depuis_mot(&mot);
                    }
                }
            }
        }
    }
    let primaire = match primaire {
        Some(p) => p,
        None if live || compilation => TypeDeSortie::Album,
        None => return None,
    };
    Some(TypeDuTag {
        primaire,
        live,
        compilation,
    })
}

/// Les TYPES SECONDAIRES de MusicBrainz, dans le mot stocké par
/// `albums.release_secondary_types` (section « Live », Bertrand, 05/10/2026).
///
/// C'est la liste des `secondary-types` d'un groupe de sortie MusicBrainz, mise
/// en bas de casse. On n'en invente pas d'autre : un mot hors de cette liste
/// n'est pas stocké.
pub const TYPES_SECONDAIRES: [&str; 12] = [
    "compilation",
    "soundtrack",
    "spokenword",
    "interview",
    "audiobook",
    "audio drama",
    "live",
    "remix",
    "dj-mix",
    "mixtape/street",
    "demo",
    "field recording",
];

/// Le séparateur de `albums.release_secondary_types` : `live;compilation`.
pub const SEPARATEUR_SECONDAIRES: char = ';';

/// Le type secondaire qui range un disque dans la section « Live ».
pub const SECONDAIRE_LIVE: &str = "live";

/// Reconnaît un type secondaire, quelle que soit sa casse et son écriture
/// (`Spoken Word`, `DJ Mix`, `Mixtape` ou `Street` seuls, parce que `/` est
/// aussi un séparateur de la balise).
fn secondaire_depuis_mot(mot: &str) -> Option<&'static str> {
    let mot = mot.trim().to_lowercase();
    let compact: String = mot.chars().filter(|c| c.is_alphanumeric()).collect();
    let trouve = match compact.as_str() {
        "spokenword" => "spokenword",
        "audiodrama" => "audio drama",
        "djmix" => "dj-mix",
        "mixtape" | "street" | "mixtapestreet" => "mixtape/street",
        "fieldrecording" => "field recording",
        _ => return TYPES_SECONDAIRES.iter().copied().find(|t| *t == mot),
    };
    Some(trouve)
}

/// Les types SECONDAIRES que porte une balise de type de sortie, sans doublon,
/// dans l'ordre de lecture. Mêmes valeurs et mêmes séparateurs que
/// [`depuis_valeurs_de_tag`] : `album; live` donne `["live"]`, `RELEASETYPE`
/// en deux champs `album` puis `live` aussi. Un primaire (`album`, `ep`…) ou
/// un mot inconnu n'y entre pas.
pub fn secondaires_depuis_valeurs_de_tag<'a>(
    valeurs: impl IntoIterator<Item = &'a str>,
) -> Vec<&'static str> {
    let mut vus: Vec<&'static str> = Vec::new();
    for valeur in valeurs {
        for mot in valeur.split([';', '/', ',', '\0']) {
            if let Some(t) = secondaire_depuis_mot(mot)
                && !vus.contains(&t)
            {
                vus.push(t);
            }
        }
    }
    vus
}

/// La valeur de colonne de [`secondaires_depuis_valeurs_de_tag`] :
/// `live;compilation`, ou `None` quand la balise n'en porte aucun.
pub fn colonne_des_secondaires<'a>(valeurs: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let secondaires = secondaires_depuis_valeurs_de_tag(valeurs);
    (!secondaires.is_empty()).then(|| secondaires.join(&SEPARATEUR_SECONDAIRES.to_string()))
}

/// Relit `albums.release_secondary_types` : la liste des types, sans vide.
pub fn secondaires_de_la_colonne(colonne: &str) -> Vec<String> {
    colonne
        .split(SEPARATEUR_SECONDAIRES)
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Le disque va-t-il dans la section « Live » ? Oui dès que ses types
/// secondaires portent `live`, QUEL QUE SOIT son type primaire : un EP live
/// est un live, pas un EP (Bertrand, 05/10/2026).
pub fn est_live(secondaires: &[String]) -> bool {
    secondaires.iter().any(|t| t == SECONDAIRE_LIVE)
}

/// Remplit `albums.release_type` depuis MusicBrainz, pour les disques dont le
/// groupe de sortie est connu.
///
/// # Ce qu'elle ne fait pas
///
/// * Elle ne touche PAS aux albums sans `musicbrainz_release_group_id` : ils
///   restent de type inconnu. Chercher leur groupe par titre+artiste
///   ramènerait un voisin, donc un type faux.
/// * Elle n'écrit jamais « inconnu » : une réponse muette laisse la colonne
///   NULLE, telle qu'elle était.
///
/// # Rythme
///
/// MusicBrainz n'accepte qu'UNE requête par seconde. Chaque tour attend donc
/// [`super::musicbrainz_release::rate_limit_delay`] — le même délai que les
/// autres passes MusicBrainz de ce dépôt, pas un second mécanisme. Le compteur
/// est volontairement pauvre : c'est une tâche de fond, elle se raconte dans
/// le journal et n'occupe aucun écran.
///
/// Rend `(examinés, remplis)`.
pub async fn remplir_types_depuis_musicbrainz(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
) -> (usize, usize) {
    let repo = crate::db::album_repo::AlbumRepo::with_backend(db);
    let candidats = match repo.albums_sans_type_de_sortie() {
        Ok(v) => v,
        Err(e) => {
            warn!(erreur = %e, "types_de_sortie_liste_impossible");
            return (0, 0);
        }
    };
    if candidats.is_empty() {
        info!("types_de_sortie_rien_a_faire");
        return (0, 0);
    }
    info!(
        candidats = candidats.len(),
        "types_de_sortie_passe_demarree"
    );

    let mut remplis = 0usize;
    for (album_id, groupe) in &candidats {
        // La passe se GARE quand l'utilisateur suspend l'enrichissement, et
        // repart au meme index (#4574). C'est le mecanisme existant, celui que
        // la passe d'images d'artistes et l'enrichissement des metadonnees
        // utilisent deja : rien ici n'en ouvre un second. La liste etant tenue
        // en memoire, sortir de la boucle perdrait le curseur.
        crate::taches_de_fond::attendre_la_reprise(crate::taches_de_fond::Tache::Enrichissement)
            .await;
        super::musicbrainz_release::rate_limit_delay().await;
        match super::musicbrainz_release::lookup_release_group_type(groupe).await {
            Some(t) => {
                if let Err(e) = repo.definir_type_de_sortie(*album_id, t.as_str()) {
                    warn!(album_id, erreur = %e, "type_de_sortie_ecriture_impossible");
                    continue;
                }
                remplis += 1;
                debug!(album_id, type_de_sortie = t.as_str(), "type_de_sortie_pose");
            }
            None => {
                // La colonne reste NULLE : « MusicBrainz n'a pas répondu » et
                // « MusicBrainz dit album » sont deux états différents, et
                // c'est tout l'objet de cette tranche.
                debug!(album_id, groupe = %groupe, "type_de_sortie_inconnu");
            }
        }
    }

    info!(
        examines = candidats.len(),
        remplis,
        inconnus = candidats.len() - remplis,
        "types_de_sortie_passe_terminee"
    );
    (candidats.len(), remplis)
}

/// Seuils de la règle de repli (#5616). Bornes STRICTES pour les durées
/// (« moins de »), inclusives pour les nombres de pistes.
///
/// Un single : 1 à 3 pistes.
pub const SINGLE_PISTES_MAX: u32 = 3;
/// … et moins de 15 minutes au total.
pub const SINGLE_DUREE_MAX_MS: u64 = 15 * 60 * 1000;
/// Un EP : 4 à 6 pistes…
pub const EP_PISTES_MIN: u32 = 4;
/// (borne haute incluse)
pub const EP_PISTES_MAX: u32 = 6;
/// … et moins de 30 minutes au total.
pub const EP_DUREE_MAX_MS: u64 = 30 * 60 * 1000;
/// Une piste de PLUS de 10 minutes interdit single et EP : le disque est un
/// album (exception du 05/10/2026, d'après la règle de FabienM, fil 2096).
pub const PISTE_LONGUE_MS: u64 = 10 * 60 * 1000;

/// Ce que la règle de repli sait des pistes d'un disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PistesDuDisque {
    /// Nombre de pistes du disque.
    pub nombre: u32,
    /// Somme des durées connues, en millisecondes.
    pub duree_totale_ms: u64,
    /// Nombre de pistes dont la durée est inconnue (nulle ou absente).
    pub durees_inconnues: u32,
    /// Durée de la piste la plus longue, en millisecondes.
    pub piste_la_plus_longue_ms: u64,
}

/// La règle de repli seule : single, EP ou album, d'après les pistes.
///
/// `None` quand elle ne peut pas conclure : aucune piste, ou une durée
/// inconnue (une somme partielle passerait un disque long sous un seuil).
pub fn deduire_depuis_pistes(p: PistesDuDisque) -> Option<TypeDeSortie> {
    if p.nombre == 0 || p.durees_inconnues > 0 {
        return None;
    }
    // Exception des pistes longues (Bertrand, 05/10/2026) : un disque qui
    // porte au moins une piste de plus de 10 minutes n'est jamais un single
    // ni un EP.
    if p.piste_la_plus_longue_ms > PISTE_LONGUE_MS {
        return Some(TypeDeSortie::Album);
    }
    if p.nombre <= SINGLE_PISTES_MAX && p.duree_totale_ms < SINGLE_DUREE_MAX_MS {
        return Some(TypeDeSortie::Single);
    }
    if (EP_PISTES_MIN..=EP_PISTES_MAX).contains(&p.nombre) && p.duree_totale_ms < EP_DUREE_MAX_MS {
        return Some(TypeDeSortie::Ep);
    }
    Some(TypeDeSortie::Album)
}

/// Le type DÉDUIT d'un disque, ou `None` quand il n'y a rien à déduire.
///
/// `None` dans trois cas, et c'est voulu :
/// * le disque a un type explicite reconnu (`explicite`) : il gagne toujours,
///   et rien n'est publié à côté ;
/// * c'est une compilation : la règle ne la touche pas ;
/// * la règle ne peut pas conclure (voir [`deduire_depuis_pistes`]).
pub fn type_deduit(
    explicite: Option<&str>,
    est_compilation: bool,
    pistes: Option<PistesDuDisque>,
) -> Option<TypeDeSortie> {
    if explicite.and_then(TypeDeSortie::depuis_mot).is_some() || est_compilation {
        return None;
    }
    deduire_depuis_pistes(pistes?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn les_cinq_types_primaires_de_musicbrainz_sont_reconnus() {
        for (brut, attendu) in [
            ("Album", "album"),
            ("Single", "single"),
            ("EP", "ep"),
            ("Broadcast", "broadcast"),
            ("Other", "other"),
        ] {
            let groupe = json!({ "primary-type": brut });
            assert_eq!(
                depuis_groupe_musicbrainz(&groupe).map(TypeDeSortie::as_str),
                Some(attendu),
                "`{brut}` doit se lire `{attendu}`"
            );
        }
    }

    /// 🔴 Le cœur de l'issue : un album LIVE reste un album.
    ///
    /// Contre-épreuve : faire lire `secondary-types` à
    /// [`depuis_groupe_musicbrainz`] fait rougir ce test — et classerait toute
    /// la discographie live de Neil Young hors de « Albums principaux ».
    #[test]
    fn un_type_secondaire_ne_transforme_pas_l_album() {
        for secondaires in [
            json!(["Live"]),
            json!(["Compilation"]),
            json!(["Soundtrack", "Live"]),
            json!(["Remix"]),
        ] {
            let groupe = json!({
                "primary-type": "Album",
                "secondary-types": secondaires,
            });
            assert_eq!(
                depuis_groupe_musicbrainz(&groupe).map(TypeDeSortie::as_str),
                Some("album"),
                "les types secondaires {secondaires:?} ne décident de rien"
            );
        }

        // Et le symétrique : un single live reste un single.
        let single_live = json!({
            "primary-type": "Single",
            "secondary-types": ["Live"],
        });
        assert_eq!(
            depuis_groupe_musicbrainz(&single_live).map(TypeDeSortie::as_str),
            Some("single")
        );
    }

    /// Un groupe de sortie sans type — MusicBrainz est un wiki — ne produit
    /// pas un type par défaut.
    #[test]
    fn un_groupe_sans_type_reste_inconnu() {
        assert!(depuis_groupe_musicbrainz(&json!({})).is_none());
        assert!(depuis_groupe_musicbrainz(&json!({ "primary-type": null })).is_none());
        assert!(depuis_groupe_musicbrainz(&json!({ "primary-type": "" })).is_none());
        assert!(
            depuis_groupe_musicbrainz(&json!({ "primary-type": "Mixtape/Street" })).is_none(),
            "un type hors vocabulaire n'est pas replié sur `other` : \
             « Other » est une réponse de MusicBrainz, pas un fourre-tout local"
        );
    }

    #[test]
    fn le_vocabulaire_des_services_se_lit_quelle_que_soit_la_casse() {
        // Tidal : `type` en capitales.
        assert_eq!(
            depuis_service("ALBUM").map(TypeDeSortie::as_str),
            Some("album")
        );
        assert_eq!(depuis_service("EP").map(TypeDeSortie::as_str), Some("ep"));
        assert_eq!(
            depuis_service("SINGLE").map(TypeDeSortie::as_str),
            Some("single")
        );
        // Qobuz : `release_type` en bas de casse.
        assert_eq!(
            depuis_service("album").map(TypeDeSortie::as_str),
            Some("album")
        );
        assert_eq!(depuis_service("ep").map(TypeDeSortie::as_str), Some("ep"));
        // Un mot hors vocabulaire reste inconnu plutôt que replié sur `album`.
        assert!(depuis_service("compilation").is_none());
        assert!(depuis_service("").is_none());
    }

    /// Témoin : le vocabulaire écrit en base est exactement celui que le
    /// client lira. Si un jour quelqu'un renomme `other` en `autre`, ce test
    /// le dit avant que le client ne cesse de trier.
    #[test]
    fn le_mot_ecrit_et_le_mot_relu_sont_le_meme() {
        for t in [
            TypeDeSortie::Album,
            TypeDeSortie::Ep,
            TypeDeSortie::Single,
            TypeDeSortie::Broadcast,
            TypeDeSortie::Autre,
        ] {
            assert_eq!(
                TypeDeSortie::depuis_mot(t.as_str()),
                Some(t),
                "`{}` doit se relire tel quel",
                t.as_str()
            );
        }
    }
    const MIN: u64 = 60 * 1000;

    fn pistes(nombre: u32, minutes: u64) -> Option<PistesDuDisque> {
        // Pistes de durée égale : la plus longue vaut la moyenne.
        Some(PistesDuDisque {
            nombre,
            duree_totale_ms: minutes * MIN,
            durees_inconnues: 0,
            piste_la_plus_longue_ms: if nombre == 0 {
                0
            } else {
                minutes * MIN / nombre as u64
            },
        })
    }

    #[test]
    fn le_type_explicite_gagne_toujours() {
        // Un « album » MusicBrainz de 2 titres courts reste un album, et un
        // « single » de 12 titres reste un single : rien n'est déduit.
        assert_eq!(type_deduit(Some("album"), false, pistes(2, 8)), None);
        assert_eq!(type_deduit(Some("SINGLE"), false, pistes(12, 70)), None);
        assert_eq!(type_deduit(Some("ep"), false, pistes(1, 3)), None);
        assert_eq!(type_deduit(Some("other"), false, pistes(1, 3)), None);
    }

    #[test]
    fn un_type_vide_ou_inconnu_laisse_jouer_la_regle() {
        assert_eq!(
            type_deduit(Some(""), false, pistes(2, 8)),
            Some(TypeDeSortie::Single)
        );
        assert_eq!(
            type_deduit(Some("epMini"), false, pistes(5, 20)),
            Some(TypeDeSortie::Ep)
        );
        assert_eq!(
            type_deduit(None, false, pistes(10, 45)),
            Some(TypeDeSortie::Album)
        );
    }

    #[test]
    fn les_seuils_du_single() {
        assert_eq!(
            deduire_depuis_pistes(pistes(1, 4).unwrap()),
            Some(TypeDeSortie::Single)
        );
        assert_eq!(
            deduire_depuis_pistes(pistes(3, 14).unwrap()),
            Some(TypeDeSortie::Single)
        );
        // 15 min pile : la borne est stricte, ce n'est plus un single, et 3
        // pistes ne font pas un EP → album.
        let quinze = PistesDuDisque {
            nombre: 3,
            duree_totale_ms: SINGLE_DUREE_MAX_MS,
            durees_inconnues: 0,
            piste_la_plus_longue_ms: 0,
        };
        assert_eq!(deduire_depuis_pistes(quinze), Some(TypeDeSortie::Album));
        let juste_sous = PistesDuDisque {
            duree_totale_ms: SINGLE_DUREE_MAX_MS - 1,
            ..quinze
        };
        assert_eq!(
            deduire_depuis_pistes(juste_sous),
            Some(TypeDeSortie::Single)
        );
    }

    #[test]
    fn les_seuils_de_l_ep() {
        assert_eq!(
            deduire_depuis_pistes(pistes(4, 10).unwrap()),
            Some(TypeDeSortie::Ep)
        );
        assert_eq!(
            deduire_depuis_pistes(pistes(6, 29).unwrap()),
            Some(TypeDeSortie::Ep)
        );
        // 4 titres courts : EP, même sous le seuil du single.
        assert_eq!(
            deduire_depuis_pistes(pistes(4, 5).unwrap()),
            Some(TypeDeSortie::Ep)
        );
        let trente = PistesDuDisque {
            nombre: 6,
            duree_totale_ms: EP_DUREE_MAX_MS,
            durees_inconnues: 0,
            piste_la_plus_longue_ms: 0,
        };
        assert_eq!(deduire_depuis_pistes(trente), Some(TypeDeSortie::Album));
        let juste_sous = PistesDuDisque {
            duree_totale_ms: EP_DUREE_MAX_MS - 1,
            ..trente
        };
        assert_eq!(deduire_depuis_pistes(juste_sous), Some(TypeDeSortie::Ep));
    }

    #[test]
    fn hors_des_fourchettes_c_est_un_album() {
        // 7 pistes courtes : au-delà de l'EP.
        assert_eq!(
            deduire_depuis_pistes(pistes(7, 20).unwrap()),
            Some(TypeDeSortie::Album)
        );
        // 5 pistes longues (jazz, classique) : album.
        assert_eq!(
            deduire_depuis_pistes(pistes(5, 42).unwrap()),
            Some(TypeDeSortie::Album)
        );
        // 2 pistes de 20 min : album (pas de règle « EP long »).
        assert_eq!(
            deduire_depuis_pistes(pistes(2, 40).unwrap()),
            Some(TypeDeSortie::Album)
        );
    }

    #[test]
    fn sans_pistes_ou_avec_une_duree_inconnue_on_ne_deduit_rien() {
        assert_eq!(deduire_depuis_pistes(PistesDuDisque::default()), None);
        let trou = PistesDuDisque {
            nombre: 2,
            duree_totale_ms: 4 * MIN,
            durees_inconnues: 1,
            piste_la_plus_longue_ms: 4 * MIN,
        };
        assert_eq!(deduire_depuis_pistes(trou), None);
        assert_eq!(type_deduit(None, false, None), None);
    }

    #[test]
    fn une_compilation_n_est_jamais_deduite() {
        assert_eq!(type_deduit(None, true, pistes(2, 8)), None);
        assert_eq!(type_deduit(None, true, pistes(5, 20)), None);
        assert_eq!(type_deduit(None, true, pistes(15, 70)), None);
    }

    #[test]
    fn les_seuils_sont_ceux_de_la_regle_ecrite() {
        assert_eq!(SINGLE_PISTES_MAX, 3);
        assert_eq!(SINGLE_DUREE_MAX_MS, 900_000);
        assert_eq!((EP_PISTES_MIN, EP_PISTES_MAX), (4, 6));
        assert_eq!(EP_DUREE_MAX_MS, 1_800_000);
    }
    #[test]
    fn une_piste_de_plus_de_dix_minutes_interdit_single_et_ep() {
        let p = |nombre, total_min: u64, longue_ms: u64| PistesDuDisque {
            nombre,
            duree_totale_ms: total_min * MIN,
            durees_inconnues: 0,
            piste_la_plus_longue_ms: longue_ms,
        };
        // 1 piste de 12 min (< 15 min au total) : album, pas single.
        assert_eq!(
            deduire_depuis_pistes(p(1, 12, 12 * MIN)),
            Some(TypeDeSortie::Album)
        );
        // 4 pistes, 25 min, dont une de 11 min : album, pas EP.
        assert_eq!(
            deduire_depuis_pistes(p(4, 25, 11 * MIN)),
            Some(TypeDeSortie::Album)
        );
        // 10 min pile : la borne est stricte, la règle ordinaire s'applique.
        assert_eq!(
            deduire_depuis_pistes(p(1, 10, PISTE_LONGUE_MS)),
            Some(TypeDeSortie::Single)
        );
        assert_eq!(
            deduire_depuis_pistes(p(4, 25, PISTE_LONGUE_MS)),
            Some(TypeDeSortie::Ep)
        );
        assert_eq!(PISTE_LONGUE_MS, 600_000);
    }

    #[test]
    fn la_balise_de_type_se_lit_sous_toutes_ses_formes() {
        let t = |v: &[&str]| depuis_valeurs_de_tag(v.iter().copied());
        let simple = |p| {
            Some(TypeDuTag {
                primaire: p,
                live: false,
                compilation: false,
            })
        };
        assert_eq!(t(&["album"]), simple(TypeDeSortie::Album));
        assert_eq!(t(&["EP"]), simple(TypeDeSortie::Ep));
        assert_eq!(t(&["Single"]), simple(TypeDeSortie::Single));
        // Combinaisons en un champ (Picard, ID3v2.3) ou en plusieurs (Vorbis).
        let live = Some(TypeDuTag {
            primaire: TypeDeSortie::Album,
            live: true,
            compilation: false,
        });
        assert_eq!(t(&["album; live"]), live);
        assert_eq!(t(&["album/live"]), live);
        assert_eq!(t(&["album\0live"]), live);
        assert_eq!(t(&["album", "live"]), live);
        assert_eq!(t(&["live"]), live, "un live seul est un album live");
        assert_eq!(
            t(&["ep;live"]),
            Some(TypeDuTag {
                primaire: TypeDeSortie::Ep,
                live: true,
                compilation: false,
            })
        );
        assert_eq!(
            t(&["compilation"]),
            Some(TypeDuTag {
                primaire: TypeDeSortie::Album,
                live: false,
                compilation: true,
            })
        );
        // Mot inconnu seul : rien.
        assert_eq!(t(&["soundtrack"]), None);
        assert_eq!(t(&[""]), None);
        assert_eq!(t(&[]), None);
        // Le premier primaire gagne ; un mot inconnu ne gêne pas.
        assert_eq!(t(&["remix; single"]), simple(TypeDeSortie::Single));
    }
    #[test]
    fn les_types_secondaires_de_la_balise_section_live() {
        let t = |v: &[&str]| secondaires_depuis_valeurs_de_tag(v.iter().copied());
        assert_eq!(t(&["album; live"]), vec!["live"]);
        assert_eq!(t(&["album", "live"]), vec!["live"]);
        assert_eq!(t(&["album\0live"]), vec!["live"]);
        assert_eq!(t(&["EP;Live;Remix"]), vec!["live", "remix"]);
        assert_eq!(t(&["album; soundtrack"]), vec!["soundtrack"]);
        // `/` est un séparateur : « mixtape/street » arrive en deux mots.
        assert_eq!(t(&["album; mixtape/street"]), vec!["mixtape/street"]);
        assert_eq!(t(&["Spoken Word", "DJ Mix"]), vec!["spokenword", "dj-mix"]);
        assert_eq!(t(&["live", "Live"]), vec!["live"], "sans doublon");
        assert!(t(&["album"]).is_empty());
        assert!(t(&["inconnu"]).is_empty());
        assert!(t(&[]).is_empty());

        assert_eq!(
            colonne_des_secondaires(["album; live; compilation"]).as_deref(),
            Some("live;compilation")
        );
        assert_eq!(colonne_des_secondaires(["single"]), None);
    }

    #[test]
    fn la_colonne_relue_et_la_regle_du_live() {
        assert_eq!(
            secondaires_de_la_colonne("live;compilation"),
            vec!["live".to_string(), "compilation".to_string()]
        );
        assert_eq!(
            secondaires_de_la_colonne(" Live ; ;"),
            vec!["live".to_string()]
        );
        assert!(secondaires_de_la_colonne("").is_empty());
        assert!(est_live(&secondaires_de_la_colonne("remix;live")));
        assert!(!est_live(&secondaires_de_la_colonne("compilation")));
        assert!(!est_live(&[]));
        // Chaque mot stocké se relit tel quel.
        for mot in TYPES_SECONDAIRES {
            assert_eq!(secondaires_depuis_valeurs_de_tag([mot]), vec![mot], "{mot}");
        }
    }
}
