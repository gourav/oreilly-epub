// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

use anyhow::{Context, Result};
use reqwest::{Client, cookie::Jar};
use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

/// Reads the cookies.json file and builds an authenticated reqwest client.
pub fn build_authenticated_client(cookies_path: &PathBuf) -> Result<Client> {
    // Read the JSON file.
    println!("Reading cookies from {cookies_path:?}...");
    let cookies_content = fs::read(cookies_path)
        .with_context(|| format!("Failed to read cookies file from {cookies_path:?}."))?;

    // Parse the JSON into a Rust HashMap.
    let cookies_map: HashMap<String, String> = serde_json::from_slice(&cookies_content)
        .context("Failed to parse cookies file. Ensure it is a flat key-value JSON object.")?;

    // Create a Cookie Jar.
    let jar = Arc::new(Jar::default());
    // `reqwest` needs a URL to associate the cookies with.
    let url = "https://learning.oreilly.com".parse::<reqwest::Url>()?;
    for (key, value) in cookies_map {
        // `reqwest` expects cookies in the standard "key=value" string format.
        let cookie_str = format!("{}={}", key, value);
        jar.add_cookie_str(&cookie_str, &url);
    }

    // Build the client with the cookie provider and a standard User-Agent.
    let client = Client::builder()
        .cookie_provider(jar)
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:147.0) Gecko/20100101 Firefox/147.0",
        )
        .build()
        .context("Failed to build the HTTP client")?;

    Ok(client)
}
