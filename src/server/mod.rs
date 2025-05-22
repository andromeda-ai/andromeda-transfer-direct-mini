use std::collections::{VecDeque, HashSet};
use std::fs::File as StdFile;
use std::io::{self, Read, Write, BufWriter};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Router,
    middleware::{self, Next},
};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Arg, Command};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info};
use crate::FileQueue;

#[derive(Clone, Debug)]
pub struct AppConfig {
    target_dir: String,
    downloaded_files_path: String,
    uploaded_files_path: String,
    api_key: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            target_dir: ".".to_string(),
            downloaded_files_path: "downloaded_files.txt".to_string(),
            uploaded_files_path: "uploaded_files.txt".to_string(),
            api_key: "".to_string(),
        }
    }
}

pub fn parse_args() -> AppConfig {
    let matches = Command::new("file-transfer")
        .version("1.0")
        .about("File transfer server")
        .arg(
            Arg::new("target-dir")
                .short('d')
                .long("target-dir")
                .value_name("DIR")
                .help("Target directory to scan for files")
                .required(true)
                .num_args(1),
        )
        .arg(
            Arg::new("downloaded-files")
                .short('f')
                .long("downloaded-files")
                .value_name("FILE")
                .help("Path to file tracking downloaded files")
                .default_value("downloaded_files.txt")
                .num_args(1),
        )
        .arg(
            Arg::new("uploaded-files")
                .short('u')
                .long("uploaded-files")
                .value_name("FILE")
                .help("Path to file tracking uploaded files")
                .default_value("uploaded_files.txt")
                .num_args(1),
        )
        .arg(
            Arg::new("api-key")
                .long("api-key")
                .value_name("KEY")
                .help("API key for authentication")
                .required(true)
                .num_args(1),
        )
        .get_matches();

    AppConfig {
        target_dir: matches.get_one::<String>("target-dir").unwrap().clone(),
        downloaded_files_path: matches.get_one::<String>("downloaded-files").unwrap().clone(),
        uploaded_files_path: matches.get_one::<String>("uploaded-files").unwrap().clone(),
        api_key: matches.get_one::<String>("api-key").unwrap().clone(),
    }
}

fn build_manifest(target_dir: &str) -> VecDeque<String> {
    info!("Building new manifest file from directory: {}", target_dir);
    let mut files = VecDeque::new();
    let start = Instant::now();
    
    info!("Counting files for progress estimation...");
    let count_bar = ProgressBar::new_spinner();
    count_bar.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    count_bar.set_message("Counting files...");
    
    let mut total_files = 0;
    for entry_result in WalkDir::new(target_dir)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .into_iter() {
        if let Ok(entry) = entry_result {
            if entry.file_type.is_file() {
                total_files += 1;
                if total_files % 1000 == 0 {
                    count_bar.set_message(format!("Counted {} files...", total_files));
                }
            }
        }
    }
    count_bar.finish_with_message(format!("Found {} files", total_files));
    
    let bar = ProgressBar::new(total_files);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .unwrap()
            .progress_chars("#>-")
    );
    bar.set_message("Scanning files...");
    
    let walker = WalkDir::new(target_dir)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false);
        
    for entry_result in walker.into_iter() {
        if let Ok(entry) = entry_result {
            if entry.file_type.is_file() {
                let path_str = entry.path().to_string_lossy().to_string();
                files.push_back(path_str);
                bar.inc(1);
                if files.len() % 1000 == 0 {
                    bar.set_message(format!("Found {} files", files.len()));
                }
            }
        }
    }
    
    bar.finish_with_message(format!("Scanned {} files", files.len()));
    
    let write_bar = ProgressBar::new(files.len() as u64);
    write_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .unwrap()
            .progress_chars("#>-")
    );
    write_bar.set_message("Writing manifest file...");
    
    const BATCH_SIZE: usize = 100_000;
    
    if let Ok(file) = StdFile::create("manifest.txt") {
        let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
        let mut count = 0;
        let mut batch = String::with_capacity(BATCH_SIZE * 100);
        
        for path in &files {
            batch.push_str(path);
            batch.push('\n');
            
            count += 1;
            
            if count % BATCH_SIZE == 0 {
                if let Err(e) = writer.write_all(batch.as_bytes()) {
                    error!("Failed to write batch to manifest.txt: {}", e);
                    break;
                }
                batch.clear();
                write_bar.set_position(count as u64);
            }
        }
        
        if !batch.is_empty() {
            if let Err(e) = writer.write_all(batch.as_bytes()) {
                error!("Failed to write final batch to manifest.txt: {}", e);
            }
        }
        
        if let Err(e) = writer.flush() {
            error!("Failed to flush manifest.txt: {}", e);
        }
        
        write_bar.finish_with_message("Manifest file written");
    } else {
        write_bar.finish_with_message("Failed to create manifest file");
        error!("Failed to create manifest.txt");
    }
    
    info!("Built manifest with {} files in {:?}", files.len(), start.elapsed());
    files
}

fn load_file_set(path: &str) -> HashSet<String> {
    let mut file_set = HashSet::new();
    if Path::new(path).exists() {
        if let Ok(mut file) = StdFile::open(path) {
            let mut content = String::new();
            if file.read_to_string(&mut content).is_ok() {
                for line in content.lines() {
                    if !line.trim().is_empty() {
                        file_set.insert(line.to_string());
                    }
                }
            }
        }
    } else {
        if let Err(e) = StdFile::create(path) {
            error!("Failed to create file list at {}: {}", path, e);
        } else {
            info!("Created new file list at {}", path);
        }
    }
    info!("Loaded {} files from {}", file_set.len(), path);
    file_set
}

fn save_file_set(file_set: &HashSet<String>, path: &str) -> io::Result<()> {
    let file = StdFile::create(path)?;
    let mut writer = BufWriter::new(file);
    for file_path in file_set {
        writeln!(writer, "{}", file_path)?;
    }
    writer.flush()?;
    info!("Saved {} files to {}", file_set.len(), path);
    Ok(())
}

fn load_downloaded_files(path: &str) -> HashSet<String> {
    load_file_set(path)
}

fn save_downloaded_files(downloaded: &HashSet<String>, path: &str) -> io::Result<()> {
    save_file_set(downloaded, path)
}

fn load_uploaded_files(path: &str) -> HashSet<String> {
    load_file_set(path)
}

fn save_uploaded_files(uploaded: &HashSet<String>, path: &str) -> io::Result<()> {
    save_file_set(uploaded, path)
}

pub fn load_file_list(config: &AppConfig) -> io::Result<VecDeque<String>> {
    let manifest_path = "manifest.txt";
    let downloaded = load_downloaded_files(&config.downloaded_files_path);
    
    if Path::new(manifest_path).exists() {
        let mut files = VecDeque::new();
        let mut manifest = String::new();
        StdFile::open(manifest_path)?.read_to_string(&mut manifest)?;
        for line in manifest.lines() {
            if !line.trim().is_empty() && !downloaded.contains(line) {
                files.push_back(line.to_string());
            }
        }
        info!("Loaded {} files from manifest (excluding {} already downloaded)", 
              files.len(), downloaded.len());
        Ok(files)
    } else {
        info!("Manifest file not found, building new one");
        let all_files = build_manifest(&config.target_dir);
        let mut remaining_files = VecDeque::new();
        for file in all_files {
            if !downloaded.contains(&file) {
                remaining_files.push_back(file);
            }
        }
        info!("Filtered manifest has {} files (excluded {} already downloaded)", 
              remaining_files.len(), downloaded.len());
        Ok(remaining_files)
    }
}

#[derive(Clone)]
struct AppState {
    queue: FileQueue,
    downloaded: Arc<Mutex<HashSet<String>>>,
    uploaded: Arc<Mutex<HashSet<String>>>,
    config: AppConfig,
}

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::http::Request<Body>,
    next: Next<Body>,
) -> Result<Response, StatusCode> {
    let api_key = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if api_key != state.config.api_key {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

async fn grab_work(State(state): State<AppState>) -> impl IntoResponse {
    let mut queue = state.queue.lock().unwrap();
    match queue.pop_front() {
        Some(file_path) => {
            debug!("Returning file path: {}", file_path);
            (StatusCode::OK, file_path)
        }
        None => {
            debug!("No more files in queue");
            (StatusCode::NO_CONTENT, String::new())
        }
    }
}

async fn upload_file(
    State(state): State<AppState>,
    AxumPath(file_path): AxumPath<String>,
    body: Body,
) -> impl IntoResponse {
    let temp_path = format!("{}.TEMP", file_path);
    if let Some(parent) = Path::new(&file_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("Failed to create directory: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create directory: {}", e),
            );
        }
    }
    let mut stream = body.into_data_stream();
    match File::create(&temp_path).await {
        Ok(mut file) => {
            let mut total_bytes = 0;
            let mut write_error = None;
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        match file.write_all(&chunk).await {
                            Ok(_) => total_bytes += chunk.len(),
                            Err(e) => {
                                error!("Failed to write chunk: {}", e);
                                write_error = Some(format!("Failed to write chunk: {}", e));
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error receiving chunk: {}", e);
                        write_error = Some(format!("Error receiving chunk: {}", e));
                        break;
                    }
                }
            }
            if let Some(error_msg) = write_error {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return (StatusCode::INTERNAL_SERVER_ERROR, error_msg);
            }
            if let Err(e) = file.flush().await {
                error!("Failed to flush file: {}", e);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to flush file: {}", e),
                );
            }
            match tokio::fs::rename(&temp_path, &file_path).await {
                Ok(_) => {
                    info!("File uploaded to {} ({} bytes)", file_path, total_bytes);
                    
                    let mut uploaded = state.uploaded.lock().unwrap();
                    uploaded.insert(file_path.clone());
                    
                    (
                        StatusCode::CREATED,
                        format!("File uploaded to {} ({} bytes)", file_path, total_bytes),
                    )
                }
                Err(e) => {
                    error!("Failed to rename temp file: {}", e);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to rename temp file: {}", e),
                    )
                }
            }
        }
        Err(e) => {
            error!("Failed to create temp file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create temp file: {}", e),
            )
        }
    }
}

async fn download_file(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    const FILE_PATH_HEADER: &str = "X-File-Path";
    
    let file_path = match headers.get(FILE_PATH_HEADER) {
        Some(value) => match value.to_str() {
            Ok(path) => path,
            Err(_) => {
                error!("Invalid file path header value");
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Invalid file path header"))
                    .unwrap();
            }
        },
        None => {
            error!("Missing file path header");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(format!("Missing '{}' header", FILE_PATH_HEADER)))
                .unwrap();
        }
    };

    if !Path::new(file_path).exists() {
        error!("File not found: {}", file_path);
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("File not found: {}", file_path)))
            .unwrap();
    }

    match tokio::fs::read(file_path).await {
        Ok(bytes) => {
            let bytes_len = bytes.len();
            debug!("Sending file {} ({} bytes)", file_path, bytes_len);
            
            let mut downloaded = state.downloaded.lock().unwrap();
            downloaded.insert(file_path.to_string());
            
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, bytes_len)
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(e) => {
            error!("Error reading file {}: {}", file_path, e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Error reading file: {}", e)))
                .unwrap()
        }
    }
}

pub async fn run_server(target_dir: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = AppConfig {
        target_dir,
        ..Default::default()
    };
    run_server_internal(config).await
}

pub async fn run_server_tls(target_dir: String, cert_path: String, key_path: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = AppConfig {
        target_dir,
        ..Default::default()
    };
    run_server_internal_tls(config, cert_path, key_path).await
}

async fn run_server_internal(config: AppConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting server with target directory: {}", config.target_dir);
    
    info!("Using downloaded files list at: {}", config.downloaded_files_path);
    let downloaded = Arc::new(Mutex::new(load_downloaded_files(&config.downloaded_files_path)));
    
    info!("Using uploaded files list at: {}", config.uploaded_files_path);
    let uploaded = Arc::new(Mutex::new(load_uploaded_files(&config.uploaded_files_path)));
    
    let files = match load_file_list(&config) {
        Ok(files) => files,
        Err(e) => {
            error!("Error loading file list: {}", e);
            VecDeque::new() 
        }
    };
    info!("Loaded {} files of interest", files.len());
    
    let downloaded_files_path = config.downloaded_files_path.clone();
    let uploaded_files_path = config.uploaded_files_path.clone();
    
    let state = AppState {
        queue: FileQueue::new(files),
        downloaded: downloaded.clone(),
        uploaded: uploaded.clone(),
        config: config.clone(),
    };
    
    let save_downloaded = downloaded.clone();
    let save_downloaded_path = downloaded_files_path.clone();
    let save_uploaded = uploaded.clone(); 
    let save_uploaded_path = uploaded_files_path.clone();
    
    let save_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            
            let downloaded_clone = {
                let guard = save_downloaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_downloaded_files(&downloaded_clone, &save_downloaded_path) {
                error!("Failed to save downloaded files: {}", e);
            }
            
            let uploaded_clone = {
                let guard = save_uploaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_uploaded_files(&uploaded_clone, &save_uploaded_path) {
                error!("Failed to save uploaded files: {}", e);
            }
        }
    });
    
    let shutdown_downloaded = downloaded.clone();
    let shutdown_downloaded_path = downloaded_files_path.clone();
    let shutdown_uploaded = uploaded.clone();
    let shutdown_uploaded_path = uploaded_files_path.clone();
    
    let shutdown_handler = tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal, saving state before exit");
            
            let downloaded_clone = {
                let guard = shutdown_downloaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_downloaded_files(&downloaded_clone, &shutdown_downloaded_path) {
                error!("Failed to save downloaded files during shutdown: {}", e);
            }
            
            let uploaded_clone = {
                let guard = shutdown_uploaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_uploaded_files(&uploaded_clone, &shutdown_uploaded_path) {
                error!("Failed to save uploaded files during shutdown: {}", e);
            }
            
            info!("Graceful shutdown complete");
            std::process::exit(0);
        }
    });
    
    let app = Router::new()
        .route("/grab_work", get(grab_work))
        .route("/upload_file/:path", put(upload_file))
        .route("/download_file", get(download_file))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    
    let listener = tokio::net::TcpListener::bind("0.0.0.0:7000").await?;
    info!("Listening on http://0.0.0.0:7000");
    
    axum::serve(listener, app).await?;
    
    save_task.abort();
    shutdown_handler.abort();
    
    Ok(())
}

async fn run_server_internal_tls(config: AppConfig, cert_path: String, key_path: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting TLS server with target directory: {}", config.target_dir);
    
    info!("Using downloaded files list at: {}", config.downloaded_files_path);
    let downloaded = Arc::new(Mutex::new(load_downloaded_files(&config.downloaded_files_path)));
    
    info!("Using uploaded files list at: {}", config.uploaded_files_path);
    let uploaded = Arc::new(Mutex::new(load_uploaded_files(&config.uploaded_files_path)));
    
    let files = match load_file_list(&config) {
        Ok(files) => files,
        Err(e) => {
            error!("Error loading file list: {}", e);
            VecDeque::new() 
        }
    };
    info!("Loaded {} files of interest", files.len());
    
    let downloaded_files_path = config.downloaded_files_path.clone();
    let uploaded_files_path = config.uploaded_files_path.clone();
    
    let state = AppState {
        queue: FileQueue::new(files),
        downloaded: downloaded.clone(),
        uploaded: uploaded.clone(),
        config: config.clone(),
    };
    
    let save_downloaded = downloaded.clone();
    let save_downloaded_path = downloaded_files_path.clone();
    let save_uploaded = uploaded.clone(); 
    let save_uploaded_path = uploaded_files_path.clone();
    
    let save_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            
            let downloaded_clone = {
                let guard = save_downloaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_downloaded_files(&downloaded_clone, &save_downloaded_path) {
                error!("Failed to save downloaded files: {}", e);
            }
            
            let uploaded_clone = {
                let guard = save_uploaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_uploaded_files(&uploaded_clone, &save_uploaded_path) {
                error!("Failed to save uploaded files: {}", e);
            }
        }
    });
    
    let shutdown_downloaded = downloaded.clone();
    let shutdown_downloaded_path = downloaded_files_path.clone();
    let shutdown_uploaded = uploaded.clone();
    let shutdown_uploaded_path = uploaded_files_path.clone();
    
    let shutdown_handler = tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal, saving state before exit");
            
            let downloaded_clone = {
                let guard = shutdown_downloaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_downloaded_files(&downloaded_clone, &shutdown_downloaded_path) {
                error!("Failed to save downloaded files during shutdown: {}", e);
            }
            
            let uploaded_clone = {
                let guard = shutdown_uploaded.lock().unwrap();
                guard.clone()
            };
            if let Err(e) = save_uploaded_files(&uploaded_clone, &shutdown_uploaded_path) {
                error!("Failed to save uploaded files during shutdown: {}", e);
            }
            
            info!("Graceful shutdown complete");
            std::process::exit(0);
        }
    });

    let app = Router::new()
        .route("/grab_work", get(grab_work))
        .route("/upload_file/:path", put(upload_file))
        .route("/download_file", get(download_file))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    info!("Loading TLS configuration from {} and {}", cert_path, key_path);
    let tls_config = RustlsConfig::from_pem_file(cert_path, key_path).await?;
    
    let addr = "0.0.0.0:7000".parse().unwrap();
    info!("Listening on https://0.0.0.0:7000");
    
    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service())
        .await?;
    
    save_task.abort();
    shutdown_handler.abort();
    
    Ok(())
} 
