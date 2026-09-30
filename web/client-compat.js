(function (root) {
  'use strict';

  function replaceChildren(element, ...nodes) {
    if (typeof element.replaceChildren === 'function') {
      element.replaceChildren(...nodes);
      return;
    }
    while (element.firstChild) element.removeChild(element.firstChild);
    nodes.forEach((node) => element.appendChild(node));
  }

  function startupFailure(error, category) {
    if (category === 'render') {
      return {
        title: 'Page could not be displayed',
        description: 'The server responded, but this page could not display the library. Reload to try again.',
        detail: error && error.message ? String(error.message) : 'The page encountered a display problem.',
      };
    }
    return {
      title: 'Server unavailable',
      description: 'Could not connect to the Puffinbox API. Check that the server is running and then reload this page.',
      detail: error && error.message ? String(error.message) : 'Network request failed',
    };
  }

  function scanStatusLabel(status) {
    return String(status || 'unknown').split('_').join(' ');
  }

  function mediaErrorDescription(error) {
    if (!error) return 'Browser media playback failed without browser error details.';
    const code = Number(error.code) || 0;
    const labels = {
      1: 'playback aborted',
      2: 'network error',
      3: 'decode error',
      4: 'source not supported',
    };
    const label = labels[code] || 'unclassified media error';
    const message = error.message ? `: ${String(error.message)}` : '';
    return `Browser media error ${code} (${label})${message}.`;
  }

  const api = { replaceChildren, scanStatusLabel, startupFailure, mediaErrorDescription };
  if (root) root.PuffinboxClientCompat = api;
  if (typeof module === 'object' && module.exports) module.exports = api;
})(typeof window === 'object' ? window : globalThis);
