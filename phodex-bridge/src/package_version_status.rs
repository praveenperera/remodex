use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct BridgeVersionInfo {
    pub bridge_version: Option<String>,
    pub bridge_latest_version: Option<String>,
}

#[derive(Clone)]
pub struct BridgePackageVersionStatusReader {
    inner: Arc<Mutex<PackageVersionCache>>,
    http: Client,
}

#[derive(Debug, Default)]
struct PackageVersionCache {
    latest_version: Option<String>,
    last_successful_resolve_at: Option<Instant>,
    last_attempted_at: Option<Instant>,
}

impl BridgePackageVersionStatusReader {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PackageVersionCache::default())),
            http: Client::builder()
                .timeout(Duration::from_secs(4))
                .build()
                .unwrap(),
        }
    }

    pub async fn read(&self) -> BridgeVersionInfo {
        let latest = {
            let cache = self.inner.lock().unwrap();
            if cache
                .last_successful_resolve_at
                .map(|at| at.elapsed() < Duration::from_secs(30 * 60))
                .unwrap_or(false)
            {
                cache.latest_version.clone()
            } else {
                None
            }
        };

        let resolved_latest = if latest.is_some() {
            latest
        } else {
            self.refresh_latest().await
        };

        BridgeVersionInfo {
            bridge_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            bridge_latest_version: resolved_latest,
        }
    }

    async fn refresh_latest(&self) -> Option<String> {
        {
            let mut cache = self.inner.lock().unwrap();
            let recently_attempted = cache
                .last_attempted_at
                .map(|at| at.elapsed() < Duration::from_secs(60))
                .unwrap_or(false);
            if recently_attempted {
                return cache.latest_version.clone();
            }
            cache.last_attempted_at = Some(Instant::now());
        }

        let latest_version = self
            .http
            .get("https://registry.npmjs.org/remodex/latest")
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()?
            .get("version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        if let Some(version) = latest_version.clone() {
            let mut cache = self.inner.lock().unwrap();
            cache.latest_version = Some(version.clone());
            cache.last_successful_resolve_at = Some(Instant::now());
        }

        latest_version
    }
}
