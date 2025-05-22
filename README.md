# Andromeda Transfer Direct

A high-performance file transfer utility for efficiently moving files between systems, with support for parallel transfers, path remapping, TLS encryption, and API key authentication.

## Features

- **Push/Pull Modes**: Transfer files in either direction between client and server
- **Parallel Processing**: Configure concurrent workers to maximize throughput
- **Path Remapping**: Automatically transform paths between source and destination
- **TLS Support**: Secure file transfers with TLS encryption
- **API Key Authentication**: Secure access with pre-shared API keys
- **Graceful Shutdown**: Handles interruptions cleanly, finishing in-progress transfers
- **Progress Tracking**: Reports progress during large transfer operations

## Installation

Clone the repository and build locally:

```bash
git clone https://github.com/andromeda-ai/andromeda-transfer-direct-mini.git
cd andromeda-transfer-direct-mini
cargo build --release
```

The compiled binary will be available at `./target/release/andromeda-transfer-direct-mini`

## Usage

### Server Mode

Start a server that listens for file transfer requests:

```bash
# Basic usage with API key
cargo run --release -- server --target-dir /path/to/files --api-key "your-secret-key"

# With TLS and API key
cargo run --release -- server --target-dir /path/to/files --tls --api-key "your-secret-key"
```

### Client Mode

Connect to a server to push or pull files:

```bash
# Basic usage with API key
cargo run --release -- client http://server-address:7000 --api-key "your-secret-key"

# With path remapping and API key
cargo run --release -- client http://server-address:7000 --mode pull --remap /source/path:/destination/path --api-key "your-secret-key"

# With custom concurrency and API key
cargo run --release -- client http://server-address:7000 --mode push --concurrency 8 --api-key "your-secret-key"

# With TLS and API key
cargo run --release -- client https://server-address:7000 --mode pull --tls --api-key "your-secret-key"
```

## Command-Line Flags

### Server Options
- `--target-dir <DIR>`: Directory to serve files from
- `--tls`: Enable TLS (HTTPS) mode
- `--api-key <KEY>`: API key for authentication (required)

### Client Options
- `--server <URL>`: Server URL to connect to
- `--mode <MODE>`: Transfer mode, either "push" or "pull"
- `--concurrency <NUMBER>`: Number of parallel workers (default: 4)
- `--remap <SOURCE:TARGET>`: Path remapping in format "source:target"
- `--tls`: Enable TLS (HTTPS) mode
- `--api-key <KEY>`: API key for authentication (required)

## Kubernetes Deployment

### Server Deployment
### Server Service with NodePort

Create a Kubernetes service to expose the server on port 7000 of the node:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: disk-access-migration
  namespace: tenant-ac-{YOUR_ORG}
  annotations:
    metallb.universe.tf/address-pool: public
spec:
  ports:
  - port: 7000
    protocol: TCP
    targetPort: 7000
  selector:
    app: disk-access
  type: LoadBalancer
```

This configuration will:
1. Make the server accessible at `service-ip:7000`

Server-IP:
```bash
kubectl get service disk-access-migration -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

## Examples

### Pulling files from a server

```bash
# Pull files from server to local system
cargo run --release -- client --server http://server:7000 --mode pull

# Pull files with remapping and high concurrency
cargo run --release -- client --server http://server:7000 --mode pull --remap /remote/data:/local/backup --concurrency 16
```
