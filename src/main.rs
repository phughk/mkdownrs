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

use crate::index::{Index, is_markdown};

const PAGE_TEMPLATE: &str = include_str!("../templates/page.html");

struct AppState {
    index: Index,
    root_hash: String,
    templates: Environment<'static>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(arg), None) = (args.next(), args.next()) else {
        bail!("usage: mkdownrs <file.md>");
    };
    if arg == "-h" || arg == "--help" {
        println!("usage: mkdownrs <file.md>");
        return Ok(());
    }

    let root = render::resolve_file(&PathBuf::from(&arg))
        .with_context(|| format!("no such file: {arg}"))?;

    let index = Index::default();
    let root_hash = index.insert(root.clone());

    let mut templates = Environment::new();
    templates.add_template("page.html", PAGE_TEMPLATE)?;

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

    let base_dir = path.parent().unwrap_or(std::path::Path::new("/"));
    let rendered = render::render(&source, base_dir, &state.index);

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
