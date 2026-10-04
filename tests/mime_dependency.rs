// The upstream MIT table is an independent oracle for the patched public lookup.
mod upstream {
    include!("../vendor/mime_guess/src/mime_types.rs");
}

#[test]
fn every_upstream_extension_retains_its_ordered_types_in_ascii_case_variants() {
    for &(extension, expected) in upstream::MIME_TYPES {
        assert!(extension.is_ascii());
        assert!(expected.iter().all(|mime| mime.is_ascii()));
        let mixed: String = extension
            .chars()
            .enumerate()
            .map(|(index, value)| {
                if index % 2 == 0 {
                    value.to_ascii_uppercase()
                } else {
                    value
                }
            })
            .collect();
        for candidate in [extension.to_owned(), extension.to_ascii_uppercase(), mixed] {
            let actual: Vec<_> = mime_guess::from_ext(&candidate).iter_raw().collect();
            assert_eq!(actual, expected, "extension {candidate}");
            let path = format!("映画/café.字幕.{candidate}");
            let actual: Vec<_> = mime_guess::from_path(path).iter_raw().collect();
            // Path::extension inspects only the last suffix, including for compound entries.
            let suffix = candidate.rsplit('.').next().unwrap();
            let path_expected = upstream::MIME_TYPES
                .iter()
                .find(|(extension, _)| extension.eq_ignore_ascii_case(suffix))
                .map(|(_, types)| *types)
                .unwrap_or(&[]);
            assert_eq!(
                actual, path_expected,
                "Unicode filename, extension {candidate}"
            );
        }
    }
}

#[test]
fn non_ascii_extensions_are_unknown_and_ascii_lookalikes_remain_recognized() {
    assert_eq!(
        mime_guess::from_ext("SVG").first_raw(),
        Some("image/svg+xml")
    );
    assert_eq!(
        mime_guess::from_ext("KML").first_raw(),
        Some("application/vnd.google-earth.kml+xml")
    );
    for extension in ["ſvg", "Kml", "字幕", "é", "", "not-a-media-extension"] {
        let guess = mime_guess::from_ext(extension);
        assert!(guess.is_empty(), "extension {extension}");
        assert_eq!(
            guess.first_or_octet_stream().to_string(),
            "application/octet-stream"
        );
    }
}

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
        ("ſvg", "application/octet-stream"),
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
