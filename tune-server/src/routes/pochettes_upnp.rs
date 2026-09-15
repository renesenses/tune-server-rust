//! Cache durable des pochettes annoncées par les serveurs UPnP (#4201).
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tune_core::library::artwork::{
    cache_fetched_image, content_hash, find_cached, sniff_image_ext,
};

const MAX_OCTETS: usize = 8 * 1024 * 1024;
const FRAICHEUR_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize)]
struct Entree {
    hash: String,
    verifie_a: u64,
}

fn maintenant() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Quatre images simultanées, huit Mio chacune, six secondes par image et
/// deux minutes au plus par passe. Une erreur de pochette ne rend jamais
/// incomplet le catalogue audio. Les fichiers déjà acquis restent durables.
pub(super) async fn preparer(
    client: &reqwest::Client,
    cache: &Path,
    urls: impl Iterator<Item = String>,
) -> HashMap<String, String> {
    let mut urls: Vec<_> = urls.collect();
    urls.sort();
    urls.dedup();
    let mut resultats = HashMap::new();
    let mut travail = stream::iter(urls)
        .map(|url| async move {
            let hash = recuperer(client, cache, &url).await;
            (url, hash)
        })
        .buffer_unordered(4);
    let fin = tokio::time::Instant::now() + Duration::from_secs(120);
    while let Ok(Some((url, hash))) = tokio::time::timeout_at(fin, travail.next()).await {
        if let Some(hash) = hash {
            resultats.insert(url, hash);
        }
    }
    resultats
}

async fn recuperer(client: &reqwest::Client, cache: &Path, url: &str) -> Option<String> {
    let cle = content_hash(url.as_bytes());
    let index = cache.join(format!("upnp-{cle}.json"));
    let ancienne: Option<Entree> = tokio::fs::read(&index)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let ancienne = ancienne.filter(|e| {
        e.hash.len() == 64
            && e.hash.bytes().all(|c| c.is_ascii_hexdigit())
            && find_cached(cache, &e.hash).is_some()
    });
    if let Some(e) = &ancienne
        && maintenant().saturating_sub(e.verifie_a) < FRAICHEUR_SECS
    {
        return Some(e.hash.clone());
    }

    let telechargement = async {
        let url = reqwest::Url::parse(url).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let mut reponse = client
            .get(url)
            .timeout(Duration::from_secs(6))
            .send()
            .await
            .ok()?;
        if !reponse.status().is_success()
            || reponse
                .content_length()
                .is_some_and(|n| n > MAX_OCTETS as u64)
        {
            return None;
        }
        let mut image = Vec::new();
        while let Some(bloc) = reponse.chunk().await.ok()? {
            if image.len().saturating_add(bloc.len()) > MAX_OCTETS {
                return None;
            }
            image.extend_from_slice(&bloc);
        }
        let ext = sniff_image_ext(&image)?;
        let dossier = cache.to_path_buf();
        let hash = tokio::task::spawn_blocking(move || cache_fetched_image(&image, &dossier, ext))
            .await
            .ok()??;
        let entree = Entree {
            hash: hash.clone(),
            verifie_a: maintenant(),
        };
        // L'index ne publie le condensat qu'après écriture complète de l'image.
        let temporaire = cache.join(format!("upnp-{cle}-{}.tmp", uuid::Uuid::new_v4()));
        if tokio::fs::write(&temporaire, serde_json::to_vec(&entree).ok()?)
            .await
            .is_ok()
        {
            let _ = tokio::fs::rename(&temporaire, &index).await;
            let _ = tokio::fs::remove_file(&temporaire).await;
        }
        Some(hash)
    };
    // Le cache périmé reste utilisable si le serveur s'éteint ou répond mal.
    tokio::time::timeout(Duration::from_secs(6), telechargement)
        .await
        .ok()
        .flatten()
        .or_else(|| ancienne.map(|e| e.hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};

    #[tokio::test]
    async fn une_copie_perimee_survit_a_une_reponse_invalide() {
        let serveur = Router::new().route("/image", get(|| async { "<html>Erreur du NAS</html>" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let tache = tokio::spawn(async move {
            axum::serve(listener, serveur).await.unwrap();
        });
        let cache = tempfile::tempdir().unwrap();
        let image = b"\x89PNG\r\n\x1a\nancienne image";
        let hash = cache_fetched_image(image, cache.path(), "png").unwrap();
        let index = cache
            .path()
            .join(format!("upnp-{}.json", content_hash(url.as_bytes())));
        tokio::fs::write(
            &index,
            serde_json::to_vec(&Entree {
                hash: hash.clone(),
                verifie_a: 0,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        assert_eq!(
            recuperer(&client, cache.path(), &url).await,
            Some(hash.clone()),
            "un rafraîchissement raté conserve la copie précédente"
        );
        let (path, _) = find_cached(cache.path(), &hash).unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), image);
        tokio::fs::remove_file(path).await.unwrap();
        assert_eq!(
            recuperer(&client, cache.path(), &url).await,
            None,
            "un index seul ne prouve pas que l'image existe"
        );
        tache.abort();
    }

    #[tokio::test]
    async fn une_image_sans_content_length_est_bornee_en_lecture() {
        let serveur = Router::new().route(
            "/image",
            get(|| async {
                let mut blocs = vec![Ok::<_, std::io::Error>(b"\x89PNG\r\n\x1a\n".to_vec())];
                for _ in 0..9 {
                    blocs.push(Ok(vec![0; 1024 * 1024]));
                }
                Body::from_stream(stream::iter(blocs))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let tache = tokio::spawn(async move {
            axum::serve(listener, serveur).await.unwrap();
        });
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(
            recuperer(&reqwest::Client::new(), cache.path(), &url).await,
            None
        );
        assert_eq!(
            std::fs::read_dir(cache.path()).unwrap().count(),
            0,
            "aucune image surdimensionnée publiée"
        );
        tache.abort();
    }
}
