(function exposeDeviceId(root, factory) {
  const api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  if (root) root.PuffinboxDeviceId = api;
})(typeof window === 'undefined' ? null : window, function makeDeviceIdApi() {
  const storageKey = 'puffinbox-device-id';
  let memoryDeviceId = null;

  function makeUuid(cryptoApi) {
    if (typeof cryptoApi?.randomUUID === 'function') return cryptoApi.randomUUID();
    if (typeof cryptoApi?.getRandomValues === 'function') {
      const bytes = new Uint8Array(16);
      cryptoApi.getRandomValues(bytes);
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
      return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    }
    return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (char) => {
      const value = Math.floor(Math.random() * 16);
      return (char === 'x' ? value : (value & 0x3) | 0x8).toString(16);
    });
  }

  function isDeviceId(value) {
    return typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
  }

  function createResolver(storageProviders, createId) {
    let memoryValue = null;
    return function getDeviceId() {
      for (const provideStorage of storageProviders) {
        try {
          const storage = provideStorage();
          const existing = storage?.getItem(storageKey);
          if (isDeviceId(existing)) return existing;
          if (!storage) continue;
          const created = createId();
          storage.setItem(storageKey, created);
          return created;
        } catch (_) {
          // Storage can be unavailable in private browsing or restricted WebViews.
        }
      }
      if (!memoryValue) memoryValue = createId();
      return memoryValue;
    };
  }

  const browserResolver = typeof window === 'undefined' ? null : createResolver(
    [() => window.localStorage, () => window.sessionStorage],
    () => makeUuid(window.crypto),
  );

  return {
    createResolver,
    readSetting(storageProvider, key, fallback) {
      try { return storageProvider()?.getItem(key) || fallback; }
      catch (_) { return fallback; }
    },
    writeSetting(storageProvider, key, value) {
      try { storageProvider()?.setItem(key, value); }
      catch (_) { /* Preferences are optional in restricted browser storage contexts. */ }
    },
    get() {
      if (browserResolver) return browserResolver();
      if (!memoryDeviceId) memoryDeviceId = makeUuid(null);
      return memoryDeviceId;
    },
  };
});
