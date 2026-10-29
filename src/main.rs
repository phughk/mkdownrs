mod export;
mod index;
mod render;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use minijinja::{Environment, context};

use crate::index::{Index, is_markdown, scan_markdown};
use crate::render::LinkMode;

const PAGE_TEMPLATE: &str = include_str!("../templates/page.html");
const EXPORT_TEMPLATE: &str = include_str!("../templates/export.html");
const STYLE: &str = include_str!("../templates/style.css");
const MERMAID_JS: &str = include_str!("../templates/mermaid.js");
const USAGE: &str = "usage: mkdownrs [file.md|dir] [-s|--strict] [--export out.html]";

struct AppState {
    index: Index,
    root_hash: String,
    templates: Environment<'static>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut file = None;
    let mut export_to = None;
    let mut strict = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                println!(
                    "  without a file, or given a directory, indexes all markdown files under it"
                );
                println!(
                    "  -s, --strict       for a directory, only index the most sensible markdown file and what it links to"
                );
                println!(
                    "  --export out.html  write everything as one standalone HTML file instead of serving"
                );
                return Ok(());
            }
            "--export" => {
                export_to = Some(
                    args.next()
                        .with_context(|| format!("--export needs a path\n{USAGE}"))?,
                );
            }
            "-s" | "--strict" => strict = true,
            _ if file.is_none() && !arg.starts_with('-') => file = Some(arg),
            _ => bail!("{USAGE}"),
        }
    }

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
            let root = docs
                .iter()
                .find(|p| p.parent() == Some(scope.as_path()) && is_readme(p))
                .or_else(|| docs.first())
                .cloned()
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
    render::collect_links(&index, LinkMode::Server);

    let state = Arc::new(AppState {
        index,
        root_hash,
        templates,
    });

    let app = Router::new()
        .route("/", get(root_redirect))
        .route("/{hash}", get(serve))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    println!("Serving {}", root.display());
    println!("  http://localhost:{port}/{}", state.root_hash);
    println!("  http://localhost:{port}/ (redirects to the above)");

    axum::serve(listener, app).await?;
    Ok(())
}

fn is_readme(path: &std::path::Path) -> bool {
    path.file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("readme"))
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
            let backlinks = render::collect_links(&state.index, LinkMode::Server);
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
