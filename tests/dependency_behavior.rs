use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

#[test]
fn accepts_a_password_hash_from_the_previous_argon2_version() {
    // RustCrypto argon2 0.5.3 known-answer fixture for "password" / "somesalt".
    let encoded =
        "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc";
    let parsed = PasswordHash::new(encoded).unwrap();
    assert!(
        Argon2::default()
            .verify_password(b"password", &parsed)
            .is_ok()
    );
    assert!(
        Argon2::default()
            .verify_password(b"wrong-password", &parsed)
            .is_err()
    );
}

#[test]
fn new_password_hashes_have_random_salts_and_verify() {
    let algorithm = Argon2::default();
    let first = algorithm.hash_password(b"a-long-test-password").unwrap();
    let second = algorithm.hash_password(b"a-long-test-password").unwrap();
    assert_ne!(first.to_string(), second.to_string());
    assert!(
        algorithm
            .verify_password(b"a-long-test-password", &first)
            .is_ok()
    );
    assert!(
        algorithm
            .verify_password(b"a-long-test-password", &second)
            .is_ok()
    );
}

#[test]
fn mac_verification_retains_the_rfc_4231_result_and_rejects_bad_tags() {
    // RFC 4231, SHA-256 test case 1.
    let mut algorithm = Hmac::<Sha256>::new_from_slice(&[0x0b; 20]).unwrap();
    algorithm.update(b"Hi There");
    let tag = [
        0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b, 0xf1,
        0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c, 0x2e, 0x32,
        0xcf, 0xf7,
    ];
    assert!(algorithm.clone().verify_slice(&tag).is_ok());
    assert!(algorithm.clone().verify_slice(&tag[..31]).is_err());
    assert!(algorithm.clone().verify_truncated_left(&tag[..16]).is_ok());
    assert!(algorithm.clone().verify_truncated_right(&tag[16..]).is_ok());
    for position in [0, 15, 31] {
        let mut wrong_tag = tag;
        wrong_tag[position] ^= 1;
        assert!(algorithm.clone().verify_slice(&wrong_tag).is_err());
    }
}

#[test]
fn url_backend_rejects_unicode_hosts_and_preserves_unicode_paths() {
    assert!(url::Url::parse("https://例.example/feed").is_err());
    let url = url::Url::parse("https://media.example/café?q=字幕").unwrap();
    assert_eq!(url.host_str(), Some("media.example"));
    assert_eq!(url.path(), "/caf%C3%A9");
    assert_eq!(url.query(), Some("q=%E5%AD%97%E5%B9%95"));
}
