use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// A catalog row as served by `GET /api/v1/plugins` on mozaiklabs.fr (the
/// Laravel `Plugin::toApiArray()`). Every non-key field is defaulted: the
/// catalog schema has grown across eras (python-era rows lack `price`,
/// `downloads`, `rating`) and a single unknown/missing field must not turn
/// the whole catalog into an empty list (serde fails the entire Vec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub slug: String,
    pub name: String,
    /// Human-facing name (`display_name` in the Laravel model).
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    /// `None` means free plugin.
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub category: String,
    #[serde(default, alias = "install_count")]
    pub downloads: u64,
    #[serde(default, alias = "vote_score")]
    pub rating: f64,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub installed_version: Option<String>,
    /// Legacy field kept for backward compat.
    #[serde(default, alias = "vote_count")]
    pub votes: i64,
    #[serde(default)]
    pub download_url: Option<String>,
    /// `wasm` rows are installable by the Rust server; python-era rows are not.
    #[serde(default)]
    pub platforms: Option<String>,
    #[serde(default)]
    pub install_type: Option<String>,
}

impl MarketplacePlugin {
    /// Can this catalog row actually be installed by this server?
    ///
    /// Relevé sur le catalogue en ligne (`GET /api/v1/plugins`, 2026-08-29) :
    /// 24 fiches sur 25 portent `platforms: "python"` et un `install_source`
    /// en `pip install …`. Ce sont les fiches héritées du Tune écrit en
    /// Python : elles ne s'installent nulle part sur un serveur Rust. Une
    /// seule fiche porte `platforms: "wasm"`.
    ///
    /// Le tri porte sur `platforms`, jamais sur `install_type` : ce dernier
    /// nomme le canal de distribution (`core`, `builtin`, `store`), pas le
    /// moteur qui exécute le plugin. Sur ce relevé les deux se recoupent par
    /// accident (22 `core` + 2 `builtin` du côté python, 1 `store` du côté
    /// wasm), mais rien n'interdit au catalogue de publier demain un plugin
    /// wasm en `core` : trier sur `install_type` le ferait disparaître.
    ///
    /// Une fiche qui ne déclare aucune plateforme est **gardée** : un champ
    /// absent ne doit pas faire disparaître un plugin que le catalogue
    /// ajouterait plus tard.
    pub fn is_installable(&self) -> bool {
        match self.platforms.as_deref().map(str::trim) {
            None | Some("") => true,
            Some(platforms) => platforms
                .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
                .any(|token| token.eq_ignore_ascii_case("wasm")),
        }
    }
}

pub struct PluginMarketplace {
    base_url: String,
}

impl PluginMarketplace {
    pub fn new(base_url: Option<&str>) -> Self {
        Self {
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// List the plugins this server can actually install.
    ///
    /// The Laravel store serves the catalog at `/api/v1/plugins` — the older
    /// `/api/v1/plugins/catalog` path never existed server-side, so this
    /// client silently returned an empty catalog (404 → `vec![]`).
    ///
    /// Le catalogue distant contient encore des fiches de l'ère Python, qui
    /// ne s'installent nulle part (voir [`MarketplacePlugin::is_installable`]).
    /// Elles sont écartées **ici**, au seul point où le catalogue est
    /// désérialisé : `detail()` et les deux routes qui exposent la liste en
    /// héritent sans le redire. Le serveur ne dépend donc plus de la propreté
    /// d'une base distante.
    pub async fn list(&self) -> Vec<MarketplacePlugin> {
        let catalog = self.fetch_catalog().await;
        let fetched = catalog.len();

        let plugins: Vec<MarketplacePlugin> = catalog
            .into_iter()
            .filter(MarketplacePlugin::is_installable)
            .collect();

        let dropped = fetched - plugins.len();
        if dropped > 0 {
            info!(
                kept = plugins.len(),
                dropped, "marketplace_catalog_uninstallable_rows_dropped"
            );
        }
        plugins
    }

    /// Raw catalog as served, before any filtering.
    async fn fetch_catalog(&self) -> Vec<MarketplacePlugin> {
        let url = format!("{}/api/v1/plugins", self.base_url);
        let client = crate::http::client::shared();

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp.json().await.unwrap_or_default(),
            Ok(resp) => {
                debug!(status = %resp.status(), "marketplace_list_failed");
                vec![]
            }
            Err(e) => {
                debug!(error = %e, "marketplace_list_request_failed");
                vec![]
            }
        }
    }

    /// Fetch detail for a single marketplace plugin by slug (or package name).
    ///
    /// The Laravel store has no per-plugin detail endpoint, so this filters
    /// the catalog list client-side.
    pub async fn detail(&self, slug: &str) -> Option<MarketplacePlugin> {
        self.list()
            .await
            .into_iter()
            .find(|p| p.slug == slug || p.name == slug)
    }

    /// Fetch the detached minisign signature for a plugin artifact.
    ///
    /// `Ok(None)` means the marketplace does not publish one — the caller
    /// decides whether that is fatal (see the `plugin_signature_required`
    /// setting in the server). A transport failure is an `Err`: "the network
    /// broke" must not be read as "this plugin is unsigned".
    pub async fn download_signature(&self, name: &str) -> Result<Option<String>, String> {
        /// A minisign signature is a couple of short base64 lines.
        const MAX_SIG_BYTES: u64 = 8 * 1024;

        let url = format!(
            "{}/api/v1/plugins/{}/download.minisig",
            self.base_url,
            urlencoding::encode(name)
        );
        let client = crate::http::client::long_timeout();

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("plugin signature request failed: {e}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("plugin signature fetch: HTTP {}", resp.status()));
        }
        if resp.content_length().unwrap_or(0) > MAX_SIG_BYTES {
            return Err("plugin signature is implausibly large".into());
        }

        let text = resp
            .text()
            .await
            .map_err(|e| format!("read plugin signature failed: {e}"))?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(text))
    }

    /// Download a plugin binary/archive by name.
    ///
    /// The body is read with a hard size cap: `resp.bytes()` buffered the whole
    /// response into memory unbounded, so a compromised or misbehaving
    /// marketplace could OOM the server with an oversized (or endless) payload.
    /// A WASM plugin is comfortably under the cap.
    ///
    /// These bytes are **not** authenticated here. The caller must run them
    /// past the signature check before they touch disk — see
    /// `verify_plugin_signature` in the server's marketplace routes (audit
    /// item 8).
    pub async fn download(&self, name: &str) -> Result<Vec<u8>, String> {
        /// 50 MiB — generous for a WASM plugin, bounds worst-case memory.
        const MAX_PLUGIN_BYTES: usize = 50 * 1024 * 1024;

        let url = format!(
            "{}/api/v1/plugins/{}/download",
            self.base_url,
            urlencoding::encode(name)
        );
        let client = crate::http::client::long_timeout();

        let mut resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("plugin download request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("plugin download failed: {}", resp.status()));
        }

        // Reject early if the advertised length already exceeds the cap.
        if let Some(len) = resp.content_length() {
            if len > MAX_PLUGIN_BYTES as u64 {
                return Err(format!(
                    "plugin too large: {len} bytes (max {MAX_PLUGIN_BYTES})"
                ));
            }
        }

        // Stream with a running cap so a missing/lying Content-Length can't
        // blow past the limit either.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("failed to read plugin bytes: {e}"))?
        {
            if bytes.len() + chunk.len() > MAX_PLUGIN_BYTES {
                return Err(format!(
                    "plugin exceeds maximum size of {MAX_PLUGIN_BYTES} bytes"
                ));
            }
            bytes.extend_from_slice(&chunk);
        }

        info!(plugin = %name, size = bytes.len(), "marketplace_plugin_downloaded");
        Ok(bytes)
    }

    /// Vote for a plugin (up or down).
    pub async fn vote(&self, name: &str, up: bool) -> Result<(), String> {
        let url = format!(
            "{}/api/v1/plugins/{}/vote",
            self.base_url,
            urlencoding::encode(name)
        );
        let client = crate::http::client::shared();

        let resp = client
            .post(&url)
            .json(&serde_json::json!({ "up": up }))
            .send()
            .await
            .map_err(|e| format!("plugin vote request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("plugin vote failed: {}", resp.status()));
        }

        info!(plugin = %name, up, "marketplace_plugin_voted");
        Ok(())
    }
}

impl Default for PluginMarketplace {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url() {
        let mp = PluginMarketplace::default();
        assert!(mp.base_url.contains("mozaiklabs.fr"));
    }

    #[test]
    fn custom_base_url() {
        let mp = PluginMarketplace::new(Some("http://localhost:3000/"));
        assert_eq!(mp.base_url, "http://localhost:3000");
    }

    /// Extrait fidèle de `GET /api/v1/plugins` (mozaiklabs.fr, 2026-08-29) :
    /// deux fiches de l'ère Python, une fiche WebAssembly. Les valeurs sont
    /// recopiées telles quelles depuis le relevé — `install_type` et
    /// `display_name` compris. Le relevé complet portait 25 fiches : 24 en
    /// `platforms: "python"`, une seule en `"wasm"`.
    const CATALOGUE_RELEVE: &str = r#"[
      {"name":"tune-plugin-dj","slug":"dj-mode","display_name":"Mode DJ",
       "install_source":"pip install tune-plugin-dj","install_type":"builtin",
       "platforms":"python","install_count":0,"vote_score":0,"vote_count":0},
      {"name":"tune-plugin-lyrics","slug":"lyrics","display_name":"Synchronized Lyrics",
       "install_source":"pip install tune-plugin-lyrics","install_type":"core",
       "platforms":"python","install_count":0,"vote_score":0,"vote_count":0},
      {"name":"tune-plugin-party","slug":"party-mode","display_name":"Mode Party",
       "install_source":"pip install tune-plugin-party","install_type":"store",
       "platforms":"wasm","install_count":0,"vote_score":0,"vote_count":0}
    ]"#;

    fn catalogue_releve() -> Vec<MarketplacePlugin> {
        serde_json::from_str(CATALOGUE_RELEVE).expect("le relevé doit se désérialiser")
    }

    #[test]
    fn les_fiches_de_l_ere_python_ne_sont_pas_installables() {
        let catalogue = catalogue_releve();
        assert_eq!(catalogue.len(), 3, "le relevé porte bien trois fiches");

        let gardees: Vec<&str> = catalogue
            .iter()
            .filter(|p| p.is_installable())
            .map(|p| p.slug.as_str())
            .collect();

        assert_eq!(
            gardees,
            vec!["party-mode"],
            "seule la fiche WebAssembly survit au tri"
        );
    }

    #[test]
    fn install_type_n_est_pas_le_critere_du_tri() {
        // `install_type` nomme le canal de distribution, pas le moteur : les
        // fiches mortes sont `builtin`/`core`, la vivante est `store`. Le tri
        // ne s'appuie pas dessus, sans quoi un plugin wasm publié en `core`
        // disparaîtrait du magasin.
        let catalogue = catalogue_releve();
        let dj = &catalogue[0];
        let party = &catalogue[2];
        assert_eq!(dj.install_type.as_deref(), Some("builtin"));
        assert_eq!(party.install_type.as_deref(), Some("store"));
        assert!(!dj.is_installable());
        assert!(party.is_installable());

        let wasm_en_core: MarketplacePlugin = serde_json::from_str(
            r#"{"name":"x","slug":"x","platforms":"wasm","install_type":"core"}"#,
        )
        .expect("désérialisation");
        assert!(
            wasm_en_core.is_installable(),
            "le canal de distribution ne doit pas décider de l'installabilité"
        );
    }

    #[test]
    fn une_fiche_sans_plateforme_declaree_est_gardee() {
        // Un champ absent ne doit pas faire disparaître un plugin : le
        // catalogue a le droit d'ajouter une fiche sans `platforms`.
        let sans_champ: MarketplacePlugin =
            serde_json::from_str(r#"{"name":"x","slug":"x"}"#).expect("désérialisation");
        assert!(sans_champ.platforms.is_none());
        assert!(sans_champ.is_installable());

        let champ_vide: MarketplacePlugin =
            serde_json::from_str(r#"{"name":"x","slug":"x","platforms":"  "}"#)
                .expect("désérialisation");
        assert!(champ_vide.is_installable());
    }

    /// Sert `CATALOGUE_RELEVE` une seule fois, sur un port local, puis rend
    /// l'URL de base à donner au client.
    ///
    /// Le mock **lit la requête jusqu'à `\r\n\r\n` avant d'écrire**, puis
    /// ferme proprement : fermer sans lire déclenche un RST qui détruit la
    /// réponse en vol (cause réelle de l'instabilité du test de #1358).
    async fn catalogue_servi_en_local() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local");
        let port = listener.local_addr().expect("addr").port();

        tokio::spawn(async move {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };

            // 1. lire la requête en entier
            let mut recu = Vec::new();
            let mut tampon = [0u8; 1024];
            while !recu.windows(4).any(|f| f == b"\r\n\r\n") {
                match socket.read(&mut tampon).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => recu.extend_from_slice(&tampon[..n]),
                }
            }

            // 2. répondre, puis fermer proprement
            let corps = CATALOGUE_RELEVE.as_bytes();
            let entetes = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                corps.len()
            );
            let _ = socket.write_all(entetes.as_bytes()).await;
            let _ = socket.write_all(corps).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });

        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn list_ecarte_les_fiches_python_du_catalogue_servi() {
        let base = catalogue_servi_en_local().await;
        let plugins = PluginMarketplace::new(Some(&base)).list().await;

        let slugs: Vec<&str> = plugins.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["party-mode"],
            "le tri doit être branché dans `list()`, pas seulement disponible"
        );
    }

    #[tokio::test]
    async fn detail_ne_rend_pas_une_fiche_python() {
        let base = catalogue_servi_en_local().await;
        assert!(
            PluginMarketplace::new(Some(&base))
                .detail("dj-mode")
                .await
                .is_none(),
            "une fiche de l'ère Python ne doit plus être proposée à l'installation"
        );
    }

    #[test]
    fn une_liste_de_plateformes_contenant_wasm_est_gardee() {
        for declaration in ["wasm", "WASM", "python,wasm", "linux wasm", "wasm; python"] {
            let fiche: MarketplacePlugin = serde_json::from_str(&format!(
                r#"{{"name":"x","slug":"x","platforms":"{declaration}"}}"#
            ))
            .expect("désérialisation");
            assert!(fiche.is_installable(), "`{declaration}` mentionne wasm");
        }

        for declaration in ["python", "python,linux", "wasmer", "no-wasm-here"] {
            let fiche: MarketplacePlugin = serde_json::from_str(&format!(
                r#"{{"name":"x","slug":"x","platforms":"{declaration}"}}"#
            ))
            .expect("désérialisation");
            assert!(
                !fiche.is_installable(),
                "`{declaration}` ne déclare aucune plateforme wasm"
            );
        }
    }
}
