//! Server configuration loaded from environment variables.

pub struct Config {
    pub bind_addr: String,
    /// STUN server (`host:port`) handed out to viewer browsers for ICE.
    /// There is no TURN: a viewer that can't reach the host via P2P simply
    /// fails to connect instead of relaying media through this server.
    pub stun_server: String,
    /// Minimum accepted `framewire.exe` version (`FW_MIN_HOST_VERSION`, e.g.
    /// "0.2.0"). A host below this — or one old enough to not report a
    /// version at all — is rejected before a room is created, so a known-bad
    /// old build can't load the server. `None` accepts any version.
    pub min_host_version: Option<String>,
    /// Directory served at `/download/*` (`FW_DOWNLOADS_DIR`) — drop
    /// `framewire.exe` in here on the host machine to publish it, no
    /// rebuild or redeploy needed. Defaults to a local `downloads/` folder
    /// for `cargo run`; the deployed container mounts this from a bind
    /// volume (see `server/docker-compose.yml`).
    pub downloads_dir: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            bind_addr: env_or("FW_BACKEND_BIND", "0.0.0.0:8090"),
            stun_server: env_or("FW_STUN_SERVER", "stun.l.google.com:19302"),
            min_host_version: std::env::var("FW_MIN_HOST_VERSION").ok(),
            downloads_dir: env_or("FW_DOWNLOADS_DIR", "downloads"),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
