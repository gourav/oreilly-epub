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
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, value_parser};
use directories::{BaseDirs, UserDirs};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use colored::Colorize;
use reqwest::Client;

/// Download and generate an EPUB from Safari Books Online.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The Book digits ID that you want to download.
    #[arg(required = true)]
    bookid: Option<String>,
    /// Path to the cookies.json file.
    #[arg(long, global = true)]
    cookies: Option<String>,
    /// Do not download files. Use if they were already downloaded in a previous run.
    #[arg(long = "skip-download", global = true)]
    skip_download: bool,
    /// Number of files to download in parallel. Limit is 8 (be polite).
    #[arg(long, global = true, value_parser=value_parser!(u32).range(1..=8), default_value_t = 3)]
    parallel: u32,
    /// Inline stylesheet contents into each chapter's <head> instead of linking external CSS files.
    #[arg(long = "embed-styles", global = true)]
    embed_styles: bool,
    /// Keep the book's original colors and fonts. By default color/background declarations
    /// (including dark-mode blocks) and every font-family except for code are stripped.
    #[arg(long = "keep-styles", global = true)]
    keep_styles: bool,
    /// Path to OperatorMono.zip. Embeds Operator Mono (Book/Bold) as the font for code.
    #[arg(long = "code-font", value_name = "ZIP", global = true)]
    code_font: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Add one or more books to the download queue.
    Add {
        /// Book digits IDs (ISBNs) to queue.
        #[arg(required = true, value_name = "ISBN")]
        isbns: Vec<String>,
    },
    /// Download a book (same as running `oreilly <ISBN>`). Queued if another download is running.
    Download {
        /// Book digits ID (ISBN) to download.
        #[arg(value_name = "ISBN")]
        isbn: String,
    },
    /// Delete a book's EPUB if it was already downloaded, then download it again (or queue it
    /// if another download is running).
    Redownload {
        /// Book digits ID (ISBN) to download again.
        #[arg(value_name = "ISBN")]
        isbn: String,
    },
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

    // `add` only touches the queue file; it needs neither the lock nor the cookies.
    if let Some(Command::Add { isbns }) = &args.command {
        std::fs::create_dir_all(&data_root)?;
        return add_to_queue(isbns, &data_root);
    }
    let bookid = match &args.command {
        Some(Command::Redownload { isbn }) => {
            delete_downloaded_epub(isbn)?;
            isbn.clone()
        }
        Some(Command::Download { isbn }) => isbn.clone(),
        _ => args.bookid.clone().context("A BOOKID is required.")?,
    };

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
                .join(format!("{}.epub", bookid));
            if epub_path.exists() {
                println!(
                    "Book '{}' is already downloaded: {}.",
                    bookid,
                    epub_path.display()
                );
                return Ok(());
            }
        }
        // Check if book is already in the queue.
        if queue_path.exists() {
            let existing = std::fs::read_to_string(&queue_path)
                .context("Failed to read queue.txt")?;
            if existing.lines().any(|l| l.trim() == bookid.as_str()) {
                println!(
                    "Book '{}' is already in the queue. Skipping.",
                    bookid
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
        writeln!(queue_file, "{}", bookid).context("Failed to write to queue.txt")?;
        // Read the queue file to compute statistics (total queued items).
        let contents = std::fs::read_to_string(&queue_path)
            .context("Failed to read queue.txt after appending")?;
        let total_queued = contents.lines().filter(|l| !l.trim().is_empty()).count();
        println!(
            "Book '{}' has been added to the queue. Queue length: {}",
            bookid,
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
    let mut current_id = bookid.clone();
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

/// Deletes `~/Downloads/oreilly-epub/{bookid}.epub` if it exists, so the book is fetched again.
fn delete_downloaded_epub(bookid: &str) -> Result<()> {
    // The ID becomes part of a path, so only plain digits are accepted.
    if bookid.is_empty() || !bookid.chars().all(|c| c.is_ascii_digit()) {
        bail!("'{bookid}' is not a valid book ID (digits only).");
    }
    let user_dirs = UserDirs::new().context("Could not get XDG user directories.")?;
    let epub_path = user_dirs
        .download_dir()
        .unwrap_or(&user_dirs.home_dir().join("Downloads"))
        .join("oreilly-epub")
        .join(format!("{bookid}.epub"));
    if epub_path.exists() {
        std::fs::remove_file(&epub_path)
            .with_context(|| format!("Failed to delete {}", epub_path.display()))?;
        println!("Deleted existing {}.", epub_path.display());
    } else {
        println!("Book '{bookid}' was not downloaded yet.");
    }
    Ok(())
}

/// Appends `isbns` to `queue.txt`, skipping IDs that are already queued, already downloaded, or
/// repeated in the arguments.
fn add_to_queue(isbns: &[String], data_root: &Path) -> Result<()> {
    let (ids, invalid): (Vec<&String>, Vec<&String>) = isbns
        .iter()
        .partition(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()));

    let queue_path = data_root.join("queue.txt");
    let mut known: std::collections::HashSet<String> = std::fs::read_to_string(&queue_path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_owned())
        .collect();
    let download_dir = UserDirs::new().map(|u| {
        u.download_dir()
            .unwrap_or(&u.home_dir().join("Downloads"))
            .join("oreilly-epub")
    });

    let (mut added, mut skipped) = (Vec::new(), 0);
    for id in ids.into_iter().cloned() {
        let downloaded = download_dir
            .as_ref()
            .is_some_and(|dir| dir.join(format!("{id}.epub")).exists());
        if downloaded || !known.insert(id.clone()) {
            skipped += 1;
        } else {
            added.push(id);
        }
    }

    if !added.is_empty() {
        let mut queue_file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&queue_path)
            .context("Failed to open queue.txt for appending")?;
        for id in &added {
            writeln!(queue_file, "{id}").context("Failed to write to queue.txt")?;
        }
    }

    println!(
        "Added {} book(s) to the queue ({skipped} already queued or downloaded).",
        added.len()
    );
    for id in &invalid {
        println!("Ignored invalid book ID: {id}");
    }
    if !added.is_empty() && !data_root.join("queue.lock").exists() {
        println!("No download is running. Start one with `oreilly <BOOKID>` to process the queue.");
    }
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
