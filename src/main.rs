use futures_util::stream::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use log::{debug, error, info, warn};
use env_logger::Env;


#[derive(Debug, Deserialize)]
struct Antenna {
    id: String,
    name: String,
    #[serde(rename = "keywords")]
    keywords_groups: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    event: String,
    #[serde(rename = "payload")]
    payload_str: String,
}

#[derive(Debug)]
struct ServerSentEvent {
    event: String,
    data: String,
}

#[derive(Debug, Deserialize)]
struct Post {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    uri: String,  // We need this for importing
    #[serde(default)]
    visibility: String,
    account: Account,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    id: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    acct: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Serialize)]
struct ImportRequest {
    uri: String,
}

// Add this function to handle the streaming connection with reconnection
async fn connect_to_stream(
    client: &Client, 
    streaming_url: &str,
    base_url: &str,
    keyword_groups: &Arc<Mutex<Vec<Vec<String>>>>,
    api_token: &str
) -> Result<(), Box<dyn Error>> {
    let mut retry_count = 0;
    let max_retries = 5;
    
    loop {
        info!("Connecting to streaming API at {}", streaming_url);
        
        // Create a client specifically for streaming with no timeout
        let streaming_client = Client::builder()
            .tcp_keepalive(Some(Duration::from_secs(10)))
            .build()?;
            
        match streaming_client.get(streaming_url)
            .header("Accept", "text/event-stream")
            .header("Connection", "keep-alive")
            .send()
            .await {
                
            Ok(response) => {
                if !response.status().is_success() {
                    error!("Failed to connect: HTTP {}", response.status());
                    retry_count += 1;
                } else {
                    info!("Connected to streaming API successfully with status: {}", response.status());
                    retry_count = 0; // Reset retry count on successful connection
                    
                    // Process the streaming response
                    let mut stream = response.bytes_stream();
                    let mut buffer = String::new();
                    let mut last_activity = std::time::Instant::now();
                    let mut incomplete_utf8 = Vec::new(); // Buffer for incomplete UTF-8 sequences
                    
                    while let Some(chunk_result) = stream.next().await {
                        // Update last activity timestamp
                        last_activity = std::time::Instant::now();
                        
                        match chunk_result {
                            Ok(chunk) => {
                                // Combine with any incomplete UTF-8 from previous chunk
                                let mut bytes = Vec::new();
                                bytes.extend_from_slice(&incomplete_utf8);
                                bytes.extend_from_slice(&chunk);
                                
                                // Try to decode as UTF-8, handling partial characters at the end
                                match String::from_utf8(bytes.clone()) {
                                    Ok(text) => {
                                        // Successfully decoded the entire chunk
                                        buffer.push_str(&text);
                                        incomplete_utf8.clear();
                                    },
                                    Err(e) => {
                                        // Get the valid part of the string
                                        let valid_up_to = e.utf8_error().valid_up_to();
                                        
                                        if valid_up_to > 0 {
                                            // Add the valid part to our buffer
                                            let valid_text = String::from_utf8_lossy(&bytes[0..valid_up_to]).into_owned();
                                            buffer.push_str(&valid_text);
                                            
                                            // Save the incomplete part for the next chunk
                                            incomplete_utf8 = bytes[valid_up_to..].to_vec();
                                        } else {
                                            // The entire chunk is invalid, save it for the next chunk
                                            incomplete_utf8 = bytes;
                                        }
                                        
                                        debug!("Incomplete UTF-8 sequence at chunk boundary, saved {} bytes", incomplete_utf8.len());
                                    }
                                }
                                
                                // Process complete events in the buffer
                                if buffer.contains("\n\n") {
                                    let mut parts: Vec<String> = buffer.split("\n\n")
                                        .map(|s| s.to_string())
                                        .collect();
                                    
                                    let last_part = if !parts.is_empty() {
                                        parts.pop().unwrap()
                                    } else {
                                        String::new()
                                    };
                                    
                                    for event in parts {
                                        if !event.is_empty() {
                                            if let Err(e) = process_message(&event, client, base_url, keyword_groups, api_token).await {
                                                error!("Error processing message: {}", e);
                                            }
                                        }
                                    }
                                    
                                    buffer = last_part;
                                }
                            },
                            Err(e) => {
                                error!("Error receiving chunk: {}", e);
                                break; // Break the inner loop to trigger reconnection
                            }
                        }
                        
                        // Check for inactivity (no data for 2 minutes)
                        if last_activity.elapsed() > Duration::from_secs(120) {
                            warn!("No activity for 2 minutes, reconnecting...");
                            break;
                        }
                    }
                    
                    warn!("Stream ended or timed out, will attempt to reconnect");
                }
            },
            Err(e) => {
                error!("Connection error: {}", e);
                retry_count += 1;
            }
        }
        
        if retry_count >= max_retries {
            return Err("Maximum retry attempts reached".into());
        }
        
        // Exponential backoff for retries
        let delay = std::cmp::min(2_u64.pow(retry_count), 60);
        info!("Reconnecting in {} seconds...", delay);
        time::sleep(Duration::from_secs(delay)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_default_env()
        .format_timestamp_secs()
        .init();
    
    info!("Starting Super Antenna");
    
    // Configuration
    let base_url = std::env::var("BASE_URL").expect("BASE_URL must be set");
    let streaming_url = std::env::var("STREAMING_URL").expect("STREAMING_URL must be set");
    let api_token = std::env::var("API_TOKEN").expect("API_TOKEN must be set");
    
    info!("Using base URL: {}", base_url);
    info!("Using streaming URL: {}", streaming_url);
    
    // Create HTTP client with no timeout for streaming connections
    let client = Client::builder()
        .tcp_keepalive(Some(Duration::from_secs(10)))  // Keep TCP keepalive
        .build()?;
    
    // Create shared keywords structure
    let keyword_groups = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    
    // Clone references for the keyword fetcher task
    let keyword_groups_for_fetcher = Arc::clone(&keyword_groups);
    let client_for_fetcher = client.clone();
    let base_url_for_fetcher = base_url.clone();
    
    // Spawn keyword fetcher task
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(60));
        
        loop {
            interval.tick().await;
            match fetch_keywords(&client_for_fetcher, &base_url_for_fetcher).await {
                Ok(new_keyword_groups) => {
                    let mut keyword_groups_guard = keyword_groups_for_fetcher.lock().unwrap();
                    *keyword_groups_guard = new_keyword_groups;
                    info!("Updated keyword groups: {:?}", *keyword_groups_guard);
                }
                Err(e) => error!("Failed to fetch keywords: {}", e),
            }
        }
    });
    
    // Initial keyword fetch
    match fetch_keywords(&client, &base_url).await {
        Ok(new_keyword_groups) => {
            let mut keyword_groups_guard = keyword_groups.lock().unwrap();
            *keyword_groups_guard = new_keyword_groups;
            info!("Initial keyword groups: {:?}", *keyword_groups_guard);
        }
        Err(e) => error!("Failed to fetch initial keywords: {}", e),
    }
    
    connect_to_stream(&client, &streaming_url, &base_url, &keyword_groups, &api_token).await?;
    
    Ok(())
}

async fn fetch_keywords(client: &Client, base_url: &str) -> Result<Vec<Vec<String>>, Box<dyn Error>> {
    info!("Fetching keywords from {}", base_url);
    
    let url = format!("{}/api/admin/antennas/global", base_url);
    
    let response = client.post(&url)
        .json(&serde_json::json!({"i": std::env::var("API_TOKEN").expect("API_TOKEN must be set")}))
        .header("Content-Type", "application/json")
        .send()
        .await?;
    
    if response.status() != StatusCode::OK {
        return Err(format!("Failed to fetch keywords: HTTP {}", response.status()).into());
    }
    
    let antennas: Vec<Antenna> = response.json().await?;
    
    let mut keyword_groups = Vec::new();
    for antenna in antennas {
        for keyword_group in antenna.keywords_groups {
            // Filter out empty keywords and convert to lowercase
            let filtered_group: Vec<String> = keyword_group
                .into_iter()
                .filter(|k| !k.is_empty())
                .map(|k| k.to_lowercase())
                .collect();
            
            if !filtered_group.is_empty() {
                keyword_groups.push(filtered_group);
            }
        }
    }
    
    info!("Fetched {} keyword groups", keyword_groups.len());
    Ok(keyword_groups)
}

async fn process_message(
    message: &str,
    client: &Client,
    base_url: &str,
    keyword_groups: &Arc<Mutex<Vec<Vec<String>>>>,
    api_token: &str,
) -> Result<(), Box<dyn Error>> {
    // Parse the server-sent event
    let sse = match parse_server_sent_event(message) {
        Ok(sse) => sse,
        Err(e) => {
            debug!("Skipping malformed SSE: {}", e);
            debug!("Message content: {}", message);
            return Ok(());
        }
    };

    // Only process update events
    if sse.event != "update" {
        debug!("Skipping non-update event: {}", sse.event);
        return Ok(());
    }

    // Parse the post from the data
    let post: Post = match serde_json::from_str(&sse.data) {
        Ok(post) => post,
        Err(e) => {
            debug!("Failed to parse post: {}", e);
            debug!("JSON content: {}", sse.data);
            return Ok(());
        }
    };
    
    // Check if the post contains any of our keyword groups
    let keyword_groups_guard = keyword_groups.lock().unwrap();
    let post_content = post.content.to_lowercase();
    
    // Check if any keyword group matches (OR condition between groups)
    let matches = keyword_groups_guard.iter().any(|group| {
        // Check if all keywords in this group match (AND condition within group)
        group.iter().all(|keyword| matches_word_boundary(&post_content, keyword))
    });
    
    if matches {
        info!("Found matching post: {} by @{}", post.id, post.account.username);
        
        // Import the post using the actual URI from the post
        let import_url = format!("{}/api/ap/show", base_url);
        let import_request = ImportRequest { uri: post.uri };
        
        let response = client.post(&import_url)
            .header("Authorization", format!("Bearer {}", api_token))
            .json(&import_request)
            .send()
            .await?;
        
        if response.status().is_success() {
            info!("Successfully imported post {}", post.id);
        } else {
            error!("Failed to import post {}: HTTP {}", post.id, response.status());
        }
    }
    
    Ok(())
}

fn extract_words(text: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    
    // Split text by non-alphanumeric characters and collect words
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        let word = word.trim();
        if !word.is_empty() {
            words.insert(word.to_string());
        }
    }
    
    words
}

fn matches_word_boundary(content: &str, keyword: &str) -> bool {
    // Split the keyword by spaces to handle individual words
    let keyword_parts: Vec<&str> = keyword.split_whitespace().collect();
    
    // If there are multiple parts, each part must match as a whole word
    if keyword_parts.len() > 1 {
        return keyword_parts.iter().all(|part| {
            content.split(|c: char| !c.is_alphanumeric())
                .any(|word| word.trim().eq_ignore_ascii_case(part))
        });
    }
    
    // For single words, check with word boundaries
    content.split(|c: char| !c.is_alphanumeric())
        .any(|word| word.trim().eq_ignore_ascii_case(keyword))
}

fn parse_server_sent_event(message: &str) -> Result<ServerSentEvent, Box<dyn Error>> {
    let mut event = String::new();
    let mut data = String::new();
    
    for line in message.lines() {
        if line.starts_with("event:") {
            event = line.trim_start_matches("event:").trim().to_string();
        } else if line.starts_with("data:") {
            data = line.trim_start_matches("data:").trim().to_string();
        }
    }
    
    if event.is_empty() {
        return Err("Missing event type in SSE".into());
    }
    
    if data.is_empty() {
        data = "{}".to_string();
        warn!("Empty data in SSE with event type: {}", event);
    }
    
    Ok(ServerSentEvent { event, data })
}
