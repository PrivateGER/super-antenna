use futures_util::stream::StreamExt;
use log::{debug, error, info, warn};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap};
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time;
use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Registry};
use warp::Filter;

#[derive(Debug, Deserialize, Clone)]
struct Antenna {
    id: String,
    #[serde(rename = "keywords")]
    keywords_groups: Vec<Vec<String>>,
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
    account: Account,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    username: String,
}

#[derive(Debug, Serialize)]
struct ImportRequest {
    uri: String,
}

// Add this struct to track rate limiting information
#[derive(Debug)]
struct RateLimitInfo {
    matches: Vec<Instant>,
    max_per_minute: usize,
    last_warning_time: Option<Instant>,
}

impl RateLimitInfo {
    fn new(max_per_minute: usize) -> Self {
        Self {
            matches: Vec::new(),
            max_per_minute,
            last_warning_time: None,
        }
    }

    fn can_process(&mut self) -> bool {
        // Remove timestamps older than 1 minute
        let one_minute_ago = Instant::now() - Duration::from_secs(60);
        self.matches.retain(|&timestamp| timestamp > one_minute_ago);
        
        // Check if we're under the limit
        if self.matches.len() < self.max_per_minute {
            self.matches.push(Instant::now());
            true
        } else {
            false
        }
    }
    
    fn should_log_rate_limit(&mut self) -> bool {
        let now = Instant::now();
        
        // Only log warnings once per minute
        match self.last_warning_time {
            Some(last_time) if now.duration_since(last_time) < Duration::from_secs(60) => false,
            _ => {
                self.last_warning_time = Some(now);
                true
            }
        }
    }
}

// Create a struct to hold our metrics
#[derive(Clone)]
struct Metrics {
    registry: Registry,
    posts_processed: IntCounter,
    posts_matched: IntCounterVec,
    posts_rate_limited: IntCounterVec,
    active_antennas: IntGauge,
    post_processing_time: Histogram,
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        
        let posts_processed = IntCounter::new(
            "super_antenna_posts_processed_total", 
            "Total number of posts processed"
        ).unwrap();
        
        let posts_matched = IntCounterVec::new(
            prometheus::opts!("super_antenna_posts_matched_total", "Total number of posts matched by antenna"),
            &["antenna_id"]
        ).unwrap();
        
        let posts_rate_limited = IntCounterVec::new(
            prometheus::opts!("super_antenna_posts_rate_limited_total", "Total number of posts rate limited by antenna"),
            &["antenna_id"]
        ).unwrap();
        
        let active_antennas = IntGauge::new(
            "super_antenna_active_antennas", 
            "Number of active antennas"
        ).unwrap();
        
        let post_processing_time = Histogram::with_opts(
            HistogramOpts::new(
                "super_antenna_post_processing_time_seconds",
                "Time taken to process a post"
            ).buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0])
        ).unwrap();
        
        // Register all metrics
        registry.register(Box::new(posts_processed.clone())).unwrap();
        registry.register(Box::new(posts_matched.clone())).unwrap();
        registry.register(Box::new(posts_rate_limited.clone())).unwrap();
        registry.register(Box::new(active_antennas.clone())).unwrap();
        registry.register(Box::new(post_processing_time.clone())).unwrap();
        
        Self {
            registry,
            posts_processed,
            posts_matched,
            posts_rate_limited,
            active_antennas,
            post_processing_time,
        }
    }
}

// Add this function to handle the streaming connection with reconnection
async fn connect_to_stream(
    client: &Client, 
    streaming_url: &str,
    base_url: &str,
    antennas: &Arc<Mutex<Vec<Antenna>>>,
    rate_limits: &Arc<Mutex<HashMap<String, RateLimitInfo>>>,
    api_token: &str,
    metrics: &Metrics
) -> Result<(), Box<dyn Error>> {
    let mut retry_count = 0;
    let max_retries = 5;
    
    // Get optional streaming token from environment
    let streaming_token = std::env::var("STREAMING_TOKEN").ok();
    if let Some(token) = &streaming_token {
        info!("Using authentication for streaming API");
    }
    
    loop {
        info!("Connecting to streaming API at {}", streaming_url);
        
        // Create a client specifically for streaming with no timeout
        let streaming_client = Client::builder()
            .tcp_keepalive(Some(Duration::from_secs(10)))
            .build()?;
            
        // Build request with optional authentication
        let mut request = streaming_client.get(streaming_url)
            .header("Accept", "text/event-stream")
            .header("Connection", "keep-alive");
            
        // Add authorization header if streaming token is provided
        if let Some(token) = &streaming_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }
        
        match request.send().await {
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
                    let mut last_activity;
                    let mut incomplete_utf8 = Vec::new(); // Buffer for incomplete UTF-8 sequences
                    
                    while let Some(chunk_result) = stream.next().await {
                        // Update last activity timestamp
                        last_activity = Instant::now();
                        
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
                                            if let Err(e) = process_message(&event, client, base_url, antennas, rate_limits, api_token, metrics).await {
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
    
    // Check if streaming token is set
    if std::env::var("STREAMING_TOKEN").is_ok() {
        info!("Authentication for streaming API is enabled");
    } else {
        info!("Authentication for streaming API is disabled");
    }
    
    // Get metrics port from environment or use default
    let metrics_port = match std::env::var("METRICS_PORT") {
        Ok(val) => val.parse::<u16>().unwrap_or(9091),
        Err(_) => 9091, // Default to port 9091
    };
    
    info!("Using base URL: {}", base_url);
    info!("Using streaming URL: {}", streaming_url);
    info!("Metrics will be exposed on port {}", metrics_port);
    
    // Initialize metrics
    let metrics = Metrics::new();
    
    // Create HTTP client with no timeout for streaming connections
    let client = Client::builder()
        .tcp_keepalive(Some(Duration::from_secs(10)))  // Keep TCP keepalive
        .build()?;
    
    // Create shared keywords structure
    let antennas = Arc::new(Mutex::new(Vec::<Antenna>::new()));
    
    // Create rate limit tracker
    let rate_limits = Arc::new(Mutex::new(HashMap::<String, RateLimitInfo>::new()));
    
    // Get max matches per minute from environment or use default
    let max_matches_per_minute = match std::env::var("MAX_MATCHES_PER_MINUTE") {
        Ok(val) => val.parse::<usize>().unwrap_or(15),
        Err(_) => 15, // Default to 15 matches per minute per antenna
    };
    
    info!("Rate limiting set to {} matches per minute per antenna", max_matches_per_minute);
    
    // Clone references for the antenna fetcher task
    let antennas_for_fetcher = Arc::clone(&antennas);
    let rate_limits_for_fetcher = Arc::clone(&rate_limits);
    let client_for_fetcher = client.clone();
    let base_url_for_fetcher = base_url.clone();
    let metrics_for_fetcher = metrics.clone();
    
    // Spawn antenna fetcher task
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(60));
        
        loop {
            interval.tick().await;
            match fetch_antennas(&client_for_fetcher, &base_url_for_fetcher).await {
                Ok(new_antennas) => {
                    let mut antennas_guard = antennas_for_fetcher.lock().unwrap();
                    *antennas_guard = new_antennas;
                    
                    // Update metrics for active antennas
                    metrics_for_fetcher.active_antennas.set(antennas_guard.len() as i64);
                    
                    // Update rate limit trackers for new antennas
                    let mut rate_limits_guard = rate_limits_for_fetcher.lock().unwrap();
                    for antenna in antennas_guard.iter() {
                        if !rate_limits_guard.contains_key(&antenna.id) {
                            rate_limits_guard.insert(antenna.id.clone(), RateLimitInfo::new(max_matches_per_minute));
                        }
                    }
                    
                    info!("Updated antennas: {}", antennas_guard.len());
                }
                Err(e) => error!("Failed to fetch antennas: {}", e),
            }
        }
    });
    
    // Initial antenna fetch
    match fetch_antennas(&client, &base_url).await {
        Ok(new_antennas) => {
            let mut antennas_guard = antennas.lock().unwrap();
            *antennas_guard = new_antennas;
            
            // Set initial metrics for active antennas
            metrics.active_antennas.set(antennas_guard.len() as i64);
            
            // Initialize rate limit trackers
            let mut rate_limits_guard = rate_limits.lock().unwrap();
            for antenna in antennas_guard.iter() {
                rate_limits_guard.insert(antenna.id.clone(), RateLimitInfo::new(max_matches_per_minute));
            }
            
            info!("Initial antennas loaded: {}", antennas_guard.len());
        }
        Err(e) => error!("Failed to fetch initial antennas: {}", e),
    }
    
    // Clone metrics for the metrics server
    let metrics_for_server = metrics.clone();
    
    // Start metrics server
    tokio::spawn(async move {
        // Create a warp filter that responds with the metrics
        let metrics_route = warp::path!("metrics")
            .map(move || {
                let encoder = prometheus::TextEncoder::new();
                let metric_families = metrics_for_server.registry.gather();
                let mut buffer = Vec::new();
                encoder.encode(&metric_families, &mut buffer).unwrap();
                String::from_utf8(buffer).unwrap()
            });
        
        // Add a health check endpoint
        let health_route = warp::path!("health")
            .map(|| "OK");
        
        // Combine routes
        let routes = metrics_route.or(health_route);
        
        info!("Starting metrics server on port {}", metrics_port);
        warp::serve(routes)
            .run(([0, 0, 0, 0], metrics_port))
            .await;
    });
    
    // Clone metrics for the streaming connection
    let metrics_for_stream = metrics.clone();
    
    connect_to_stream(
        &client, 
        &streaming_url, 
        &base_url, 
        &antennas, 
        &rate_limits, 
        &api_token,
        &metrics_for_stream
    ).await?;
    
    Ok(())
}

async fn fetch_antennas(client: &Client, base_url: &str) -> Result<Vec<Antenna>, Box<dyn Error>> {
    info!("Fetching antennas from {}", base_url);
    
    let url = format!("{}/api/admin/antennas/global", base_url);
    
    let response = client.post(&url)
        .json(&serde_json::json!({"i": std::env::var("API_TOKEN").expect("API_TOKEN must be set")}))
        .header("Content-Type", "application/json")
        .send()
        .await?;
    
    if response.status() != StatusCode::OK {
        return Err(format!("Failed to fetch antennas: HTTP {}", response.status()).into());
    }
    
    let antennas: Vec<Antenna> = response.json().await?;
    info!("Fetched {} antennas", antennas.len());
    
    Ok(antennas)
}

async fn process_message(
    message: &str,
    client: &Client,
    base_url: &str,
    antennas: &Arc<Mutex<Vec<Antenna>>>,
    rate_limits: &Arc<Mutex<HashMap<String, RateLimitInfo>>>,
    api_token: &str,
    metrics: &Metrics,
) -> Result<(), Box<dyn Error>> {
    // Start timing the processing
    let timer = metrics.post_processing_time.start_timer();
    
    // Increment the posts processed counter
    metrics.posts_processed.inc();
    
    // Parse the server-sent event
    let sse = match parse_server_sent_event(message) {
        Ok(sse) => sse,
        Err(e) => {
            debug!("Skipping malformed SSE: {}", e);
            debug!("Message content: {}", message);
            // Stop the timer
            timer.observe_duration();
            return Ok(());
        }
    };

    // Only process update events
    if sse.event != "update" {
        debug!("Skipping non-update event: {}", sse.event);
        // Stop the timer
        timer.observe_duration();
        return Ok(());
    }

    // Parse the post from the data
    let post: Post = match serde_json::from_str(&sse.data) {
        Ok(post) => post,
        Err(e) => {
            debug!("Failed to parse post: {}", e);
            debug!("JSON content: {}", sse.data);
            // Stop the timer
            timer.observe_duration();
            return Ok(());
        }
    };
    
    // Check if the post contains any of our keyword groups
    let antennas_guard = antennas.lock().unwrap();
    let post_content = post.content.to_lowercase();
    
    // Find matching antennas
    let matching_antennas: Vec<String> = antennas_guard.iter()
        .filter(|antenna| {
            antenna.keywords_groups.iter().any(|group| {
                // Check if all keywords in this group match (AND condition within group)
                group.iter().all(|keyword| matches_word_boundary(&post_content, keyword))
            })
        })
        .map(|antenna| antenna.id.clone())
        .collect();
    
    // Drop the antennas guard before processing matches
    drop(antennas_guard);
    
    if !matching_antennas.is_empty() {
        // Check rate limits for each matching antenna
        let mut rate_limits_guard = rate_limits.lock().unwrap();
        let mut processed_antennas = Vec::new();
        
        for antenna_id in matching_antennas {
            let rate_limit = rate_limits_guard.entry(antenna_id.clone())
                .or_insert_with(|| {
                    warn!("Creating missing rate limit entry for antenna {}", antenna_id);
                    RateLimitInfo::new(5) // Default to 5 if missing
                });
            
            if rate_limit.can_process() {
                processed_antennas.push(antenna_id.clone());
                // Increment the matched posts counter for this antenna
                metrics.posts_matched.with_label_values(&[&antenna_id]).inc();
            } else {
                // Increment the rate limited posts counter for this antenna
                metrics.posts_rate_limited.with_label_values(&[&antenna_id]).inc();
                
                // Only log if this is the first time we're hitting the limit
                if rate_limit.should_log_rate_limit() {
                    warn!("Rate limit exceeded for antenna {}, skipping matches until rate limit resets", antenna_id);
                }
            }
        }
        
        // Drop the rate limits guard before making API calls
        drop(rate_limits_guard);

        if !processed_antennas.is_empty() {
            info!("Found matching post: {} by @{} for {} antennas", 
                  post.id, post.account.username, processed_antennas.len());
            
            // Import the post using the actual URI from the post
            let import_url = format!("{}/api/ap/show", base_url);
            let import_request = ImportRequest { uri: post.uri };
            
            let response = client.post(&import_url)
                .header("Authorization", format!("Bearer {}", api_token))
                .json(&import_request)
                .send()
                .await?;
            
            if response.status().is_success() {
                info!("Successfully imported post {} for antennas: {:?}", post.id, processed_antennas);
            } else {
                error!("Failed to import post {}: HTTP {}", post.id, response.status());
            }
        }
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use serde_json::json;

    #[test]
    fn test_parse_server_sent_event() {
        // Test valid event
        let message = "event: update\ndata: {\"content\":\"test\"}";
        let result = parse_server_sent_event(message).unwrap();
        assert_eq!(result.event, "update");
        assert_eq!(result.data, "{\"content\":\"test\"}");

        // Test missing data (should default to {})
        let message = "event: delete\n";
        let result = parse_server_sent_event(message).unwrap();
        assert_eq!(result.event, "delete");
        assert_eq!(result.data, "{}");

        // Test missing event (should error)
        let message = "data: {\"content\":\"test\"}";
        assert!(parse_server_sent_event(message).is_err());
    }

    #[test]
    fn test_matches_word_boundary() {
        // Test single word matches
        assert!(matches_word_boundary("hello world", "hello"));
        assert!(matches_word_boundary("hello world", "world"));
        
        // Test case insensitivity
        assert!(matches_word_boundary("Hello World", "hello"));
        assert!(matches_word_boundary("HELLO WORLD", "world"));
        
        // Test word boundaries
        assert!(!matches_word_boundary("helloworld", "hello"));
        assert!(!matches_word_boundary("worldly", "world"));
        
        // Test multi-word matches
        assert!(matches_word_boundary("hello beautiful world", "hello world"));
        assert!(!matches_word_boundary("helloworld", "hello world"));
        
        // Test with punctuation
        assert!(matches_word_boundary("hello, world!", "hello"));
        assert!(matches_word_boundary("hello, world!", "world"));
        
        // Test with special characters
        assert!(matches_word_boundary("hello-world", "hello"));
        assert!(matches_word_boundary("hello_world", "world"));
        
        // Test empty strings
        assert!(!matches_word_boundary("", "hello"));
        assert!(!matches_word_boundary("hello world", ""));
        
        // Test with numbers
        assert!(matches_word_boundary("test123 hello", "test123"));
        assert!(matches_word_boundary("hello 42 world", "42"));
        
        // Test with multiple spaces
        assert!(matches_word_boundary("hello    world", "hello world"));
    }

    #[test]
    fn test_rate_limit_info() {
        let mut rate_limit = RateLimitInfo::new(3);
        
        // Should allow first 3 requests
        assert!(rate_limit.can_process());
        assert!(rate_limit.can_process());
        assert!(rate_limit.can_process());
        
        // Should deny 4th request
        assert!(!rate_limit.can_process());
        
        // Test expiration of old requests
        let mut rate_limit = RateLimitInfo::new(2);
        assert!(rate_limit.can_process());
        assert!(rate_limit.can_process());
        assert!(!rate_limit.can_process());
        
        // Manually modify timestamps to simulate time passing
        rate_limit.matches[0] = Instant::now() - Duration::from_secs(61);
        
        // Should allow another request after one expired
        assert!(rate_limit.can_process());
        assert!(!rate_limit.can_process());
        
        // Test with zero limit
        let mut zero_limit = RateLimitInfo::new(0);
        assert!(!zero_limit.can_process());
    }
    
    #[test]
    fn test_antenna_keyword_matching() {
        // Create test antennas with different keyword groups
        let antenna1 = Antenna {
            id: "1".to_string(),
            keywords_groups: vec![
                vec!["rust".to_string(), "programming".to_string()],
                vec!["tokio".to_string()]
            ]
        };
        
        let antenna2 = Antenna {
            id: "2".to_string(),
            keywords_groups: vec![
                vec!["hello".to_string(), "world".to_string()]
            ]
        };
        
        let antennas = Arc::new(Mutex::new(vec![antenna1, antenna2]));
        
        // Test post that should match antenna1's first group (both keywords)
        let post_content1 = "I love rust programming!".to_lowercase();
        let antennas_guard = antennas.lock().unwrap();
        let matching1: Vec<String> = antennas_guard.iter()
            .filter(|antenna| {
                antenna.keywords_groups.iter().any(|group| {
                    group.iter().all(|keyword| matches_word_boundary(&post_content1, keyword))
                })
            })
            .map(|antenna| antenna.id.clone())
            .collect();
        assert_eq!(matching1, vec!["1"]);
        
        // Test post that should match antenna1's second group
        let post_content2 = "Learning about tokio async runtime".to_lowercase();
        let matching2: Vec<String> = antennas_guard.iter()
            .filter(|antenna| {
                antenna.keywords_groups.iter().any(|group| {
                    group.iter().all(|keyword| matches_word_boundary(&post_content2, keyword))
                })
            })
            .map(|antenna| antenna.id.clone())
            .collect();
        assert_eq!(matching2, vec!["1"]);
        
        // Test post that should match antenna2
        let post_content3 = "Hello world example".to_lowercase();
        let matching3: Vec<String> = antennas_guard.iter()
            .filter(|antenna| {
                antenna.keywords_groups.iter().any(|group| {
                    group.iter().all(|keyword| matches_word_boundary(&post_content3, keyword))
                })
            })
            .map(|antenna| antenna.id.clone())
            .collect();
        assert_eq!(matching3, vec!["2"]);
        
        // Test post that should match no antennas
        let post_content4 = "Nothing interesting here".to_lowercase();
        let matching4: Vec<String> = antennas_guard.iter()
            .filter(|antenna| {
                antenna.keywords_groups.iter().any(|group| {
                    group.iter().all(|keyword| matches_word_boundary(&post_content4, keyword))
                })
            })
            .map(|antenna| antenna.id.clone())
            .collect();
        assert_eq!(matching4, vec![] as Vec<String>);
    }
    
    #[test]
    fn test_post_deserialization() {
        // Test basic post JSON
        let json_str = r#"{"id":"123","content":"Hello world","uri":"https://example.com/posts/123","account":{"username":"testuser"}}"#;
        let post: Post = serde_json::from_str(json_str).unwrap();
        assert_eq!(post.id, "123");
        assert_eq!(post.content, "Hello world");
        assert_eq!(post.uri, "https://example.com/posts/123");
        assert_eq!(post.account.username, "testuser");
        
        // Test with missing optional fields
        let json_str = r#"{"uri":"https://example.com/posts/123","account":{"username":"testuser"}}"#;
        let post: Post = serde_json::from_str(json_str).unwrap();
        assert_eq!(post.id, "");
        assert_eq!(post.content, "");
        assert_eq!(post.uri, "https://example.com/posts/123");
        assert_eq!(post.account.username, "testuser");
        
        // Test with missing required fields
        let json_str = r#"{"id":"123","content":"Hello world"}"#;
        let result = serde_json::from_str::<Post>(json_str);
        assert!(result.is_err());
    }
    
    #[test]
    fn test_antenna_deserialization() {
        // Test basic antenna JSON
        let json_str = r#"{"id":"123","keywords":[["rust","programming"],["tokio"]]}"#;
        let antenna: Antenna = serde_json::from_str(json_str).unwrap();
        assert_eq!(antenna.id, "123");
        assert_eq!(antenna.keywords_groups.len(), 2);
        assert_eq!(antenna.keywords_groups[0], vec!["rust", "programming"]);
        assert_eq!(antenna.keywords_groups[1], vec!["tokio"]);
        
        // Test with empty keywords
        let json_str = r#"{"id":"123","keywords":[]}"#;
        let antenna: Antenna = serde_json::from_str(json_str).unwrap();
        assert_eq!(antenna.id, "123");
        assert_eq!(antenna.keywords_groups.len(), 0);
    }
}
