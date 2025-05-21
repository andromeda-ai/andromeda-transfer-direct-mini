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

pub async fn run_client_parallel(
    server_url: String, 
    concurrency: usize, 
    mode: Mode,
    remap: Option<String>,
    use_tls: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server_url = if use_tls && !server_url.starts_with("https://") {
        if server_url.starts_with("http://") {
            server_url.replace("http://", "https://")
        } else {
            format!("https://{}", server_url)
        }
    } else if !use_tls && !server_url.starts_with("http://") {
        if server_url.starts_with("https://") {
            server_url.replace("https://", "http://")
        } else {
            format!("http://{}", server_url)
        }
    } else {
        server_url
    };

    info!("Client initialized with {} concurrent workers, mode: {:?}, TLS: {}", concurrency, mode, use_tls);
    
    if let Some(remap_str) = &remap {
        info!("Path remapping enabled: {}", remap_str);
    }
    
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                info!("Ctrl+C received, initiating graceful shutdown. Waiting for pending transfers to complete...");
                let _ = shutdown_tx_clone.send(());
            }
            Err(err) => {
                error!("Failed to listen for shutdown signal: {}", err);
            }
        }
    });
    
    let processed_count = Arc::new(Mutex::new(0));
    let mut join_set = JoinSet::new();

    for worker_id in 0..concurrency {
        let worker_url = server_url.clone();
        let worker_processed_count = Arc::clone(&processed_count);
        let worker_mode = mode;
        let worker_remap = remap.clone();
        let worker_use_tls = use_tls;
        let mut shutdown_rx = shutdown_tx.subscribe();
        
        join_set.spawn(async move {
            let client = if worker_use_tls {
                FileTransferClient::new_with_tls(worker_url, worker_remap, worker_use_tls)
            } else {
                FileTransferClient::new(worker_url, worker_remap)
            };
            debug!("Worker {}: started", worker_id);
            
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    debug!("Worker {}: shutdown requested, finishing current task and exiting", worker_id);
                    break;
                }
                
                match client.grab_work().await {
                    Ok(Some(file_path)) => {
                        let is_log_file = {
                            let count = *worker_processed_count.lock().unwrap();
                            count % 100 == 0
                        };
                        
                        if is_log_file {
                            debug!("Worker {}: Processing file: {}", worker_id, file_path);
                        }
                        
                        let remapped_file_path = client.apply_remap(&file_path, worker_mode);
                        match worker_mode {
                            Mode::Push => {
                                let source_path = Path::new(&remapped_file_path);
                                if !source_path.exists() {
                                    warn!("Worker {}: Source file not found: {}", worker_id, remapped_file_path);
                                    continue;
                                }
                                
                                match client.upload_file(source_path, &remapped_file_path).await {
                                    Ok(_) => {
                                        if is_log_file {
                                            debug!("Worker {}: Uploaded file to {}", worker_id, remapped_file_path);
                                        }
                                    },
                                    Err(e) => error!("Worker {}: Failed to upload file: {}", worker_id, e),
                                }
                            },
                            Mode::Pull => {
                                match client.download_file(&file_path, &remapped_file_path).await {
                                    Ok(_) => {
                                        if is_log_file {
                                            debug!("Worker {}: Downloaded file to {}", worker_id, file_path);
                                        }
                                    },
                                    Err(e) => error!("Worker {}: Failed to download file: {}", worker_id, e),
                                }
                            }
                        }
                        
                        let mut count = worker_processed_count.lock().unwrap();
                        *count += 1;
                        if *count % 100 == 0 {
                            info!("Progress: {} files processed", *count);
                        }
                    },
                    Ok(None) => {
                        debug!("Worker {}: No more work available, finishing", worker_id);
                        break;
                    },
                    Err(e) => {
                        error!("Worker {}: Error fetching work: {}", worker_id, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            }
            
            debug!("Worker {}: finished", worker_id);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
    }

    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            error!("Worker task failed: {}", e);
        }
    }

    let total_processed = *processed_count.lock().unwrap();
    info!("Processing complete. Total files processed: {}", total_processed);
    Ok(())
}

pub async fn run_client(server_url: String, mode: Mode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_client_parallel(server_url, 4, mode, None, false).await
}

pub async fn run_client_with_remap(
    server_url: String, 
    mode: Mode, 
    concurrency: Option<usize>,
    remap: Option<String>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workers = concurrency.unwrap_or(4);
    run_client_parallel(server_url, workers, mode, remap, false).await
}

pub async fn run_client_with_remap_tls(
    server_url: String, 
    mode: Mode, 
    concurrency: Option<usize>,
    remap: Option<String>,
    use_tls: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workers = concurrency.unwrap_or(4);
    run_client_parallel(server_url, workers, mode, remap, use_tls).await
} 