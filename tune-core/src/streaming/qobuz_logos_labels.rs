//! #6736 — Qobuz comme [`CatalogueDeLabels`] : retrouver le label Qobuz d'un
//! nom de label local et l'adresse de son image.
//!
//! Deux voies, la plus sûre d'abord :
//!
//! 1. un album Qobuz de la bibliothèque porte ce label : son `label.id`
//!    (`album/get`, servi par le cache d'album de #2190) — à condition que le
//!    nom du label Qobuz concorde, un album mal étiqueté ne doit pas donner
//!    son logo à un autre label ;
//! 2. sinon `label/search`, et seul un résultat dont le nom concorde exactement
//!    ([`noms_concordent`]) est retenu.
//!
//! Puis `label/get` pour la fiche du label, dont on lit l'image. Au plus trois
//! appels par label, et un label n'est demandé qu'une fois par délai (voir
//! [`crate::library::logos_labels`]).

use super::QobuzService;
use crate::library::logos_labels::{
    CatalogueDeLabels, LabelDuCatalogue, TAILLE_MAX_IMAGE, noms_concordent,
};

/// Un identifiant Qobuz, qu'il arrive en nombre ou en chaîne.
fn identifiant(v: &serde_json::Value) -> Option<String> {
    v.as_u64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().filter(|s| !s.is_empty()).map(String::from))
}

/// L'adresse de l'image d'une fiche `label/get`. La forme n'est pas
/// documentée : une chaîne, ou un objet de tailles comme celui des albums
/// (`large`, `medium`…) — la plus grande gagne. Seules les adresses http(s)
/// sont retenues.
pub(crate) fn image_de_label(fiche: &serde_json::Value) -> Option<String> {
    let http =
        |s: &str| (s.starts_with("https://") || s.starts_with("http://")).then(|| s.to_string());
    for cle in ["image", "images", "logo", "picture"] {
        let v = &fiche[cle];
        if let Some(s) = v.as_str().and_then(http) {
            return Some(s);
        }
        for taille in [
            "mega",
            "extralarge",
            "large",
            "medium",
            "small",
            "thumbnail",
        ] {
            if let Some(s) = v[taille].as_str().and_then(http) {
                return Some(s);
            }
        }
    }
    None
}

/// Le premier label d'une réponse `label/search` dont le nom concorde.
pub(crate) fn label_concordant(reponse: &serde_json::Value, nom: &str) -> Option<(String, String)> {
    let items = reponse["labels"]["items"]
        .as_array()
        .or_else(|| reponse["items"].as_array())?;
    items.iter().find_map(|it| {
        let n = it["name"].as_str()?;
        noms_concordent(n, nom).then(|| identifiant(&it["id"]).map(|id| (id, n.to_string())))?
    })
}

#[async_trait::async_trait]
impl CatalogueDeLabels for QobuzService {
    async fn trouver_label(
        &self,
        nom: &str,
        album_du_catalogue: Option<&str>,
    ) -> Result<Option<LabelDuCatalogue>, String> {
        let mut candidat = None;
        if let Some(album_id) = album_du_catalogue
            && let Ok(album) = self.detail_album(album_id).await
            && let Some(id) = identifiant(&album["label"]["id"])
        {
            let n = album["label"]["name"].as_str().unwrap_or_default();
            if noms_concordent(n, nom) {
                candidat = Some((id, n.to_string()));
            }
        }
        if candidat.is_none() {
            let reponse = self
                .api_get("/label/search", &[("query", nom), ("limit", "10")])
                .await?;
            candidat = label_concordant(&reponse, nom);
        }
        let Some((id, nom_qobuz)) = candidat else {
            return Ok(None);
        };
        let fiche = self.api_get("/label/get", &[("label_id", &id)]).await?;
        Ok(Some(LabelDuCatalogue {
            image_url: image_de_label(&fiche),
            id,
            nom: nom_qobuz,
        }))
    }

    async fn telecharger_image(&self, url: &str) -> Result<Vec<u8>, String> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("adresse refusée : {url}"));
        }
        let reponse = self
            .client
            .get(url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !reponse.status().is_success() {
            return Err(format!("HTTP {}", reponse.status()));
        }
        if reponse
            .content_length()
            .is_some_and(|n| n as usize > TAILLE_MAX_IMAGE)
        {
            return Err(String::from("image trop lourde"));
        }
        let octets = reponse.bytes().await.map_err(|e| e.to_string())?;
        Ok(octets.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::HashMap as Carte;
    use std::sync::{Arc, Mutex};

    #[test]
    fn l_image_d_un_label_se_lit_en_chaine_ou_en_tailles() {
        assert_eq!(
            image_de_label(&json!({"image": "https://static.qobuz.test/l.jpg"})).as_deref(),
            Some("https://static.qobuz.test/l.jpg")
        );
        assert_eq!(
            image_de_label(
                &json!({"image": {"small": "https://s/s.jpg", "large": "https://s/l.jpg"}})
            )
            .as_deref(),
            Some("https://s/l.jpg")
        );
        assert_eq!(image_de_label(&json!({"image": null})), None);
        assert_eq!(
            image_de_label(&json!({"image": "file:///etc/passwd"})),
            None
        );
    }

    #[test]
    fn la_recherche_ne_retient_qu_un_nom_qui_concorde() {
        let r = json!({"labels": {"items": [
            {"id": 1, "name": "Warp Records"},
            {"id": "2", "name": "Warp"}
        ]}});
        assert_eq!(
            label_concordant(&r, "warp"),
            Some(("2".into(), "Warp".into()))
        );
        assert_eq!(label_concordant(&r, "Warp Rec"), None);
    }

    /// Un Qobuz simulé (aucun appel réseau réel) qui journalise ses routes.
    async fn qobuz_simule() -> (String, Arc<Mutex<Vec<String>>>) {
        let journal = Arc::new(Mutex::new(Vec::new()));
        let (j1, j2, j3) = (journal.clone(), journal.clone(), journal.clone());
        let app = Router::new()
            .route(
                "/album/get",
                get(move |Query(q): Query<Carte<String, String>>| {
                    j1.lock()
                        .unwrap()
                        .push(format!("album/get {}", q["album_id"]));
                    async move {
                        Json(json!({
                            "id": q["album_id"], "title": "t", "artist": {"name": "a"},
                            "label": {"id": 77, "name": "Verve Records"},
                            "tracks_count": 0, "tracks": {"items": [], "total": 0}
                        }))
                    }
                }),
            )
            .route(
                "/label/search",
                get(move |Query(q): Query<Carte<String, String>>| {
                    j2.lock()
                        .unwrap()
                        .push(format!("label/search {}", q["query"]));
                    async move {
                        Json(json!({"labels": {"total": 2, "items": [
                            {"id": 5, "name": "ECM Records"},
                            {"id": 9, "name": "ECM"}
                        ]}}))
                    }
                }),
            )
            .route(
                "/label/get",
                get(move |Query(q): Query<Carte<String, String>>| {
                    j3.lock()
                        .unwrap()
                        .push(format!("label/get {}", q["label_id"]));
                    async move {
                        Json(json!({"id": q["label_id"], "name": "x",
                                    "image": "https://static.qobuz.test/label.jpg"}))
                    }
                }),
            );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        (format!("http://{adresse}"), journal)
    }

    #[tokio::test]
    async fn sans_album_rapproche_la_recherche_puis_la_fiche_du_label() {
        let (base, journal) = qobuz_simule().await;
        let svc = QobuzService::avec_base_forcee(base);
        let label = svc
            .trouver_label("ecm", None)
            .await
            .unwrap()
            .expect("ECM trouvé");
        assert_eq!(label.id, "9", "« ECM Records » ne passe pas pour « ECM »");
        assert_eq!(
            label.image_url.as_deref(),
            Some("https://static.qobuz.test/label.jpg")
        );
        assert_eq!(
            *journal.lock().unwrap(),
            vec!["label/search ecm", "label/get 9"]
        );
    }

    #[tokio::test]
    async fn l_album_rapproche_donne_le_label_sans_recherche() {
        let (base, journal) = qobuz_simule().await;
        let svc = QobuzService::avec_base_forcee(base);
        let label = svc
            .trouver_label("Verve Records", Some("0602"))
            .await
            .unwrap()
            .expect("label de l'album");
        assert_eq!(label.id, "77");
        assert_eq!(
            *journal.lock().unwrap(),
            vec!["album/get 0602", "label/get 77"]
        );
    }

    #[tokio::test]
    async fn un_album_d_un_autre_label_ne_donne_pas_son_logo() {
        let (base, journal) = qobuz_simule().await;
        let svc = QobuzService::avec_base_forcee(base);
        // L'album rapproché porte « Verve Records » : pour « ECM », on cherche.
        let label = svc
            .trouver_label("ECM", Some("0602"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(label.id, "9");
        assert_eq!(
            *journal.lock().unwrap(),
            vec!["album/get 0602", "label/search ECM", "label/get 9"]
        );
    }
}
