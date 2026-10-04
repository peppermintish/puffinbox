#[cfg(feature = "rev-mappings")]
#[path = "src/ascii_case.rs"]
mod ascii_case;
#[cfg(feature = "rev-mappings")]
use ascii_case::AsciiCase;

use std::env;
use std::fs::File;
#[cfg(feature = "rev-mappings")]
use std::io::prelude::*;
use std::io::BufWriter;
use std::path::Path;

#[cfg(feature = "rev-mappings")]
use std::collections::BTreeMap;

#[cfg(feature = "rev-mappings")]
use mime_types::MIME_TYPES;

#[cfg(feature = "rev-mappings")]
#[path = "src/mime_types.rs"]
mod mime_types;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("mime_types_generated.rs");
    #[allow(unused_mut, unused_variables)]
    let mut outfile = BufWriter::new(File::create(&dest_path).unwrap());

    println!(
        "cargo:rustc-env=MIME_TYPES_GENERATED_PATH={}",
        dest_path.display()
    );

    #[cfg(feature = "rev-mappings")]
    build_rev_map(&mut outfile);
}

#[cfg(feature = "rev-mappings")]
fn build_rev_map<W: Write>(out: &mut W) {
    use std::fmt::Write as _;

    macro_rules! ascii_const {
        ($s:expr) => ({ format_args!("AsciiCase::new({:?})", $s) })
    }

    let dyn_map = get_rev_mappings();

    write!(out, "static REV_MAPPINGS: &'static [(AsciiCase<'static>, TopLevelExts)] = &[").unwrap();

    let mut exts = Vec::new();

    for (top, subs) in dyn_map {
        let top_start = exts.len();

        let mut sub_map = String::new();

        for (sub, sub_exts) in subs {
            let sub_start = exts.len();
            exts.extend(sub_exts);
            let sub_end = exts.len();

            write!(
                sub_map,
                "({}, ({}, {})),",
                ascii_const!(sub), sub_start, sub_end
            ).unwrap();
        }

        let top_end = exts.len();

        write!(
            out,
            "({}, TopLevelExts {{ start: {}, end: {}, subs: &[{}] }}),",
            ascii_const!(top), top_start, top_end, sub_map
        ).unwrap();
    }

    writeln!(out, "];").unwrap();

    writeln!(out, "const EXTS: &'static [&'static str] = &{:?};", exts).unwrap();
}

#[cfg(feature = "rev-mappings")]
fn get_rev_mappings(
) -> BTreeMap<AsciiCase<'static>, BTreeMap<AsciiCase<'static>, Vec<&'static str>>> {
    // First, collect all the mime type -> ext mappings)
    let mut dyn_map = BTreeMap::new();
    for &(key, types) in MIME_TYPES {
        for val in types {
            let (top, sub) = split_mime(val);
            dyn_map
                .entry(AsciiCase::new(top))
                .or_insert_with(BTreeMap::new)
                .entry(AsciiCase::new(sub))
                .or_insert_with(Vec::new)
                .push(key);
        }
    }
    dyn_map
}

#[cfg(feature = "rev-mappings")]
fn split_mime(mime: &str) -> (&str, &str) {
    let split_idx = mime.find('/').unwrap();
    (&mime[..split_idx], &mime[split_idx + 1..])
}
