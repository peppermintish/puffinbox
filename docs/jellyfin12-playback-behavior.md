# Jellyfin 12 music playback state

The local Jellyfin 12.0.0 reference server marks music as played when playback starts. It counts each new play, including a replay of an already played track, and keeps the saved resume position at zero. Music completion does not depend on a final position near the end of the track.

This matters for Jellyfin Desktop. One Puffinbox album run decoded all three tracks through audio EOF but reported only 17.09 seconds for a 20-second track. Puffinbox's previous 95-percent completion rule left that track unplayed. The same start and stop reports on the reference server leave it played, with one new play and no saved resume position.

## Reference conditions

The test used the [official Jellyfin container](https://jellyfin.org/docs/general/installation/container/), pinned to image digest `sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`. Its public system-info response identifies version `12.0.0`. The container ran as an ordinary user on a separate internal Docker network, with read-only synthetic media. HTTP requests stayed inside that container. The reference runtime is external test infrastructure and is excluded from Puffinbox's distribution.

The original AIFF bytes match the native and web album fixtures: SHA-256 `8c3d76ac7cb18a57f63a1557c7afec49695174aaa8e398fd799c1e62abf0fdbd`, duration 20 seconds. Additional original FLAC fixtures have durations of 299, 300 and 600 seconds. Public API reads confirmed every duration before its playback-state check. No Jellyfin implementation code was inspected or copied.

The reference's resume settings were `MinResumePct=5`, `MaxResumePct=90` and `MinResumeDurationSeconds=300`. These values did not make the long music tracks resumable. This observation does not establish video or audiobook behavior under those settings.

## Observed state

| Event | Played | Saved position | Play count | Last played date |
| --- | --- | --- | --- | --- |
| New music playback starts | True | Zero | Increases by one | Updated |
| Progress | True | Zero | Unchanged | Unchanged |
| Stop at zero, a partial position, or EOF | True | Zero | Unchanged | Unchanged |
| Another complete playback starts | True | Zero | Increases by one again | Updated |
| Repeated stop for the same playback | True | Zero | Unchanged | Unchanged |

The 20-second comparison includes stops at 0, 1, 5, 10, 17.09, 18, 18.01, 19, 19.75 and 20.021332 seconds; both zero/full duplicate orders; two complete plays; `Failed=true`; `NextMediaType=Audio`; and empty playback IDs. The longer fixtures cover progress and stop at 0, 1, 5, 50, 85, 90, 95 and 100 percent. These are immediate public API reports, not decoder, timing or audible playback tests.

Explicit user-data writes reset `PlayCount` on the reference server. Puffinbox now accepts explicit counts and last-played dates through its [partial user-data update](jellyfin12-user-data.md). The earlier playback comparisons retained the baseline and assessed count increases; those records were made before explicit count support.

## Puffinbox scope

Puffinbox applies the observed music policy to `Audio` items in a `music` library. It retains the active session's reported position separately from saved resume state. A repeated start with the same authenticated playback ID does not add another count; a new playback does. Progress and the first stop restore the played state without incrementing the count or changing the last played date. Other item types and libraries retain their existing resume policy.

An ended duplicate cannot overwrite a later explicit user-data edit or a newer playback. The reference server did overwrite an explicit edit made after stop when a duplicate stop followed it. Puffinbox preserves its existing ownership and state-revision protections here; complete user-data compatibility is not claimed.

Raw reference evidence is retained under `.local/jellyfin12-reference-20261003`: `reference.json`, `metadata-probe.json`, `stop-comparison.json`, `long-audio-fixtures.json`, `long-audio-comparison.json` and `edit-during-music.json`. Container identity, image and fixture hashes are recorded separately from private account credentials. The earlier native failure and unchanged-image passing repeat remain under `.local/album-stop-race-native-20261003` and `.local/album-stop-race-native-repeat-20261003`.

The corrected core image `ce34d9aa` replayed all 18 short-track scenarios successfully. The unchanged official web and Qt 6 Desktop album Play controls each advanced through the three-track fixture, with separate reads confirming exactly one new play per track and zero saved resume. Web decoded full-timeline AAC HLS; Desktop decoder logs independently confirmed audio EOF for each original AIFF. Evidence is under `.local/music-play-state-web-20261003` and `.local/music-play-state-native-20261003`; the public API replay is `stop-comparison-after.json` in the reference evidence directory. These client checks do not validate audible quality or broad music formats.
