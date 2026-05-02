// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

use crate::{
    models::{Chapter, EpubResponse, FileEntry},
    xml::{build_epub_chapter, sanitize_xhtml_file, write_modified_opf},
};
use anyhow::{Context, Result};
use futures_util::{FutureExt, StreamExt, TryStreamExt, stream::FuturesUnordered};
use ogrim::xml;
use relative_path::{RelativePath, RelativePathBuf};
use reqwest::Client;
use std::{
    collections::HashMap,
    future::Future,
    io::{BufReader, Write},
    path::Path,
    pin::Pin,
};
use tokio::fs::{self, File};
use tokio_util::io::StreamReader;
use zip::{CompressionMethod, ZipWriter, write::FileOptions};

/// Creates and writes container.xml.
fn write_container_xml<W: Write>(out: &mut W, opf_full_path: &RelativePathBuf) -> Result<()> {
    // Prepare file contents.
    let contents = xml!(
        <?xml version="1.0" encoding="UTF-8"?>
        <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
          <rootfiles>
            <rootfile full-path={opf_full_path} media-type="application/oebps-package+xml"/>
          </rootfiles>
        </container>
    );

    // Write down the file.
    out.write_all(contents.as_str().as_bytes())?;
    Ok(())
}

/// Downloads files in parallel to the relative location specified in full_path.
pub async fn download_all_files(
    client: &Client,
    file_entries: &[FileEntry],
    dest_root: &Path,
    max_concurrent: usize,
    progress: Option<(
        indicatif::ProgressBar,
        indicatif::ProgressBar,
        indicatif::ProgressBar,
    )>,
) -> Result<()> {
    use futures_util::StreamExt;
    let mut downloading: FuturesUnordered<
        Pin<Box<dyn Future<Output = (Result<()>, usize)> + Send>>,
    > = FuturesUnordered::new();
    let mut idx = 0;
    let total = file_entries.len();

    // Helper to categorize a file.
    fn category(entry: &FileEntry) -> &'static str {
        match entry.media_type.as_str() {
            "application/xhtml+xml" | "text/html" => "page",
            "text/css" => "style",
            mt if mt.starts_with("image/") => "image",
            _ => "other",
        }
    }

    // Start downloading the first n files.
    while idx < max_concurrent && idx < total {
        let fut: Pin<Box<dyn Future<Output = (Result<()>, usize)> + Send>> = Box::pin(async move {
            (
                download_one_file(client, &file_entries[idx], dest_root).await,
                idx,
            )
        });
        downloading.push(fut);
        idx += 1;
    }

    // Obtain completed files from the list as they finish downloading, until empty.
    while let Some((result, file_idx)) = downloading.next().await {
        let entry = &file_entries[file_idx];
        result?;
        if let Some((ref pb_pages, ref pb_styles, ref pb_images)) = progress {
            match category(entry) {
                "page" => {
                    pb_pages.inc(1);
                    pb_pages.set_prefix(format!(
                        "{:>w$}/{:>w$}",
                        pb_pages.position(),
                        pb_pages.length().unwrap_or(0),
                        w = pb_pages.length().unwrap_or(0).to_string().len()
                    ));
                }
                "style" => {
                    pb_styles.inc(1);
                    pb_styles.set_prefix(format!(
                        "{:>w$}/{:>w$}",
                        pb_styles.position(),
                        pb_styles.length().unwrap_or(0),
                        w = pb_styles.length().unwrap_or(0).to_string().len()
                    ));
                }
                "image" => {
                    pb_images.inc(1);
                    pb_images.set_prefix(format!(
                        "{:>w$}/{:>w$}",
                        pb_images.position(),
                        pb_images.length().unwrap_or(0),
                        w = pb_images.length().unwrap_or(0).to_string().len()
                    ));
                }
                _ => {}
            }
        }
        // Refill the slot (if there are any remaining).
        if idx < total {
            let fut: Pin<Box<dyn Future<Output = (Result<()>, usize)> + Send>> =
                Box::pin(async move {
                    (
                        download_one_file(client, &file_entries[idx], dest_root).await,
                        idx,
                    )
                });
            downloading.push(fut);
            idx += 1;
        }
    }

    if let Some((pb_pages, pb_styles, pb_images)) = progress {
        pb_pages.finish();
        pb_styles.finish();
        pb_images.finish();
    }
    Ok(())
}

/// Downloads the given file to the relative location specified in full_path.
pub async fn download_one_file(
    client: &Client,
    file_entry: &FileEntry,
    dest_root: &Path,
) -> Result<()> {
    let dest_path = file_entry.full_path.to_path(dest_root);

    // Ensure the directory exists and open the file.
    if let Some(parent_dir) = dest_path.parent() {
        fs::create_dir_all(parent_dir).await?;
    }
    let mut file = File::create(dest_path).await?;

    // Obtain the resource as a stream of bytes.
    let bytes_stream = client
        .get(file_entry.url.clone())
        .send()
        .await?
        .error_for_status()?
        .bytes_stream();
    // Convert the bytes stream are fed to a reader. Must map errors to io errors.
    let mut reader = StreamReader::new(bytes_stream.map_err(std::io::Error::other));

    // Pipe bytes from the stream to the file.
    tokio::io::copy(&mut reader, &mut file).await?;
    Ok(())
}

/// Creates the EPUB archive (creates zip and includes all files in it).
pub fn create_epub_archive(
    epub_data: &EpubResponse,
    epub_root: &Path,
    output_epub: &Path,
    file_entries: &[FileEntry],
    chapters: &HashMap<String, Chapter>,
    embed_styles: bool,
) -> Result<()> {
    if let Some(parent_dir) = output_epub.parent() {
        std::fs::create_dir_all(parent_dir)?;
    }
    let out_file = std::fs::File::create(output_epub)?;
    let mut zip = ZipWriter::new(out_file);

    // Write mimetype to zip first. It must be uncompressed.
    let options: FileOptions<()> =
        FileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", options)?;
    zip.write_all(b"application/epub+zip")?;

    // Find the OPF file entry to reference it in "container.xml".
    let opf_entry = file_entries
        .iter()
        .find(|f| f.filename_ext == ".opf" && f.media_type == "application/oebps-package+xml")
        .context("No OPF file with the correct MIME type was found.")?;
    // Write down the "container.xml" to zip.
    let options: FileOptions<()> =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("META-INF/container.xml", options)?;
    write_container_xml(&mut zip, &opf_entry.full_path)?;

    // Prepare url path to local path mapping to clean xhtml files from external dependencies.
    let url_path_to_local = file_entries
        .iter()
        .map(|e| (e.url.path(), &e.full_path))
        .collect::<HashMap<_, _>>();
    // Prepare url to local path mapping to insert related assets based on their URL.
    let url_to_file = file_entries
        .iter()
        .map(|e| (&e.url, e))
        .collect::<HashMap<_, _>>();

    // Add the rest of the files according to file_entries.
    // The `options` variable remains unchanged from "container.xml".
    for entry in file_entries {
        zip.start_file(&entry.full_path, options)?;
        let src_file = std::fs::File::open(entry.full_path.to_path(epub_root))?;
        let mut buf_reader = BufReader::new(src_file);
        if let Some(chapter) = chapters.get(&entry.ourn) {
            let chapter_dir = entry.full_path.parent().unwrap_or(RelativePath::new(""));
            build_epub_chapter(
                epub_data,
                chapter,
                chapter_dir,
                buf_reader,
                &url_to_file,
                &url_path_to_local,
                epub_root,
                embed_styles,
                &mut zip,
            )?;
        } else if entry.ourn == opf_entry.ourn {
            write_modified_opf(buf_reader, &mut zip, &epub_data.descriptions.plain)?;
        } else if matches!(
            entry.media_type.as_str(),
            "application/xhtml+xml" | "text/html"
        ) {
            // Run XHTML files through the sanitizer to strip injected <script>
            // elements and normalize HTML5 boolean attributes for XML validity.
            sanitize_xhtml_file(buf_reader, &mut zip)?;
        } else {
            std::io::copy(&mut buf_reader, &mut zip)?;
        }
    }

    zip.finish()?;

    Ok(())
}
