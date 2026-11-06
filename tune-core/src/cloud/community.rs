use serde::Deserialize;
use tracing::{debug, info};

use crate::cloud::refusal::CloudError;

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// A community-approved album cover returned by mozaiklabs.fr.
#[derive(Debug, Clone, Deserialize)]
pub struct CommunityCover {
    pub mbid_release: String,
    pub album_title: String,
    #[serde(default)]
    pub artist_name: Option<String>,
    pub image_url: String,
    pub approved_at: String,
}

/// Report a community-sourced artist image for a given MusicBrainz ID.
pub async fn report_artist_image(
    mbid: &str,
    image_url: &str,
    base_url: Option<&str>,
) -> Result<(), CloudError> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
    let url = format!("{base}/api/v1/artists/{mbid}/image/report");
    let client = crate::http::client::shared();

    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "image_url": image_url }))
        .send()
        .await
        .map_err(|e| format!("report artist image failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(mbid, status = %status, "artist_image_report_rejected");
        return Err(CloudError::from_response(
            format!("artist image report failed: {status}"),
            resp,
        )
        .await);
    }

    info!(mbid, "artist_image_reported");
    Ok(())
}

/// A metadata report to submit to the community backend.
#[derive(Debug, Clone, Default)]
pub struct ReportSubmission<'a> {
    pub entity: &'a str,
    pub mbid: Option<&'a str>,
    pub field: Option<&'a str>,
    pub value: Option<&'a str>,
    pub reason: &'a str,
    pub comment: Option<&'a str>,
}

/// Submit a metadata report (wrong cover, wrong credit, bogus bio…).
/// Values reported by enough distinct instances get unpublished server-side.
pub async fn submit_report(
    base_url: &str,
    instance_id: &str,
    report: &ReportSubmission<'_>,
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/reports");
    let client = crate::http::client::shared();

    let mut body = serde_json::json!({
        "instance_id": instance_id,
        "entity": report.entity,
        "reason": report.reason,
    });
    for (key, val) in [
        ("mbid", report.mbid),
        ("field", report.field),
        ("value", report.value),
        ("comment", report.comment),
    ] {
        if let Some(v) = val {
            body[key] = serde_json::json!(v);
        }
    }

    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit report failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(entity = report.entity, status = %status, "metadata_report_rejected");
        return Err(
            CloudError::from_response(format!("metadata report failed: {status}"), resp).await,
        );
    }

    info!(
        entity = report.entity,
        reason = report.reason,
        "metadata_report_submitted"
    );
    Ok(())
}

/// Submit a community album cover to mozaiklabs.fr for approval.
pub async fn submit_cover(
    base_url: &str,
    mbid_release: &str,
    album_title: &str,
    artist_name: Option<&str>,
    instance_id: &str,
    image_data: &[u8],
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/covers");
    let client = crate::http::client::shared();

    let mut form = reqwest::multipart::Form::new()
        .text("mbid_release", mbid_release.to_string())
        .text("album_title", album_title.to_string())
        .text("instance_id", instance_id.to_string());

    if let Some(artist) = artist_name {
        form = form.text("artist_name", artist.to_string());
    }

    let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name(format!("{mbid_release}.jpg"))
        .mime_str("image/jpeg")
        .map_err(|e| format!("mime error: {e}"))?;
    form = form.part("image", image_part);

    let resp = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit cover failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(mbid_release, status = %status, "cover_submit_rejected");
        return Err(
            CloudError::from_response(format!("cover submit failed: {status}"), resp).await,
        );
    }

    info!(mbid_release, "community_cover_submitted");
    Ok(())
}

/// Propose la photo de l'appareil d'une zone au catalogue communautaire
/// (`POST /api/v1/community/device-images`, tune-web-client#2109).
///
/// Le site l'enregistre **en attente** et ne la sert à personne avant
/// l'approbation de Bertrand (« une photo ne peut pas être redescendue avant
/// que je ne l'aie approuvée », 20/09/2026). La clef est la paire marque +
/// modèle ; `instance_id` borne à UNE proposition par appareil et par
/// instance (une seconde remplace la première et repart en attente).
///
/// `mime` vient des OCTETS (`sniff_image_ext` puis `cache_mime`), jamais d'un
/// type annoncé : le site valide une image, il la refuserait sinon.
pub async fn submit_device_image(
    base_url: &str,
    brand: &str,
    model: &str,
    instance_id: &str,
    image_data: &[u8],
    ext: &str,
    mime: &str,
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/device-images");
    let client = crate::http::client::shared();

    let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name(format!("appareil.{ext}"))
        .mime_str(mime)
        .map_err(|e| format!("mime error: {e}"))?;
    let form = reqwest::multipart::Form::new()
        .text("brand", brand.to_string())
        .text("model", model.to_string())
        .text("instance_id", instance_id.to_string())
        .part("image", image_part);

    let resp = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("submit device image failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(brand, model, status = %status, "device_image_submit_rejected");
        return Err(CloudError::from_response(
            format!("device image submit failed: {status}"),
            resp,
        )
        .await);
    }

    info!(brand, model, "community_device_image_submitted");
    Ok(())
}

/// Fetch approved community covers from mozaiklabs.fr.
/// Pass `since` for incremental sync (ISO 8601 timestamp).
pub async fn fetch_approved_covers(
    base_url: &str,
    since: Option<&str>,
) -> Result<Vec<CommunityCover>, CloudError> {
    let base = base_url.trim_end_matches('/');
    let mut url = format!("{base}/api/v1/community/covers/approved");
    if let Some(s) = since {
        url.push_str(&format!("?since={}", urlencoding::encode(s)));
    }
    let client = crate::http::client::shared();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("fetch approved covers failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(status = %status, "fetch_approved_covers_failed");
        return Err(CloudError::from_response(
            format!("fetch approved covers failed: {status}"),
            resp,
        )
        .await);
    }

    #[derive(Deserialize)]
    struct Wrapper {
        covers: Vec<CommunityCover>,
    }

    let wrapper: Wrapper = resp
        .json()
        .await
        .map_err(|e| format!("parse approved covers: {e}"))?;

    info!(count = wrapper.covers.len(), "community_covers_fetched");
    Ok(wrapper.covers)
}

/// A community-approved artist image returned by mozaiklabs.fr.
#[derive(Debug, Clone, Deserialize)]
pub struct CommunityArtistImage {
    pub mbid: String,
    pub artist_name: String,
    pub image_url: String,
    pub approved_at: String,
}

/// Submit a community artist image to mozaiklabs.fr for approval.
///
/// `source` dit d'où vient l'image (`fanart`, `theaudiodb`, `musicbrainz`,
/// `discogs`, `deezer`, `lastfm`, `manual`…) : le site approuve d'office les
/// sources publiques fiables et laisse les autres en validation manuelle
/// (décision du 31/10/2026). Sans elle, toute image attendait un modérateur
/// et chaque serveur refaisait le travail auprès des sources externes.
pub async fn submit_artist_image(
    base_url: &str,
    mbid: &str,
    artist_name: &str,
    instance_id: &str,
    source: &str,
    image_data: &[u8],
) -> Result<(), CloudError> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/community/artist-images");
    let client = crate::http::client::shared();

    let image_part = reqwest::multipart::Part::bytes(image_data.to_vec())
        .file_name(format!("{mbid}.jpg"))
        .mime_str("image/jpeg")
        .map_err(|e| format!("mime error: {e}"))?;

    let form = reqwest::multipart::Form::new()
        .text("mbid", mbid.to_string())
        .text("artist_name", artist_name.to_string())
        .text("instance_id", instance_id.to_string())
        .text("source", source.to_string())
        .part("image", image_part);

    let resp = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("submit artist image failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(mbid, status = %status, "artist_image_submit_rejected");
        return Err(CloudError::from_response(
            format!("artist image submit failed: {status}"),
            resp,
        )
        .await);
    }

    info!(mbid, artist_name, "community_artist_image_submitted");
    Ok(())
}

/// Le curseur de la page suivante de la liste approuvée (`next` du site).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CurseurImagesApprouvees {
    pub since: String,
    pub after_id: i64,
}

/// Une page de la liste approuvée : les images, et le curseur de la suivante
/// (`None` sur la dernière, et toujours `None` avec un site d'avant la
/// pagination, qui rendait tout d'un bloc).
#[derive(Debug, Clone)]
pub struct PageImagesApprouvees {
    pub images: Vec<CommunityArtistImage>,
    pub next: Option<CurseurImagesApprouvees>,
}

/// Taille de page demandée au site (il en sert 1 000 au plus).
pub const IMAGES_APPROUVEES_PAR_PAGE: u32 = 500;

/// Garde-fou : au-delà, la reprise s'arrête (500 000 images). Un site qui
/// rendrait sans fin de nouveaux curseurs ne fait pas tourner la passe à vide.
pub const IMAGES_APPROUVEES_PAGES_MAX: usize = 1_000;

/// Pause entre deux pages : le site limite cette route à 30 appels par
/// minute et par adresse.
pub const PAUSE_ENTRE_PAGES: std::time::Duration = std::time::Duration::from_millis(2_100);

/// Fetch ONE page of approved community artist images from mozaiklabs.fr.
pub async fn fetch_approved_artist_images_page(
    base_url: &str,
    curseur: Option<&CurseurImagesApprouvees>,
) -> Result<PageImagesApprouvees, CloudError> {
    let base = base_url.trim_end_matches('/');
    let mut url = format!(
        "{base}/api/v1/community/artist-images/approved?limit={IMAGES_APPROUVEES_PAR_PAGE}"
    );
    if let Some(c) = curseur {
        url.push_str(&format!(
            "&since={}&after_id={}",
            urlencoding::encode(&c.since),
            c.after_id
        ));
    }
    let client = crate::http::client::shared();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("fetch approved artist images failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        debug!(status = %status, "fetch_approved_artist_images_failed");
        return Err(CloudError::from_response(
            format!("fetch approved artist images failed: {status}"),
            resp,
        )
        .await);
    }

    #[derive(Deserialize)]
    struct Wrapper {
        images: Vec<CommunityArtistImage>,
        #[serde(default)]
        next: Option<CurseurImagesApprouvees>,
    }

    let wrapper: Wrapper = resp
        .json()
        .await
        .map_err(|e| format!("parse approved artist images: {e}"))?;

    Ok(PageImagesApprouvees {
        images: wrapper.images,
        next: wrapper.next,
    })
}

/// Fetch ALL approved community artist images from mozaiklabs.fr, page by
/// page, following the site's `next` cursor.
///
/// Avant la pagination, un seul appel rendait toute la liste ; une liste de
/// dizaines de milliers d'images ne tient plus dans une réponse. Une erreur
/// sur une page suivante garde ce qui a déjà été lu : la passe applique ce
/// qu'elle a, la suivante reprendra le reste.
pub async fn fetch_approved_artist_images(
    base_url: &str,
) -> Result<Vec<CommunityArtistImage>, CloudError> {
    fetch_approved_artist_images_with_pause(base_url, PAUSE_ENTRE_PAGES).await
}

/// [`fetch_approved_artist_images`] avec une pause réglable entre deux pages
/// (les témoins la mettent à zéro).
pub async fn fetch_approved_artist_images_with_pause(
    base_url: &str,
    pause: std::time::Duration,
) -> Result<Vec<CommunityArtistImage>, CloudError> {
    let mut toutes = Vec::new();
    let mut curseur: Option<CurseurImagesApprouvees> = None;
    for page in 0..IMAGES_APPROUVEES_PAGES_MAX {
        if page > 0 && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        let lue = match fetch_approved_artist_images_page(base_url, curseur.as_ref()).await {
            Ok(p) => p,
            Err(e) if page == 0 => return Err(e),
            Err(e) => {
                debug!(page, error = %e, "fetch_approved_artist_images_page_failed");
                break;
            }
        };
        toutes.extend(lue.images);
        match lue.next {
            // Un curseur qui n'avance pas bouclerait : on s'arrête.
            Some(n) if curseur.as_ref() != Some(&n) => curseur = Some(n),
            _ => break,
        }
    }

    info!(count = toutes.len(), "community_artist_images_fetched");
    Ok(toutes)
}

/// L'adresse d'une image approuvée, rendue absolue.
///
/// Le site sert `image_url` sous la forme `/storage/…`, RELATIVE à son
/// origine. Téléchargée telle quelle, elle échouait toujours (une requête
/// HTTP veut une adresse absolue) : la reprise en lot des images approuvées
/// n'avait jamais rien posé.
pub fn url_image_communautaire(base_url: &str, image_url: &str) -> String {
    if image_url.starts_with("http://") || image_url.starts_with("https://") {
        image_url.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            image_url.trim_start_matches('/')
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_url_constant() {
        assert_eq!(super::DEFAULT_BASE_URL, "https://mozaiklabs.fr");
    }
}

/// Images d'artistes partagées par le cloud (31/10/2026) : la source part
/// avec l'image, la liste approuvée se lit par pages, ses adresses relatives
/// sont rendues absolues.
#[cfg(test)]
mod tests_images_artistes {
    use super::*;
    use std::sync::{Arc, Mutex};

    async fn servir(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://127.0.0.1:{port}")
    }

    fn image(mbid: &str) -> serde_json::Value {
        serde_json::json!({
            "mbid": mbid,
            "artist_name": format!("Artiste {mbid}"),
            "image_url": format!("/storage/community-artist-images/{mbid}.jpg"),
            "approved_at": "2026-10-31T12:00:00+00:00",
        })
    }

    #[tokio::test]
    async fn l_envoi_d_une_image_d_artiste_porte_sa_source() {
        let recu = Arc::new(Mutex::new(String::new()));
        let r = recu.clone();
        let app = axum::Router::new().route(
            "/api/v1/community/artist-images",
            axum::routing::post(move |corps: axum::body::Bytes| {
                let r = r.clone();
                async move {
                    *r.lock().unwrap() = String::from_utf8_lossy(&corps).into_owned();
                    (
                        axum::http::StatusCode::CREATED,
                        axum::Json(serde_json::json!({"status": "approved"})),
                    )
                }
            }),
        );
        let base = servir(app).await;

        submit_artist_image(
            &base,
            "0383dadf-2a4e-4d10-a46a-e9e041da8eb3",
            "Queen",
            "b2b2b2b2-0000-4000-8000-000000000001",
            "fanart",
            b"octets",
        )
        .await
        .unwrap();

        let corps = recu.lock().unwrap().clone();
        assert!(
            corps.contains("name=\"source\"\r\n\r\nfanart\r\n"),
            "le champ `source` manque à l'envoi de l'image d'artiste : le site la \
             laissera en validation manuelle. Corps reçu : {corps}"
        );
    }

    /// Deux pages servies par curseur ; l'appel sans curseur demande la
    /// première, celui avec `after_id=2` la seconde.
    #[tokio::test]
    async fn la_reprise_suit_les_pages_du_site_jusqu_a_la_derniere() {
        let appels = Arc::new(Mutex::new(Vec::<String>::new()));
        let a = appels.clone();
        let app = axum::Router::new().route(
            "/api/v1/community/artist-images/approved",
            axum::routing::get(move |axum::extract::RawQuery(q): axum::extract::RawQuery| {
                let a = a.clone();
                async move {
                    let q = q.unwrap_or_default();
                    a.lock().unwrap().push(q.clone());
                    let corps = if q.contains("after_id=2") {
                        serde_json::json!({"images": [image("c")], "next": null})
                    } else {
                        serde_json::json!({
                            "images": [image("a"), image("b")],
                            "next": {"since": "2026-10-31T12:00:00+00:00", "after_id": 2},
                        })
                    };
                    axum::Json(corps)
                }
            }),
        );
        let base = servir(app).await;

        let toutes = fetch_approved_artist_images_with_pause(&base, std::time::Duration::ZERO)
            .await
            .unwrap();

        let mbids: Vec<&str> = toutes.iter().map(|i| i.mbid.as_str()).collect();
        assert_eq!(
            mbids,
            ["a", "b", "c"],
            "la reprise en lot s'est arrêtée à la première page du site"
        );
        let appels = appels.lock().unwrap().clone();
        assert_eq!(appels.len(), 2, "{appels:?}");
        assert!(appels[0].contains("limit=500"), "{appels:?}");
        assert!(
            appels[1].contains("since=2026-10-31T12%3A00%3A00%2B00%3A00")
                && appels[1].contains("after_id=2"),
            "le curseur `next` n'est pas renvoyé tel quel : {appels:?}"
        );
    }

    /// Un site d'avant la pagination ne rend pas `next` : un seul appel.
    #[tokio::test]
    async fn un_site_sans_pagination_rend_tout_en_un_appel() {
        let appels = Arc::new(Mutex::new(0usize));
        let a = appels.clone();
        let app = axum::Router::new().route(
            "/api/v1/community/artist-images/approved",
            axum::routing::get(move || {
                let a = a.clone();
                async move {
                    *a.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({"images": [image("a")]}))
                }
            }),
        );
        let base = servir(app).await;

        let toutes = fetch_approved_artist_images_with_pause(&base, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(toutes.len(), 1);
        assert_eq!(*appels.lock().unwrap(), 1);
    }

    /// Un curseur qui n'avance pas ne fait pas boucler la passe.
    #[tokio::test]
    async fn un_curseur_qui_n_avance_pas_arrete_la_reprise() {
        let appels = Arc::new(Mutex::new(0usize));
        let a = appels.clone();
        let app = axum::Router::new().route(
            "/api/v1/community/artist-images/approved",
            axum::routing::get(move || {
                let a = a.clone();
                async move {
                    *a.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({
                        "images": [image("a")],
                        "next": {"since": "2026-10-31T12:00:00+00:00", "after_id": 1},
                    }))
                }
            }),
        );
        let base = servir(app).await;

        let _ = fetch_approved_artist_images_with_pause(&base, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(*appels.lock().unwrap(), 2);
    }

    #[test]
    fn l_adresse_relative_d_une_image_approuvee_devient_absolue() {
        assert_eq!(
            url_image_communautaire(
                "https://mozaiklabs.fr",
                "/storage/community-artist-images/x.jpg"
            ),
            "https://mozaiklabs.fr/storage/community-artist-images/x.jpg",
            "une adresse `/storage/…` téléchargée telle quelle échoue toujours"
        );
        assert_eq!(
            url_image_communautaire("https://mozaiklabs.fr/", "https://cdn.example/x.jpg"),
            "https://cdn.example/x.jpg"
        );
    }
}
