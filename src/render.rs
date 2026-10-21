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

/// A link from one markdown document (or section of it) to another.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Hash of the linking document.
    pub source: String,
    /// Heading `(id, text)` the link appears under, if any.
    pub section: Option<(String, String)>,
    /// Plain text of the block (paragraph, list item, ...) containing the link.
    pub snippet: String,
    /// Hash of the linked document.
    pub target: String,
    /// Linked heading id, without the `#`.
    pub fragment: Option<String>,
}

const SNIPPET_LEN: usize = 200;

/// Render markdown to HTML, collecting a table of contents and rewriting
/// links to local files into `/{hash}` routes (indexing them on the way).
/// `backlinks` pointing at this document are listed at the end of the
/// section they target, or at the end of the page.
pub fn render(source: &str, path: &Path, index: &Index, backlinks: &[Link]) -> Rendered {
    let (mut events, toc, _) = analyze(source, path, index);
    let has_mermaid = convert_mermaid_blocks(&mut events);
    insert_backlinks(&mut events, &Index::hash(path), backlinks, index);

    let mut out = String::with_capacity(source.len() * 3 / 2);
    html::push_html(&mut out, events.into_iter());
    Rendered {
        html: out,
        toc,
        has_mermaid,
    }
}

/// Outgoing links from a document to markdown documents (itself included).
pub fn links(source: &str, path: &Path, index: &Index) -> Vec<Link> {
    analyze(source, path, index).2
}

fn analyze<'a>(
    source: &'a str,
    path: &Path,
    index: &Index,
) -> (Vec<Event<'a>>, Vec<TocEntry>, Vec<Link>) {
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
    let mut section: Option<(String, String)> = None;
    let mut headings = toc.iter();
    let mut in_code = false;
    // Open text blocks: their text so far, and the links waiting on it.
    let mut blocks: Vec<(String, Vec<Link>)> = Vec::new();

    for event in &mut events {
        match event {
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
                    let snippet = snippet(&text);
                    links.extend(pending.into_iter().map(|l| Link {
                        snippet: snippet.clone(),
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
                if let Some((hash, fragment, _)) = rewrite_link(dest_url, base_dir, index) {
                    *dest_url = format!("/{hash}{fragment}").into();
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let target = if let Some(frag) = dest_url.strip_prefix('#') {
                    Some((self_hash.clone(), frag.to_string()))
                } else if let Some((hash, fragment, resolved)) =
                    rewrite_link(dest_url, base_dir, index)
                {
                    let fragment = fragment.trim_start_matches('#').to_string();
                    *dest_url = match fragment.as_str() {
                        "" => format!("/{hash}"),
                        f => format!("/{hash}#{f}"),
                    }
                    .into();
                    is_markdown(&resolved).then_some((hash, fragment))
                } else {
                    None
                };
                if let (Some((target, fragment)), Some((_, pending))) = (target, blocks.last_mut())
                {
                    pending.push(Link {
                        source: self_hash.clone(),
                        section: section.clone(),
                        snippet: String::new(),
                        target,
                        fragment: (!fragment.is_empty()).then(|| percent_decode(&fragment)),
                    });
                }
            }
            _ => {}
        }
    }
    links.dedup();
    (events, toc, links)
}

fn snippet(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(SNIPPET_LEN) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text,
    }
}

/// Insert "Linked from" lists: at the end of each targeted section's own
/// content (before the next heading), and for whole-document links at the end.
fn insert_backlinks(events: &mut Vec<Event>, self_hash: &str, backlinks: &[Link], index: &Index) {
    let headings: Vec<(usize, String)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Event::Start(Tag::Heading { id: Some(id), .. }) => Some((i, id.to_string())),
            _ => None,
        })
        .collect();

    // Insertion position -> title and links to list there.
    let mut at: Vec<(usize, &str, Vec<&Link>)> = Vec::new();
    let mut page: Vec<&Link> = Vec::new();
    for link in backlinks.iter().filter(|l| l.target == self_hash) {
        let heading = link
            .fragment
            .as_ref()
            .and_then(|f| headings.iter().position(|(_, id)| id == f));
        match heading {
            Some(k) => {
                let pos = headings.get(k + 1).map_or(events.len(), |(i, _)| *i);
                match at.iter_mut().find(|(p, _, _)| *p == pos) {
                    Some((_, _, list)) => list.push(link),
                    None => at.push((pos, "Linked from", vec![link])),
                }
            }
            // Links into this page that point nowhere in particular aren't backlinks.
            None if link.source == self_hash => {}
            None => page.push(link),
        }
    }
    if !page.is_empty() {
        at.push((events.len(), "Pages linking here", page));
    }

    // Insert from the back so earlier positions stay valid; the stable sort
    // keeps the page-level list after a last section's list.
    at.sort_by_key(|(pos, _, _)| *pos);
    for (pos, title, list) in at.into_iter().rev() {
        let html = backlinks_html(title, &list, self_hash, index);
        events.insert(pos, Event::Html(html.into()));
    }
}

fn backlinks_html(title: &str, links: &[&Link], self_hash: &str, index: &Index) -> String {
    let mut html = format!(
        "<aside class=\"backlinks\">\n<div class=\"backlinks-title\">{title}</div>\n<ul>\n"
    );
    for link in links {
        let mut label = String::new();
        if link.source != self_hash {
            let name = index
                .get(&link.source)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| link.source.clone());
            label.push_str(&name);
        }
        if let Some((_, text)) = &link.section {
            if !label.is_empty() {
                label.push_str(" › ");
            }
            label.push_str(text);
        }
        if label.is_empty() {
            label.push_str("This page");
        }
        let href = match (&link.section, link.source == self_hash) {
            (Some((id, _)), true) => format!("#{id}"),
            (Some((id, _)), false) => format!("/{}#{id}", link.source),
            (None, _) => format!("/{}", link.source),
        };

        html.push_str("<li><a href=\"");
        escape_html(&mut html, &href);
        html.push_str("\">");
        escape_html(&mut html, &label);
        html.push_str("</a>");
        if !link.snippet.is_empty() {
            html.push_str("<div class=\"snippet\">");
            escape_html(&mut html, &link.snippet);
            html.push_str("</div>");
        }
        html.push_str("</li>\n");
    }
    html.push_str("</ul>\n</aside>\n");
    html
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
/// local file, index it and return its hash, the `#fragment` (possibly empty)
/// and the resolved path.
fn rewrite_link<'d>(
    dest: &'d str,
    base_dir: &Path,
    index: &Index,
) -> Option<(String, &'d str, PathBuf)> {
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
    let hash = index.insert(resolved.clone());
    Some((hash, fragment, resolved))
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
            &[],
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
            &dir.join("t.md"),
            &Index::default(),
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
        let r = render(&md, &dir.join("t.md"), &index, &[]);
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
    fn backlinks_land_in_the_targeted_section() {
        let dir = std::env::temp_dir().join(format!("mkdownrs-bl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a_src = "# A\n## Intro\nSee [setup](b.md#setup) & 1 < 2.\n\n- [whole](b.md)\n";
        let b_src = "# B\n## Setup\nSteps. Back to [usage](#usage).\n## Usage\nUse it.\n";
        fs::write(dir.join("a.md"), a_src).unwrap();
        fs::write(dir.join("b.md"), b_src).unwrap();
        let a = dir.join("a.md").canonicalize().unwrap();
        let b = dir.join("b.md").canonicalize().unwrap();
        let (a_hash, b_hash) = (Index::hash(&a), Index::hash(&b));

        let index = Index::default();
        index.insert(a.clone());
        let mut all = links(a_src, &a, &index);
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
        all.extend(links(b_src, &b, &index));

        let html = render(b_src, &b, &index, &all).html;
        let setup = html.find(r#"<h2 id="setup">"#).unwrap();
        let usage = html.find(r#"<h2 id="usage">"#).unwrap();
        let from_a = html
            .find(&format!(r#"<a href="/{a_hash}#intro">a.md › Intro</a>"#))
            .expect(&html);
        assert!(setup < from_a && from_a < usage, "{html}");
        assert!(html.contains("See setup &amp; 1 &lt; 2."));

        // Same-page link shows up under Usage, pointing back to Setup.
        let from_self = html.find(r##"<a href="#setup">Setup</a>"##).unwrap();
        assert!(from_self > usage);

        // Whole-document link goes at the end of the page.
        let whole = html
            .rfind(&format!(r#"<a href="/{a_hash}#intro">"#))
            .unwrap();
        assert!(whole > from_self && html[whole..].contains("whole"));
        let page_title = html.find("Pages linking here").unwrap();
        assert!(from_self < page_title && page_title < whole);

        fs::remove_dir_all(&dir).unwrap();
    }
}
