#[tokio::test]
async fn file_service_keeps_media_headers_for_unicode_paths_and_ascii_extensions() {
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tower_http::services::ServeDir;

    let directory = std::env::temp_dir().join(format!("puffinbox-mime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(directory.clone());
    for (extension, expected) in [
        ("FLAC", "audio/flac"),
        ("Mp4", "video/mp4"),
        ("VTT", "text/vtt"),
        ("SRT", "application/x-subrip"),
        ("SVG", "image/svg+xml"),
        ("JpG", "image/jpeg"),
        ("EPUB", "application/epub+zip"),
        ("PDF", "application/pdf"),
    ] {
        let filename = format!("café.{extension}");
        std::fs::write(directory.join(&filename), b"synthetic header fixture").unwrap();
        let encoded: String = filename
            .bytes()
            .map(|value| {
                if value.is_ascii_alphanumeric() || value == b'.' {
                    char::from(value).to_string()
                } else {
                    format!("%{value:02X}")
                }
            })
            .collect();
        let response = ServeDir::new(&directory)
            .oneshot(
                Request::builder()
                    .uri(format!("/{encoded}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "file {filename}");
        assert_eq!(
            response.headers()["content-type"],
            expected,
            "file {filename}"
        );
    }
}
