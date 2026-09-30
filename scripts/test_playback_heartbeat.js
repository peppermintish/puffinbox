'use strict';

const assert = require('node:assert/strict');
const heartbeat = require('../web/playback-heartbeat.js');

function fakeTimers() {
  const callbacks = new Map();
  let nextId = 1;
  return {
    callbacks,
    setInterval(callback, milliseconds) {
      const id = nextId++;
      callbacks.set(id, { callback, milliseconds, kind: 'interval' });
      return id;
    },
    clearInterval(id) { callbacks.delete(id); },
    setTimeout(callback, milliseconds) {
      const id = nextId++;
      callbacks.set(id, { callback: () => { callbacks.delete(id); callback(); }, milliseconds, kind: 'timeout' });
      return id;
    },
    clearTimeout(id) { callbacks.delete(id); },
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((yes) => { resolve = yes; });
  return { promise, resolve };
}

async function main() {
  const timers = fakeTimers();
  let heartbeatCount = 0;
  const active = { stopping: false, heartbeatTimer: null };
  assert.equal(heartbeat.start(active, () => true, () => { heartbeatCount += 1; }, timers), true);
  assert.equal([...timers.callbacks.values()][0].milliseconds, 15_000);
  [...timers.callbacks.values()][0].callback();
  assert.equal(heartbeatCount, 1);
  assert.equal(heartbeat.stop(active, timers), true);
  assert.equal(active.heartbeatTimer, null);

  const delayedStart = deferred();
  const closing = { stopping: false, heartbeatTimer: null };
  const settleStart = delayedStart.promise.then(() => heartbeat.start(closing, () => true, () => {}, timers));
  closing.stopping = true;
  heartbeat.stop(closing, timers);
  delayedStart.resolve();
  assert.equal(await settleStart, false, 'a delayed playback start cannot create a timer after stop');
  assert.equal(timers.callbacks.size, 0);

  assert.equal(heartbeat.shouldAutoplayAfterTrackChange({ isNative: false, media: { paused: true } }), false,
    'browser track changes preserve a paused video element');
  assert.equal(heartbeat.shouldAutoplayAfterTrackChange({ isNative: false, media: { paused: false } }), true,
    'browser track changes continue active playback');
  assert.equal(heartbeat.shouldAutoplayAfterTrackChange({ isNative: true, isPaused: true }), false,
    'native track changes preserve a paused player');
  assert.equal(heartbeat.shouldAutoplayAfterTrackChange({ isNative: true, isPaused: false }), true,
    'native track changes continue active playback');
  assert.equal(heartbeat.nextHeartbeatAction({ started: false, activitySeen: false, usesHls: true }), 'keepalive',
    'a paused prepared HLS stream renews its lease without reporting playback');
  assert.equal(heartbeat.nextHeartbeatAction({ started: false, activitySeen: false, usesHls: false }), 'none',
    'a direct stream does not report activity before actual playback evidence');
  assert.equal(heartbeat.nextHeartbeatAction({ started: false, activitySeen: true, usesHls: false }), 'start',
    'actual playback evidence retries a failed start request on the next heartbeat');
  assert.equal(heartbeat.nextHeartbeatAction({ started: true, activitySeen: true, usesHls: true }), 'progress',
    'a started session uses progress heartbeats');

  const pausedHls = { stopping: false, started: false, heartbeatTimer: null, isPaused: true, media: { paused: true } };
  let preparedLeaseRenewals = 0;
  let playbackProgressRenewals = 0;
  heartbeat.start(pausedHls, () => true, () => {
    if (pausedHls.started) playbackProgressRenewals += 1;
    else preparedLeaseRenewals += 1;
  }, timers);
  const pausedHlsTimer = timers.callbacks.get(pausedHls.heartbeatTimer);
  for (let tick = 0; tick < 8; tick += 1) pausedHlsTimer.callback();
  assert.equal(preparedLeaseRenewals, 8,
    'a deliberately paused HLS session continues its prepared lease past the 60-second cleanup window');
  assert.equal(pausedHls.started, false, 'prepared-lease heartbeats do not invent a Playing event');
  pausedHls.started = true;
  pausedHlsTimer.callback();
  assert.equal(playbackProgressRenewals, 1, 'actual playback switches the same timer to session progress');
  heartbeat.stop(pausedHls, timers);
  assert.equal(heartbeat.shouldAutoplayAfterTrackChange(pausedHls), false,
    'a long paused HLS track change remains paused');

  const resumedPausedNative = { nativePositionMs: 0, nativePositionInitialized: false, isPaused: true };
  assert.equal(heartbeat.recordNativePosition(resumedPausedNative, 45_000), false,
    'a nonzero first position on a paused resumed native load is not playback evidence');
  assert.equal(heartbeat.recordNativePosition(resumedPausedNative, 47_000), false,
    'a deliberately paused player does not start a session from position changes');
  const movingNative = { nativePositionMs: 0, nativePositionInitialized: false, isPaused: false };
  assert.equal(heartbeat.recordNativePosition(movingNative, 45_000), false,
    'the first nonzero offset establishes a baseline instead of claiming playback');
  assert.equal(heartbeat.recordNativePosition(movingNative, 45_050), false,
    'small position jitter does not claim playback');
  assert.equal(heartbeat.recordNativePosition(movingNative, 45_200), true,
    'forward progress after a position baseline is evidence of active playback');

  const singleFlightState = {};
  let startAttempts = 0;
  const failedStart = heartbeat.singleFlight(singleFlightState, 'startPromise', async () => {
    startAttempts += 1;
    throw new Error('temporary failure');
  });
  assert.equal(heartbeat.singleFlight(singleFlightState, 'startPromise', async () => {
    startAttempts += 1;
  }), failedStart, 'concurrent start attempts share one request');
  await assert.rejects(failedStart, /temporary failure/);
  await heartbeat.singleFlight(singleFlightState, 'startPromise', async () => { startAttempts += 1; });
  assert.equal(startAttempts, 2, 'a failed start request can be retried after the in-flight attempt settles');

  let startupFailureCount = 0;
  const noActivity = { stopping: false, started: false, nativeActivitySeen: false, nativeStartupTimer: null };
  assert.equal(heartbeat.startNativeStartupWatchdog(noActivity, () => true,
    () => { startupFailureCount += 1; }, timers, 20_000), true);
  const startupWatchdog = timers.callbacks.get(noActivity.nativeStartupTimer);
  assert.equal(startupWatchdog.milliseconds, 20_000);
  startupWatchdog.callback();
  assert.equal(startupFailureCount, 1, 'an accepted request without a playing/progress event reaches a bounded failure');
  assert.equal(noActivity.nativeStartupTimer, null);

  const started = { stopping: false, started: false, nativeActivitySeen: false, nativeStartupTimer: null };
  heartbeat.startNativeStartupWatchdog(started, () => true, () => { startupFailureCount += 1; }, timers, 20_000);
  started.nativeActivitySeen = true;
  heartbeat.stopNativeStartupWatchdog(started, timers);
  assert.equal(timers.callbacks.size, 0, 'playback activity clears its startup watchdog');

  const replaced = { stopping: false, heartbeatTimer: null };
  let replacedIsCurrent = true;
  heartbeat.start(replaced, () => replacedIsCurrent, () => {}, timers);
  const id = replaced.heartbeatTimer;
  replacedIsCurrent = false;
  timers.callbacks.get(id).callback();
  assert.equal(replaced.heartbeatTimer, null, 'the timer clears itself when the session loses ownership');
  assert.equal(timers.callbacks.size, 0);
  process.stdout.write('Playback heartbeat lifecycle tests passed.\n');
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
