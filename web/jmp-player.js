((root, factory) => {
  const adapter = factory();
  if (typeof module === 'object' && module.exports) module.exports = adapter;
  else root.PuffinboxJmpPlayer = adapter;
})(typeof globalThis === 'undefined' ? this : globalThis, () => {
  'use strict';

  const API_WAIT_MS = 1_500;
  const COMMAND_WAIT_MS = 3_000;
  const hostStates = new WeakMap();

  function isPresent(host = globalThis) {
    return Boolean(host?.jmpInfo && host?.NativeShell?.AppHost?.getDeviceProfile && host?.apiPromise);
  }

  function getDeviceProfile(host = globalThis) {
    if (!isPresent(host)) return null;
    try {
      const profile = host.NativeShell.AppHost.getDeviceProfile();
      return profile && typeof profile === 'object' ? profile : null;
    } catch (_) {
      return null;
    }
  }

  function bounded(promise, milliseconds, label) {
    let timer;
    return Promise.race([
      Promise.resolve(promise),
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label} timed out.`)), milliseconds);
      }),
    ]).finally(() => clearTimeout(timer));
  }

  function callbackCommand(target, method, args = [], timeout = COMMAND_WAIT_MS) {
    if (typeof target?.[method] !== 'function') return Promise.reject(new Error(`Native playback does not support ${method}.`));
    return bounded(new Promise((resolve, reject) => {
      let settled = false;
      const finish = (error, value) => {
        if (settled) return;
        settled = true;
        if (error) reject(error);
        else resolve(value);
      };
      try {
        target[method](...args, (value) => finish(null, value));
      } catch (error) { finish(error); }
    }), timeout, `Native ${method}`);
  }

  function abortError() {
    const error = new Error('Native playback was cancelled.');
    error.name = 'AbortError';
    return error;
  }

  function hostState(host) {
    let state = hostStates.get(host);
    if (!state) {
      state = { generation: 0, session: null };
      hostStates.set(host, state);
    }
    return state;
  }

  function isCurrent(session) {
    return !session.disconnected && session.state.session === session
      && session.state.generation === session.generation;
  }

  function withAbort(promise, signal) {
    if (!signal) return Promise.resolve(promise);
    if (signal.aborted) return Promise.reject(abortError());
    return new Promise((resolve, reject) => {
      const cleanup = () => signal.removeEventListener('abort', onAbort);
      const onAbort = () => { cleanup(); reject(abortError()); };
      signal.addEventListener('abort', onAbort, { once: true });
      if (signal.aborted) { onAbort(); return; }
      Promise.resolve(promise).then((value) => { cleanup(); resolve(value); }, (error) => { cleanup(); reject(error); });
    });
  }

  function checkedSameOriginUrl(rawUrl, host) {
    const location = host.location;
    const url = new URL(rawUrl, location.href);
    if (!location?.origin || url.origin !== location.origin) {
      throw new Error('The server returned a media URL outside this Puffinbox origin.');
    }
    return url;
  }

  function streamIndex(streams, selectedIndex, type) {
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0) return '';
    const choices = streams.filter((stream) => stream?.Type === type);
    const ordinal = choices.findIndex((stream) => Number(stream.Index) === selectedIndex);
    return ordinal < 0 ? '' : `#${ordinal + 1}`;
  }

  function isTextSubtitle(stream) {
    const format = String(stream?.Codec || stream?.Format || '').toLowerCase();
    return stream?.IsTextSubtitleStream === true || stream?.SupportsExternalStream === true
      || stream?.IsExternal === true || ['srt', 'vtt', 'ass', 'ssa', 'subrip', 'webvtt'].includes(format);
  }

  function externalSubtitleFormat(deviceProfile) {
    const profiles = Array.isArray(deviceProfile?.SubtitleProfiles) ? deviceProfile.SubtitleProfiles : [];
    const externalFormats = new Set(profiles
      .filter((profile) => String(profile?.Method || '').toLowerCase() === 'external')
      .map((profile) => String(profile?.Format || '').toLowerCase()));
    if (externalFormats.has('srt') || externalFormats.has('subrip')) return 'srt';
    if (externalFormats.has('vtt') || externalFormats.has('webvtt')) return 'vtt';
    return '';
  }

  function playbackCapabilities(playback) {
    const source = playback?.MediaSources?.[0];
    return {
      sourcePresent: Boolean(source),
      directPlay: source?.SupportsDirectPlay === true,
      directStream: source?.SupportsDirectStream === true,
      directStreamUrlPresent: Boolean(source?.DirectStreamUrl),
      transcoding: source?.SupportsTranscoding === true,
      transcodingUrlPresent: Boolean(source?.TranscodingUrl),
    };
  }

  function formatPlaybackRouteDiagnostic(diagnostic) {
    const value = diagnostic && typeof diagnostic === 'object' ? diagnostic : {};
    const capabilities = (candidate) => candidate && typeof candidate === 'object' ? {
      sourcePresent: candidate.sourcePresent === true,
      directPlay: candidate.directPlay === true,
      directStream: candidate.directStream === true,
      directStreamUrlPresent: candidate.directStreamUrlPresent === true,
      transcoding: candidate.transcoding === true,
      transcodingUrlPresent: candidate.transcodingUrlPresent === true,
    } : null;
    return JSON.stringify({
      helperUsed: value.helperUsed === true,
      nativeHls: value.nativeHls === true,
      subtitleSelectionRequested: value.subtitleSelectionRequested === true,
      requestCount: Number.isInteger(value.requestCount) ? Math.max(0, Math.min(value.requestCount, 3)) : 0,
      branch: ['direct', 'external-hls', 'selected-subtitle'].includes(value.branch) ? value.branch : 'unavailable',
      probeSourcePresent: value.probeSourcePresent === true,
      probeCapabilities: capabilities(value.probeCapabilities),
      selectedStreamFound: value.selectedStreamFound === true,
      selectedStreamText: value.selectedStreamText === true,
      selectedStreamExternalSupported: value.selectedStreamExternalSupported === true,
      externalProfileFormatAvailable: value.externalProfileFormatAvailable === true,
      finalCapabilities: capabilities(value.finalCapabilities),
    }, null, 2);
  }

  async function playbackInfoForSubtitle(requestPlaybackInfo, payload) {
    const selectedIndex = payload?.SubtitleStreamIndex;
    if (typeof requestPlaybackInfo !== 'function') throw new TypeError('PlaybackInfo requests must be callable.');
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0) {
      return { playback: await requestPlaybackInfo(payload), subtitleDeliveryFormat: '' };
    }

    const diagnostic = {
      requestCount: 0,
      branch: 'selected-subtitle',
      probeSourcePresent: false,
      probeCapabilities: null,
      selectedStreamFound: false,
      selectedStreamText: false,
      selectedStreamExternalSupported: false,
      externalProfileFormatAvailable: false,
      finalCapabilities: null,
    };
    const request = async (requestPayload) => {
      diagnostic.requestCount = Math.min(diagnostic.requestCount + 1, 3);
      return requestPlaybackInfo(requestPayload);
    };
    const result = (playback, subtitleDeliveryFormat) => ({
      playback,
      subtitleDeliveryFormat,
      diagnostic: {
        ...diagnostic,
        finalCapabilities: playbackCapabilities(playback),
      },
    });

    // A native subtitle switch can use the player's own track selector when
    // the original file remains directly playable. Keep the playback offset
    // for the native load call rather than making the server reject direct play.
    const directPayload = { ...payload };
    delete directPayload.SubtitleStreamIndex;
    delete directPayload.StartTimeTicks;
    const directPlayback = await request(directPayload);
    const source = directPlayback?.MediaSources?.[0];
    const streams = Array.isArray(source?.MediaStreams) ? source.MediaStreams : [];
    const selectedStream = streams.find((stream) => stream?.Type === 'Subtitle' && Number(stream.Index) === selectedIndex);
    const externalFormat = externalSubtitleFormat(payload.DeviceProfile);
    diagnostic.probeSourcePresent = Boolean(source);
    diagnostic.probeCapabilities = playbackCapabilities(directPlayback);
    diagnostic.selectedStreamFound = Boolean(selectedStream);
    diagnostic.selectedStreamText = isTextSubtitle(selectedStream);
    diagnostic.selectedStreamExternalSupported = selectedStream?.SupportsExternalStream === true;
    diagnostic.externalProfileFormatAvailable = Boolean(externalFormat);
    if (source?.SupportsDirectPlay === true && isTextSubtitle(selectedStream)) {
      diagnostic.branch = 'direct';
      return result(directPlayback,
        selectedStream.IsExternal === true ? externalFormat : '');
    }

    if (isTextSubtitle(selectedStream) && selectedStream.SupportsExternalStream === true && externalFormat) {
      // Keep the server's HLS request subtitle-free, then load the subtitle as
      // a separate native track in one of the client's declared external formats.
      const hlsPayload = { ...payload };
      delete hlsPayload.SubtitleStreamIndex;
      diagnostic.branch = 'external-hls';
      return result(await request(hlsPayload), externalFormat);
    }

    return result(await request(payload), '');
  }

  function subtitleArgument({
    host, itemId, streams, selectedIndex, accessToken, usesHls = false,
    deliveryFormat = '', startTimeMilliseconds = 0,
  }) {
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0) return '';
    const stream = streams.find((entry) => entry?.Type === 'Subtitle' && Number(entry.Index) === selectedIndex);
    if (!stream) return '';
    if (!isTextSubtitle(stream)) return '';
    if (!usesHls && !stream.IsExternal) return streamIndex(streams, selectedIndex, 'Subtitle');

    const format = ['srt', 'vtt'].includes(String(deliveryFormat).toLowerCase())
      ? String(deliveryFormat).toLowerCase()
      : 'vtt';
    const startTicks = usesHls
      ? Math.max(0, Math.round(Number(startTimeMilliseconds) * 10_000) || 0)
      : 0;
    const position = startTicks > 0 ? `/${startTicks}` : '';
    const candidate = usesHls
      ? `/Videos/${encodeURIComponent(itemId)}/${encodeURIComponent(itemId)}`
        + `/Subtitles/${encodeURIComponent(String(stream.Index))}${position}/Stream.${format}`
      : stream.DeliveryUrl
        || `/Videos/${encodeURIComponent(itemId)}/${encodeURIComponent(itemId)}`
          + `/Subtitles/${encodeURIComponent(String(stream.Index))}/Stream.vtt`;
    const url = checkedSameOriginUrl(candidate, host);
    if (!usesHls && deliveryFormat && /\/Stream\.(?:srt|vtt)$/i.test(url.pathname)) {
      url.pathname = url.pathname.replace(/\/Stream\.(?:srt|vtt)$/i, `/Stream.${format}`);
    }
    url.searchParams.set('ApiKey', accessToken);
    return `#,${url.href}`;
  }

  function makeMetadata(item, mediaType) {
    return {
      Id: String(item?.Id || ''),
      Name: String(item?.Name || 'Media'),
      Type: String(item?.Type || (mediaType === 'audio' ? 'Audio' : 'Video')),
      MediaType: mediaType === 'audio' ? 'audio' : 'video',
    };
  }

  async function load(options, host = globalThis) {
    if (!isPresent(host)) throw new Error('Native playback is unavailable in this browser.');
    if (!options?.accessToken) throw new Error('Sign in again to use native playback.');
    const state = hostState(host);
    if (state.session) await stop(state.session, host);
    const session = {
      api: null,
      host,
      state,
      generation: ++state.generation,
      positionMs: 0,
      durationMs: 0,
      disconnected: false,
      loadAccepted: false,
      loadIssued: false,
      pendingEvents: [],
      listeners: [],
      onEvent: typeof options.onEvent === 'function' ? options.onEvent : () => {},
      signal: options.signal,
      cancelController: new AbortController(),
      stopPromise: null,
    };
    state.session = session;
    session.abortListener = () => { void stop(session, host); };
    session.signal?.addEventListener('abort', session.abortListener, { once: true });
    try {
      if (session.signal?.aborted) throw abortError();
      const mediaUrl = checkedSameOriginUrl(options.url, host);
      // Native requests carry a short-lived read-only media token in their
      // same-origin URL because the player does not reliably retain headers.
      // The server accepts it only on bounded GET/HEAD media routes.
      mediaUrl.searchParams.set('ApiKey', options.accessToken);
      const url = mediaUrl.href;
      const api = await bounded(withAbort(Promise.resolve(host.apiPromise), session.cancelController.signal), API_WAIT_MS, 'Native playback connection');
      if (!isCurrent(session) || session.signal?.aborted) throw abortError();
      if (!api?.player || typeof api.player.load !== 'function') throw new Error('The native player interface is unavailable.');
      session.api = api;

      const connect = (event, name, transform = (value) => value) => {
        if (typeof event?.connect !== 'function') return;
        const listener = (value) => {
          if (!isCurrent(session) || !session.loadIssued) return;
          if (!session.loadAccepted) {
            if (['error', 'canceled', 'finished'].includes(name)) return;
            const coalesced = ['position', 'duration'].includes(name)
              ? session.pendingEvents.findIndex((entry) => entry[0] === name) : -1;
            if (coalesced >= 0) session.pendingEvents[coalesced] = [name, value, transform];
            else if (session.pendingEvents.length < 16) session.pendingEvents.push([name, value, transform]);
            return;
          }
          session.onEvent(name, transform(value), session);
        };
        event.connect(listener);
        session.listeners.push([event, listener]);
      };
      connect(api.player.playing, 'playing');
      connect(api.player.paused, 'paused');
      connect(api.player.finished, 'finished');
      connect(api.player.canceled, 'canceled');
      connect(api.player.error, 'error', (value) => String(value || 'Native playback failed.'));
      connect(api.player.positionUpdate, 'position', (value) => {
        session.positionMs = Math.max(0, Number(value) || 0);
        return session.positionMs;
      });
      connect(api.player.updateDuration, 'duration', (value) => {
        session.durationMs = Math.max(0, Number(value) || 0);
        return session.durationMs;
      });

      const stage = host.document?.querySelector?.('#player-stage');
      session.updateRectangle = () => {
        if (!isCurrent(session) || !stage || typeof api.player.setVideoRectangle !== 'function') return;
        const rect = stage.getBoundingClientRect();
        api.player.setVideoRectangle(Math.round(rect.left), Math.round(rect.top), Math.round(rect.width), Math.round(rect.height));
      };
      session.resetRectangle = () => {
        if (typeof api.player.setVideoRectangle === 'function') api.player.setVideoRectangle(-1, -1, -1, -1);
      };
      if (options.mediaType !== 'audio') {
        session.updateRectangle();
        host.addEventListener?.('resize', session.updateRectangle);
        host.addEventListener?.('scroll', session.updateRectangle, true);
      }

      const streams = Array.isArray(options.streams) ? options.streams : [];
      const data = {
        type: options.mediaType === 'audio' ? 'music' : 'video',
        headers: {},
        metadata: makeMetadata(options.item, options.mediaType),
        media: {},
      };
      const tracksAlreadySelected = options.usesHls === true;
      const audio = tracksAlreadySelected ? '' : streamIndex(streams, options.audioStreamIndex, 'Audio');
      const subtitle = tracksAlreadySelected && !options.subtitleDeliveryFormat ? '' : subtitleArgument({
        host,
        itemId: options.item?.Id,
        streams,
        selectedIndex: options.subtitleStreamIndex,
        accessToken: options.accessToken,
        usesHls: tracksAlreadySelected,
        deliveryFormat: options.subtitleDeliveryFormat,
        startTimeMilliseconds: options.subtitleStartTimeMilliseconds,
      });
      if (!isCurrent(session)) throw abortError();
      const position = {
        startMilliseconds: Math.max(0, Math.round(Number(options.startTimeMilliseconds) || 0)),
        autoplay: options.autoplay !== false,
      };
      session.loadIssued = true;
      const accepted = await withAbort(callbackCommand(api.player, 'load', [url, position, data, audio, subtitle]), session.cancelController.signal);
      if (!isCurrent(session) || session.signal?.aborted) throw abortError();
      if (accepted === false) throw new Error('The native player rejected this stream.');
      session.loadAccepted = true;
      for (const [name, value, transform] of session.pendingEvents.splice(0)) {
        if (!isCurrent(session)) break;
        session.onEvent(name, transform(value), session);
      }
      return session;
    } catch (error) {
      await stop(session, host);
      throw error;
    }
  }

  async function stop(session, host = globalThis) {
    if (!session) return;
    if (session.stopPromise) return session.stopPromise;
    const owned = isCurrent(session);
    session.disconnected = true;
    session.cancelController.abort();
    if (owned) {
      session.state.session = null;
      session.state.generation += 1;
    }
    session.signal?.removeEventListener('abort', session.abortListener);
    for (const [event, listener] of session.listeners) {
      try { event.disconnect?.(listener); } catch (_) { /* The client may already have removed the event. */ }
    }
    host.removeEventListener?.('resize', session.updateRectangle);
    host.removeEventListener?.('scroll', session.updateRectangle, true);
    if (owned) {
      try { session.api?.player?.stop?.(); } catch (_) { /* Continue restoring the native video surface. */ }
      try { session.resetRectangle?.(); } catch (_) { /* The client may be closing. */ }
    }
    session.stopPromise = Promise.resolve();
    return session.stopPromise;
  }

  function control(session, action, ...args) {
    const player = session?.api?.player;
    if (!player || !isCurrent(session) || typeof player[action] !== 'function') return false;
    player[action](...args);
    return true;
  }

  function seek(session, milliseconds) {
    const player = session?.api?.player;
    const position = Math.max(0, Math.round(Number(milliseconds) || 0));
    if (!player || !isCurrent(session) || typeof player.seekTo !== 'function') return false;
    player.seekTo(position);
    session.positionMs = position;
    return true;
  }

  function getPosition(session, timeout = 1_500) {
    if (!isCurrent(session) || typeof session?.api?.player?.getPosition !== 'function') {
      return Promise.resolve(Math.max(0, Number(session?.positionMs) || 0));
    }
    return callbackCommand(session.api.player, 'getPosition', [], timeout)
      .then((value) => {
        session.positionMs = Math.max(0, Number(value) || 0);
        return session.positionMs;
      });
  }

  return { isPresent, getDeviceProfile, externalSubtitleFormat, playbackInfoForSubtitle, formatPlaybackRouteDiagnostic, load, stop, control, seek, getPosition };
});
