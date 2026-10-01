#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const adapter = require('../web/jmp-player.js');
const heartbeat = require('../web/playback-heartbeat.js');

function event() {
  const listeners = new Set();
  return {
    connect(listener) { listeners.add(listener); },
    disconnect(listener) { listeners.delete(listener); },
    emit(value) { for (const listener of [...listeners]) listener(value); },
    get size() { return listeners.size; },
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function makePlayer(onLoad = (args) => args.at(-1)(true)) {
  const calls = [];
  const player = {
    playing: event(), paused: event(), finished: event(), canceled: event(), error: event(),
    positionUpdate: event(), updateDuration: event(),
    setVideoRectangle(...args) { calls.push(['rectangle', ...args]); },
    load(...args) { calls.push(['load', ...args.slice(0, 5)]); onLoad(args); },
    stop() { calls.push(['stop']); },
    pause() { calls.push(['pause']); },
    play() { calls.push(['play']); },
    seekTo(value) { calls.push(['seek', value]); },
    getPosition(callback) { callback(12.345); },
  };
  return { player, calls };
}

function makeHost(player, apiPromise = Promise.resolve({ player })) {
  const nativeProfile = { DirectPlayProfiles: [{ Type: 'Video', Container: 'mkv' }], TranscodingProfiles: [{ Protocol: 'hls' }] };
  const stage = { getBoundingClientRect: () => ({ left: 8, top: 30, width: 800, height: 450 }) };
  return {
    location: { href: 'http://127.0.0.1:18096/web/', origin: 'http://127.0.0.1:18096' },
    jmpInfo: {},
    NativeShell: { AppHost: { getDeviceProfile: () => nativeProfile } },
    apiPromise,
    document: { querySelector: (selector) => selector === '#player-stage' ? stage : null },
    addEventListener() {},
    removeEventListener() {},
  };
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 30; attempt += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error('Timed out waiting for the native test event.');
}

async function main() {
  const resumed = {
    nativePositionMs: 39_000,
    nativeDurationMs: 214_000,
    positionOffsetTicks: 858_733_980,
    item: { RunTimeTicks: 3_000_000_000 },
  };
  assert.deepEqual(adapter.timeline(resumed), {
    positionMs: 124_873.398,
    durationMs: 300_000,
    seekMinimumMs: 85_873.398,
    seekMaximumMs: 299_873.398,
    canSeek: true,
  }, 'a resumed native stream displays the source timeline and bounds seeks to its available clip');
  assert.deepEqual(adapter.timeline({ nativePositionMs: 120_000, nativeDurationMs: 300_000 }), {
    positionMs: 120_000, durationMs: 300_000, seekMinimumMs: 0, seekMaximumMs: 300_000, canSeek: true,
  }, 'direct playback already uses source-relative positions');
  assert.equal(adapter.timeline({ ...resumed, isLiveTv: true }).canSeek, false);
  assert.equal(adapter.timeline({ ...resumed, nativeDurationMs: 0 }).canSeek, false);
  assert.equal(adapter.reachedEnd({ ...resumed, nativePositionMs: 0 }), false,
    'an early native finished event must not mark the whole item played');
  assert.equal(adapter.reachedEnd({ ...resumed, nativePositionMs: 214_000 }), true,
    'the end of a resumed clip reaches the end of the source item');
  assert.equal(adapter.reachedEnd({ ...resumed, nativePositionMs: 214_000, isLiveTv: true }), false);
  const calls = [];
  const player = {
    playing: event(), paused: event(), finished: event(), canceled: event(), error: event(),
    positionUpdate: event(), updateDuration: event(),
    setVideoRectangle(...args) { calls.push(['rectangle', ...args]); },
    load(...args) { calls.push(['load', ...args.slice(0, 5)]); args.at(-1)(true); },
    stop() { calls.push(['stop']); },
    pause() { calls.push(['pause']); },
    play() { calls.push(['play']); },
    seekTo(value) { calls.push(['seek', value]); },
    getPosition(callback) { callback(12.345); },
  };
  const nativeProfile = { DirectPlayProfiles: [{ Type: 'Video', Container: 'mkv' }], TranscodingProfiles: [] };
  const stage = { getBoundingClientRect: () => ({ left: 8, top: 30, width: 800, height: 450 }) };
  const host = {
    location: { href: 'http://127.0.0.1:18096/web/' , origin: 'http://127.0.0.1:18096' },
    jmpInfo: {},
    NativeShell: { AppHost: { getDeviceProfile: () => nativeProfile } },
    apiPromise: Promise.resolve({ player }),
    document: { querySelector: (selector) => selector === '#player-stage' ? stage : null },
    addEventListener() {},
    removeEventListener() {},
  };
  assert.equal(adapter.isPresent(host), true);
  assert.deepEqual(adapter.getDeviceProfile(host), nativeProfile);

  const diagnosticText = adapter.formatPlaybackRouteDiagnostic({
    helperUsed: true,
    nativeHls: true,
    subtitleSelectionRequested: true,
    requestCount: 4,
    branch: 'external-hls',
    probeSourcePresent: true,
    probeCapabilities: { sourcePresent: true, transcoding: true, transcodingUrlPresent: true, url: '/private' },
    selectedStreamFound: true,
    selectedStreamText: true,
    selectedStreamExternalSupported: true,
    externalProfileFormatAvailable: true,
    finalCapabilities: { sourcePresent: true, transcoding: false, transcodingUrlPresent: false, token: 'private' },
    url: '/private',
    token: 'private',
  });
  const presentedDiagnostic = JSON.parse(diagnosticText);
  assert.equal(presentedDiagnostic.requestCount, 3, 'the client diagnostic presentation caps request counts');
  assert.equal(presentedDiagnostic.branch, 'external-hls');
  assert.equal(presentedDiagnostic.subtitleSelectionRequested, true);
  assert.equal(presentedDiagnostic.probeCapabilities.transcodingUrlPresent, true);
  assert.equal(presentedDiagnostic.url, undefined, 'the diagnostic presentation excludes arbitrary fields');
  assert.equal(presentedDiagnostic.probeCapabilities.url, undefined, 'nested diagnostic values exclude URLs');
  assert.equal(presentedDiagnostic.finalCapabilities.token, undefined, 'nested diagnostic values exclude credentials');

  const switchPayload = {
    DeviceProfile: nativeProfile,
    StartTimeTicks: 45_000_000,
    SubtitleStreamIndex: 3,
  };
  const negotiationRequests = [];
  const directSubtitlePlayback = await adapter.playbackInfoForSubtitle(async (payload) => {
    negotiationRequests.push(payload);
    return {
      MediaSources: [{
        SupportsDirectPlay: true,
        DirectStreamUrl: '/Videos/subtitle-switch/stream',
        MediaStreams: [{ Index: 3, Type: 'Subtitle', Codec: 'subrip', IsExternal: false }],
      }],
    };
  }, switchPayload);
  assert.equal(negotiationRequests.length, 1, 'native text subtitle selection should first keep the direct file playable');
  assert.deepEqual(negotiationRequests[0], { DeviceProfile: nativeProfile },
    'native subtitle switching leaves both stream selection and seek position to the native player during direct-play negotiation');
  assert.equal(directSubtitlePlayback.playback.MediaSources[0].SupportsDirectPlay, true);
  assert.equal(directSubtitlePlayback.diagnostic.requestCount, 1,
    'a direct subtitle switch uses one subtitle-free capability probe');
  assert.equal(directSubtitlePlayback.diagnostic.branch, 'direct');
  assert.equal(directSubtitlePlayback.diagnostic.selectedStreamFound, true);
  assert.equal(directSubtitlePlayback.diagnostic.selectedStreamText, true);
  assert.equal(directSubtitlePlayback.diagnostic.probeCapabilities.directPlay, true);

  const subtitleSwitchPlayer = makePlayer();
  const subtitleSwitchHost = makeHost(subtitleSwitchPlayer.player);
  const subtitleSwitchSession = await adapter.load({
    url: directSubtitlePlayback.playback.MediaSources[0].DirectStreamUrl,
    accessToken: 'token', mediaType: 'video', item: { Id: 'subtitle-switch' },
    streams: directSubtitlePlayback.playback.MediaSources[0].MediaStreams,
    subtitleStreamIndex: 3, startTimeMilliseconds: 4_500,
  }, subtitleSwitchHost);
  const subtitleSwitchLoad = subtitleSwitchPlayer.calls.find((entry) => entry[0] === 'load');
  assert.deepEqual(subtitleSwitchLoad[2], { startMilliseconds: 4_500, autoplay: true },
    'native direct playback keeps the playback position when the subtitle selection changes');
  assert.equal(subtitleSwitchLoad[5], '#1', 'native direct playback selects an embedded text subtitle by its relative track order');
  await adapter.stop(subtitleSwitchSession, subtitleSwitchHost);

  const selectedTrackPlayback = { MediaSources: [{ SupportsDirectPlay: false, SupportsTranscoding: true }] };
  const fallbackRequests = [];
  const fallbackPlayback = await adapter.playbackInfoForSubtitle(async (payload) => {
    fallbackRequests.push(payload);
    return fallbackRequests.length === 1
      ? { MediaSources: [{
        SupportsDirectPlay: false,
        MediaStreams: [{ Index: 3, Type: 'Subtitle', Codec: 'subrip' }],
      }] }
      : selectedTrackPlayback;
  }, switchPayload);
  assert.deepEqual(fallbackRequests, [{ DeviceProfile: nativeProfile }, switchPayload],
    'when direct playback is unavailable the server receives the selected subtitle and seek position for HLS negotiation');
  assert.equal(fallbackPlayback.playback, selectedTrackPlayback);
  assert.equal(fallbackPlayback.diagnostic.requestCount, 2);
  assert.equal(fallbackPlayback.diagnostic.branch, 'selected-subtitle');
  assert.equal(fallbackPlayback.diagnostic.selectedStreamExternalSupported, false);

  const bitmapFallbackRequests = [];
  await adapter.playbackInfoForSubtitle(async (payload) => {
    bitmapFallbackRequests.push(payload);
    return bitmapFallbackRequests.length === 1
      ? { MediaSources: [{
        SupportsDirectPlay: true,
        MediaStreams: [{ Index: 3, Type: 'Subtitle', Codec: 'pgssub', IsExternal: false }],
      }] }
      : selectedTrackPlayback;
  }, switchPayload);
  assert.deepEqual(bitmapFallbackRequests, [{ DeviceProfile: nativeProfile }, switchPayload],
    'a bitmap subtitle is negotiated by the server because the native adapter has no text-track route for it');

  const hlsSubtitleProfile = {
    ...nativeProfile,
    SubtitleProfiles: [
      { Format: 'srt', Method: 'External' },
      { Format: 'srt', Method: 'Embed' },
    ],
  };
  const hlsSubtitlePayload = { ...switchPayload, DeviceProfile: hlsSubtitleProfile };
  const externalRequests = [];
  const hlsSubtitlePlayback = await adapter.playbackInfoForSubtitle(async (payload) => {
    externalRequests.push(payload);
    return {
      MediaSources: [{
        SupportsDirectPlay: false,
        SupportsTranscoding: true,
        TranscodingUrl: '/Videos/subtitle-switch/master.m3u8?startTimeTicks=45000000',
        MediaStreams: [{ Index: 3, Type: 'Subtitle', Codec: 'subrip', IsTextSubtitleStream: true, SupportsExternalStream: true }],
      }],
    };
  }, hlsSubtitlePayload);
  assert.equal(hlsSubtitlePlayback.subtitleDeliveryFormat, 'srt',
    'native external subtitle delivery follows the format advertised by the player');
  assert.equal(hlsSubtitlePlayback.diagnostic.requestCount, 2);
  assert.equal(hlsSubtitlePlayback.diagnostic.branch, 'external-hls');
  assert.equal(hlsSubtitlePlayback.diagnostic.selectedStreamExternalSupported, true);
  assert.equal(hlsSubtitlePlayback.diagnostic.externalProfileFormatAvailable, true);
  assert.equal(hlsSubtitlePlayback.diagnostic.probeCapabilities.directPlay, false);
  assert.equal(hlsSubtitlePlayback.diagnostic.probeCapabilities.transcoding, true);
  assert.equal(hlsSubtitlePlayback.diagnostic.finalCapabilities.transcoding, true);
  assert.deepEqual(externalRequests, [
    { DeviceProfile: hlsSubtitleProfile },
    { DeviceProfile: hlsSubtitleProfile, StartTimeTicks: 45_000_000 },
  ], 'HLS negotiation leaves the subtitle out of the manifest when the player declares SRT External');

  const hlsSubtitlePlayer = makePlayer();
  const hlsSubtitleHost = makeHost(hlsSubtitlePlayer.player);
  const hlsSubtitleSession = await adapter.load({
    url: hlsSubtitlePlayback.playback.MediaSources[0].TranscodingUrl,
    accessToken: 'token', mediaType: 'video', item: { Id: 'subtitle-switch' }, usesHls: true,
    streams: hlsSubtitlePlayback.playback.MediaSources[0].MediaStreams,
    subtitleStreamIndex: 3, subtitleDeliveryFormat: hlsSubtitlePlayback.subtitleDeliveryFormat,
    subtitleStartTimeMilliseconds: 4_500,
  }, hlsSubtitleHost);
  const hlsSubtitleLoad = hlsSubtitlePlayer.calls.find((entry) => entry[0] === 'load');
  const hlsSubtitleUrl = new URL(hlsSubtitleLoad[5].slice(2));
  assert.equal(hlsSubtitleUrl.pathname, '/Videos/subtitle-switch/subtitle-switch/Subtitles/3/45000000/Stream.srt',
    'HLS playback loads a time-shifted external SRT when the profile declares that delivery method');
  assert.equal(hlsSubtitleUrl.searchParams.get('ApiKey'), 'token', 'external HLS subtitles carry scoped query authorization');
  await adapter.stop(hlsSubtitleSession, hlsSubtitleHost);

  const events = [];
  const session = await adapter.load({
    url: '/Videos/item-id/stream', accessToken: 'secret-token', mediaType: 'video',
    item: { Id: 'item-id', Name: 'Original fixture', Type: 'Movie' },
    streams: [
      { Index: 4, Type: 'Audio' }, { Index: 7, Type: 'Audio' },
      { Index: 3, Type: 'Subtitle', Codec: 'ass', IsExternal: false },
      { Index: 9, Type: 'Subtitle', Codec: 'srt', IsExternal: true, SupportsExternalStream: true,
        DeliveryUrl: '/Videos/item-id/item-id/Subtitles/9/Stream.vtt' },
    ],
    audioStreamIndex: 7, subtitleStreamIndex: 9, startTimeMilliseconds: 4_500,
    onEvent: (name, value) => events.push([name, value]),
  }, host);
  const load = calls.find((entry) => entry[0] === 'load');
  assert.equal(load[1], 'http://127.0.0.1:18096/Videos/item-id/stream?ApiKey=secret-token');
  assert.equal(new URL(load[1]).searchParams.get('ApiKey'), 'secret-token', 'direct media uses scoped query auth');
  assert.deepEqual(load[2], { startMilliseconds: 4_500, autoplay: true });
  assert.deepEqual(load[3].headers, {}, 'the native player never receives the scoped token as a header');
  assert.equal(load[4], '#2', 'audio selector must use relative one-based stream order');
  const subtitleUrl = new URL(load[5].slice(2));
  assert.equal(subtitleUrl.pathname, '/Videos/item-id/item-id/Subtitles/9/Stream.vtt');
  assert.equal(subtitleUrl.searchParams.get('ApiKey'), 'secret-token', 'external native subtitles use scoped query auth');
  assert.deepEqual(calls[0], ['rectangle', 8, 30, 800, 450]);

  player.playing.emit();
  player.positionUpdate.emit(25_000);
  player.updateDuration.emit(90_000);
  assert.deepEqual(events.slice(-3), [['playing', undefined], ['position', 25_000], ['duration', 90_000]]);
  assert.equal(await adapter.getPosition(session), 12_345,
    'native stop callbacks return seconds and must preserve millisecond progress');
  assert.equal(adapter.seek(session, 40_000), true);
  assert.ok(calls.some((entry) => entry[0] === 'seek' && entry[1] === 40_000));
  assert.equal(adapter.control(session, 'pause'), true);
  await adapter.stop(session, host);
  assert.equal(player.playing.size, 0, 'stop must disconnect callbacks');
  assert.deepEqual(calls.at(-1), ['rectangle', -1, -1, -1, -1]);
  assert.ok(calls.some((entry) => entry[0] === 'stop'));

  await assert.rejects(() => adapter.load({
    url: 'https://elsewhere.example/media', accessToken: 'secret-token', mediaType: 'video', item: { Id: 'x' },
  }, host), /outside this Puffinbox origin/);
  await assert.rejects(() => adapter.load({
    url: '/Videos/x/stream', mediaType: 'video', item: { Id: 'x' },
  }, host), /Sign in again/);

  const pendingApi = deferred();
  const pendingCalls = [];
  const pendingPlayer = makePlayer((args) => pendingCalls.push(args));
  const pendingHost = makeHost(pendingPlayer.player, pendingApi.promise);
  const abortController = new AbortController();
  const pendingLoad = adapter.load({
    url: '/Videos/pending/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'pending' },
    signal: abortController.signal,
  }, pendingHost);
  await new Promise((resolve) => setImmediate(resolve));
  abortController.abort();
  await assert.rejects(pendingLoad, { name: 'AbortError' }, 'closing while apiPromise is pending cancels native startup');
  pendingApi.resolve({ player: pendingPlayer.player });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(pendingCalls.length, 0, 'a late native API connection must not start a closed session');

  let callbackA;
  let callbackB;
  const switching = makePlayer((args) => {
    if (args[0].includes('/a/')) callbackA = args.at(-1);
    else callbackB = args.at(-1);
  });
  const switchingHost = makeHost(switching.player);
  const switchingEvents = [];
  const loadA = adapter.load({
    url: '/Videos/a/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'a' },
    onEvent: (name) => switchingEvents.push(['a', name]),
  }, switchingHost);
  const loadARejected = assert.rejects(loadA, { name: 'AbortError' }, 'the first startup is cancelled when item B takes ownership');
  await waitFor(() => typeof callbackA === 'function');
  const loadB = adapter.load({
    url: '/Videos/b/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'b' },
    onEvent: (name) => switchingEvents.push(['b', name]),
  }, switchingHost);
  await waitFor(() => typeof callbackB === 'function');
  switching.player.error.emit('late error from item A');
  await loadARejected;
  assert.deepEqual(switchingEvents, [], 'an error while item B is awaiting its load callback is not attributed to it');
  callbackB(true);
  const sessionB = await loadB;
  const stopCountAfterBStarted = switching.calls.filter((entry) => entry[0] === 'stop').length;
  callbackA(true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(switching.calls.filter((entry) => entry[0] === 'stop').length, stopCountAfterBStarted,
    'a late callback from item A must not stop item B');
  assert.equal(adapter.control(sessionB, 'pause'), true);
  await adapter.stop(sessionB, switchingHost);

  const earlyEventsPlayer = makePlayer();
  earlyEventsPlayer.player.load = (...args) => {
    earlyEventsPlayer.calls.push(['load', ...args.slice(0, 5)]);
    earlyEventsPlayer.player.playing.emit();
    for (let position = 1; position <= 100; position += 1) earlyEventsPlayer.player.positionUpdate.emit(position * 1_000);
    earlyEventsPlayer.player.updateDuration.emit(95_000);
    args.at(-1)(true);
  };
  const earlyEventsHost = makeHost(earlyEventsPlayer.player);
  const earlyEvents = [];
  const earlySession = await adapter.load({
    url: '/Videos/early/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'early' },
    onEvent: (name, value) => earlyEvents.push([name, value]),
  }, earlyEventsHost);
  assert.deepEqual(earlyEvents, [['playing', undefined], ['position', 100_000], ['duration', 95_000]],
    'real early events are replayed after acknowledgement with position updates coalesced');
  assert.equal(await adapter.getPosition(earlySession), 12_345);
  await adapter.stop(earlySession, earlyEventsHost);

  const failedBeforeAck = makePlayer();
  failedBeforeAck.player.load = (...args) => {
    failedBeforeAck.calls.push(['load', ...args.slice(0, 5)]);
    failedBeforeAck.player.error.emit('new-load failure before callback');
    args.at(-1)(true);
  };
  const failedBeforeAckHost = makeHost(failedBeforeAck.player);
  const startupEvents = [];
  const acceptedWithoutPlayback = await adapter.load({
    url: '/Videos/no-activity/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'no-activity' },
    onEvent: (name, value) => startupEvents.push([name, value]),
  }, failedBeforeAckHost);
  assert.deepEqual(startupEvents, [], 'the load callback must not fabricate successful playback');
  let startupFailure = '';
  let startupTimer;
  const timerHarness = {
    setTimeout(callback, milliseconds) { startupTimer = { callback, milliseconds }; return 91; },
    clearTimeout() { startupTimer = null; },
  };
  heartbeat.startNativeStartupWatchdog(acceptedWithoutPlayback, () => true,
    () => { startupFailure = 'no native playback activity'; }, timerHarness, 20_000);
  assert.equal(startupTimer.milliseconds, 20_000);
  startupTimer.callback();
  assert.equal(startupFailure, 'no native playback activity', 'pre-ack failure with no later events reaches a bounded failure');
  await adapter.stop(acceptedWithoutPlayback, failedBeforeAckHost);

  const badSubtitle = makePlayer();
  const badSubtitleHost = makeHost(badSubtitle.player);
  await assert.rejects(() => adapter.load({
    url: '/Videos/subtitle/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'subtitle' },
    streams: [{ Index: 2, Type: 'Subtitle', Codec: 'srt', IsExternal: true,
      DeliveryUrl: 'https://attacker.example/subtitle.vtt' }], subtitleStreamIndex: 2,
  }, badSubtitleHost), /outside this Puffinbox origin/);
  assert.equal(badSubtitle.player.playing.size, 0, 'setup failures disconnect event listeners');
  assert.deepEqual(badSubtitle.calls.at(-1), ['rectangle', -1, -1, -1, -1], 'setup failures restore the native surface');

  const transcoded = makePlayer();
  const transcodedHost = makeHost(transcoded.player);
  const hlsToken = 'secret+/=&token';
  const transcodedSession = await adapter.load({
    url: '/Videos/transcoded/master.m3u8?PlaySessionId=test', accessToken: hlsToken, mediaType: 'video',
    item: { Id: 'transcoded' }, usesHls: true, startTimeMilliseconds: 0,
    streams: [{ Index: 4, Type: 'Audio' }, { Index: 8, Type: 'Audio' }, { Index: 11, Type: 'Subtitle' }],
    audioStreamIndex: 8, subtitleStreamIndex: 11,
  }, transcodedHost);
  const transcodedLoad = transcoded.calls.find((entry) => entry[0] === 'load');
  const transcodedUrl = new URL(transcodedLoad[1]);
  assert.equal(transcodedUrl.searchParams.get('ApiKey'), hlsToken, 'HLS root carries the read-only media credential');
  assert.equal(transcodedUrl.searchParams.get('PlaySessionId'), 'test', 'HLS auth preserves the negotiated playback session');
  assert.equal(transcodedLoad[4], '#1', 'the native player must enable the single audio track selected by the HLS server');
  assert.equal(transcodedLoad[5], '', 'HLS subtitle selection is already applied and offset by the server');
  assert.deepEqual(transcodedLoad[3].headers, {}, 'HLS token is not exposed in a native authorization header');
  await adapter.stop(transcodedSession, transcodedHost);

  for (const selection of [undefined, 4, 7, -1]) {
    const defaultAudio = makePlayer();
    const defaultAudioHost = makeHost(defaultAudio.player);
    const audioSession = await adapter.load({
      url: '/Videos/default-audio/stream', accessToken: 'token', mediaType: 'video',
      item: { Id: 'default-audio' },
      streams: [{ Index: 4, Type: 'Audio' }, { Index: 7, Type: 'Audio', IsDefault: true }],
      audioStreamIndex: selection,
    }, defaultAudioHost);
    assert.equal(defaultAudio.calls.find(([name]) => name === 'load')[4],
      selection === -1 ? '' : selection === 4 ? '#1' : '#2',
      'native playback enables the default track while preserving explicit selection or disablement');
    await adapter.stop(audioSession, defaultAudioHost);
  }
  for (const [streams, selection] of [[[], undefined], [[{ Index: 4, Type: 'Audio' }], -1]]) {
    const silent = makePlayer();
    const silentHost = makeHost(silent.player);
    const silentSession = await adapter.load({
      url: '/Videos/silent/master.m3u8', accessToken: 'token', mediaType: 'video',
      item: { Id: 'silent' }, usesHls: true, streams, audioStreamIndex: selection,
    }, silentHost);
    assert.equal(silent.calls.find(([name]) => name === 'load')[4], '',
      'silent sources and explicitly disabled audio do not enable a native audio track');
    await adapter.stop(silentSession, silentHost);
  }

  const fullTimeline = makePlayer();
  const fullTimelineHost = makeHost(fullTimeline.player);
  const fullTimelineEvents = [];
  const fullTimelineSession = await adapter.load({
    url: '/Videos/full/master.m3u8?fullTimeline=true', accessToken: 'token', mediaType: 'video',
    item: { Id: 'full' }, usesHls: true, fullHlsTimeline: true, startTimeMilliseconds: 160_213,
    onEvent: (name, value) => fullTimelineEvents.push([name, value]),
  }, fullTimelineHost);
  assert.equal(fullTimeline.calls.find(([name]) => name === 'load')[2].startMilliseconds, 0,
    'the HLS origin is established before a native resume seek');
  assert.equal(fullTimeline.calls.some(([name]) => name === 'seek'), false,
    'acknowledging a load does not establish a decoded origin');
  fullTimeline.player.playing.emit();
  fullTimeline.player.positionUpdate.emit(500);
  assert.deepEqual(fullTimelineEvents, [], 'startup at zero must not overwrite the saved resume position');
  assert.deepEqual(fullTimeline.calls.at(-1), ['seek', 160_213]);
  fullTimeline.player.positionUpdate.emit(750);
  assert.equal(fullTimeline.calls.filter(([name]) => name === 'seek').length, 1);
  assert.deepEqual(fullTimelineEvents, [], 'old position updates are ignored while the resume seek is pending');
  fullTimeline.player.positionUpdate.emit(160_213);
  assert.deepEqual(fullTimelineEvents, [['position', 160_213], ['playing', undefined]],
    'playback is reported only after the native player reaches the resumed source time');
  assert.equal(adapter.seek(fullTimelineSession, 0), true);
  fullTimeline.player.positionUpdate.emit(0);
  assert.deepEqual(fullTimelineEvents.at(-1), ['position', 0], 'later backward seeks use the full source timeline');
  await adapter.stop(fullTimelineSession, fullTimelineHost);

  const externalHls = makePlayer();
  const externalHlsHost = makeHost(externalHls.player);
  await assert.rejects(() => adapter.load({
    url: 'https://attacker.example/master.m3u8', accessToken: hlsToken, mediaType: 'video',
    item: { Id: 'external-hls' }, usesHls: true,
  }, externalHlsHost), /outside this Puffinbox origin/);
  assert.equal(externalHls.calls.some((entry) => entry[0] === 'load'), false,
    'the native player never receives an external URL with the media credential attached');

  const pausedNative = makePlayer();
  const pausedNativeHost = makeHost(pausedNative.player);
  const pausedNativeSession = await adapter.load({
    url: '/Videos/paused/stream', accessToken: 'token', mediaType: 'video', item: { Id: 'paused' },
    autoplay: false,
  }, pausedNativeHost);
  const pausedNativeLoad = pausedNative.calls.find((entry) => entry[0] === 'load');
  assert.equal(pausedNativeLoad[2].autoplay, false, 'a paused native player remains paused across track reload');
  await adapter.stop(pausedNativeSession, pausedNativeHost);
  console.log('Documented native player adapter tests passed.');
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
