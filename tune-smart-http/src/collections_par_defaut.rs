//! Les collections intelligentes livrées portent une CLÉ stable, pas seulement
//! un nom français.
//!
//! Le semis écrit les seize collections par défaut en français, dans deux
//! migrations — `tune-core/src/db/migrations.rs:546` (`seed_default_smart_`
//! `collections`) et `:614` (`reseed_smart_collections`) : « 🖼️ Sans pochette »,
//! « 🆕 Récents », « 🎻 Classique », « 🎬 Bandes Originales ». Les tuiles
//! voisines — Jazz, Rock, Pop, Piano, Audiophile, Soul & Funk, SACD / DSD,
//! World Music — passent inaperçues parce que leur nom est déjà neutre ; les
//! quatre autres sautent aux yeux dans une interface roumaine (v0.9.161).
//!
//! La colonne `name` reste la source de vérité en base et sur le fil : elle ne
//! bouge pas, `docs/contrat-web.json` la cite et le client publié continue de
//! l'afficher. Ce qui est AJOUTÉ, ce sont deux champs facultatifs :
//!
//! * `name_key` — `smartCollection.default.recent`, `…noCover`, `…classical`…
//! * `description_key` — la même clé suffixée `.description`.
//!
//! Le client les rend dans SA langue (il embarque les dix catalogues) et
//! retombe sur `name` quand la clé manque — exactement le geste qu'il fait
//! déjà pour les rubriques Qobuz (`SECTION_KEYS`, `TAG_KEYS`) et pour les
//! genres de podcasts (`src/lib/podcast-genres.ts`).
//!
//! **Aucune migration, et rien de renommé.** La reconnaissance se fait à
//! l'affichage, sur la valeur lue en base, et seulement si elle est encore MOT
//! POUR MOT celle du semis. Une collection que l'utilisateur a renommée ne
//! ressemble plus à aucune entrée de la table : elle ne reçoit pas de clé, et
//! son nom à lui est rendu intact. C'est la garantie « ne pas renommer ce que
//! l'utilisateur a renommé », obtenue sans écrire une ligne en base.

/// Le nom d'une collection, débarrassé de son emoji de tête et replié en
/// minuscules — la forme sous laquelle il est comparé au semis.
///
/// Les noms semés commencent tous par un emoji (`🎻 Classique`), stocké en
/// double dans la colonne `icon`. Le retirer rend la reconnaissance insensible
/// à un emoji perdu à l'export/import, sans jamais rendre deux noms différents
/// identiques : aucune paire du semis ne se distingue par son seul emoji.
fn noyau(nom: &str) -> String {
    nom.trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_lowercase()
}

/// La clé de traduction d'une collection livrée, ou rien si ce nom n'est pas
/// (ou n'est plus) celui d'une collection du semis.
pub fn cle_du_nom(nom: &str) -> Option<&'static str> {
    let cle = match noyau(nom).as_str() {
        "audiophile" => "smartCollection.default.audiophile",
        "bandes originales" => "smartCollection.default.soundtracks",
        "classique" => "smartCollection.default.classical",
        "electro & ambient" => "smartCollection.default.electroAmbient",
        "french touch" => "smartCollection.default.frenchTouch",
        "jazz" => "smartCollection.default.jazz",
        "rock" => "smartCollection.default.rock",
        "sacd / dsd" => "smartCollection.default.sacdDsd",
        "soul & funk" => "smartCollection.default.soulFunk",
        "récents" => "smartCollection.default.recent",
        "sans pochette" => "smartCollection.default.noCover",
        "piano" => "smartCollection.default.piano",
        "vocal / a cappella" => "smartCollection.default.vocalACappella",
        "blues" => "smartCollection.default.blues",
        "world music" => "smartCollection.default.world",
        "pop" => "smartCollection.default.pop",
        _ => return None,
    };
    Some(cle)
}

/// La description semée qui accompagne une clé de nom.
///
/// Elle sert d'ultime vérification : `description_key` n'est posée que si la
/// description lue en base est encore celle du semis. Un utilisateur qui a
/// gardé le nom mais réécrit la description garde SON texte, sans clé.
fn description_semee(cle_nom: &str) -> Option<&'static str> {
    let texte = match cle_nom {
        "smartCollection.default.audiophile" => "Enregistrements haute résolution",
        "smartCollection.default.soundtracks" => "Bandes originales de films",
        "smartCollection.default.classical" => "Musique classique et orchestrale",
        "smartCollection.default.electroAmbient" => "Électronique et ambient",
        "smartCollection.default.frenchTouch" => "Chanson française",
        "smartCollection.default.jazz" => "Tous les albums de jazz",
        "smartCollection.default.rock" => "Rock, alt-rock, prog-rock",
        "smartCollection.default.sacdDsd" => "Super Audio CD et DSD",
        "smartCollection.default.soulFunk" => "Soul, Funk, R&B",
        "smartCollection.default.recent" => "Ajoutés dans les 90 derniers jours",
        "smartCollection.default.noCover" => "Albums sans couverture",
        "smartCollection.default.piano" => "Piano solo et concertos",
        "smartCollection.default.vocalACappella" => "Musique vocale et a cappella",
        "smartCollection.default.blues" => "Blues et blues-rock",
        "smartCollection.default.world" => "Musiques du monde et folk",
        "smartCollection.default.pop" => "Pop et synth-pop",
        _ => return None,
    };
    Some(texte)
}

/// Les deux clés à joindre à une collection : celle du nom, et celle de la
/// description lorsqu'elle est encore celle du semis.
pub fn cles(nom: &str, description: Option<&str>) -> (Option<&'static str>, Option<String>) {
    let Some(cle_nom) = cle_du_nom(nom) else {
        return (None, None);
    };
    let cle_desc = description
        .map(str::trim)
        .filter(|d| Some(*d) == description_semee(cle_nom))
        .map(|_| format!("{cle_nom}.description"));
    (Some(cle_nom), cle_desc)
}

#[cfg(test)]
mod tests {
    use super::{cle_du_nom, cles};

    #[test]
    fn les_seize_collections_semees_ont_toutes_une_cle() {
        // La liste EXACTE de `reseed_smart_collections`
        // (tune-core/src/db/migrations.rs:616-632).
        for nom in [
            "💎 Audiophile",
            "🎬 Bandes Originales",
            "🎻 Classique",
            "🎧 Electro & Ambient",
            "🇫🇷 French Touch",
            "🎷 Jazz",
            "🎸 Rock",
            "💿 SACD / DSD",
            "🕺 Soul & Funk",
            "🆕 Récents",
            "🖼️ Sans pochette",
            "🎹 Piano",
            "🎤 Vocal / A cappella",
            "🎵 Blues",
            "🌍 World Music",
            "🎺 Pop",
        ] {
            assert!(cle_du_nom(nom).is_some(), "collection semée « {nom} »");
        }
    }

    #[test]
    fn les_quatre_noms_francais_du_signalement_portent_leur_cle() {
        assert_eq!(
            cle_du_nom("🖼️ Sans pochette"),
            Some("smartCollection.default.noCover")
        );
        assert_eq!(
            cle_du_nom("🆕 Récents"),
            Some("smartCollection.default.recent")
        );
        assert_eq!(
            cle_du_nom("🎻 Classique"),
            Some("smartCollection.default.classical")
        );
        assert_eq!(
            cle_du_nom("🎬 Bandes Originales"),
            Some("smartCollection.default.soundtracks")
        );
    }

    #[test]
    fn chaque_collection_semee_a_sa_propre_cle() {
        // Contre-épreuve : une table qui rendrait la même clé pour tout le
        // monde passerait l'essai précédent.
        let cles: Vec<_> = [
            "💎 Audiophile",
            "🎬 Bandes Originales",
            "🎻 Classique",
            "🎧 Electro & Ambient",
            "🇫🇷 French Touch",
            "🎷 Jazz",
            "🎸 Rock",
            "💿 SACD / DSD",
            "🕺 Soul & Funk",
            "🆕 Récents",
            "🖼️ Sans pochette",
            "🎹 Piano",
            "🎤 Vocal / A cappella",
            "🎵 Blues",
            "🌍 World Music",
            "🎺 Pop",
        ]
        .iter()
        .map(|n| cle_du_nom(n).unwrap())
        .collect();
        let mut uniques = cles.clone();
        uniques.sort_unstable();
        uniques.dedup();
        assert_eq!(uniques.len(), 16, "seize clés distinctes");
    }

    #[test]
    fn une_collection_renommee_ne_recoit_aucune_cle() {
        // LE point de la fiche : on ne renomme rien, et on ne recolle pas non
        // plus une étiquette sur ce que l'utilisateur a rebaptisé.
        assert_eq!(cle_du_nom("🎻 Mes concertos"), None);
        assert_eq!(cle_du_nom("Récents chez moi"), None);
        assert_eq!(cle_du_nom(""), None);
    }

    #[test]
    fn la_description_reecrite_perd_sa_cle_mais_le_nom_garde_la_sienne() {
        let (nom, desc) = cles("🆕 Récents", Some("Ajoutés dans les 90 derniers jours"));
        assert_eq!(nom, Some("smartCollection.default.recent"));
        assert_eq!(
            desc.as_deref(),
            Some("smartCollection.default.recent.description")
        );

        let (nom, desc) = cles("🆕 Récents", Some("Ce que j'ai acheté ce trimestre"));
        assert_eq!(nom, Some("smartCollection.default.recent"));
        assert_eq!(
            desc, None,
            "la description de l'utilisateur reste la sienne"
        );
    }

    #[test]
    fn les_seize_descriptions_semees_sont_reconnues() {
        for (nom, description) in [
            ("💎 Audiophile", "Enregistrements haute résolution"),
            ("🎬 Bandes Originales", "Bandes originales de films"),
            ("🎻 Classique", "Musique classique et orchestrale"),
            ("🎧 Electro & Ambient", "Électronique et ambient"),
            ("🇫🇷 French Touch", "Chanson française"),
            ("🎷 Jazz", "Tous les albums de jazz"),
            ("🎸 Rock", "Rock, alt-rock, prog-rock"),
            ("💿 SACD / DSD", "Super Audio CD et DSD"),
            ("🕺 Soul & Funk", "Soul, Funk, R&B"),
            ("🆕 Récents", "Ajoutés dans les 90 derniers jours"),
            ("🖼️ Sans pochette", "Albums sans couverture"),
            ("🎹 Piano", "Piano solo et concertos"),
            ("🎤 Vocal / A cappella", "Musique vocale et a cappella"),
            ("🎵 Blues", "Blues et blues-rock"),
            ("🌍 World Music", "Musiques du monde et folk"),
            ("🎺 Pop", "Pop et synth-pop"),
        ] {
            let (_, desc) = cles(nom, Some(description));
            assert!(desc.is_some(), "description semée de « {nom} »");
        }
    }
}
