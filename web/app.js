(() => {
  'use strict';

  const app = document.querySelector('#app');
  const toast = document.querySelector('#toast');
  const playerDialog = document.querySelector('#player-dialog');
  const nativePlayerControls = document.querySelector('#native-player-controls');
  const nativePlayPauseButton = document.querySelector('#native-play-pause');
  const nativeSeek = document.querySelector('#native-seek');
  const nativePosition = document.querySelector('#native-position');
  const itemDialog = document.querySelector('#item-dialog');
  const playerOptions = document.querySelector('#player-options');
  const audioTrackControl = document.querySelector('#audio-track-control');
  const subtitleTrackControl = document.querySelector('#subtitle-track-control');
  const audioTrackSelect = document.querySelector('#player-audio-track');
  const subtitleTrackSelect = document.querySelector('#player-subtitle-track');
  const playerResumeControls = document.querySelector('#player-resume-controls');
  const playerResumeButton = document.querySelector('#player-resume');
  const playerStartOverButton = document.querySelector('#player-start-over');
  const OFFLINE_WORKER_URL = '/web/offline-sw.js?revision=offline-media-navigation-v2';
  const MAX_EMBEDDED_OFFLINE_PLAYBACK_BYTES = 512 * 1024 * 1024;
  let activePlayback = null;
  let playbackQueue = null;
  let mediaAccessToken = null;
  let mediaAccessTokenExpiresAt = 0;
  let pendingPlayback = null;
  let playbackGeneration = 0;
  let scanStatusTimer = null;
  let photoLoadController = null;
  let photoObjectUrl = null;
  let offlinePlaybackObjectUrl = null;
  let offlinePlaybackGeneration = 0;
  let offlinePollTimer = null;
  let offlineStorageUsage = null;
  const offlineDownloadControllers = new Map();
  const offlineDownloadTasks = new Map();
  let offlineCacheGeneration = 0;
  const offlineTabId = window.crypto?.randomUUID?.() || `${Date.now()}-${Math.random()}`;
  const offlineChannel = typeof BroadcastChannel === 'function'
    ? new BroadcastChannel('puffinbox-offline')
    : null;
  let offlineSessionRevision = 0;
  const unratedCategories = [
    ['Movie', 'Movies'], ['Trailer', 'Trailers'], ['Series', 'Series'], ['Music', 'Music'],
    ['Book', 'Books'], ['LiveTvChannel', 'Live TV channels'], ['LiveTvProgram', 'Live TV programs'],
    ['ChannelContent', 'Channel content'], ['Other', 'Other'],
  ];
  const state = {
    startup: null,
    user: null,
    server: null,
    libraries: [],
    views: [],
    users: [],
    items: [],
    itemTotal: 0,
    itemStart: 0,
    itemLimit: 100,
    activeLibrary: null,
    currentFolder: null,
    screen: 'home',
    searchTerm: '',
    showAdmin: false,
    offlineSettings: null,
    offlineServerPackages: [],
    offlineCache: [],
    offlineError: null,
    offlineAccountGeneration: null,
    navigationStack: [],
    parentalRatings: [],
    playlists: [],
    playlistTotal: 0,
    playlistStart: 0,
    selectedPlaylistId: null,
    playlistEntries: [],
    playlistEntriesLoading: false,
    playlistEntriesError: false,
    playlistAudioItems: [],
    playlistAudioTotal: 0,
    playlistAudioStart: 0,
  };
  const palette = [
    ['#284b4e', '#25364a'], ['#534360', '#31314a'], ['#3d5a54', '#263546'],
    ['#6a4c43', '#343247'], ['#3d4e6b', '#303247'], ['#4b5b42', '#273a43'],
  ];

  function escapeHtml(value) {
    return String(value ?? '').replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char]);
  }

  function showToast(message) {
    toast.textContent = message;
    toast.classList.add('show');
    clearTimeout(showToast.timer);
    showToast.timer = setTimeout(() => toast.classList.remove('show'), 3000);
  }

  async function request(path, options = {}) {
    const { timeoutMs = 15_000, signal: callerSignal, ...fetchOptions } = options;
    const headers = new Headers(options.headers || {});
    if (options.body && !headers.has('Content-Type')) headers.set('Content-Type', 'application/json');
    const controller = typeof AbortController === 'undefined' ? null : new AbortController();
    const timer = controller ? setTimeout(() => controller.abort(), timeoutMs) : null;
    const forwardAbort = () => controller?.abort(callerSignal?.reason);
    callerSignal?.addEventListener('abort', forwardAbort, { once: true });
    if (callerSignal?.aborted) forwardAbort();
    try {
      const response = await fetch(path, { ...fetchOptions, headers, credentials: 'same-origin', ...(controller ? { signal: controller.signal } : {}) });
      if (response.status === 204) return null;
      const contentType = response.headers.get('content-type') || '';
      const body = contentType.includes('json') ? await response.json().catch(() => null) : await response.text().catch(() => '');
      if (!response.ok) {
        const message = typeof body === 'string' ? body : (body?.Message || body?.message || body?.error || response.statusText);
        const error = new Error(message || `Request failed (${response.status})`);
        error.status = response.status;
        throw error;
      }
      return body;
    } finally {
      if (timer !== null) clearTimeout(timer);
      callerSignal?.removeEventListener('abort', forwardAbort);
    }
  }

  function json(method, data) {
    return { method, body: JSON.stringify(data) };
  }

  function setTheme(theme) {
    document.documentElement.classList.toggle('light-theme', theme === 'light');
    window.PuffinboxDeviceId.writeSetting(() => window.localStorage, 'puffinbox-theme', theme);
  }

  function greeting() {
    const hour = new Date().getHours();
    return hour < 12 ? 'Good morning' : hour < 18 ? 'Good afternoon' : 'Good evening';
  }

  function initials(name) {
    return String(name || '?').trim().split(/\s+/).slice(0, 2).map((part) => part[0]?.toUpperCase() || '').join('') || '?';
  }

  function formatType(type) {
    const names = { Movie: 'Movie', Series: 'Series', Episode: 'Episode', Audio: 'Music', MusicAlbum: 'Album', MusicArtist: 'Artist', Photo: 'Photo', Book: 'Book', Folder: 'Folder', CollectionFolder: 'Library' };
    return names[type] || String(type || 'Media').replace(/([a-z])([A-Z])/g, '$1 $2');
  }

  function iconFor(type) {
    const symbols = { Movie: '▶', Series: '▤', Episode: '▶', Audio: '♫', MusicAlbum: '♫', MusicArtist: '♫', Photo: '▧', Book: '▣', Folder: '⌂', CollectionFolder: '▦' };
    return symbols[type] || '◈';
  }

  function colorsFor(seed) {
    const total = Array.from(String(seed || '')).reduce((sum, char) => sum + char.charCodeAt(0), 0);
    return palette[total % palette.length];
  }

  function mediaCard(item) {
    const [a, b] = colorsFor(item.Id || item.Name);
    const card = document.createElement('button');
    card.type = 'button';
    card.className = 'media-card';
    card.dataset.itemId = String(item.Id ?? '');
    card.style.setProperty('--tile-a', a);
    card.style.setProperty('--tile-b', b);
    const art = document.createElement('span');
    art.className = 'media-art';
    const symbol = document.createElement('span');
    symbol.className = 'media-symbol';
    symbol.textContent = iconFor(item.Type);
    const type = document.createElement('span');
    type.className = 'media-type';
    type.textContent = formatType(item.Type);
    art.append(symbol, type);
    const title = document.createElement('span');
    title.className = 'media-title';
    title.textContent = item.Name || 'Untitled';
    const meta = document.createElement('span');
    meta.className = 'media-meta';
    meta.textContent = item.ProductionYear || item.Album || item.Overview || formatType(item.Type);
    card.append(art, title, meta);
    return card;
  }

  function mediaGrid(source) {
    return `<div class="media-grid" data-media-grid="${source}"></div>`;
  }

  function fillMediaGrids() {
    app.querySelectorAll('[data-media-grid]').forEach((grid) => {
      const items = grid.dataset.mediaGrid === 'recent'
        ? state.items.filter((item) => !['Folder', 'CollectionFolder'].includes(item.Type)).slice(0, 8)
        : state.items;
      window.PuffinboxClientCompat.replaceChildren(grid, ...items.map(mediaCard));
    });
  }

  function emptyState(title, description, symbol = '＋') {
    return `<div class="empty-state"><span class="empty-icon">${symbol}</span><strong>${escapeHtml(title)}</strong><p>${escapeHtml(description)}</p></div>`;
  }

  function navButton(id, label, icon, active = false, mobile = false) {
    return `<button class="nav-button${active ? ' active' : ''}" data-screen="${id}"${mobile ? ' aria-label="' + escapeHtml(label) + '"' : ''}><span class="nav-icon" aria-hidden="true">${icon}</span><span>${escapeHtml(label)}</span></button>`;
  }

  function navMarkup(mobile = false) {
    const admin = state.user?.Policy?.IsAdministrator || state.user?.IsAdministrator;
    const buttons = [
      navButton('home', 'Home', '⌂', state.screen === 'home' && !mobile, mobile),
      navButton('browse', 'Browse', '▦', state.screen === 'browse' && !mobile, mobile),
      navButton('music', 'Music', '♫', state.screen === 'music' && !mobile, mobile),
      ...(window.PuffinboxLiveTv?.canAccess(state.user) ? [navButton('live-tv', 'Live TV', '◉', state.screen === 'live-tv' && !mobile, mobile)] : []),
      ...(admin ? [navButton('libraries', 'Libraries', '▤', state.screen === 'libraries' && !mobile, mobile), navButton('users', 'People', '♙', state.screen === 'users' && !mobile, mobile)] : []),
      navButton('offline', 'Offline', '⇩', state.screen === 'offline' && !mobile, mobile),
      navButton('settings', 'Settings', '⚙', state.screen === 'settings' && !mobile, mobile),
    ].join('');
    return mobile ? `<nav class="mobile-nav" aria-label="Main navigation">${buttons}</nav>` : `<aside class="sidebar">
      <div class="nav-group"><p class="nav-label">Your space</p>${buttons}</div>
      <div class="sidebar-note"><strong>Built for your home</strong>Media stays on your server. Puffinbox opens the library you choose.</div>
    </aside>`;
  }

  function shell(content) {
    app.innerHTML = `<header class="topbar">
      <a href="#" class="brand-lockup" data-screen="home" aria-label="Puffinbox home"><span class="brand-mark">p</span><span class="brand-name">puffinbox</span></a>
      <div class="topbar-main"><div class="search-wrap"><span class="search-icon" aria-hidden="true">⌕</span><input id="global-search" class="search-input" type="search" placeholder="Search your library…" autocomplete="off" aria-label="Search your library"><div id="search-menu" class="search-menu" role="listbox"></div></div></div>
      <div class="topbar-actions"><button class="icon-button" data-screen="settings" aria-label="Settings">⚙</button><button class="user-chip" id="user-menu" aria-label="Sign out"><span class="avatar">${escapeHtml(initials(state.user?.Name))}</span><span>${escapeHtml(state.user?.Name || 'Account')}</span></button></div>
    </header><div class="layout">${navMarkup()}<main class="main-content">${content}</main></div>${navMarkup(true)}`;
    fillMediaGrids();
    bindShellEvents();
  }

  function pageHeading(title, description, action = '') {
    return `<div class="page-heading"><div><h1>${escapeHtml(title)}</h1><p>${escapeHtml(description)}</p></div>${action}</div>`;
  }

  function listItems(items) {
    return items.map(mediaCard).join('');
  }

  function homeScreen() {
    const action = state.libraries.length ? `<button class="button secondary" data-screen="browse">Browse library</button>` : '';
    const libraryTiles = state.libraries.length ? `<div class="library-grid">${state.libraries.map((library) => {
      const [a, b] = colorsFor(library.Id || library.Name);
      const title = escapeHtml(library.Name || 'Library');
      const id = escapeHtml(library.Id || library.ItemId || '');
      const count = Number.isFinite(library.ChildCount) ? `${library.ChildCount} items` : escapeHtml(formatType(library.CollectionType || 'Collection'));
      return `<button class="library-card" data-library-id="${id}" style="--tile-a:${a};--tile-b:${b}"><span class="library-symbol">${escapeHtml(iconFor(library.Type || 'CollectionFolder'))}</span><strong>${title}</strong><small>${count}</small></button>`;
    }).join('')}</div>` : emptyState('Your library is ready for its first folder', 'Add a library folder to start browsing your own media.', '▤');
    return `${pageHeading(`${greeting()}, ${state.user?.Name || 'there'}`, 'Pick up where you left off or explore something new.', action)}
      <section class="hero-card"><div class="hero-copy"><p class="eyebrow">Your media, together</p><h2>A quieter place for everything you love.</h2><p>Bring your films, music, photos and more into one library, ready when you are.</p><button class="button" data-screen="browse">Explore your library</button></div></section>
      <section><div class="section-heading"><div><h2>Your libraries</h2><p>Collections available to this account</p></div>${state.user?.IsAdministrator || state.user?.Policy?.IsAdministrator ? '<button class="section-link" data-screen="libraries">Manage libraries&nbsp; →</button>' : ''}</div>${libraryTiles}</section>
      <section><div class="section-heading"><div><h2>Recently added</h2><p>Fresh from your collection</p></div></div>${state.items.some((item) => !['Folder', 'CollectionFolder'].includes(item.Type)) ? mediaGrid('recent') : emptyState('Nothing here yet', 'Add media folders to your libraries, then refresh to see their contents.', '✦')}</section>`;
  }

  function browseScreen() {
    const libraryName = state.currentFolder?.Name || state.activeLibrary?.Name || 'All media';
    const parent = state.navigationStack.length ? state.navigationStack[state.navigationStack.length - 1] : null;
    const parentName = parent?.folder?.Name || state.activeLibrary?.Name || (parent?.searchTerm ? 'search results' : 'all media');
    const itemBar = state.currentFolder ? `<button class="button secondary small" id="browse-back">← Back to ${escapeHtml(parentName)}</button>` : '';
    const actions = `${itemBar}${state.activeLibrary && !state.currentFolder ? '' : ''}`;
    const media = state.items;
    const cards = media.length ? mediaGrid('items') : emptyState('No media in this view', 'Choose another library or add media to this folder.', '⌕');
    const pages = Math.max(1, Math.ceil(state.itemTotal / state.itemLimit));
    const page = Math.floor(state.itemStart / state.itemLimit) + 1;
    const paging = state.itemTotal > state.itemLimit ? `<div class="section-heading"><p>Page ${page} of ${pages} · ${state.itemTotal} items</p><div class="data-row-actions"><button class="button secondary small" data-page="prev"${page <= 1 ? ' disabled' : ''}>Previous</button><button class="button secondary small" data-page="next"${page >= pages ? ' disabled' : ''}>Next</button></div></div>` : '';
    return `${pageHeading(libraryName, state.searchTerm ? `Search results for “${state.searchTerm}”` : 'Browse movies, shows, music, photos and more.', actions)}
      ${state.libraries.length ? `<div class="library-grid">${state.libraries.map((library) => {
        const [a, b] = colorsFor(library.Id || library.Name);
        return `<button class="library-card" data-library-id="${escapeHtml(library.Id || library.ItemId || '')}" style="--tile-a:${a};--tile-b:${b}"><span class="library-symbol">▦</span><strong>${escapeHtml(library.Name)}</strong><small>${escapeHtml(formatType(library.CollectionType || 'Collection'))}</small></button>`;
      }).join('')}</div>` : ''}
      <div class="section-heading"><div><h2>${state.searchTerm ? 'Results' : state.currentFolder ? 'In this folder' : 'All media'}</h2><p>${state.itemTotal} item${state.itemTotal === 1 ? '' : 's'}</p></div></div>
      ${cards}${paging}`;
  }

  function isAudioItem(item) {
    return item?.Type === 'Audio' || /^audio$/i.test(String(item?.MediaType || ''));
  }

  function musicScreen() {
    const playlists = state.playlists.length ? `<div class="playlist-picker" role="group" aria-label="Your playlists">${state.playlists.map((playlist) => {
      const id = String(playlist.Id || '');
      const selected = id === String(state.selectedPlaylistId || '');
      return `<button class="button small${selected ? '' : ' secondary'}" type="button" data-select-playlist="${escapeHtml(id)}" aria-pressed="${selected}">${escapeHtml(playlist.Name || 'Untitled playlist')}</button>`;
    }).join('')}</div>` : emptyState('No playlists yet', 'Create a playlist to collect audio from libraries you can access.', '♫');
    const playlistPages = state.playlistTotal > 100 ? `<div class="section-heading playlist-pagination"><p>Playlists ${state.playlistStart + 1}–${Math.min(state.playlistStart + state.playlists.length, state.playlistTotal)} of ${state.playlistTotal}</p><div class="data-row-actions"><button class="button secondary small" data-playlist-page="prev"${state.playlistStart <= 0 ? ' disabled' : ''}>Previous</button><button class="button secondary small" data-playlist-page="next"${state.playlistStart + 100 >= state.playlistTotal ? ' disabled' : ''}>Next</button></div></div>` : '';
    const selectedPlaylist = state.playlists.find((playlist) => String(playlist.Id) === String(state.selectedPlaylistId));
    const tracks = state.playlistEntriesLoading
      ? '<div class="loading-row" aria-live="polite">Loading playlist…</div>'
      : state.playlistEntriesError
        ? emptyState('Playlist could not be loaded', 'Select it again to retry.', '!')
        : state.playlistEntries.length ? `<div class="data-list">${state.playlistEntries.map((entry, index) => {
      const entryId = String(entry.PlaylistItemId || '');
      const actions = `<div class="data-row-actions"><button class="button secondary small" type="button" data-move-playlist-entry="up" data-entry-id="${escapeHtml(entryId)}"${index === 0 ? ' disabled' : ''} aria-label="Move ${escapeHtml(entry.Name || 'track')} up">↑</button><button class="button secondary small" type="button" data-move-playlist-entry="down" data-entry-id="${escapeHtml(entryId)}"${index === state.playlistEntries.length - 1 ? ' disabled' : ''} aria-label="Move ${escapeHtml(entry.Name || 'track')} down">↓</button><button class="button danger small" type="button" data-remove-playlist-entry="${escapeHtml(entryId)}">Remove</button></div>`;
      return `<div class="data-row playlist-track-row"><div class="playlist-track-position" aria-hidden="true">${index + 1}</div><div class="data-row-main"><strong>${escapeHtml(entry.Name || 'Untitled track')}</strong><small>${escapeHtml(entry.Album || entry.Container || 'Audio')}</small></div>${actions}</div>`;
    }).join('')}</div>` : emptyState('This playlist is empty', 'Add an audio item from the list below to get started.', '♫');
    const canPlayback = state.user?.Policy?.EnableMediaPlayback !== false;
    const trackSummary = state.playlistEntriesLoading ? 'Loading tracks…' : state.playlistEntriesError ? 'Tracks unavailable' : `${state.playlistEntries.length} track${state.playlistEntries.length === 1 ? '' : 's'}`;
    const playlistPanel = selectedPlaylist ? `<section class="panel playlist-panel"><div class="panel-title"><div><strong>${escapeHtml(selectedPlaylist.Name || 'Playlist')}</strong><p>${trackSummary} · Private to your account</p></div><button class="button small" id="play-playlist" type="button"${!canPlayback || !state.playlistEntries.length || state.playlistEntriesLoading || state.playlistEntriesError ? ' disabled' : ''}>Play playlist</button></div>${tracks}</section>` : `<section class="panel playlist-panel">${emptyState('Choose a playlist', 'Create a playlist or select one above before adding audio.', '♫')}</section>`;
    const audioRows = state.playlistAudioItems.length ? `<div class="data-list">${state.playlistAudioItems.map((item) => `<div class="data-row playlist-audio-row"><div class="data-row-main"><strong>${escapeHtml(item.Name || 'Untitled track')}</strong><small>${escapeHtml(item.Album || item.AlbumArtist || (Array.isArray(item.Artists) ? item.Artists.join(', ') : '') || 'Audio')}</small></div><div class="data-row-actions"><button class="button secondary small" type="button" data-add-playlist-item="${escapeHtml(item.Id)}" aria-label="Add ${escapeHtml(item.Name || 'track')} to selected playlist"${!selectedPlaylist || !canPlayback ? ' disabled' : ''}>Add</button></div></div>`).join('')}</div>` : emptyState('No audio in this view', 'Audio items you can access will appear here.', '♫');
    const audioPages = state.playlistAudioTotal > 100 ? `<div class="section-heading playlist-pagination"><p>Audio ${state.playlistAudioStart + 1}–${Math.min(state.playlistAudioStart + state.playlistAudioItems.length, state.playlistAudioTotal)} of ${state.playlistAudioTotal}</p><div class="data-row-actions"><button class="button secondary small" data-audio-page="prev"${state.playlistAudioStart <= 0 ? ' disabled' : ''}>Previous</button><button class="button secondary small" data-audio-page="next"${state.playlistAudioStart + 100 >= state.playlistAudioTotal ? ' disabled' : ''}>Next</button></div></div>` : '';
    const playbackNote = canPlayback ? '' : '<p class="form-hint">This account does not have permission to play media.</p>';
    return `${pageHeading('Music', 'Create private audio playlists from items available to your account.')}
      <section class="panel playlist-create-panel"><div class="panel-title"><div>Your playlists<p>Only you can view and manage these playlists.</p></div></div><form id="playlist-create-form" class="playlist-create-form"><label class="form-field">Playlist name<input name="Name" required maxlength="255" autocomplete="off" placeholder="Late night listening"></label><button class="button" type="submit">Create playlist</button></form>${playlists}${playlistPages}</section>
      ${playlistPanel}
      <section class="panel playlist-audio-panel"><div class="panel-title"><div>Audio you can access<p>Add individual tracks to the selected playlist.</p></div></div>${playbackNote}${audioRows}${audioPages}</section>`;
  }

  function libraryScreen() {
    const rows = state.libraries.length ? `<div class="data-list">${state.libraries.map((library) => `<div class="data-row"><div class="data-row-main"><strong>${escapeHtml(library.Name)}</strong><small>${escapeHtml((library.Locations || []).join(' · ') || formatType(library.CollectionType || 'Library'))}</small></div><div class="data-row-actions"><span class="pill">${escapeHtml(formatType(library.CollectionType || 'Mixed'))}</span><button class="button danger small" data-delete-library="${escapeHtml(library.Name)}">Remove</button></div></div>`).join('')}</div>` : emptyState('No libraries yet', 'Add a folder that contains media. Puffinbox scans the location and makes its contents available to permitted users.', '▤');
    return `${pageHeading('Libraries', 'Choose the folders Puffinbox makes available on this server.')}
      <section class="panel"><div class="panel-title"><div>Library scan<p>Refresh existing folders and check scan progress.</p></div><button id="refresh-libraries" class="button secondary small" type="button">Refresh now</button></div><div id="scan-status-list" class="data-list" aria-live="polite"><span class="subtle">Loading scan status…</span></div></section>
      <section class="panel"><div class="panel-title"><div>Add a library folder<p>Use an absolute path that the server process can read.</p></div></div>
        <form id="library-form"><div class="form-row"><label class="form-field">Library name<input name="Name" required maxlength="80" placeholder="Films"></label><label class="form-field">Collection type<select name="CollectionType"><option value="movies">Movies</option><option value="tvshows">TV shows</option><option value="music">Music</option><option value="photos">Photos</option><option value="books">Books</option><option value="homevideos">Home videos</option></select></label></div>
          <label class="form-field" style="margin-top:13px">Folder path<input name="Location" required placeholder="/srv/media/films" autocomplete="off"></label><div class="form-actions"><button class="button" type="submit">Add library</button></div></form></section>
      <section class="panel"><div class="panel-title"><div>Available libraries<p>Removing a library does not delete files from disk.</p></div></div>${rows}</section>`;
  }

  function userScreen() {
    const rows = state.users.length ? `<div class="data-list">${state.users.map((user) => {
      const access = user.Policy?.EnableAllFolders === true ? 'All libraries' : `${(user.Policy?.EnabledFolders || []).length} selected libraries`;
      const blockedCategories = Array.isArray(user.Policy?.BlockUnratedItems) ? user.Policy.BlockUnratedItems : [];
      const unrated = blockedCategories.length
        ? ` · unrated content blocked for ${blockedCategories.length} ${blockedCategories.length === 1 ? 'category' : 'categories'}`
        : '';
      return `<div class="data-row"><div class="data-row-main"><strong>${escapeHtml(user.Name)} ${user.IsAdministrator ? '<span class="pill good">Administrator</span>' : ''}</strong><small>${user.Policy?.EnableMediaPlayback === false ? 'Playback disabled' : 'Playback enabled'} · ${user.Policy?.EnableRemoteAccess === false ? 'Local access' : 'Remote access enabled'} · ${escapeHtml(access)}${unrated}</small></div><div class="data-row-actions"><button class="button secondary small" data-edit-user="${escapeHtml(user.Id)}">Manage</button>${user.Id !== state.user.Id ? `<button class="button danger small" data-delete-user="${escapeHtml(user.Id)}">Remove</button>` : ''}</div></div>`;
    }).join('')}</div>` : emptyState('No other accounts', 'Create an account for each person who uses this server.', '♙');
    return `${pageHeading('People', 'Create accounts and choose what each person can access.')}
      <section class="panel"><div class="panel-title"><div id="user-form-title">Add a person<p id="user-form-caption">Each account has its own sign-in and library access.</p></div></div>
      <form id="user-form"><input type="hidden" name="Id"><div class="form-row"><label class="form-field">Name<input name="Name" required maxlength="80" autocomplete="off" placeholder="Alex"></label><label class="form-field" id="user-password-field">Password<input name="Password" type="password" required minlength="12" maxlength="1024" autocomplete="new-password" placeholder="At least 12 UTF-8 bytes"><span class="form-hint">At least 12 UTF-8 bytes.</span></label></div>
      <div class="check-row" style="margin-top:15px"><label><input name="IsAdministrator" type="checkbox"> Administrator</label><label><input name="EnableRemoteAccess" type="checkbox"> Allow remote access</label><label><input name="EnableMediaPlayback" type="checkbox" checked> Allow media playback</label><label><input name="EnableContentDownloading" type="checkbox"> Allow offline downloads</label></div>
      <label class="form-field" style="margin-top:15px">Maximum parental rating<select name="MaxParentalRating" class="inline-input">${parentalRatingOptions()}</select><span class="form-hint">US-MPAA-v1 values are Puffinbox ordinal thresholds, not ages. Unknown labels and other rating systems are unrated and can be controlled below.</span></label>
      <fieldset class="form-field policy-categories"><legend>Block unrated content by type</legend><div class="check-row">${unratedCategoryChoices()}</div><p class="form-hint">Choose which content types should be hidden when no rating is available.</p></fieldset>
      <div class="form-field" style="margin-top:15px">Library access<div class="check-row"><label><input name="EnableAllFolders" type="checkbox"> Allow all libraries</label></div><div id="library-access" class="check-row">${libraryAccessChoices()}</div><p class="form-hint">When “Allow all libraries” is off, the account can access only selected libraries. No selection means no library access.</p></div>
      <div class="form-actions"><button class="button secondary" id="cancel-user-edit" type="button" hidden>Cancel</button><button class="button" type="submit" id="save-user-button">Create account</button></div></form></section>
      <section class="panel"><div class="panel-title"><div>Accounts<p>${state.users.length} account${state.users.length === 1 ? '' : 's'} on this server</p></div></div>${rows}</section>`;
  }

  function libraryAccessChoices() {
    if (!state.libraries.length) return '<span class="subtle">No libraries are configured yet.</span>';
    return state.libraries.map((library) => `<label><input type="checkbox" name="AllowedLibraryIds" value="${escapeHtml(library.Id || library.ItemId)}"> ${escapeHtml(library.Name)}</label>`).join('');
  }

  function parentalRatingOptions() {
    const supported = state.parentalRatings.filter((rating) => String(rating.Name || '').startsWith('US-MPAA-v1:')
      && Number.isInteger(Number(rating.Value)) && Number(rating.Value) >= 0 && Number(rating.Value) <= 100);
    return `<option value="">No limit</option>${supported.map((rating) => `<option value="${Number(rating.Value)}">${escapeHtml(rating.Name)}</option>`).join('')}`;
  }

  function unratedCategoryChoices() {
    return unratedCategories.map(([value, label]) => `<label><input type="checkbox" name="BlockUnratedItems" value="${value}"> ${label}</label>`).join('');
  }

  function settingsScreen() {
    const serverName = state.server?.ServerName || state.startup?.ServerName || 'Puffinbox';
    const version = state.server?.Version || 'Development build';
    const count = state.server?.ItemCount ?? state.items.length;
    const theme = window.PuffinboxDeviceId.readSetting(() => window.localStorage, 'puffinbox-theme', 'dark');
    return `${pageHeading('Settings', 'Your account and this server.')}
      <section class="panel"><div class="panel-title"><div>Server<p>Connection details reported by the server API.</p></div></div><div class="settings-grid"><div class="setting-card"><span class="setting-label">Server name</span><strong>${escapeHtml(serverName)}</strong></div><div class="setting-card"><span class="setting-label">Server version</span><strong>${escapeHtml(version)}</strong></div><div class="setting-card"><span class="setting-label">Signed in as</span><strong>${escapeHtml(state.user?.Name || 'Unknown')}</strong></div><div class="setting-card"><span class="setting-label">Libraries available</span><strong>${state.libraries.length}</strong></div></div></section>
      <section class="panel"><div class="panel-title"><div>Appearance<p>Saved on this device.</p></div></div><div class="theme-switch"><p>Color theme</p><select id="theme-select" class="inline-input" style="max-width:190px"><option value="dark"${theme === 'dark' ? ' selected' : ''}>Dark</option><option value="light"${theme === 'light' ? ' selected' : ''}>Light</option></select></div></section>
      <section class="panel"><div class="panel-title"><div>Account<p>Sign out from this browser.</p></div><button class="button secondary" id="signout-button">Sign out</button></div></section>`;
  }

  function formatBytes(value) {
    const bytes = Number(value);
    if (!Number.isFinite(bytes) || bytes < 0) return 'Unknown size';
    if (bytes < 1024) return `${bytes} B`;
    const units = ['KiB', 'MiB', 'GiB', 'TiB'];
    let size = bytes;
    let unit = -1;
    do { size /= 1024; unit += 1; } while (size >= 1024 && unit < units.length - 1);
    return `${size.toFixed(size >= 100 ? 0 : size >= 10 ? 1 : 2)} ${units[unit]}`;
  }

  function offlineScreen() {
    const serverItems = Array.isArray(state.offlineServerPackages) ? state.offlineServerPackages : [];
    const cached = Array.isArray(state.offlineCache) ? state.offlineCache : [];
    const cachedById = new Map(cached.map((entry) => [String(entry.packageId), entry]));
    const seen = new Set();
    const entries = serverItems.map((entry) => {
      const id = String(entry.Id || entry.id || '');
      seen.add(id);
      return { server: entry, local: cachedById.get(id) || null, id };
    });
    for (const local of cached) {
      if (!seen.has(String(local.packageId))) entries.push({ server: null, local, id: String(local.packageId) });
    }
    const policyAllows = state.user?.Policy?.EnableContentDownloading === true;
    const settings = state.offlineSettings;
    const capacity = settings
      ? `${formatBytes(settings.UsedBytes)} used · ${formatBytes(settings.QuotaBytes)} account limit · ${formatBytes(settings.ReservedBytes)} reserved`
      : 'Server storage details are unavailable while disconnected.';
    const rows = entries.length ? `<div class="data-list">${entries.map(({ server, local, id }) => {
      const name = local?.itemName || server?.ItemName || 'Offline media';
      const type = local?.itemType || server?.ItemType || 'Media';
      const size = local?.sourceSize ?? server?.SourceSize ?? 0;
      const serverStatus = String(server?.Status || 'unavailable').toLowerCase();
      const localStatus = String(local?.status || 'not-downloaded').toLowerCase();
      const status = localStatus === 'complete' ? 'Stored in this browser' : `Server: ${serverStatus}${localStatus === 'paused' ? ' · browser copy paused' : ''}`;
      const progress = local?.bytesCopied ? `${formatBytes(local.bytesCopied)} of ${formatBytes(size)}` : `${formatBytes(size)}${server?.ChunkCount ? ` · ${server.ChunkCount} chunks` : ''}`;
      const open = local?.status === 'complete' ? `<a class="button small" data-open-offline="${escapeHtml(local.packageId)}" href="${escapeHtml(offlineUrl(state.user?.Id, local.urlToken))}" target="${offlineLinkTarget()}" rel="noopener">Open offline</a>` : '';
      const transfer = server && serverStatus === 'ready' && localStatus !== 'complete' && policyAllows
        ? `<button class="button secondary small" data-download-offline="${escapeHtml(id)}">${localStatus === 'paused' || localStatus === 'downloading' ? 'Resume in this browser' : 'Save in this browser'}</button>` : '';
      const cancel = offlineDownloadControllers.has(id) ? `<button class="button secondary small" data-pause-offline="${escapeHtml(id)}">Pause</button>` : '';
      const removeLocal = local ? `<button class="button danger small" data-remove-local="${escapeHtml(local.packageId)}">Remove browser copy</button>` : '';
      const removeServer = server ? `<button class="button danger small" data-remove-server="${escapeHtml(id)}">Remove server package</button>` : '';
      const error = local?.errorCode ? ` · ${escapeHtml(offlineErrorMessage(local.errorCode))}` : '';
      return `<div class="data-row offline-row"><div class="data-row-main"><strong>${escapeHtml(name)}</strong><small>${escapeHtml(formatType(type))} · ${escapeHtml(status)} · <span data-offline-progress="${escapeHtml(id)}">${escapeHtml(progress)}</span>${error}</small></div><div class="data-row-actions">${open}${transfer}${cancel}${removeLocal}${removeServer}</div></div>`;
    }).join('')}</div>` : emptyState('No offline copies yet', 'Open a media item and choose “Prepare offline” to create a server package, then save it in this browser.', '⇩');
    const disabledReason = !policyAllows
      ? '<p class="form-hint">This account cannot download content for offline use. Ask an administrator to enable content downloading.</p>'
      : '';
    const failure = state.offlineError ? `<div class="error-banner offline-error">${escapeHtml(state.offlineError)}</div>` : '';
    const deviceUse = offlineStorageUsage && offlineStorageUsage.quota
      ? `${formatBytes(offlineStorageUsage.usage)} of approximately ${formatBytes(offlineStorageUsage.quota)} used by this site in this browser.`
      : 'This browser does not report an estimate for this site’s storage.';
    return `${pageHeading('Offline', 'Keep selected media in this browser for times without a connection.', '<button class="button secondary small" id="refresh-offline" type="button">Refresh</button>')}
      <section class="panel"><div class="panel-title"><div>Browser storage and access<p>${escapeHtml(capacity)} · ${escapeHtml(deviceUse)}</p></div><span class="pill${policyAllows ? ' good' : ''}">${policyAllows ? 'Downloads allowed' : 'Downloads disabled'}</span></div>${disabledReason}${failure}<p class="form-hint">Copies are account-specific and stay in this browser’s storage for this site. They are cleared when you sign out. Each server chunk is checked against SHA-256 and the complete file is checked before it can be opened. Browser settings or storage pressure may remove local data.</p></section>
      <section class="panel"><div class="panel-title"><div>Offline packages<p>${entries.length} package${entries.length === 1 ? '' : 's'} available to this account</p></div></div>${rows}</section>`;
  }

  function offlineUrl(accountId, token) {
    if (!accountId || !token) return '#';
    return `/web/__offline/${encodeURIComponent(String(accountId))}/${encodeURIComponent(String(token))}`;
  }

  function offlineLinkTarget() {
    return window.jmpInfo ? '_self' : '_blank';
  }

  function offlinePlaybackContentType(entry) {
    const contentType = String(entry.contentType || '').split(';', 1)[0].trim().toLowerCase();
    const inlineTypes = new Set([
      'video/mp4', 'video/webm', 'video/x-matroska', 'video/quicktime',
      'audio/mpeg', 'audio/mp4', 'audio/aac', 'audio/flac', 'audio/wav', 'audio/ogg', 'audio/webm',
      'image/jpeg', 'image/png', 'image/gif', 'image/webp', 'image/avif', 'image/bmp',
    ]);
    if (inlineTypes.has(contentType)) return contentType;
    const extension = String(entry.fileName || entry.itemName || '').split('.').pop().toLowerCase();
    const byExtension = {
      mp4: 'video/mp4', m4v: 'video/mp4', webm: 'video/webm', mkv: 'video/x-matroska', mov: 'video/quicktime',
      mp3: 'audio/mpeg', m4a: 'audio/mp4', aac: 'audio/aac', flac: 'audio/flac', wav: 'audio/wav', ogg: 'audio/ogg', opus: 'audio/ogg',
      jpg: 'image/jpeg', jpeg: 'image/jpeg', png: 'image/png', gif: 'image/gif', webp: 'image/webp', avif: 'image/avif', bmp: 'image/bmp',
    };
    return byExtension[extension] || 'application/octet-stream';
  }

  async function createVerifiedOfflineBlob(entry, accountId, generation) {
    const cache = window.PuffinboxOfflineCache;
    const size = Number(entry.sourceSize);
    const chunkSize = Number(entry.chunkSize);
    const chunkCount = Number(entry.chunkCount);
    if (!Number.isSafeInteger(size) || size < 1 || size > MAX_EMBEDDED_OFFLINE_PLAYBACK_BYTES) {
      throw new Error(`This embedded player can open verified offline files up to ${formatBytes(MAX_EMBEDDED_OFFLINE_PLAYBACK_BYTES)}. For a larger file, save a copy in a browser that supports offline streaming.`);
    }
    if (!Number.isSafeInteger(chunkSize) || chunkSize < 1 || chunkSize > cache.CHUNK_SIZE
        || !Number.isSafeInteger(chunkCount) || chunkCount !== Math.ceil(size / chunkSize)) {
      throw new Error('The saved offline copy has invalid chunk metadata.');
    }
    const parts = [];
    const completeHash = new cache.Sha256();
    let verifiedSize = 0;
    for (let index = 0; index < chunkCount; index += 1) {
      const row = await cache.getChunkForActiveAccount(accountId, entry.packageId, index, generation, entry.transferToken);
      if (!row) throw new Error('A verified offline chunk is missing or belongs to another account.');
      const bytes = new Uint8Array(row.bytes);
      const expectedLength = Math.min(chunkSize, size - verifiedSize);
      const digest = new cache.Sha256().update(bytes).digestHex();
      if (bytes.byteLength !== expectedLength || digest !== String(row.digest).toLowerCase()) {
        throw new Error(`Offline chunk ${index + 1} failed its local SHA-256 check.`);
      }
      completeHash.update(bytes);
      parts.push(row.bytes);
      verifiedSize += bytes.byteLength;
    }
    if (verifiedSize !== size || completeHash.digestHex() !== String(entry.sha256 || '').toLowerCase()) {
      throw new Error('The saved offline file failed its complete SHA-256 check.');
    }
    return new Blob(parts, { type: offlinePlaybackContentType(entry) });
  }

  async function playOfflinePackageInEmbeddedPlayer(packageId) {
    const generation = ++offlinePlaybackGeneration;
    const note = document.querySelector('#player-note');
    const playerStage = document.querySelector('#player-stage');
    if (!playerDialog.open) playerDialog.showModal();
    setNativePlayerSurface(false);
    nativePlayerControls.hidden = true;
    audioTrackControl.hidden = true;
    subtitleTrackControl.hidden = true;
    playerOptions.hidden = true;
    playerResumeControls.hidden = true;
    document.querySelector('#player-title').textContent = 'Offline media';
    note.textContent = 'Verifying the saved copy…';
    window.PuffinboxClientCompat.replaceChildren(playerStage);
    try {
      const cache = window.PuffinboxOfflineCache;
      const active = await cache.getActiveSession();
      const accountId = String(state.user?.Id || active?.accountId || '');
      if (!accountId || active?.accountId !== accountId || !active.generation) throw new Error('Sign in to the account that owns this offline copy.');
      const entry = await cache.getPackage(accountId, packageId);
      if (!entry || entry.status !== 'complete' || !entry.transferToken) throw new Error('This verified offline copy is no longer available. Refresh the offline list.');
      const blob = await createVerifiedOfflineBlob(entry, accountId, active.generation);
      if (generation !== offlinePlaybackGeneration || !playerDialog.open) return;
      if (offlinePlaybackObjectUrl) URL.revokeObjectURL(offlinePlaybackObjectUrl);
      offlinePlaybackObjectUrl = URL.createObjectURL(blob);
      const contentType = offlinePlaybackContentType(entry);
      const isImage = contentType.startsWith('image/');
      const isAudio = contentType.startsWith('audio/') || ['Audio', 'AudioBook'].includes(entry.itemType);
      const title = String(entry.itemName || entry.fileName || 'Offline media');
      document.querySelector('#player-title').textContent = title;
      if (isImage) {
        const image = document.createElement('img');
        image.src = offlinePlaybackObjectUrl;
        image.alt = title;
        image.style.maxWidth = '100%';
        image.style.maxHeight = '68vh';
        window.PuffinboxClientCompat.replaceChildren(playerStage, image);
        note.textContent = 'Showing the verified copy stored in this player.';
        return;
      }
      if (['Book', 'EBook'].includes(entry.itemType) && contentType === 'application/octet-stream') {
        const download = document.createElement('a');
        download.href = offlinePlaybackObjectUrl;
        download.download = String(entry.fileName || title).replace(/[\\/\r\n]/g, '_');
        download.textContent = `Save ${title}`;
        download.className = 'button';
        window.PuffinboxClientCompat.replaceChildren(playerStage, download);
        note.textContent = 'The verified book copy is ready to save from this player.';
        download.click();
        return;
      }
      const media = document.createElement(isAudio ? 'audio' : 'video');
      media.controls = true;
      media.autoplay = true;
      media.playsInline = true;
      media.preload = 'auto';
      media.src = offlinePlaybackObjectUrl;
      media.addEventListener('error', () => {
        if (generation === offlinePlaybackGeneration && playerDialog.open) note.textContent = 'This embedded player could not decode the saved media format.';
      }, { once: true });
      window.PuffinboxClientCompat.replaceChildren(playerStage, media);
      note.textContent = 'Playing the verified copy stored in this player.';
      const playPromise = media.play();
      if (playPromise) playPromise.catch(() => {
        if (generation === offlinePlaybackGeneration && playerDialog.open) note.textContent = 'Press play to start the verified offline copy.';
      });
    } catch (error) {
      if (generation !== offlinePlaybackGeneration || !playerDialog.open) return;
      note.textContent = error?.message || 'The saved offline copy could not be opened.';
      showToast(note.textContent);
    }
  }

  function offlineErrorMessage(code) {
    const messages = {
      'storage-quota': 'not enough browser storage',
      'storage-unavailable': 'browser storage is unavailable',
      'package-changed': 'server file changed; refresh the package',
      'integrity-failed': 'integrity check failed',
      'network-interrupted': 'transfer paused',
    };
    return messages[String(code)] || String(code);
  }

  function bindShellEvents() {
    app.querySelectorAll('[data-screen]').forEach((button) => button.addEventListener('click', () => openScreen(button.dataset.screen)));
    window.PuffinboxLiveTv?.bind(app, request, () => {
      if (state.screen === 'live-tv') void render();
    }, showToast, showError, (channel) => {
      if (window.PuffinboxLiveTv?.canPlayLiveTv(state.user) && channel?.Type === 'LiveTvChannel') {
        return playItem(channel, { offerResume: false });
      }
    });
    app.querySelectorAll('[data-library-id]').forEach((button) => button.addEventListener('click', () => openLibrary(button.dataset.libraryId)));
    app.querySelectorAll('[data-item-id]').forEach((button) => button.addEventListener('click', () => openItem(button.dataset.itemId)));
    app.querySelector('#user-menu')?.addEventListener('click', logout);
    const searchInput = app.querySelector('#global-search');
    const menu = app.querySelector('#search-menu');
    let timer;
    searchInput?.addEventListener('input', () => {
      clearTimeout(timer);
      const term = searchInput.value.trim();
      if (term.length < 2) { menu.classList.remove('open'); menu.innerHTML = ''; return; }
      timer = setTimeout(() => searchHints(term), 220);
    });
    searchInput?.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') { event.preventDefault(); runSearch(searchInput.value.trim()); }
      if (event.key === 'Escape') { menu.classList.remove('open'); searchInput.blur(); }
    });
    menu?.addEventListener('click', (event) => {
      const button = event.target.closest('[data-search-term]');
      if (button) runSearch(button.dataset.searchTerm);
    });
    app.querySelector('#library-form')?.addEventListener('submit', addLibrary);
    app.querySelector('#refresh-libraries')?.addEventListener('click', refreshLibraries);
    app.querySelectorAll('[data-delete-library]').forEach((button) => button.addEventListener('click', () => removeLibrary(button.dataset.deleteLibrary)));
    app.querySelector('#user-form')?.addEventListener('submit', saveUser);
    app.querySelector('[name="EnableAllFolders"]')?.addEventListener('change', (event) => {
      app.querySelectorAll('[name="AllowedLibraryIds"]').forEach((checkbox) => { checkbox.disabled = event.target.checked; });
    });
    app.querySelector('[name="IsAdministrator"]')?.addEventListener('change', (event) => {
      if (event.target.checked) {
        const allLibraries = app.querySelector('[name="EnableAllFolders"]');
        allLibraries.checked = true;
        allLibraries.dispatchEvent(new Event('change'));
      }
    });
    app.querySelector('#cancel-user-edit')?.addEventListener('click', () => openScreen('users'));
    app.querySelectorAll('[data-edit-user]').forEach((button) => button.addEventListener('click', () => editUser(button.dataset.editUser)));
    app.querySelectorAll('[data-delete-user]').forEach((button) => button.addEventListener('click', () => removeUser(button.dataset.deleteUser)));
    app.querySelector('#browse-back')?.addEventListener('click', () => { void restoreBrowseParent().catch(showError); });
    app.querySelectorAll('[data-page]').forEach((button) => button.addEventListener('click', () => changePage(button.dataset.page)));
    app.querySelector('#playlist-create-form')?.addEventListener('submit', createPlaylist);
    app.querySelectorAll('[data-select-playlist]').forEach((button) => button.addEventListener('click', () => { void selectPlaylist(button.dataset.selectPlaylist); }));
    app.querySelectorAll('[data-add-playlist-item]').forEach((button) => button.addEventListener('click', () => { void addPlaylistItem(button.dataset.addPlaylistItem); }));
    app.querySelectorAll('[data-move-playlist-entry]').forEach((button) => button.addEventListener('click', () => {
      void movePlaylistEntry(button.dataset.entryId, button.dataset.movePlaylistEntry === 'up' ? -1 : 1);
    }));
    app.querySelectorAll('[data-remove-playlist-entry]').forEach((button) => button.addEventListener('click', () => { void removePlaylistEntry(button.dataset.removePlaylistEntry); }));
    app.querySelector('#play-playlist')?.addEventListener('click', playPlaylist);
    app.querySelectorAll('[data-playlist-page]').forEach((button) => button.addEventListener('click', () => {
      void loadMusicPlaylistPage(state.playlistStart + (button.dataset.playlistPage === 'next' ? 100 : -100));
    }));
    app.querySelectorAll('[data-audio-page]').forEach((button) => button.addEventListener('click', () => {
      void loadMusicAudioPage(state.playlistAudioStart + (button.dataset.audioPage === 'next' ? 100 : -100));
    }));
    app.querySelector('#theme-select')?.addEventListener('change', (event) => setTheme(event.target.value));
    app.querySelector('#signout-button')?.addEventListener('click', logout);
    app.querySelector('#refresh-offline')?.addEventListener('click', () => { void loadOfflineData(true); });
    app.querySelectorAll('[data-download-offline]').forEach((button) => button.addEventListener('click', () => { void downloadOfflinePackage(button.dataset.downloadOffline); }));
    app.querySelectorAll('[data-pause-offline]').forEach((button) => button.addEventListener('click', () => offlineDownloadControllers.get(button.dataset.pauseOffline)?.abort()));
    app.querySelectorAll('[data-remove-local]').forEach((button) => button.addEventListener('click', () => { void removeOfflineLocal(button.dataset.removeLocal); }));
    app.querySelectorAll('[data-remove-server]').forEach((button) => button.addEventListener('click', () => { void removeOfflineServer(button.dataset.removeServer); }));
    app.querySelectorAll('[data-open-offline]').forEach((link) => link.addEventListener('click', (event) => {
      if (window.jmpInfo) {
        event.preventDefault();
        void playOfflinePackageInEmbeddedPlayer(link.dataset.openOffline);
        return;
      }
      if (navigator.serviceWorker?.controller?.scriptURL === new URL(OFFLINE_WORKER_URL, location.href).href) return;
      event.preventDefault();
      void ensureOfflineServiceWorker().then((ready) => {
        if (ready) window.location.assign(link.href);
        else showToast('Open this copy once while connected so this browser can prepare offline access.');
      });
    }));
  }

  audioTrackSelect.addEventListener('change', changePlaybackTracks);
  subtitleTrackSelect.addEventListener('change', changePlaybackTracks);
  playerResumeButton.addEventListener('click', () => {
    const pending = pendingPlayback;
    if (!pending) return;
    pendingPlayback = null;
    void playItem(pending.item, {
      ...selectedTrackOptions(),
      startTimeTicks: pending.resumeTicks,
      offerResume: false,
    });
  });
  playerStartOverButton.addEventListener('click', () => {
    const pending = pendingPlayback;
    if (!pending) return;
    pendingPlayback = null;
    void playItem(pending.item, { offerResume: false });
  });

  function showError(error) {
    console.error(error);
    showToast(error?.message || 'Something went wrong. Please try again.');
  }

  async function loadBaseData() {
    const offlineCache = window.PuffinboxOfflineCache;
    const observedOfflineSessionRevision = offlineSessionRevision;
    const previousOfflineSession = offlineCache
      ? await offlineCache.getActiveSession().catch(() => null)
      : null;
    const [me, server, views, libraries, parentalRatings] = await Promise.all([
      request('/Users/Me'),
      request('/System/Info').catch(() => ({})),
      request('/UserViews').catch(() => ({ Items: [] })),
      request('/Library/VirtualFolders').catch(() => []),
      request('/Localization/ParentalRatings').catch(() => []),
    ]);
    let offlineGeneration = null;
    if (me?.Id && offlineCache) {
      offlineGeneration = await offlineCache.setActiveAccount(me.Id, previousOfflineSession?.generation || null)
        .catch((error) => { console.debug('Offline account isolation unavailable', error); return null; });
      let currentOfflineSession = await offlineCache.getActiveSession().catch(() => null);
      if (!offlineGeneration && offlineSessionRevision !== observedOfflineSessionRevision
          && currentOfflineSession?.accountId === String(me.Id) && currentOfflineSession.generation) {
        offlineGeneration = await offlineCache.setActiveAccount(me.Id, currentOfflineSession.generation)
          .catch((error) => { console.debug('Offline account isolation retry unavailable', error); return null; });
        currentOfflineSession = await offlineCache.getActiveSession().catch(() => null);
      }
      if (currentOfflineSession?.generation && currentOfflineSession.accountId !== String(me.Id)) {
        const error = new Error('The active browser account changed while this page was opening. Sign in again.');
        error.name = 'OfflineAccountChangedError';
        throw error;
      }
      if (offlineGeneration && (currentOfflineSession?.accountId !== String(me.Id)
          || currentOfflineSession.generation !== offlineGeneration)) {
        const error = new Error('The active browser account changed while this page was opening. Sign in again.');
        error.name = 'OfflineAccountChangedError';
        throw error;
      }
      if (offlineSessionRevision !== observedOfflineSessionRevision
          && currentOfflineSession?.generation && currentOfflineSession.accountId !== String(me.Id)) {
        const error = new Error('The active browser account changed while this page was opening. Sign in again.');
        error.name = 'OfflineAccountChangedError';
        throw error;
      }
    }
    state.user = me;
    if (window.jmpInfo) {
      await restoreMediaAccessToken().catch((error) => {
        // Native playback is optional; the cookie-backed web session remains usable.
        const status = Number.isInteger(error?.status) ? `HTTP ${error.status}` : 'no HTTP status';
        console.debug('Native media authorization could not be restored', status);
      });
    }
    state.offlineAccountGeneration = offlineGeneration;
    state.parentalRatings = Array.isArray(parentalRatings) ? parentalRatings : (parentalRatings?.Items || []);
    if (me?.Id && offlineGeneration) {
      broadcastOfflineMessage({ type: 'account-session', accountId: String(me.Id), generation: offlineGeneration });
      void ensureOfflineServiceWorker();
    }
    state.server = server || {};
    state.views = views?.Items || [];
    const adminLibraries = Array.isArray(libraries) ? libraries : [];
    const merged = new Map();
    for (const view of state.views) merged.set(view.Id, { ...view, ItemId: view.Id });
    for (const library of adminLibraries) merged.set(library.ItemId || library.Id, { ...merged.get(library.ItemId || library.Id), ...library, Id: library.ItemId || library.Id });
    state.libraries = Array.from(merged.values());
    await loadAllItems();
  }

  async function restoreMediaAccessToken() {
    if (mediaAccessToken && mediaAccessTokenExpiresAt > Date.now() + 60_000) return mediaAccessToken;
    mediaAccessToken = null;
    mediaAccessTokenExpiresAt = 0;
    const restored = await request('/Users/Me/MediaAccessToken', { method: 'POST' });
    const token = typeof restored?.AccessToken === 'string' ? restored.AccessToken : null;
    const expiresAt = Date.parse(restored?.ExpiresAt || '');
    if (!token || !Number.isFinite(expiresAt) || expiresAt <= Date.now()) {
      throw new Error('The server did not return a usable native media authorization.');
    }
    mediaAccessToken = token;
    mediaAccessTokenExpiresAt = expiresAt;
    return mediaAccessToken;
  }

  async function ensureOfflineServiceWorker() {
    if (!navigator.serviceWorker || !window.isSecureContext) return false;
    try {
      const expectedScriptUrl = new URL(OFFLINE_WORKER_URL, location.href).href;
      const registration = await navigator.serviceWorker.register(OFFLINE_WORKER_URL, { scope: '/web/', updateViaCache: 'none' });
      const installingWorker = registration.installing || registration.waiting;
      if (installingWorker && !(await waitForServiceWorkerActivation(installingWorker))) return false;
      if (registration.active?.scriptURL !== expectedScriptUrl) return false;
      if (navigator.serviceWorker.controller?.scriptURL === expectedScriptUrl) return true;
      return await new Promise((resolve) => {
        const finish = () => {
          clearTimeout(timer);
          navigator.serviceWorker.removeEventListener('controllerchange', checkController);
          resolve(navigator.serviceWorker.controller?.scriptURL === expectedScriptUrl);
        };
        const checkController = () => {
          if (navigator.serviceWorker.controller?.scriptURL === expectedScriptUrl) finish();
        };
        const timer = setTimeout(finish, 8000);
        navigator.serviceWorker.addEventListener('controllerchange', checkController);
        checkController();
      });
    } catch (error) {
      console.debug('Offline file handler unavailable', error);
      return false;
    }
  }

  function waitForServiceWorkerActivation(worker) {
    if (worker.state === 'activated') return Promise.resolve(true);
    if (worker.state === 'redundant') return Promise.resolve(false);
    return new Promise((resolve) => {
      const finish = (activated) => {
        clearTimeout(timer);
        worker.removeEventListener('statechange', onStateChange);
        resolve(activated);
      };
      const onStateChange = () => {
        if (worker.state === 'activated') finish(true);
        else if (worker.state === 'redundant') finish(false);
      };
      const timer = setTimeout(() => finish(worker.state === 'activated'), 10000);
      worker.addEventListener('statechange', onStateChange);
      onStateChange();
    });
  }

  async function showOfflineStartup(error) {
    const cache = window.PuffinboxOfflineCache;
    if (!cache) return false;
    try {
      const offlineSession = await cache.getActiveSession();
      const signedOutMarker = await cache.readSetting('puffinbox-local-signed-out');
      if (cache.localSignOutApplies(signedOutMarker, offlineSession)
          || (offlineSession && !offlineSession.accountId && offlineSession.generation)) {
        showLogin();
        return true;
      }
      const accountId = offlineSession?.accountId;
      const accountGeneration = offlineSession?.generation;
      if (!accountId || !accountGeneration) return false;
      const entries = (await cache.listPackages(accountId)).filter((entry) => entry.status === 'complete');
      if (!entries.length) return false;
      app.innerHTML = `<main class="auth-screen"><section class="auth-card offline-fallback"><div class="brand-lockup"><span class="brand-mark">p</span><span>puffinbox</span></div><h1>Your offline library</h1><p>The server is unreachable. These verified copies are stored in this browser for this account.</p><div class="data-list">${entries.map((entry) => `<div class="data-row"><div class="data-row-main"><strong>${escapeHtml(entry.itemName || 'Offline media')}</strong><small>${escapeHtml(formatType(entry.itemType))} · ${escapeHtml(formatBytes(entry.sourceSize))}</small></div><a class="button small" data-open-offline="${escapeHtml(entry.packageId)}" href="${escapeHtml(offlineUrl(accountId, entry.urlToken))}" target="${offlineLinkTarget()}" rel="noopener">Open</a></div>`).join('')}</div><button id="forget-offline" class="button secondary" type="button" style="width:100%;margin-top:16px">Forget these offline copies</button><p class="form-hint" style="margin-top:12px">${escapeHtml(error?.message || 'Reconnect to your server to browse or manage packages.')}</p></section></main>`;
      app.querySelectorAll('a[href^="/web/__offline/"]').forEach((link) => link.addEventListener('click', (event) => {
        if (window.jmpInfo) {
          event.preventDefault();
          void playOfflinePackageInEmbeddedPlayer(link.dataset.openOffline);
          return;
        }
        if (navigator.serviceWorker?.controller?.scriptURL === new URL(OFFLINE_WORKER_URL, location.href).href) return;
        event.preventDefault();
        void ensureOfflineServiceWorker().then((ready) => {
          if (ready) window.location.assign(link.href);
          else showToast('Open this copy once while connected so this browser can prepare offline access.');
        });
      }));
      app.querySelector('#forget-offline')?.addEventListener('click', async () => {
        try {
          await cache.forgetAccount(accountId, accountGeneration);
          showStartupError(error, 'server');
        } catch (storageError) { showError(storageError); }
      });
      return true;
    } catch (storageError) {
      console.debug('Offline startup view unavailable', storageError);
      return false;
    }
  }

  async function loadOfflineData(silent = false) {
    clearTimeout(offlinePollTimer);
    const accountId = String(state.user?.Id || '');
    if (!accountId || !window.PuffinboxOfflineCache) {
      state.offlineError = 'This browser cannot store offline copies.';
      state.offlineCache = [];
      state.offlineServerPackages = [];
      return;
    }
    const cache = window.PuffinboxOfflineCache;
    const results = await Promise.allSettled([
      request('/Puffinbox/Offline/Settings'),
      request('/Puffinbox/Offline/Packages?StartIndex=0&Limit=100'),
      cache.listPackages(accountId),
    ]);
    const settingsResult = results[0];
    const packagesResult = results[1];
    const cacheResult = results[2];
    state.offlineSettings = settingsResult.status === 'fulfilled' ? settingsResult.value : null;
    state.offlineServerPackages = packagesResult.status === 'fulfilled'
      ? (Array.isArray(packagesResult.value) ? packagesResult.value : (packagesResult.value?.Packages || packagesResult.value?.Items || []))
      : [];
    state.offlineCache = cacheResult.status === 'fulfilled' ? cacheResult.value : [];
    state.offlineError = null;
    if (settingsResult.status === 'rejected' && packagesResult.status === 'rejected') {
      state.offlineError = 'The server is offline. Existing complete browser copies remain available below.';
    } else if (packagesResult.status === 'rejected') {
      state.offlineError = packagesResult.reason?.message || 'Server packages could not be refreshed.';
    }
    if (cacheResult.status === 'rejected') {
      state.offlineError = 'This browser’s local storage is unavailable. Check browser storage settings or available space.';
      state.offlineCache = [];
    }
    offlineStorageUsage = null;
    if (navigator.storage?.estimate) {
      try { offlineStorageUsage = await navigator.storage.estimate(); } catch (_) { /* Storage estimates are optional. */ }
    }
    if (state.screen === 'offline') await render();
    if (state.offlineServerPackages.some((entry) => ['queued', 'running', 'processing'].includes(String(entry.Status || '').toLowerCase()))) {
      offlinePollTimer = setTimeout(() => { void loadOfflineData(true); }, 3000);
    }
    if (!silent && state.offlineError && state.offlineCache.length === 0 && packagesResult.status === 'rejected') {
      showToast(state.offlineError);
    }
  }

  async function queueOfflineItem(item) {
    if (state.user?.Policy?.EnableContentDownloading !== true) {
      showToast('This account cannot prepare offline copies.');
      return;
    }
    try {
      await request('/Puffinbox/Offline/Packages', json('POST', { ItemId: item.Id }));
      broadcastOfflineMessage({ type: 'cache-changed', accountId: String(state.user?.Id || ''), generation: state.offlineAccountGeneration });
      itemDialog.close();
      showToast('Offline package queued on the server.');
      await openScreen('offline');
    } catch (error) { showError(error); }
  }

  function packageContentUrl(packageId) {
    return `/Puffinbox/Offline/Packages/${encodeURIComponent(String(packageId))}/Content`;
  }

  function canonicalSha256(value) {
    const text = String(value || '').trim().replace(/^"|"$/g, '').toLowerCase();
    return /^[0-9a-f]{64}$/.test(text) ? text : null;
  }

  function offlineAbortError(message) {
    const error = new Error(message || 'Transfer paused.');
    error.name = 'AbortError';
    return error;
  }

  function updateOfflineProgress(packageId, text) {
    for (const target of app.querySelectorAll('[data-offline-progress]')) {
      if (target.dataset.offlineProgress === String(packageId)) target.textContent = text;
    }
  }

  async function readExactChunk(response, expectedLength, signal) {
    if (!response.body?.getReader) throw new Error('This browser cannot stream bounded offline chunks.');
    const output = new Uint8Array(expectedLength);
    const reader = response.body.getReader();
    let offset = 0;
    try {
      while (true) {
        if (signal.aborted) throw offlineAbortError();
        const { done, value } = await reader.read();
        if (done) break;
        if (offset + value.byteLength > expectedLength) throw new Error('The server sent more than one bounded offline chunk.');
        output.set(value, offset);
        offset += value.byteLength;
      }
    } catch (error) {
      await reader.cancel().catch(() => {});
      throw error;
    }
    if (offset !== expectedLength) throw new Error('The server returned an incomplete offline chunk. Resume the transfer to continue.');
    return output;
  }

  async function downloadOfflinePackage(packageId) {
    const id = String(packageId);
    if (offlineDownloadTasks.has(id)) return;
    const serverPackage = state.offlineServerPackages.find((entry) => String(entry.Id || entry.id) === id);
    if (!serverPackage || String(serverPackage.Status || '').toLowerCase() !== 'ready') {
      showToast('This server package is not ready to transfer yet.');
      return;
    }
    if (state.user?.Policy?.EnableContentDownloading !== true) {
      showToast('This account cannot download content for offline use.');
      return;
    }
    const controller = new AbortController();
    const accountId = String(state.user.Id);
    const generation = offlineCacheGeneration;
    const accountGeneration = state.offlineAccountGeneration;
    if (!accountGeneration) {
      showToast('This browser could not establish an isolated offline account session.');
      return;
    }
    offlineDownloadControllers.set(id, controller);
    const task = window.PuffinboxOfflineCache.getPackageGeneration(accountId, id, accountGeneration)
      .then((packageEpoch) => transferOfflinePackage(serverPackage, accountId, generation, accountGeneration, packageEpoch, controller.signal));
    offlineDownloadTasks.set(id, task);
    await task.catch((error) => {
      if (error.name !== 'AbortError' && error.message !== 'This offline transfer was removed or replaced.') {
        const code = window.PuffinboxOfflineCache.offlineStorageErrorCode(error);
        showError(new Error(code === 'storage-quota'
          ? 'Browser storage for this site is full. Free space in this browser and resume the copy.'
          : code === 'storage-unavailable'
            ? 'Browser storage for this site is unavailable. Check this site’s storage permissions and retry.'
            : error.message));
      }
    });
    offlineDownloadTasks.delete(id);
    offlineDownloadControllers.delete(id);
    if (state.screen === 'offline' && activeOfflineAccount(accountId, generation)) await loadOfflineData(true);
  }

  function activeOfflineAccount(accountId, generation) {
    return generation === offlineCacheGeneration && String(state.user?.Id || '') === accountId;
  }

  async function transferOfflinePackage(serverPackage, accountId, generation, accountGeneration, packageEpoch, signal) {
    const cache = window.PuffinboxOfflineCache;
    const packageId = String(serverPackage.Id || serverPackage.id);
    const size = Number(serverPackage.SourceSize);
    const expectedHash = canonicalSha256(serverPackage.Sha256);
    const chunkSize = Number(state.offlineSettings?.ChunkBytes || cache.CHUNK_SIZE);
    if (!Number.isSafeInteger(size) || size < 0 || size > cache.MAX_ITEM_SIZE) throw new Error('This package is larger than the 8 GiB per-item offline limit.');
    if (!expectedHash || !Number.isSafeInteger(chunkSize) || chunkSize < 1 || chunkSize > cache.CHUNK_SIZE) throw new Error('The server package has invalid integrity or chunk information.');
    const chunkCount = Math.ceil(size / chunkSize);
    if (Number(serverPackage.ChunkCount) !== chunkCount) throw new Error('The server package reported an inconsistent chunk count.');
    if (navigator.storage?.persist) {
      try { await navigator.storage.persist(); } catch (_) { /* Persistence is best effort. */ }
    }
    const existing = await cache.getPackage(accountId, packageId);
    const info = {
      itemId: String(serverPackage.ItemId || ''),
      itemName: String(serverPackage.ItemName || 'Offline media'),
      fileName: String(serverPackage.FileName || serverPackage.ItemName || 'offline-item'),
      itemType: String(serverPackage.ItemType || 'Media'),
      contentType: String(serverPackage.ContentType || ''),
      container: String(serverPackage.Container || ''),
      sourceSize: size, sha256: expectedHash, etag: `"${expectedHash}"`, chunkSize, chunkCount,
      status: 'downloading', errorCode: null,
    };
    let nextPackageEpoch = packageEpoch;
    if (existing && (existing.sha256 !== expectedHash || existing.sourceSize !== size || existing.chunkSize !== chunkSize)) {
      nextPackageEpoch = await cache.removePackage(accountId, packageId, accountGeneration, existing.transferToken);
    }
    const transferToken = await cache.startPackageTransfer(accountId, packageId, info, accountGeneration, nextPackageEpoch);
    const estimate = navigator.storage?.estimate ? await navigator.storage.estimate().catch(() => null) : null;
    const current = await cache.chunkIndexes(accountId, packageId, accountGeneration, transferToken);
    const expectedMissingBytes = Math.max(0, size - current.length * chunkSize);
    if (estimate?.quota && estimate.usage + expectedMissingBytes > estimate.quota) {
      await cache.updatePackageTransfer(accountId, packageId, { status: 'paused', errorCode: 'storage-quota' }, accountGeneration, transferToken);
      throw new DOMException('The browser storage quota is insufficient for this copy.', 'QuotaExceededError');
    }
    try {
      for (let index = 0; index < chunkCount; index += 1) {
        if (signal.aborted) throw offlineAbortError();
        if (!activeOfflineAccount(accountId, generation)) throw offlineAbortError('Account changed.');
        const expectedLength = Math.min(chunkSize, size - index * chunkSize);
        const saved = await cache.getChunk(accountId, packageId, index, accountGeneration, transferToken);
        if (saved && saved.bytes.byteLength === expectedLength
            && new cache.Sha256().update(new Uint8Array(saved.bytes)).digestHex() === saved.digest) {
          continue;
        }
        const start = index * chunkSize;
        const end = start + expectedLength - 1;
        const response = await fetch(packageContentUrl(packageId), {
          method: 'GET', credentials: 'same-origin', signal,
          headers: { Range: `bytes=${start}-${end}` },
        });
        if (response.status !== 206) throw new Error(response.status === 403 ? 'Offline downloading is no longer allowed for this account or item.' : `The server did not return a partial chunk (${response.status}). Refresh packages and retry.`);
        const contentRange = response.headers.get('content-range') || '';
        if (contentRange !== `bytes ${start}-${end}/${size}`) throw new Error('The server returned an unexpected byte range.');
        if (canonicalSha256(response.headers.get('etag')) !== expectedHash) throw new Error('The server package changed during transfer. Refresh the package before continuing.');
        const chunkHash = canonicalSha256(response.headers.get('x-chunk-sha256'));
        if (!chunkHash) throw new Error('The server did not provide a valid chunk integrity digest.');
        const bytes = await readExactChunk(response, expectedLength, signal);
        if (!activeOfflineAccount(accountId, generation)) throw offlineAbortError('Account changed.');
        await cache.putChunk(accountId, packageId, index, bytes, chunkHash, accountGeneration, transferToken);
        const bytesCopied = Math.min(size, (index + 1) * chunkSize);
        await cache.updatePackageTransfer(accountId, packageId, { bytesCopied, status: 'downloading' }, accountGeneration, transferToken);
        updateOfflineProgress(packageId, `${formatBytes(bytesCopied)} of ${formatBytes(size)}`);
      }
      if (!activeOfflineAccount(accountId, generation)) throw offlineAbortError('Account changed.');
      const complete = await cache.verifyAndComplete(accountId, packageId, size, expectedHash, chunkSize, accountGeneration, transferToken);
      if (!complete) throw new Error('This browser is missing one or more offline chunks. Resume the transfer to continue.');
      await cache.updatePackageTransfer(accountId, packageId, { status: 'complete', bytesCopied: size, completedAt: new Date().toISOString(), errorCode: null }, accountGeneration, transferToken);
      await ensureOfflineServiceWorker();
      broadcastOfflineMessage({ type: 'cache-changed', accountId, generation: accountGeneration, packageId });
      showToast('Offline copy saved and verified in this browser.');
    } catch (error) {
      if (activeOfflineAccount(accountId, generation)) {
        const storageError = cache.offlineStorageErrorCode(error);
        const message = String(error.message || '').toLowerCase();
        const errorCode = storageError
          || (message.includes('integrity') || message.includes('sha-256') || message.includes('digest') ? 'integrity-failed'
            : message.includes('changed') ? 'package-changed' : 'network-interrupted');
        await cache.updatePackageTransfer(accountId, packageId, { status: 'paused', errorCode }, accountGeneration, transferToken).catch(() => {});
      }
      throw error;
    }
  }

  async function removeOfflineLocal(packageId) {
    if (!window.confirm('Remove this downloaded copy from this browser? The server package will remain available.')) return;
    try {
      const id = String(packageId);
      offlineDownloadControllers.get(id)?.abort();
      await offlineDownloadTasks.get(id)?.catch(() => {});
      const local = state.offlineCache.find((entry) => String(entry.packageId) === id);
      if (!local?.transferToken) throw new Error('This offline copy changed in another tab. Refresh the offline list before removing it.');
      await window.PuffinboxOfflineCache.removePackage(state.user.Id, id, state.offlineAccountGeneration, local.transferToken);
      broadcastOfflineMessage({ type: 'cache-changed', accountId: String(state.user.Id), generation: state.offlineAccountGeneration, packageId: id });
      showToast('Browser copy removed.');
      await loadOfflineData(true);
    } catch (error) { showError(error); }
  }

  async function removeOfflineServer(packageId) {
    if (!window.confirm('Remove this prepared package from the server? A browser copy already saved here will remain until you remove it.')) return;
    try {
      await request(`/Puffinbox/Offline/Packages/${encodeURIComponent(String(packageId))}`, { method: 'DELETE' });
      broadcastOfflineMessage({ type: 'cache-changed', accountId: String(state.user?.Id || ''), generation: state.offlineAccountGeneration, packageId: String(packageId) });
      showToast('Server package removed.');
      await loadOfflineData(true);
    } catch (error) { showError(error); }
  }

  async function loadAllItems(start = 0) {
    state.itemStart = start;
    const result = await request(`/Items?Recursive=true&Limit=${state.itemLimit}&StartIndex=${state.itemStart}`);
    state.items = result?.Items || [];
    state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
  }

  async function loadLibraryItems(library) {
    state.activeLibrary = library || null;
    state.currentFolder = null;
    state.navigationStack = [];
    state.screen = 'browse';
    state.searchTerm = '';
    await render();
    if (!library) { await loadAllItems(); await render(); return; }
    const result = await request(`/Items?ParentId=${encodeURIComponent(library.Id || library.ItemId)}&Recursive=true&Limit=${state.itemLimit}&StartIndex=0`);
    state.items = result?.Items || [];
    state.itemStart = 0;
    state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
    await render();
  }

  async function openLibrary(id) {
    const library = state.libraries.find((entry) => String(entry.Id || entry.ItemId) === String(id));
    await loadLibraryItems(library);
  }

  let playlistSelectionGeneration = 0;

  async function getPlaylistEntries(playlistId) {
    const entries = [];
    let total = 0;
    for (let start = 0; start < 1_000; start += 100) {
      const result = await request(`/Playlists/${encodeURIComponent(playlistId)}/Items?StartIndex=${start}&Limit=100`);
      const page = Array.isArray(result?.Items) ? result.Items : [];
      entries.push(...page.filter((item) => isAudioItem(item) && item.PlaylistItemId));
      total = Number(result?.TotalRecordCount ?? entries.length);
      if (!page.length || start + page.length >= total) break;
    }
    return entries;
  }

  async function loadMusicData(preferredPlaylistId = state.selectedPlaylistId) {
    const generation = ++playlistSelectionGeneration;
    let loadingPlaylistId = null;
    try {
      const [playlistResult, audioResult] = await Promise.all([
        request(`/Playlists?StartIndex=${state.playlistStart}&Limit=100`),
        request(`/Items?IncludeItemTypes=Audio&Recursive=true&Limit=100&StartIndex=${state.playlistAudioStart}`),
      ]);
      if (generation !== playlistSelectionGeneration || state.screen !== 'music') return;
      state.playlists = Array.isArray(playlistResult?.Items) ? playlistResult.Items.filter((item) => item.MediaType === 'Audio') : [];
      state.playlistTotal = Number(playlistResult?.TotalRecordCount ?? state.playlists.length);
      state.playlistAudioItems = Array.isArray(audioResult?.Items) ? audioResult.Items.filter(isAudioItem) : [];
      state.playlistAudioTotal = Number(audioResult?.TotalRecordCount ?? state.playlistAudioItems.length);
      const selected = state.playlists.find((playlist) => String(playlist.Id) === String(preferredPlaylistId)) || state.playlists[0] || null;
      state.selectedPlaylistId = selected?.Id ? String(selected.Id) : null;
      loadingPlaylistId = state.selectedPlaylistId;
      state.playlistEntries = [];
      state.playlistEntriesLoading = !!selected;
      state.playlistEntriesError = false;
      if (selected) await render();
      const entries = selected ? await getPlaylistEntries(loadingPlaylistId) : [];
      if (generation !== playlistSelectionGeneration || state.screen !== 'music') return;
      state.playlistEntries = entries;
      state.playlistEntriesLoading = false;
    } catch (error) {
      if (generation === playlistSelectionGeneration && state.screen === 'music') {
        state.playlistEntries = [];
        state.playlistEntriesLoading = false;
        state.playlistEntriesError = Boolean(loadingPlaylistId);
        if (!loadingPlaylistId) {
          state.playlists = [];
          state.playlistTotal = 0;
          state.selectedPlaylistId = null;
          state.playlistAudioItems = [];
          state.playlistAudioTotal = 0;
        }
        showError(error);
      }
    }
    if (generation === playlistSelectionGeneration && state.screen === 'music') await render();
  }

  async function loadMusicPlaylistPage(start) {
    if (!Number.isInteger(start) || start < 0) return;
    state.playlistStart = start;
    await loadMusicData();
  }

  async function loadMusicAudioPage(start) {
    if (!Number.isInteger(start) || start < 0) return;
    try {
      const result = await request(`/Items?IncludeItemTypes=Audio&Recursive=true&Limit=100&StartIndex=${start}`);
      state.playlistAudioStart = start;
      state.playlistAudioItems = Array.isArray(result?.Items) ? result.Items.filter(isAudioItem) : [];
      state.playlistAudioTotal = Number(result?.TotalRecordCount ?? state.playlistAudioItems.length);
      await render();
    } catch (error) { showError(error); }
  }

  async function selectPlaylist(playlistId) {
    const playlist = state.playlists.find((item) => String(item.Id) === String(playlistId));
    if (!playlist) return;
    const generation = ++playlistSelectionGeneration;
    state.selectedPlaylistId = String(playlist.Id);
    state.playlistEntries = [];
    state.playlistEntriesLoading = true;
    state.playlistEntriesError = false;
    await render();
    try {
      const entries = await getPlaylistEntries(state.selectedPlaylistId);
      if (generation !== playlistSelectionGeneration || state.screen !== 'music') return;
      state.playlistEntries = entries;
      state.playlistEntriesLoading = false;
      await render();
    } catch (error) {
      if (generation === playlistSelectionGeneration && state.screen === 'music') {
        state.playlistEntries = [];
        state.playlistEntriesLoading = false;
        state.playlistEntriesError = true;
        showError(error);
        await render();
      }
    }
  }

  async function createPlaylist(event) {
    event.preventDefault();
    const form = event.currentTarget;
    const name = String(new FormData(form).get('Name') || '').trim();
    if (!name) { showToast('Enter a playlist name.'); return; }
    const submit = form.querySelector('[type="submit"]');
    if (submit) submit.disabled = true;
    try {
      const created = await request('/Playlists', json('POST', { Name: name, MediaType: 'Audio' }));
      state.playlistStart = 0;
      await loadMusicData(created?.Id || null);
      showToast('Playlist created.');
    } catch (error) { showError(error); }
    finally { if (submit) submit.disabled = false; }
  }

  async function addPlaylistItem(itemId) {
    if (!state.selectedPlaylistId || state.user?.Policy?.EnableMediaPlayback === false) {
      showToast('Select a playlist and make sure media playback is allowed for this account.');
      return;
    }
    const playlistId = state.selectedPlaylistId;
    try {
      await request(`/Playlists/${encodeURIComponent(playlistId)}/Items?Ids=${encodeURIComponent(itemId)}`, { method: 'POST' });
      if (state.screen === 'music' && state.selectedPlaylistId === playlistId) await selectPlaylist(playlistId);
      showToast('Track added to playlist.');
    } catch (error) { showError(error); }
  }

  async function movePlaylistEntry(entryId, direction) {
    if (!state.selectedPlaylistId) return;
    const playlistId = state.selectedPlaylistId;
    const index = state.playlistEntries.findIndex((entry) => String(entry.PlaylistItemId) === String(entryId));
    const target = index + direction;
    if (index < 0 || target < 0 || target >= state.playlistEntries.length) return;
    try {
      await request(`/Playlists/${encodeURIComponent(playlistId)}/Items/${encodeURIComponent(entryId)}/Move/${target}`, { method: 'POST' });
      if (state.screen === 'music' && state.selectedPlaylistId === playlistId) await selectPlaylist(playlistId);
    } catch (error) { showError(error); }
  }

  async function removePlaylistEntry(entryId) {
    if (!state.selectedPlaylistId) return;
    const playlistId = state.selectedPlaylistId;
    try {
      await request(`/Playlists/${encodeURIComponent(playlistId)}/Items?EntryIds=${encodeURIComponent(entryId)}`, { method: 'DELETE' });
      if (state.screen === 'music' && state.selectedPlaylistId === playlistId) await selectPlaylist(playlistId);
      showToast('Track removed from playlist.');
    } catch (error) { showError(error); }
  }

  function playPlaylist() {
    const items = state.playlistEntries.filter(isAudioItem);
    if (!items.length) { showToast('Add a track before playing this playlist.'); return; }
    if (state.user?.Policy?.EnableMediaPlayback === false) { showToast('Media playback is not allowed for this account.'); return; }
    playbackQueue = items.slice(1);
    void playItem(items[0], { offerResume: false, preserveQueue: true });
  }

  async function openScreen(screen) {
    state.screen = screen;
    state.searchTerm = '';
    if (screen !== 'offline') clearTimeout(offlinePollTimer);
    if (screen === 'offline') {
      state.currentFolder = null;
      state.navigationStack = [];
      await loadOfflineData();
      await render();
      return;
    }
    if (screen === 'live-tv') {
      if (!window.PuffinboxLiveTv?.canAccess(state.user)) {
        state.screen = 'home';
        await render();
        return;
      }
      window.PuffinboxLiveTv.prepare(state.user, state.libraries);
      await render();
      void window.PuffinboxLiveTv.load(request, state.user, state.libraries, () => {
        if (state.screen === 'live-tv') void render();
      });
      return;
    }
    if (screen === 'home' || screen === 'browse') {
      state.activeLibrary = null;
      state.currentFolder = null;
      state.navigationStack = [];
      try { await loadAllItems(); } catch (error) { showError(error); }
    }
    if (screen === 'users') {
      try { state.users = await request('/Users'); } catch (error) { showError(error); state.users = []; }
    }
    if (screen === 'music') {
      state.playlistStart = 0;
      state.playlistAudioStart = 0;
      await loadMusicData();
      return;
    }
    await render();
  }

  async function render() {
    if (!state.user) return;
    setTheme(window.PuffinboxDeviceId.readSetting(() => window.localStorage, 'puffinbox-theme', 'dark'));
    let content;
    switch (state.screen) {
      case 'browse': content = browseScreen(); break;
      case 'music': content = musicScreen(); break;
      case 'live-tv': content = window.PuffinboxLiveTv?.render(state.user, state.libraries) || ''; break;
      case 'libraries': content = libraryScreen(); break;
      case 'users': content = userScreen(); break;
      case 'settings': content = settingsScreen(); break;
      case 'offline': content = offlineScreen(); break;
      default: content = homeScreen();
    }
    shell(content);
    if (state.screen === 'libraries') void loadScanStatus();
  }

  async function runSearch(term) {
    const menu = document.querySelector('#search-menu');
    menu?.classList.remove('open');
    state.searchTerm = term;
    state.screen = 'browse';
    state.activeLibrary = null;
    state.currentFolder = null;
    state.navigationStack = [];
    await render();
    if (!term) { await loadAllItems(); await render(); return; }
    try {
      const result = await request(`/Items?SearchTerm=${encodeURIComponent(term)}&Recursive=true&Limit=${state.itemLimit}&StartIndex=0`);
      state.items = result?.Items || [];
      state.itemStart = 0;
      state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
      await render();
    } catch (error) { showError(error); }
  }

  async function searchHints(term) {
    try {
      const result = await request(`/Search/Hints?SearchTerm=${encodeURIComponent(term)}&Limit=6`);
      const hints = (result?.SearchHints || []).slice(0, 6);
      const menu = document.querySelector('#search-menu');
      if (!menu) return;
      const options = hints.length ? hints.map((hint) => {
        const button = document.createElement('button');
        button.className = 'search-suggestion';
        button.type = 'button';
        button.setAttribute('role', 'option');
        button.dataset.searchTerm = String(hint.Name || hint.SearchTerm || term);
        button.append(document.createTextNode(String(hint.Name || hint.SearchTerm || term)));
        const label = document.createElement('span');
        label.textContent = formatType(hint.Type);
        button.append(label);
        return button;
      }) : (() => {
        const button = document.createElement('button');
        button.className = 'search-suggestion';
        button.type = 'button';
        button.setAttribute('role', 'option');
        button.dataset.searchTerm = term;
        button.textContent = `Search for “${term}”`;
        return [button];
      })();
      window.PuffinboxClientCompat.replaceChildren(menu, ...options);
      menu.classList.add('open');
    } catch (error) { console.debug('Search hints unavailable', error); }
  }

  async function changePage(direction) {
    const lastStart = Math.max(0, Math.floor(Math.max(0, state.itemTotal - 1) / state.itemLimit) * state.itemLimit);
    const nextStart = Math.max(0, Math.min(lastStart, state.itemStart + (direction === 'next' ? state.itemLimit : -state.itemLimit)));
    if (nextStart === state.itemStart) return;
    let path;
    if (state.searchTerm) path = `/Items?SearchTerm=${encodeURIComponent(state.searchTerm)}&Recursive=true&Limit=${state.itemLimit}&StartIndex=${nextStart}`;
    else if (state.currentFolder) path = `/Items?ParentId=${encodeURIComponent(state.currentFolder.Id)}&Recursive=false&Limit=${state.itemLimit}&StartIndex=${nextStart}`;
    else if (state.activeLibrary) path = `/Items?ParentId=${encodeURIComponent(state.activeLibrary.Id || state.activeLibrary.ItemId)}&Recursive=true&Limit=${state.itemLimit}&StartIndex=${nextStart}`;
    else path = `/Items?Recursive=true&Limit=${state.itemLimit}&StartIndex=${nextStart}`;
    try {
      const result = await request(path);
      state.items = result?.Items || [];
      state.itemStart = nextStart;
      state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
      await render();
    } catch (error) { showError(error); }
  }

  async function openItem(id) {
    const item = state.items.find((entry) => String(entry.Id) === String(id));
    let details = item;
    try { details = await request(`/Items/${encodeURIComponent(id)}`); } catch (error) { console.debug('Item details unavailable', error); }
    if (isBrowsableContainer(details)) {
      state.navigationStack.push({
        folder: state.currentFolder,
        items: state.items,
        total: state.itemTotal,
        start: state.itemStart,
        searchTerm: state.searchTerm,
        activeLibrary: state.activeLibrary,
      });
      state.currentFolder = details;
      state.searchTerm = '';
      try {
        const result = await request(`/Items?ParentId=${encodeURIComponent(details.Id)}&Recursive=false&Limit=${state.itemLimit}&StartIndex=0`);
        state.items = result?.Items || [];
        state.itemStart = 0;
        state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
        state.screen = 'browse';
        await render();
      } catch (error) { showError(error); }
      return;
    }
    showItemDetails(details || item);
  }

  function isBrowsableContainer(item) {
    return !!item && (item.IsFolder === true || [
      'Folder', 'CollectionFolder', 'Series', 'Season', 'MusicArtist', 'MusicAlbum',
    ].includes(item.Type));
  }

  async function restoreBrowseParent() {
    const parent = state.navigationStack.pop();
    if (parent) {
      state.currentFolder = parent.folder;
      state.items = parent.items;
      state.itemTotal = parent.total;
      state.itemStart = parent.start;
      state.searchTerm = parent.searchTerm;
      state.activeLibrary = parent.activeLibrary;
    } else {
      state.currentFolder = null;
      state.searchTerm = '';
      if (state.activeLibrary) {
        const result = await request(`/Items?ParentId=${encodeURIComponent(state.activeLibrary.Id || state.activeLibrary.ItemId)}&Recursive=true&Limit=${state.itemLimit}&StartIndex=0`);
        state.items = result?.Items || [];
        state.itemTotal = Number(result?.TotalRecordCount ?? state.items.length);
        state.itemStart = 0;
      } else {
        await loadAllItems();
      }
    }
    await render();
  }

  function showItemDetails(item) {
    if (!item) return;
    const type = formatType(item.Type);
    const canPlay = ['Movie', 'Episode', 'Audio', 'Video'].includes(item.Type) || /video|audio/i.test(item.MediaType || '');
    const canViewPhoto = item.Type === 'Photo';
    const canReadBook = ['Book', 'EBook'].includes(item.Type);
    const canDownloadBook = canReadBook && state.user?.Policy?.EnableContentDownloading === true;
    const canPrepareOffline = state.user?.Policy?.EnableContentDownloading === true
      && ['Movie', 'Episode', 'Audio', 'Video', 'MusicVideo', 'Photo', 'Book', 'AudioBook', 'EBook'].includes(item.Type);
    const detail = document.createElement('div');
    detail.className = 'item-detail';
    const header = document.createElement('div');
    header.className = 'item-detail-head';
    const heading = document.createElement('div');
    const typeLabel = document.createElement('p');
    typeLabel.className = 'detail-type';
    typeLabel.textContent = type;
    const title = document.createElement('h2');
    title.textContent = item.Name || 'Untitled';
    heading.append(typeLabel, title);
    const close = document.createElement('button');
    close.className = 'icon-button';
    close.type = 'button';
    close.setAttribute('aria-label', 'Close details');
    close.textContent = '×';
    close.id = 'close-item-dialog';
    header.append(heading, close);
    detail.append(header);
    for (const value of [item.ProductionYear, item.Overview || 'No description is available for this item.', item.RunTimeTicks ? formatDuration(item.RunTimeTicks) : '']) {
      if (!value) continue;
      const paragraph = document.createElement('p');
      paragraph.textContent = String(value);
      detail.append(paragraph);
    }
    if (canViewPhoto) {
      const photo = document.createElement('div');
      photo.id = 'photo-view';
      photo.className = 'photo-view';
      photo.textContent = 'Loading photo…';
      detail.append(photo);
    }
    const actions = document.createElement('div');
    actions.className = 'detail-actions';
    if (canPlay) {
      const play = document.createElement('button');
      play.className = 'button';
      play.type = 'button';
      play.textContent = 'Play';
      play.addEventListener('click', () => { itemDialog.close(); playItem(item); });
      actions.append(play);
    }
    if (canReadBook) {
      const read = document.createElement('a');
      read.className = 'button';
      read.id = 'read-book';
      read.href = `/Books/${encodeURIComponent(item.Id)}/Reader`;
      read.textContent = 'Read';
      actions.append(read);
    }
    if (canDownloadBook) {
      const download = document.createElement('a');
      download.className = 'button';
      download.id = 'download-book';
      download.href = `/Books/${encodeURIComponent(item.Id)}/Download`;
      download.download = '';
      download.textContent = 'Download book';
      download.addEventListener('click', () => showToast('Book download started.'));
      actions.append(download);
    }
    if (canPrepareOffline) {
      const offline = document.createElement('button');
      offline.className = 'button secondary';
      offline.type = 'button';
      offline.textContent = 'Prepare offline';
      offline.addEventListener('click', () => { void queueOfflineItem(item); });
      actions.append(offline);
    }
    const done = document.createElement('button');
    done.className = 'button secondary';
    done.type = 'button';
    done.id = 'close-item-secondary';
    done.textContent = 'Done';
    done.addEventListener('click', () => itemDialog.close());
    actions.append(done);
    detail.append(actions);
    window.PuffinboxClientCompat.replaceChildren(itemDialog, detail);
    itemDialog.showModal();
    close.addEventListener('click', () => itemDialog.close());
    if (canViewPhoto) viewPhoto(item);
  }

  async function viewPhoto(item) {
    const target = itemDialog.querySelector('#photo-view');
    if (!target) return;
    photoLoadController?.abort();
    photoLoadController = new AbortController();
    const controller = photoLoadController;
    if (photoObjectUrl) URL.revokeObjectURL(photoObjectUrl);
    photoObjectUrl = null;
    window.PuffinboxClientCompat.replaceChildren(target);
    target.textContent = 'Loading photo…';
    try {
      const photoUrl = new URL(`/Items/${encodeURIComponent(item.Id)}/File`, window.location.href);
      const response = await fetch(photoUrl.href, { credentials: 'same-origin', signal: controller.signal });
      if (!response.ok) throw new Error(response.status === 404 ? 'Photo file was not found.' : `Photo could not be loaded (${response.status}).`);
      const mime = (response.headers.get('content-type') || '').split(';', 1)[0].trim().toLowerCase();
      const safeRasterTypes = new Set(['image/jpeg', 'image/png', 'image/gif', 'image/webp', 'image/avif', 'image/bmp']);
      if (!safeRasterTypes.has(mime)) throw new Error('This file is not a supported browser-safe raster image.');
      const maxPhotoBytes = 32 * 1024 * 1024;
      const declaredLength = Number(response.headers.get('content-length'));
      if (Number.isFinite(declaredLength) && declaredLength > maxPhotoBytes) throw new Error('This photo is larger than the 32 MiB preview limit. Download it to view it elsewhere.');
      if (!response.body?.getReader) throw new Error('This browser cannot safely preview a bounded photo response. Download the image instead.');
      const chunks = [];
      let totalBytes = 0;
      const reader = response.body.getReader();
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          totalBytes += value.byteLength;
          if (totalBytes > maxPhotoBytes) {
            await reader.cancel();
            throw new Error('This photo is larger than the 32 MiB preview limit. Download it to view it elsewhere.');
          }
          chunks.push(value);
        }
      } finally { reader.releaseLock(); }
      if (controller.signal.aborted || !itemDialog.open) return;
      const objectUrl = URL.createObjectURL(new Blob(chunks, { type: mime }));
      photoObjectUrl = objectUrl;
      const image = document.createElement('img');
      image.src = objectUrl;
      image.alt = item.Name || 'Photo';
      image.className = 'photo-image';
      window.PuffinboxClientCompat.replaceChildren(target, image);
      target.dataset.objectUrl = objectUrl;
    } catch (error) {
      if (error.name !== 'AbortError') target.textContent = error.message || 'Photo could not be loaded.';
    }
  }

  function formatDuration(ticks) {
    const seconds = Math.floor(Number(ticks) / 10_000_000);
    if (!Number.isFinite(seconds) || seconds <= 0) return '';
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    return hours ? `${hours} hr ${minutes} min` : `${minutes} min`;
  }

  function deviceProfile() {
    if (window.jmpInfo) {
      const nativeDeviceProfile = window.PuffinboxJmpPlayer?.getDeviceProfile();
      if (nativeDeviceProfile && Array.isArray(nativeDeviceProfile.DirectPlayProfiles)
        && Array.isArray(nativeDeviceProfile.TranscodingProfiles)) {
        const nativeHls = nativeDeviceProfile.TranscodingProfiles.some((profile) => String(profile?.Protocol || '').toLowerCase() === 'hls');
        return { DeviceProfile: nativeDeviceProfile, native: true, nativeHls, hlsJs: false };
      }
      return { DeviceProfile: { DirectPlayProfiles: [], TranscodingProfiles: [] }, native: true, nativeHls: false, hlsJs: false };
    }
    const video = document.createElement('video');
    const audio = document.createElement('audio');
    const supports = (element, mime) => element.canPlayType(mime) !== '';
    const directPlayProfiles = [];
    const checks = [
      [video, 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"', { Container: 'mp4,m4v,mov', Type: 'Video', VideoCodec: 'h264', AudioCodec: 'aac' }],
      [video, 'video/mp4; codecs="av01.0.05M.08, mp4a.40.2"', { Container: 'mp4,m4v', Type: 'Video', VideoCodec: 'av1', AudioCodec: 'aac' }],
      [video, 'video/webm; codecs="vp9, opus"', { Container: 'webm', Type: 'Video', VideoCodec: 'vp9', AudioCodec: 'opus' }],
      [video, 'video/webm; codecs="vp8, vorbis"', { Container: 'webm', Type: 'Video', VideoCodec: 'vp8', AudioCodec: 'vorbis' }],
      [audio, 'audio/mpeg; codecs="mp3"', { Container: 'mp3', Type: 'Audio', AudioCodec: 'mp3' }],
      [audio, 'audio/mp4; codecs="mp4a.40.2"', { Container: 'm4a', Type: 'Audio', AudioCodec: 'aac' }],
      [audio, 'audio/aac', { Container: 'aac', Type: 'Audio', AudioCodec: 'aac' }],
      [audio, 'audio/ogg; codecs="opus"', { Container: 'ogg', Type: 'Audio', AudioCodec: 'opus' }],
      [audio, 'audio/ogg; codecs="vorbis"', { Container: 'ogg', Type: 'Audio', AudioCodec: 'vorbis' }],
      [audio, 'audio/flac', { Container: 'flac', Type: 'Audio', AudioCodec: 'flac' }],
      [audio, 'audio/wav; codecs="1"', { Container: 'wav', Type: 'Audio', AudioCodec: 'pcm_s16le' }],
    ];
    for (const [element, mime, profile] of checks) if (supports(element, mime)) directPlayProfiles.push(profile);
    const nativeHls = supports(video, 'application/vnd.apple.mpegurl') || supports(video, 'application/x-mpegURL');
    const hlsJs = typeof window.Hls !== 'undefined' && window.Hls.isSupported();
    const mseH264Aac = typeof MediaSource !== 'undefined' && MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E, mp4a.40.2"');
    const transcodingProfiles = nativeHls || (hlsJs && mseH264Aac) ? [
      { Container: 'ts', Type: 'Video', AudioCodec: 'aac', VideoCodec: 'h264', Protocol: 'hls' },
      { Container: 'ts', Type: 'Audio', AudioCodec: 'aac', Protocol: 'hls' },
    ] : [];
    return { DeviceProfile: { DirectPlayProfiles: directPlayProfiles, TranscodingProfiles: transcodingProfiles }, nativeHls, hlsJs: hlsJs && mseH264Aac };
  }

  function playbackPositionTicks(media, positionOffsetTicks = 0) {
    const seconds = Number(media?.currentTime);
    const elapsed = Number.isFinite(seconds) && seconds > 0 ? Math.round(seconds * 10_000_000) : 0;
    return Math.min(Math.max(0, Number(positionOffsetTicks) || 0) + elapsed, 3_155_760_000_000_000);
  }

  function sessionPositionTicks(session) {
    if (session?.isLiveTv) return 0;
    if (session?.isNative) {
      const milliseconds = Math.max(0, Number(session.nativePositionMs) || 0);
      return Math.min(Math.max(0, Number(session.positionOffsetTicks) || 0) + Math.round(milliseconds * 10_000), 3_155_760_000_000_000);
    }
    return playbackPositionTicks(session?.media, session?.positionOffsetTicks);
  }

  function renderNativePosition(session) {
    const current = Math.max(0, Number(session?.nativePositionMs) || 0);
    const duration = Math.max(0, Number(session?.nativeDurationMs) || 0);
    nativeSeek.disabled = duration <= 0;
    nativeSeek.max = String(duration);
    nativeSeek.value = String(Math.min(current, duration || current));
    nativePosition.textContent = `${formatPlaybackPosition(current * 10_000)} / ${formatPlaybackPosition(duration * 10_000)}`;
  }

  function formatPlaybackPosition(ticks) {
    const seconds = Math.max(0, Math.floor(Number(ticks) / 10_000_000));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const remainder = seconds % 60;
    return hours
      ? `${hours}:${String(minutes).padStart(2, '0')}:${String(remainder).padStart(2, '0')}`
      : `${minutes}:${String(remainder).padStart(2, '0')}`;
  }

  function updatePlayerOptions(streams, selection = {}) {
    const audios = streams.filter((stream) => stream.Type === 'Audio');
    const subtitles = streams.filter((stream) => stream.Type === 'Subtitle');
    const setOptions = (select, choices, automaticLabel, offLabel = null) => {
      const options = [];
      const automatic = document.createElement('option');
      automatic.value = '';
      automatic.textContent = automaticLabel;
      options.push(automatic);
      if (offLabel) {
        const off = document.createElement('option');
        off.value = '-1';
        off.textContent = offLabel;
        options.push(off);
      }
      for (const stream of choices) {
        const option = document.createElement('option');
        option.value = String(stream.Index);
        const title = stream.DisplayTitle || stream.Title || stream.Language || stream.Codec || `Track ${Number(stream.Index) + 1}`;
        const markers = [stream.IsDefault ? 'Default' : '', stream.IsForced ? 'Forced' : ''].filter(Boolean).join(', ');
        option.textContent = markers ? `${title} · ${markers}` : title;
        options.push(option);
      }
      window.PuffinboxClientCompat.replaceChildren(select, ...options);
    };
    setOptions(audioTrackSelect, audios, 'Server default');
    setOptions(subtitleTrackSelect, subtitles, 'Server default', 'Off');
    audioTrackSelect.value = Number.isInteger(selection.audioStreamIndex) ? String(selection.audioStreamIndex) : '';
    subtitleTrackSelect.value = Number.isInteger(selection.subtitleStreamIndex) ? String(selection.subtitleStreamIndex) : '';
    audioTrackControl.hidden = audios.length < 2;
    subtitleTrackControl.hidden = subtitles.length === 0;
    playerOptions.hidden = audioTrackControl.hidden && subtitleTrackControl.hidden && playerResumeControls.hidden;
  }

  function setPlayerLoading(message) {
    const row = document.createElement('div');
    row.className = 'loading-row';
    const spinner = document.createElement('span');
    spinner.className = 'spinner';
    const label = document.createElement('span');
    label.textContent = message;
    row.append(spinner, label);
    window.PuffinboxClientCompat.replaceChildren(document.querySelector('#player-stage'), row);
  }

  async function beginPlaybackSession(session) {
    if (session.started || session.stopping) return session.startPromise;
    session.activitySeen = true;
    startPlaybackHeartbeat(session);
    return window.PuffinboxPlaybackHeartbeat.singleFlight(session, 'startPromise', async () => {
      try {
        await request('/Sessions/Playing', { ...json('POST', {
          ItemId: session.item.Id,
          PlaySessionId: session.playSessionId,
          PositionTicks: sessionPositionTicks(session),
          PlayMethod: session.mode,
        }), timeoutMs: 8_000 });
        session.started = true;
        startPlaybackHeartbeat(session);
      } catch (_) {
        setPlaybackNote(session, 'Playback is active, but the server could not start this playback session.');
      }
    });
  }

  function refreshPlaybackLease(session) {
    if (session.stopping || activePlayback !== session || session.generation !== playbackGeneration) return;
    const action = window.PuffinboxPlaybackHeartbeat.nextHeartbeatAction(session);
    if (action === 'progress') {
      void reportPlaybackProgress(session, true);
      return;
    }
    if (action === 'start') {
      void beginPlaybackSession(session);
      return;
    }
    if (action !== 'keepalive') return;
    const kind = session.mediaType === 'audio' ? 'Audio' : 'Videos';
    const keepalivePath = session.isLiveTv
      ? `/LiveTv/Channels/${encodeURIComponent(session.item.Id)}/hls/${encodeURIComponent(session.playSessionId)}/keepalive`
      : `/${kind}/${encodeURIComponent(session.item.Id)}/hls/${encodeURIComponent(session.playSessionId)}/keepalive`;
    request(keepalivePath, {
      method: 'POST', timeoutMs: 5_000,
    }).then(() => { session.leaseFailures = 0; }).catch((error) => {
      // The HLS request may still be starting when the browser reports its
      // manifest. Retry on the next heartbeat without claiming playback.
      session.leaseFailures = (session.leaseFailures || 0) + 1;
      if (session.leaseFailures >= 2 && error.status !== 404) {
        setPlaybackNote(session, 'The server could not renew the prepared stream. Press play to continue or reopen this item.');
      }
    });
  }

  function startPlaybackHeartbeat(session) {
    return window.PuffinboxPlaybackHeartbeat.start(session,
      () => activePlayback === session && session.generation === playbackGeneration,
      () => refreshPlaybackLease(session));
  }

  function startPreparedHlsLease(session) {
    if (!session?.usesHls || session.started || session.stopping) return;
    refreshPlaybackLease(session);
    startPlaybackHeartbeat(session);
  }

  function setPlaybackNote(session, message) {
    if (!session.naturalEnded && activePlayback === session && session.generation === playbackGeneration) {
      document.querySelector('#player-note').textContent = message;
    }
  }

  function setTerminalPlaybackStatus(session, message) {
    if (session.naturalEnded && activePlayback === session && session.generation === playbackGeneration) {
      document.querySelector('#player-note').textContent = message;
    }
  }

  async function reportPlaybackProgress(session, force = false) {
    if (!session.started || session.stopping || (session.media?.paused && !force)) return;
    if (session.progressPending) {
      if (force) session.progressAgain = true;
      return;
    }
    const now = Date.now();
    if (!force && now - session.lastProgressAt < 15_000) return;
    session.lastProgressAt = now;
    session.progressPending = true;
    try {
      await request('/Sessions/Playing/Progress', { ...json('POST', {
        ItemId: session.item.Id,
        PlaySessionId: session.playSessionId,
        PositionTicks: sessionPositionTicks(session),
        PlayMethod: session.mode,
      }), timeoutMs: 8_000 });
    } catch (_) {
      if (!session.naturalEnded && !session.stopping) {
        setPlaybackNote(session, 'The server could not save the latest playback position.');
      }
    } finally {
      session.progressPending = false;
      if (session.progressAgain && !session.stopping) {
        session.progressAgain = false;
        void reportPlaybackProgress(session, true);
      }
    }
  }

  function stopPlaybackSession(session, playedToCompletion = false) {
    if (!session) return Promise.resolve();
    if (session.stopPromise) return session.stopPromise;
    session.stopping = true;
    window.PuffinboxPlaybackHeartbeat.stop(session);
    window.PuffinboxPlaybackHeartbeat.stopNativeStartupWatchdog(session);
    let nativePositionPromise = null;
    if (session.isNative && session.nativePlayer) {
      nativePositionPromise = window.PuffinboxJmpPlayer.getPosition(session.nativePlayer, 1_500)
        .then((position) => { session.nativePositionMs = position; })
        .catch(() => { /* Keep the last position update received from the player. */ });
      session.nativeStopPromise = window.PuffinboxJmpPlayer.stop(session.nativePlayer);
    }
    if (session.isNative) setNativePlayerSurface(false);
    session.nativeAbortController?.abort();
    session.stopPromise = (async () => {
      if (nativePositionPromise) await nativePositionPromise;
      else if (!session.isNative && !playedToCompletion) session.media?.pause();
      const positionTicks = sessionPositionTicks(session);
      if (session.startPromise) await session.startPromise;
      if (session.started) {
        try {
          await request('/Sessions/Playing/Stopped', { ...json('POST', {
            ItemId: session.item.Id,
            PlaySessionId: session.playSessionId,
            PositionTicks: positionTicks,
            PlayedToCompletion: playedToCompletion,
          }), timeoutMs: 8_000 });
          if (session.naturalEnded) setTerminalPlaybackStatus(session, 'Playback finished. Final position saved.');
        } catch (_) {
          if (session.naturalEnded) setTerminalPlaybackStatus(session, 'Playback finished. Final position could not be saved.');
          else setPlaybackNote(session, 'The server could not save the final playback position.');
        }
      }
      if (session.usesHls) {
        const kind = session.mediaType === 'audio' ? 'Audio' : 'Videos';
        const stopPath = session.isLiveTv
          ? `/LiveTv/Channels/${encodeURIComponent(session.item.Id)}/hls/${encodeURIComponent(session.playSessionId)}`
          : `/${kind}/${encodeURIComponent(session.item.Id)}/hls/${encodeURIComponent(session.playSessionId)}`;
        try { await request(stopPath, { method: 'DELETE', timeoutMs: 5_000 }); }
        catch (error) { if (error.status !== 404) setPlaybackNote(session, 'The server could not stop the HLS conversion job.'); }
      }
      if (session.nativeStopPromise) await session.nativeStopPromise;
      const hls = session.hls;
      session.hls = null;
      hls?.destroy();
      if (playedToCompletion && session.media) {
        // Clear HLS.js/native playlist state after the ended event. Browsers
        // can emit a synthetic media error while tearing down that source;
        // the ended handler has already recorded the terminal state.
        session.media.removeAttribute('src');
        session.media.load();
      }
      if (activePlayback === session) activePlayback = null;
      if (activePlayback !== session) nativePlayerControls.hidden = true;
    })();
    return session.stopPromise;
  }

  function finishPlayback(session) {
    if (!session || session.stopping || session.naturalEnded) return session?.stopPromise || Promise.resolve();
    session.naturalEnded = true;
    const queue = playbackQueue;
    const nextItem = queue?.shift() || null;
    if (nextItem) {
      document.querySelector('#player-note').textContent = 'Moving to the next track…';
      return stopPlaybackSession(session, true).then(() => {
        if (playbackQueue === queue && playerDialog.open) {
          void playItem(nextItem, { offerResume: false, preserveQueue: true });
        }
      });
    }
    if (queue) playbackQueue = null;
    const note = document.querySelector('#player-note');
    if (activePlayback === session && session.generation === playbackGeneration) {
      const replay = document.createElement('button');
      replay.type = 'button';
      replay.className = 'button small secondary';
      replay.textContent = 'Play again';
      replay.addEventListener('click', () => {
        void playItem(session.item, { ...selectedTrackOptions(), offerResume: false });
      }, { once: true });
      const finished = document.createElement('section');
      finished.className = 'playback-complete-state';
      finished.setAttribute('role', 'status');
      const heading = document.createElement('h3');
      heading.textContent = 'Playback finished';
      const detail = document.createElement('p');
      detail.textContent = session.item.Name || 'This item has finished playing.';
      finished.append(heading, detail, replay);
      window.PuffinboxClientCompat.replaceChildren(document.querySelector('#player-stage'), finished);
      note.textContent = 'Playback finished.';
    }
    return stopPlaybackSession(session, true);
  }

  function stopActivePlayback(playedToCompletion = false) {
    return stopPlaybackSession(activePlayback, playedToCompletion);
  }

  function setNativePlayerSurface(enabled) {
    const stage = document.querySelector('#player-stage');
    const toggle = (element, className) => {
      if (!element) return;
      if (enabled) element.classList.add(className);
      else element.classList.remove(className);
    };
    toggle(playerDialog, 'native-player-active');
    toggle(stage, 'native-player-active');
    toggle(document.documentElement, 'native-player-active');
    toggle(document.body, 'native-player-active');
  }

  function selectedTrackOptions() {
    const audioValue = audioTrackSelect.value;
    const subtitleValue = subtitleTrackSelect.value;
    return {
      audioStreamIndex: audioValue === '' ? undefined : Number(audioValue),
      subtitleStreamIndex: subtitleValue === '' ? undefined : Number(subtitleValue),
    };
  }

  function changePlaybackTracks() {
    const item = activePlayback?.item || pendingPlayback?.item;
    if (!item) return;
    const session = activePlayback;
    const positionTicks = session ? sessionPositionTicks(session) : 0;
    const autoplay = window.PuffinboxPlaybackHeartbeat.shouldAutoplayAfterTrackChange(session);
    const offerResume = !activePlayback && !!pendingPlayback?.offerResume;
    void playItem(item, { ...selectedTrackOptions(), startTimeTicks: positionTicks, offerResume, autoplay, preserveQueue: !!playbackQueue });
  }

  async function playItem(item, options = {}) {
    if (!options.preserveQueue) playbackQueue = null;
    const generation = ++playbackGeneration;
    const previous = activePlayback;
    if (previous) await stopPlaybackSession(previous, false);
    if (generation !== playbackGeneration) return;
    pendingPlayback = null;
    const title = document.querySelector('#player-title');
    const note = document.querySelector('#player-note');
    title.textContent = item.Name || 'Media';
    note.textContent = '';
    playerResumeControls.hidden = true;
    if (!playerDialog.open) playerDialog.showModal();
    setPlayerLoading('Preparing playback…');
    let session = null;
    try {
      const profile = deviceProfile();
      if (profile.native) {
        try { await restoreMediaAccessToken(); }
        catch (error) {
          const status = Number.isInteger(error?.status) && error.status >= 100 && error.status <= 599
            ? ` (HTTP ${error.status})`
            : '';
          throw new Error(`Native playback authorization is unavailable${status}. Reopen the app while connected and try again.`);
        }
      }
      const payload = { DeviceProfile: profile.DeviceProfile };
      if (Number.isInteger(options.startTimeTicks) && options.startTimeTicks > 0) payload.StartTimeTicks = options.startTimeTicks;
      if (Number.isInteger(options.audioStreamIndex)) payload.AudioStreamIndex = options.audioStreamIndex;
      if (Number.isInteger(options.subtitleStreamIndex)) payload.SubtitleStreamIndex = options.subtitleStreamIndex;
      const playbackInfoPath = `/Items/${encodeURIComponent(item.Id)}/PlaybackInfo`;
      const requestPlaybackInfo = (playbackPayload) => request(playbackInfoPath, json('POST', playbackPayload));
      let playback;
      let subtitleDeliveryFormat = '';
      let subtitleNegotiationDiagnostic = null;
      if (profile.native && Number.isInteger(options.subtitleStreamIndex) && options.subtitleStreamIndex >= 0) {
        const negotiation = await window.PuffinboxJmpPlayer.playbackInfoForSubtitle(requestPlaybackInfo, payload);
        playback = negotiation.playback;
        subtitleDeliveryFormat = negotiation.subtitleDeliveryFormat;
        subtitleNegotiationDiagnostic = negotiation.diagnostic || null;
      } else playback = await requestPlaybackInfo(payload);
      if (generation !== playbackGeneration || !playerDialog.open) return;
      const source = playback?.MediaSources?.[0] || {};
      const streams = Array.isArray(source.MediaStreams) ? source.MediaStreams : [];
      updatePlayerOptions(streams, options);
      const userData = item.Type === 'LiveTvChannel'
        ? null
        : await request(`/Items/${encodeURIComponent(item.Id)}/UserData`).catch(() => null);
      if (generation !== playbackGeneration || !playerDialog.open) return;
      const savedPosition = Number(userData?.PlaybackPositionTicks || 0);
      const canOfferResume = options.offerResume !== false && !(options.startTimeTicks > 0)
        && userData?.Played !== true && savedPosition > 300_000_000;
      if (canOfferResume) {
        pendingPlayback = { item, resumeTicks: savedPosition, offerResume: true };
        playerResumeButton.textContent = `Resume from ${formatPlaybackPosition(savedPosition)}`;
        playerResumeControls.hidden = false;
        playerOptions.hidden = false;
        const message = document.createElement('p');
        message.className = 'player-prompt';
        message.textContent = 'You have a saved position for this item.';
        window.PuffinboxClientCompat.replaceChildren(document.querySelector('#player-stage'), message);
        note.textContent = 'Choose whether to resume or start over.';
        return;
      }

      const type = item.Type === 'Audio' || /audio/i.test(item.MediaType || '') ? 'audio' : 'video';
      let url = null;
      let playbackMode = '';
      if (source.SupportsDirectPlay === true) {
        url = source.DirectStreamUrl || (type === 'audio' ? `/Audio/${encodeURIComponent(item.Id)}/stream` : `/Videos/${encodeURIComponent(item.Id)}/stream`);
        playbackMode = 'DirectPlay';
      } else if (source.SupportsDirectStream === true && source.DirectStreamUrl) {
        url = source.DirectStreamUrl;
        playbackMode = 'DirectStream';
      } else if (source.SupportsTranscoding === true && source.TranscodingUrl && (profile.hlsJs || profile.nativeHls)) {
        url = source.TranscodingUrl;
        playbackMode = 'Transcode';
      }
      if (!url) {
        if (profile.native) {
          const diagnostic = subtitleNegotiationDiagnostic;
          const routeDiagnostic = {
            helperUsed: Boolean(diagnostic),
            nativeHls: profile.nativeHls === true,
            subtitleSelectionRequested: Number.isInteger(options.subtitleStreamIndex) && options.subtitleStreamIndex >= 0,
            requestCount: Number.isInteger(diagnostic?.requestCount) ? diagnostic.requestCount : 0,
            branch: ['direct', 'external-hls', 'selected-subtitle'].includes(diagnostic?.branch)
              ? diagnostic.branch : 'unavailable',
            probeSourcePresent: diagnostic?.probeSourcePresent === true,
            probeCapabilities: diagnostic?.probeCapabilities || null,
            selectedStreamFound: diagnostic?.selectedStreamFound === true,
            selectedStreamText: diagnostic?.selectedStreamText === true,
            selectedStreamExternalSupported: diagnostic?.selectedStreamExternalSupported === true,
            externalProfileFormatAvailable: diagnostic?.externalProfileFormatAvailable === true,
            finalCapabilities: diagnostic?.finalCapabilities || {
              sourcePresent: Boolean(playback?.MediaSources?.[0]),
              directPlay: source.SupportsDirectPlay === true,
              directStream: source.SupportsDirectStream === true,
              directStreamUrlPresent: Boolean(source.DirectStreamUrl),
              transcoding: source.SupportsTranscoding === true,
              transcodingUrlPresent: Boolean(source.TranscodingUrl),
            },
          };
          window.console?.info?.('Native subtitle route diagnostic', routeDiagnostic);
          const playbackError = new Error('The server did not offer a stream supported by this native player.');
          playbackError.nativeSubtitleRouteDiagnostic = routeDiagnostic;
          throw playbackError;
        }
        throw new Error(profile.native
          ? 'The server did not offer a stream supported by this native player.'
          : (profile.nativeHls || profile.hlsJs) ? 'The server did not offer a compatible stream for this browser.' : 'This browser has no verified HLS support and the server did not offer a direct-compatible stream.');
      }
      const mediaUrl = new URL(url, window.location.href);
      if (mediaUrl.origin !== window.location.origin) throw new Error('The server returned a stream URL outside this Puffinbox origin.');
      const media = profile.native ? null : document.createElement(type);
      const shouldAutoplay = options.autoplay !== false;
      if (media) {
        media.controls = true;
        media.autoplay = shouldAutoplay;
        media.playsInline = true;
        media.preload = 'metadata';
      }
      session = {
        item, media, mediaType: type, mode: playbackMode,
        isLiveTv: item.Type === 'LiveTvChannel',
        isNative: profile.native, nativePlayer: null, nativePositionMs: 0, nativeDurationMs: 0,
        nativePositionInitialized: false,
        nativeAbortController: profile.native ? new AbortController() : null,
        playSessionId: String(playback?.PlaySessionId || ''),
        startTimeTicks: Number.isInteger(options.startTimeTicks) ? options.startTimeTicks : 0,
        usesHls: mediaUrl.pathname.toLowerCase().endsWith('/master.m3u8'),
        positionOffsetTicks: mediaUrl.pathname.toLowerCase().endsWith('/master.m3u8')
          && Number.isInteger(options.startTimeTicks) ? options.startTimeTicks : 0,
        started: false, activitySeen: false, startPromise: null, stopPromise: null, stopping: false, hls: null,
        isPaused: !shouldAutoplay,
        lastProgressAt: 0, progressPending: false,
        generation,
      };
      if (!session.playSessionId) throw new Error('The server did not provide a playback session identifier.');
      activePlayback = session;
      if (media) {
        media.addEventListener('playing', () => {
          if (activePlayback === session) session.isPaused = false;
          if (activePlayback === session) session.activitySeen = true;
          if (activePlayback === session) void beginPlaybackSession(session);
        });
        media.addEventListener('timeupdate', () => { void reportPlaybackProgress(session); });
        media.addEventListener('pause', () => {
          session.isPaused = true;
          void reportPlaybackProgress(session, true);
        });
        media.addEventListener('ended', () => { void finishPlayback(session); });
        media.addEventListener('error', () => {
          if (generation !== playbackGeneration || session.stopping || session.naturalEnded) return;
          const originalError = media.error ? { code: media.error.code, message: media.error.message } : null;
          note.textContent = window.PuffinboxClientCompat.mediaErrorDescription(originalError);
          void stopPlaybackSession(session, false);
        });
        if (session.usesHls) {
          media.addEventListener('loadedmetadata', () => startPreparedHlsLease(session), { once: true });
        }
        if (session.startTimeTicks > 0 && !session.usesHls) {
          media.addEventListener('loadedmetadata', () => {
            const seconds = session.startTimeTicks / 10_000_000;
            if (Number.isFinite(seconds) && seconds > 0 && Number.isFinite(media.duration) && seconds < media.duration) media.currentTime = seconds;
          }, { once: true });
        }
      }
      if (!profile.native && !session.usesHls && Number.isInteger(options.subtitleStreamIndex) && options.subtitleStreamIndex >= 0) {
        const subtitle = streams.find((stream) => stream.Type === 'Subtitle' && stream.Index === options.subtitleStreamIndex);
        const subtitleCodec = String(subtitle?.Codec || subtitle?.Format || '').toLowerCase();
        const textSubtitle = subtitle && (subtitle.IsTextSubtitleStream === true || subtitle.SupportsExternalStream === true
          || subtitle.IsExternal === true || ['srt', 'vtt', 'ass', 'ssa', 'subrip', 'webvtt'].includes(subtitleCodec));
        let subtitleUrl = null;
        if (textSubtitle && subtitle.DeliveryUrl) {
          try {
            const delivery = new URL(subtitle.DeliveryUrl, window.location.href);
            const expectedPrefix = `/Videos/${encodeURIComponent(String(item.Id))}/`;
            if (delivery.origin === window.location.origin && delivery.pathname.startsWith(expectedPrefix)
              && delivery.pathname.toLowerCase().endsWith('.vtt')) subtitleUrl = delivery.href;
          } catch (_) { /* Ignore malformed server delivery URLs. */ }
        }
        if (textSubtitle && !subtitleUrl) {
          subtitleUrl = `/Videos/${encodeURIComponent(item.Id)}/${encodeURIComponent(item.Id)}/Subtitles/${encodeURIComponent(String(subtitle.Index))}/Stream.vtt`;
        }
        if (subtitleUrl) {
          const track = document.createElement('track');
          track.kind = 'subtitles';
          track.label = String(subtitle.DisplayTitle || subtitle.Title || subtitle.Language || 'Subtitles');
          track.srclang = String(subtitle.Language || 'und');
          track.src = subtitleUrl;
          track.default = true;
          track.addEventListener('load', () => { track.track.mode = 'showing'; }, { once: true });
          media.append(track);
        }
      }
      const playerStage = document.querySelector('#player-stage');
      if (profile.native) {
        setNativePlayerSurface(true);
        const nativeSurface = document.createElement('div');
        nativeSurface.className = 'native-player-surface';
        nativeSurface.setAttribute('aria-label', type === 'audio' ? 'Audio playing in native player' : 'Video playing in native player');
        window.PuffinboxClientCompat.replaceChildren(playerStage, nativeSurface);
      } else window.PuffinboxClientCompat.replaceChildren(playerStage, media);
      nativePlayerControls.hidden = !profile.native;
      if (profile.native) renderNativePosition(session);
      note.textContent = profile.native
        ? 'Using the client’s native player.'
        : session.usesHls && playbackMode === 'DirectStream' ? 'Playing a server-remuxed HLS stream.'
          : session.usesHls ? 'Playing a server-converted HLS stream.' : '';
      if (!shouldAutoplay) note.textContent = 'Playback is paused. Press play to continue.';
      if (profile.native) {
        nativePlayPauseButton.textContent = shouldAutoplay ? 'Pause' : 'Play';
        const native = window.PuffinboxJmpPlayer;
        const nativeSession = await native.load({
          url: mediaUrl.href,
          accessToken: mediaAccessToken,
          mediaType: type,
          item,
          streams,
          signal: session.nativeAbortController?.signal,
          usesHls: session.usesHls,
          autoplay: shouldAutoplay,
          startTimeMilliseconds: session.usesHls ? 0 : session.startTimeTicks / 10_000,
          subtitleDeliveryFormat,
          subtitleStartTimeMilliseconds: session.usesHls ? session.startTimeTicks / 10_000 : 0,
          audioStreamIndex: options.audioStreamIndex,
          subtitleStreamIndex: options.subtitleStreamIndex,
          onEvent: (eventName, detail, bridgeSession) => {
            if (generation !== playbackGeneration || activePlayback !== session) return;
            session.nativePlayer = bridgeSession;
            if (eventName === 'playing') {
              session.isPaused = false;
              session.activitySeen = true;
              session.nativeActivitySeen = true;
              window.PuffinboxPlaybackHeartbeat.stopNativeStartupWatchdog(session);
              nativePlayPauseButton.textContent = 'Pause';
              void beginPlaybackSession(session);
            } else if (eventName === 'paused') {
              session.isPaused = true;
              nativePlayPauseButton.textContent = 'Play';
              void reportPlaybackProgress(session, true);
            } else if (eventName === 'position') {
              const advanced = window.PuffinboxPlaybackHeartbeat.recordNativePosition(session, detail);
              renderNativePosition(session);
              if (advanced) {
                session.activitySeen = true;
                session.nativeActivitySeen = true;
                window.PuffinboxPlaybackHeartbeat.stopNativeStartupWatchdog(session);
                void beginPlaybackSession(session);
                void reportPlaybackProgress(session);
              } else if (session.started) void reportPlaybackProgress(session);
            } else if (eventName === 'duration') {
              session.nativeDurationMs = detail;
              renderNativePosition(session);
            } else if (eventName === 'finished') {
              void finishPlayback(session);
            } else if (eventName === 'canceled') {
              void stopPlaybackSession(session, false);
            } else if (eventName === 'error') {
              note.textContent = `Native playback stopped: ${detail || 'stream error'}.`;
              void stopPlaybackSession(session, false);
            }
          },
        });
        if (generation !== playbackGeneration || activePlayback !== session || session.stopping || !playerDialog.open) {
          await native.stop(nativeSession);
          return;
        }
        session.nativePlayer = nativeSession;
        if (session.usesHls && !shouldAutoplay) startPreparedHlsLease(session);
        if (shouldAutoplay) {
          window.PuffinboxPlaybackHeartbeat.startNativeStartupWatchdog(session,
            () => activePlayback === session && session.generation === playbackGeneration,
            () => {
              setPlaybackNote(session, 'The native player accepted the request but reported no playback activity. The media may be unsupported by this player.');
              void stopPlaybackSession(session, false);
            });
        }
      } else if (session.usesHls && profile.hlsJs) {
        const hls = new window.Hls({ enableWorker: true });
        session.hls = hls;
        hls.on(window.Hls.Events.ERROR, (_event, data) => {
          if (generation !== playbackGeneration || session.hls !== hls || session.stopping || session.naturalEnded) return;
          if (data.fatal) {
            if (profile.nativeHls && !session.nativeHlsFallbackAttempted) {
              session.nativeHlsFallbackAttempted = true;
              session.nativeHlsFallbackActive = true;
              session.hls = null;
              hls.destroy();
              media.src = mediaUrl.href;
              media.load();
              if (shouldAutoplay) {
                media.play().catch(() => { setPlaybackNote(session, 'Press play to try browser HLS playback.'); });
              }
              setPlaybackNote(session, 'The HLS adapter could not handle this stream; trying browser HLS playback once.');
              return;
            }
            note.textContent = `HLS playback stopped: ${data.details || 'stream error'}.`;
            void stopPlaybackSession(session, false);
          }
        });
        hls.on(window.Hls.Events.MANIFEST_PARSED, () => {
          if (generation !== playbackGeneration) return;
          startPreparedHlsLease(session);
          if (!shouldAutoplay) {
            media.pause();
            setPlaybackNote(session, 'Playback is paused. Press play to continue.');
            return;
          }
          media.play().catch(() => { note.textContent = 'Press play to start. Browser autoplay may be blocked.'; });
        });
        hls.attachMedia(media);
        hls.on(window.Hls.Events.MEDIA_ATTACHED, () => { if (generation === playbackGeneration && !session.stopping) hls.loadSource(mediaUrl.href); });
      } else {
        media.src = mediaUrl.href;
        if (session.usesHls && !shouldAutoplay) startPreparedHlsLease(session);
        if (shouldAutoplay) {
          const playPromise = media.play();
          if (playPromise) playPromise.catch(() => { note.textContent = 'Press play to start. Browser autoplay may be blocked.'; });
        }
      }
    } catch (error) {
      if (generation !== playbackGeneration) return;
      if (session) await stopPlaybackSession(session, false);
      pendingPlayback = null;
      window.PuffinboxClientCompat.replaceChildren(document.querySelector('#player-stage'));
      note.textContent = error.message || 'The server could not prepare playback for this item.';
      if (error?.nativeSubtitleRouteDiagnostic) {
        note.textContent += `\nNative subtitle route diagnostic\n${window.PuffinboxJmpPlayer.formatPlaybackRouteDiagnostic(error.nativeSubtitleRouteDiagnostic)}`;
      }
      showError(error);
    }
  }

  async function addLibrary(event) {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const data = { Name: form.get('Name').trim(), Locations: [form.get('Location').trim()], CollectionType: form.get('CollectionType') };
    try {
      await request('/Library/VirtualFolders', json('POST', data));
      showToast('Library added.');
      await loadBaseData();
      await openScreen('libraries');
    } catch (error) { showError(error); }
  }

  async function loadScanStatus() {
    clearTimeout(scanStatusTimer);
    const target = app.querySelector('#scan-status-list');
    if (!target) return;
    try {
      const response = await request('/Library/ScanStatus');
      const scans = Array.isArray(response) ? response : (Array.isArray(response?.Items) ? response.Items : []);
      window.PuffinboxClientCompat.replaceChildren(target);
      if (!scans.length) {
        target.textContent = 'No library scan has run yet.';
        return;
      }
      const names = new Map(state.libraries.map((library) => [String(library.ItemId || library.Id), library.Name]));
      for (const scan of scans) {
        const row = document.createElement('div');
        row.className = 'data-row';
        const main = document.createElement('div');
        main.className = 'data-row-main';
        const name = document.createElement('strong');
        name.textContent = names.get(String(scan.LibraryId)) || 'Library';
        const details = document.createElement('small');
        const files = Number(scan.FilesSeen || 0);
        const indexed = Number(scan.ItemsIndexed || 0);
        const errors = Number(scan.Errors || 0);
        details.textContent = `${files} files seen · ${indexed} indexed · ${errors} issue${errors === 1 ? '' : 's'}`;
        main.append(name, details);
        const badge = document.createElement('span');
        badge.className = scan.Status === 'completed' ? 'pill good' : 'pill';
        badge.textContent = window.PuffinboxClientCompat.scanStatusLabel(scan.Status);
        row.append(main, badge);
        target.append(row);
      }
      if (scans.some((scan) => scan.Status === 'running')) {
        scanStatusTimer = setTimeout(() => { void loadScanStatus(); }, 2000);
      }
    } catch (error) {
      target.textContent = error.message || 'Scan status could not be loaded.';
    }
  }

  async function refreshLibraries(event) {
    const button = event.currentTarget;
    button.disabled = true;
    try {
      const result = await request('/Library/Refresh', json('POST', {}));
      if (result?.CapacityReached) showToast('The scan queue is full. Try again after a scan finishes.');
      else if (result?.AlreadyRunning) showToast('A library scan is already running.');
      else showToast('Library refresh requested.');
      await loadScanStatus();
    } catch (error) {
      showError(error);
    } finally {
      button.disabled = false;
    }
  }

  async function removeLibrary(name) {
    if (!window.confirm(`Remove the “${name}” library from Puffinbox? The files on disk will stay in place.`)) return;
    try {
      await request(`/Library/VirtualFolders?Name=${encodeURIComponent(name)}`, { method: 'DELETE' });
      showToast('Library removed.');
      await loadBaseData();
      await openScreen('libraries');
    } catch (error) { showError(error); }
  }

  async function saveUser(event) {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const id = event.currentTarget.elements.Id.value;
    const enableAllFolders = event.currentTarget.elements.EnableAllFolders.checked;
    const data = {
      Name: event.currentTarget.elements.Name.value.trim(),
      IsAdministrator: form.has('IsAdministrator'),
      EnableRemoteAccess: form.has('EnableRemoteAccess'),
      EnableMediaPlayback: form.has('EnableMediaPlayback'),
      EnableContentDownloading: form.has('EnableContentDownloading'),
      EnableAllFolders: enableAllFolders,
      BlockUnratedItems: form.getAll('BlockUnratedItems'),
      MaxParentalRating: form.get('MaxParentalRating') === '' ? null : Number(form.get('MaxParentalRating')),
      AllowedLibraryIds: enableAllFolders ? [] : Array.from(app.querySelectorAll('[name="AllowedLibraryIds"]:checked')).map((checkbox) => checkbox.value),
    };
    if (!id) {
      data.Password = form.get('Password');
      const passwordLength = new TextEncoder().encode(data.Password).length;
      if (passwordLength < 12 || passwordLength > 1024) {
        showToast('Use a password between 12 and 1,024 UTF-8 bytes.');
        return;
      }
    }
    try {
      await request(id ? `/Users/${encodeURIComponent(id)}` : '/Users', json('POST', data));
      showToast(id ? 'Account updated.' : 'Account created.');
      await openScreen('users');
    } catch (error) { showError(error); }
  }

  function editUser(id) {
    const user = state.users.find((entry) => String(entry.Id) === String(id));
    if (!user) return;
    const form = app.querySelector('#user-form');
    form.elements.Id.value = user.Id;
    form.elements.Name.value = user.Name;
    form.elements.Name.readOnly = true;
    form.elements.Password.required = false;
    form.elements.Password.placeholder = 'Leave blank to keep current password';
    form.elements.IsAdministrator.checked = !!(user.IsAdministrator || user.Policy?.IsAdministrator);
    form.elements.EnableRemoteAccess.checked = user.Policy?.EnableRemoteAccess !== false;
    form.elements.EnableMediaPlayback.checked = user.Policy?.EnableMediaPlayback !== false;
    form.elements.EnableAllFolders.checked = user.Policy?.EnableAllFolders === true;
    form.elements.MaxParentalRating.value = user.Policy?.MaxParentalRating == null ? '' : String(user.Policy.MaxParentalRating);
    form.elements.EnableContentDownloading.checked = user.Policy?.EnableContentDownloading === true;
    form.querySelector('#user-password-field').hidden = true;
    const allowed = new Set((user.Policy?.EnabledFolders || []).map(String));
    app.querySelectorAll('[name="AllowedLibraryIds"]').forEach((checkbox) => { checkbox.checked = allowed.has(checkbox.value); checkbox.disabled = form.elements.EnableAllFolders.checked; });
    const blocked = new Set(Array.isArray(user.Policy?.BlockUnratedItems) ? user.Policy.BlockUnratedItems : []);
    app.querySelectorAll('[name="BlockUnratedItems"]').forEach((checkbox) => { checkbox.checked = blocked.has(checkbox.value); });
    app.querySelector('#user-form-title').firstChild.textContent = `Manage ${user.Name}`;
    app.querySelector('#user-form-caption').textContent = 'Change account permissions and library access.';
    app.querySelector('#save-user-button').textContent = 'Save changes';
    app.querySelector('#cancel-user-edit').hidden = false;
    form.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  async function removeUser(id) {
    const user = state.users.find((entry) => String(entry.Id) === String(id));
    if (!user || !window.confirm(`Remove the “${user.Name}” account?`)) return;
    try {
      await request(`/Users/${encodeURIComponent(id)}`, { method: 'DELETE' });
      showToast('Account removed.');
      await openScreen('users');
    } catch (error) { showError(error); }
  }

  async function logout() {
    if (!window.confirm('Sign out from this browser?')) return;
    const accountId = String(state.user?.Id || '');
    const cache = window.PuffinboxOfflineCache;
    await cache?.writeSetting('puffinbox-local-signed-out', JSON.stringify({ accountId, generation: state.offlineAccountGeneration || '' }));
    playbackQueue = null;
    playlistSelectionGeneration += 1;
    state.playlists = [];
    state.playlistTotal = 0;
    state.selectedPlaylistId = null;
    state.playlistEntries = [];
    state.playlistEntriesLoading = false;
    state.playlistEntriesError = false;
    state.playlistAudioItems = [];
    state.playlistAudioTotal = 0;
    offlineCacheGeneration += 1;
    for (const controller of offlineDownloadControllers.values()) controller.abort();
    await Promise.allSettled(Array.from(offlineDownloadTasks.values()));
    await stopActivePlayback(false);
    clearTimeout(offlinePollTimer);
    const revokeSession = () => request('/Sessions/Logout', { method: 'POST', timeoutMs: 3_000 });
    const clearLocalCopies = async () => {
      if (!accountId || !cache) return;
      const logoutGeneration = await cache.deactivateAccount(accountId, state.offlineAccountGeneration);
      await cache.forgetAccount(accountId, state.offlineAccountGeneration, logoutGeneration);
    };
    let outcome;
    if (cache?.settleLogout) {
      outcome = await cache.settleLogout(revokeSession, clearLocalCopies);
    } else {
      let serverRevoked = false;
      let localCopiesCleared = false;
      let serverError = null;
      let localError = null;
      try { await revokeSession(); serverRevoked = true; } catch (error) { serverError = error; }
      try { await clearLocalCopies(); localCopiesCleared = true; } catch (error) { localError = error; }
      outcome = { serverRevoked, localCopiesCleared, serverError, localError };
    }
    if (!outcome.serverRevoked) {
      console.warn('Server session revocation could not be confirmed during local sign-out', outcome.serverError);
      showToast('Signed out on this browser. Server session revocation could not be confirmed while offline.');
    }
    if (!outcome.localCopiesCleared) {
      console.warn('Offline copies could not be cleared during local sign-out', outcome.localError);
      showToast('Signed out, but local copies could not be cleared. Remove this site’s stored data to erase them.');
    }
    broadcastOfflineMessage({ type: 'signed-out', accountId, generation: state.offlineAccountGeneration });
    state.offlineAccountGeneration = null;
    if (playerDialog.open) playerDialog.close();
    if (itemDialog.open) itemDialog.close();
    mediaAccessToken = null;
    mediaAccessTokenExpiresAt = 0;
    state.user = null;
    state.items = [];
    state.users = [];
    state.libraries = [];
    showLogin();
  }

  function showAuth(title, description, formMarkup, submitLabel, onSubmit) {
    app.innerHTML = `<main class="auth-screen"><section class="auth-card"><div class="brand-lockup"><span class="brand-mark">p</span><span>puffinbox</span></div><h1>${escapeHtml(title)}</h1><p>${escapeHtml(description)}</p><div id="auth-error"></div><form id="auth-form">${formMarkup}<button class="button" type="submit">${escapeHtml(submitLabel)}</button></form><p class="form-hint" style="margin-top:19px">${escapeHtml(state.startup?.ServerName || state.server?.ServerName || 'Your media server')}</p></section></main>`;
    app.querySelector('#auth-form').addEventListener('submit', async (event) => {
      event.preventDefault();
      const submit = event.currentTarget.querySelector('button[type="submit"]');
      submit.disabled = true;
      const errorBox = app.querySelector('#auth-error');
      errorBox.innerHTML = '';
      try { await onSubmit(new FormData(event.currentTarget)); }
      catch (error) { errorBox.innerHTML = `<div class="error-banner">${escapeHtml(error.message || 'Please check your details and try again.')}</div>`; }
      finally { submit.disabled = false; }
    });
  }

  function showLogin() {
    showAuth('Welcome back', 'Sign in to continue to your library.', `
      <label class="form-field">Username<input name="Username" autocomplete="username" required autofocus></label>
      <label class="form-field">Password<input name="Pw" type="password" autocomplete="current-password" required></label>`, 'Sign in', async (form) => {
      await request('/Users/AuthenticateByName', json('POST', { Username: form.get('Username'), Pw: form.get('Pw'), Client: 'Puffinbox Web', DeviceName: 'Web browser', DeviceId: window.PuffinboxDeviceId.get(), Version: '0.1.0' }));
      mediaAccessToken = null;
      mediaAccessTokenExpiresAt = 0;
      await window.PuffinboxOfflineCache?.writeSetting('puffinbox-local-signed-out', '');
      await enterApp();
    });
  }

  function broadcastOfflineMessage(message) {
    try { offlineChannel?.postMessage({ ...message, sender: offlineTabId }); } catch (_) { /* Cross-tab refresh is optional. */ }
  }

  async function handleOfflineSessionChange(message) {
    if (!message || message.sender === offlineTabId) return;
    const accountId = String(state.user?.Id || '');
    if (message.type === 'signed-out' || message.type === 'account-session') offlineSessionRevision += 1;
    if (message.type === 'cache-changed') {
      if (accountId && accountId === String(message.accountId) && state.screen === 'offline') {
        await loadOfflineData(true).catch(() => {});
      }
      return;
    }
    if (message.type !== 'signed-out' && message.type !== 'account-session') return;
    if (!accountId) return;
    if (message.type === 'signed-out' && String(message.accountId || '') !== accountId) {
      const currentOfflineSession = await window.PuffinboxOfflineCache?.getActiveSession().catch(() => null);
      if (currentOfflineSession?.accountId === accountId
          && currentOfflineSession.generation === state.offlineAccountGeneration) return;
    }
    if (message.type === 'account-session' && accountId === String(message.accountId)
        && state.offlineAccountGeneration === message.generation) return;
    offlineCacheGeneration += 1;
    for (const controller of offlineDownloadControllers.values()) controller.abort();
    await Promise.allSettled(Array.from(offlineDownloadTasks.values()));
    await stopActivePlayback(false);
    mediaAccessToken = null;
    mediaAccessTokenExpiresAt = 0;
    state.user = null;
    state.offlineAccountGeneration = null;
    state.offlineCache = [];
    state.offlineServerPackages = [];
    showLogin();
    showToast('This browser account changed in another tab. Sign in again to continue.');
  }

  offlineChannel?.addEventListener('message', (event) => {
    void handleOfflineSessionChange(event.data);
  });

  function showSetup() {
    showAuth('Set up your server', 'Create the first administrator account. Enter the one-time setup token shown in the server data directory.', `
      <label class="form-field">Setup token<input name="SetupToken" autocomplete="off" required></label>
      <label class="form-field">Administrator name<input name="Username" autocomplete="username" required autofocus></label>
      <label class="form-field">Password<input name="Password" type="password" autocomplete="new-password" minlength="12" maxlength="1024" required><span class="form-hint">At least 12 UTF-8 bytes.</span></label>
      <p class="form-hint">The token is generated by the server and never shown in this page. An operator can also set PUFFINBOX_SETUP_TOKEN before first start.</p>`, 'Create administrator', async (form) => {
      const passwordLength = new TextEncoder().encode(form.get('Password')).length;
      if (passwordLength < 12 || passwordLength > 1024) throw new Error('Use a password between 12 and 1,024 UTF-8 bytes.');
      await request('/Startup/User', json('POST', { Username: form.get('Username'), Password: form.get('Password'), SetupToken: form.get('SetupToken') }));
      mediaAccessToken = null;
      mediaAccessTokenExpiresAt = 0;
      await enterApp();
    });
  }

  function showStartupError(error, category = 'server') {
    const copy = window.PuffinboxClientCompat.startupFailure(error, category);
    app.innerHTML = `<main class="auth-screen"><section class="auth-card"><div class="brand-lockup"><span class="brand-mark">p</span><span>puffinbox</span></div><h1>${escapeHtml(copy.title)}</h1><p>${escapeHtml(copy.description)}</p><div class="error-banner">${escapeHtml(copy.detail)}</div><button id="retry-boot" class="button" style="width:100%;margin-top:18px">Try again</button></section></main>`;
    app.querySelector('#retry-boot').addEventListener('click', boot);
  }

  async function enterApp() {
    try {
      await loadBaseData();
    } catch (error) {
      if (error.status === 401) { mediaAccessToken = null; mediaAccessTokenExpiresAt = 0; state.user = null; showLogin(); return; }
      if (error.name === 'OfflineAccountChangedError') { mediaAccessToken = null; mediaAccessTokenExpiresAt = 0; state.user = null; showLogin(); return; }
      if (await showOfflineStartup(error)) return;
      showStartupError(error, 'server');
      return;
    }
    state.screen = 'home';
    try { await render(); }
    catch (error) { showStartupError(error, 'render'); }
  }

  async function boot() {
    app.innerHTML = '<main class="boot-screen"><span class="brand-mark">p</span><p>Opening your library…</p></main>';
    void ensureOfflineServiceWorker();
    try {
      state.startup = await request('/Startup/Configuration');
    } catch (error) {
      if (await showOfflineStartup(error)) return;
      showStartupError(error, 'server');
      return;
    }
    try {
      if (!state.startup?.IsStartupWizardCompleted) { showSetup(); return; }
      const localSignOut = await window.PuffinboxOfflineCache?.readSetting('puffinbox-local-signed-out');
      const offlineSession = await window.PuffinboxOfflineCache?.getActiveSession().catch(() => null);
      const locallySignedOut = window.PuffinboxOfflineCache?.localSignOutApplies(localSignOut, offlineSession) || false;
      if (locallySignedOut || (offlineSession && !offlineSession.accountId && offlineSession.generation)) {
        showLogin();
        return;
      }
      await enterApp();
      if (!state.user && !app.querySelector('#auth-form') && !app.querySelector('#retry-boot')) showLogin();
    } catch (error) { showStartupError(error, 'render'); }
  }

  playerDialog.addEventListener('close', () => {
    playbackGeneration += 1;
    offlinePlaybackGeneration += 1;
    pendingPlayback = null;
    playbackQueue = null;
    void stopActivePlayback(false);
    const media = playerDialog.querySelector('video, audio');
    if (media) { media.pause(); media.removeAttribute('src'); media.load(); }
    if (offlinePlaybackObjectUrl) URL.revokeObjectURL(offlinePlaybackObjectUrl);
    offlinePlaybackObjectUrl = null;
    playerResumeControls.hidden = true;
    audioTrackControl.hidden = true;
    subtitleTrackControl.hidden = true;
    playerOptions.hidden = true;
  });

  itemDialog.addEventListener('close', () => {
    photoLoadController?.abort();
    photoLoadController = null;
    if (photoObjectUrl) URL.revokeObjectURL(photoObjectUrl);
    photoObjectUrl = null;
    const photo = itemDialog.querySelector('#photo-view');
    if (photo) delete photo.dataset.objectUrl;
  });

  itemDialog.addEventListener('click', (event) => { if (event.target === itemDialog) itemDialog.close(); });
  nativePlayPauseButton.addEventListener('click', () => {
    if (!activePlayback?.isNative || !activePlayback.nativePlayer) return;
    const action = nativePlayPauseButton.textContent === 'Pause' ? 'pause' : 'play';
    if (!window.PuffinboxJmpPlayer.control(activePlayback.nativePlayer, action)) {
      setPlaybackNote(activePlayback, 'The native player does not support this control.');
    }
  });
  nativeSeek.addEventListener('change', () => {
    if (!activePlayback?.isNative || !activePlayback.nativePlayer) return;
    const position = Number(nativeSeek.value);
    if (!window.PuffinboxJmpPlayer.seek(activePlayback.nativePlayer, position)) {
      setPlaybackNote(activePlayback, 'Seeking is unavailable in the native player.');
      return;
    }
    activePlayback.nativePositionMs = position;
    renderNativePosition(activePlayback);
    void reportPlaybackProgress(activePlayback, true);
  });
  setTheme(window.PuffinboxDeviceId.readSetting(() => window.localStorage, 'puffinbox-theme', 'dark'));
  boot();
})();
