//! `rxh adapt`: converts a Caddyfile or an nginx configuration into RXH
//! TOML. What has no RXH equivalent is left out and reported.

mod caddyfile;
mod directive;
mod nginx;
mod output;

use std::path::Path;

use crate::cli::Adapter;

/// A converted file.
pub(crate) struct Adapted {
    pub toml: String,
    /// What was left out, with the line it comes from.
    pub warnings: Vec<String>,
    /// No server could be converted.
    pub is_empty: bool,
}

/// Converts `text`, read from `source`, with `adapter`.
pub(crate) fn adapt(text: &str, adapter: Adapter, source: &str) -> Result<Adapted, String> {
    let mut out = output::Output::default();
    match adapter {
        Adapter::Caddyfile => caddyfile::convert(text, &mut out)?,
        Adapter::Nginx => nginx::convert(text, &mut out)?,
    }
    out.warnings.sort_by_key(|(line, _)| *line);
    Ok(Adapted {
        toml: out.to_toml(source),
        is_empty: out.servers.is_empty(),
        warnings: out
            .warnings
            .iter()
            .map(|(line, warning)| format!("line {line}: {warning}"))
            .collect(),
    })
}

/// The adapter for a file: from its name (`Caddyfile`, `nginx.conf`,
/// `*.conf`), else nginx when a line ends with `;`.
pub(crate) fn detect(path: &Path, text: &str) -> Adapter {
    let name: String = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("caddyfile") {
        return Adapter::Caddyfile;
    }
    if name.contains("nginx") || name.ends_with(".conf") {
        return Adapter::Nginx;
    }
    let nginx: bool = text.lines().any(|line| {
        let code: &str = line.split('#').next().unwrap_or_default();
        code.trim_end().ends_with(';')
    });
    if nginx {
        Adapter::Nginx
    } else {
        Adapter::Caddyfile
    }
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Config;
    use rxh_core::placeholder::{self, Lookup};
    use toml::{Table, Value};

    use super::*;

    /// The converted document, checked by RXH's own parser, and the warnings.
    fn convert(text: &str, adapter: Adapter) -> (Table, Vec<String>) {
        let adapted: Adapted = adapt(text, adapter, "test").unwrap();
        let expanded = placeholder::expand(&adapted.toml, |lookup| match lookup {
            Lookup::Env(name) => Ok(format!("{}.test:80", name.to_ascii_lowercase())),
            Lookup::File(path) => Err(format!("{path} is not read in tests")),
        });
        if let Err(err) = Config::from_toml(&expanded.unwrap()) {
            panic!("{err}\n{}", adapted.toml);
        }
        let document: Table = adapted.toml.parse().unwrap();
        (document, adapted.warnings)
    }

    fn servers(document: &Table) -> &[Value] {
        document["server"].as_array().unwrap()
    }

    fn routes(server: &Value) -> &[Value] {
        server["match"].as_array().unwrap()
    }

    fn has_warning(warnings: &[String], line: usize, text: &str) -> bool {
        warnings
            .iter()
            .any(|warning| warning.starts_with(&format!("line {line}: ")) && warning.contains(text))
    }

    fn value(toml: &str) -> Value {
        toml::from_str::<Table>(&format!("v = {toml}")).unwrap()["v"].clone()
    }

    #[test]
    fn caddyfile_site_with_a_load_balanced_reverse_proxy() {
        let text = "example.com www.example.com {\n\
                    \treverse_proxy 10.0.0.1:3000 10.0.0.2 {\n\
                    \t\tlb_policy least_conn\n\t\tlb_retries 2\n\t\thealth_uri /health\n\
                    \t\theader_up X-Origin rxh\n\t}\n}\n";
        let (document, warnings) = convert(text, Adapter::Caddyfile);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            document["upstream"]["example_com"],
            value(
                "{ policy = \"least_conn\", backends = [\"10.0.0.1:3000\", \"10.0.0.2:80\"], \
                 retries = 2, health = { uri = \"/health\" } }"
            )
        );
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(server["listen"].as_str(), Some("0.0.0.0:443"));
        assert_eq!(
            server["host"],
            value("[\"example.com\", \"www.example.com\"]")
        );
        assert_eq!(server["tls"].as_str(), Some("auto"));
        assert_eq!(
            routes(server),
            [value(
                "{ forward = \"@example_com\", request = { headers = { set = { X-Origin = \
                 \"rxh\" } } } }"
            )]
        );
    }

    #[test]
    fn caddyfile_routes_follow_caddy_directive_order() {
        let text = ":8080 {\n\tencode gzip\n\theader -Server\n\tfile_server\n\
                    \treverse_proxy /api/* :3000\n\thandle_path /static/* {\n\
                    \t\troot * /srv/static\n\t\tfile_server browse\n\t}\n\
                    \tredir /old /new permanent\n}\n";
        let (document, warnings) = convert(text, Adapter::Caddyfile);
        // Response header rules of RXH apply to proxied responses only.
        let header = "header on files and redirects";
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        for line in [4, 8, 10] {
            assert!(has_warning(&warnings, line, header), "{warnings:?}");
        }
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(server["listen"].as_str(), Some("0.0.0.0:8080"));
        assert!(server.get("tls").is_none());
        assert_eq!(
            routes(server),
            [
                value("{ path = [\"/old\"], redirect = { to = \"/new\", status = 301 } }"),
                value(
                    "{ uri = \"/static\", serve = { root = \"/srv/static\", browse = true }, \
                     encode = [\"gzip\"] }"
                ),
                value(
                    "{ path = [\"/api/*\"], forward = \"127.0.0.1:3000\", response = { headers \
                     = { remove = [\"Server\"] } }, encode = [\"gzip\"] }"
                ),
                value("{ serve = { root = \".\", browse = false }, encode = [\"gzip\"] }"),
            ]
        );
    }

    #[test]
    fn caddyfile_global_options_tls_files_and_basic_auth() {
        let text = "{\n\temail ops@example.com\n\tadmin off\n\thttp_port 8080\n\
                    \tauto_https disable_redirects\n\tgrace_period 10s\n}\n\
                    a.example.com {\n\ttls /certs/a.pem /certs/a.key\n\
                    \tbasic_auth {\n\t\tbob $2b$12$gy7i8SeTmMJhASGF0qP8AOC5Z4MtS8GfD3Vm9DlcQNsqkF2qdwBtO\n\t}\n\treverse_proxy localhost:3000\n}\n\
                    b.example.com {\n\treverse_proxy localhost:4000\n}\n";
        let (document, warnings) = convert(text, Adapter::Caddyfile);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(document["grace_period"].as_str(), Some("10s"));
        assert_eq!(document["admin"], value("{ enabled = false }"));
        assert_eq!(
            document["acme"],
            value("{ email = \"ops@example.com\", http_port = 8080 }")
        );
        let [a, b] = servers(&document) else {
            panic!("two servers expected");
        };
        assert_eq!(
            a["tls"],
            value("{ cert = \"/certs/a.pem\", key = \"/certs/a.key\" }")
        );
        assert!(a.get("https_redirect").is_none());
        assert_eq!(
            routes(a)[0]["basic_auth"],
            value(
                "{ users = { bob = \"$2b$12$gy7i8SeTmMJhASGF0qP8AOC5Z4MtS8GfD3Vm9DlcQNsqkF2qdwBtO\" } }"
            )
        );
        assert_eq!(b["tls"].as_str(), Some("auto"));
        assert_eq!(b["https_redirect"].as_bool(), Some(false));
    }

    #[test]
    fn caddyfile_single_site_without_braces_and_placeholders() {
        let text = "localhost:2015\n\nreverse_proxy {$BACKEND} {$OTHER:127.0.0.1:9}\n\
                    redir /docs https://docs.example.com{http.request.uri.path}\n";
        let (document, warnings) = convert(text, Adapter::Caddyfile);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(has_warning(&warnings, 1, "internal CA on 127.0.0.1:2015"));
        assert!(has_warning(
            &warnings,
            3,
            "default value of {$OTHER:127.0.0.1:9}"
        ));
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(server["listen"].as_str(), Some("127.0.0.1:2015"));
        assert!(server.get("tls").is_none());
        assert_eq!(
            routes(server),
            [
                value("{ path = [\"/docs\"], redirect = \"https://docs.example.com{path}\" }"),
                value("{ forward = [\"{env.BACKEND}\", \"{env.OTHER}\"] }"),
            ]
        );
    }

    #[test]
    fn caddyfile_reports_what_it_leaves_out() {
        let text = "(common) {\n\tencode gzip\n}\n\n:80 {\n\t@bot expression {http.agent}\n\
                    \treverse_proxy @bot :1\n\tphp_fastcgi :9000\n\treverse_proxy :3000 {\n\
                    \t\tlb_policy fancy\n\t}\n}\n:81 {\n\timport common\n}\n";
        let (document, warnings) = convert(text, Adapter::Caddyfile);
        for (line, text) in [
            (1, "snippet (common)"),
            (6, "expression matcher of @bot"),
            (8, "directive php_fastcgi"),
            (10, "lb_policy fancy"),
            (13, "nothing rxh can serve"),
            (14, "directive import"),
        ] {
            assert!(has_warning(&warnings, line, text), "{text}: {warnings:?}");
        }
        let [server] = servers(&document) else {
            panic!("only the :80 site can be served");
        };
        assert_eq!(routes(server), [value("{ forward = \"127.0.0.1:3000\" }")]);
    }

    #[test]
    fn caddyfile_syntax_errors_are_errors() {
        let err = adapt("a.com {\n\treverse_proxy :1\n", Adapter::Caddyfile, "x")
            .err()
            .unwrap();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn nginx_upstream_and_tls_server() {
        let text = "events { worker_connections 1024; }\nhttp {\n\
                    upstream app {\n least_conn;\n server 10.0.0.1:8080 weight=3;\n\
                    server 10.0.0.2 max_fails=2 fail_timeout=30;\n keepalive 16;\n}\n\
                    server {\n listen 443 ssl;\n server_name example.com .example.org;\n\
                    ssl_certificate /etc/ssl/cert.pem;\n ssl_certificate_key /etc/ssl/key.pem;\n\
                    location / {\n  proxy_pass http://app;\n  proxy_set_header Host $host;\n\
                    proxy_set_header X-Team web;\n }\n}\n}\n";
        let (document, warnings) = convert(text, Adapter::Nginx);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            document["upstream"]["app"],
            value(
                "{ policy = \"least_conn\", keepalive_idle_conns_per_host = 16, backends = \
                 [{ address = \"10.0.0.1:8080\", weight = 3 }, \"10.0.0.2:80\"], \
                 passive = { max_fails = 2, fail_duration = \"30s\" } }"
            )
        );
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(server["listen"].as_str(), Some("0.0.0.0:443"));
        assert_eq!(
            server["host"],
            value("[\"example.com\", \"example.org\", \"*.example.org\"]")
        );
        assert_eq!(
            server["tls"],
            value("{ cert = \"/etc/ssl/cert.pem\", key = \"/etc/ssl/key.pem\" }")
        );
        assert_eq!(
            routes(server),
            [value(
                "{ forward = \"@app\", request = { headers = { set = { X-Team = \"web\" } } } }"
            )]
        );
    }

    #[test]
    fn nginx_locations_in_nginx_order_with_inherited_settings() {
        let text = "server {\n listen 127.0.0.1:8080;\n root /srv/www;\n index index.html;\n\
                    gzip on;\n client_max_body_size 10m;\n add_header X-Site www;\n\
                    location /api/ {\n  proxy_pass http://127.0.0.1:3000;\n }\n\
                    location /api/v2/ {\n  proxy_pass https://v2.internal;\n\
                    add_header X-Api v2;\n }\n\
                    location = /old {\n  return 308 https://$server_name/new?$args;\n }\n\
                    location /files/ {\n  alias /srv/files/;\n  autoindex on;\n }\n\
                    server_name example.com;\n}\n";
        let (document, warnings) = convert(text, Adapter::Nginx);
        // Response header rules of RXH apply to proxied responses only.
        let header = "add_header on files and redirects";
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        for line in [1, 15, 18] {
            assert!(has_warning(&warnings, line, header), "{warnings:?}");
        }
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(server["listen"].as_str(), Some("127.0.0.1:8080"));
        assert!(server.get("tls").is_none());
        let common = "encode = [\"gzip\"], request = { max_body_size = \"10MiB\" }";
        let site = "response = { headers = { set = { X-Site = \"www\" } } }";
        assert_eq!(
            routes(server),
            [
                value(
                    "{ path = \"/old\", redirect = { to = \"https://example.com/new?{query}\", \
                     status = 308 }, request = { max_body_size = \"10MiB\" } }"
                ),
                value(&format!(
                    "{{ path = \"/api/v2/*\", forward = \"https://v2.internal\", {common}, \
                     response = {{ headers = {{ set = {{ X-Api = \"v2\" }} }} }} }}"
                )),
                value(&format!(
                    "{{ uri = \"/files\", serve = {{ root = \"/srv/files/\", browse = true, \
                     index = [\"index.html\"] }}, {common} }}"
                )),
                value(&format!(
                    "{{ path = \"/api/*\", forward = \"127.0.0.1:3000\", {common}, {site} }}"
                )),
                value(&format!(
                    "{{ serve = {{ root = \"/srv/www\", browse = false, index = \
                     [\"index.html\"] }}, {common} }}"
                )),
            ]
        );
    }

    #[test]
    fn nginx_redirect_server_and_stream_block() {
        let text = "http {\n server {\n  listen 80;\n  server_name a.example.com;\n\
                    return 301 https://$host$request_uri;\n }\n}\n\
                    stream {\n upstream db { server 10.0.0.5:5432; }\n\
                    server {\n  listen 5432;\n  proxy_pass db;\n  proxy_timeout 1m;\n }\n\
                    server {\n  listen 53 udp;\n  listen 53;\n  proxy_pass 10.0.0.53:53;\n }\n}\n";
        let (document, warnings) = convert(text, Adapter::Nginx);
        assert!(warnings.is_empty(), "{warnings:?}");
        let [web, db, dns_tcp, dns_udp] = servers(&document) else {
            panic!("four servers expected");
        };
        assert_eq!(web["host"].as_str(), Some("a.example.com"));
        assert_eq!(
            routes(web),
            [value(
                "{ redirect = { to = \"https://{host}{uri}\", status = 301 } }"
            )]
        );
        assert_eq!(
            db,
            &value(
                "{ listen = \"0.0.0.0:5432\", protocol = \"tcp\", forward = \"@db\", \
                 idle_timeout = \"1m\" }"
            )
        );
        assert_eq!(dns_tcp["protocol"].as_str(), Some("tcp"));
        assert_eq!(
            dns_udp,
            &value("{ listen = \"0.0.0.0:53\", protocol = \"udp\", forward = \"10.0.0.53:53\" }")
        );
    }

    #[test]
    fn nginx_reports_what_it_leaves_out() {
        let text = "include mime.types;\nserver {\n listen 80;\n\
                    location ~ \\.php$ { fastcgi_pass unix:/run/php.sock; }\n\
                    location /x { proxy_pass http://$backend; }\n\
                    location /y { return 200 ok; }\n\
                    location / { proxy_pass http://127.0.0.1:3000/app/; \
                    proxy_set_header X-Real-IP $remote_addr; }\n}\n";
        let (document, warnings) = convert(text, Adapter::Nginx);
        for (line, text) in [
            (1, "include"),
            (4, "regex and named locations"),
            (5, "with variables"),
            (6, "return 200"),
            (7, "URI /app/ of proxy_pass"),
            (7, "variable $remote_addr"),
        ] {
            assert!(has_warning(&warnings, line, text), "{text}: {warnings:?}");
        }
        let [server] = servers(&document) else {
            panic!("one server expected");
        };
        assert_eq!(routes(server), [value("{ forward = \"127.0.0.1:3000\" }")]);
    }

    #[test]
    fn nginx_syntax_errors_are_errors() {
        let err = adapt("server { listen 80;", Adapter::Nginx, "x")
            .err()
            .unwrap();
        assert!(err.contains("not closed"), "{err}");
    }

    #[test]
    fn detects_the_format_from_the_name_then_the_content() {
        let caddy = "example.com {\n\treverse_proxy :3000\n}\n";
        let nginx = "server {\n\tlisten 80; # port\n}\n";
        assert_eq!(
            detect(Path::new("/etc/caddy/Caddyfile"), nginx),
            Adapter::Caddyfile
        );
        assert_eq!(
            detect(Path::new("site.caddyfile"), nginx),
            Adapter::Caddyfile
        );
        assert_eq!(
            detect(Path::new("/etc/nginx/nginx.conf"), caddy),
            Adapter::Nginx
        );
        assert_eq!(detect(Path::new("app.conf"), caddy), Adapter::Nginx);
        assert_eq!(detect(Path::new("site"), nginx), Adapter::Nginx);
        assert_eq!(detect(Path::new("site"), caddy), Adapter::Caddyfile);
    }
}
