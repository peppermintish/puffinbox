# PostgreSQL SCRAM role fix

This is the registry `sqlx-postgres 0.8.6` package with a narrow SCRAM fix and an unsafe-code prohibition. The verified crate archive and every upstream file hash are recorded in `provenance.json`. MIT and Apache-2.0 license texts and upstream copyright notices are retained.

`src/connection/sasl.rs` sends an empty SCRAM username. PostgreSQL uses the unchanged startup role and ignores the SCRAM username, as documented in [SASL authentication](https://www.postgresql.org/docs/current/sasl-authentication.html). The upstream username SASLprep call can panic on valid Unicode roles. The real PostgreSQL regression retains Unicode and punctuation roles, wrong passwords, missing roles and altered server proofs.

Password SASLprep now follows PostgreSQL: normalize valid input and retain original bytes when preparation fails. The real regression reproduced rejection of a valid normalized password before this correction and covers both normalized and raw fallback credentials. Proofs and server verification retain the upstream implementation; the original stringprep dependency remains enabled. This is a behavior fix, not a license exclusion. The normal permissive Unicode licenses are accepted by the project policy. Remove this patch after adopting an upstream release that supports these role cases and passing the regression.
