//! Le CATALOGUE d'un service, quand une règle le demande — #4473.
//!
//! Bertrand, 19/09/2026 : *« Serait-il possible d'étendre les source = Qobuz à
//! l'intégralité du catalogue ? »*
//!
//! ## Ce qu'un service sait répondre, et ce qu'il ne sait pas
//!
//! Le contrat d'un connecteur (`tune_core::streaming::StreamingService`) offre
//! `search`, `get_artist_albums`, `get_artist_top_tracks`, `get_album_tracks`.
//! **Aucun service n'énumère son catalogue** : on peut chercher, pas
//! interroger. Une règle « source = Qobuz ET année = 2025 » n'a donc aucune
//! requête derrière elle, chez aucun service.
//!
//! En revanche « tout Coltrane chez Qobuz » est exactement `get_artist_albums`.
//!
//! ## Deux valeurs, deux sens — et l'ancienne ne change pas
//!
//! * `source = qobuz` — les **favoris** du profil. Gratuit, hors ligne,
//!   déterministe. Inchangé.
//! * `source = catalogue:qobuz` — le **catalogue** du service, borné à ce
//!   qu'une règle `artist` ou `album` nomme (arbitrage de Bertrand, 19/09).
//!
//! ## 🔴 Sans cible, on REFUSE — on ne rend ni tout ni rien
//!
//! Une règle `catalogue:<service>` sans règle artiste ni album n'a aucune
//! requête possible. Elle doit être refusée **explicitement**. C'est la leçon
//! de #4469 : soixante-six combinaisons rendaient la bibliothèque entière parce
//! qu'une règle non traduite valait « vrai pour tout ».
//!
//! ## La frontière de crate reste fermée
//!
//! Ce module ne connaît aucun service : il déclare **ce dont il a besoin**, et
//! `tune-server` — qui tient le registre — le fournit. `SmartHttpState` porte
//! un `Option<Arc<dyn CatalogueDistant>>`, `None` en test et partout où le
//! registre n'existe pas.
use serde_json::Value;

/// Le préfixe qui distingue le catalogue des favoris dans la valeur d'une
/// règle `source`.
pub const PREFIXE_CATALOGUE: &str = "catalogue:";

/// Un album tel qu'un service le rend. Les champs que la carte d'album de
/// l'écran sait afficher, et rien de plus.
#[derive(Debug, Clone, Default)]
pub struct AlbumDistant {
    pub service: String,
    pub source_id: String,
    pub title: String,
    pub artist: String,
    pub cover_url: Option<String>,
    pub year: Option<i64>,
}

/// Une piste telle qu'un service la rend.
#[derive(Debug, Clone, Default)]
pub struct PisteDistante {
    pub service: String,
    pub source_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_url: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Ce que le module des règles a besoin de demander à un service.
///
/// Les implémentations rendent une liste **vide** quand le service est inconnu,
/// déconnecté ou muet : une panne de réseau ne doit pas faire échouer la
/// résolution d'une collection qui a par ailleurs des albums locaux.
#[async_trait::async_trait]
pub trait CatalogueDistant: Send + Sync {
    async fn albums_par_artiste(&self, service: &str, nom: &str) -> Vec<AlbumDistant>;
    async fn albums_par_titre(&self, service: &str, titre: &str) -> Vec<AlbumDistant>;
    async fn pistes_par_artiste(&self, service: &str, nom: &str) -> Vec<PisteDistante>;
    /// Les pistes de l'album de ce TITRE — le pendant de
    /// [`CatalogueDistant::albums_par_titre`] pour une playlist.
    async fn pistes_par_album(&self, service: &str, titre: &str) -> Vec<PisteDistante>;
}

/// Le type d'objet qu'une vue intelligente rend.
///
/// 🔴 Il change la LECTURE des règles, pas seulement l'affichage : le champ
/// `title` nomme le titre de l'ALBUM dans une collection et celui de la PISTE
/// dans une playlist (`regles_sql::colonne_piste` → `t.title`). Confondre les
/// deux ferait chercher un album nommé « Giant Steps » là où l'utilisateur
/// demandait une piste.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Objet {
    /// Une playlist intelligente : des pistes.
    Piste,
    /// Une collection intelligente : des albums.
    Album,
}

impl Objet {
    /// Les champs qui nomment un titre d'ALBUM, pour cet objet.
    fn champs_album(self) -> &'static [&'static str] {
        match self {
            // L'éditeur de COLLECTIONS nomme le titre d'album `title`
            // (`CHAMPS` de `smartRegles.ts`, libellé
            // `smartCollection.fieldAlbumTitle`), et `build_album_query` le
            // traduit par `al.title`.
            Objet::Album => &["album", "album_title", "title"],
            // L'éditeur de PLAYLISTS écrit `album` pour l'album et `title`
            // pour la piste : ici `title` n'est pas un album.
            Objet::Piste => &["album", "album_title"],
        }
    }

    /// Les champs qui nomment un titre de PISTE — jamais une cible : aucun
    /// service ne cherche « la piste intitulée X » dans tout son catalogue.
    /// Ils servent à TRIER ce que le service rend, comme le titre d'album trie
    /// la discographie d'un artiste.
    fn champs_piste(self) -> &'static [&'static str] {
        match self {
            Objet::Piste => &["title", "track_title"],
            Objet::Album => &[],
        }
    }

    /// Tout ce que ce chemin sait honorer à côté d'un catalogue.
    fn champs_honorables(self) -> Vec<&'static str> {
        let mut v = CHAMPS_ARTISTE.to_vec();
        v.extend_from_slice(self.champs_album());
        v.extend_from_slice(self.champs_piste());
        v
    }
}

/// Ce qu'une règle `catalogue:<service>` peut faire chercher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cible {
    Artiste(String),
    Album(String),
}

fn valeur(regle: &Value) -> String {
    regle
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn champ(regle: &Value) -> String {
    regle
        .get("field")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// Le service nommé par une règle `source = catalogue:<service>`, s'il y en a.
///
/// Comparaison en minuscules : l'éditeur écrit `catalogue:qobuz`, un semis
/// pourrait écrire `Catalogue:Qobuz`.
pub fn service_du_catalogue(rules_json: &str) -> Option<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    rules.iter().find_map(|r| {
        if champ(r) != "source" {
            return None;
        }
        let v = valeur(r).to_lowercase();
        let nom = v.strip_prefix(PREFIXE_CATALOGUE)?.trim().to_string();
        (!nom.is_empty()).then_some(nom)
    })
}

/// L'artiste ou l'album que les règles nomment — la seule chose qu'un service
/// sache chercher.
///
/// L'artiste prime : « catalogue Qobuz, artiste Coltrane, album Blue Train »
/// se lit comme « chez cet artiste », et `get_artist_albums` répond mieux
/// qu'une recherche par titre.
///
/// Seule l'égalité compte. « Artiste **contient** Col » n'est pas une requête
/// qu'un service sait honorer : il chercherait « Col » et rendrait autre chose.
///
/// 🔴 `title` se lit selon l'OBJET, et c'est la raison d'être du paramètre.
/// Une collection intelligente porte sur des albums : son éditeur écrit
/// `{field:"title"}` pour le titre de l'album (`CHAMPS` de `smartRegles.ts`,
/// libellé `smartCollection.fieldAlbumTitle`), et `build_album_query` le
/// traduit par `al.title` (`"album" | "album_title" | "title"`). Une playlist
/// intelligente, elle, porte sur des pistes : `regles_sql::colonne_piste`
/// traduit `title` par `t.title`, le titre de la PISTE. Lire le même champ des
/// deux façons ferait chercher un ALBUM « Giant Steps » là où l'utilisateur
/// demandait une piste. Voir [`Objet`].
pub fn cible(rules_json: &str, objet: Objet) -> Option<Cible> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    if let Some(a) = nomme(&rules, CHAMPS_ARTISTE) {
        return Some(Cible::Artiste(a));
    }
    nomme(&rules, objet.champs_album()).map(Cible::Album)
}

/// La première valeur non vide d'une ÉGALITÉ sur l'un de ces champs.
fn nomme(rules: &[Value], quoi: &[&str]) -> Option<String> {
    rules.iter().find_map(|r| {
        let c = champ(r);
        if !quoi.contains(&c.as_str()) || !est_egalite(r) {
            return None;
        }
        let v = valeur(r);
        (!v.is_empty()).then_some(v)
    })
}

/// Les champs qui nomment un ARTISTE, pour le catalogue.
const CHAMPS_ARTISTE: &[&str] = &["artist", "artist_name"];

fn est_egalite(r: &Value) -> bool {
    matches!(
        crate::regles_sql::normaliser_op(crate::regles_sql::lire_op(r)),
        "="
    )
}

/// Les règles qu'un service ne sait PAS honorer à côté d'un catalogue — #4473.
///
/// Le service répond à « les albums de cet artiste » ou « l'album de ce
/// titre », rien d'autre. Une règle de format, de fréquence, d'année, de
/// nombre d'écoutes, de dossier ou de favori n'a aucun sens à distance : les
/// albums rendus par le service ne la respecteraient pas, et la collection
/// afficherait sous « FLAC 24 bits » des albums qui ne le sont pas. Même chose
/// pour un artiste ou un titre qui n'est pas une ÉGALITÉ (« contient Col »).
///
/// Rend la liste lisible de ces règles (`champ opérateur`), vide quand tout
/// est honorable. L'appelant REFUSE si elle ne l'est pas — l'arbitrage de
/// l'issue, et la leçon de #4469 : une règle non traduite ne vaut jamais
/// « vrai pour tout ».
pub fn regles_hors_service(rules_json: &str, objet: Objet) -> Vec<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let honorables = objet.champs_honorables();
    rules
        .iter()
        .filter(|r| {
            let c = champ(r);
            if c == "source" && valeur(r).to_lowercase().starts_with(PREFIXE_CATALOGUE) {
                return false;
            }
            !(honorables.contains(&c.as_str()) && est_egalite(r))
        })
        .map(|r| {
            let op = crate::regles_sql::lire_op(r);
            format!("{} {op}", champ(r))
        })
        .collect()
}

/// Le titre d'album qu'une règle nomme À CÔTÉ d'un artiste.
///
/// « catalogue Qobuz, artiste Coltrane, album Blue Train » : la recherche part
/// de l'artiste ([`cible`]), et ce titre-ci doit encore TRIER ce que le
/// service rend — sinon la collection montrerait toute la discographie.
pub fn titre_exige(rules_json: &str, objet: Objet) -> Option<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    nomme(&rules, objet.champs_album())
}

/// Le titre de PISTE qu'une règle nomme — playlists seulement.
///
/// Aucun service ne cherche « la piste intitulée X » dans tout son catalogue :
/// ce titre ne peut donc pas être une [`Cible`]. Il TRIE ce que le service a
/// rendu pour l'artiste ou l'album, exactement comme [`titre_exige`] trie une
/// discographie. Sans ce filtre, « catalogue Qobuz + Coltrane + titre = Giant
/// Steps » afficherait toute la sélection de l'artiste.
pub fn titre_de_piste_exige(rules_json: &str, objet: Objet) -> Option<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    nomme(&rules, objet.champs_piste())
}

/// Ce qu'une règle « catalogue » demande, une fois validée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demande {
    /// Le service nommé, en minuscules.
    pub service: String,
    /// Ce qu'on va chercher chez lui.
    pub cible: Cible,
    /// Un titre d'ALBUM nommé à côté d'un artiste : il trie ce que le service
    /// rend (voir [`titre_exige`]).
    pub titre_album: Option<String>,
    /// Un titre de PISTE nommé à côté (playlists seulement, voir
    /// [`titre_de_piste_exige`]).
    pub titre_piste: Option<String>,
}

/// Ce que les règles disent du catalogue — l'UNIQUE lecture, partagée par les
/// collections et les playlists.
///
/// 🔴 Deux mises en œuvre de la même règle finissent toujours par diverger :
/// c'est ce qui s'est produit entre le chemin des ALBUMS, qui savait aller au
/// catalogue depuis la v0.9.158, et celui des PISTES, qui traduisait encore
/// `catalogue:qobuz` en `t.source = 'catalogue:qobuz'` et rendait zéro piste
/// (#4473, second volet). Le service nommé, la cible, et les trois refus sont
/// décidés ICI ; l'appelant ne fait plus que l'aller-retour propre à son objet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lecture {
    /// Aucune règle ne demande de catalogue : l'appelant ne change rien.
    Aucune,
    /// La demande n'a pas de requête possible. Le message est destiné à
    /// l'utilisateur, et NOMME ce qui bloque — jamais un vide silencieux.
    Refus(String),
    /// Ce qu'il y a à demander au service.
    Demande(Demande),
}

/// Lit les règles pour cet objet. Voir [`Lecture`].
pub fn lire(rules_json: &str, objet: Objet) -> Lecture {
    let Some(service) = service_du_catalogue(rules_json) else {
        return Lecture::Aucune;
    };
    let Some(cible) = cible(rules_json, objet) else {
        return Lecture::Refus(
            "Une règle « catalogue » doit nommer un artiste ou un album : \
             aucun service ne sait énumérer son catalogue."
                .to_string(),
        );
    };
    // Ce que le service ne saurait pas filtrer : refusé, et nommé (#4473).
    let hors_service = regles_hors_service(rules_json, objet);
    if !hors_service.is_empty() {
        return Lecture::Refus(format!(
            "Le catalogue d'un service ne sait chercher qu'un artiste ou un album \
             (égalité). Ces règles ne peuvent pas s'y appliquer : {}.",
            hors_service.join(", ")
        ));
    }
    Lecture::Demande(Demande {
        service,
        titre_album: titre_exige(rules_json, objet),
        titre_piste: titre_de_piste_exige(rules_json, objet),
        cible,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAT: &str = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                          {"field":"artist","op":"equals","value":"John Coltrane"}]"#;

    #[test]
    fn le_catalogue_se_distingue_des_favoris() {
        assert_eq!(service_du_catalogue(CAT).as_deref(), Some("qobuz"));
        // 🔴 L'ancienne valeur ne change pas de sens : « qobuz » tout court
        // reste les FAVORIS, et ne doit surtout pas devenir le catalogue.
        assert!(service_du_catalogue(r#"[{"field":"source","op":"=","value":"qobuz"}]"#).is_none());
        assert!(service_du_catalogue(r#"[{"field":"source","op":"=","value":"local"}]"#).is_none());
        assert!(service_du_catalogue("[]").is_none());
    }

    #[test]
    fn la_graphie_du_prefixe_ne_compte_pas() {
        for v in ["catalogue:qobuz", "Catalogue:Qobuz", "CATALOGUE:QOBUZ"] {
            let r = format!(r#"[{{"field":"source","op":"=","value":"{v}"}}]"#);
            assert_eq!(service_du_catalogue(&r).as_deref(), Some("qobuz"), "{v}");
        }
    }

    #[test]
    fn un_prefixe_sans_service_ne_compte_pas() {
        assert!(
            service_du_catalogue(r#"[{"field":"source","op":"=","value":"catalogue:"}]"#).is_none()
        );
        assert!(
            service_du_catalogue(r#"[{"field":"source","op":"=","value":"catalogue:   "}]"#)
                .is_none()
        );
    }

    #[test]
    fn l_artiste_est_la_cible_et_prime_sur_l_album() {
        assert_eq!(
            cible(CAT, Objet::Album),
            Some(Cible::Artiste("John Coltrane".into())),
            "l'artiste doit primer"
        );
        let deux = r#"[{"field":"artist","op":"=","value":"Coltrane"},
                       {"field":"album","op":"=","value":"Blue Train"}]"#;
        assert_eq!(
            cible(deux, Objet::Album),
            Some(Cible::Artiste("Coltrane".into()))
        );
        let seul_album = r#"[{"field":"album","op":"equals","value":"Blue Train"}]"#;
        assert_eq!(
            cible(seul_album, Objet::Album),
            Some(Cible::Album("Blue Train".into()))
        );
    }

    /// 🔴 Le champ que l'ÉCRAN des collections écrit réellement.
    ///
    /// `CHAMPS` de `smartRegles.ts` nomme le titre d'album `title` (libellé
    /// `smartCollection.fieldAlbumTitle`), pas `album`. Une collection
    /// « catalogue Qobuz + titre = Blue Train » partait donc sans cible, et le
    /// serveur la refusait après que l'écran l'avait acceptée.
    #[test]
    fn le_titre_d_une_collection_est_un_titre_d_album() {
        for champ in ["album", "album_title", "title"] {
            let r = format!(r#"[{{"field":"{champ}","op":"=","value":"Blue Train"}}]"#);
            assert_eq!(
                cible(&r, Objet::Album),
                Some(Cible::Album("Blue Train".into())),
                "{champ}"
            );
        }
    }

    #[test]
    fn les_deux_graphies_du_champ_artiste_sont_accueillies() {
        for champ in ["artist", "artist_name"] {
            let r = format!(r#"[{{"field":"{champ}","op":"=","value":"Coltrane"}}]"#);
            assert_eq!(
                cible(&r, Objet::Album),
                Some(Cible::Artiste("Coltrane".into())),
                "{champ}"
            );
        }
    }

    #[test]
    fn seule_l_egalite_est_une_cible() {
        // « artiste CONTIENT Col » n'est pas une requête qu'un service sait
        // honorer : il chercherait « Col » et rendrait autre chose. On refuse
        // plutôt que de deviner.
        for op in ["contains", "starts_with", "not_equals", "is_empty"] {
            let r = format!(r#"[{{"field":"artist","op":"{op}","value":"Coltrane"}}]"#);
            assert_eq!(cible(&r, Objet::Album), None, "opérateur {op}");
        }
    }

    /// 🔴 #4473 — ce que le service ne sait pas filtrer est NOMMÉ, pour être
    /// refusé : « catalogue Qobuz + Coltrane + FLAC » rendrait sinon des
    /// albums Qobuz sous une règle de format qu'ils ne respectent pas.
    #[test]
    fn les_regles_qu_un_service_ne_sait_pas_honorer_sont_nommees() {
        let honorable = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                            {"field":"artist","op":"=","value":"John Coltrane"},
                            {"field":"title","op":"equals","value":"Blue Train"}]"#;
        assert!(regles_hors_service(honorable, Objet::Album).is_empty());

        let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                    {"field":"artist","op":"=","value":"John Coltrane"},
                    {"field":"format","op":"=","value":"FLAC"},
                    {"field":"play_count","op":">=","value":"3"},
                    {"field":"album","op":"contains","value":"Blue"}]"#;
        assert_eq!(
            regles_hors_service(r, Objet::Album),
            vec!["format =", "play_count >=", "album contains"]
        );
        // Une seconde règle de source (« local ») n'a pas de sens à distance.
        let deux = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                       {"field":"source","op":"=","value":"local"}]"#;
        assert_eq!(regles_hors_service(deux, Objet::Album), vec!["source ="]);
    }

    #[test]
    fn le_titre_exige_a_cote_d_un_artiste() {
        let r = r#"[{"field":"artist","op":"=","value":"Coltrane"},
                    {"field":"album","op":"=","value":"Blue Train"}]"#;
        assert_eq!(titre_exige(r, Objet::Album).as_deref(), Some("Blue Train"));
        assert_eq!(
            titre_exige(r#"[{"field":"artist","op":"=","value":"C"}]"#, Objet::Album),
            None
        );
    }

    #[test]
    fn sans_cible_il_n_y_a_rien_a_chercher() {
        // C'est le cas qui doit REFUSER en amont : « catalogue Qobuz ET année
        // 2025 » n'a aucune requête possible.
        let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                    {"field":"year","op":"=","value":"2025"}]"#;
        assert_eq!(service_du_catalogue(r).as_deref(), Some("qobuz"));
        assert_eq!(cible(r, Objet::Album), None);
    }

    /// 🔴 #4473, second volet — `title` ne veut PAS dire la même chose des
    /// deux côtés, et la v0.9.159 le disait déjà en commentaire sans pouvoir
    /// l'appliquer : une collection l'entend comme le titre de l'ALBUM, une
    /// playlist comme celui de la PISTE (`colonne_piste` → `t.title`).
    #[test]
    fn le_titre_se_lit_selon_l_objet() {
        let r = r#"[{"field":"title","op":"=","value":"Giant Steps"}]"#;
        assert_eq!(
            cible(r, Objet::Album),
            Some(Cible::Album("Giant Steps".into())),
            "une collection cherche l'ALBUM de ce titre"
        );
        assert_eq!(
            cible(r, Objet::Piste),
            None,
            "une playlist ne peut pas chercher une PISTE par son titre : \
             aucun service n'énumère son catalogue"
        );
        // Et le titre de piste reste HONORABLE : il trie ce que le service
        // rend, il n'est simplement pas une cible.
        assert!(regles_hors_service(r, Objet::Piste).is_empty());
        assert_eq!(
            titre_de_piste_exige(r, Objet::Piste).as_deref(),
            Some("Giant Steps")
        );
        assert_eq!(titre_de_piste_exige(r, Objet::Album), None);
    }

    /// Le cas de FabienM, lu comme une PLAYLIST — `Test Qobuz Coltrane`.
    #[test]
    fn la_playlist_de_fabienm_est_une_demande_valide() {
        let Lecture::Demande(d) = lire(CAT, Objet::Piste) else {
            panic!("la playlist de FabienM doit être honorée");
        };
        assert_eq!(d.service, "qobuz");
        assert_eq!(d.cible, Cible::Artiste("John Coltrane".into()));
        assert_eq!(d.titre_album, None);
        assert_eq!(d.titre_piste, None);
    }

    #[test]
    fn la_lecture_est_la_meme_des_deux_cotes() {
        // Aucune règle de catalogue : rien à faire, pour l'un comme pour
        // l'autre.
        for o in [Objet::Album, Objet::Piste] {
            assert_eq!(
                lire(r#"[{"field":"source","op":"=","value":"qobuz"}]"#, o),
                Lecture::Aucune,
                "{o:?}"
            );
            // Sans cible : le MÊME refus, mot pour mot.
            let sans = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                           {"field":"year","op":"=","value":"2025"}]"#;
            let Lecture::Refus(m) = lire(sans, o) else {
                panic!("{o:?} : sans cible, il faut refuser");
            };
            assert!(m.contains("artiste ou un album"), "{o:?} : {m}");
            // Une règle que le service ne sait pas filtrer est NOMMÉE.
            let hors = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                           {"field":"artist","op":"=","value":"Coltrane"},
                           {"field":"format","op":"=","value":"FLAC"}]"#;
            let Lecture::Refus(m) = lire(hors, o) else {
                panic!("{o:?} : une règle de format doit être refusée");
            };
            assert!(
                m.contains("format ="),
                "{o:?} : la règle doit être NOMMÉE — {m}"
            );
        }
    }

    /// « catalogue Qobuz + Coltrane + album Blue Train » dans une playlist :
    /// l'album trie la sélection, le titre de piste aussi s'il est là.
    #[test]
    fn une_playlist_retient_les_deux_titres() {
        let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                    {"field":"artist","op":"=","value":"Coltrane"},
                    {"field":"album","op":"=","value":"Blue Train"},
                    {"field":"title","op":"=","value":"Moment's Notice"}]"#;
        let Lecture::Demande(d) = lire(r, Objet::Piste) else {
            panic!("demande valide");
        };
        assert_eq!(d.cible, Cible::Artiste("Coltrane".into()));
        assert_eq!(d.titre_album.as_deref(), Some("Blue Train"));
        assert_eq!(d.titre_piste.as_deref(), Some("Moment's Notice"));
    }
}
