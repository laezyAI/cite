//! Supabase REST and Storage client with retries (PostgREST embeds one-to-one relations as objects or arrays).

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, RequestBuilder, Response};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::core::CiteError;

pub type Row = Map<String, Value>;

const UPLOAD_ATTEMPTS: u64 = 3;

#[derive(Debug, Clone)]
pub struct Supabase {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    bearer: String,
}

impl Supabase {
    pub fn new(client: reqwest::Client, base_url: &str, api_key: &str, bearer: &str) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            bearer: bearer.to_string(),
        }
    }

    pub fn url(&self) -> &str {
        &self.base_url
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header("apikey", &self.api_key)
            .header("Authorization", format!("Bearer {}", self.bearer))
    }

    pub async fn select(&self, table: &str, query: &str) -> Result<Vec<Value>, CiteError> {
        let response = self
            .request(Method::GET, &format!("/rest/v1/{table}?{query}"))
            .send()
            .await?;
        let response = ensure_success(response, format!("Failed to read {table}")).await?;
        Ok(response.json().await?)
    }

    pub async fn find_id(
        &self,
        table: &str,
        field: &str,
        value: &str,
    ) -> Result<Option<i64>, CiteError> {
        let query = format!("select=id&{field}=eq.{}", encode(value));
        let rows = self.select(table, &query).await?;
        Ok(rows.first().and_then(|row| row["id"].as_i64()))
    }

    pub async fn insert_returning(&self, table: &str, row: &Row) -> Result<Value, CiteError> {
        let response = self
            .request(Method::POST, &format!("/rest/v1/{table}"))
            .header("Prefer", "return=representation")
            .json(row)
            .send()
            .await?;
        let response = ensure_success(response, format!("Failed to insert into {table}")).await?;
        Ok(match response.json().await? {
            Value::Array(mut rows) if !rows.is_empty() => rows.swap_remove(0),
            other => other,
        })
    }

    pub async fn insert(&self, table: &str, row: &Row) -> Result<i64, CiteError> {
        self.insert_returning(table, row).await?["id"]
            .as_i64()
            .ok_or_else(|| CiteError::Deploy(format!("Could not get {table} id from response")))
    }

    pub async fn update(&self, table: &str, id: i64, row: &Row) -> Result<(), CiteError> {
        let response = self
            .request(Method::PATCH, &format!("/rest/v1/{table}?id=eq.{id}"))
            .json(row)
            .send()
            .await?;
        ensure_success(response, format!("Failed to update {table} {id}")).await?;
        Ok(())
    }

    pub async fn upsert(&self, table: &str, row: &Row, on_conflict: &str) -> Result<(), CiteError> {
        let response = self
            .request(
                Method::POST,
                &format!("/rest/v1/{table}?on_conflict={on_conflict}"),
            )
            .header("Prefer", "resolution=merge-duplicates")
            .json(row)
            .send()
            .await?;
        ensure_success(response, format!("Failed to upsert into {table}")).await?;
        Ok(())
    }

    pub async fn delete(&self, table: &str, id: i64) -> Result<(), CiteError> {
        self.delete_where(table, &format!("id=eq.{id}")).await
    }

    pub async fn delete_where(&self, table: &str, filter: &str) -> Result<(), CiteError> {
        let response = self
            .request(Method::DELETE, &format!("/rest/v1/{table}?{filter}"))
            .send()
            .await?;
        ensure_success(
            response,
            format!("Failed to delete from {table} ({filter})"),
        )
        .await?;
        Ok(())
    }

    pub async fn upload(
        &self,
        bucket: &str,
        object_path: &str,
        bytes: &[u8],
        mime: &str,
    ) -> Result<String, CiteError> {
        let path = format!("/storage/v1/object/{bucket}/{object_path}");
        let mut last_err = String::new();
        for attempt in 1..=UPLOAD_ATTEMPTS {
            let response = self
                .request(Method::POST, &path)
                .header("Content-Type", mime)
                .header("x-upsert", "true")
                .body(bytes.to_vec())
                .send()
                .await;
            match response {
                Ok(r) if r.status().is_success() => {
                    let storage_path = format!("{bucket}/{object_path}");
                    info!("Uploaded {storage_path}");
                    return Ok(storage_path);
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    last_err = format!("HTTP {status} - {body}");
                }
                Err(e) => last_err = e.to_string(),
            }
            warn!("Upload attempt {attempt}/{UPLOAD_ATTEMPTS} failed: {last_err}");
            if attempt < UPLOAD_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
            }
        }
        Err(CiteError::Deploy(format!(
            "Failed to upload {object_path} after {UPLOAD_ATTEMPTS} attempts: {last_err}"
        )))
    }

    pub async fn delete_object(&self, storage_path: &str) -> Result<(), CiteError> {
        let response = self
            .request(
                Method::DELETE,
                &format!("/storage/v1/object/{storage_path}"),
            )
            .send()
            .await?;
        ensure_success(
            response,
            format!("Failed to delete storage object {storage_path}"),
        )
        .await?;
        Ok(())
    }
}

pub fn row<const N: usize>(fields: [(&str, Value); N]) -> Row {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

pub fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

async fn ensure_success(
    response: Response,
    context: impl std::fmt::Display,
) -> Result<Response, CiteError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(CiteError::Deploy(format!(
        "{context}: HTTP {status} - {body}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client(server: &MockServer) -> Supabase {
        Supabase::new(reqwest::Client::new(), &server.base_url(), "key", "jwt")
    }

    #[tokio::test]
    async fn test_find_id_encodes_the_filter_and_sends_auth_headers() {
        let server = MockServer::start();
        let found = server.mock(|w, t| {
            w.method(GET)
                .path("/rest/v1/urls")
                .query_param("url", "eq.https://x.y/a?b=c&d")
                .header("apikey", "key")
                .header("Authorization", "Bearer jwt");
            t.status(200).json_body(serde_json::json!([{ "id": 7 }]));
        });
        let id = client(&server)
            .find_id("urls", "url", "https://x.y/a?b=c&d")
            .await
            .unwrap();
        assert_eq!(id, Some(7));
        found.assert();
    }

    #[tokio::test]
    async fn test_failed_request_reports_status_and_body() {
        let server = MockServer::start();
        server.mock(|w, t| {
            w.method(POST).path("/rest/v1/news");
            t.status(403)
                .body("new row violates row-level security policy");
        });
        let err = client(&server)
            .insert("news", &row([("title", Value::from("T"))]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to insert into news"), "{err}");
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("row-level security"), "{err}");
    }

    #[tokio::test]
    async fn test_upload_retries_then_gives_up() {
        let server = MockServer::start();
        let attempts = server.mock(|w, t| {
            w.method(POST)
                .path("/storage/v1/object/podcasts/artist/ep.mp3")
                .header("x-upsert", "true");
            t.status(500).body("busy");
        });
        let err = client(&server)
            .upload("podcasts", "artist/ep.mp3", b"mp3", "audio/mpeg")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(attempts.hits(), UPLOAD_ATTEMPTS as usize);
        assert!(
            err.contains("after 3 attempts") && err.contains("busy"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_upload_returns_the_storage_path_the_app_resolves() {
        let server = MockServer::start();
        server.mock(|w, t| {
            w.method(POST)
                .path("/storage/v1/object/assets/artist/n.png");
            t.status(200);
        });
        let path = client(&server)
            .upload("assets", "artist/n.png", b"png", "image/png")
            .await
            .unwrap();
        assert_eq!(path, "assets/artist/n.png");
    }
}
