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
//! **Aucune heuristique** : ni le nombre de titres, ni la durée totale. Un
//! disque de quatre titres peut être un album, un single peut en porter six
//! avec ses remixes. C'est écrit dans l'issue : *un classement faux est pire
//! qu'une section absente*. Ce module ne contient donc, délibérément, aucune
//! fonction qui regarde `track_count`.
//!
//! # Les types secondaires ne décident de rien
//!
//! Un groupe de sortie MusicBrainz porte un `primary-type` ET des
//! `secondary-types` (Live, Compilation, Soundtrack, Remix, Demo…). Les
//! seconds ne changent JAMAIS le premier : un album live est un `album`, un
//! album de remixes est un `album`. Pour « compilation », la colonne qui fait
//! foi reste `albums.is_compilation`, écrite par le scan d'après les tags
//! (#1957) — ce module ne la touche pas.

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
}
