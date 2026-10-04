#![cfg(feature = "rev-mappings")]

mod upstream {
    include!("../src/mime_types.rs");
}

#[test]
fn reverse_mappings_retain_every_ascii_type_and_extension_in_table_order() {
    let mut expected = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for &(extension, types) in upstream::MIME_TYPES {
        for &mime in types {
            expected.entry(mime).or_default().push(extension);
        }
    }
    for (mime, extensions) in expected {
        assert_eq!(
            mime_guess::get_mime_extensions_str(mime),
            Some(extensions.as_slice())
        );
        assert_eq!(
            mime_guess::get_mime_extensions_str(&mime.to_ascii_uppercase()),
            Some(extensions.as_slice())
        );
        assert_eq!(
            mime_guess::get_mime_extensions_str(&format!("{mime}; charset=utf-8")),
            Some(extensions.as_slice())
        );
    }
}

#[test]
fn reverse_wildcards_preserve_the_full_sorted_table_projection() {
    let mut expected = std::collections::BTreeMap::<(String, String), Vec<&str>>::new();
    for &(extension, types) in upstream::MIME_TYPES {
        for &mime in types {
            let pair = mime.split_once('/').unwrap();
            expected
                .entry((pair.0.to_ascii_lowercase(), pair.1.to_ascii_lowercase()))
                .or_default()
                .push(extension);
        }
    }
    let all: Vec<_> = expected.values().flatten().copied().collect();
    assert_eq!(mime_guess::get_extensions("*", "*"), Some(all.as_slice()));
    let mut top = std::collections::BTreeMap::<String, Vec<&str>>::new();
    for ((name, _), extensions) in expected {
        top.entry(name).or_default().extend(extensions);
    }
    for (name, extensions) in top {
        assert_eq!(
            mime_guess::get_extensions(&name, "*"),
            Some(extensions.as_slice())
        );
        assert_eq!(
            mime_guess::get_extensions(&name.to_ascii_uppercase(), "*"),
            Some(extensions.as_slice())
        );
    }
}

#[test]
fn reverse_lookup_does_not_fold_unicode_into_ascii_mime_tokens() {
    for value in ["image/ſvg+xml", "text/Kml", "字幕/plain", ""] {
        assert_eq!(mime_guess::get_mime_extensions_str(value), None);
    }
}
