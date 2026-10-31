mod export;
mod index;
mod render;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use clap::Parser;
use minijinja::{Environment, context};

use crate::index::{Index, is_markdown, root_document, scan_markdown};
use crate::render::LinkMode;

const PAGE_TEMPLATE: &str = include_str!("../templates/page.html");
const EXPORT_TEMPLATE: &str = include_str!("../templates/export.html");
const STYLE: &str = include_str!("../templates/style.css");
const MERMAID_JS: &str = include_str!("../templates/mermaid.js");

/// Serve or export markdown files as linked, rendered pages.
///
/// Without a file, or given a directory, indexes all markdown files under it.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Markdown file or directory to serve
    file: Option<String>,

    /// For a directory, only index the most sensible markdown file and what it links to
    #[arg(short, long)]
    strict: bool,

    /// Write everything as one standalone HTML file instead of serving
    #[arg(long, value_name = "OUT.HTML")]
    export: Option<String>,

    /// Address to serve on: HOST, :PORT or HOST:PORT (default localhost, random port).
    /// 0.0.0.0 or :: binds all interfaces
    #[arg(short, long, value_name = "ADDR")]
    bind: Option<String>,
}

/// Splits `--bind` into host and port. A missing host means localhost, a missing port means random.
fn parse_bind(spec: &str) -> anyhow::Result<(String, u16)> {
    let spec = spec.trim();
    if let Ok(addr) = spec.parse::<SocketAddr>() {
        return Ok((addr.ip().to_string(), addr.port()));
    }
    // A bare IPv6 address (with or without brackets) has no port.
    let bare = spec.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<Ipv6Addr>().is_ok() {
        return Ok((bare.to_string(), 0));
    }
    let (host, port) = match spec.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .with_context(|| format!("invalid port in --bind {spec:?}"))?,
        ),
        None => (spec, 0),
    };
    let host = if host.is_empty() { "localhost" } else { host };
    Ok((host.to_string(), port))
}

/// Hosts to print links for: every local address when bound to an unspecified one.
fn display_hosts(bound: IpAddr, requested: &str) -> Vec<String> {
    if requested.parse::<IpAddr>().is_err() {
        return vec![requested.to_string()];
    }
    if !bound.is_unspecified() {
        return vec![match bound {
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        }];
    }
    let mut hosts: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| bound.is_ipv6() || ip.is_ipv4())
        .map(|ip| match ip {
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        })
        .collect();
    hosts.sort_by_key(|h| (h.starts_with('['), h != "127.0.0.1"));
    if hosts.is_empty() {
        hosts.push("localhost".into());
    }
    hosts
}

struct AppState {
    index: Index,
    root_hash: String,
    templates: Environment<'static>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Cli {
        file,
        strict,
        export: export_to,
        bind,
    } = Cli::parse();

    let index = Index::default();
    let mut scope = std::env::current_dir()?.canonicalize()?;
    let target = file.as_deref().map(PathBuf::from);
    let root = match target.filter(|t| !t.is_dir()) {
        Some(path) => {
            let root = render::resolve_file(&path)
                .with_context(|| format!("no such file: {}", path.display()))?;
            index.insert(root.clone());
            if let Some(dir) = root.parent() {
                scope = dir.to_path_buf();
            }
            root
        }
        None => {
            if let Some(dir) = &file {
                scope = PathBuf::from(dir)
                    .canonicalize()
                    .with_context(|| format!("no such directory: {dir}"))?;
            }
            let docs = scan_markdown(&scope);
            let root = root_document(&docs, &scope)
                .with_context(|| format!("no markdown files under {}", scope.display()))?;
            if strict {
                index.insert(root.clone());
            } else {
                for doc in docs {
                    index.insert(doc);
                }
            }
            root
        }
    };
    let root_hash = Index::hash(&root);

    let mut templates = Environment::new();
    templates.add_template("page.html", PAGE_TEMPLATE)?;
    templates.add_template("export.html", EXPORT_TEMPLATE)?;
    templates.add_template("style.css", STYLE)?;
    templates.add_template("mermaid.js", MERMAID_JS)?;

    if let Some(out) = export_to {
        let html = export::export(&index, &root, &scope, &root_hash, &templates)?;
        std::fs::write(&out, html).with_context(|| format!("writing {out}"))?;
        println!("Exported {} documents to {out}", index.documents().len());
        return Ok(());
    }

    // Index everything reachable up front so every page's URL works immediately.
    render::collect_links(&index, LinkMode::Server, true);

    let state = Arc::new(AppState {
        index,
        root_hash,
        templates,
    });

    let app = Router::new()
        .route("/", get(root_redirect))
        .route("/{hash}", get(serve))
        .with_state(state.clone());

    let (host, port) = parse_bind(bind.as_deref().unwrap_or(""))?;
    // Plain "localhost" can resolve to ::1 first; keep the loopback IPv4 default.
    let bind_host = if host == "localhost" {
        "127.0.0.1"
    } else {
        host.as_str()
    };
    let listener = tokio::net::TcpListener::bind((bind_host, port))
        .await
        .with_context(|| format!("binding {host}:{port}"))?;
    let local = listener.local_addr()?;
    println!("Serving {}", root.display());
    for h in display_hosts(local.ip(), &host) {
        println!("  http://{h}:{}/{}", local.port(), state.root_hash);
    }
    println!("  (the root path / redirects to the above)");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn root_redirect(State(state): State<Arc<AppState>>) -> Redirect {
    Redirect::temporary(&format!("/{}", state.root_hash))
}

async fn serve(State(state): State<Arc<AppState>>, Path(hash): Path<String>) -> Response {
    let Some(path) = state.index.get(&hash) else {
        return (StatusCode::NOT_FOUND, "not indexed").into_response();
    };

    if !is_markdown(&path) {
        return match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mime = mime_guess::from_path(&path).first_or_octet_stream();
                ([(header::CONTENT_TYPE, mime.to_string())], bytes).into_response()
            }
            Err(e) => (StatusCode::NOT_FOUND, format!("{}: {e}", path.display())).into_response(),
        };
    }

    let source = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::NOT_FOUND, format!("{}: {e}", path.display())).into_response();
        }
    };

    let rendered = {
        let state = state.clone();
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            // Re-read on each request so backlinks reflect the files as they are now.
            let backlinks = render::collect_links(&state.index, LinkMode::Server, false);
            render::render(&source, &path, &state.index, LinkMode::Server, &backlinks)
        })
        .await
    };
    let rendered = match rendered {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("render failed: {e}"),
            )
                .into_response();
        }
    };

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = rendered
        .toc
        .iter()
        .find(|t| t.level == 1)
        .map(|t| t.text.clone())
        .unwrap_or_else(|| file_name.clone());
    let documents: Vec<_> = state
        .index
        .documents()
        .into_iter()
        .map(|(h, p)| {
            context! {
                hash => h,
                name => p.file_name().map(|n| n.to_string_lossy().into_owned()),
                path => p.display().to_string(),
            }
        })
        .collect();

    let page = state.templates.get_template("page.html").and_then(|t| {
        t.render(context! {
            title,
            file_name,
            path => path.display().to_string(),
            hash,
            root_hash => &state.root_hash,
            toc => rendered.toc,
            content => rendered.html,
            has_mermaid => rendered.has_mermaid,
            documents,
        })
    });

    match page {
        Ok(html) => Html(html).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e:#}"),
        )
            .into_response(),
    }
}
