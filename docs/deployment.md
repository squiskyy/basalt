# Deployment

Guide for building, running, and deploying Basalt in production. Basalt ships as a single binary with no external dependencies - just the executable and an optional config file.

## Building

### Release Build

```bash
cargo build --release
```

The binary is at `target/release/basalt`.

### With io_uring (Linux only)

```bash
cargo build --release --features io-uring
```

This adds the `--io-uring` CLI flag for the RESP server. The HTTP server always uses tokio regardless.

### Cross-Compilation

```bash
# For ARM64 (aarch64)
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

## Running

### Basic

```bash
# All defaults (HTTP on 7380, RESP on 6380)
./basalt

# Custom ports
./basalt --http-port 8080 --resp-port 6379

# With persistence
./basalt --db-path /var/lib/basalt

# With auth
./basalt --auth "bsk-admin:*" --auth "bsk-agent1:agent-1,shared"
```

### With Config File

```bash
./basalt --config /etc/basalt/basalt.toml
```

See [Configuration](./configuration.md) for full TOML options.

### Production Example

```bash
./basalt \
  --config /etc/basalt/basalt.toml \
  --http-host 0.0.0.0 \
  --resp-host 0.0.0.0 \
  --shards 128 \
  --db-path /var/lib/basalt \
  --snapshot-interval 30000 \
  --sweep-interval 15000 \
  --compression-threshold 512
```

### Log Levels

Basalt uses `RUST_LOG` for log filtering:

```bash
# Default: info level for basalt only
RUST_LOG=basalt=info ./basalt

# Debug everything
RUST_LOG=debug ./basalt

# Debug specific modules
RUST_LOG=basalt::resp=debug,basalt::store=debug ./basalt

# Trace the engine
RUST_LOG=basalt::store::engine=trace ./basalt
```

## Systemd

Create `/etc/systemd/system/basalt.service`:

```ini
[Unit]
Description=Basalt Memory Stack for AI Platforms
After=network.target

[Service]
Type=simple
User=basalt
Group=basalt
ExecStart=/usr/local/bin/basalt --config /etc/basalt/basalt.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/basalt
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```bash
# Create user and data directory
sudo useradd -r -s /bin/false basalt
sudo mkdir -p /var/lib/basalt
sudo chown basalt:basalt /var/lib/basalt

# Install and start
sudo cp target/release/basalt /usr/local/bin/
sudo systemctl daemon-reload
sudo systemctl enable basalt
sudo systemctl start basalt

# Check status
sudo systemctl status basalt
journalctl -u basalt -f
```

## Docker

### Dockerfile

```dockerfile
FROM rust:1.94-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/basalt /usr/local/bin/basalt
COPY basalt.example.toml /etc/basalt/basalt.toml

EXPOSE 7380 6380
VOLUME /var/lib/basalt

ENTRYPOINT ["basalt"]
CMD ["--config", "/etc/basalt/basalt.toml"]
```

### Build and Run

```bash
# Build
docker build -t basalt .

# Run
docker run -d \
  --name basalt \
  -p 7380:7380 \
  -p 6380:6380 \
  -v basalt-data:/var/lib/basalt \
  -v /etc/basalt/tokens.txt:/etc/basalt/tokens.txt:ro \
  basalt

# With custom config
docker run -d \
  --name basalt \
  -p 7380:7380 \
  -p 6380:6380 \
  -v basalt-data:/var/lib/basalt \
  -v /etc/basalt/basalt.toml:/etc/basalt/basalt.toml:ro \
  basalt --config /etc/basalt/basalt.toml
```

### Docker Compose

```yaml
version: "3.8"
services:
  basalt:
    image: basalt:latest
    ports:
      - "7380:7380"
      - "6380:6380"
    volumes:
      - basalt-data:/var/lib/basalt
      - ./basalt.toml:/etc/basalt/basalt.toml:ro
      - ./tokens.txt:/etc/basalt/tokens.txt:ro
    command: ["--config", "/etc/basalt/basalt.toml"]
    restart: unless-stopped
    deploy:
      resources:
        limits:
          memory: 4G

  basalt-replica:
    image: basalt:latest
    ports:
      - "7381:7380"
      - "6381:6380"
    volumes:
      - basalt-replica-data:/var/lib/basalt
    command: ["--db-path", "/var/lib/basalt"]
    restart: unless-stopped
    depends_on:
      - basalt

volumes:
  basalt-data:
  basalt-replica-data:
```

## Production Checklist

### Security

- [ ] Enable auth with scoped tokens (`--auth` or `--auth-file`)
- [ ] Bind to `127.0.0.1` if only local access is needed
- [ ] Use `0.0.0.0` only if behind a firewall or reverse proxy
- [ ] Set `LimitNOFILE=65536` or higher for many concurrent connections
- [ ] Run as a non-root user (`User=basalt` in systemd)
- [ ] Use `ProtectSystem=strict` and `ReadWritePaths` in systemd

### Persistence

- [ ] Set `--db-path` for data persistence across restarts
- [ ] Choose an appropriate `--snapshot-interval` (30-60s for most workloads)
- [ ] Ensure the data directory is on a fast disk (SSD preferred)
- [ ] Monitor disk space (each snapshot is roughly proportional to entry count)
- [ ] Set `--snapshot-compression-threshold` for large text workloads

### Performance

- [ ] Set `--shards` to match your core count (default 64 is good for most cases)
- [ ] Enable compression for workloads with large values (`--compression-threshold 512`)
- [ ] Set `--max-entries` to prevent OOM under burst writes
- [ ] Set `--sweep-interval` for episodic-heavy workloads (15-30s)

### Monitoring

- [ ] Check `/health` endpoint for liveness
- [ ] Check `/info` endpoint for shard count, role, replication status
- [ ] Set `RUST_LOG=basalt=info` and monitor logs for errors
- [ ] Watch for "max entries exceeded" errors (capacity limits)
- [ ] Watch for "replica fell behind" warnings (replication health)

### Replication

- [ ] Set `--wal-size` large enough for your replication window
- [ ] Monitor `connected_replicas` via INFO
- [ ] Have a plan for manual failover (`REPLICAOF NO ONE`)
- [ ] Use a VPN or secure network for replication traffic (no TLS)

## Reverse Proxy (HTTP)

If you need TLS for the HTTP API, use a reverse proxy:

### nginx

```nginx
upstream basalt {
    server 127.0.0.1:7380;
}

server {
    listen 443 ssl;
    server_name basalt.example.com;

    ssl_certificate /etc/ssl/certs/basalt.pem;
    ssl_certificate_key /etc/ssl/private/basalt.key;

    location / {
        proxy_pass http://basalt;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Caddy

```
basalt.example.com {
    reverse_proxy localhost:7380
}
```

## Graceful Shutdown

Basalt handles SIGTERM and SIGINT for graceful shutdown:

1. Both servers stop accepting new connections
2. In-flight requests are completed
3. The snapshot loop performs one final snapshot (if `db_path` is configured)
4. The process exits

This ensures no data is lost during restarts when persistence is enabled.
