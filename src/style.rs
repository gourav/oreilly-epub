// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

//! Optional stylesheet rewriting for e-ink readers: strips colors and font families, and embeds
//! Operator Mono as the code font.

use anyhow::{Context, Result, bail};
use relative_path::RelativePath;
use std::{io::Read, path::Path};

/// Family name used in the generated `@font-face` rules.
const FAMILY: &str = "Operator Mono";
/// Directory (inside the EPUB) the font files are written to.
const FONT_DIR: &str = "Misc";
/// Faces taken from `OperatorMono.zip`: (file suffix, weight, italic).
const FACES: [(&str, u16, bool); 4] = [
    ("Book", 400, false),
    ("BookItalic", 400, true),
    ("Bold", 700, false),
    ("BoldItalic", 700, true),
];

/// A single font file to embed in the EPUB.
pub struct FontFace {
    /// Path of the file inside the EPUB, e.g. `Misc/OperatorMono-Book.otf`.
    pub path: String,
    pub data: Vec<u8>,
    weight: u16,
    italic: bool,
}

/// What to do to the book's stylesheets.
#[derive(Default)]
pub struct StyleOptions {
    /// Remove color/background declarations, dark-mode blocks and non-code `font-family`.
    simplify: bool,
    /// Embedded code font. Applied to `pre`/`code`/`kbd`/`samp` rules.
    code_font: Option<Vec<FontFace>>,
}

impl StyleOptions {
    /// Builds the options, loading the font faces from `code_font_zip` when given.
    pub fn new(simplify: bool, code_font_zip: Option<&Path>) -> Result<Self> {
        let code_font = code_font_zip.map(load_operator_mono).transpose()?;
        Ok(Self {
            simplify,
            code_font,
        })
    }

    /// Whether any stylesheet rewriting is requested.
    pub fn is_active(&self) -> bool {
        self.simplify || self.code_font.is_some()
    }

    /// The font files that must be added to the archive and manifest.
    pub fn font_faces(&self) -> &[FontFace] {
        self.code_font.as_deref().unwrap_or(&[])
    }

    /// Rewrites `css`. `css_dir` is the directory the CSS is served from (the stylesheet's own
    /// directory, or the chapter's when inlined); it is used to build relative font URLs.
    pub fn transform_css(&self, css: &str, css_dir: &RelativePath) -> String {
        let faces: String = self
            .font_faces()
            .iter()
            .map(|f| {
                format!(
                    "@font-face{{font-family:\"{FAMILY}\";font-style:{};font-weight:{};\
                     src:url({}) format(\"opentype\")}}",
                    if f.italic { "italic" } else { "normal" },
                    f.weight,
                    css_dir.relative(RelativePath::new(&f.path)),
                )
            })
            .collect();
        faces + &self.process_items(&strip_comments(css))
    }

    /// Processes a sequence of top-level rules / at-rules.
    fn process_items(&self, css: &str) -> String {
        let mut out = String::new();
        for item in parse_items(css) {
            match item {
                Item::Stmt(stmt) => out.push_str(stmt),
                Item::Block(prelude, body) => {
                    let lower = prelude.to_ascii_lowercase();
                    if lower.starts_with("@media") || lower.starts_with("@supports") {
                        if self.simplify && lower.contains("prefers-color-scheme") {
                            continue;
                        }
                        let inner = self.process_items(body);
                        if !inner.is_empty() {
                            out.push_str(&format!("{prelude}{{{inner}}}"));
                        }
                    } else if lower.starts_with("@font-face") {
                        // Existing book fonts are only referenced by the families removed below.
                        if !(self.simplify && self.code_font.is_some()) {
                            out.push_str(&format!("{prelude}{{{body}}}"));
                        }
                    } else if lower.starts_with('@') {
                        out.push_str(&format!("{prelude}{{{body}}}"));
                    } else {
                        let decls = self.filter_declarations(prelude, body);
                        if !decls.is_empty() {
                            out.push_str(&format!("{prelude}{{{decls}}}"));
                        }
                    }
                }
            }
        }
        out
    }

    /// Filters the declarations of one style rule.
    fn filter_declarations(&self, selector: &str, body: &str) -> String {
        let is_code = is_code_selector(selector);
        let mut kept = Vec::new();
        let mut pos = 0;
        while pos < body.len() {
            let end = find_unquoted(body, pos, b";").unwrap_or(body.len());
            let decl = body[pos..end].trim();
            pos = end + 1;
            let Some((prop, value)) = decl.split_once(':') else {
                continue;
            };
            let (prop, value) = (prop.trim(), value.trim());
            let lower = prop.to_ascii_lowercase();
            if self.simplify
                && (lower == "color"
                    || lower.starts_with("background")
                    || lower.ends_with("-color"))
            {
                continue;
            }
            if lower == "font-family" {
                if is_code {
                    if self.code_font.is_some() {
                        kept.push(format!("{prop}:\"{FAMILY}\",{value}"));
                        continue;
                    }
                } else if self.simplify {
                    continue;
                }
            }
            kept.push(format!("{prop}:{value}"));
        }
        kept.join(";")
    }
}

/// Reads the Book/Bold (+ italic) `.otf` files out of `OperatorMono.zip`.
fn load_operator_mono(zip_path: &Path) -> Result<Vec<FontFace>> {
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("Failed to open code font archive {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("Code font archive is not a valid zip")?;
    let mut faces = Vec::new();
    for (suffix, weight, italic) in FACES {
        let wanted = format!("OperatorMono-{suffix}.otf");
        let idx = (0..archive.len())
            .find(|&i| {
                archive
                    .name_for_index(i)
                    .is_some_and(|n| n.rsplit('/').next() == Some(wanted.as_str()))
            })
            .with_context(|| format!("{wanted} not found in {}", zip_path.display()))?;
        let mut data = Vec::new();
        archive.by_index(idx)?.read_to_end(&mut data)?;
        if data.is_empty() {
            bail!("{wanted} in {} is empty", zip_path.display());
        }
        faces.push(FontFace {
            path: format!("{FONT_DIR}/{wanted}"),
            data,
            weight,
            italic,
        });
    }
    Ok(faces)
}

/// True if any compound selector in the list targets a code element.
fn is_code_selector(selector: &str) -> bool {
    selector
        .split(|c: char| c == ',' || c.is_whitespace() || matches!(c, '>' | '+' | '~'))
        .any(|compound| {
            let element = compound
                .split(['.', '#', '[', ':'])
                .next()
                .unwrap_or_default();
            matches!(element, "pre" | "code" | "kbd" | "samp")
        })
}

enum Item<'a> {
    /// An at-rule without a block, e.g. `@import ...;`.
    Stmt(&'a str),
    /// A prelude (selector or at-rule header) and the text between its braces.
    Block(&'a str, &'a str),
}

fn parse_items(css: &str) -> Vec<Item<'_>> {
    let mut items = Vec::new();
    let mut pos = 0;
    while pos < css.len() {
        let Some(i) = find_unquoted(css, pos, b"{;") else {
            let rest = css[pos..].trim();
            if !rest.is_empty() {
                items.push(Item::Stmt(rest));
            }
            break;
        };
        if css.as_bytes()[i] == b';' {
            items.push(Item::Stmt(css[pos..=i].trim()));
            pos = i + 1;
        } else {
            let Some(close) = matching_brace(css, i) else {
                break;
            };
            items.push(Item::Block(css[pos..i].trim(), &css[i + 1..close]));
            pos = close + 1;
        }
    }
    items
}

/// Finds the first byte in `targets` at or after `from`, ignoring quoted strings and `(...)`.
fn find_unquoted(s: &str, from: usize, targets: &[u8]) -> Option<usize> {
    let bytes = s.as_bytes();
    let (mut quote, mut parens, mut i) = (0u8, 0usize, from);
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == b'\\' {
                i += 1;
            } else if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'(' {
            parens += 1;
        } else if c == b')' {
            parens = parens.saturating_sub(1);
        } else if parens == 0 && targets.contains(&c) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the `}` matching the `{` at `open`.
fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let (mut quote, mut depth, mut i) = (0u8, 1usize, open + 1);
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == b'\\' {
                i += 1;
            } else if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(simplify: bool, with_font: bool) -> StyleOptions {
        StyleOptions {
            simplify,
            code_font: with_font.then(|| {
                vec![FontFace {
                    path: "Misc/OperatorMono-Book.otf".into(),
                    data: vec![0],
                    weight: 400,
                    italic: false,
                }]
            }),
        }
    }

    #[test]
    fn strips_color_background_and_dark_mode() {
        let css = "p{color:#000;margin:0;background:red}\
                   @media (prefers-color-scheme:dark){p{color:#fff}}";
        let out = opts(true, false).transform_css(css, RelativePath::new("Styles"));
        assert_eq!(out, "p{margin:0}");
    }

    #[test]
    fn keeps_font_family_only_for_code() {
        let css = "div{font-family:Verdana;margin:0}pre.code-area{font-family:Mono,monospace}";
        let out = opts(true, true).transform_css(css, RelativePath::new("Styles"));
        assert!(out.contains("div{margin:0}"));
        assert!(out.contains("pre.code-area{font-family:\"Operator Mono\",Mono,monospace}"));
        assert!(out.starts_with("@font-face{font-family:\"Operator Mono\""));
        assert!(out.contains("url(../Misc/OperatorMono-Book.otf)"));
    }

    #[test]
    fn drops_old_font_faces_when_replacing_code_font() {
        let css = "@font-face{font-family:\"JetBrains Mono\";src:url(a.woff2)}p{margin:0}";
        let out = opts(true, true).transform_css(css, RelativePath::new("Styles"));
        assert!(!out.contains("JetBrains"));
    }

    #[test]
    fn inactive_options_leave_rules_intact() {
        let css = "p{color:red;font-family:Georgia}";
        let out = opts(false, false).transform_css(css, RelativePath::new(""));
        assert_eq!(out, css);
    }

    #[test]
    fn selector_lists_detect_code_elements() {
        assert!(is_code_selector("#x .code-area-container pre"));
        assert!(is_code_selector("a, code"));
        assert!(!is_code_selector(".code-area"));
    }
}
