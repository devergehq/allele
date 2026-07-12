//! Global base infrastructure — a single Traefik reverse proxy + shared
//! Docker network that all Allele sessions register routes against.
//!
//! Allele's responsibility is deliberately narrow: it owns exactly one
//! container (Traefik) and one network (`allele`), both of which exist
//! solely to make multi-session HTTPS routing work. It does NOT manage
//! project services (databases, Redis, Mailpit) — those are brought up by
//! each project's session-start script onto the shared network.
//!
//! The contract Allele provides:
//!   - the `allele` Docker network exists (external, shared)
//!   - Traefik watches `~/.allele/base-infra/traefik/dynamic/` (file provider)
//!   - Traefik discovers labelled containers (docker provider)
//!   - TLS certs live in `~/.allele/base-infra/certs/`
//!
//! Everything is opt-in (a settings toggle) and degrades gracefully when
//! Docker is unavailable — Allele is not, and must never become, a general
//! Docker orchestrator.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

/// Name of the shared Docker network. Both Traefik and any project /
/// shared-service container attach to this as an `external` network.
pub const NETWORK_NAME: &str = "allele";

/// Bundled Traefik compose. Written to disk on first enable and never
/// overwritten afterwards, so the user is free to edit it (e.g. to add
/// shared Redis/Mailpit services on the same network).
const COMPOSE_TEMPLATE: &str = r#"# Allele base infrastructure — managed Traefik reverse proxy.
#
# Allele writes this file once and never overwrites it. You may edit it
# freely — e.g. add shared services (Redis, Mailpit) here; they'll join the
# `allele` network alongside Traefik. Allele only runs `docker compose up -d`
# against this file; it does not parse or manage individual services.

services:
  traefik:
    image: traefik:v3.4
    container_name: allele-traefik
    restart: unless-stopped
    command:
      - --api.dashboard=true
      - --api.insecure=true
      # File provider: per-session host-app routes (php artisan serve, etc.)
      - --providers.file.directory=/etc/traefik/dynamic
      - --providers.file.watch=true
      # Docker provider: containerized services self-register via labels.
      - --providers.docker=true
      - --providers.docker.exposedbydefault=false
      - --entrypoints.web.address=:80
      - --entrypoints.websecure.address=:443
      - --log.level=INFO
    ports:
      - "80:80"
      - "443:443"
      - "8090:8080"
    volumes:
      - ./traefik/dynamic:/etc/traefik/dynamic:ro
      - ./certs:/etc/traefik/certs:ro
      - /var/run/docker.sock:/var/run/docker.sock:ro
    extra_hosts:
      - "host.docker.internal:host-gateway"
    networks:
      - allele

networks:
  allele:
    name: allele
    external: true
"#;

/// Shared middlewares referenced by per-session route files
/// (`https-redirect`, `default-headers`). Written once, user-editable.
const MIDDLEWARES_TEMPLATE: &str = r#"# Shared Traefik middlewares referenced by per-session route files.
# Allele writes this once and never overwrites it.
http:
  middlewares:
    default-headers:
      headers:
        customRequestHeaders:
          X-Forwarded-Proto: "https"
        customResponseHeaders:
          X-Frame-Options: "SAMEORIGIN"
          X-Content-Type-Options: "nosniff"
          Referrer-Policy: "same-origin"

    https-redirect:
      redirectScheme:
        scheme: "https"
        permanent: true
"#;

/// Root of the base-infra tree: `~/.allele/base-infra/`.
pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".allele").join("base-infra"))
}

/// Watched dynamic-config dir. Session-start scripts write route files here.
pub fn dynamic_dir() -> Option<PathBuf> {
    base_dir().map(|d| d.join("traefik").join("dynamic"))
}

/// TLS certificate dir. Projects drop their wildcard `*.pem` here.
pub fn certs_dir() -> Option<PathBuf> {
    base_dir().map(|d| d.join("certs"))
}

/// Ports already claimed by existing session route files in the Traefik
/// dynamic dir, parsed from `host.docker.internal:<port>` backend URLs.
///
/// These claims are the durable record of port ownership: a suspended
/// session keeps its route file (suspend doesn't run session-stop), even
/// though its dev server is no longer listening. A plain TCP-bind probe
/// would see those ports as free and hand them out again, colliding two
/// sessions on one port. Feeding this set into `config::allocate_port`
/// makes a resumed session skip past ports another session still owns.
///
/// `exclude_stem` is the route-file stem of the session being allocated
/// for (e.g. `"session-06d7068d"`), so a session resuming under an id it
/// already has a route file for can reclaim its own port rather than
/// reserving it against itself.
pub fn registered_ports(exclude_stem: Option<&str>) -> HashSet<u16> {
    let mut ports = HashSet::new();
    let Some(dir) = dynamic_dir() else {
        return ports;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return ports;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        if exclude_stem.is_some() && path.file_stem().and_then(|s| s.to_str()) == exclude_stem {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for tail in contents.split("host.docker.internal:").skip(1) {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(port) = digits.parse::<u16>() {
                ports.insert(port);
            }
        }
    }
    ports
}

/// Path to the managed compose file.
pub fn compose_path() -> Option<PathBuf> {
    base_dir().map(|d| d.join("docker-compose.yml"))
}

/// Is the Docker daemon reachable? `docker info` succeeds only when the
/// CLI is on PATH and the daemon is running.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create the directory tree and write the compose + middlewares templates
/// if they don't already exist. Existing files are never overwritten so
/// user edits survive.
pub fn ensure_scaffold() -> std::io::Result<()> {
    let Some(base) = base_dir() else {
        return Err(std::io::Error::other("no home directory"));
    };
    let dynamic = dynamic_dir().unwrap();
    let certs = certs_dir().unwrap();
    std::fs::create_dir_all(&dynamic)?;
    std::fs::create_dir_all(&certs)?;

    let compose = base.join("docker-compose.yml");
    if !compose.exists() {
        std::fs::write(&compose, COMPOSE_TEMPLATE)?;
    }
    let middlewares = dynamic.join("_middlewares.yml");
    if !middlewares.exists() {
        std::fs::write(&middlewares, MIDDLEWARES_TEMPLATE)?;
    }
    Ok(())
}

/// Bring the base infrastructure up: scaffold files, create the shared
/// network, then `docker compose up -d`. Returns a human-readable error on
/// failure (Docker missing, port 443 taken, etc.) suitable for the UI.
pub fn up() -> Result<(), String> {
    if !docker_available() {
        return Err("Docker is not available. Start Docker (or OrbStack) and try again.".into());
    }
    ensure_scaffold().map_err(|e| format!("Failed to create base-infra files: {e}"))?;

    // Create the shared network (idempotent — ignore "already exists").
    let _ = Command::new("docker")
        .args(["network", "create", NETWORK_NAME])
        .output();

    let Some(compose) = compose_path() else {
        return Err("Could not resolve base-infra compose path.".into());
    };

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose)
        .args(["up", "-d"])
        .output()
        .map_err(|e| format!("Failed to run docker compose: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("port is already allocated")
            || stderr.contains("address already in use")
            || stderr.contains("Bind for")
        {
            return Err(
                "Ports 80/443 are already in use by another process. Free them \
                 (stop any other reverse proxy) and try again."
                    .into(),
            );
        }
        return Err(format!("docker compose up failed: {}", stderr.trim()));
    }
    Ok(())
}

/// Tear down the managed Traefik. Leaves the network in place — Docker
/// refuses to remove a network with attached containers, and shared
/// services may still be using it.
pub fn down() -> Result<(), String> {
    if !docker_available() {
        return Err("Docker is not available.".into());
    }
    let Some(compose) = compose_path() else {
        return Err("Could not resolve base-infra compose path.".into());
    };
    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose)
        .args(["down"])
        .output()
        .map_err(|e| format!("Failed to run docker compose: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker compose down failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
