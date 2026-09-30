//! Test helper for certificate validation in the static Linux build.
use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = std::env::var("PUFFINBOX_TLS_TEST_URL").expect("test URL is required");
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("TLS client should initialize");
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => {}
        _ => std::process::exit(1),
    }
}
