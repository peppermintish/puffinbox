# Reviewer notes

Puffinbox is partial and unreleased. Start with [compatibility](compatibility.md), [current acceptance and reproduction](acceptance.md), and [licensing](licensing.md).

The compatible-license correction removes project C/assembly, obsolete GNU/OpenSSL experiments and license-workaround forks. HTTP and PostgreSQL TLS use rustls. Project Rust forbids unsafe code across production, examples and tests. Safe dependency APIs preserve descriptor-based filesystem access and media resource/syscall confinement; the server re-enters a single-threaded internal worker before decoder execution.

The one retained source patch fixes SQLx PostgreSQL SCRAM behavior for valid Unicode roles and PostgreSQL password normalization/raw fallback. Upstream archive and file hashes, the two changed source hashes and full notices are checked in. Real database regressions cover positive/negative authentication and tampered proofs.

Current local source, dependency, browser, container and HTTPS checks passed. Both official clients completed the tagged four-track FLAC album and retained independently verified play counts. Official web displayed decoded advancing HLS video. Desktop video captures remain black, so visual Desktop video acceptance is open. Exact artifact IDs and limits belong in acceptance.md.

The core distribution license gate is satisfied under the corrected policy; the other release gates remain open. Next Up and SyncPlay, broader features/clients, Internet security and the requested storage/stream scale still need work. No release tag or archive has been created. The Rust and license correction passed [cloud CI](https://github.com/peppermintish/puffinbox/actions/runs/37229872635) at `d7417dd`.

The subsequent PDF restoration retains 194 unchanged upstream files and nine compatible notices, excluding the GPL Liberation fonts and unused scripting assets. Twelve local Chromium checks cover CMaps/text, Foxit glyphs, JPEG2000/JBIG2 pixels, decoder fallbacks and real worker termination. The rebuilt image passed all 35 container checks and four authenticated reader cases. Cloud validation of this increment remains pending.

Keep credentials, client storage/logs and media under ignored local test state. Preserve existing personal libraries and older synthetic stacks. The long retired experiment narrative remains available in Git history.
