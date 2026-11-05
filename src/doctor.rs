// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

//! `doctor`: applies the stylesheet fixes (and the embedded code font) to EPUBs that were
//! already downloaded.

use crate::{
    style::StyleOptions,
    xml::{add_manifest_items, find_opf_path},
};
use anyhow::{Context, Result, bail};
use relative_path::{RelativePath, RelativePathBuf};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

enum Outcome {
    Fixed,
    AlreadyFixed,
    NoStylesheet,
}

/// Fixes every EPUB in `paths` (files, or directories searched non-recursively). With no paths,
/// the current directory is used.
pub fn run(paths: &[PathBuf], style: &StyleOptions, dry_run: bool) -> Result<()> {
    if !style.is_active() {
        bail!("Nothing to do: --keep-styles is set and no --code-font was given.");
    }
    let files = collect_epubs(paths)?;
    if files.is_empty() {
        println!("No .epub files found.");
        return Ok(());
    }

    let (mut fixed, mut already, mut failed) = (0, 0, 0);
    for file in &files {
        match fix_epub(file, style, dry_run) {
            Ok(Outcome::Fixed) => {
                fixed += 1;
                let verb = if dry_run { "would fix" } else { "fixed" };
                println!("{verb}: {}", file.display());
            }
            Ok(Outcome::AlreadyFixed) => {
                already += 1;
                println!("already fixed: {}", file.display());
            }
            Ok(Outcome::NoStylesheet) => {
                already += 1;
                println!("no stylesheet, skipped: {}", file.display());
            }
            Err(e) => {
                failed += 1;
                println!("error: {}: {e:#}", file.display());
            }
        }
    }
    let verb = if dry_run { "would fix" } else { "fixed" };
    println!("\n{verb} {fixed}, already fixed {already}, failed {failed}.");
    if failed > 0 {
        bail!("{failed} file(s) could not be fixed.");
    }
    Ok(())
}

/// Expands the given paths into a sorted list of `.epub` files.
fn collect_epubs(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let default = [PathBuf::from(".")];
    let paths = if paths.is_empty() { &default } else { paths };
    let is_epub = |p: &Path| {
        p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
    };

    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            for entry in std::fs::read_dir(path)
                .with_context(|| format!("Failed to read directory {}", path.display()))?
            {
                let p = entry?.path();
                if p.is_file() && is_epub(&p) {
                    found.push(p);
                }
            }
            found.sort();
            files.extend(found);
        } else if path.is_file() {
            files.push(path.clone());
        } else {
            bail!("{} does not exist.", path.display());
        }
    }
    Ok(files)
}

fn read_entry(archive: &mut ZipArchive<BufReader<File>>, name: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    archive
        .by_name(name)
        .with_context(|| format!("{name} not found in the archive"))?
        .read_to_end(&mut data)?;
    Ok(data)
}

/// Rewrites one EPUB in place (through a temporary file next to it).
fn fix_epub(path: &Path, style: &StyleOptions, dry_run: bool) -> Result<Outcome> {
    let file = File::open(path).context("Failed to open file")?;
    let mut archive = ZipArchive::new(BufReader::new(file)).context("Not a valid zip/EPUB")?;
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.name_for_index(i).unwrap_or_default().to_owned())
        .collect();
    let name_set: HashSet<&str> = names.iter().map(String::as_str).collect();

    // Locate the OPF; fonts go next to it, so that manifest hrefs stay simple.
    let container = read_entry(&mut archive, "META-INF/container.xml")?;
    let opf_path = RelativePathBuf::from(find_opf_path(&container)?);
    let opf_dir = opf_path.parent().unwrap_or(RelativePath::new("")).to_owned();
    let mut style = style.clone();
    style.relocate_fonts(&opf_dir);

    // Work out which entries change.
    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();
    let css_names: Vec<&String> = names
        .iter()
        .filter(|n| n.to_ascii_lowercase().ends_with(".css"))
        .collect();
    // Without a stylesheet there is nothing to simplify, and the font would go unused.
    if css_names.is_empty() {
        return Ok(Outcome::NoStylesheet);
    }
    for name in css_names {
        let old = String::from_utf8(read_entry(&mut archive, name)?)
            .with_context(|| format!("{name} is not valid UTF-8"))?;
        let css_dir = RelativePath::new(name)
            .parent()
            .unwrap_or(RelativePath::new(""));
        let new = style.transform_css(&old, css_dir);
        if new != old {
            replacements.insert(name.clone(), new.into_bytes());
        }
    }
    if !style.font_faces().is_empty() {
        let items: Vec<(String, String, &str)> = style
            .font_faces()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let href = opf_dir.relative(RelativePath::new(&f.path)).to_string();
                (href, format!("operator-mono-{i}"), "font/otf")
            })
            .collect();
        let opf = read_entry(&mut archive, opf_path.as_str())?;
        let mut new_opf = Vec::new();
        if add_manifest_items(opf.as_slice(), &mut new_opf, &items)? > 0 {
            replacements.insert(opf_path.to_string(), new_opf);
        }
    }
    let new_fonts: Vec<_> = style
        .font_faces()
        .iter()
        .filter(|f| !name_set.contains(f.path.as_str()))
        .collect();

    if replacements.is_empty() && new_fonts.is_empty() {
        return Ok(Outcome::AlreadyFixed);
    }
    if dry_run {
        return Ok(Outcome::Fixed);
    }

    // Write the new archive. Untouched entries are copied as-is, which keeps `mimetype` first and
    // uncompressed.
    let tmp = path.with_extension("epub.tmp");
    let written = (|| -> Result<()> {
        let mut zip = ZipWriter::new(File::create(&tmp)?);
        let options: FileOptions<()> =
            FileOptions::default().compression_method(CompressionMethod::Deflated);
        for (i, name) in names.iter().enumerate() {
            if let Some(bytes) = replacements.get(name) {
                zip.start_file(name.as_str(), options)?;
                zip.write_all(bytes)?;
            } else {
                zip.raw_copy_file(archive.by_index_raw(i)?)?;
            }
        }
        for font in &new_fonts {
            zip.start_file(font.path.as_str(), options)?;
            zip.write_all(&font.data)?;
        }
        zip.finish()?;
        Ok(())
    })();
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    std::fs::rename(&tmp, path).context("Failed to replace the original file")?;
    Ok(Outcome::Fixed)
}
