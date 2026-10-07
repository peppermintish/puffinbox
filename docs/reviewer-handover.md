# Reviewer notes

Puffinbox is partial and unreleased. Start with [compatibility](compatibility.md), [current acceptance and reproduction](acceptance.md), and [licensing](licensing.md).

The compatible-license correction removes project C/assembly, obsolete GNU/OpenSSL experiments and license-workaround forks. HTTP and PostgreSQL TLS use rustls. Project Rust forbids unsafe code across production, examples and tests. Safe dependency APIs preserve descriptor-based filesystem access and media resource/syscall confinement; the server re-enters a single-threaded internal worker before decoder execution.

The one retained source patch fixes SQLx PostgreSQL SCRAM behavior for valid Unicode roles and PostgreSQL password normalization/raw fallback. Upstream archive and file hashes, the two changed source hashes and full notices are checked in. Real database regressions cover positive/negative authentication and tampered proofs.

Recorded source, dependency, browser, container and HTTPS checks passed at their named checkpoints. Both official clients completed the tagged four-track FLAC album and retained independently verified play counts. Official Web displayed decoded advancing HLS video and the current two-session Web run created, joined, paused and resumed a SyncPlay group. A graceful server restart drained both media children and resumed the persisted HLS position. A default-renderer Desktop profile reported normal stops and resumed its stored movie position across the local-metadata container replacement, with advancing video, ended database rows and FFmpeg cleanup verified. Native Desktop synchronization still has the documented unit-mismatch replay failure; general codecs and audible continuity remain open. Exact artifact IDs and limits belong in acceptance.md.

The core distribution license gate is satisfied under the corrected policy; the other release gates remain open. The local-metadata image passed 35 container checks and 69 reference projections, including separately qualified series sorting/counts. Scans now queue local metadata automatically, with database checks for rescans and sidecar removal. Both official clients displayed Next Up and completed watched/favorite actions and undo on the preceding action image, with original history restored. Both Favorites views passed after the sorting correction. Wider numbering, SyncPlay, broader features/clients, Internet security and the requested storage/stream scale still need work. No release tag or archive has been created. The action aliases passed [both cloud jobs at `53ac450`](https://github.com/peppermintish/puffinbox/actions/runs/37366181589). The [local-metadata cloud run](https://github.com/peppermintish/puffinbox/actions/runs/37370581543) could not acquire hosted runners; both jobs were cancelled before running test steps.

The initial SyncPlay group routes now use authenticated token sessions and
bounded membership with current policy checks. The full 31-case database run
passed after recording/offline policy snapshot omissions were corrected. Final
source checks and the group regression passed after matching omitted/null join
IDs and Unicode name lengths. An unused asset reader and obsolete lint
suppressions were removed; confinement tests exercise the production reader.
Shared queues and synchronization commands are now implemented with per-entry
IDs, bounded queues, current participant media checks and separate queue/command
revisions. The final source passed 249 standard cases and all 32 database cases,
including UTC clock and multi-socket/last-socket cleanup. The named official
Web two-session run now covers group creation, joining, pause/resume and
graceful restart/resume; packaged and native-client synchronization remain
unvalidated. See [the contract](jellyfin12-syncplay.md) and the current
acceptance record.
The group checkpoint `4560a12` passed both cloud jobs before the queue changes.

The [documentation checkpoint at `5463c0c`](https://github.com/peppermintish/puffinbox/actions/runs/37372909200)
passed isolated media acceptance for the unchanged local-metadata implementation.
Its source job again could not acquire a hosted runner and ran no test steps.

The PDF restoration retains 194 unchanged upstream files and nine compatible notices, excluding the GPL Liberation fonts and unused scripting assets. Twelve Chromium checks cover CMaps/text, Foxit glyphs, JPEG2000/JBIG2 pixels, decoder fallbacks and real worker termination. The rebuilt image passed all 35 container checks and four authenticated reader cases. The blank unembedded Japanese page exposed in cloud CI was resolved by installing a Japanese test font; both jobs passed at `601c817`, preserving the pixel assertion. The later offline reopen race was reproduced and fixed, with both jobs passing at `1f971e4`.

Keep credentials, client storage/logs and media under ignored local test state. Preserve existing personal libraries and older synthetic stacks. The long retired experiment narrative remains available in Git history.
