//! Serving the web app with a strict Content Security Policy.
//!
//! The generated pages (Nuxt) contain a few inline scripts: the import map and the runtime config.
//! They are allowed by their SHA-256 hash, computed once at startup from the files on disk; any
//! other inline script (e.g. injected markup) is blocked by the browser.

use std::path::Path;

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// CSP for API responses and when no web app is configured.
pub const MINIMAL_CSP: &str = "frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

/// Script types that are data, not code; the browser never executes them.
const DATA_TYPES: &[&str] = &["application/json", "application/ld+json", "text/plain"];

/// Hashes (`'sha256-…'`) of all executable inline scripts in an HTML document.
pub fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<script").map(|i| i + pos) {
        let Some(tag_end) = lower[start..].find('>').map(|i| i + start) else {
            break;
        };
        let Some(close) = lower[tag_end..].find("</script").map(|i| i + tag_end) else {
            break;
        };
        let attrs = &lower[start + "<script".len()..tag_end];
        let body = &html[tag_end + 1..close];
        pos = close;
        let has_src = attrs.contains(" src=") || attrs.contains("\tsrc=");
        let script_type = attr_value(attrs, "type").unwrap_or_default();
        if has_src || DATA_TYPES.contains(&script_type.as_str()) {
            continue;
        }
        let hash =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body.as_bytes()));
        out.push(format!("'sha256-{hash}'"));
    }
    out
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let i = attrs.find(&format!("{name}="))?;
    let rest = &attrs[i + name.len() + 1..];
    let (quote, rest) = match rest.chars().next()? {
        q @ ('"' | '\'') => (q, &rest[1..]),
        _ => (' ', rest),
    };
    let end = rest
        .find(|c: char| c == quote || (quote == ' ' && c == '>'))
        .unwrap_or(rest.len());
    Some(rest[..end].trim().to_owned())
}

/// Builds the CSP for the web app from all HTML files below `dir`.
pub fn csp_for_dir(dir: &Path) -> Result<String, String> {
    let mut hashes = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut pages = 0;
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "html") {
                let html =
                    std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                hashes.extend(inline_script_hashes(&html));
                pages += 1;
            }
        }
    }
    if !dir.join("index.html").is_file() {
        return Err(format!(
            "{}: index.html fehlt (Web-App nicht gebaut?)",
            dir.display()
        ));
    }
    hashes.sort();
    hashes.dedup();
    tracing::info!(pages, inline_scripts = hashes.len(), "Web-App gefunden");
    Ok(format!(
        "default-src 'self'; script-src 'self' {}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        hashes.join(" ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_only_executable_inline_scripts() {
        let html = r##"<html><head>
<script type="importmap">{"imports":{"#entry":"/_nuxt/a.js"}}</script>
<script type="module" src="/_nuxt/a.js" crossorigin></script>
<SCRIPT>window.__NUXT__={};</SCRIPT>
<script type="application/json" id="__NUXT_DATA__">[1,2]</script>
</head></html>"##;
        let h = inline_script_hashes(html);
        assert_eq!(h.len(), 2, "{h:?}");
        let expected = base64::engine::general_purpose::STANDARD
            .encode(Sha256::digest(b"window.__NUXT__={};"));
        assert!(h.contains(&format!("'sha256-{expected}'")));
    }

    #[test]
    fn missing_index_is_an_error() {
        let dir = std::env::temp_dir().join(format!("xlrx-web-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(csp_for_dir(&dir).is_err());
        std::fs::write(dir.join("index.html"), "<script>a()</script>").unwrap();
        let csp = csp_for_dir(&dir).unwrap();
        assert!(csp.contains("'sha256-") && csp.contains("frame-ancestors 'none'"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
