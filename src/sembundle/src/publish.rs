use std::path::PathBuf;

use reqwest::blocking::Client;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("bundle file not found: {0}")]
    FileNotFound(PathBuf),
    #[error("file does not have .sembundle extension: {0}")]
    InvalidExtension(PathBuf),
    #[error("HTTP error {status}: {body}")]
    HttpError { status: u16, body: String },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("token is required (use --token or set SemBundle_TOKEN env var)")]
    MissingToken,
    #[error("registry URL is required (use --registry or set SemBundle_REGISTRY_URL env var)")]
    MissingRegistry,
}

pub struct PublishOptions {
    pub bundle_path: PathBuf,
    pub registry_url: String,
    pub token: String,
}

pub fn publish(opts: PublishOptions) -> Result<(String, String), PublishError> {
    // Validate path exists
    if !opts.bundle_path.exists() {
        return Err(PublishError::FileNotFound(opts.bundle_path));
    }

    // Validate .sembundle extension
    match opts.bundle_path.extension().and_then(|e| e.to_str()) {
        Some("sembundle") => {}
        _ => return Err(PublishError::InvalidExtension(opts.bundle_path)),
    }

    // Extract filename for multipart part
    let filename = opts
        .bundle_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bundle.sembundle")
        .to_string();

    // Read file bytes
    let bytes = std::fs::read(&opts.bundle_path)?;

    // Build multipart form
    let part = reqwest::blocking::multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str("application/octet-stream")
        .map_err(PublishError::Network)?;
    let form = reqwest::blocking::multipart::Form::new().part("file", part);

    // POST to registry
    let url = format!("{}/publish", opts.registry_url.trim_end_matches('/'));
    let client = Client::new();
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", opts.token))
        .multipart(form)
        .send()?;

    let status = response.status();
    let body = response.text().unwrap_or_default();

    if !status.is_success() {
        return Err(PublishError::HttpError {
            status: status.as_u16(),
            body,
        });
    }

    // Parse JSON response for name and version
    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let name = json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok((name, version))
}
