//! The host's theme library and Open VSX catalog. Definitions are resolved by
//! the desktop's theme parser before they can paint or become a selected theme.

use reqwest::{Method, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;

pub const MAX_THEME_BYTES: usize = 512 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("the theme already exists with different content")]
    Conflict,
    #[error("the theme exceeds the size limit")]
    Size,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl ThemeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Size => "size",
            Self::Transport(_) => "save",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ThemeExtension {
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub label: String,
    pub icon: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThemeSource {
    pub path: String,
    pub name: String,
    pub text: String,
    pub error: bool,
}

/// Per-instance selections share the canonical settings document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_system_theme: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_theme_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark_theme_id: Option<String>,
}

fn malformed() -> TransportError {
    TransportError::Malformed("theme response".into())
}

impl ServerClient {
    pub async fn theme_preferences(&self) -> Result<ThemePreferences, ThemeError> {
        self.theme_request(Method::GET, self.settings_url(), None)
            .await
    }

    pub async fn save_theme_preferences(
        &self,
        preferences: &ThemePreferences,
    ) -> Result<ThemePreferences, ThemeError> {
        let saved: ThemePreferences = self
            .theme_request(Method::PUT, self.settings_url(), Some(json!(preferences)))
            .await?;
        if saved != *preferences {
            return Err(malformed().into());
        }
        Ok(saved)
    }

    async fn theme_request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<T, ThemeError> {
        let mut request = self
            .authorized(method, url)
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(TransportError::from_request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                409 => ThemeError::Conflict,
                413 => ThemeError::Size,
                401 => TransportError::AuthenticationRequired.into(),
                300..=399 => TransportError::Redirected.into(),
                status => TransportError::Http { status }.into(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(malformed().into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(TransportError::from_request)?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(malformed().into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| malformed().into())
    }

    pub async fn custom_themes(&self) -> Result<Vec<Value>, ThemeError> {
        #[derive(Deserialize)]
        struct Response {
            themes: Vec<Value>,
        }
        let response: Response = self
            .theme_request(Method::GET, self.endpoint().url("api/config/themes"), None)
            .await?;
        Ok(response.themes)
    }

    pub async fn import_theme(&self, definition: &Value) -> Result<Value, ThemeError> {
        #[derive(Deserialize)]
        struct Response {
            theme: Value,
        }
        let response: Response = self
            .theme_request(
                Method::POST,
                self.endpoint().url("api/config/themes"),
                Some(json!({"theme": definition})),
            )
            .await?;
        Ok(response.theme)
    }

    pub async fn delete_theme(&self, id: &str) -> Result<(), ThemeError> {
        #[derive(Deserialize)]
        struct Response {
            success: bool,
        }
        let mut url = self.endpoint().url("api/config/themes");
        url.path_segments_mut().expect("HTTP URL").push(id);
        let response: Response = self.theme_request(Method::DELETE, url, None).await?;
        if !response.success {
            return Err(malformed().into());
        }
        Ok(())
    }

    pub async fn search_theme_catalog(
        &self,
        query: &str,
    ) -> Result<Vec<ThemeExtension>, ThemeError> {
        #[derive(Deserialize)]
        struct Response {
            items: Vec<ThemeExtension>,
        }
        let response: Response = self
            .theme_request(
                Method::POST,
                self.endpoint().url("api/config/themes/catalog/search"),
                Some(json!({"query": query})),
            )
            .await?;
        if response.items.len() > 24
            || response.items.iter().any(|item| {
                item.icon.as_ref().is_some_and(|icon| {
                    Url::parse(icon).map_or(true, |url| {
                        url.scheme() != "https"
                            || !matches!(
                                url.host_str(),
                                Some("open-vsx.org" | "openvsx.eclipsecontent.org")
                            )
                    })
                })
            })
        {
            return Err(malformed().into());
        }
        Ok(response.items)
    }

    pub async fn theme_package(
        &self,
        extension: &ThemeExtension,
    ) -> Result<Vec<ThemeSource>, ThemeError> {
        #[derive(Deserialize)]
        struct Response {
            items: Vec<ThemeSource>,
        }
        let response: Response = self
            .theme_request(
                Method::POST,
                self.endpoint().url("api/config/themes/catalog/package"),
                Some(json!(extension)),
            )
            .await?;
        if response.items.len() > 40 {
            return Err(malformed().into());
        }
        Ok(response.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preference_writes_use_the_canonical_settings_contract() {
        let (client, log) = crate::test_server::serve(Box::new(|request| {
            let mut saved = serde_json::Map::new();
            for key in [
                "themeId",
                "themeVariant",
                "useSystemTheme",
                "lightThemeId",
                "darkThemeId",
            ] {
                if let Some(value) = request.body.get(key) {
                    saved.insert(key.into(), value.clone());
                }
            }
            (200, Value::Object(saved))
        }))
        .await;
        let preferences = ThemePreferences {
            use_system_theme: Some(false),
            theme_variant: Some("dark".into()),
            theme_id: Some("custom-dark".into()),
            light_theme_id: Some("custom-light".into()),
            dark_theme_id: Some("custom-dark".into()),
        };
        assert_eq!(
            client
                .save_theme_preferences(&preferences)
                .await
                .expect("save preferences"),
            preferences
        );
        let log = log.lock().expect("request log");
        assert_eq!(log[0].method, "PUT");
        // The shared recorder retains Url::path(), without the query.
        assert_eq!(log[0].path, "/api/config/settings");
        assert_eq!(client.settings_url().query(), Some("surface=desktop"));
        assert_eq!(
            log[0].body,
            json!({"useSystemTheme":false,"themeVariant":"dark","themeId":"custom-dark","lightThemeId":"custom-light","darkThemeId":"custom-dark"})
        );
    }

    #[tokio::test]
    async fn silently_dropped_preference_updates_are_failed_saves() {
        let (client, _) = crate::test_server::serve(Box::new(|_| (200, json!({})))).await;
        assert!(
            client
                .save_theme_preferences(&ThemePreferences {
                    use_system_theme: Some(true),
                    ..ThemePreferences::default()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn themes_keep_methods_bodies_and_encoded_identity() {
        let (client, log) =
            crate::test_server::serve(Box::new(|request| match request.method.as_str() {
                "GET" => (200, json!({"themes":[{"metadata":{"id":"custom"}}]})),
                "POST" if request.path.ends_with("search") => (200, json!({"items":[]})),
                "POST" => (201, json!({"theme":request.body["theme"]})),
                "DELETE" => (200, json!({"success":true})),
                _ => panic!("unexpected request"),
            }))
            .await;
        assert_eq!(
            client.custom_themes().await.expect("custom themes").len(),
            1
        );
        let definition = json!({"metadata":{"name":"A"},"colors":{}});
        assert_eq!(
            client
                .import_theme(&definition)
                .await
                .expect("import theme"),
            definition
        );
        client.delete_theme("a/b ?#").await.expect("delete theme");
        assert!(
            client
                .search_theme_catalog("nord")
                .await
                .expect("catalog search")
                .is_empty()
        );
        let log = log.lock().expect("request log");
        assert_eq!(log[1].body, json!({"theme":definition}));
        assert_eq!(log[2].path, "/api/config/themes/a%2Fb%20%3F%23");
        assert_eq!(log[3].body, json!({"query":"nord"}));
    }

    #[tokio::test]
    async fn malformed_and_refused_results_are_never_empty_success() {
        let (client, _) =
            crate::test_server::serve(Box::new(|request| match request.method.as_str() {
                "GET" => (200, json!({"themes":null})),
                "DELETE" => (200, json!({"success":false})),
                _ => (409, json!({"error":"conflict"})),
            }))
            .await;
        assert!(client.custom_themes().await.is_err());
        assert!(client.delete_theme("custom").await.is_err());
        assert!(matches!(
            client.import_theme(&json!({})).await,
            Err(ThemeError::Conflict)
        ));
    }
}
