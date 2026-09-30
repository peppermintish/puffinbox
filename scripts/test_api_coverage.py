#!/usr/bin/env python3
"""Check route discovery across production Rust modules without network access."""

from __future__ import annotations

from api_coverage import ROUTE_START, source_routes, strip_test_only_items


def main() -> int:
    sample = '''
pub fn routes() {
    #[cfg(test)]
    mod fixture {
        fn router() {
            Router::new().route("/test-only", get(handler));
        }
    }
    Router::new().route("/production", get(handler).post(handler));
}
'''
    production = strip_test_only_items(sample)
    paths = [match.group(2) for match in ROUTE_START.finditer(production)]
    assert paths == ["/production"], f"test-only route entered coverage report: {paths}"

    raw_string_sample = r'''
#[cfg(test)]
mod fixture {
    const JSON: &[u8] = br#"[{"name":"{not a Rust block}"}]"#;
    fn router() { Router::new().route("/test-only-raw", get(handler)); }
}
Router::new().route("/production-raw", get(handler));
'''
    raw_production = strip_test_only_items(raw_string_sample)
    raw_paths = [match.group(2) for match in ROUTE_START.finditer(raw_production)]
    assert raw_paths == ["/production-raw"], (
        f"raw string contents confused test-only route removal: {raw_paths}"
    )

    declarations = source_routes()
    assert any(row["source"] == "src/api.rs" for row in declarations)
    assert any(str(row["source"]).startswith("src/media_features/") for row in declarations)
    assert all(str(row["source"]).startswith("src/") for row in declarations)
    print(f"API source discovery tests passed ({len(declarations)} Axum route declarations).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
