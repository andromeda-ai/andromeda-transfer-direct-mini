use std::fs;
use std::path::Path;
use andromeda_transfer_direct_mini::client::{self, Mode};
use andromeda_transfer_direct_mini::server;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "andromeda-transfer-direct-mini")]
#[command(author, version, about = "Tool for transferring files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Server {
        #[arg(long)]
        target_dir: String,
        
        #[arg(long)]
        tls: bool,

        #[arg(long)]
        api_key: String,
    },
    
    Client {
        server_url: String,
        
        #[arg(default_value_t = 4)]
        concurrency: usize,
        
        #[arg(long, default_value_t = true, conflicts_with = "pull")]
        push: bool,
        
        #[arg(long)]
        pull: bool,
        
        #[arg(long)]
        remap: Option<String>,
        
        #[arg(long)]
        tls: bool,

        #[arg(long)]
        api_key: String,
    },
}

fn generate_self_signed_cert() -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let cert_dir = Path::new("certs");
    let cert_path = cert_dir.join("cert.pem");
    let key_path = cert_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        tracing::info!("Using existing certificates");
        return Ok((cert_path.to_string_lossy().to_string(), key_path.to_string_lossy().to_string()));
    }

    fs::create_dir_all(cert_dir)?;

    let subject_alt_names = vec!["localhost".to_string()];
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)?;

    fs::write(&cert_path, cert.pem())?;
    fs::write(&key_path, key_pair.serialize_pem())?;

    tracing::info!("Generated new self-signed certificates");
    Ok((cert_path.to_string_lossy().to_string(), key_path.to_string_lossy().to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Server { target_dir, tls, api_key } => {
            tracing::info!("Starting in server mode");
            
            if tls {
                let (cert_path, key_path) = generate_self_signed_cert()?;
                server::run_server_tls(target_dir, cert_path, key_path, api_key).await?;
            } else {
                server::run_server(target_dir, api_key).await?;
            }
        },
        Commands::Client { server_url, concurrency, push: _, pull, remap, tls, api_key } => {
            let mode = if pull { Mode::Pull } else { Mode::Push };
            
            tracing::info!(
                "Starting in client mode, connecting to {} with {} workers, mode: {:?}, TLS: {}", 
                server_url, 
                concurrency,
                mode,
                tls
            );
            
            client::run_client_parallel(server_url, concurrency, mode, remap, tls, api_key).await?;
        }
    }
    
    Ok(())
}
