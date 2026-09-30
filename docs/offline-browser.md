# Browser offline copies

Puffinbox can prepare an authorized server package, save it to the current browser, and open the saved media while the server is unreachable. The Offline screen shows server packages alongside copies saved in the browser. Incomplete transfers can be paused or resumed; a copy becomes available to open only after each chunk and the complete file pass SHA-256 verification.

## Where copies live

Media chunks and package records live in IndexedDB for the Puffinbox site origin in that browser. The service worker caches the web screen files for the same origin and serves verified media bytes from IndexedDB. This storage is specific to this browser and site. It does not create a device-wide file or make media available to another browser, profile, or device. Browser privacy settings, storage pressure, or clearing site data can remove it.

Offline access requires a secure browser context with IndexedDB and service-worker support. Open Puffinbox while connected at least once so the browser can install the service worker and cache the screen files. The original media file is saved; Puffinbox does not transcode it for offline playback. Playback therefore depends on the browser’s codecs and media support.

Copies are scoped to the signed-in account. Signing out clears that account’s browser copies. If clearing storage fails, the saved sign-out marker keeps the offline library behind the sign-in screen. Switching accounts in another Puffinbox tab also signs the stale app view out and prevents its service worker from reading the previous account’s media.

Removing a prepared package from the server leaves a copy already saved in this browser in place. Remove that browser copy from the Offline screen when it is no longer needed.

## Browser checks

Run `node scripts/test_offline_browser.js` with Chrome or Chromium installed. Set `CHROME_BIN` to select an executable outside the standard install paths. The deterministic check drives the user-facing queue, an interrupted transfer and resume, injected bad chunk and quota errors, IndexedDB removal, cross-tab account isolation, service-worker byte ranges, integrity failures, offline WAV playback, and the sign-out fallback. CI and tagged-release source validation run this check.
