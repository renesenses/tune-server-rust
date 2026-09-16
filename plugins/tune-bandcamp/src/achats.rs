//! Lot 3 — rendre à l'acheteur ce qu'il a payé : ses achats en FLAC.
//!
//! Yves, 16/09/2026 : « sur Bandcamp, les albums achetés, donc de sa
//! collection, ne sont pas joués à la résolution d'achat mais en mp3/128 ! »
//! Il a raison sur le constat, et la tête de `lib.rs` le dit depuis le lot 1 :
//! le `mp3-128` est la SEULE qualité que Bandcamp serve sans session d'achat.
//! Il n'existe pas de flux lossless chez Bandcamp — seulement des FICHIERS,
//! livrés par la page de téléchargement, derrière le cookie de session
//! `identity`.
//!
//! Ce module fait donc ce que fait un acheteur à la main, et rien d'autre :
//!
//!   1. `collection_items` **avec le cookie** rend, en plus des articles, le
//!      bloc `redownload_urls` — une URL de page de téléchargement par achat,
//!      indexée par `<sale_item_type><sale_item_id>` (`p123456`) ;
//!   2. cette page porte un `<div id="pagedata" data-blob="…">` : du JSON
//!      échappé en HTML, dont `digital_items[0].downloads.flac.url` ;
//!   3. Bandcamp exige un passage par `/statdownload/` avant de servir le
//!      fichier — la réponse est du JavaScript enveloppant un JSON qui peut
//!      porter une `download_url` fraîche ;
//!   4. le fichier arrive : un `.zip` pour un album, un `.flac` nu pour une
//!      piste. Il est posé dans le dossier de données du greffon, dézippé,
//!      et le dossier obtenu est rendu au client — qui le confie à
//!      l'assistant d'import (`/library/ingest`), lequel sait déjà placer,
//!      identifier et scanner. Ce greffon n'a pas de scanner, et n'en veut
//!      pas : un deuxième chemin d'entrée dans la bibliothèque serait un
//!      deuxième endroit où les règles de rangement divergeraient.
//!
//! Le cookie est un SECRET de session : il est écrit dans les réglages, jamais
//! rendu — `GET` ne dit que « présent » ou « absent ». Il n'est envoyé qu'à
//! `bandcamp.com` et à ses hôtes de téléchargement (`*.bcbits.com`,
//! `popplers*.bandcamp.com`), jamais ailleurs.
//!
//! ⚠️ Ce chemin n'a PAS pu être mesuré de bout en bout ici : il demande une
//! session d'acheteur, que seul le testeur possède. Le découpage des pages est
//! celui que les outils communautaires (bandcamp-collection-downloader) tiennent
//! depuis des années, et chaque étape de découpage est testée sur des
//! échantillons ; le journal nomme l'étape qui échoue, pour qu'un premier essai
//! chez Yves dise EXACTEMENT où la page a changé, s'il y a lieu.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;

/// Réglage qui porte le cookie `identity` de Bandcamp. Un secret : jamais lu
/// par une route, seulement « présent / absent ».
pub(crate) const CLE_IDENTITE: &str = "bandcamp_identity";

/// Nom de dossier des achats sous le dossier de données du greffon.
pub(crate) const DOSSIER_ACHATS: &str = "achats";

/// Le format qu'on va chercher. Bandcamp en propose d'autres (`wav`, `aiff`,
/// `alac`, `mp3-320`…) ; le FLAC est sans perte, compact et lu partout —
/// c'est le format de la bibliothèque, pas un choix à offrir.
pub(crate) const FORMAT: &str = "flac";

/// Nettoyer ce qu'un utilisateur colle : `identity=…`, guillemets, blancs.
///
/// Les navigateurs livrent le cookie sous des formes diverses selon d'où on le
/// copie (l'inspecteur donne la valeur nue, un `document.cookie` donne
/// `identity=…; autre=…`). On accepte tout ça et on garde la VALEUR.
pub(crate) fn nettoyer_identite(brut: &str) -> Option<String> {
    let mut s = brut.trim();
    if let Some(reste) = s.strip_prefix("Cookie:") {
        s = reste.trim();
    }
    // `document.cookie` : plusieurs paires ; on cherche la nôtre.
    if s.contains(';') || s.starts_with("identity=") {
        let paire = s
            .split(';')
            .map(str::trim)
            .find(|p| p.starts_with("identity="))?;
        s = &paire["identity=".len()..];
    }
    let s = s.trim().trim_matches('"').trim_matches('\'').trim();
    if s.is_empty() || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    Some(s.to_string())
}

/// L'en-tête `Cookie` à envoyer à Bandcamp.
pub(crate) fn en_tete_cookie(identite: &str) -> String {
    format!("identity={identite}")
}

/// La clé d'un achat dans `redownload_urls` : `<sale_item_type><sale_item_id>`.
///
/// Mesuré par les outils communautaires sur des réponses réelles ; les deux
/// champs sont sur l'article lui-même.
pub(crate) fn cle_d_achat(article: &Value) -> Option<String> {
    let t = article["sale_item_type"].as_str()?;
    let id = article["sale_item_id"].as_i64()?;
    Some(format!("{t}{id}"))
}

/// L'URL de page de téléchargement d'un article, si la collection en porte une.
pub(crate) fn url_de_retelechargement(brut: &Value, article: &Value) -> Option<String> {
    let cle = cle_d_achat(article)?;
    brut["redownload_urls"][cle.as_str()]
        .as_str()
        .map(str::to_string)
}

/// Décoder les cinq entités HTML qu'un attribut peut porter.
fn decoder_attribut_html(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Le JSON du `<div id="pagedata" data-blob="…">` d'une page Bandcamp.
pub(crate) fn blob_de_page(html: &str) -> Result<Value, String> {
    let i = html
        .find("id=\"pagedata\"")
        .ok_or("page sans bloc pagedata")?;
    let reste = &html[i..];
    let j = reste
        .find("data-blob=\"")
        .ok_or("pagedata sans data-blob")?;
    let reste = &reste[j + "data-blob=\"".len()..];
    let fin = reste.find('"').ok_or("data-blob non terminé")?;
    serde_json::from_str(&decoder_attribut_html(&reste[..fin]))
        .map_err(|e| format!("data-blob illisible : {e}"))
}

/// Ce que la page de téléchargement dit d'un achat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArticleTelechargeable {
    pub artiste: String,
    pub titre: String,
    /// `a` (album) ou `t` (piste) — c'est ce qui dit zip ou fichier nu.
    pub genre: String,
    pub url: String,
}

/// L'URL FLAC dans le blob d'une page de téléchargement.
pub(crate) fn article_du_blob(blob: &Value) -> Result<ArticleTelechargeable, String> {
    let item = blob["digital_items"]
        .as_array()
        .and_then(|v| v.first())
        .ok_or("blob sans digital_items")?;
    let dl = &item["downloads"][FORMAT];
    let url = dl["url"]
        .as_str()
        .ok_or_else(|| {
            let formats: Vec<&str> = item["downloads"]
                .as_object()
                .map(|m| m.keys().map(String::as_str).collect())
                .unwrap_or_default();
            format!("pas de {FORMAT} proposé (formats : {})", formats.join(", "))
        })?
        .to_string();
    Ok(ArticleTelechargeable {
        artiste: item["artist"].as_str().unwrap_or("").to_string(),
        titre: item["title"].as_str().unwrap_or("").to_string(),
        genre: item["type"]
            .as_str()
            .or(item["download_type"].as_str())
            .unwrap_or("a")
            .to_string(),
        url,
    })
}

/// L'URL `/statdownload/` à appeler avant le fichier.
pub(crate) fn url_statdownload(url: &str, alea: u64) -> String {
    format!(
        "{}&.vrs=1&.rand={alea}",
        url.replacen("/download/", "/statdownload/", 1)
    )
}

/// Le JSON dans la réponse `statdownload` :
/// `if ( window.Downloads ) { Downloads.statResult( {…} ) };`
///
/// Le JSON est l'ARGUMENT de `statResult(…)` : le premier `{` du corps est
/// celui du bloc `if`, pas le sien, et le dernier `}` referme ce même bloc.
/// On se cale donc sur l'appel, et sur la parenthèse qui le ferme.
pub(crate) fn json_de_statdownload(corps: &str) -> Result<Value, String> {
    let apres = corps
        .find("statResult")
        .map(|i| &corps[i..])
        .unwrap_or(corps);
    let debut = apres.find('{').ok_or("statdownload sans JSON")?;
    let borne = apres.rfind(')').unwrap_or(apres.len());
    let fin = apres[..borne]
        .rfind('}')
        .filter(|f| *f >= debut)
        .ok_or("statdownload sans JSON")?;
    serde_json::from_str(&apres[debut..=fin]).map_err(|e| format!("statdownload illisible : {e}"))
}

/// L'URL finale du fichier : celle que `statdownload` rend, sinon l'originale.
pub(crate) fn url_finale(stat: &Value, originale: &str) -> Result<String, String> {
    if stat["result"].as_str() == Some("err") {
        return Err(format!(
            "Bandcamp refuse le téléchargement : {}",
            stat["errortype"].as_str().unwrap_or("motif non donné")
        ));
    }
    Ok(stat["download_url"]
        .as_str()
        .or(stat["url"].as_str())
        .unwrap_or(originale)
        .to_string())
}

/// Un nom de dossier ou de fichier sûr, tiré d'un titre.
pub(crate) fn nom_sur(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    if n.is_empty() { "sans-titre".into() } else { n }
}

/// Le nom de fichier d'un `Content-Disposition: attachment; filename="…"`.
pub(crate) fn nom_de_fichier(disposition: Option<&str>) -> Option<String> {
    let d = disposition?;
    let i = d.find("filename=")?;
    let v = d[i + "filename=".len()..].trim().trim_matches('"');
    let v = v.split(';').next()?.trim().trim_matches('"');
    let v = nom_sur(v);
    (v != "sans-titre").then_some(v)
}

/// Où en est un téléchargement.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state")]
pub(crate) enum Etape {
    /// Lecture de la page de téléchargement.
    Page,
    /// Le fichier descend.
    Telechargement { octets: u64 },
    /// Le zip s'ouvre.
    Extraction,
    /// Terminé : le dossier à donner à l'assistant d'import.
    Termine { dossier: String, fichiers: usize },
    /// Échec, avec l'étape qui a lâché.
    Echec { erreur: String },
}

/// Un téléchargement, suivi par sa clé d'achat.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Tache {
    pub sale_item: String,
    pub artist: String,
    pub title: String,
    #[serde(flatten)]
    pub etape: Etape,
}

/// Les téléchargements en cours et faits, par clé d'achat.
#[derive(Clone, Default)]
pub(crate) struct Registre(Arc<Mutex<BTreeMap<String, Tache>>>);

impl Registre {
    pub(crate) fn liste(&self) -> Vec<Tache> {
        self.0
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
    pub(crate) fn lire(&self, cle: &str) -> Option<Tache> {
        self.0.lock().ok().and_then(|m| m.get(cle).cloned())
    }
    fn poser(&self, cle: &str, etape: Etape) {
        if let Ok(mut m) = self.0.lock() {
            if let Some(t) = m.get_mut(cle) {
                t.etape = etape;
            }
        }
    }
    /// Inscrire une tâche. `false` si elle tourne déjà — on ne descend pas
    /// deux fois le même achat en parallèle.
    pub(crate) fn inscrire(&self, cle: &str, artist: &str, title: &str) -> bool {
        let Ok(mut m) = self.0.lock() else {
            return false;
        };
        if let Some(t) = m.get(cle) {
            if matches!(
                t.etape,
                Etape::Page | Etape::Telechargement { .. } | Etape::Extraction
            ) {
                return false;
            }
        }
        m.insert(
            cle.to_string(),
            Tache {
                sale_item: cle.to_string(),
                artist: artist.to_string(),
                title: title.to_string(),
                etape: Etape::Page,
            },
        );
        true
    }
}

/// Ce qu'il faut pour descendre un achat.
pub(crate) struct Commande {
    pub cle: String,
    pub url_page: String,
    pub identite: String,
    pub racine: PathBuf,
}

/// Descendre un achat, de la page au dossier. Chaque étape est journalisée
/// avec son nom : le premier essai chez un testeur doit dire où ça casse.
pub(crate) async fn telecharger(registre: Registre, cmd: Commande) {
    let cle = cmd.cle.clone();
    match telecharger_ou_echouer(&registre, &cmd).await {
        Ok((dossier, fichiers)) => {
            tracing::info!(achat = %cle, dossier = %dossier.display(), fichiers, "bandcamp_achat_telecharge");
            registre.poser(
                &cle,
                Etape::Termine {
                    dossier: dossier.to_string_lossy().into_owned(),
                    fichiers,
                },
            );
        }
        Err(e) => {
            tracing::warn!(achat = %cle, erreur = %e, "bandcamp_achat_en_echec");
            registre.poser(&cle, Etape::Echec { erreur: e });
        }
    }
}

async fn telecharger_ou_echouer(
    registre: &Registre,
    cmd: &Commande,
) -> Result<(PathBuf, usize), String> {
    let client = tune_core::http::client::shared();
    let cookie = en_tete_cookie(&cmd.identite);

    // 1. La page de téléchargement, avec la session.
    let page = client
        .get(&cmd.url_page)
        .header("Cookie", &cookie)
        .header("User-Agent", "Mozilla/5.0 (compatible; Tune)")
        .send()
        .await
        .map_err(|e| format!("page de téléchargement : {e}"))?;
    if !page.status().is_success() {
        return Err(format!("page de téléchargement : HTTP {}", page.status()));
    }
    let html = page
        .text()
        .await
        .map_err(|e| format!("page de téléchargement : {e}"))?;
    let article = article_du_blob(&blob_de_page(&html)?)?;

    // 2. statdownload — Bandcamp compte le téléchargement et peut rendre une
    // URL fraîche.
    let alea = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1);
    let stat = client
        .get(url_statdownload(&article.url, alea))
        .header("Cookie", &cookie)
        .header("User-Agent", "Mozilla/5.0 (compatible; Tune)")
        .send()
        .await
        .map_err(|e| format!("statdownload : {e}"))?;
    let url = if stat.status().is_success() {
        let corps = stat.text().await.unwrap_or_default();
        url_finale(&json_de_statdownload(&corps)?, &article.url)?
    } else {
        // Un statdownload en échec n'est pas un refus : on tente le fichier.
        tracing::warn!(achat = %cmd.cle, statut = %stat.status(), "bandcamp_statdownload_en_echec");
        article.url.clone()
    };

    // 3. Le fichier, en flux, dans un dossier à son nom.
    let dossier = cmd
        .racine
        .join(DOSSIER_ACHATS)
        .join(nom_sur(&format!("{} - {}", article.artiste, article.titre)));
    tokio::fs::create_dir_all(&dossier)
        .await
        .map_err(|e| format!("dossier {} : {e}", dossier.display()))?;
    registre.poser(&cmd.cle, Etape::Telechargement { octets: 0 });
    let mut reponse = client
        .get(&url)
        .header("Cookie", &cookie)
        .header("User-Agent", "Mozilla/5.0 (compatible; Tune)")
        .send()
        .await
        .map_err(|e| format!("fichier : {e}"))?;
    if !reponse.status().is_success() {
        return Err(format!("fichier : HTTP {}", reponse.status()));
    }
    let nom = nom_de_fichier(
        reponse
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok()),
    )
    .unwrap_or_else(|| {
        let ext = if article.genre == "t" { FORMAT } else { "zip" };
        format!("{}.{ext}", nom_sur(&article.titre))
    });
    let chemin = dossier.join(&nom);
    let mut fichier = tokio::fs::File::create(&chemin)
        .await
        .map_err(|e| format!("écriture {} : {e}", chemin.display()))?;
    let mut octets: u64 = 0;
    use tokio::io::AsyncWriteExt;
    while let Some(morceau) = reponse
        .chunk()
        .await
        .map_err(|e| format!("fichier : {e}"))?
    {
        fichier
            .write_all(&morceau)
            .await
            .map_err(|e| format!("écriture {} : {e}", chemin.display()))?;
        octets += morceau.len() as u64;
        registre.poser(&cmd.cle, Etape::Telechargement { octets });
    }
    fichier
        .flush()
        .await
        .map_err(|e| format!("écriture : {e}"))?;
    drop(fichier);

    // 4. Un zip s'ouvre sur place ; un fichier nu est déjà à sa place.
    let est_zip = tokio::fs::read(&chemin)
        .await
        .map(|b| b.starts_with(b"PK\x03\x04"))
        .unwrap_or(false);
    if !est_zip {
        return Ok((dossier, 1));
    }
    registre.poser(&cmd.cle, Etape::Extraction);
    let dossier_extraction = dossier.clone();
    let chemin_zip = chemin.clone();
    let fichiers = tokio::task::spawn_blocking(move || dezipper(&chemin_zip, &dossier_extraction))
        .await
        .map_err(|e| format!("extraction : {e}"))??;
    let _ = tokio::fs::remove_file(&chemin).await;
    Ok((dossier, fichiers))
}

/// Ouvrir un zip dans `dans`, sans jamais sortir de ce dossier.
pub(crate) fn dezipper(zip: &Path, dans: &Path) -> Result<usize, String> {
    let f = std::fs::File::open(zip).map_err(|e| format!("extraction : {e}"))?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| format!("extraction : {e}"))?;
    let mut ecrits = 0;
    for i in 0..archive.len() {
        let mut entree = archive
            .by_index(i)
            .map_err(|e| format!("extraction : {e}"))?;
        // `enclosed_name` refuse `..` et les chemins absolus.
        let Some(relatif) = entree.enclosed_name() else {
            tracing::warn!(nom = %entree.name(), "bandcamp_zip_entree_refusee");
            continue;
        };
        let cible = dans.join(relatif);
        if entree.is_dir() {
            std::fs::create_dir_all(&cible).map_err(|e| format!("extraction : {e}"))?;
            continue;
        }
        if let Some(parent) = cible.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("extraction : {e}"))?;
        }
        let mut sortie = std::fs::File::create(&cible)
            .map_err(|e| format!("extraction {} : {e}", cible.display()))?;
        let mut tampon = [0u8; 64 * 1024];
        loop {
            let n = entree
                .read(&mut tampon)
                .map_err(|e| format!("extraction : {e}"))?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut sortie, &tampon[..n])
                .map_err(|e| format!("extraction : {e}"))?;
        }
        ecrits += 1;
    }
    Ok(ecrits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn l_identite_se_nettoie_sous_toutes_ses_formes() {
        assert_eq!(
            nettoyer_identite("  abc%2Fdef  ").as_deref(),
            Some("abc%2Fdef")
        );
        assert_eq!(nettoyer_identite("identity=abc").as_deref(), Some("abc"));
        assert_eq!(
            nettoyer_identite("session=x; identity=\"abc\"; client_id=1").as_deref(),
            Some("abc")
        );
        assert_eq!(
            nettoyer_identite("Cookie: identity=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(nettoyer_identite(""), None);
        assert_eq!(nettoyer_identite("session=x; client_id=1"), None);
        assert_eq!(nettoyer_identite("a b"), None);
    }

    #[test]
    fn la_cle_d_achat_indexe_redownload_urls() {
        let brut = json!({
            "items": [{ "sale_item_type": "p", "sale_item_id": 123456, "band_name": "X" }],
            "redownload_urls": { "p123456": "https://bandcamp.com/download?from=collection&payment_id=1&sig=s&sitem_id=123456" }
        });
        let art = &brut["items"][0];
        assert_eq!(cle_d_achat(art).as_deref(), Some("p123456"));
        assert!(
            url_de_retelechargement(&brut, art)
                .unwrap()
                .starts_with("https://bandcamp.com/download?")
        );
        // Sans cookie, Bandcamp ne rend pas le bloc : rien à télécharger.
        assert_eq!(url_de_retelechargement(&json!({"items": []}), art), None);
    }

    #[test]
    fn le_blob_de_page_se_lit_malgre_l_echappement_html() {
        let html = r#"<html><body><div id="pagedata" data-blob="{&quot;digital_items&quot;:[{&quot;artist&quot;:&quot;A &amp; B&quot;,&quot;title&quot;:&quot;T&quot;,&quot;type&quot;:&quot;a&quot;,&quot;downloads&quot;:{&quot;flac&quot;:{&quot;url&quot;:&quot;https://p5.bandcamp.com/download/album?enc=flac&amp;id=1&amp;sig=s&quot;},&quot;mp3-320&quot;:{&quot;url&quot;:&quot;x&quot;}}}]}"></div></body></html>"#;
        let art = article_du_blob(&blob_de_page(html).unwrap()).unwrap();
        assert_eq!(
            art,
            ArticleTelechargeable {
                artiste: "A & B".into(),
                titre: "T".into(),
                genre: "a".into(),
                url: "https://p5.bandcamp.com/download/album?enc=flac&id=1&sig=s".into(),
            }
        );
    }

    #[test]
    fn un_achat_sans_flac_nomme_les_formats_proposes() {
        let blob = json!({"digital_items": [{"downloads": {"mp3-320": {"url": "x"}, "wav": {"url": "y"}}}]});
        let e = article_du_blob(&blob).unwrap_err();
        assert!(e.contains("mp3-320") && e.contains("wav"), "{e}");
        assert!(blob_de_page("<html></html>").is_err());
    }

    #[test]
    fn statdownload_se_deroule_et_prefere_l_url_fraiche() {
        assert_eq!(
            url_statdownload("https://p5.bandcamp.com/download/album?enc=flac&id=1", 7),
            "https://p5.bandcamp.com/statdownload/album?enc=flac&id=1&.vrs=1&.rand=7"
        );
        let corps = r#"if ( window.Downloads ) { Downloads.statResult( {"result":"ok","download_url":"https://p5.bandcamp.com/download/album?enc=flac&id=1&sig=fresh"} ) };"#;
        let stat = json_de_statdownload(corps).unwrap();
        assert_eq!(
            url_finale(&stat, "orig").unwrap(),
            "https://p5.bandcamp.com/download/album?enc=flac&id=1&sig=fresh"
        );
        assert_eq!(
            url_finale(&json!({"result": "ok"}), "orig").unwrap(),
            "orig"
        );
        assert!(
            url_finale(&json!({"result": "err", "errortype": "expired"}), "orig")
                .unwrap_err()
                .contains("expired")
        );
        assert!(json_de_statdownload("rien").is_err());
        // Un corps déjà nu — si Bandcamp change d'enveloppe — passe aussi.
        assert_eq!(
            json_de_statdownload(r#"{"result":"ok"}"#).unwrap()["result"],
            "ok"
        );
    }

    #[test]
    fn les_noms_de_fichiers_sont_surs() {
        assert_eq!(nom_sur("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(nom_sur("  ..  "), "sans-titre");
        assert_eq!(
            nom_de_fichier(Some("attachment; filename=\"Artist - Album.zip\"")).as_deref(),
            Some("Artist - Album.zip")
        );
        assert_eq!(nom_de_fichier(Some("inline")), None);
        assert_eq!(nom_de_fichier(None), None);
    }

    #[test]
    fn le_registre_refuse_un_doublon_en_cours_et_accepte_une_reprise() {
        let r = Registre::default();
        assert!(r.inscrire("p1", "A", "T"));
        assert!(!r.inscrire("p1", "A", "T"), "déjà en cours");
        r.poser("p1", Etape::Echec { erreur: "x".into() });
        assert!(r.inscrire("p1", "A", "T"), "un échec se retente");
        assert_eq!(r.liste().len(), 1);
        let j = serde_json::to_value(r.lire("p1").unwrap()).unwrap();
        assert_eq!(j["state"], "page");
        assert_eq!(j["sale_item"], "p1");
    }

    #[test]
    fn le_zip_s_ouvre_dans_son_dossier_et_refuse_de_remonter() {
        let tmp = std::env::temp_dir().join(format!("tune-bc-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let chemin = tmp.join("a.zip");
        {
            let f = std::fs::File::create(&chemin).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("01 - Un.flac", opts).unwrap();
            std::io::Write::write_all(&mut w, b"fLaC").unwrap();
            w.start_file("cover.jpg", opts).unwrap();
            std::io::Write::write_all(&mut w, b"jpg").unwrap();
            w.start_file("../evasion.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"non").unwrap();
            w.finish().unwrap();
        }
        let n = dezipper(&chemin, &tmp).unwrap();
        assert_eq!(n, 2);
        assert!(tmp.join("01 - Un.flac").exists());
        assert!(!tmp.parent().unwrap().join("evasion.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
