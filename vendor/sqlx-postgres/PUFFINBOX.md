# PostgreSQL SCRAM username

This directory retains SQLx PostgreSQL 0.8.6 from the crates.io archive with SHA-256
`db58fcd5a53cf07c184b154801ff91347e4c30d17a3562a635ff028ad5deda46`.
The archive records upstream revision `bab1b022bd56a64f9a08b46b36b97c5cff19d77e`
and repository path `sqlx-postgres`. The upstream MIT and Apache-2.0 notices are retained.

Two upstream files are changed: `Cargo.toml` drops `stringprep`, and
`src/connection/sasl.rs` sends an empty SCRAM username. PostgreSQL selects the
unchanged role name from the connection startup message and ignores this SCRAM
field; see [the PostgreSQL protocol documentation](https://www.postgresql.org/docs/18/sasl-authentication.html).
Nonce generation, password processing, proofs, server-signature verification and
TLS behavior are unchanged. This is a PostgreSQL driver change, not a general
SASLprep replacement or a new password normalization implementation.

`Cargo.toml.orig` is the byte-identical historical manifest and is not active.
All other upstream files are retained byte for byte. Registry bookkeeping
`.cargo_vcs_info.json` is omitted; the revision is recorded here and in the review
inventory. `PUFFINBOX.md` is an original MIT OR Apache-2.0 note. The exact retained
116-file inventory is guarded in `vendor/dependency-replacements.json`.

The original root integration test covers ASCII, Unicode and punctuation role
names, incorrect passwords, a missing role, and modified server signatures using
an isolated local PostgreSQL database. Its wire proxy checks the startup role and
empty SCRAM field independently. The proxy disables TLS only for that loopback
test fixture; it does not change production connection options. Broader Unicode
password normalization and SCRAM channel-binding coverage remain unvalidated.
