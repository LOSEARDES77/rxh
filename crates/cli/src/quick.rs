//! Configurations for the commands that run without a configuration file.

use std::path::Path;

use rxh_core::config::Config;
use serde_json::json;

use crate::cli::ServiceProtocol;

/// `rxh file-server`: one HTTP server serving `root`, admin API disabled.
pub(crate) fn file_server(
    listen: std::net::SocketAddr,
    root: &Path,
    browse: bool,
    encode: bool,
) -> Result<Config, String> {
    let encodings = if encode {
        json!(["zstd", "gzip"])
    } else {
        json!([])
    };
    build(json!({
        "admin": { "enabled": false },
        "server": [{
            "listen": [listen],
            "serve": { "root": root, "browse": browse },
            "encode": encodings,
        }],
    }))
}

/// `rxh reverse-proxy`: one server forwarding to a pool of `to`.
pub(crate) fn reverse_proxy(
    from: std::net::SocketAddr,
    to: &[String],
    policy: &str,
    protocol: ServiceProtocol,
    host: Option<&str>,
) -> Result<Config, String> {
    let protocol = match protocol {
        ServiceProtocol::Http => "http",
        ServiceProtocol::Tcp => "tcp",
        ServiceProtocol::Udp => "udp",
    };
    let mut server = json!({
        "listen": [from],
        "protocol": protocol,
        "forward": "@proxy",
    });
    if let Some(host) = host {
        if protocol != "http" {
            return Err(String::from("--host only applies to --protocol http"));
        }
        server["host"] = json!(host);
    }
    build(json!({
        "admin": { "enabled": false },
        "upstream": { "proxy": { "policy": policy, "backends": to } },
        "server": [server],
    }))
}

fn build(value: serde_json::Value) -> Result<Config, String> {
    let config: Config = serde_json::from_value(value).map_err(|err| err.to_string())?;
    config.validate().map_err(|err| err.to_string())?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Action;

    use super::*;

    #[test]
    fn file_server_options() {
        let config = file_server(
            "127.0.0.1:8080".parse().unwrap(),
            Path::new("/srv"),
            false,
            true,
        )
        .expect("valid");
        assert!(!config.admin.enabled);
        let pattern = &config.servers[0].patterns[0];
        assert_eq!(pattern.encode.len(), 2);
        let Action::Serve(files) = &pattern.action else {
            panic!("serve expected");
        };
        assert!(!files.browse);
        assert_eq!(files.root, "/srv");
    }

    #[test]
    fn reverse_proxy_options() {
        let to = [String::from("127.0.0.1:1"), String::from("127.0.0.1:2")];
        let config = reverse_proxy(
            "127.0.0.1:80".parse().unwrap(),
            &to,
            "round_robin",
            ServiceProtocol::Http,
            Some("a.test"),
        )
        .expect("valid");
        assert_eq!(config.servers[0].host, ["a.test"]);
        assert_eq!(config.upstreams["proxy"].backends.len(), 2);

        let tcp = reverse_proxy(
            "127.0.0.1:80".parse().unwrap(),
            &to,
            "random",
            ServiceProtocol::Tcp,
            Some("a"),
        );
        assert!(tcp.is_err());
        let bad = reverse_proxy(
            "127.0.0.1:80".parse().unwrap(),
            &to,
            "nope",
            ServiceProtocol::Http,
            None,
        );
        assert!(bad.is_err());
    }
}
