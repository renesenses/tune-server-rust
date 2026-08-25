//! « Nouveautés de vos artistes » — les parutions récentes des services de
//! streaming, restreintes aux artistes que l'utilisateur possède ou aime.
//!
//! ## Le sens de la circulation
//!
//! On ne demande PAS « quoi de neuf chez cet artiste ? » service par service :
//! ce serait un appel réseau par artiste, soit plus d'un millier sur une
//! bibliothèque ordinaire. On tire **le fil des nouveautés de chaque service —
//! un appel par service** — et on l'intersecte avec les noms qu'on connaît.
//!
//! Le coût est donc celui du nombre de services connectés, pas de la taille de
//! la bibliothèque. C'est ce qui rend la section tenable sur l'accueil, qui se
//! charge à chaque ouverture.
//!
//! ## Le rapprochement se fait par NOM
//!
//! Faute d'identifiant partagé entre la bibliothèque et les services. Le nom
//! est donc normalisé — casse, accents, ponctuation, article de tête — et cette
//! normalisation est la seule chose qui décide de ce qui remonte : elle est
//! isolée ici, et testée.
//!
//! Elle reste volontairement conservatrice. « The Beatles » et « Beatles »
//! doivent se rejoindre ; « Bach » et « Johann Sebastian Bach » **non** — un
//! rapprochement large ferait remonter des parutions qui ne sont pas celles de
//! l'artiste qu'on écoute, et une section qui se trompe est pire qu'une section
//! vide.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

/// Les services qui servent un vrai fil de nouveautés daté.
///
/// Bandcamp n'y est pas, et ce n'est pas un oubli : sa page publique de
/// discographie donne titre, lien et pochette — **aucune date**. « Nouveau » y
/// suppose de garder une empreinte par artiste et de la comparer, donc un
/// mécanisme différent, qui ne rend rien avant son deuxième passage.
const SERVICES: [&str; 4] = ["qobuz", "tidal", "deezer", "spotify"];

#[derive(Deserialize)]
pub(super) struct Params {
    limit: Option<usize>,
}

/// Normalise un nom d'artiste pour le rapprochement.
///
/// Minuscules, accents retirés, ponctuation et espaces réduits, article de tête
/// (`the`, `le`, `la`, `les`) enlevé. Rien de plus : chaque règle ajoutée ici
/// élargit ce qui remonte, et l'élargir à tort est le seul vrai risque de cette
/// section.
pub(super) fn nom_normalise(nom: &str) -> String {
    let sans_accents: String = nom
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            autre => autre,
        })
        .collect();

    // Tout ce qui n'est ni lettre ni chiffre devient une coupure de mot :
    // « AC/DC », « AC-DC » et « AC DC » désignent le même groupe.
    let mots: Vec<&str> = sans_accents
        .split(|c: char| !c.is_alphanumeric())
        .filter(|m| !m.is_empty())
        .collect();

    let mots = match mots.split_first() {
        // Un nom REDUIT a un article reste ce nom : « The » seul doit rendre
        // « the », pas la chaine vide — un artiste sans nom ne se rapproche de
        // rien, et pire, une chaine vide rapprocherait entre eux tous les noms
        // vides. D'ou `!reste.is_empty()`.
        Some((premier, reste))
            if !reste.is_empty() && matches!(*premier, "the" | "le" | "la" | "les") =>
        {
            reste.to_vec()
        }
        _ => mots,
    };

    mots.join(" ")
}

/// Les libelles qui ne designent PAS un artiste.
///
/// « Various Artists » est le fourre-tout des compilations. Sur une
/// bibliotheque reelle il portait **121 albums** — et les services publient des
/// parutions sous ce meme libelle. Il se hissait donc EN TETE de la section,
/// devant John Coltrane et Lambchop, qui sont eux de vrais resultats.
///
/// Ces libelles sont ecartes des DEUX cotes du rapprochement : ni comme artiste
/// connu, ni comme artiste d'une parution. Les ecarter d'un seul cote ne
/// servirait a rien — c'est leur rencontre qui produit le faux positif.
///
/// La liste est volontairement courte et litterale. Elle ne contient que des
/// libelles qu'aucun artiste ne porterait : y ajouter « soundtrack » ou
/// « orchestra », par exemple, ecarterait de vrais noms.
const LIBELLES_FOURRE_TOUT: [&str; 8] = [
    "various artists",
    "various",
    "va",
    "unknown artist",
    "unknown",
    "divers",
    "artistes divers",
    "compilation",
];

/// Vrai si ce nom, une fois normalise, ne designe pas un artiste.
pub(super) fn est_un_fourre_tout(nom_normalise_: &str) -> bool {
    LIBELLES_FOURRE_TOUT.contains(&nom_normalise_)
}

/// `GET /home/artist-releases` — les parutions récentes des artistes connus.
pub(super) async fn artist_releases(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Json<Value>, AppError> {
    let limite = p.limit.unwrap_or(20).clamp(1, 100);

    // 1. Les artistes qu'on aime : les favoris explicites, plus l'artiste des
    //    albums et morceaux mis en favori. Ils passeront devant les autres.
    let mut aimes: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sql in [
        "SELECT DISTINCT item_name FROM favorites WHERE item_type = 'artist' AND item_name IS NOT NULL",
        "SELECT DISTINCT item_artist FROM favorites WHERE item_artist IS NOT NULL AND item_artist != ''",
    ] {
        for cols in state.backend.query_many(sql, &[]).unwrap_or_default() {
            if let Some(n) = cols.first().and_then(|v| v.as_string()) {
                let cle = nom_normalise(&n);
                if !est_un_fourre_tout(&cle) {
                    aimes.insert(cle);
                }
            }
        }
    }

    // 1 bis. Les favoris STREAMING comptent autant que les favoris locaux :
    //    un artiste suivi sur Tidal ou Qobuz n'a souvent AUCUNE trace dans la
    //    table `favorites` (Bertrand, 25/08 : compte Tidal fraichement relie,
    //    section muette sur ses artistes suivis). Un appel par service
    //    connecte — le meme budget que les fils de nouveautes plus bas.
    for nom_service in SERVICES {
        let arc = {
            let registre = state.services.lock().await;
            registre.get(nom_service)
            // le verrou du registre tombe ici, comme plus bas
        };
        let Some(arc) = arc else { continue };
        let svc = arc.read().await;
        if !svc.enabled() || !svc.auth_status().await.authenticated {
            continue;
        }
        // Un service en echec n'emporte ni les autres ni la section.
        let Ok(artistes) = svc.get_user_artists().await else {
            continue;
        };
        drop(svc);
        for artiste in artistes {
            let cle = nom_normalise(&artiste.name);
            if !est_un_fourre_tout(&cle) {
                aimes.insert(cle);
            }
        }
    }

    // 2. Les artistes de la bibliotheque, avec le nombre d'albums possedes —
    //    c'est ce qui permet de dire « 5 albums dans votre bibliotheque ».
    let mut connus: std::collections::HashMap<String, (String, i64)> =
        std::collections::HashMap::new();
    let sql_biblio = "SELECT ar.name, COUNT(al.id) \
         FROM artists ar LEFT JOIN albums al ON al.artist_id = ar.id \
         WHERE ar.name IS NOT NULL AND ar.name != '' \
         GROUP BY ar.id, ar.name";
    for cols in state
        .backend
        .query_many(sql_biblio, &[])
        .unwrap_or_default()
    {
        if let Some(nom) = cols.first().and_then(|v| v.as_string()) {
            let cle = nom_normalise(&nom);
            if est_un_fourre_tout(&cle) {
                continue;
            }
            let albums = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            connus.insert(cle, (nom, albums));
        }
    }

    if connus.is_empty() && aimes.is_empty() {
        return Ok(Json(json!([])));
    }

    // 3. Un appel par service connecte, jamais par artiste.
    let mut groupes: Vec<Value> = Vec::new();
    for nom_service in SERVICES {
        let arc = {
            let registre = state.services.lock().await;
            registre.get(nom_service)
            // le verrou du registre tombe ici : on ne le tient pas pendant l'appel reseau
        };
        let Some(arc) = arc else { continue };
        let svc = arc.read().await;
        if !svc.enabled() || !svc.auth_status().await.authenticated {
            continue;
        }
        let Ok(albums) = svc.get_new_releases().await else {
            // Un service en echec ne doit pas emporter les autres : la section
            // vaut mieux incomplete que muette.
            continue;
        };
        drop(svc);

        for album in albums {
            let cle = nom_normalise(&album.artist);
            // La meme garde de ce cote-ci : une parution publiee sous
            // « Various Artists » ne doit rencontrer personne, meme si un
            // libelle avait echappe aux deux collectes ci-dessus.
            if est_un_fourre_tout(&cle) {
                continue;
            }
            let aime = aimes.contains(&cle);
            let Some((affiche, albums_possedes)) = connus.get(&cle).cloned().or_else(|| {
                // Un favori peut ne rien avoir en local (artiste suivi sur un
                // service uniquement) : il compte quand meme.
                aime.then(|| (album.artist.clone(), 0))
            }) else {
                continue;
            };

            let parution = json!({
                "service": nom_service,
                "source_id": album.id,
                "title": album.title,
                "cover_path": album.cover_path,
                "year": album.year,
            });

            match groupes
                .iter_mut()
                .find(|g| g["key"].as_str() == Some(cle.as_str()))
            {
                Some(g) => {
                    if let Some(arr) = g["releases"].as_array_mut() {
                        arr.push(parution);
                    }
                }
                None => groupes.push(json!({
                    "key": cle,
                    "artist_name": affiche,
                    "is_favorite": aime,
                    "library_albums": albums_possedes,
                    "releases": [parution],
                })),
            }
        }
    }

    // 3 bis. Bandcamp, qui n'a pas de fil de nouveautes : ses parutions sont
    //        DEPOSEES par la veille de fond, jamais cherchees ici. Lire un
    //        reglage coute une requete SQL ; aller voir Bandcamp couterait un
    //        appel reseau par artiste, sur une page qui se charge a chaque
    //        ouverture.
    #[cfg(feature = "bandcamp")]
    for depot in crate::bandcamp_sweep::nouveautes_deposees(&state.backend) {
        let Some(nom) = depot.get("artist_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let cle = nom_normalise(nom);
        if est_un_fourre_tout(&cle) {
            continue;
        }
        let parutions: Vec<Value> = depot
            .get("parutions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                json!({
                    "service": "bandcamp",
                    "source_id": p.get("url"),
                    "title": p.get("titre"),
                    "cover_path": p.get("pochette"),
                    // Bandcamp ne date pas sa discographie : annoncer une annee
                    // serait l'inventer.
                    "year": Value::Null,
                })
            })
            .collect();
        if parutions.is_empty() {
            continue;
        }

        let aime = aimes.contains(&cle);
        let albums_possedes = connus.get(&cle).map(|(_, n)| *n).unwrap_or(0);
        match groupes
            .iter_mut()
            .find(|g| g["key"].as_str() == Some(cle.as_str()))
        {
            Some(g) => {
                if let Some(arr) = g["releases"].as_array_mut() {
                    arr.extend(parutions);
                }
            }
            None => groupes.push(json!({
                "key": cle,
                "artist_name": nom,
                "is_favorite": aime,
                "library_albums": albums_possedes,
                "releases": parutions,
            })),
        }
    }

    // 4. Les favoris d'abord, puis ceux dont on possede le plus. Un artiste
    //    dont on a cinq albums merite d'etre annonce avant celui dont on a une
    //    compilation.
    groupes.sort_by(|a, b| {
        let fav = b["is_favorite"].as_bool().cmp(&a["is_favorite"].as_bool());
        fav.then_with(|| {
            b["library_albums"]
                .as_i64()
                .cmp(&a["library_albums"].as_i64())
        })
    });
    groupes.truncate(limite);

    Ok(Json(json!(groupes)))
}

#[cfg(test)]
mod tests {
    use super::{est_un_fourre_tout, nom_normalise};

    #[test]
    fn la_casse_et_les_espaces_ne_comptent_pas() {
        assert_eq!(nom_normalise("  Pink   FLOYD "), "pink floyd");
    }

    #[test]
    fn les_accents_sont_retires() {
        // Les services ecrivent rarement les accents comme les etiquettes des
        // fichiers locaux.
        assert_eq!(nom_normalise("Étienne Daho"), nom_normalise("Etienne Daho"));
        assert_eq!(nom_normalise("Sigur Rós"), "sigur ros");
    }

    #[test]
    fn la_ponctuation_est_une_coupure_de_mot() {
        assert_eq!(nom_normalise("AC/DC"), nom_normalise("AC-DC"));
        assert_eq!(nom_normalise("AC/DC"), nom_normalise("AC DC"));
    }

    #[test]
    fn larticle_de_tete_saute() {
        assert_eq!(nom_normalise("The Beatles"), nom_normalise("Beatles"));
        assert_eq!(nom_normalise("Les Rita Mitsouko"), "rita mitsouko");
    }

    /// La borne du precedent : un nom REDUIT a un article reste ce nom.
    ///
    /// Ecrit d'abord avec « The The », qui ne prouvait rien : les deux mots
    /// font que l'article de tete saute normalement, avec ou sans la garde.
    /// La contre-epreuve l'a montre — le test restait vert la garde retiree.
    /// C'est « The » SEUL qui l'exerce.
    #[test]
    fn un_nom_reduit_a_un_article_le_garde() {
        assert_eq!(nom_normalise("The"), "the");
        assert_eq!(nom_normalise("Les"), "les");
        // Et il ne doit surtout pas devenir vide : deux artistes reduits a un
        // article different se confondraient.
        assert_ne!(nom_normalise("The"), nom_normalise("Les"));
    }

    /// « The The » : les deux mots, donc l'article de tete saute comme partout.
    #[test]
    fn the_the_garde_un_mot() {
        assert_eq!(nom_normalise("The The"), "the");
        assert_ne!(nom_normalise("The The"), nom_normalise("The Beatles"));
    }

    /// Ce que la normalisation NE doit PAS rapprocher.
    ///
    /// C'est le vrai risque de cette section : un rapprochement trop large fait
    /// remonter les parutions de quelqu'un d'autre.
    #[test]
    fn deux_artistes_distincts_ne_se_rejoignent_pas() {
        assert_ne!(
            nom_normalise("Bach"),
            nom_normalise("Johann Sebastian Bach")
        );
        assert_ne!(nom_normalise("Miles Davis"), nom_normalise("Miles"));
        assert_ne!(nom_normalise("John Williams"), nom_normalise("John Adams"));
    }

    /// « Various Artists » n'est pas un artiste, et il etait EN TETE.
    #[test]
    fn les_libelles_fourre_tout_sont_ecartes() {
        for libelle in [
            "Various Artists",
            "various artists",
            "VA",
            "Unknown Artist",
            "Divers",
            "Compilation",
        ] {
            assert!(
                est_un_fourre_tout(&nom_normalise(libelle)),
                "« {libelle} » devrait etre ecarte"
            );
        }
    }

    /// La borne : la liste ne doit pas mordre sur de vrais noms.
    #[test]
    fn de_vrais_artistes_ne_sont_pas_ecartes() {
        for nom in [
            "John Coltrane",
            "Lambchop",
            "The Divine Comedy",  // contient « divine », pas « divers »
            "Various Production", // un vrai groupe, et il commence par « various »
            "Unknown Mortal Orchestra",
        ] {
            assert!(
                !est_un_fourre_tout(&nom_normalise(nom)),
                "« {nom} » ne devrait PAS etre ecarte"
            );
        }
    }

    #[test]
    fn un_nom_vide_ou_ponctuation_seule_rend_une_chaine_vide() {
        assert_eq!(nom_normalise(""), "");
        assert_eq!(nom_normalise(" -- / "), "");
    }
}
