//! Le texte de présentation d'un album de service, ramené à du texte (#6480).
//!
//! # La demande
//!
//! cpadiou77, fil forum 2327 (18/10/2026) : « il est possible d'accéder aux
//! commentaires Qobuz sur certaines playlists mais les commentaires sur les
//! albums ne sont pas affichés ». Qobuz publie bien ce texte : `album/get`
//! porte `description` (une chaîne) et `description_language` — relevé par la
//! sonde `raw-keys` sur le .18 le 19/10/2026, pour Kind of Blue
//! (5099749522428) et son édition Legacy.
//!
//! # Pourquoi du texte, et pas du HTML
//!
//! La critique arrive en HTML (`<p>`, `<br>`, `<em>`, des liens, des entités).
//! L'afficher telle quelle obligerait chaque client — web, Android, iOS — à
//! interpréter du balisage venu d'un tiers, avec ce que cela ouvre (script,
//! gestionnaires d'événements, liens `javascript:`). On le ramène donc ICI à
//! du texte : paragraphes séparés par une ligne vide, entités décodées, toute
//! balise retirée. Le client n'a plus qu'à l'afficher comme du texte, échappé.

use std::sync::LazyLock;

use regex::Regex;

use super::traits::StreamAlbumDescription;

/// Les blocs dont le CONTENU n'est pas du texte lisible.
static BLOCS_MUETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<(script|style|noscript|template)\b[^>]*>.*?</(script|style|noscript|template)\s*>",
    )
    .expect("regex des blocs muets")
});
/// Les commentaires HTML.
static COMMENTAIRES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("regex des commentaires"));
/// Un saut de ligne simple.
static SAUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").expect("regex des sauts"));
/// Ce qui ferme (ou ouvre) un paragraphe.
static PARAGRAPHE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(p|div|h[1-6]|blockquote|ul|ol|table|tr)\b[^>]*>")
        .expect("regex des paragraphes")
});
/// Un élément de liste.
static ELEMENT_DE_LISTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<li\b[^>]*>").expect("regex des listes"));
/// Toute autre balise, ouvrante ou fermante.
static BALISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]*>").expect("regex des balises"));
/// Une entité nommée ou numérique.
static ENTITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});")
        .expect("regex des entités")
});
/// Plus de deux sauts d'affilée.
static SAUTS_EN_TROP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("regex des sauts en trop"));

/// Les entités nommées que l'on rencontre dans une critique — français,
/// anglais, allemand, italien, espagnol. Une entité inconnue reste écrite
/// telle quelle : mieux vaut `&foo;` lisible qu'un caractère inventé.
fn entite_nommee(nom: &str) -> Option<char> {
    Some(match nom {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "rsquo" => '’',
        "lsquo" => '‘',
        "rdquo" => '”',
        "ldquo" => '“',
        "laquo" => '«',
        "raquo" => '»',
        "hellip" => '…',
        "ndash" => '–',
        "mdash" => '—',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "eacute" => 'é',
        "egrave" => 'è',
        "ecirc" => 'ê',
        "euml" => 'ë',
        "Eacute" => 'É',
        "Egrave" => 'È',
        "Ecirc" => 'Ê',
        "aacute" => 'á',
        "agrave" => 'à',
        "acirc" => 'â',
        "auml" => 'ä',
        "Agrave" => 'À',
        "Auml" => 'Ä',
        "iacute" => 'í',
        "icirc" => 'î',
        "iuml" => 'ï',
        "oacute" => 'ó',
        "ograve" => 'ò',
        "ocirc" => 'ô',
        "ouml" => 'ö',
        "Ouml" => 'Ö',
        "uacute" => 'ú',
        "ugrave" => 'ù',
        "ucirc" => 'û',
        "uuml" => 'ü',
        "Uuml" => 'Ü',
        "ccedil" => 'ç',
        "Ccedil" => 'Ç',
        "ntilde" => 'ñ',
        "szlig" => 'ß',
        "oelig" => 'œ',
        "aelig" => 'æ',
        _ => return None,
    })
}

fn decoder_entites(texte: &str) -> String {
    ENTITE
        .replace_all(texte, |c: &regex::Captures| {
            let corps = &c[1];
            let car = if let Some(hex) = corps
                .strip_prefix("#x")
                .or_else(|| corps.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = corps.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                entite_nommee(corps)
            };
            match car {
                // Un caractère de contrôle n'a rien à faire dans un texte
                // affiché (hors saut de ligne et tabulation, rendus en espace
                // plus bas).
                Some(ch) if !ch.is_control() || ch == '\n' || ch == '\t' => ch.to_string(),
                _ => c[0].to_string(),
            }
        })
        .into_owned()
}

/// Le texte lisible d'une présentation HTML : paragraphes séparés par une
/// ligne vide, lignes sans espaces superflus, entités décodées, AUCUNE
/// balise. `None` quand il ne reste rien à lire.
pub fn texte_de_presentation(html: &str) -> Option<String> {
    let s = html.replace("\r\n", "\n").replace('\r', "\n");
    let s = BLOCS_MUETS.replace_all(&s, "");
    let s = COMMENTAIRES.replace_all(&s, "");
    // Dans le source HTML, un retour à la ligne n'est qu'un espace : seules
    // les balises disent où un paragraphe finit.
    let s = s.replace('\n', " ");
    let s = SAUT.replace_all(&s, "\n");
    let s = PARAGRAPHE.replace_all(&s, "\n\n");
    let s = ELEMENT_DE_LISTE.replace_all(&s, "\n• ");
    let s = BALISE.replace_all(&s, "");
    let s = decoder_entites(&s);

    let lignes: Vec<String> = s
        .split('\n')
        .map(|ligne| {
            ligne
                .split([' ', '\t'])
                .filter(|m| !m.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
                .trim_matches('\u{a0}')
                .trim()
                .to_string()
        })
        .collect();
    let texte = SAUTS_EN_TROP
        .replace_all(lignes.join("\n").trim(), "\n\n")
        .into_owned();
    let texte = texte.trim().to_string();
    (!texte.is_empty()).then_some(texte)
}

/// La présentation d'un album Qobuz, depuis la réponse de `album/get`.
pub fn presentation_qobuz(album: &serde_json::Value) -> Option<StreamAlbumDescription> {
    let description = texte_de_presentation(album["description"].as_str()?)?;
    let language = album["description_language"]
        .as_str()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    Some(StreamAlbumDescription {
        description,
        language,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn les_paragraphes_et_les_sauts_sont_gardes_les_balises_retirees() {
        let html = "<p>Enregistré en <strong>1959</strong>,<br/>un disque &laquo;&nbsp;modal&nbsp;&raquo;.</p>\n<p>Second <em>paragraphe</em>.</p>";
        assert_eq!(
            texte_de_presentation(html).as_deref(),
            Some("Enregistré en 1959,\nun disque «\u{a0}modal\u{a0}».\n\nSecond paragraphe.")
        );
    }

    #[test]
    fn script_style_gestionnaires_et_liens_ne_passent_pas() {
        let html = r#"<p onclick="alert(1)">Bon<script>alert('x')</script> texte</p><style>p{}</style><a href="javascript:alert(2)">lien</a><img src=x onerror=alert(3)>"#;
        let t = texte_de_presentation(html).unwrap();
        assert_eq!(t, "Bon texte\n\nlien");
        for interdit in ["<", ">", "script", "onclick", "onerror", "javascript"] {
            assert!(!t.contains(interdit), "{interdit:?} a passé : {t:?}");
        }
    }

    #[test]
    fn une_entite_qui_decrit_une_balise_reste_du_texte() {
        // `&lt;b&gt;` est le TEXTE « <b> » : décodé après le retrait des
        // balises, il ne redevient pas une balise retirée — et le client
        // l'affiche échappé.
        assert_eq!(
            texte_de_presentation("Le signe &lt;b&gt; et &amp;amp; &#233;t&#xE9;").as_deref(),
            Some("Le signe <b> et &amp; été")
        );
    }

    #[test]
    fn une_entite_inconnue_ou_un_controle_reste_ecrit() {
        assert_eq!(
            texte_de_presentation("a &inconnue; b &#0; c").as_deref(),
            Some("a &inconnue; b &#0; c")
        );
    }

    #[test]
    fn rien_a_lire_donne_none() {
        assert_eq!(texte_de_presentation(""), None);
        assert_eq!(texte_de_presentation("<p> </p><br><p>&nbsp;</p>"), None);
    }

    #[test]
    fn les_listes_et_les_lignes_vides_en_trop() {
        let html = "<p>Titres :</p><ul><li>So What</li><li>Blue in Green</li></ul><p></p><p></p><p>Fin</p>";
        assert_eq!(
            texte_de_presentation(html).as_deref(),
            Some("Titres :\n\n• So What\n• Blue in Green\n\nFin")
        );
    }

    #[test]
    fn la_reponse_qobuz_donne_texte_et_langue() {
        let album = json!({
            "id": "5099749522428",
            "description": "<p>Le disque de jazz le plus vendu.</p>",
            "description_language": "fr",
        });
        assert_eq!(
            presentation_qobuz(&album),
            Some(StreamAlbumDescription {
                description: "Le disque de jazz le plus vendu.".into(),
                language: Some("fr".into()),
            })
        );
    }

    #[test]
    fn sans_description_ou_description_vide_rien() {
        assert_eq!(presentation_qobuz(&json!({"id": "1"})), None);
        assert_eq!(presentation_qobuz(&json!({"description": ""})), None);
        assert_eq!(presentation_qobuz(&json!({"description": null})), None);
        let sans_langue =
            presentation_qobuz(&json!({"description": "x", "description_language": ""})).unwrap();
        assert_eq!(sans_langue.language, None);
    }
}
