(() => {
  'use strict';

  const model = {
    user: null,
    libraries: [],
    channels: [],
    programs: [],
    timers: [],
    seriesTimers: [],
    recordings: [],
    sources: [],
    editingSourceId: null,
    errors: {},
    loading: false,
    guideLoading: false,
    guideError: null,
    selectedChannelId: null,
    guideTotal: 0,
    editingTimerId: null,
    editingSeriesTimerId: null,
    generation: 0,
    guideGeneration: 0,
  };

  function escapeHtml(value) {
    return String(value ?? '').replace(/[&<>"']/g, (char) => ({
      '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
    })[char]);
  }

  function isAdmin(user) {
    return user?.Policy?.IsAdministrator === true || user?.IsAdministrator === true;
  }

  function canAccess(user) {
    return isAdmin(user) || user?.Policy?.EnableLiveTvAccess === true;
  }

  function canPlayLiveTv(user) {
    return canAccess(user) && user?.Policy?.EnableMediaPlayback !== false;
  }

  function canManageTimers(user) {
    return canAccess(user)
      && user?.Policy?.EnableMediaPlayback !== false
      && (isAdmin(user) || user?.Policy?.EnableLiveTvManagement === true);
  }

  function pageHeading(title, description, action) {
    return '<div class="page-heading"><div><h1>' + escapeHtml(title) + '</h1><p>'
      + escapeHtml(description) + '</p></div>' + (action || '') + '</div>';
  }

  function emptyState(title, description, symbol) {
    return '<div class="empty-state"><span class="empty-icon">' + escapeHtml(symbol || '◉')
      + '</span><strong>' + escapeHtml(title) + '</strong><p>' + escapeHtml(description) + '</p></div>';
  }

  function errorState(message) {
    return '<div class="error-banner live-tv-error" role="alert">' + escapeHtml(message)
      + '<button class="button secondary small" type="button" data-live-tv-refresh>Retry</button></div>';
  }

  function formatTime(value) {
    const date = new Date(value);
    if (!Number.isFinite(date.getTime())) return 'Time unavailable';
    return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(date);
  }

  function formatDateTimeLocal(date) {
    const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000);
    return local.toISOString().slice(0, 16);
  }

  function formatDateTimeLocalPrecise(value) {
    const date = new Date(value);
    if (!Number.isFinite(date.getTime())) return '';
    const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000);
    return local.toISOString().slice(0, 23);
  }

  function editedDateValue(form, fieldName, parsedDate) {
    const original = form.dataset[fieldName === 'StartDate' ? 'originalStartDate' : 'originalEndDate'];
    return original && form.elements[fieldName].value === formatDateTimeLocalPrecise(original)
      ? original
      : parsedDate.toISOString();
  }

  function liveTvEditFormError(form, message) {
    const output = form.querySelector('[data-live-tv-edit-error]');
    if (!output) return;
    output.textContent = message;
    output.hidden = false;
  }

  function timerEditValues(form, isSeries) {
    const values = new FormData(form);
    const name = String(values.get('Name') || '').trim();
    const start = new Date(String(values.get('StartDate') || ''));
    const end = new Date(String(values.get('EndDate') || ''));
    const pre = Number(values.get('PrePaddingSeconds'));
    const post = Number(values.get('PostPaddingSeconds'));
    if (!name || name.length > 512 || /[\u0000-\u001f\u007f]/.test(name)) {
      throw new Error('Timer name must contain 1 to 512 printable characters.');
    }
    if (!Number.isFinite(start.getTime()) || !Number.isFinite(end.getTime())) {
      throw new Error('Choose a valid start and end time.');
    }
    if (!Number.isInteger(pre) || pre < 0 || pre > 3600 || !Number.isInteger(post) || post < 0 || post > 3600) {
      throw new Error('Padding must be between 0 and 3,600 seconds.');
    }
    const now = Date.now();
    if (start.getTime() <= now - 2 * 60_000) {
      throw new Error('The start time must be no more than two minutes in the past.');
    }
    if (isSeries && start.getTime() > now + 31 * 24 * 60 * 60_000) {
      throw new Error('A series timer must start within the next 31 days.');
    }
    if (end <= start || end.getTime() - start.getTime() + (pre + post) * 1000 > 4 * 60 * 60_000) {
      throw new Error('The timer window plus padding must be no longer than four hours.');
    }
    return { values, name, start, end, pre, post };
  }

  const weekdayNames = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];

  function seriesDayPattern(timer) {
    const pattern = String(timer.DayPattern || '').toLowerCase();
    if (!Array.isArray(timer.Days) && ['daily', 'weekdays', 'weekends'].includes(pattern)) {
      return pattern[0].toUpperCase() + pattern.slice(1);
    }
    return Array.isArray(timer.Days) ? 'Custom' : 'Daily';
  }

  function seriesDaySelection(timer) {
    if (Array.isArray(timer.Days)) {
      const days = new Set(timer.Days.map((day) => String(day).toLowerCase()));
      return weekdayNames.filter((day) => days.has(day.toLowerCase()));
    }
    const pattern = seriesDayPattern(timer);
    if (pattern === 'Weekdays') return weekdayNames.slice(1, 6);
    if (pattern === 'Weekends') return [weekdayNames[0], weekdayNames[6]];
    return weekdayNames.slice();
  }

  function timerEditForm(timer) {
    const id = String(timer.Id || '');
    const name = String(timer.Name || '');
    const programName = timer.ProgramId ? ' readonly aria-describedby="live-tv-program-name-help"' : '';
    return '<form class="live-tv-edit-form" data-live-tv-edit-timer-form="' + escapeHtml(id)
      + '" data-original-start-date="' + escapeHtml(timer.StartDate || '') + '" data-original-end-date="' + escapeHtml(timer.EndDate || '')
      + '" aria-label="Edit timer ' + escapeHtml(name || 'Untitled timer') + '">'
      + '<label class="form-field">Timer name<input name="Name" required maxlength="512" autocomplete="off" value="' + escapeHtml(name) + '"' + programName + '></label>'
      + (timer.ProgramId ? '<span class="form-hint" id="live-tv-program-name-help">The linked guide entry controls this title.</span>' : '')
      + '<label class="form-field">Start<input name="StartDate" type="datetime-local" step="0.001" required value="' + escapeHtml(formatDateTimeLocalPrecise(timer.StartDate)) + '"></label>'
      + '<label class="form-field">End<input name="EndDate" type="datetime-local" step="0.001" required value="' + escapeHtml(formatDateTimeLocalPrecise(timer.EndDate)) + '"></label>'
      + '<label class="form-field">Padding before (seconds)<input name="PrePaddingSeconds" type="number" required min="0" max="3600" step="1" value="' + escapeHtml(timer.PrePaddingSeconds ?? 0) + '"></label>'
      + '<label class="form-field">Padding after (seconds)<input name="PostPaddingSeconds" type="number" required min="0" max="3600" step="1" value="' + escapeHtml(timer.PostPaddingSeconds ?? 0) + '"></label>'
      + '<div class="live-tv-edit-actions"><button class="button" type="submit">Save changes</button><button class="button secondary" type="button" data-live-tv-cancel-edit-timer="' + escapeHtml(id) + '">Cancel</button></div>'
      + '<p class="live-tv-edit-error" data-live-tv-edit-error role="alert" aria-live="assertive" hidden></p></form>';
  }

  function seriesTimerEditForm(timer) {
    const id = String(timer.Id || '');
    const name = String(timer.Name || '');
    const pattern = seriesDayPattern(timer);
    const selectedDays = new Set(seriesDaySelection(timer));
    const dayChoices = weekdayNames.map((day) => '<label><input type="checkbox" name="Days" value="' + day + '"'
      + (selectedDays.has(day) ? ' checked' : '') + (pattern === 'Custom' ? '' : ' disabled') + '> ' + day + '</label>').join('');
    const disabled = pattern === 'Custom' ? '' : ' disabled';
    return '<form class="live-tv-edit-form live-tv-series-edit-form" data-live-tv-edit-series-form="' + escapeHtml(id)
      + '" data-original-start-date="' + escapeHtml(timer.StartDate || '') + '" data-original-end-date="' + escapeHtml(timer.EndDate || '')
      + '" aria-label="Edit series timer ' + escapeHtml(name || 'Untitled series timer') + '">'
      + '<label class="form-field">Timer name<input name="Name" required maxlength="512" autocomplete="off" value="' + escapeHtml(name) + '"></label>'
      + '<label class="form-field">Start<input name="StartDate" type="datetime-local" step="0.001" required value="' + escapeHtml(formatDateTimeLocalPrecise(timer.StartDate)) + '"></label>'
      + '<label class="form-field">End<input name="EndDate" type="datetime-local" step="0.001" required value="' + escapeHtml(formatDateTimeLocalPrecise(timer.EndDate)) + '"></label>'
      + '<label class="form-field">Padding before (seconds)<input name="PrePaddingSeconds" type="number" required min="0" max="3600" step="1" value="' + escapeHtml(timer.PrePaddingSeconds ?? 0) + '"></label>'
      + '<label class="form-field">Padding after (seconds)<input name="PostPaddingSeconds" type="number" required min="0" max="3600" step="1" value="' + escapeHtml(timer.PostPaddingSeconds ?? 0) + '"></label>'
      + '<label class="form-field">Repeat days<select name="DayPattern" aria-label="Repeat days"><option value="Daily"' + (pattern === 'Daily' ? ' selected' : '')
      + '>Daily</option><option value="Weekdays"' + (pattern === 'Weekdays' ? ' selected' : '') + '>Weekdays</option><option value="Weekends"'
      + (pattern === 'Weekends' ? ' selected' : '') + '>Weekends</option><option value="Custom"' + (pattern === 'Custom' ? ' selected' : '') + '>Choose days</option></select></label>'
      + '<fieldset class="live-tv-edit-days"' + disabled + '><legend>Custom days</legend>' + dayChoices + '</fieldset>'
      + '<div class="live-tv-edit-actions"><button class="button" type="submit">Save changes</button><button class="button secondary" type="button" data-live-tv-cancel-edit-series="' + escapeHtml(id) + '">Cancel</button></div>'
      + '<p class="live-tv-edit-error" data-live-tv-edit-error role="alert" aria-live="assertive" hidden></p></form>';
  }

  function itemList(result) {
    return Array.isArray(result?.Items) ? result.Items : [];
  }

  function selectedChannel() {
    return model.channels.find((channel) => String(channel.Id) === String(model.selectedChannelId)) || null;
  }

  function channelRows() {
    if (model.loading && !model.channels.length && !model.errors.channels) {
      return '<div class="loading-row" aria-live="polite">Loading channels…</div>';
    }
    if (model.errors.channels) return errorState(model.errors.channels);
    if (!model.channels.length) {
      return emptyState('No channels available', 'Channels from enabled IPTV sources will appear here after an administrator refreshes a source.', '◉');
    }
    return '<div class="live-tv-channel-list" role="list">' + model.channels.map((channel) => {
      const id = String(channel.Id || '');
      const selected = id === String(model.selectedChannelId || '');
      const name = channel.Name || 'Untitled channel';
      const summary = channel.Overview || 'Channel guide';
      return '<button type="button" class="live-tv-channel' + (selected ? ' selected' : '')
        + '" data-live-tv-channel="' + escapeHtml(id) + '" aria-pressed="' + selected + '"><strong>'
        + escapeHtml(name) + '</strong><small>' + escapeHtml(summary) + '</small></button>';
    }).join('') + '</div>';
  }

  function programRows() {
    const channel = selectedChannel();
    if (!channel) return emptyState('Choose a channel', 'Select a channel to inspect its guide.', '◉');
    if (model.guideLoading) return '<div class="loading-row" aria-live="polite">Loading guide…</div>';
    if (model.guideError) return errorState(model.guideError);
    if (!model.programs.length) {
      return emptyState('No guide data available', 'This channel has no programmes in the current guide window.', '◷');
    }
    const canManage = canManageTimers(model.user);
    const rows = model.programs.map((program) => {
      const id = String(program.Id || '');
      const title = program.Name || 'Untitled programme';
      const metadata = [program.Category, program.OfficialRating].filter(Boolean).join(' · ');
      const detail = [formatTime(program.StartDate), formatTime(program.EndDate), metadata]
        .filter(Boolean).join(' · ');
      const actions = canManage
        ? '<div class="data-row-actions live-tv-program-actions"><button class="button secondary small" type="button" data-live-tv-record-program="' + escapeHtml(id) + '">Record once</button>'
          + '<label class="live-tv-repeat-label">Repeat<select class="inline-input" name="DayPattern" aria-label="Repeat days for ' + escapeHtml(title) + '"><option value="Daily">Daily</option><option value="Weekdays">Weekdays</option><option value="Weekends">Weekends</option></select></label>'
          + '<button class="button secondary small" type="button" data-live-tv-record-series="' + escapeHtml(id) + '">Record series</button></div>'
        : '';
      const description = program.Overview
        ? '<p class="live-tv-program-description">' + escapeHtml(program.Overview) + '</p>'
        : '';
      return '<article class="live-tv-program"><div class="data-row-main"><strong>' + escapeHtml(title)
        + '</strong><small>' + escapeHtml(detail) + '</small>' + description + '</div>' + actions + '</article>';
    }).join('');
    const more = model.guideTotal > model.programs.length
      ? '<p class="form-hint">Showing ' + model.programs.length + ' of ' + model.guideTotal + ' programmes for this channel.</p>'
      : '';
    return '<div class="live-tv-program-list">' + rows + '</div>' + more;
  }

  function manualTimerForm() {
    if (!canManageTimers(model.user)) {
      return '<p class="form-hint">This account can browse Live TV but cannot manage recording timers.</p>';
    }
    const options = model.channels.map((channel) => {
      const id = String(channel.Id || '');
      return '<option value="' + escapeHtml(id) + '"' + (id === String(model.selectedChannelId || '') ? ' selected' : '')
        + '>' + escapeHtml(channel.Name || 'Untitled channel') + '</option>';
    }).join('');
    const start = new Date(Date.now() + 30 * 60_000);
    start.setSeconds(0, 0);
    const end = new Date(start.getTime() + 60 * 60_000);
    const disabled = !model.channels.length ? ' disabled' : '';
    return '<form id="live-tv-manual-timer-form" class="live-tv-manual-form"><label class="form-field">Timer name'
      + '<input name="Name" required maxlength="512" autocomplete="off" placeholder="Programme recording"></label>'
      + '<label class="form-field">Channel<select name="ChannelId" required' + disabled + '>' + options + '</select></label>'
      + '<label class="form-field">Start<input name="StartDate" type="datetime-local" required value="' + escapeHtml(formatDateTimeLocal(start)) + '"></label>'
      + '<label class="form-field">End<input name="EndDate" type="datetime-local" required value="' + escapeHtml(formatDateTimeLocal(end)) + '"></label>'
      + '<button class="button" type="submit"' + disabled + '>Create manual timer</button></form>'
      + '<p class="form-hint">Manual timers record the selected channel at the chosen time. A timer must be no longer than four hours.</p>';
  }

  function timerRows() {
    if (model.loading && !model.timers.length && !model.errors.timers) {
      return '<div class="loading-row" aria-live="polite">Loading timers…</div>';
    }
    if (model.errors.timers) return errorState(model.errors.timers);
    if (!model.timers.length) {
      return emptyState('No upcoming timers', 'One off timers and active recordings will appear here.', '◷');
    }
    return '<div class="data-list">' + model.timers.map((timer) => {
      const id = String(timer.Id || '');
      const status = String(timer.Status || 'Unknown');
      const canCancel = canManageTimers(model.user) && (status === 'New' || status === 'InProgress');
      const canEdit = canManageTimers(model.user) && status === 'New' && !timer.SeriesTimerId;
      const schedule = formatTime(timer.StartDate) + ' – ' + formatTime(timer.EndDate);
      const actions = canCancel ? '<div class="data-row-actions">'
        + (canEdit ? '<button class="button secondary small" type="button" data-live-tv-edit-timer="' + escapeHtml(id) + '" aria-expanded="' + (String(model.editingTimerId) === id) + '">Edit</button>' : '')
        + '<button class="button danger small" type="button" data-live-tv-cancel-timer="' + escapeHtml(id) + '">Cancel</button></div>' : '';
      const detail = [timer.ChannelName, schedule, status, timer.LastErrorCode].filter(Boolean).join(' · ');
      return '<div class="live-tv-row-group"><div class="data-row live-tv-timer-row"><div class="data-row-main"><strong>' + escapeHtml(timer.Name || 'Untitled timer')
        + '</strong><small>' + escapeHtml(detail) + '</small></div>' + actions + '</div>'
        + (String(model.editingTimerId) === id && canEdit ? timerEditForm(timer) : '') + '</div>';
    }).join('') + '</div>';
  }

  function seriesTimerRows() {
    if (model.loading && !model.seriesTimers.length && !model.errors.seriesTimers) {
      return '<div class="loading-row" aria-live="polite">Loading series timers…</div>';
    }
    if (model.errors.seriesTimers) return errorState(model.errors.seriesTimers);
    if (!model.seriesTimers.length) {
      return emptyState('No series timers', 'Repeat rules you create from the guide will appear here.', '↻');
    }
    return '<div class="data-list">' + model.seriesTimers.map((timer) => {
      const id = String(timer.Id || '');
      const pattern = timer.DayPattern || (Array.isArray(timer.Days) ? timer.Days.join(', ') : 'Daily');
      const detail = [timer.ChannelName, pattern, formatTime(timer.StartDate)].filter(Boolean).join(' · ');
      const canManage = canManageTimers(model.user);
      const actions = canManage
        ? '<div class="data-row-actions"><button class="button secondary small" type="button" data-live-tv-edit-series="' + escapeHtml(id) + '" aria-expanded="' + (String(model.editingSeriesTimerId) === id) + '">Edit</button><button class="button danger small" type="button" data-live-tv-cancel-series="' + escapeHtml(id) + '">Cancel rule</button></div>'
        : '';
      return '<div class="live-tv-row-group"><div class="data-row live-tv-series-row"><div class="data-row-main"><strong>' + escapeHtml(timer.Name || 'Untitled series timer')
        + '</strong><small>' + escapeHtml(detail) + '</small></div>' + actions + '</div>'
        + (String(model.editingSeriesTimerId) === id && canManage ? seriesTimerEditForm(timer) : '') + '</div>';
    }).join('') + '</div>';
  }

  function recordingRows() {
    if (model.loading && !model.recordings.length && !model.errors.recordings) {
      return '<div class="loading-row" aria-live="polite">Loading recordings…</div>';
    }
    if (model.errors.recordings) return errorState(model.errors.recordings);
    if (!model.recordings.length) {
      return emptyState('No recordings yet', 'Finished and in progress recordings for this account will appear here.', '▣');
    }
    return '<div class="data-list">' + model.recordings.map((recording) => {
      const detail = [recording.ChannelName, String(recording.Status || 'Unknown'), formatTime(recording.StartDate),
        Number.isFinite(Number(recording.ByteCount)) ? formatBytes(recording.ByteCount) : '', recording.ErrorCode]
        .filter(Boolean).join(' · ');
      return '<div class="data-row"><div class="data-row-main"><strong>' + escapeHtml(recording.Name || 'Untitled recording')
        + '</strong><small>' + escapeHtml(detail) + '</small></div></div>';
    }).join('') + '</div>';
  }

  function formatBytes(value) {
    const bytes = Number(value);
    if (!Number.isFinite(bytes) || bytes < 0) return '';
    if (bytes < 1024) return bytes + ' B';
    const units = ['KiB', 'MiB', 'GiB', 'TiB'];
    let size = bytes;
    let unit = -1;
    do { size /= 1024; unit += 1; } while (size >= 1024 && unit < units.length - 1);
    return size.toFixed(size >= 100 ? 0 : size >= 10 ? 1 : 2) + ' ' + units[unit];
  }

  function sourceRows() {
    if (model.loading && !model.sources.length && !model.errors.sources) {
      return '<div class="loading-row" aria-live="polite">Loading sources…</div>';
    }
    if (model.errors.sources) return errorState(model.errors.sources);
    if (!model.sources.length) {
      return emptyState('No IPTV sources', 'Add a pinned M3U source below, then refresh it to load channels and guide data.', '◉');
    }
    return '<div class="data-list">' + model.sources.map((source) => {
      const id = String(source.Id || '');
      const disabled = source.Enabled === false;
      const status = source.RefreshStatus || 'Unknown';
      const refreshed = source.LastRefreshedAt ? formatTime(source.LastRefreshedAt) : 'Not refreshed yet';
      const detail = [status, refreshed, source.LastErrorCode].filter(Boolean).join(' · ');
      const action = '<div class="data-row-actions"><span class="pill' + (status === 'ready' ? ' good' : '') + '">'
        + escapeHtml(status) + '</span><button class="button secondary small" type="button" data-live-tv-refresh-source="' + escapeHtml(id) + '"' + (disabled ? ' disabled' : '') + '>Refresh</button>'
        + '<button class="button secondary small" type="button" data-live-tv-edit-source="' + escapeHtml(id) + '">Edit</button>'
        + '<button class="button secondary small" type="button" data-live-tv-delete-source="' + escapeHtml(id) + '">Delete</button></div>';
      return '<div class="data-row"><div class="data-row-main"><strong>' + escapeHtml(source.Name || 'Untitled source')
        + '</strong><small>' + escapeHtml(detail) + '</small></div>' + action + '</div>'
        + (model.editingSourceId === id ? sourceForm(source) : '');
    }).join('') + '</div>';
  }

  function sourceForm(source = null) {
    const editing = Boolean(source);
    const sourceId = editing ? String(source.Id || '') : '';
    const formId = editing ? 'live-tv-source-edit-form' : 'live-tv-source-form';
    const formName = editing ? 'Edit IPTV source' : 'Add an IPTV source';
    const libraries = model.libraries.map((library) => {
      const id = String(library.Id || library.ItemId || '');
      return '<option value="' + escapeHtml(id) + '"' + (editing && id === String(source.LibraryId || '') ? ' selected' : '')
        + '>' + escapeHtml(library.Name || 'Library') + '</option>';
    }).join('');
    const noLibraries = !model.libraries.length;
    return '<section class="panel"><div class="panel-title"><div>' + formName
      + '<p>' + (editing
        ? 'Feed URLs and origin pins are never shown. Leave fields blank to keep their settings. Include new pins when a URL uses a different origin; use the checkbox to clear the guide.'
        : 'Use an enabled library as the recording destination.') + '</p></div></div>'
      + (noLibraries ? '<p class="form-hint">Create a library before adding an IPTV source.</p>' : '')
      + '<form id="' + formId + '" class="live-tv-source-form"' + (editing ? ' data-live-tv-source-id="' + escapeHtml(sourceId) + '"' : '') + '>'
      + '<label class="form-field">Source name<input name="Name" required maxlength="160" placeholder="Living room channels" value="' + escapeHtml(editing ? source.Name || '' : '') + '"' + (noLibraries ? ' disabled' : '') + '></label>'
      + '<label class="form-field">Recording library<select name="LibraryId" required' + (noLibraries ? ' disabled' : '') + '><option value=""' + (!editing ? ' selected' : '') + '>Choose a library</option>' + libraries + '</select></label>'
      + '<label class="form-field">M3U playlist URL<input name="PlaylistUrl" type="url"' + (editing ? '' : ' required') + ' maxlength="8192" placeholder="' + (editing ? 'Leave blank to keep the current URL' : 'https://tv.example/channels.m3u') + '"' + (noLibraries ? ' disabled' : '') + '></label>'
      + '<label class="form-field">XMLTV guide URL (optional)<input name="GuideUrl" type="url" maxlength="8192" placeholder="' + (editing ? 'Leave blank to keep the current guide URL' : 'https://tv.example/guide.xml') + '"' + (noLibraries ? ' disabled' : '') + '></label>'
      + (editing ? '<label class="form-field"><span><input name="ClearGuideUrl" type="checkbox"> Clear the current XMLTV guide URL</span></label>' : '')
      + '<label class="form-field">Pinned origin<input name="Origin" type="url"' + (editing ? '' : ' required') + ' placeholder="' + (editing ? 'Leave blank to keep current pins' : 'https://tv.example/') + '"' + (noLibraries ? ' disabled' : '') + '><span class="form-hint">' + (editing ? 'Leave the origin and addresses blank to keep current pins. Enter both to replace them.' : 'Enter the exact HTTP(S) origin only, with no path, query or fragment.') + '</span></label>'
      + '<label class="form-field">Pinned server IP addresses<textarea name="Addresses"' + (editing ? '' : ' required') + ' rows="3" placeholder="' + (editing ? 'Leave blank to keep current pins' : '203.0.113.10') + '" maxlength="1024"' + (noLibraries ? ' disabled' : '') + '></textarea><span class="form-hint">Enter 1 to 16 literal IP addresses, one per line. The server pins feed requests to these addresses and rejects redirects.</span></label>'
      + '<div class="live-tv-edit-actions"><button class="button" type="submit"' + (noLibraries ? ' disabled' : '') + '>' + (editing ? 'Save source' : 'Add source') + '</button>'
      + (editing ? '<button class="button secondary" type="button" data-live-tv-cancel-source-edit>Cancel</button>' : '')
      + '</div></form></section>';
  }

  function render(user, libraries) {
    model.user = user;
    model.libraries = Array.isArray(libraries) ? libraries : [];
    if (!canAccess(user)) return '';
    const channel = selectedChannel();
    const guideHeading = channel ? escapeHtml(channel.Name || 'Channel guide') : 'Channel guide';
    const loadingNote = model.loading ? '<p class="form-hint live-tv-refreshing" aria-live="polite">Refreshing Live TV data…</p>' : '';
    const guideDateRange = '<p class="form-hint">Showing guide entries from the last two hours through the next seven days.</p>';
    const playbackAction = channel && canPlayLiveTv(user)
      ? '<button class="button small" type="button" data-live-tv-play="' + escapeHtml(channel.Id) + '">Watch live</button>'
      : '';
    const sources = isAdmin(user)
      ? '<section class="panel"><div class="panel-title"><div>IPTV sources<p>Feed URLs and origin pins are stored server-side and are not shown in the source list.</p></div></div>'
        + sourceRows() + '</section>' + sourceForm()
      : '';
    return pageHeading('Live TV', 'Watch live channels, browse guide data, and manage recordings.',
      playbackAction + '<button class="button secondary small" type="button" id="live-tv-refresh">Refresh</button>')
      + loadingNote
      + '<div class="live-tv-grid"><section class="panel"><div class="panel-title"><div>Channels<p>' + model.channels.length + ' available</p></div></div>'
      + channelRows() + '</section><section class="panel"><div class="panel-title"><div>' + guideHeading + '<p>Programme guide</p></div></div>'
      + guideDateRange + programRows() + '</section></div>'
      + '<section class="panel"><div class="panel-title"><div>Create a manual timer<p>Choose a channel and a time window up to four hours.</p></div></div>'
      + manualTimerForm() + '</section>'
      + '<section class="panel"><div class="panel-title"><div>Upcoming timers<p>One off timers and active recordings for this account.</p></div></div>'
      + timerRows() + '</section>'
      + '<section class="panel"><div class="panel-title"><div>Series timers<p>Repeat capture follows the exact programme title on the same channel.</p></div></div>'
      + seriesTimerRows() + '</section>'
      + '<section class="panel"><div class="panel-title"><div>Recordings<p>Recording status and file size.</p></div></div>'
      + recordingRows() + '</section>' + sources;
  }

  async function loadGuide(request, channelId, onUpdate) {
    const generation = model.generation;
    const guideGeneration = ++model.guideGeneration;
    model.selectedChannelId = String(channelId || '');
    model.programs = [];
    model.guideTotal = 0;
    model.guideLoading = true;
    model.guideError = null;
    onUpdate();
    const minimum = new Date(Date.now() - 2 * 60 * 60_000).toISOString();
    const maximum = new Date(Date.now() + 7 * 24 * 60 * 60_000).toISOString();
    const path = '/LiveTv/Programs?ChannelId=' + encodeURIComponent(channelId)
      + '&MinStartDate=' + encodeURIComponent(minimum)
      + '&MaxStartDate=' + encodeURIComponent(maximum)
      + '&StartIndex=0&Limit=500';
    try {
      const result = await request(path);
      if (generation !== model.generation || guideGeneration !== model.guideGeneration) return;
      model.programs = itemList(result);
      model.guideTotal = Number(result?.TotalRecordCount || model.programs.length);
    } catch (error) {
      if (generation !== model.generation || guideGeneration !== model.guideGeneration) return;
      model.guideError = error?.message || 'Guide data could not be loaded.';
    }
    if (generation === model.generation && guideGeneration === model.guideGeneration) {
      model.guideLoading = false;
      onUpdate();
    }
  }

  function prepare(user, libraries) {
    const previousSelection = model.selectedChannelId;
    model.user = user;
    model.libraries = Array.isArray(libraries) ? libraries : [];
    model.channels = [];
    model.programs = [];
    model.timers = [];
    model.seriesTimers = [];
    model.recordings = [];
    model.sources = [];
    model.editingSourceId = null;
    model.errors = {};
    model.loading = true;
    model.guideLoading = false;
    model.guideError = null;
    model.guideTotal = 0;
    model.selectedChannelId = previousSelection;
    model.editingTimerId = null;
    model.editingSeriesTimerId = null;
    model.generation += 1;
  }

  async function load(request, user, libraries, onUpdate) {
    if (!canAccess(user)) return;
    if (!model.loading) {
      prepare(user, libraries);
      onUpdate();
    }
    else {
      model.user = user;
      model.libraries = Array.isArray(libraries) ? libraries : [];
    }
    const generation = ++model.generation;
    model.guideGeneration += 1;
    model.errors = {};
    model.programs = [];
    model.guideLoading = false;
    model.guideError = null;
    const jobs = [
      ['channels', '/LiveTv/Channels?StartIndex=0&Limit=500'],
      ['scheduledTimers', '/LiveTv/Timers?IsScheduled=true'],
      ['activeTimers', '/LiveTv/Timers?IsActive=true'],
      ['seriesTimers', '/LiveTv/SeriesTimers?StartIndex=0&Limit=64'],
      ['recordings', '/LiveTv/Recordings?StartIndex=0&Limit=500'],
    ];
    if (isAdmin(user)) jobs.push(['sources', '/Admin/LiveTv/Sources']);
    const results = await Promise.all(jobs.map(async ([key, path]) => {
      try { return [key, await request(path), null]; }
      catch (error) { return [key, null, error?.message || 'Live TV data could not be loaded.']; }
    }));
    if (generation !== model.generation) return;
    const byKey = Object.fromEntries(results.map(([key, result, error]) => [key, { result, error }]));
    const channels = byKey.channels;
    model.errors = {};
    if (channels.error) model.errors.channels = channels.error;
    else model.channels = itemList(channels.result);
    const timers = byKey.scheduledTimers;
    const activeTimers = byKey.activeTimers;
    if (timers.error) model.errors.timers = timers.error;
    else {
      model.timers = itemList(timers.result);
      if (!activeTimers.error) {
        const known = new Set(model.timers.map((timer) => String(timer.Id)));
        for (const timer of itemList(activeTimers.result)) {
          if (!known.has(String(timer.Id))) model.timers.push(timer);
        }
      }
    }
    if (activeTimers.error && !model.errors.timers) model.errors.timers = activeTimers.error;
    const series = byKey.seriesTimers;
    if (series.error) model.errors.seriesTimers = series.error;
    else model.seriesTimers = itemList(series.result);
    const recordings = byKey.recordings;
    if (recordings.error) model.errors.recordings = recordings.error;
    else model.recordings = itemList(recordings.result);
    if (isAdmin(user)) {
      const sources = byKey.sources;
      if (sources.error) model.errors.sources = sources.error;
      else model.sources = Array.isArray(sources.result) ? sources.result : itemList(sources.result);
    }
    const priorSelectionExists = model.channels.some((channel) => String(channel.Id) === String(model.selectedChannelId));
    if (!priorSelectionExists) model.selectedChannelId = model.channels.length ? String(model.channels[0].Id) : null;
    model.loading = false;
    if (model.selectedChannelId) await loadGuide(request, model.selectedChannelId, onUpdate);
    else onUpdate();
  }

  async function runAction(button, operation, showToast, showError) {
    button.disabled = true;
    try { await operation(); }
    catch (error) { showError(error); }
    finally { if (button.isConnected) button.disabled = false; }
  }

  function bind(root, request, onUpdate, showToast, showError, playChannel) {
    root.querySelector('#live-tv-refresh')?.addEventListener('click', (event) => {
      void runAction(event.currentTarget, () => load(request, model.user, model.libraries, onUpdate), showToast, showError);
    });
    root.querySelectorAll('[data-live-tv-refresh]').forEach((button) => button.addEventListener('click', () => {
      void load(request, model.user, model.libraries, onUpdate);
    }));
    root.querySelectorAll('[data-live-tv-channel]').forEach((button) => button.addEventListener('click', () => {
      void loadGuide(request, button.dataset.liveTvChannel, onUpdate);
    }));
    root.querySelectorAll('[data-live-tv-play]').forEach((button) => button.addEventListener('click', () => {
      const channel = model.channels.find((candidate) => String(candidate.Id) === button.dataset.liveTvPlay);
      if (!channel || !canPlayLiveTv(model.user) || typeof playChannel !== 'function') return;
      void runAction(button, () => playChannel(channel), showToast, showError);
    }));
    root.querySelector('#live-tv-manual-timer-form')?.addEventListener('submit', (event) => {
      event.preventDefault();
      const form = event.currentTarget;
      const values = new FormData(form);
      const start = new Date(String(values.get('StartDate') || ''));
      const end = new Date(String(values.get('EndDate') || ''));
      if (!Number.isFinite(start.getTime()) || !Number.isFinite(end.getTime()) || end <= start
          || end - start > 4 * 60 * 60_000) {
        showError(new Error('Choose a valid timer window of no more than four hours.'));
        return;
      }
      const payload = {
        Name: String(values.get('Name') || '').trim(),
        ChannelId: String(values.get('ChannelId') || ''),
        StartDate: start.toISOString(),
        EndDate: end.toISOString(),
        PrePaddingSeconds: 0,
        PostPaddingSeconds: 0,
      };
      void runAction(form.querySelector('[type="submit"]'), async () => {
        await request('/LiveTv/Timers', { method: 'POST', body: JSON.stringify(payload) });
        showToast('Manual timer created.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    });
    root.querySelectorAll('[data-live-tv-record-program]').forEach((button) => button.addEventListener('click', () => {
      const programId = button.dataset.liveTvRecordProgram;
      const program = model.programs.find((entry) => String(entry.Id) === String(programId));
      if (!program || !model.selectedChannelId) return;
      void runAction(button, async () => {
        await request('/LiveTv/Timers', {
          method: 'POST',
          body: JSON.stringify({
            ChannelId: model.selectedChannelId,
            ProgramId: programId,
            PrePaddingSeconds: 0,
            PostPaddingSeconds: 0,
          }),
        });
        showToast('Programme timer created.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
    root.querySelectorAll('[data-live-tv-record-series]').forEach((button) => button.addEventListener('click', () => {
      const programId = button.dataset.liveTvRecordSeries;
      const program = model.programs.find((entry) => String(entry.Id) === String(programId));
      if (!program || !model.selectedChannelId) return;
      const pattern = button.closest('.live-tv-program')?.querySelector('[name="DayPattern"]')?.value || 'Daily';
      void runAction(button, async () => {
        await request('/LiveTv/SeriesTimers', {
          method: 'POST',
          body: JSON.stringify({
            ChannelId: model.selectedChannelId,
            ProgramId: programId,
            DayPattern: pattern,
            PrePaddingSeconds: 0,
            PostPaddingSeconds: 0,
            RecordAnyTime: true,
            RecordAnyChannel: false,
            SkipEpisodesInLibrary: false,
            RecordNewOnly: false,
            KeepUpTo: 0,
            KeepUntil: 'UntilDeleted',
            Priority: 0,
          }),
        });
        showToast('Series timer created.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
    root.querySelectorAll('[data-live-tv-edit-timer]').forEach((button) => button.addEventListener('click', () => {
      const timerId = button.dataset.liveTvEditTimer;
      const timer = model.timers.find((entry) => String(entry.Id) === String(timerId));
      if (!canManageTimers(model.user) || !timer || timer.Status !== 'New' || timer.SeriesTimerId) return;
      model.editingTimerId = timerId;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-cancel-edit-timer]').forEach((button) => button.addEventListener('click', () => {
      model.editingTimerId = null;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-edit-timer-form]').forEach((form) => form.addEventListener('submit', (event) => {
      event.preventDefault();
      if (!canManageTimers(model.user)) return;
      const timerId = form.dataset.liveTvEditTimerForm;
      const timer = model.timers.find((entry) => String(entry.Id) === String(timerId));
      if (!timer || timer.Status !== 'New' || timer.SeriesTimerId) return;
      let edit;
      try { edit = timerEditValues(form, false); }
      catch (error) { liveTvEditFormError(form, error?.message || 'Timer details are not valid.'); return; }
      const submit = form.querySelector('[type="submit"]');
      submit.disabled = true;
      void (async () => {
        try {
          await request('/LiveTv/Timers/' + encodeURIComponent(timerId), {
            method: 'POST',
            body: JSON.stringify({
              Id: timer.Id,
              Name: edit.name,
              ChannelId: timer.ChannelId,
              ProgramId: timer.ProgramId ?? null,
              StartDate: editedDateValue(form, 'StartDate', edit.start),
              EndDate: editedDateValue(form, 'EndDate', edit.end),
              PrePaddingSeconds: edit.pre,
              PostPaddingSeconds: edit.post,
              OutputLibraryId: timer.OutputLibraryId,
            }),
          });
        } catch (error) {
          liveTvEditFormError(form, error?.message || 'The timer could not be updated.');
          return;
        } finally {
          if (submit.isConnected) submit.disabled = false;
        }
        model.editingTimerId = null;
        showToast('Timer updated.');
        await load(request, model.user, model.libraries, onUpdate);
      })();
    }));
    root.querySelectorAll('[data-live-tv-edit-series]').forEach((button) => button.addEventListener('click', () => {
      const timerId = button.dataset.liveTvEditSeries;
      const timer = model.seriesTimers.find((entry) => String(entry.Id) === String(timerId));
      if (!canManageTimers(model.user) || !timer) return;
      model.editingSeriesTimerId = timerId;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-cancel-edit-series]').forEach((button) => button.addEventListener('click', () => {
      model.editingSeriesTimerId = null;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-edit-series-form]').forEach((form) => {
      form.elements.DayPattern.addEventListener('change', () => {
        form.querySelector('.live-tv-edit-days').disabled = form.elements.DayPattern.value !== 'Custom';
      });
      form.addEventListener('submit', (event) => {
        event.preventDefault();
        if (!canManageTimers(model.user)) return;
        const timerId = form.dataset.liveTvEditSeriesForm;
        const timer = model.seriesTimers.find((entry) => String(entry.Id) === String(timerId));
        if (!timer) return;
        let edit;
        try { edit = timerEditValues(form, true); }
        catch (error) { liveTvEditFormError(form, error?.message || 'Series timer details are not valid.'); return; }
        const pattern = String(edit.values.get('DayPattern') || 'Daily');
        const days = weekdayNames.filter((day) => form.querySelector('input[name="Days"][value="' + day + '"]')?.checked);
        if (pattern === 'Custom' && days.length === 0) {
          liveTvEditFormError(form, 'Choose at least one repeat day.');
          return;
        }
        const submit = form.querySelector('[type="submit"]');
        submit.disabled = true;
        void (async () => {
          try {
            const payload = {
              Id: timer.Id,
              Name: edit.name,
              ChannelId: timer.ChannelId,
              ProgramId: timer.ProgramId ?? null,
              StartDate: editedDateValue(form, 'StartDate', edit.start),
              EndDate: editedDateValue(form, 'EndDate', edit.end),
              PrePaddingSeconds: edit.pre,
              PostPaddingSeconds: edit.post,
              OutputLibraryId: timer.OutputLibraryId,
              RecordAnyTime: timer.RecordAnyTime ?? true,
              RecordAnyChannel: timer.RecordAnyChannel ?? false,
              SkipEpisodesInLibrary: timer.SkipEpisodesInLibrary ?? false,
              RecordNewOnly: timer.RecordNewOnly ?? false,
              KeepUpTo: timer.KeepUpTo ?? 0,
              KeepUntil: timer.KeepUntil || 'UntilDeleted',
              Priority: timer.Priority ?? 0,
            };
            if (pattern === 'Custom') payload.Days = days;
            else payload.DayPattern = pattern;
            await request('/LiveTv/SeriesTimers/' + encodeURIComponent(timerId), {
              method: 'POST',
              body: JSON.stringify(payload),
            });
          } catch (error) {
            liveTvEditFormError(form, error?.message || 'The series timer could not be updated.');
            return;
          } finally {
            if (submit.isConnected) submit.disabled = false;
          }
          model.editingSeriesTimerId = null;
          showToast('Series timer updated.');
          await load(request, model.user, model.libraries, onUpdate);
        })();
      });
    });
    root.querySelectorAll('[data-live-tv-cancel-timer]').forEach((button) => button.addEventListener('click', () => {
      const timerId = button.dataset.liveTvCancelTimer;
      void runAction(button, async () => {
        await request('/LiveTv/Timers/' + encodeURIComponent(timerId), { method: 'DELETE' });
        showToast('Timer cancelled.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
    root.querySelectorAll('[data-live-tv-cancel-series]').forEach((button) => button.addEventListener('click', () => {
      const timerId = button.dataset.liveTvCancelSeries;
      void runAction(button, async () => {
        await request('/LiveTv/SeriesTimers/' + encodeURIComponent(timerId), { method: 'DELETE' });
        showToast('Series timer cancelled.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
    function sourcePayload(form) {
      const values = new FormData(form);
      const editing = Boolean(form.dataset.liveTvSourceId);
      const origin = String(values.get('Origin') || '').trim();
      const addresses = String(values.get('Addresses') || '').split(/[\s,]+/).filter(Boolean);
      if ((!editing || origin || addresses.length) && (!origin || addresses.length < 1 || addresses.length > 16)) {
        showError(new Error('Enter between one and sixteen literal IP addresses.'));
        return;
      }
      let pinUrl = null;
      if (origin) {
        try { pinUrl = new URL(origin); }
        catch (_) { showError(new Error('Enter a valid pinned HTTP(S) origin.')); return; }
        if (!['http:', 'https:'].includes(pinUrl.protocol) || pinUrl.pathname !== '/'
            || pinUrl.search || pinUrl.hash || pinUrl.username || pinUrl.password) {
          showError(new Error('The pinned origin must contain only an HTTP(S) origin, without credentials, path, query or fragment.'));
          return;
        }
      }
      const playlistUrl = String(values.get('PlaylistUrl') || '').trim();
      const guideUrl = String(values.get('GuideUrl') || '').trim();
      const payload = {
        LibraryId: String(values.get('LibraryId') || ''),
        Name: String(values.get('Name') || '').trim(),
      };
      if (!editing || playlistUrl) payload.PlaylistUrl = playlistUrl;
      if (values.get('ClearGuideUrl') === 'on') payload.GuideUrl = null;
      else if (guideUrl) payload.GuideUrl = guideUrl;
      if (pinUrl) payload.OriginPins = [{ origin: pinUrl.origin + '/', addresses }];
      return payload;
    }
    function submitSourceForm(form, sourceId = null) {
      let payload;
      try { payload = sourcePayload(form); }
      catch (error) { showError(error); return; }
      if (!payload) return;
      void runAction(form.querySelector('[type="submit"]'), async () => {
        const path = sourceId
          ? '/Admin/LiveTv/Sources/' + encodeURIComponent(sourceId)
          : '/Admin/LiveTv/Sources';
        await request(path, { method: 'POST', body: JSON.stringify(payload) });
        model.editingSourceId = null;
        showToast(sourceId
          ? 'IPTV source updated. Refresh it to load the new channels and guide data.'
          : 'IPTV source added. Refresh it to load channels and guide data.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }
    root.querySelector('#live-tv-source-form')?.addEventListener('submit', (event) => {
      event.preventDefault();
      if (!isAdmin(model.user)) return;
      submitSourceForm(event.currentTarget);
    });
    root.querySelectorAll('#live-tv-source-edit-form').forEach((form) => form.addEventListener('submit', (event) => {
      event.preventDefault();
      if (!isAdmin(model.user)) return;
      submitSourceForm(event.currentTarget, form.dataset.liveTvSourceId);
    }));
    root.querySelectorAll('[data-live-tv-edit-source]').forEach((button) => button.addEventListener('click', () => {
      if (!isAdmin(model.user)) return;
      const sourceId = button.dataset.liveTvEditSource;
      model.editingSourceId = model.editingSourceId === sourceId ? null : sourceId;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-cancel-source-edit]').forEach((button) => button.addEventListener('click', () => {
      model.editingSourceId = null;
      onUpdate();
    }));
    root.querySelectorAll('[data-live-tv-delete-source]').forEach((button) => button.addEventListener('click', () => {
      if (!isAdmin(model.user)) return;
      const sourceId = button.dataset.liveTvDeleteSource;
      void runAction(button, async () => {
        await request('/Admin/LiveTv/Sources/' + encodeURIComponent(sourceId), { method: 'DELETE' });
        if (model.editingSourceId === sourceId) model.editingSourceId = null;
        showToast('IPTV source deleted.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
    root.querySelectorAll('[data-live-tv-refresh-source]').forEach((button) => button.addEventListener('click', () => {
      if (!isAdmin(model.user)) return;
      const sourceId = button.dataset.liveTvRefreshSource;
      void runAction(button, async () => {
        await request('/Admin/LiveTv/Sources/' + encodeURIComponent(sourceId) + '/Refresh', {
          method: 'POST',
          timeoutMs: 75_000,
        });
        showToast('IPTV source refreshed.');
        await load(request, model.user, model.libraries, onUpdate);
      }, showToast, showError);
    }));
  }

  window.PuffinboxLiveTv = { canAccess, canPlayLiveTv, prepare, load, render, bind };
})();
