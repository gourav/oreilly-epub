// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

use anyhow::{Context, Result};
use ogrim::{Document, xml};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use relative_path::{RelativePath, RelativePathBuf};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use url::Url;

use crate::models::{Chapter, EpubResponse, FileEntry};

/// Checks if a tag is a standard HTML void element that shouldn't have a closing tag.
fn is_html_void_tag(name: &[u8]) -> bool {
    matches!(
        name,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

/// Processes the fragment and outputs a complete, EPUB-ready XHTML document.
pub fn build_epub_chapter<R: BufRead, W: Write>(
    epub_data: &EpubResponse,
    chapter: &Chapter,
    chapter_dir: &RelativePath,
    fragment_input: R,
    url_to_file: &HashMap<&Url, &FileEntry>,
    url_path_to_local: &HashMap<&str, &RelativePathBuf>,
    mut out: &mut W,
) -> Result<()> {
    // EPUB XHTML Boilerplate wrapper.
    // EPUBs strictly require the w3 and idpf namespaces to validate properly.
    let wrapper_xhtml = xml!(
        <?xml version="1.0" encoding="UTF-8"?>
        <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"
            lang={epub_data.language} xml:lang={epub_data.language}>
        <head>
            <title>{chapter.title}</title>
            {|doc| make_stylesheet_links(doc, chapter, chapter_dir, url_to_file)}
        </head>
        <body>
        </body>
        </html>
    );
    let wrapper_suffix = "</body></html>";
    let wrapper_prefix = wrapper_xhtml
        .as_str()
        .strip_suffix(wrapper_suffix)
        .context("Wrapper must end with </body></html>")?;

    // Write wrapper prefix to output first.
    out.write_all(wrapper_prefix.as_bytes())?;
    out.write_all(b"\n")?;

    // Setup the XML Reader and Writer.
    let mut reader = Reader::from_reader(fragment_input);
    // Preserve spacing for EPUB text formatting.
    reader.config_mut().trim_text(false);
    // Fragments could have unmatched tags - tell the parser not to panic if so.
    reader.config_mut().check_end_names = false;
    let mut writer = Writer::new(&mut out);

    // Loop through the XML events and rewrite tags.
    // Source files may be full XHTML documents (with their own XML declaration,
    // DOCTYPE, <html>, <head>, <body> elements).  We must skip all of that
    // structural boilerplate so that only the body content is emitted into the
    // wrapper we already wrote above.  If the source is already a bare
    // fragment these checks are simply never triggered.
    let mut in_head = false;
    let mut in_script = false;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            // Skip XML declarations and DOCTYPE – already provided by the wrapper.
            Ok(Event::Decl(_)) | Ok(Event::DocType(_)) => {}
            Ok(Event::Start(tag_data)) => {
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                // Skip the outer structural tags of a full HTML/XHTML document.
                if matches!(name, b"html" | b"body") {
                    continue;
                }
                // Track the source document's <head> so its contents are suppressed.
                if name == b"head" {
                    in_head = true;
                    continue;
                }
                if in_head {
                    continue;
                }
                // Strip injected <script> elements from the body.
                if name == b"script" {
                    in_script = true;
                    continue;
                }
                if in_script {
                    continue;
                }
                // Normal processing: convert void tags to self-closing form.
                let tag_type = if is_html_void_tag(name) {
                    Event::Empty
                } else {
                    Event::Start
                };
                writer.write_event(tag_type(rewrite_attributes(
                    &tag_data,
                    url_path_to_local,
                    chapter_dir,
                )))?;
            }
            Ok(Event::Empty(tag_data)) => {
                if in_head || in_script {
                    continue;
                }
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                if name == b"script" {
                    continue;
                }
                // If tags are already empty, leave them as-is.
                writer.write_event(Event::Empty(rewrite_attributes(
                    &tag_data,
                    url_path_to_local,
                    chapter_dir,
                )))?;
            }
            Ok(Event::End(tag_data)) => {
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                // Skip closing structural tags.
                if matches!(name, b"html" | b"body") {
                    continue;
                }
                if name == b"head" {
                    in_head = false;
                    continue;
                }
                if in_head {
                    continue;
                }
                if name == b"script" {
                    in_script = false;
                    continue;
                }
                if in_script {
                    continue;
                }
                // Silently drop closing tags for void elements if they exist (e.g. <img></img>).
                if !is_html_void_tag(name) {
                    writer.write_event(Event::End(tag_data))?;
                }
            }
            Ok(Event::Eof) => break,
            Ok(tag_data) => {
                if !in_head && !in_script {
                    writer.write_event(tag_data)?; // Pass through text, comments, etc. unmodified.
                }
            }
            Err(e) => anyhow::bail!(e),
        }
        buffer.clear();
    }

    // Finish by flushing wrapper suffix to output.
    out.write_all(b"\n")?;
    out.write_all(wrapper_suffix.as_bytes())?;

    Ok(())
}

/// Processes the fragment and outputs a complete, EPUB-ready XHTML document.
pub fn write_modified_opf<R: BufRead, W: Write>(
    opf_input: R,
    mut out: &mut W,
    description: &str,
) -> Result<()> {
    // Setup the XML Reader and Writer.
    let mut reader = Reader::from_reader(opf_input);
    // Preserve spacing for easier diffs.
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(&mut out);

    // Loop through the XML events and check tags.
    let mut buffer = Vec::new();
    let mut desc_found = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag_data)) => {
                // Simply record if dc:description found.
                if tag_data.name().as_ref() == b"dc:description" {
                    desc_found = true;
                }
                // Then pass through unmodified.
                writer.write_event(Event::Start(tag_data))?;
            }
            Ok(Event::End(tag_data)) => {
                // Write description if end of metadata is reached without finding one.
                if tag_data.name().as_ref() == b"metadata" && !desc_found {
                    writer.write_event(Event::Start(BytesStart::new("dc:description")))?;
                    writer.write_event(Event::Text(BytesText::new(description)))?;
                    writer.write_event(Event::End(BytesEnd::new("dc:description")))?;
                }
                // Pass through unmodified.
                writer.write_event(Event::End(tag_data))?;
            }
            Ok(Event::Eof) => break,
            Ok(tag_data) => writer.write_event(tag_data)?, // Pass through text, comments, etc. unmodified.
            Err(e) => anyhow::bail!(e),
        }
    }

    Ok(())
}

/// Helper function add link elements for stylesheets to an xml Document.
fn make_stylesheet_links(
    doc: &mut Document,
    chapter: &Chapter,
    chapter_dir: &RelativePath,
    url_to_file: &HashMap<&Url, &FileEntry>,
) {
    chapter
        .related_assets
        .stylesheets
        .iter()
        .filter_map(|u| url_to_file.get(u))
        .for_each(|e| {
            let rel_path = chapter_dir.relative(&e.full_path);
            xml!(doc,
                <link rel="stylesheet" type={e.media_type} href={rel_path} />
            );
        })
}

/// Helper function to inspect tags and rewrite the elements' attributes.
fn rewrite_attributes<'a>(
    tag_data: &BytesStart<'a>,
    url_path_to_local: &HashMap<&str, &RelativePathBuf>,
    chapter_dir: &RelativePath,
) -> BytesStart<'static> {
    let name = String::from_utf8_lossy(tag_data.name().as_ref()).into_owned();
    let mut new_elem = BytesStart::new(name);

    for attr in tag_data.attributes().filter_map(Result::ok) {
        let key = attr.key.as_ref();

        // Intercept element attributes which could contain links to resources.
        if matches!(
            (tag_data.name().as_ref(), key),
            (b"img", b"src")
                | (b"source", b"src")
                | (b"video", b"src")
                | (b"audio", b"src")
                | (b"a", b"href")
                | (b"link", b"href")
                | (b"object", b"data")
                | (b"embed", b"src")
                | (b"iframe", b"src")
                | (b"track", b"src")
        ) {
            let url = String::from_utf8_lossy(&attr.value);

            // If we have a local path, inject it instead of the absolute URL.
            if let Some(local_path) = url_path_to_local.get(url.as_ref()) {
                let key = String::from_utf8_lossy(key);
                new_elem.push_attribute((key.as_ref(), chapter_dir.relative(local_path).as_str()));
                continue;
            }
        }

        // Keep all other attributes intact, normalizing HTML5 boolean attributes
        // (e.g. `async` → `async="async"`) to produce valid XML.
        if attr.value.as_ref().is_empty() {
            let key_str = String::from_utf8_lossy(key).into_owned();
            new_elem.push_attribute((key_str.as_str(), key_str.as_str()));
        } else {
            new_elem.push_attribute(attr);
        }
    }

    new_elem
}

/// Sanitizes a non-chapter XHTML file for EPUB inclusion.
/// Strips injected `<script>` elements and normalizes HTML5 boolean attributes
/// to valid XML form, while preserving the full document structure.
pub fn sanitize_xhtml_file<R: BufRead, W: Write>(input: R, mut out: &mut W) -> Result<()> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut writer = Writer::new(&mut out);

    let mut in_script = false;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag_data)) => {
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                if name == b"script" {
                    in_script = true;
                    continue;
                }
                if in_script {
                    continue;
                }
                // Convert void elements to self-closing form for XML validity.
                let tag_type = if is_html_void_tag(name) {
                    Event::Empty
                } else {
                    Event::Start
                };
                writer.write_event(tag_type(rewrite_attributes(
                    &tag_data,
                    &HashMap::new(),
                    RelativePath::new(""),
                )))?;
            }
            Ok(Event::Empty(tag_data)) => {
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                if name == b"script" || in_script {
                    continue;
                }
                writer.write_event(Event::Empty(rewrite_attributes(
                    &tag_data,
                    &HashMap::new(),
                    RelativePath::new(""),
                )))?;
            }
            Ok(Event::End(tag_data)) => {
                let name_buf = tag_data.name().as_ref().to_owned();
                let name = name_buf.as_slice();
                if name == b"script" {
                    in_script = false;
                    continue;
                }
                if in_script {
                    continue;
                }
                if !is_html_void_tag(name) {
                    writer.write_event(Event::End(tag_data))?;
                }
            }
            Ok(Event::Eof) => break,
            Ok(event) => {
                if !in_script {
                    writer.write_event(event)?;
                }
            }
            Err(e) => anyhow::bail!(e),
        }
        buffer.clear();
    }
    Ok(())
}
