((root, factory) => {
  const heartbeat = factory();
  if (typeof module === 'object' && module.exports) module.exports = heartbeat;
  else root.PuffinboxPlaybackHeartbeat = heartbeat;
})(typeof globalThis === 'undefined' ? this : globalThis, () => {
  'use strict';

  function start(session, isCurrent, callback, timers = globalThis) {
    if (!session || session.stopping || session.heartbeatTimer != null || !isCurrent()) return false;
    session.heartbeatTimer = timers.setInterval(() => {
      if (session.stopping || !isCurrent()) {
        stop(session, timers);
        return;
      }
      callback();
    }, 15_000);
    return true;
  }

  function stop(session, timers = globalThis) {
    if (!session || session.heartbeatTimer == null) return false;
    timers.clearInterval(session.heartbeatTimer);
    session.heartbeatTimer = null;
    return true;
  }

  function startNativeStartupWatchdog(session, isCurrent, callback, timers = globalThis, delayMs = 20_000) {
    if (!session || session.stopping || session.nativeActivitySeen || session.nativeStartupTimer != null || !isCurrent()) return false;
    session.nativeStartupTimer = timers.setTimeout(() => {
      session.nativeStartupTimer = null;
      if (!session.stopping && !session.started && !session.nativeActivitySeen && isCurrent()) callback();
    }, delayMs);
    return true;
  }

  function stopNativeStartupWatchdog(session, timers = globalThis) {
    if (!session || session.nativeStartupTimer == null) return false;
    timers.clearTimeout(session.nativeStartupTimer);
    session.nativeStartupTimer = null;
    return true;
  }

  function shouldAutoplayAfterTrackChange(session) {
    if (!session) return true;
    if (session.isNative) return session.isPaused !== true;
    if (session.media) return session.media.paused !== true;
    return true;
  }

  function nextHeartbeatAction(session) {
    if (!session || session.stopping) return 'none';
    if (session.started) return 'progress';
    if (session.activitySeen) return 'start';
    if (session.usesHls) return 'keepalive';
    return 'none';
  }

  function singleFlight(session, key, operation) {
    if (session?.[key]) return session[key];
    let tracked;
    tracked = Promise.resolve().then(operation).finally(() => {
      if (session?.[key] === tracked) session[key] = null;
    });
    if (session) session[key] = tracked;
    return tracked;
  }

  function recordNativePosition(session, positionMs, minimumAdvanceMs = 100) {
    const next = Math.max(0, Number(positionMs) || 0);
    const previous = Math.max(0, Number(session?.nativePositionMs) || 0);
    const hadPrevious = session?.nativePositionInitialized === true;
    if (session) {
      session.nativePositionMs = next;
      session.nativePositionInitialized = true;
    }
    return hadPrevious && session?.isPaused !== true && next > previous + minimumAdvanceMs;
  }

  return {
    start,
    stop,
    startNativeStartupWatchdog,
    stopNativeStartupWatchdog,
    shouldAutoplayAfterTrackChange,
    nextHeartbeatAction,
    singleFlight,
    recordNativePosition,
  };
});
