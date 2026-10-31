use std::collections::HashSet;
use std::path::Path;

use minijinja::{Environment, context};

use crate::index::Index;
use crate::render::{self, LinkMode};

const MERMAID_LIB: &str = include_str!("../assets/mermaid.min.js");

/// Render every document reachable from the index (following links as they
/// are discovered, but never leaving `scope`) into one standalone HTML page.
pub fn export(
    index: &Index,
    root: &Path,
    scope: &Path,
    root_hash: &str,
    templates: &Environment<'_>,
    show_files: bool,
) -> anyhow::Result<String> {
    let mut done = HashSet::new();
    let mut documents = Vec::new();
    let mut has_mermaid = false;

    // Indexes everything reachable within scope, so the loop below is stable
    // after one pass in practice.
    let backlinks = render::collect_links(index, LinkMode::Export(scope), true);

    // Rendering a document can index more documents, so loop until stable.
    loop {
        let pending: Vec<_> = index
            .documents()
            .into_iter()
            .filter(|(h, _)| !done.contains(h))
            .collect();
        if pending.is_empty() {
            break;
        }
        for (hash, path) in pending {
            done.insert(hash.clone());
            let source = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            let rendered =
                render::render(&source, &path, index, LinkMode::Export(scope), &backlinks);
            has_mermaid |= rendered.has_mermaid;
            documents.push((hash, path, rendered));
        }
    }
    documents.sort_by(|a, b| a.1.cmp(&b.1));

    let title = documents
        .iter()
        .find(|(h, ..)| h == root_hash)
        .and_then(|(.., r)| r.toc.iter().find(|t| t.level == 1))
        .map(|t| t.text.clone())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let documents: Vec<_> = documents
        .into_iter()
        .map(|(hash, path, rendered)| {
            context! {
                hash,
                name => path.file_name().map(|n| n.to_string_lossy().into_owned()),
                path => path.display().to_string(),
                toc => rendered.toc,
                content => rendered.html,
            }
        })
        .collect();

    let html = templates.get_template("export.html")?.render(context! {
        title,
        root_hash,
        show_files,
        documents,
        has_mermaid,
        // A literal `</script` would end the inline script early.
        mermaid_lib => MERMAID_LIB.replace("</script", "<\\/script"),
    })?;
    Ok(html)
}
