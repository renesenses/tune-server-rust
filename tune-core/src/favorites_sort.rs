//! Tri des favoris (#2001) — « l'ordre d'ajout est subi ».
//!
//! Tades a enregistré ses favoris dans le désordre et voulait « les écouter
//! dans l'ordre séquentiel ». Le client web sait trier depuis la v0.9.96
//! (`favoritesSort.ts`), mais **lui seul** : les clients Flutter, Swift, le
//! widget et l'UPnP reçoivent toujours l'ordre d'ajout, parce que le tri
//! n'existait nulle part côté serveur.
//!
//! Ce module porte les mêmes trois règles que le module web, pour qu'un même
//! catalogue se range pareil quel que soit le client :
//!
//! 1. **les accents se rangent avec leur lettre** — « Éric » entre « Eric » et
//!    « Erik », pas après « Zoé » ;
//! 2. **un champ absent finit la liste, quel que soit le sens** — descendre le
//!    tri ne doit pas remonter les trous en tête ;
//! 3. **« Volume 2 » précède « Volume 10 »** — les nombres se comparent comme
//!    des nombres, pas comme du texte.
//!
//! ## Piste 2 — l'ordre manuel (`sort=manual`)
//!
//! Le tri ci-dessus range d'après un champ ; il ne rend pas le geste de Tades,
//! qui voulait **déplacer** un favori. [`CleDeTri::Manuel`] lit un rang écrit
//! par l'utilisateur (colonne `position`), posé par les routes
//! `POST /profiles/{id}/favorites/reorder` et `…/favorites/streaming/reorder`.
//!
//! **Périmètre.** Le rang manuel n'existe que sur les deux tables que Tune
//! possède — `favorites` (bibliothèque locale) et `streaming_favorites`
//! (favoris de service enregistrés chez Tune) — et il est **par onglet** :
//! la clé du rang est `(profil, item_type)`. Il n'existe PAS sur les favoris
//! lus en direct chez Qobuz/Tidal (`/streaming/{service}/favorites/{type}`) :
//! ces lignes reviennent du service à chaque resynchronisation, Tune n'en
//! possède aucune, et leur donner un rang durable demanderait une table de
//! correspondance — arbitrage non rendu (cf. #2001, piste 2). Une demande
//! `sort=manual` sur cette route-là ne range donc rien et ne fait pas d'erreur.
//!
//! Rétro-compatibilité : sans paramètre `sort`, [`TriFavoris::depuis`] rend
//! `None` et l'appelant garde son `ORDER BY` d'origine. Le tri se fait en Rust
//! et non en SQL, pour deux raisons : les règles ci-dessus n'ont pas
//! d'équivalent portable entre SQLite et PostgreSQL (collations, tri naturel),
//! et un tri appliqué APRÈS coup laisse intacts les caches qui mémorisent la
//! réponse brute d'un service (cf. le cache 120 s de #1621/#2818).

use std::cmp::Ordering;

/// Le champ sur lequel ranger. `Ajout` = l'ordre rendu par la requête, laissé
/// tel quel puis éventuellement retourné : c'est déjà `created_at DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleDeTri {
    Ajout,
    Titre,
    Artiste,
    Album,
    /// L'ordre que l'utilisateur a posé **à la main** (#2001, piste 2) — le
    /// geste que Tades avait tenté à la souris. Lu dans la colonne `position`
    /// de `favorites` / `streaming_favorites`, écrite par les routes
    /// `…/favorites/reorder` et `…/favorites/streaming/reorder`.
    ///
    /// Un favori jamais rangé à la main n'a pas de rang : il finit la liste,
    /// dans les deux sens, comme n'importe quel champ absent (règle 2).
    Manuel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sens {
    Croissant,
    Decroissant,
}

/// Un tri demandé : une clé et un sens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriFavoris {
    pub cle: CleDeTri,
    pub sens: Sens,
}

impl TriFavoris {
    /// Lit les paramètres de requête `sort` et `order`.
    ///
    /// Rend `None` dès que `sort` est absent, vide ou inconnu : l'appelant doit
    /// alors rendre la liste **exactement** comme avant. Un `sort` mal
    /// orthographié ne fait donc pas d'erreur — il ne trie simplement pas, ce
    /// qui vaut mieux qu'un 400 sur une route de lecture que trois clients
    /// appellent en parallèle.
    pub fn depuis(sort: Option<&str>, order: Option<&str>) -> Option<Self> {
        let brut = sort?.trim().to_ascii_lowercase();
        let cle = match brut.as_str() {
            "added" | "ajout" | "created_at" | "date" => CleDeTri::Ajout,
            "title" | "titre" | "name" | "nom" => CleDeTri::Titre,
            "artist" | "artiste" => CleDeTri::Artiste,
            "album" => CleDeTri::Album,
            "manual" | "manuel" | "custom" | "ordre" | "position" => CleDeTri::Manuel,
            _ => return None,
        };
        let sens = match order.map(|o| o.trim().to_ascii_lowercase()).as_deref() {
            Some("desc") | Some("descending") | Some("decroissant") => Sens::Decroissant,
            _ => Sens::Croissant,
        };
        Some(Self { cle, sens })
    }
}

/// Range `items` selon `sens`, la valeur de chaque élément étant donnée par
/// `cle`. Le tri est **stable** : deux éléments dont la clé est identique — ou
/// tous deux absents — gardent l'ordre que la requête leur avait donné, ce qui
/// préserve `created_at DESC` comme départage.
pub fn trier_par<T>(items: &mut [T], sens: Sens, cle: impl Fn(&T) -> Option<String>) {
    items.sort_by(|a, b| comparer(cle(a).as_deref(), cle(b).as_deref(), sens));
}

/// Applique la clé `Ajout`, qui n'a aucun champ à lire : la source rend déjà
/// les favoris du plus récent au plus ancien (`ORDER BY created_at DESC` en
/// base, ordre du service pour Qobuz/Tidal). `Decroissant` est donc l'ordre
/// existant, et `Croissant` le renverse pour donner « du plus ancien au plus
/// récent » — l'ordre dans lequel Tades les avait ajoutés, précisément ce
/// qu'il cherchait à réécouter.
pub fn appliquer_ajout<T>(items: &mut [T], sens: Sens) {
    if sens == Sens::Croissant {
        items.reverse();
    }
}

/// Range `items` selon le **rang manuel** rendu par `rang` (#2001, piste 2).
///
/// Le rang est un entier, pas du texte : il se compare **numériquement**, ce
/// qui compte ici plus qu'ailleurs. Le miroir PostgreSQL de ce dépôt stocke
/// `position` en TEXT (comme `listen_history.context_position`, PG 046) ; un
/// `ORDER BY position` en SQL rangerait donc « 10 » avant « 2 » sur PostgreSQL
/// et pas sur SQLite — deux ordres différents pour la même bibliothèque. Le
/// rang est relu par `SqlValue::as_i64`, qui convertit le TEXT, et comparé
/// ici : les deux moteurs rendent le même ordre.
///
/// Un favori sans rang (`None` — jamais rangé à la main, ou ajouté après le
/// dernier réordonnancement) finit la liste **dans les deux sens**, règle 2.
/// Le tri restant stable, ces sans-rang gardent entre eux le `created_at DESC`
/// de la requête.
pub fn trier_par_rang<T>(items: &mut [T], sens: Sens, rang: impl Fn(&T) -> Option<i64>) {
    items.sort_by(|a, b| comparer_rang(rang(a), rang(b), sens));
}

/// Compare deux rangs manuels. Voir [`trier_par_rang`].
pub fn comparer_rang(a: Option<i64>, b: Option<i64>, sens: Sens) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => match sens {
            Sens::Croissant => a.cmp(&b),
            Sens::Decroissant => b.cmp(&a),
        },
    }
}

/// Compare deux valeurs de champ. Un champ absent — `None`, vide ou blanc —
/// part en fin de liste **dans les deux sens** (règle 2).
pub fn comparer(a: Option<&str>, b: Option<&str>, sens: Sens) -> Ordering {
    let a = a.map(str::trim).filter(|s| !s.is_empty());
    let b = b.map(str::trim).filter(|s| !s.is_empty());
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let ord = comparer_naturel(a, b);
            match sens {
                Sens::Croissant => ord,
                Sens::Decroissant => ord.reverse(),
            }
        }
    }
}

/// Range **sur place** la liste que porte une réponse de service de streaming,
/// de la forme `{"tracks": [...]}` / `{"albums": …}` / `{"artists": …}` /
/// `{"playlists": …}`.
///
/// Cette fonction s'applique **après** le cache de contenu utilisateur (#1621 /
/// PR #2818) et jamais avant : le cache mémorise la réponse **brute** du
/// service, clé sur `(service, ressource)`, et le tri n'est qu'une vue posée
/// dessus au moment de répondre. Conséquences voulues :
///
/// - la clé du cache n'a pas à porter le tri — sinon quatre tris feraient
///   quatre entrées, donc quatre appels au service là où il en faut un ;
/// - aucune purge supplémentaire n'est nécessaire : trier ne mute rien, et les
///   douze sites de `purge_contenu_utilisateur` gardent exactement leur rôle.
///
/// Les noms de champs suivent la sérialisation de `StreamTrack` / `StreamAlbum`
/// / `StreamArtist` / `StreamPlaylist` : `title` ou `name` pour le titre,
/// `artist_name` pour l'artiste, `album_title` pour l'album.
pub fn trier_liste_json(donnees: &mut serde_json::Value, tri: TriFavoris) {
    let Some(objet) = donnees.as_object_mut() else {
        return;
    };
    for (_, valeur) in objet.iter_mut() {
        let Some(liste) = valeur.as_array_mut() else {
            continue;
        };
        let champ = match tri.cle {
            CleDeTri::Ajout => {
                appliquer_ajout(liste, tri.sens);
                continue;
            }
            // Un ordre manuel n'existe QUE sur les lignes que Tune possède
            // (`favorites`, `streaming_favorites`). Ici la liste vient d'être
            // lue chez Qobuz/Tidal : ces éléments n'ont pas de rang, et le
            // service en renvoie un jeu qui change à chaque resynchronisation.
            // On laisse donc l'ordre du service intact plutôt que d'inventer
            // un rang — voir la note de périmètre en tête de module.
            CleDeTri::Manuel => continue,
            CleDeTri::Titre => &["title", "name"][..],
            CleDeTri::Artiste => &["artist_name", "artist"][..],
            CleDeTri::Album => &["album_title", "album"][..],
        };
        trier_par(liste, tri.sens, |item| {
            champ
                .iter()
                .find_map(|c| item.get(*c).and_then(|v| v.as_str()))
                .map(str::to_string)
        });
    }
}

/// Un morceau de clé : une suite de chiffres, ou une suite de tout le reste.
#[derive(Debug, PartialEq, Eq)]
enum Morceau {
    /// Chiffres, zéros de tête retirés. Comparés d'abord par longueur, ce qui
    /// évite tout débordement sur un « nombre » de cent chiffres.
    Nombre(String),
    Texte(String),
}

impl Ord for Morceau {
    fn cmp(&self, autre: &Self) -> Ordering {
        match (self, autre) {
            (Morceau::Nombre(a), Morceau::Nombre(b)) => {
                a.len().cmp(&b.len()).then_with(|| a.cmp(b))
            }
            (Morceau::Texte(a), Morceau::Texte(b)) => a.cmp(b),
            // Un nombre passe avant du texte : « 2 Unlimited » avant « ABBA ».
            (Morceau::Nombre(_), Morceau::Texte(_)) => Ordering::Less,
            (Morceau::Texte(_), Morceau::Nombre(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Morceau {
    fn partial_cmp(&self, autre: &Self) -> Option<Ordering> {
        Some(self.cmp(autre))
    }
}

fn comparer_naturel(a: &str, b: &str) -> Ordering {
    let (ma, mb) = (decouper(a), decouper(b));
    ma.cmp(&mb)
        // Deux libellés qui ne diffèrent que par la casse ou les accents ne
        // doivent pas s'échanger d'un appel à l'autre : on départage sur la
        // forme brute pour que le tri soit total, donc reproductible.
        .then_with(|| a.cmp(b))
}

/// Découpe en morceaux comparables, après repli des accents et passage en
/// minuscules (règles 1 et 3).
fn decouper(s: &str) -> Vec<Morceau> {
    let plie = plier(s);
    let mut morceaux = Vec::new();
    let mut courant = String::new();
    let mut dans_les_chiffres = false;

    for c in plie.chars() {
        let chiffre = c.is_ascii_digit();
        if !courant.is_empty() && chiffre != dans_les_chiffres {
            morceaux.push(fabriquer(std::mem::take(&mut courant), dans_les_chiffres));
        }
        dans_les_chiffres = chiffre;
        courant.push(c);
    }
    if !courant.is_empty() {
        morceaux.push(fabriquer(courant, dans_les_chiffres));
    }
    morceaux
}

fn fabriquer(morceau: String, chiffres: bool) -> Morceau {
    if chiffres {
        let sans_zeros = morceau.trim_start_matches('0');
        Morceau::Nombre(if sans_zeros.is_empty() {
            "0".to_string()
        } else {
            sans_zeros.to_string()
        })
    } else {
        Morceau::Texte(morceau)
    }
}

/// Minuscules + retrait des diacritiques, pour que « Éric » se range avec
/// « Eric ». La décomposition NFD suivie du retrait des marques combinantes est
/// la même mécanique que `library::smart_collections` emploie pour ses `LIKE`.
fn plier(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    s.to_lowercase()
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri(sort: &str, order: &str) -> TriFavoris {
        TriFavoris::depuis(Some(sort), Some(order)).unwrap()
    }

    #[test]
    fn sans_parametre_aucun_tri() {
        assert!(TriFavoris::depuis(None, None).is_none());
        assert!(TriFavoris::depuis(Some(""), None).is_none());
        assert!(TriFavoris::depuis(Some("  "), Some("desc")).is_none());
    }

    #[test]
    fn une_cle_inconnue_ne_trie_pas_et_ne_panique_pas() {
        assert!(TriFavoris::depuis(Some("annee"), None).is_none());
        assert!(TriFavoris::depuis(Some("DROP TABLE favorites"), None).is_none());
    }

    #[test]
    fn les_cles_se_lisent_dans_les_deux_langues() {
        assert_eq!(tri("title", "asc").cle, CleDeTri::Titre);
        assert_eq!(tri("titre", "asc").cle, CleDeTri::Titre);
        assert_eq!(tri("ARTIST", "asc").cle, CleDeTri::Artiste);
        assert_eq!(tri("album", "asc").cle, CleDeTri::Album);
        assert_eq!(tri("added", "asc").cle, CleDeTri::Ajout);
        assert_eq!(tri("manual", "asc").cle, CleDeTri::Manuel);
        assert_eq!(tri("manuel", "asc").cle, CleDeTri::Manuel);
        assert_eq!(tri("CUSTOM", "asc").cle, CleDeTri::Manuel);
        assert_eq!(tri("title", "desc").sens, Sens::Decroissant);
        // Un `order` absent ou farfelu vaut croissant.
        assert_eq!(tri("title", "n'importe quoi").sens, Sens::Croissant);
        assert_eq!(
            TriFavoris::depuis(Some("title"), None).unwrap().sens,
            Sens::Croissant
        );
    }

    fn ranger(mut v: Vec<&str>, sens: Sens) -> Vec<&str> {
        trier_par(&mut v, sens, |s| Some((*s).to_string()));
        v
    }

    #[test]
    fn les_accents_se_rangent_avec_leur_lettre() {
        assert_eq!(
            ranger(vec!["Zoé", "Erik", "Éric", "Eric"], Sens::Croissant),
            vec!["Eric", "Éric", "Erik", "Zoé"]
        );
    }

    #[test]
    fn volume_2_precede_volume_10() {
        assert_eq!(
            ranger(
                vec!["Volume 10", "Volume 2", "Volume 1", "Volume 20"],
                Sens::Croissant
            ),
            vec!["Volume 1", "Volume 2", "Volume 10", "Volume 20"]
        );
    }

    #[test]
    fn les_zeros_de_tete_ne_changent_pas_le_rang() {
        assert_eq!(
            ranger(vec!["Track 09", "Track 8", "Track 010"], Sens::Croissant),
            vec!["Track 8", "Track 09", "Track 010"]
        );
    }

    #[test]
    fn un_champ_absent_finit_la_liste_dans_les_deux_sens() {
        let mut v = vec![Some("B"), None, Some("A"), Some("  ")];
        trier_par(&mut v, Sens::Croissant, |o| o.map(str::to_string));
        assert_eq!(&v[..2], &[Some("A"), Some("B")]);
        assert!(v[2].is_none() || v[2] == Some("  "));
        assert!(v[3].is_none() || v[3] == Some("  "));

        let mut v = vec![Some("B"), None, Some("A"), Some("  ")];
        trier_par(&mut v, Sens::Decroissant, |o| o.map(str::to_string));
        assert_eq!(&v[..2], &[Some("B"), Some("A")]);
        assert!(v[2].is_none() || v[2] == Some("  "));
        assert!(v[3].is_none() || v[3] == Some("  "));
    }

    #[test]
    fn la_casse_ne_compte_pas() {
        assert_eq!(
            ranger(vec!["banjo", "Alto", "ZITHER", "cello"], Sens::Croissant),
            vec!["Alto", "banjo", "cello", "ZITHER"]
        );
    }

    #[test]
    fn un_tri_egal_garde_l_ordre_d_arrivee() {
        // Stabilité : trois titres identiques restent dans l'ordre de la
        // requête (`created_at DESC`), le départage naturel des favoris.
        let mut v = vec![("Live", 3), ("Live", 2), ("Live", 1)];
        trier_par(&mut v, Sens::Croissant, |(t, _)| Some((*t).to_string()));
        assert_eq!(v, vec![("Live", 3), ("Live", 2), ("Live", 1)]);
    }

    #[test]
    fn ajout_croissant_remonte_le_plus_ancien() {
        // La requête rend `created_at DESC` : le plus récent d'abord.
        let mut v = vec!["recent", "moyen", "ancien"];
        appliquer_ajout(&mut v, Sens::Croissant);
        assert_eq!(v, vec!["ancien", "moyen", "recent"]);

        let mut v = vec!["recent", "moyen", "ancien"];
        appliquer_ajout(&mut v, Sens::Decroissant);
        assert_eq!(v, vec!["recent", "moyen", "ancien"]);
    }

    fn ranger_rangs(v: Vec<(&str, Option<i64>)>, sens: Sens) -> Vec<&str> {
        let mut v = v;
        trier_par_rang(&mut v, sens, |(_, r)| *r);
        v.into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn le_rang_manuel_se_compare_en_nombre_pas_en_texte() {
        // Le miroir PostgreSQL stocke `position` en TEXT : un ORDER BY SQL y
        // mettrait « 10 » avant « 2 ». Comparé en i64, l'ordre est le meme sur
        // les deux moteurs.
        assert_eq!(
            ranger_rangs(
                vec![("dix", Some(10)), ("deux", Some(2)), ("un", Some(1))],
                Sens::Croissant
            ),
            vec!["un", "deux", "dix"]
        );
    }

    #[test]
    fn un_favori_sans_rang_manuel_finit_la_liste_dans_les_deux_sens() {
        // Regle 2, transposee au rang : un favori ajoute apres le dernier
        // reordonnancement n'a pas de rang. Il va EN FIN, meme en descendant —
        // sinon descendre l'ordre manuel remonterait les nouveaux venus en tete.
        assert_eq!(
            ranger_rangs(
                vec![("b", Some(2)), ("neuf", None), ("a", Some(1))],
                Sens::Croissant
            ),
            vec!["a", "b", "neuf"]
        );
        assert_eq!(
            ranger_rangs(
                vec![("b", Some(2)), ("neuf", None), ("a", Some(1))],
                Sens::Decroissant
            ),
            vec!["b", "a", "neuf"]
        );
    }

    #[test]
    fn deux_favoris_sans_rang_gardent_l_ordre_d_arrivee() {
        // Stabilite : les sans-rang restent departages par `created_at DESC`.
        assert_eq!(
            ranger_rangs(
                vec![("recent", None), ("ancien", None), ("range", Some(1))],
                Sens::Croissant
            ),
            vec!["range", "recent", "ancien"]
        );
    }

    /// Une reponse de service (Qobuz/Tidal) n'a pas de rang manuel : le tri
    /// manuel ne doit rien y bouger, et surtout pas paniquer.
    #[test]
    fn le_tri_manuel_laisse_une_liste_de_service_intacte() {
        let mut donnees = serde_json::json!({
            "albums": [{"title": "C"}, {"title": "A"}, {"title": "B"}]
        });
        let avant = donnees.clone();
        trier_liste_json(&mut donnees, tri("manual", "asc"));
        assert_eq!(donnees, avant);
        trier_liste_json(&mut donnees, tri("manual", "desc"));
        assert_eq!(donnees, avant);
    }

    #[test]
    fn un_nombre_passe_avant_une_lettre() {
        assert_eq!(
            ranger(vec!["ABBA", "2 Unlimited"], Sens::Croissant),
            vec!["2 Unlimited", "ABBA"]
        );
    }
}
