use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;

use crate::index::{Index, is_markdown, root_document, scan_markdown};

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

impl LinkMode<'_> {
    /// URL of a document, or of a heading within it.
    fn doc_url(self, hash: &str, fragment: Option<&str>) -> String {
        match (self, fragment) {
            (LinkMode::Server, None) => format!("/{hash}"),
            (LinkMode::Server, Some(f)) => format!("/{hash}#{f}"),
            (LinkMode::Export(_), None) => format!("#/{hash}"),
            (LinkMode::Export(_), Some(f)) => format!("#/{hash}/{f}"),
        }
    }
}

/// A link from one markdown document (or section of it) to another.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Hash of the linking document.
    pub source: String,
    /// Heading `(id, text)` the link appears under, if any.
    pub section: Option<(String, String)>,
    /// The sentence containing the link, as plain text.
    pub snippet: String,
    /// Hash of the linked document.
    pub target: String,
    /// Linked heading id, without the `#`.
    pub fragment: Option<String>,
}

const SNIPPET_LEN: usize = 200;

/// Render markdown to HTML, collecting a table of contents and rewriting
/// links to local files (indexing them on the way). `backlinks` pointing at
/// this document are listed at the end of the section they target, or at
/// the end of the page.
pub fn render(
    source: &str,
    path: &Path,
    index: &Index,
    mode: LinkMode<'_>,
    backlinks: &[Link],
) -> Rendered {
    let (mut events, toc, _, _) = analyze(source, path, index, mode);
    let has_mermaid = convert_mermaid_blocks(&mut events);
    insert_backlinks(&mut events, &Index::hash(path), backlinks, index, mode);

    let mut out = String::with_capacity(source.len() * 3 / 2);
    html::push_html(&mut out, events.into_iter());
    Rendered {
        html: out,
        toc,
        has_mermaid,
    }
}

/// Outgoing links from a document to markdown documents (itself included).
#[cfg(test)]
fn links(source: &str, path: &Path, index: &Index, mode: LinkMode<'_>) -> Vec<Link> {
    analyze(source, path, index, mode).2
}

/// Read every markdown document reachable from the index (indexing new ones
/// as they're found) and return all their links. With `warn`, broken links
/// are reported on stderr.
pub fn collect_links(index: &Index, mode: LinkMode<'_>, warn: bool) -> Vec<Link> {
    let mut queue: Vec<_> = index.documents().into_iter().map(|(h, _)| h).collect();
    let mut seen: HashSet<String> = queue.iter().cloned().collect();
    let mut all = Vec::new();

    while let Some(hash) = queue.pop() {
        let Some(path) = index.get(&hash) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (_, _, found, broken) = analyze(&source, &path, index, mode);
        if warn {
            for dest in broken {
                eprintln!("warning: {}: broken link `{dest}`", path.display());
            }
        }
        for link in found {
            if seen.insert(link.target.clone()) {
                queue.push(link.target.clone());
            }
            all.push(link);
        }
    }
    all
}

fn analyze<'a>(
    source: &'a str,
    path: &Path,
    index: &Index,
    mode: LinkMode<'_>,
) -> (Vec<Event<'a>>, Vec<TocEntry>, Vec<Link>, Vec<String>) {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_GFM;
    let mut events: Vec<Event> = Parser::new_ext(source, options).collect();

    let toc = assign_heading_ids(&mut events);
    let self_hash = Index::hash(path);
    let base_dir = path.parent().unwrap_or(Path::new("/"));

    let mut links = Vec::new();
    let mut broken = Vec::new();
    let mut in_broken = false;
    let mut section: Option<(String, String)> = None;
    let mut headings = toc.iter();
    let mut in_code = false;
    // Open text blocks: their text so far, and the links waiting on it
    // (with the text offset each link starts at).
    let mut blocks: Vec<(String, Vec<(Link, usize)>)> = Vec::new();

    for event in &mut events {
        let mut replacement = None;
        match &mut *event {
            Event::Start(Tag::Heading { .. }) => {
                section = headings.next().map(|t| (t.id.clone(), t.text.clone()));
                blocks.push(Default::default());
            }
            Event::Start(Tag::Paragraph | Tag::Item | Tag::TableCell) => {
                blocks.push(Default::default());
            }
            Event::End(
                TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::Item | TagEnd::TableCell,
            ) => {
                if let Some((text, pending)) = blocks.pop() {
                    links.extend(pending.into_iter().map(|(l, offset)| Link {
                        snippet: sentence_at(&text, offset),
                        ..l
                    }));
                }
            }
            Event::Start(Tag::CodeBlock(_)) => in_code = true,
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) if !in_code => {
                if let Some((text, _)) = blocks.last_mut() {
                    text.push_str(t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((text, _)) = blocks.last_mut() {
                    text.push(' ');
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                if let Rewritten::Local(url, _) = rewrite_link(dest_url, base_dir, index, mode) {
                    *dest_url = url.into();
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let target = if let Some(frag) = dest_url.strip_prefix('#') {
                    Some((self_hash.clone(), frag.to_string()))
                } else {
                    match rewrite_link(dest_url, base_dir, index, mode) {
                        Rewritten::Local(url, doc) => {
                            *dest_url = url.into();
                            doc
                        }
                        Rewritten::Broken => {
                            let mut title = String::new();
                            escape_html(&mut title, &format!("Broken link: {dest_url}"));
                            broken.push(dest_url.to_string());
                            in_broken = true;
                            replacement = Some(Event::InlineHtml(
                                format!("<span class=\"broken-link\" title=\"{title}\">⚠ ").into(),
                            ));
                            None
                        }
                        Rewritten::External => None,
                    }
                };
                if let (Some((target, fragment)), Some((text, pending))) =
                    (target, blocks.last_mut())
                {
                    let link = Link {
                        source: self_hash.clone(),
                        section: section.clone(),
                        snippet: String::new(),
                        target,
                        fragment: (!fragment.is_empty()).then(|| percent_decode(&fragment)),
                    };
                    pending.push((link, text.len()));
                }
            }
            Event::End(TagEnd::Link) if in_broken => {
                in_broken = false;
                replacement = Some(Event::InlineHtml("</span>".into()));
            }
            _ => {}
        }
        if let Some(r) = replacement {
            *event = r;
        }
    }
    links.dedup();
    (events, toc, links, broken)
}

/// The sentence of `text` around byte `offset`, whitespace-collapsed.
fn sentence_at(text: &str, offset: usize) -> String {
    let is_end = |i: usize, c: char| {
        matches!(c, '.' | '!' | '?')
            && text[i + c.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    };
    let start = text[..offset]
        .char_indices()
        .rfind(|&(i, c)| is_end(i, c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let end = text[offset..]
        .char_indices()
        .find(|&(i, c)| is_end(offset + i, c))
        .map_or(text.len(), |(i, c)| offset + i + c.len_utf8());

    let sentence = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match sentence.char_indices().nth(SNIPPET_LEN) {
        Some((cut, _)) => format!("{}…", sentence[..cut].trim_end()),
        None => sentence,
    }
}

/// Insert backlink pills at the end of each targeted section's own content
/// (before the next heading), and for whole-document links at the end.
fn insert_backlinks(
    events: &mut Vec<Event>,
    self_hash: &str,
    backlinks: &[Link],
    index: &Index,
    mode: LinkMode<'_>,
) {
    let headings: Vec<(usize, String)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Event::Start(Tag::Heading { id: Some(id), .. }) => Some((i, id.to_string())),
            _ => None,
        })
        .collect();

    // Insertion position -> links to show there.
    let mut at: Vec<(usize, Vec<&Link>)> = Vec::new();
    for link in backlinks.iter().filter(|l| l.target == self_hash) {
        let heading = link
            .fragment
            .as_ref()
            .and_then(|f| headings.iter().position(|(_, id)| id == f));
        let pos = match heading {
            Some(k) => headings.get(k + 1).map_or(events.len(), |(i, _)| *i),
            // Links into this page that point nowhere in particular aren't backlinks.
            None if link.source == self_hash => continue,
            None => events.len(),
        };
        match at.iter_mut().find(|(p, _)| *p == pos) {
            Some((_, list)) => list.push(link),
            None => at.push((pos, vec![link])),
        }
    }

    // Insert from the back so earlier positions stay valid.
    at.sort_by_key(|(pos, _)| *pos);
    for (pos, list) in at.into_iter().rev() {
        let html = backlinks_html(&list, self_hash, index, mode);
        events.insert(pos, Event::Html(html.into()));
    }
}

/// A row of `[file | heading | sentence]` pills, each one link back to
/// where the link was made.
fn backlinks_html(links: &[&Link], self_hash: &str, index: &Index, mode: LinkMode<'_>) -> String {
    let segment = |html: &mut String, class: &str, text: &str| {
        html.push_str(&format!("<span class=\"{class}\">"));
        escape_html(html, text);
        html.push_str("</span>");
    };
    let mut html = String::from("<div class=\"backlinks\">\n");
    for link in links {
        let name = index
            .get(&link.source)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| link.source.clone());
        let id = link.section.as_ref().map(|(id, _)| id.as_str());
        let href = match (mode, id) {
            // Same-page anchors avoid a reload; the export page routes them itself.
            (LinkMode::Server, Some(id)) if link.source == self_hash => format!("#{id}"),
            _ => mode.doc_url(&link.source, id),
        };

        html.push_str("<a class=\"backlink\" href=\"");
        escape_html(&mut html, &href);
        if !link.snippet.is_empty() {
            html.push_str("\" title=\"");
            escape_html(&mut html, &link.snippet);
        }
        html.push_str("\">");
        segment(&mut html, "file", &name);
        if let Some((_, text)) = &link.section {
            segment(&mut html, "section", text);
        }
        if !link.snippet.is_empty() {
            segment(&mut html, "excerpt", &link.snippet);
        }
        html.push_str("</a>\n");
    }
    html.push_str("</div>\n");
    html
}

/// Replace ```mermaid fenced code blocks with `<pre class="mermaid">` for
/// client-side rendering, alongside a hidden copy of the source and a
/// Diagram/Source switch. Returns whether any were found.
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
        let mut source = String::new();
        for inner in iter.by_ref() {
            match inner {
                Event::End(TagEnd::CodeBlock) => break,
                Event::Text(t) => escape_html(&mut source, &t),
                _ => {}
            }
        }
        // mermaid replaces the diagram's text with an SVG, so keep a copy to show.
        let html = format!(
            concat!(
                "<div class=\"mermaid-block\" data-view=\"diagram\">\n",
                "<div class=\"mermaid-switch\" role=\"tablist\">",
                "<button type=\"button\" role=\"tab\" data-view=\"diagram\" aria-selected=\"true\">Diagram</button>",
                "<button type=\"button\" role=\"tab\" data-view=\"source\" aria-selected=\"false\">Source</button>",
                "</div>\n",
                "<pre class=\"mermaid\">{source}</pre>\n",
                "<pre class=\"mermaid-source\" hidden><code class=\"language-mermaid\">{source}</code></pre>\n",
                "</div>\n",
            ),
            source = source
        );
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

/// What a link destination turned out to be.
enum Rewritten {
    /// Not a local path (URL, anchor, or outside the export scope): left alone.
    External,
    /// A local path that doesn't exist.
    Broken,
    /// An existing local file: its replacement URL, plus — for markdown
    /// documents — its hash and `#fragment` (without the `#`, possibly empty).
    Local(String, Option<(String, String)>),
}

/// Resolve a link destination against `base_dir`. Existing local files are
/// indexed. A directory is treated as on the command line: every markdown
/// file under it is indexed and the link opens its root document.
fn rewrite_link(dest: &str, base_dir: &Path, index: &Index, mode: LinkMode<'_>) -> Rewritten {
    if dest.is_empty() || dest.starts_with('#') || dest.starts_with("//") || has_scheme(dest) {
        return Rewritten::External;
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
    let Ok(canonical) = candidate.canonicalize() else {
        return Rewritten::Broken;
    };
    let resolved = if canonical.is_dir() {
        let docs = scan_markdown(&canonical);
        let Some(root) = root_document(&docs, &canonical) else {
            return Rewritten::Broken;
        };
        for doc in docs {
            if !matches!(mode, LinkMode::Export(scope) if !doc.starts_with(scope)) {
                index.insert(doc);
            }
        }
        root
    } else {
        canonical
    };
    if let LinkMode::Export(scope) = mode
        && !resolved.starts_with(scope)
    {
        return Rewritten::External;
    }
    let markdown = is_markdown(&resolved);
    if matches!(mode, LinkMode::Export(_)) && !markdown {
        let Ok(bytes) = std::fs::read(&resolved) else {
            return Rewritten::Broken;
        };
        let mime = mime_guess::from_path(&resolved).first_or_octet_stream();
        return Rewritten::Local(format!("data:{mime};base64,{}", base64(&bytes)), None);
    }
    let hash = index.insert(resolved);
    let fragment = fragment.trim_start_matches('#');
    let url = mode.doc_url(&hash, (!fragment.is_empty()).then_some(fragment));
    Rewritten::Local(url, markdown.then(|| (hash, fragment.to_string())))
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
            &dir.join("t.md"),
            &Index::default(),
            LinkMode::Server,
            &[],
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
            &std::env::temp_dir().join("t.md"),
            &Index::default(),
            LinkMode::Server,
            &[],
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
            &dir.join("t.md"),
            &Index::default(),
            LinkMode::Server,
            &[],
        );
        assert!(r.has_mermaid);
        assert!(
            r.html
                .contains("<pre class=\"mermaid\">graph TD\n  A--&gt;B &amp; C&lt;br/&gt;\n</pre>"),
            "{}",
            r.html
        );
        assert!(
            r.html.contains(concat!(
                "<pre class=\"mermaid-source\" hidden><code class=\"language-mermaid\">",
                "graph TD\n  A--&gt;B &amp; C&lt;br/&gt;\n</code></pre>"
            )),
            "{}",
            r.html
        );
        assert!(r.html.contains(r#"<code class="language-rust">"#));

        let r = render(
            "```\nplain\n```\n",
            &dir.join("t.md"),
            &Index::default(),
            LinkMode::Server,
            &[],
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
        let r = render(&md, &dir.join("t.md"), &index, LinkMode::Server, &[]);
        let hash = Index::hash(&other);

        assert!(
            r.html.contains(&format!(r#"href="/{hash}#x""#)),
            "{}",
            r.html
        );
        assert!(r.html.contains(&format!(r#"href="/{hash}""#)));
        assert!(r.html.contains(r#"href="https://example.com""#));
        assert!(r.html.contains(r##"href="#top""##));
        assert!(!r.html.contains(r#"href="nope.md""#));
        assert!(r.html.contains(
            r#"<span class="broken-link" title="Broken link: nope.md">⚠ missing</span>"#
        ));
        assert_eq!(index.get(&hash), Some(other));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_links_open_root_and_index_contents() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-dirtest-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::write(dir.join("sub/README.md"), "# Sub").unwrap();
        fs::write(dir.join("sub/deep/a.md"), "# A").unwrap();
        let readme = dir.join("sub/README.md").canonicalize().unwrap();
        let deep = dir.join("sub/deep/a.md").canonicalize().unwrap();

        let index = Index::default();
        let r = render("[d](sub)", &dir.join("t.md"), &index, LinkMode::Server, &[]);
        assert!(
            r.html
                .contains(&format!(r#"href="/{}""#, Index::hash(&readme)))
        );
        assert!(index.get(&Index::hash(&deep)).is_some());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sentence_excerpts() {
        let text = "First thing. Then see x here! Last one.";
        assert_eq!(
            sentence_at(text, text.find('x').unwrap()),
            "Then see x here!"
        );
        assert_eq!(sentence_at(text, 0), "First thing.");
        assert_eq!(sentence_at("v1.2 is out", 5), "v1.2 is out");
    }

    #[test]
    fn backlinks_land_in_the_targeted_section() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-bl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a_src = "# A\n## Intro\nUnrelated. See [setup](b.md#setup) & 1 < 2. After.\n\n- [whole](b.md)\n";
        let b_src = "# B\n## Setup\nSteps. Back to [usage](#usage).\n## Usage\nUse it.\n";
        fs::write(dir.join("a.md"), a_src).unwrap();
        fs::write(dir.join("b.md"), b_src).unwrap();
        let a = dir.join("a.md").canonicalize().unwrap();
        let b = dir.join("b.md").canonicalize().unwrap();
        let (a_hash, b_hash) = (Index::hash(&a), Index::hash(&b));

        let index = Index::default();
        index.insert(a.clone());
        let mut all = links(a_src, &a, &index, LinkMode::Server);
        assert_eq!(
            all[0],
            Link {
                source: a_hash.clone(),
                section: Some(("intro".into(), "Intro".into())),
                snippet: "See setup & 1 < 2.".into(),
                target: b_hash.clone(),
                fragment: Some("setup".into()),
            }
        );
        assert_eq!(all[1].fragment, None);
        assert_eq!(all[1].snippet, "whole");
        all.extend(links(b_src, &b, &index, LinkMode::Server));

        let html = render(b_src, &b, &index, LinkMode::Server, &all).html;
        let setup = html.find(r#"<h2 id="setup">"#).unwrap();
        let usage = html.find(r#"<h2 id="usage">"#).unwrap();
        let pill = format!(
            concat!(
                r#"<a class="backlink" href="/{a}#intro" title="See setup &amp; 1 &lt; 2.">"#,
                r#"<span class="file">a.md</span><span class="section">Intro</span>"#,
                r#"<span class="excerpt">See setup &amp; 1 &lt; 2.</span></a>"#,
            ),
            a = a_hash
        );
        let from_a = html.find(&pill).expect(&html);
        assert!(setup < from_a && from_a < usage, "{html}");

        // Same-page link shows up under Usage, pointing back to Setup.
        let from_self = html.find(r##"<a class="backlink" href="#setup""##).unwrap();
        assert!(from_self > usage);

        // Whole-document link goes at the end of the page.
        let whole = html.rfind(r#"<span class="excerpt">whole</span>"#).unwrap();
        assert!(whole > usage);

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
            &dir.join("t.md"),
            &Index::default(),
            LinkMode::Export(&dir.canonicalize().unwrap()),
            &[],
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
            &inner.join("t.md"),
            &index,
            LinkMode::Export(&inner),
            &[],
        );
        assert!(r.html.contains(r#"href="../outer.md""#), "{}", r.html);
        assert!(index.documents().is_empty());
    }

    #[test]
    fn export_backlinks_use_routes() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-export-bl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        let a_src = "# A\nSee [b](b.md#top).\n";
        fs::write(&a, a_src).unwrap();
        fs::write(&b, "# Top\n").unwrap();

        let index = Index::default();
        let mode = LinkMode::Export(&dir);
        let all = links(a_src, &a, &index, mode);
        let html = render("# Top\n", &b, &index, mode, &all).html;
        let a_hash = Index::hash(&a);
        assert!(
            html.contains(&format!(r##"<a class="backlink" href="#/{a_hash}/a""##)),
            "{html}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
