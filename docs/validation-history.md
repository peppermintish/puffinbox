# Earlier validation records

These records predate the current dependency and shutdown changes. They are retained as context, not current-source acceptance. The old checkpoint commits belong to the previous local history and are not present in the newly configured repository's initial history.

| Recorded checkpoint or image | Reported result and scope |
| --- | --- |
| Source checkpoint 813a337, 2026-09-30 | 197 Rust tests passed; 18 PostgreSQL integration cases plus one recorder unit test; 23 Python tests and browser/reader checks. Direct-source acceptance passed 18 checks. Container packaging stopped at the old Cargo license failure. |
| Operator image sha256:b9b4008da83a477322bf19cb37b2b920e6eb21cdd34561e516c513223fe6fffb, 2026-10-01 | 20 container checks passed before full-duration HLS. Official web decoded the movie but offset-stream resume had a timeline mismatch. Native seeking and stop reporting passed within the resumed clip. |
| Operator image sha256:29e002e281f1e7d7e72e83e90b9226427e7c17ed77f2aa5aa19a58c865d77234 | Earlier isolated container acceptance passed 18 checks. |
| Operator image 664a5a3, 2026-09-29 | Installed player completed selected synthetic online/offline fixtures and visibly rendered English cues on a 180-second subtitle fixture, including one live track switch. Its HLS-window progress reset did not establish seeking. Photo display remained unexplained. |
| Source browser review, 2026-09-29 | Synthetic MP4 completion, creation of a one-track music queue, verified offline photo download and display. Full music completion was not checked. |
| Earlier PostgreSQL review, 2026-09-29 | A DVR connection timeout and metadata/plugin SIGSEGVs were unresolved. Six later metadata runs and a debugger run passed without reproducing the metadata failure. No cause was established. |
| Scanner benchmark | 2,048 synthetic files at about 4,567 rows per second. This is not a petabyte, billion-file, or thousand-stream result. |

See [acceptance.md](acceptance.md) for the current results and [compatibility.md](compatibility.md) for remaining limits.
