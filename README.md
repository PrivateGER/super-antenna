# Super Antenna Documentation
Super Antenna is a Rust application that interfaces with a firehose to scan posts and import them into the Sharkey instance.

# Overview
The application works by:
- Fetching the global antenna list from the instance
- Connecting to the firehose API to monitor new posts
- Checking each post against the configured keyword groups
- Importing matching posts to the instance using AP with the ap/show endpoint


## Required Token Scopes

- View Account info - read:account
- Read global antenna - read:admin:antennas

## Usage
The application requires the following environment variables:
| Variable | Description |
|----------|-------------|
| BASE_URL | The base URL of the Sharkey instance (e.g., https://your-instance.com) |
| STREAMING_URL | The full URL for the streaming API (e.g., https://fedi.buzz/api/v1/streaming/public) |
| API_TOKEN | Your API token with the required scopes |
