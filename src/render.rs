use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;

use crate::index::{Index, is_markdown};

#[derive(Debug, Serialize)]
pub struct TocEntry {
    pub level: u8,
    pub text: String,
    pub id: String,
}

pub struct Rendered {
    pub html: String,
    pub toc: Vec<TocEntry>,
    /// Whether the page contains mermaid diagrams (so the client script is needed).
    pub has_mermaid: bool,
}

/// How links to local files are rewritten.
#[derive(Clone, Copy)]
pub enum LinkMode<'a> {
    /// `/{hash}` routes served by the HTTP server.
    Server,
    /// Everything lives in one file: markdown links become `#/{hash}` routes
    /// and other files are inlined as `data:` URIs. Only files under the
    /// given directory are bundled; links pointing elsewhere are left alone.
    Export(&'a Path),
}

/// Render markdown to HTML, collecting a table of contents and rewriting
/// links to local files (indexing them on the way).
pub fn render(source: &str, base_dir: &Path, index: &Index, mode: LinkMode<'_>) -> Rendered {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_GFM;
    let mut events: Vec<Event> = Parser::new_ext(source, options).collect();

    let toc = assign_heading_ids(&mut events);
    let has_mermaid = convert_mermaid_blocks(&mut events);

    for event in &mut events {
        match event {
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. }) => {
                if let Some(rewritten) = rewrite_link(dest_url, base_dir, index, mode) {
                    *dest_url = rewritten.into();
                }
            }
            _ => {}
        }
    }

    let mut out = String::with_capacity(source.len() * 3 / 2);
    html::push_html(&mut out, events.into_iter());
    Rendered {
        html: out,
        toc,
        has_mermaid,
    }
}

/// Replace ```mermaid fenced code blocks with `<pre class="mermaid">` for
/// client-side rendering. Returns whether any were found.
fn convert_mermaid_blocks(events: &mut Vec<Event>) -> bool {
    let mut found = false;
    let mut out = Vec::with_capacity(events.len());
    let mut iter = std::mem::take(events).into_iter();
    while let Some(event) = iter.next() {
        let is_mermaid = matches!(
            &event,
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.split_whitespace().next() == Some("mermaid")
        );
        if !is_mermaid {
            out.push(event);
            continue;
        }
        found = true;
        let mut html = String::from(r#"<pre class="mermaid">"#);
        for inner in iter.by_ref() {
            match inner {
                Event::End(TagEnd::CodeBlock) => break,
                Event::Text(t) => escape_html(&mut html, &t),
                _ => {}
            }
        }
        html.push_str("</pre>\n");
        out.push(Event::Html(html.into()));
    }
    *events = out;
    found
}

fn escape_html(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Give every heading an id (keeping explicit `{#id}` ones) and return the TOC.
fn assign_heading_ids(events: &mut [Event]) -> Vec<TocEntry> {
    let mut toc = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    let mut i = 0;
    while i < events.len() {
        let Event::Start(Tag::Heading { level, .. }) = &events[i] else {
            i += 1;
            continue;
        };
        let level = *level as u8;

        let mut text = String::new();
        let mut j = i + 1;
        while j < events.len() && !matches!(events[j], Event::End(TagEnd::Heading(_))) {
            match &events[j] {
                Event::Text(t) | Event::Code(t) | Event::InlineMath(t) => text.push_str(t),
                Event::SoftBreak | Event::HardBreak => text.push(' '),
                _ => {}
            }
            j += 1;
        }
        let text = text.trim().to_string();

        let Event::Start(Tag::Heading { id, .. }) = &mut events[i] else {
            unreachable!()
        };
        let final_id = match id {
            Some(existing) => existing.to_string(),
            None => {
                let base = slugify(&text);
                let base = if base.is_empty() {
                    "section".to_string()
                } else {
                    base
                };
                let n = seen.entry(base.clone()).or_insert(0);
                let slug = if *n == 0 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                };
                *n += 1;
                *id = Some(CowStr::from(slug.clone()));
                slug
            }
        };

        toc.push(TocEntry {
            level,
            text,
            id: final_id,
        });
        i = j + 1;
    }
    toc
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            slug.push(c);
        } else if (c == ' ' || c == '-') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Resolve a link destination against `base_dir`. If it points to an existing
/// local file, index it and return its replacement URL (keeping any `#fragment`).
fn rewrite_link(dest: &str, base_dir: &Path, index: &Index, mode: LinkMode<'_>) -> Option<String> {
    if dest.is_empty() || dest.starts_with('#') || dest.starts_with("//") || has_scheme(dest) {
        return None;
    }

    let (path_part, fragment) = match dest.find(['#', '?']) {
        Some(pos) if dest[pos..].starts_with('#') => (&dest[..pos], &dest[pos..]),
        // Drop query strings; they mean nothing for local files.
        Some(pos) => (
            &dest[..pos],
            dest[pos..].find('#').map_or("", |f| &dest[pos + f..]),
        ),
        None => (dest, ""),
    };

    let decoded = percent_decode(path_part);
    let candidate = if decoded.starts_with('/') {
        PathBuf::from(&decoded)
    } else {
        base_dir.join(&decoded)
    };
    let resolved = resolve_file(&candidate)?;
    if let LinkMode::Export(scope) = mode
        && !resolved.starts_with(scope)
    {
        return None;
    }
    if matches!(mode, LinkMode::Export(_)) && !is_markdown(&resolved) {
        let bytes = std::fs::read(&resolved).ok()?;
        let mime = mime_guess::from_path(&resolved).first_or_octet_stream();
        return Some(format!("data:{mime};base64,{}", base64(&bytes)));
    }
    let hash = index.insert(resolved);
    Some(match mode {
        LinkMode::Server => format!("/{hash}{fragment}"),
        LinkMode::Export(_) => format!("#/{hash}{}", fragment.replace('#', "/")),
    })
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Canonicalize a path to an existing file. Directories resolve to a
/// README/index markdown file inside them when present.
pub fn resolve_file(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    if canonical.is_file() {
        return Some(canonical);
    }
    if canonical.is_dir() {
        for name in ["README.md", "readme.md", "index.md"] {
            let inner = canonical.join(name);
            if inner.is_file() && is_markdown(&inner) {
                return Some(inner);
            }
        }
    }
    None
}

fn has_scheme(dest: &str) -> bool {
    match dest.find(':') {
        Some(pos) if pos > 1 => dest[..pos]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')),
        _ => false,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  a -- b  "), "a-b");
        assert_eq!(slugify("snake_case Thing"), "snake_case-thing");
    }

    #[test]
    fn toc_and_duplicate_ids() {
        let dir = std::env::temp_dir();
        let r = render(
            "# Title\n## Part\n## Part\n### `code` bit {#custom}\n",
            &dir,
            &Index::default(),
            LinkMode::Server,
        );
        let ids: Vec<_> = r.toc.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["title", "part", "part-1", "custom"]);
        assert_eq!(r.toc[3].text, "code bit");
        assert_eq!(r.toc[3].level, 3);
        assert!(r.html.contains(r#"<h2 id="part-1">"#));
    }

    #[test]
    fn callouts() {
        let r = render(
            "> [!WARNING]\n> Careful.\n\n> plain quote\n",
            &std::env::temp_dir(),
            &Index::default(),
            LinkMode::Server,
        );
        assert!(
            r.html
                .contains(r#"<blockquote class="markdown-alert-warning">"#),
            "{}",
            r.html
        );
        assert!(!r.html.contains("[!WARNING]"));
        assert!(r.html.contains("<blockquote>\n<p>plain quote</p>"));
    }

    #[test]
    fn mermaid_blocks() {
        let dir = std::env::temp_dir();
        let r = render(
            "```mermaid\ngraph TD\n  A-->B & C<br/>\n```\n\n```rust\nfn main() {}\n```\n",
            &dir,
            &Index::default(),
            LinkMode::Server,
        );
        assert!(r.has_mermaid);
        assert!(
            r.html
                .contains("<pre class=\"mermaid\">graph TD\n  A--&gt;B &amp; C&lt;br/&gt;\n</pre>"),
            "{}",
            r.html
        );
        assert!(r.html.contains(r#"<code class="language-rust">"#));

        let r = render(
            "```\nplain\n```\n",
            &dir,
            &Index::default(),
            LinkMode::Server,
        );
        assert!(!r.has_mermaid);
    }

    #[test]
    fn local_links_are_indexed_and_rewritten() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(dir.join("docs/other file.md"), "# Other").unwrap();
        let other = dir.join("docs/other file.md").canonicalize().unwrap();

        let index = Index::default();
        let md = format!(
            "[rel](docs/other%20file.md#x) [abs](<{}>) [web](https://example.com) [anchor](#top) [missing](nope.md)",
            other.display()
        );
        let r = render(&md, &dir, &index, LinkMode::Server);
        let hash = Index::hash(&other);

        assert!(
            r.html.contains(&format!(r#"href="/{hash}#x""#)),
            "{}",
            r.html
        );
        assert!(r.html.contains(&format!(r#"href="/{hash}""#)));
        assert!(r.html.contains(r#"href="https://example.com""#));
        assert!(r.html.contains(r##"href="#top""##));
        assert!(r.html.contains(r#"href="nope.md""#));
        assert_eq!(index.get(&hash), Some(other));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn export_links_are_routes_and_data_uris() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-export-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("other.md"), "# Other").unwrap();
        fs::write(dir.join("a.txt"), "hello").unwrap();
        let other = dir.join("other.md").canonicalize().unwrap();

        let r = render(
            "[o](other.md#x) ![a](a.txt)",
            &dir,
            &Index::default(),
            LinkMode::Export(&dir.canonicalize().unwrap()),
        );
        let hash = Index::hash(&other);
        assert!(
            r.html.contains(&format!(r##"href="#/{hash}/x""##)),
            "{}",
            r.html
        );
        assert!(
            r.html.contains(r#"src="data:text/plain;base64,aGVsbG8=""#),
            "{}",
            r.html
        );
    }

    #[test]
    fn export_ignores_links_outside_scope() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-scope-{}", std::process::id()));
        fs::create_dir_all(dir.join("inner")).unwrap();
        fs::write(dir.join("outer.md"), "# Outer").unwrap();
        let inner = dir.join("inner").canonicalize().unwrap();

        let index = Index::default();
        let r = render(
            "[up](../outer.md)",
            &inner,
            &index,
            LinkMode::Export(&inner),
        );
        assert!(r.html.contains(r#"href="../outer.md""#), "{}", r.html);
        assert!(index.documents().is_empty());
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
