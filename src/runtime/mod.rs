pub mod native;
pub mod traits;

pub use native::NativeRuntime;
pub use traits::RuntimeAdapter;

use crate::config::RuntimeConfig;

/// Factory: create the right runtime from config
pub fn create_runtime(config: &RuntimeConfig) -> Box<dyn RuntimeAdapter> {
    match config.kind.as_str() {
        "native" | "docker" => Box::new(NativeRuntime::new()),
        "cloudflare" => {
            tracing::warn!("Cloudflare runtime not yet implemented, falling back to native");
            Box::new(NativeRuntime::new())
        }
        _ => {
            tracing::warn!("Unknown runtime '{}', falling back to native", config.kind);
            Box::new(NativeRuntime::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_native() {
        let cfg = RuntimeConfig {
            kind: "native".into(),
        };
        let rt = create_runtime(&cfg);
        assert_eq!(rt.name(), "native");
        assert!(rt.has_shell_access());
    }

    #[test]
    fn factory_docker_returns_native() {
        let cfg = RuntimeConfig {
            kind: "docker".into(),
        };
        let rt = create_runtime(&cfg);
        assert_eq!(rt.name(), "native");
    }

    #[test]
    fn factory_cloudflare_falls_back() {
        let cfg = RuntimeConfig {
            kind: "cloudflare".into(),
        };
        let rt = create_runtime(&cfg);
        assert_eq!(rt.name(), "native");
    }

    #[test]
    fn factory_unknown_falls_back() {
        let cfg = RuntimeConfig {
            kind: "wasm-edge-unknown".into(),
        };
        let rt = create_runtime(&cfg);
        assert_eq!(rt.name(), "native");
    }

    #[test]
    fn factory_empty_falls_back() {
        let cfg = RuntimeConfig { kind: "".into() };
        let rt = create_runtime(&cfg);
        assert_eq!(rt.name(), "native");
    }
}
