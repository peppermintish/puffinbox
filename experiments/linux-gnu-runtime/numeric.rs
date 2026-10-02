// SPDX-License-Identifier: MIT OR Apache-2.0
use std::hint::black_box;

unsafe extern "C" {
    fn round(value: f64) -> f64;
    fn floor(value: f64) -> f64;
    fn ceil(value: f64) -> f64;
    fn trunc(value: f64) -> f64;
    fn rint(value: f64) -> f64;
    fn floorf(value: f32) -> f32;
    fn ceilf(value: f32) -> f32;
    fn truncf(value: f32) -> f32;
    fn rintf(value: f32) -> f32;
}

fn bits64(value: f64) -> String {
    if value.is_nan() {
        "nan".into()
    } else {
        format!("{:016x}", value.to_bits())
    }
}
fn bits32(value: f32) -> String {
    if value.is_nan() {
        "nan".into()
    } else {
        format!("{:08x}", value.to_bits())
    }
}

fn floats(value: f64, small: f32) {
    let value = black_box(value);
    let small = black_box(small);
    // All inputs are ordinary IEEE floating values. These C functions have
    // the public libm ABI; the baseline resolves them from compiler-builtins.
    unsafe {
        println!(
            "f {:016x} {} {} {} {} {} {:08x} {} {} {} {}",
            value.to_bits(),
            bits64(round(value)),
            bits64(floor(value)),
            bits64(ceil(value)),
            bits64(trunc(value)),
            bits64(rint(value)),
            small.to_bits(),
            bits32(floorf(small)),
            bits32(ceilf(small)),
            bits32(truncf(small)),
            bits32(rintf(small))
        );
    }
}

fn main() {
    let seed = std::env::args()
        .nth(1)
        .expect("runtime seed")
        .parse::<u128>()
        .unwrap();
    let mut state = seed;
    for value in [
        -0.0,
        -2.5,
        -1.5,
        -0.5,
        0.0,
        0.5,
        1.5,
        2.5,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ] {
        floats(value, value as f32);
    }
    for _ in 0..4096 {
        state ^= state << 23;
        state ^= state >> 17;
        state ^= state << 26;
        let numerator = black_box(state);
        let divisor = black_box((state.rotate_left(53) ^ seed) | 1);
        let signed = black_box(numerator as i128);
        println!(
            "i {numerator} {divisor} {} {}",
            numerator % divisor,
            bits64(signed as f64)
        );
        floats(
            f64::from_bits(state as u64),
            f32::from_bits((state >> 64) as u32),
        );
    }
    let value = "　ÅÉİΣ　";
    assert_eq!(value.trim(), "ÅÉİΣ");
    assert!(value.trim().chars().all(char::is_alphabetic));
    assert_eq!(
        value
            .trim()
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>(),
        "åéi\u{307}σ"
    );
    assert!("١٢٣".chars().all(char::is_numeric));
    println!("Unicode trimming, alphabetic, numeric and case conversion passed.");
}
