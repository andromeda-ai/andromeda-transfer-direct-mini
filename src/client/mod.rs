use std::error::Error;
use std::path::Path;
use std::sync::{Arc, Mutex};
use reqwest::{Client, StatusCode, Body};
use tokio::fs;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use futures_util::StreamExt;
use tokio::signal;
use tokio::sync::broadcast;
use axum::http::HeaderMap;
use tokio::io::AsyncReadExt;
use crate::FileQueue;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Push,
    Pull,
}

#[derive(Clone)]
pub struct FileTransferClient {
    client: Client,
    server_url: String,
    remap: Option<(String, String)>,
}

impl FileTransferClient {
    pub fn new(server_url: String, remap: Option<String>) -> Self {
        let client = Client::new();
        
        let parsed_remap = remap.map(|r| {
            let parts: Vec<&str> = r.split(':').collect();
            if parts.len() == 2 {
                (parts[0].to_string(), parts[1].to_string())
            } else {
                warn!("Invalid remap format, should be 'source:target'. Ignoring remap.");
                ("".to_string(), "".to_string())
            }
        });
        
        Self { client, server_url, remap: parsed_remap }
    }
    
    fn apply_remap(&self, path: &str, mode: Mode) -> String {
        println!("Applying remap to {} with mode {:?}, remap: {:?}", path, mode, self.remap);
        if let Some((source, target)) = &self.remap {
            if source.is_empty() || target.is_empty() {
                return path.to_string();
            }
            
            match mode {
                Mode::Pull => {
                    println!("Pulling file from {} to {}", path, target);
                    if path.starts_with(source) {
                        println!("CHANGING PATH");
                        return path.replacen(source, target, 1);
                    }
                },
                Mode::Push => {
                    if path.starts_with(target) {
                        return path.replacen(target, source, 1);
                    }
                }
            }
        }
        path.to_string()
    }

    pub async fn grab_work(&self) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/grab_work", self.server_url);
        let resp = self.client.get(&url).send().await?;
        match resp.status() {
            StatusCode::OK => {
                let file_path = resp.text().await?;
                Ok(Some(file_path))
            },
            StatusCode::NO_CONTENT => {
                Ok(None)
            },
            status => {
                Err(format!("Unexpected status code: {}", status).into())
            }
        }
    }

    pub async fn upload_file<P: AsRef<Path>>(&self, source_path: P, remote_path: &str) 
        -> Result<(), Box<dyn Error + Send + Sync>> {
        let source_path = source_path.as_ref();
        if !source_path.exists() {
            return Err(format!("Source file not found: {:?}", source_path).into());
        }

        let file = fs::File::open(source_path).await?;
        let file_size = file.metadata().await?.len();
        let stream = tokio_util::io::ReaderStream::new(file);
        let body = Body::wrap_stream(stream);

        let server_path = self.apply_remap(remote_path, Mode::Push);
        
        let url = format!("{}/upload_file/{}", self.server_url, server_path);
        let resp = self.client.put(&url)
            .body(body)
            .send()
            .await?;

        if resp.status().is_success() {
            debug!("Successfully uploaded file to {} ({} bytes)", server_path, file_size);
            Ok(())
        } else {
            let error_text = resp.text().await?;
            error!("Failed to upload file: {}", error_text);
            Err(format!("Upload failed: {}", error_text).into())
        }
    }

    pub async fn download_file(&self, remote_path: &str, target_path: &str) 
        -> Result<(), Box<dyn Error + Send + Sync>> {
        let url = format!("{}/download_file", self.server_url);
        
        let server_path = remote_path;
        let local_path = target_path;
        println!("Downloading file from {} to {}", server_path, local_path);
        
        let resp = self.client.get(&url)
            .header("X-File-Path", server_path)
            .send()
            .await?;
            
        if resp.status().is_success() {
            let file_size = resp.content_length().unwrap_or(0);
            
            if let Some(parent) = Path::new(&local_path).parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent).await?;
                }
            }
            
            let mut file = fs::File::create(&local_path).await?;
            let mut stream = resp.bytes_stream();
            
            while let Some(chunk_result) = stream.next().await {
                let chunk = chunk_result?;
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            }
            
            debug!("Successfully downloaded file from {} to {} ({} bytes)", server_path, local_path, file_size);
            Ok(())
        } else {
            let error_text = resp.text().await?;
            error!("Failed to download file: {}", error_text);
            Err(format!("Download failed: {}", error_text).into())
        }
    }

    pub fn new_with_tls(server_url: String, remap: Option<String>, use_tls: bool) -> Self {
        let client_builder = reqwest::Client::builder();
        
        let client_builder = if use_tls {
            client_builder.danger_accept_invalid_certs(true)
        } else {
            client_builder
        };
        
        let client = client_builder.build().unwrap_or_else(|e| {
            tracing::error!("Failed to build HTTP client: {}", e);
            reqwest::Client::new()
        });
        
        let parsed_remap = remap.map(|r| {
            let parts: Vec<&str> = r.split(':').collect();
            if parts.len() == 2 {
                (parts[0].to_string(), parts[1].to_string())
            } else {
                tracing::warn!("Invalid remap format, should be 'source:target'. Ignoring remap.");
                ("".to_string(), "".to_string())
            }
        });
        
        Self { client, server_url, remap: parsed_remap }
    }
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server_url: String,
    pub concurrency: usize,
    pub mode: Mode,
    pub remap: Option<String>,
    pub tls: bool,
    pub api_key: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: "http://localhost:7000".to_string(),
            concurrency: 4,
            mode: Mode::Push,
            remap: None,
            tls: false,
            api_key: "".to_string(),
        }
    }
}

async fn make_request(
    client: &Client,
    url: &str,
    headers: Option<HeaderMap>,
    body: Option<Vec<u8>>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut request = client.request(reqwest::Method::GET, url);
    
    if let Some(headers) = headers {
        request = request.headers(headers);
    }
    
    if let Some(body) = body {
        request = request.body(body);
    }
    
    request.send().await
}

pub async fn run_client_parallel(
    server_url: String,
    concurrency: usize,
    mode: Mode,
    remap: Option<String>,
    tls: bool,
    api_key: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = ClientConfig {
        server_url,
        concurrency,
        mode,
        remap,
        tls,
        api_key,
    };
    
    let client = if config.tls {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?
    } else {
        reqwest::Client::new()
    };

    let mut headers = HeaderMap::new();
    headers.insert("X-API-Key", config.api_key.parse()?);

    match config.mode {
        Mode::Push => {
            // ... existing push code ...
            let mut headers = headers.clone();
            headers.insert("Content-Type", "application/octet-stream".parse()?);
            // Use headers in make_request calls
        },
        Mode::Pull => {
            // ... existing pull code ...
            // Use headers in make_request calls
        }
    }

    Ok(())
}

pub async fn run_client(server_url: String, mode: Mode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_client_parallel(server_url, 4, mode, None, false, "".to_string()).await
}

pub async fn run_client_with_remap(
    server_url: String, 
    mode: Mode, 
    concurrency: Option<usize>,
    remap: Option<String>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workers = concurrency.unwrap_or(4);
    run_client_parallel(server_url, workers, mode, remap, false, "".to_string()).await
}

pub async fn run_client_with_remap_tls(
    server_url: String, 
    mode: Mode, 
    concurrency: Option<usize>,
    remap: Option<String>,
    use_tls: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workers = concurrency.unwrap_or(4);
    run_client_parallel(server_url, workers, mode, remap, use_tls, "".to_string()).await
} 
