// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

mod epub;
mod http_client;
mod models;
mod style;
mod xml;

use std::collections::HashMap;
use std::env::current_dir;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::epub::{create_epub_archive, download_all_files};
use crate::http_client::build_authenticated_client;
use crate::models::{Chapter, EpubResponse, FileEntry, Paginated};
use crate::style::StyleOptions;
use anyhow::{Context, Result, anyhow};
use clap::{Parser, value_parser};
use directories::{BaseDirs, UserDirs};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use colored::Colorize;
use reqwest::Client;

/// Download and generate an EPUB from Safari Books Online.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The Book digits ID that you want to download.
    #[arg(required = true)]
    bookid: String,
    /// Path to the cookies.json file.
    #[arg(long)]
    cookies: Option<String>,
    /// Do not download files. Use if they were already downloaded in a previous run.
    #[arg(long = "skip-download")]
    skip_download: bool,
    /// Number of files to download in parallel. Limit is 8 (be polite).
    #[arg(long, value_parser=value_parser!(u32).range(1..=8), default_value_t = 3)]
    parallel: u32,
    /// Inline stylesheet contents into each chapter's <head> instead of linking external CSS files.
    #[arg(long = "embed-styles")]
    embed_styles: bool,
    /// Keep the book's original colors and fonts. By default color/background declarations
    /// (including dark-mode blocks) and every font-family except for code are stripped.
    #[arg(long = "keep-styles")]
    keep_styles: bool,
    /// Path to OperatorMono.zip. Embeds Operator Mono (Book/Bold) as the font for code.
    #[arg(long = "code-font", value_name = "ZIP")]
    code_font: Option<PathBuf>,
}

/// Fetches EPUB structural data (like the chapters URL).
async fn fetch_epub_data(client: &Client, bookid: &str) -> Result<EpubResponse> {
    let url = format!("https://learning.oreilly.com/api/v2/epubs/urn:orm:book:{bookid}/");
    let response = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<EpubResponse>()
        .await
        .context("Failed to deserialize EPUB API response")?;
    Ok(response)
}

/// Fetch a paginated API.
async fn fetch_all_pages<T>(client: &reqwest::Client, mut url: url::Url) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut items = Vec::new();
    loop {
        // GET current URL and deserialize into Paginated<T>.
        let response = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json::<Paginated<T>>()
            .await
            .context("Failed to deserialize API response.")?;
        // Extend items with the page's results.
        items.extend(response.results);
        // Set url to next page if available, else break.
        if let Some(next) = response.next {
            url = next;
        } else {
            break;
        }
    }
    Ok(items)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Obtain relevant XDG base directories.
    let base_dirs = BaseDirs::new().context("Could not get XDG base directories.")?;
    let data_root = base_dirs.data_dir().join("oreilly-epub");
    let config_root = base_dirs.config_dir().join("oreilly-epub");

    // Parse the command line arguments.
    let args = Args::parse();

    // Paths for the lock file and queue file, stored next to the app data.
    std::fs::create_dir_all(&data_root)?;
    let lock_path = data_root.join("queue.lock");
    let queue_path = data_root.join("queue.txt");

    // Try to become the primary process by creating the lock file exclusively.
    // `create_new` fails if the file already exists, which means another
    // instance is already running.
    let is_primary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .is_ok();

    if !is_primary {
        // Another process holds the lock — check if book is already downloaded before enqueuing.
        if let Some(user_dirs) = UserDirs::new() {
            let epub_path = user_dirs
                .download_dir()
                .unwrap_or(&user_dirs.home_dir().join("Downloads"))
                .join("oreilly-epub")
                .join(format!("{}.epub", args.bookid));
            if epub_path.exists() {
                println!(
                    "Book '{}' is already downloaded: {}.",
                    args.bookid,
                    epub_path.display()
                );
                return Ok(());
            }
        }
        // Check if book is already in the queue.
        if queue_path.exists() {
            let existing = std::fs::read_to_string(&queue_path)
                .context("Failed to read queue.txt")?;
            if existing.lines().any(|l| l.trim() == args.bookid.as_str()) {
                println!(
                    "Book '{}' is already in the queue. Skipping.",
                    args.bookid
                );
                return Ok(());
            }
        }
        // Enqueue this ISBN and exit.
        let mut queue_file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&queue_path)
            .context("Failed to open queue.txt for appending")?;
        writeln!(queue_file, "{}", args.bookid).context("Failed to write to queue.txt")?;
        // Read the queue file to compute statistics (total queued items).
        let contents = std::fs::read_to_string(&queue_path)
            .context("Failed to read queue.txt after appending")?;
        let total_queued = contents.lines().filter(|l| !l.trim().is_empty()).count();
        println!(
            "Book '{}' has been added to the queue. Queue length: {}",
            args.bookid,
            total_queued
        );
        return Ok(());
    }

    // RAII guard: removes the lock file when dropped, even on error or panic.
    struct LockGuard(PathBuf);
    impl Drop for LockGuard {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
        }
    }
    let _lock_guard = LockGuard(lock_path);

    // We are the primary process. Resolve the HTTP client once and reuse it.
    println!("Loading cookies and initialising the HTTP client...");
    let candidate_cookies_paths = [
        args.cookies.clone().map(PathBuf::from),
        Some(config_root.join("cookies.json")),
        Some(current_dir()?.join("cookies.json")),
    ];
    let cookies_file = candidate_cookies_paths
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .ok_or_else(|| {
            anyhow!(
                "No cookies.json found. {}, {} {:?}.",
                "Either provide one through the --cookies option",
                "or create one in the current directory or at",
                config_root
            )
        })?;
    let client = build_authenticated_client(&cookies_file)?;

    let style = StyleOptions::new(!args.keep_styles, args.code_font.as_deref())?;

    // Process the requested book first, then drain the queue.
    let mut current_id = args.bookid.clone();
    loop {
        println!("\nWelcome to SafariBooks Rust Port!");
        println!("Target Book ID: {current_id}");
        process_book(
            &client,
            &current_id,
            &data_root,
            args.skip_download,
            args.parallel.try_into()?,
            args.embed_styles,
            &style,
        )
        .await?;

        // After finishing, check if there is anything queued.
        match dequeue_next(&queue_path)? {
            Some(next_id) => {
                // Compute remaining queued items after consuming one.
                let total_remaining = if queue_path.exists() {
                    std::fs::read_to_string(&queue_path)
                        .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
                        .unwrap_or(0)
                } else {
                    0
                };
                println!("\nPicked up next book from queue: {next_id} (remaining queued: {total_remaining})");
                current_id = next_id;
            }
            None => break,
        }
    }

    // _lock_guard is dropped here (or earlier on any error), removing the lock.
    Ok(())
}

/// Removes and returns the first ISBN from `queue.txt`, or `None` if the file
/// is absent or empty. The file is rewritten without the consumed entry.
fn dequeue_next(queue_path: &Path) -> Result<Option<String>> {
    if !queue_path.exists() {
        return Ok(None);
    }
    let mut contents = String::new();
    std::fs::File::open(queue_path)?.read_to_string(&mut contents)?;

    let mut lines: Vec<&str> = contents.lines().collect();
    if lines.is_empty() {
        return Ok(None);
    }
    let next = lines.remove(0).to_owned();
    // Rewrite the file without the consumed entry.
    let remaining = lines.join("\n");
    if remaining.is_empty() {
        std::fs::remove_file(queue_path).ok();
    } else {
        std::fs::write(queue_path, format!("{remaining}\n"))?;
    }
    Ok(Some(next))
}

/// Downloads and packages a single book identified by `bookid`.
async fn process_book(
    client: &Client,
    bookid: &str,
    data_root: &Path,
    skip_download: bool,
    parallel: usize,
    embed_styles: bool,
    style: &StyleOptions,
) -> Result<()> {
    // Obtain the path to the destination EPUB file.
    let user_dirs = UserDirs::new().context("Could not get XDG user directories.")?;
    let epub_path = user_dirs
        .download_dir()
        .unwrap_or(&user_dirs.home_dir().join("Downloads"))
        .join("oreilly-epub")
        .join(format!("{bookid}.epub"));

    if epub_path.exists() {
        println!(
            "Book '{bookid}' is already downloaded: {}. Skipping.",
            epub_path.display()
        );
        return Ok(());
    }

    println!("Fetching book metadata...");
    let epub_data = fetch_epub_data(client, bookid).await?;
    println!("Publication date: {}", epub_data.publication_date);
    println!("Title: {}", epub_data.title.green());
    println!("Chapters URL: {}", epub_data.chapters);
    println!("Resources URL: {}", epub_data.files);
    println!("------------------\n");

    println!("Fetching book structure...");
    let chapters: Vec<Chapter> = fetch_all_pages(client, epub_data.chapters.clone()).await?;
    let chapters: HashMap<String, Chapter> =
        chapters.into_iter().map(|c| (c.ourn.clone(), c)).collect();
    let file_entries: Vec<FileEntry> = fetch_all_pages(client, epub_data.files.clone()).await?;

    let epub_root = data_root.join("files").join(bookid);

    // Categorize file_entries and set up progress bars before download.
    let mut pages = Vec::new();
    let mut styles = Vec::new();
    let mut images = Vec::new();
    for entry in &file_entries {
        match entry.media_type.as_str() {
            "application/xhtml+xml" | "text/html" => pages.push(entry),
            "text/css" => styles.push(entry),
            mt if mt.starts_with("image/") => images.push(entry),
            _ => {}
        }
    }
    let label_width = "Chapters".len();
    let max_count = pages.len().max(styles.len()).max(images.len());
    let count_width = format!("{max_count}/{max_count}").len();
    let fmt_count = |pos: usize, total: usize| -> String {
        format!("{:>count_width$}", format!("{pos}/{total}"))
    };
    let template = format!("{{msg:<{label_width}}} [{{wide_bar}}] {{prefix}}");

    let m = MultiProgress::new();
    let pb_pages = m.add(ProgressBar::new(pages.len() as u64));
    pb_pages.set_style(
        ProgressStyle::with_template(&template)
            .unwrap()
            .progress_chars("#! "),
    );
    pb_pages.set_message("Chapters");
    pb_pages.set_prefix(fmt_count(0, pages.len()));
    let pb_styles = m.add(ProgressBar::new(styles.len() as u64));
    pb_styles.set_style(
        ProgressStyle::with_template(&template)
            .unwrap()
            .progress_chars("#! "),
    );
    pb_styles.set_message("Styles");
    pb_styles.set_prefix(fmt_count(0, styles.len()));
    let pb_images = m.add(ProgressBar::new(images.len() as u64));
    pb_images.set_style(
        ProgressStyle::with_template(&template)
            .unwrap()
            .progress_chars("#! "),
    );
    pb_images.set_message("Images");
    pb_images.set_prefix(fmt_count(0, images.len()));

    if !skip_download {
        download_all_files(
            client,
            &file_entries,
            &epub_root,
            parallel,
            Some((pb_pages.clone(), pb_styles.clone(), pb_images.clone())),
        )
        .await?;
    }
    println!("Generating the EPUB file...");
    create_epub_archive(
        &epub_data,
        &epub_root,
        &epub_path,
        &file_entries,
        &chapters,
        embed_styles,
        style,
    )?;
    println!("Saved to: {}", epub_path.display());
    Ok(())
}
