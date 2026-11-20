/* Server-rendered HTML owns data; this module owns per-generation view state. */
(() => {
  'use strict';
  const $ = (selector, root = document) => root.querySelector(selector);
  const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
  const root = $('.app-shell');
  const views = new WeakMap();
  let current = null;
  let mode = 'map';
  let filter = 'active';
  let detailTab = 'tasks';
  let requestedThread = null;
  let requestedBot = null;
  const railScroll = new Map();
  let navigationToken = 0;
  let navigationXHR = null;
  let failedSelection = null;
  let indexChecked = Date.now();
  let indexFailed = false;
  let indexRestore = null;
  const origin = event => event.detail.requestConfig?.elt || event.detail.elt;
  const panelFor = event => origin(event)?.closest('.thread-panel');
  const isMobile = () => root.classList.contains('is-mobile');
  const announce = message => { $('#announcer').textContent = message; };
  const loadedCount = panel => $('.events', panel).children.length;
  const newView = panel => ({
    paused: false, following: true, top: 0, checked: Date.now(), failed: false,
    adjusting: false, loadingOlder: false, total: Number(panel.dataset.eventCount),
    pending: 0, newBelow: 0, generationPending: false, resuming: false,
    tasksOpen: !isMobile() && window.innerHeight >= 700,
  });
  const view = panel => {
    if (!views.has(panel)) views.set(panel, newView(panel));
    return views.get(panel);
  };

  function timestamps(container = document) {
    $$('time[data-time]', container).forEach(element => {
      const record = element.closest('.event');
      if (record && element.dataset.formatted) return;
      const raw = element.dataset.time;
      const stamp = /^-?\d+$/.test(raw) ? Number(raw) : Date.parse(raw);
      const label = $('.time-label', element);
      if (!Number.isFinite(stamp)) {
        label.textContent = 'unknown';
        return;
      }
      const date = new Date(stamp);
      if (!Number.isFinite(date.getTime())) { label.textContent = 'unknown'; return; }
      // Relative ages freeze with the last server snapshot, not with collector health.
      const now = indexFailed || Date.now() - indexChecked >= 15000 ? indexChecked : Date.now();
      const seconds = Math.max(0, Math.floor((now - stamp) / 1000));
      label.textContent = record
        ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false })
        : seconds < 60 ? `${seconds}s ago` : seconds < 3600 ? `${Math.floor(seconds / 60)}m ago`
          : seconds < 86400 ? `${Math.floor(seconds / 3600)}h ago` : `${Math.floor(seconds / 86400)}d ago`;
      element.dateTime = date.toISOString();
      element.title = date.toLocaleString([], { dateStyle: 'medium', timeStyle: 'medium' });
      $('.time-full', element).textContent = ` (${element.title})`;
      if (record) element.dataset.formatted = 'true';
    });
  }

  const railFor = id => $$('.agent-rail').find(rail => rail.dataset.botId === id);
  const matchesFilter = link => (link.dataset.active === 'true') === (filter === 'active');

  function filterThreads() {
    const activeOnly = filter === 'active';
    const summary = $('.index-summary').dataset;
    const count = activeOnly ? Number(summary.active) : Number(summary.total) - Number(summary.active);
    $$('.thread-link').forEach(link => {
      link.hidden = !matchesFilter(link);
      if (link.classList.contains('map-thread')) link.closest('.map-branch').hidden = link.hidden;
      if (mode !== 'map' && link.dataset.thread === requestedThread) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    });
    $$('.agent-rail').forEach(rail => {
      rail.hidden = mode === 'map' || rail.dataset.botId !== requestedBot;
      const shown = $$('.thread-item:not([hidden])', rail).length;
      const empty = $('.rail-empty', rail);
      empty.hidden = shown > 0;
      $('p', empty).textContent = `No ${filter} threads for ${rail.dataset.botName}.`;
    });
    $$('.map-threads').forEach(tree => {
      const branches = $$('.map-branch', tree);
      branches.forEach(branch => branch.classList.remove('last-branch'));
      const shown = branches.filter(branch => !branch.hidden);
      shown.at(-1)?.classList.add('last-branch');
      const bot = tree.closest('.map-bot');
      const empty = $('.map-bot-empty', bot);
      empty.hidden = shown.length > 0 || count === 0;
      empty.textContent = `No ${filter} threads`;
      const links = $$('.map-thread', tree);
      const target = links.find(matchesFilter) || links[0];
      const open = $('.bot-open', bot);
      if (open && target) open.href = target.href;
    });
    $$('[data-filter]').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.filter === filter)));
    $$('.reporting-past').forEach(button => { button.hidden = !activeOnly; });
    $$('.toggle-filter').forEach(button => { button.textContent = `Show ${activeOnly ? 'past' : 'active'} threads`; });
    $('#map-summary').textContent = `${count} ${filter} threads across ${summary.bots} bots · select a bot to focus on it`;
    const empty = $('#map-empty');
    empty.hidden = count > 0;
    $('p', empty).textContent = activeOnly
      ? Number(summary.unknown) > 0
        ? `No active threads. ${summary.unknown} previously active threads are Unknown because their collectors are offline; they are under Past.`
        : 'No thread is Working, Queued or Waiting.'
      : 'No past threads recorded.';
  }

  function updatePending() {
    const metadata = new Map($$('.thread-item').map(link => [link.dataset.thread, link.dataset]));
    $$('.thread-panel', $('#panels')).forEach(panel => {
      const data = metadata.get(panel.dataset.thread);
      if (!data) return;
      const state = view(panel);
      state.generationPending = data.version !== panel.dataset.version;
      state.pending = state.generationPending ? Number(data.eventCount) : Math.max(0, Number(data.eventCount) - state.total);
      refreshControls(panel);
    });
  }

  // Keep source events in chronological order: cursors and pending counts are
  // records, not cards. Only the presentation is combined, using explicit IDs.
  function groupTools(panel) {
    const records = $$('.events > .event', panel);
    const previous = new Map($$('.tool-group-view', panel).map(card => [card.dataset.group, card]));
    const groups = new Map();
    for (const record of records) {
      const content = $('.event-content', record);
      if (record.classList.contains('event-tool') && !$('.tool-raw', content)) {
        const raw = document.createElement('div');
        raw.className = 'tool-raw';
        record.dataset.recordError = String(record.classList.contains('event-error'));
        raw.append(...content.childNodes);
        $$('.tool-input details', raw).forEach((section, index) => {
          section.dataset.section = JSON.stringify(['input', record.dataset.event, index]);
        });
        $$('details.tool-record', raw).forEach((section, index) => {
          section.dataset.section = JSON.stringify([record.dataset.event, index]);
        });
        content.append(raw);
      }
      record.dataset.grouped = '';
      if (record.dataset.recordError !== undefined) record.classList.toggle('event-error', record.dataset.recordError === 'true');
      const raw = $('.tool-raw', content);
      if (raw) raw.hidden = false;
      const { toolSession, toolCall, toolName, toolKind } = record.dataset;
      if (!toolSession || !toolCall || !['tool_call', 'tool_start', 'tool_result'].includes(toolKind)) continue;
      const key = JSON.stringify([toolSession, toolCall, toolName]);
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(record);
    }
    const retained = new Set();
    for (const [key, members] of groups) {
      // Ambiguous duplicate calls are not evidence of one operation.
      if (members.length < 2 || members.filter(record => record.dataset.toolKind === 'tool_call').length > 1) continue;
      const anchor = members[0];
      const input = members.find(record => record.dataset.toolKind === 'tool_call')
        || members.find(record => record.dataset.toolKind === 'tool_start') || anchor;
      const results = members.filter(record => record.dataset.toolKind === 'tool_result');
      const error = results.find(record => record.classList.contains('event-error'));
      const stateSource = error || results.at(-1)
        || members.find(record => record.dataset.toolKind === 'tool_start') || input;
      const signature = JSON.stringify(members.map(record => record.dataset.event));
      let card = previous.get(key);
      if (!card) {
        card = document.createElement('div');
        card.className = 'tool-group-view';
        card.dataset.group = key;
      }
      let focusKey = null;
      if (card.dataset.signature !== signature) {
        const open = new Map($$('details[data-section]', card).map(section => [section.dataset.section, section.open]));
        for (const member of members) {
          $$('details[data-section]', $('.tool-raw', member)).forEach(section => {
            if (!open.has(section.dataset.section)) open.set(section.dataset.section, section.open);
          });
        }
        const focused = document.activeElement?.closest('details[data-section]');
        focusKey = focused && (card.contains(focused) || members.some(member => member.contains(focused))) ? focused.dataset.section : null;
        const parts = [];
        const heading = $('.tool-card-heading', $('.tool-raw', input))?.cloneNode(true);
        if (heading) {
          const status = $('.tool-status', heading);
          const observed = $('.tool-status', $('.tool-raw', stateSource));
          if (status && observed) status.replaceWith(observed.cloneNode(true));
          parts.push(heading);
        }
        const fields = $('.tool-input', $('.tool-raw', input));
        if (fields) {
          const clone = fields.cloneNode(true);
          $$('details', clone).forEach((section, index) => {
            section.dataset.section = JSON.stringify(['input', input.dataset.event, index]);
            if (open.has(section.dataset.section)) section.open = open.get(section.dataset.section);
          });
          parts.push(clone);
        }
        // Output first; retain every input/start record below it for inspection.
        for (const member of [...results, ...members.filter(record => record.dataset.toolKind !== 'tool_result')]) {
          $$('details.tool-record', $('.tool-raw', member)).forEach((section, index) => {
            const clone = section.cloneNode(true);
            clone.dataset.section = JSON.stringify([member.dataset.event, index]);
            if (open.has(clone.dataset.section)) clone.open = open.get(clone.dataset.section);
            const stamp = $(':scope > time', member);
            const summary = $('summary', clone);
            if (stamp && summary) summary.append(' · ', stamp.cloneNode(true));
            parts.push(clone);
          });
        }
        card.replaceChildren(...parts);
        card.dataset.signature = signature;
      }
      const destination = $('.event-content', anchor);
      if (card.parentElement !== destination) destination.append(card);
      anchor.classList.toggle('event-error', Boolean(error));
      for (const member of members) {
        member.dataset.grouped = member === anchor ? 'anchor' : 'member';
        $('.tool-raw', member).hidden = true;
      }
      if (focusKey) {
        const replacement = $$('details[data-section]', card).find(section => section.dataset.section === focusKey);
        $('summary', replacement || card)?.focus({ preventScroll: true });
      }
      retained.add(key);
    }
    for (const [key, card] of previous) if (!retained.has(key)) card.remove();
  }

  function filterEvents(panel) {
    const query = $('.transcript-search', panel).value.trim();
    const normalized = query.toLocaleLowerCase();
    const records = $$('.events > .event', panel);
    let matched = 0;
    const matchingGroups = new Set();
    records.forEach(record => {
      const content = $('.tool-raw', record) || $('.event-content', record);
      const hit = !normalized || (content?.textContent || '').toLocaleLowerCase().includes(normalized);
      if (hit) {
        matched++;
        if (record.dataset.grouped) matchingGroups.add(JSON.stringify([record.dataset.toolSession, record.dataset.toolCall, record.dataset.toolName]));
      }
      record.hidden = record.dataset.grouped === 'member' || !hit;
    });
    // A match in any member reveals the whole operation, including its command.
    for (const card of $$('.tool-group-view', panel)) {
      card.closest('.event').hidden = !matchingGroups.has(card.dataset.group);
    }
    $('.record-count', panel).textContent = `${records.length} loaded records`;
    $('.search-count', panel).textContent = query
      ? `${matched} of ${records.length} loaded records match “${query}”. Only loaded history is searched; auto-follow is off while searching.`
      : '';
    const empty = $('.empty-events', panel);
    empty.hidden = matched > 0;
    $('p', empty).textContent = query
      ? `No matches in the ${records.length} loaded records. Older records aren't searched until they're loaded.`
      : 'Nothing recorded yet.';
    $('.search-older', panel).hidden = !query || !$('.older', panel);
    $('.clear-search', panel).hidden = !query;
  }

  function setScroll(panel, top) {
    const state = view(panel);
    state.adjusting = true;
    const scroll = $('.transcript-scroll', panel);
    scroll.scrollTop = top;
    state.top = scroll.scrollTop;
    requestAnimationFrame(() => { state.adjusting = false; });
  }

  function applyDetailTab(panel, focus = false) {
    const state = view(panel);
    const tabs = $$('.detail-tab', panel);
    const todoPanel = $('.todo-panel', panel);
    const transcriptPanel = $('.transcript-panel', panel);
    const mobile = isMobile();
    $('.detail-tabs', panel).hidden = mobile;
    if (mobile) {
      todoPanel.hidden = false;
      transcriptPanel.hidden = false;
      return;
    }
    const tasks = detailTab === 'tasks';
    todoPanel.hidden = !tasks;
    transcriptPanel.hidden = tasks;
    for (const tab of tabs) {
      const selected = tab.dataset.detailTab === detailTab;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
      if (selected && focus) tab.focus();
    }
    const count = $$('.todo', todoPanel).length;
    $('.tab-count', panel).textContent = String(count);
  }

  function refreshControls(panel) {
    const state = view(panel);
    const searching = Boolean($('.transcript-search', panel).value.trim());
    const pause = $('.pause', panel);
    pause.textContent = state.paused ? state.pending > 0 ? `Resume · ${state.pending} new` : 'Resume' : 'Pause';
    pause.setAttribute('aria-pressed', String(state.paused));
    $$('.older, .search-older', panel).forEach(button => {
      button.disabled = state.paused || state.loadingOlder;
      button.textContent = state.loadingOlder ? 'Loading older records…' : 'Load older records';
    });
    $$('.paused-older-hint', panel).forEach(hint => { hint.hidden = !state.paused; });
    const toggle = $('.toggle-todos', panel);
    if (toggle) {
      toggle.setAttribute('aria-expanded', String(state.tasksOpen));
      toggle.textContent = state.tasksOpen ? 'Collapse' : 'Show todos';
      $('.todo-list', panel).hidden = !state.tasksOpen;
    }
    const mobileToggle = $('.mobile-todos', panel);
    mobileToggle?.setAttribute('aria-expanded', String(state.tasksOpen));
    if (mobileToggle) mobileToggle.textContent = state.tasksOpen ? 'Close todos' : 'Todos';
    $('.todo-panel', panel)?.classList.toggle('mobile-open', state.tasksOpen);
    applyDetailTab(panel);
    $$('.jump', panel).forEach(button => button.setAttribute('aria-pressed', String(state.following && !searching)));
    const stale = !state.paused && Date.now() - state.checked > 15000;
    const health = state.failed || stale;
    const label = $('.freshness', panel);
    label.dataset.state = health ? 'retrying' : state.paused ? 'paused' : state.following && !searching ? 'live' : 'held';
    label.textContent = health ? `${stale ? '◌ Stale' : '↻ Retrying'} · showing last loaded`
      : state.paused ? state.generationPending ? '❚❚ Paused · history replaced; resume to view'
        : state.pending > 0 ? `❚❚ Paused · ${state.pending} new waiting` : '❚❚ Paused · transcript frozen'
      : state.following && !searching ? '● Live · following latest' : '‖ Live · scroll held';
    const pill = $('.new-records', panel);
    pill.hidden = state.paused || state.following && !searching || state.newBelow === 0;
    pill.textContent = `${state.newBelow} new records · Follow latest ↓`;
  }

  function initPanel(panel) {
    if (views.has(panel)) return;
    view(panel);
    const toolbar = $('.transcript-toolbar', panel);
    const sizeToolbar = () => {
      if (!panel.hidden && mode === 'thread') panel.style.setProperty('--toolbar-height', `${toolbar.getBoundingClientRect().height}px`);
    };
    new ResizeObserver(sizeToolbar).observe(toolbar);
    sizeToolbar();
    $('.transcript-scroll', panel).addEventListener('scroll', () => {
      const state = view(panel);
      if (state.adjusting || panel.hidden || mode !== 'thread') return;
      const scroll = $('.transcript-scroll', panel);
      state.top = scroll.scrollTop;
      const bottom = scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop <= 50;
      if (!bottom) state.following = false;
      else if (!$('.transcript-search', panel).value.trim()) {
        state.following = true;
        state.newBelow = 0;
      }
      refreshControls(panel);
    }, { passive: true });
    groupTools(panel);
    filterEvents(panel);
    refreshControls(panel);
  }

  function closeSidebar(restoreFocus = true) {
    const wasOpen = $('#thread-sidebar').classList.contains('is-open');
    $('#thread-sidebar').classList.remove('is-open');
    $('#drawer-scrim').hidden = true;
    $('#thread-main').inert = false;
    $('#menu').setAttribute('aria-expanded', 'false');
    if (wasOpen && restoreFocus && isMobile()) $('#menu').focus({ preventScroll: true });
  }

  function openSidebar() {
    if (!isMobile() || mode === 'map') return;
    $('#thread-sidebar').classList.add('is-open');
    $('#drawer-scrim').hidden = false;
    $('#thread-main').inert = true;
    $('#menu').setAttribute('aria-expanded', 'true');
    $('#close-drawer').focus();
  }

  function rememberScroll() {
    if (current && mode === 'thread') view(current).top = $('.transcript-scroll', current).scrollTop;
    const rail = railFor(requestedBot);
    if (rail && mode !== 'map') railScroll.set(requestedBot, $('.thread-list', rail).scrollTop);
  }

  function setURL(id) {
    const url = new URL(location.href);
    if (id) url.searchParams.set('thread', id);
    else url.searchParams.delete('thread');
    history.replaceState(null, '', `${url.pathname}${url.search}${url.hash}`);
  }

  function showMap(focus = true) {
    rememberScroll();
    navigationToken++;
    navigationXHR?.abort();
    navigationXHR = null;
    requestedThread = null;
    requestedBot = null;
    mode = 'map';
    root.dataset.mode = 'map';
    $('#agent-map').hidden = false;
    $('#opening').hidden = true;
    $('#panels').hidden = true;
    $('#request-error').hidden = true;
    $$('.thread-panel').forEach(panel => { panel.hidden = true; });
    $('#mobile-title').textContent = 'Threadwatch';
    closeSidebar(false);
    filterThreads();
    setURL(null);
    if (focus) $('#thread-main').focus({ preventScroll: true });
  }

  function activate(panel, focus = false) {
    rememberScroll();
    $$('.thread-panel').forEach(item => { item.hidden = item !== panel; });
    current = panel;
    requestedThread = panel.dataset.thread;
    requestedBot = panel.dataset.botId;
    mode = 'thread';
    root.dataset.mode = 'thread';
    initPanel(panel);
    $('#agent-map').hidden = true;
    $('#opening').hidden = true;
    $('#panels').hidden = false;
    $('#request-error').hidden = true;
    $('#mobile-title').textContent = panel.dataset.bot || 'Thread';
    closeSidebar(false);
    filterThreads();
    const rail = railFor(requestedBot);
    if (rail) $('.thread-list', rail).scrollTop = railScroll.get(requestedBot) || 0;
    timestamps(panel);
    const state = view(panel);
    requestAnimationFrame(() => setScroll(panel, state.following && !$('.transcript-search', panel).value.trim()
      ? $('.transcript-scroll', panel).scrollHeight : state.top));
    setURL(panel.dataset.thread);
    if (focus) {
      $('#thread-main').focus({ preventScroll: true });
      if (!state.paused) htmx.trigger($('.poller', panel), 'refresh');
    }
  }

  function selectThread(id, title) {
    rememberScroll();
    navigationToken++;
    navigationXHR?.abort();
    navigationXHR = null;
    requestedThread = id;
    const metadata = $$('.thread-item').find(link => link.dataset.thread === id)?.dataset;
    requestedBot = metadata?.botId || null;
    const cached = $$('.thread-panel', $('#panels')).find(panel => panel.dataset.thread === id);
    if (cached) { activate(cached, true); return; }
    mode = 'opening';
    root.dataset.mode = 'thread';
    $('#agent-map').hidden = true;
    $('#panels').hidden = true;
    $$('.thread-panel').forEach(panel => { panel.hidden = true; });
    $('#opening').hidden = false;
    $('#opening h1').textContent = title;
    $('#request-error').hidden = true;
    $('#mobile-title').textContent = metadata?.botName || 'Opening…';
    closeSidebar(false);
    filterThreads();
    const rail = railFor(requestedBot);
    if (rail) $('.thread-list', rail).scrollTop = railScroll.get(requestedBot) || 0;
    setURL(id);
    $('#thread-main').focus({ preventScroll: true });
    // Use a stable request source: index swaps must not detach an opening request.
    htmx.ajax('GET', `ui/thread?id=${encodeURIComponent(id)}`, {
      source: $('#detail-stage'), target: '#detail-stage', swap: 'innerHTML',
    }).catch(() => { /* htmx error events retain the previous view and offer retry. */ });
  }

  function follow(panel) {
    const state = view(panel);
    const search = $('.transcript-search', panel);
    search.value = '';
    filterEvents(panel);
    state.following = true;
    state.newBelow = 0;
    setScroll(panel, $('.transcript-scroll', panel).scrollHeight);
    refreshControls(panel);
    announce(state.paused ? 'Jumped to latest. Transcript is still paused.' : 'Following latest.');
  }


  document.addEventListener('submit', async event => {
    const form = event.target.closest('.chat-form');
    if (!form) return;
    event.preventDefault();
    const input = $('textarea', form);
    const button = $('button[type="submit"]', form);
    const feedback = $('.chat-feedback', form);
    const text = input.value.trim();
    if (!text) {
      feedback.textContent = 'Enter a message.';
      return;
    }
    input.disabled = true;
    button.disabled = true;
    feedback.textContent = 'Sending…';
    try {
      const response = await fetch(form.dataset.commandUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        throw new Error(body.error || 'Message was not queued.');
      }
      input.value = '';
      feedback.textContent = 'Queued for bridge delivery.';
      htmx.trigger($('.poller', form.closest('.thread-panel')), 'refresh');
    } catch (error) {
      feedback.textContent = error.message || 'Message was not queued.';
    } finally {
      input.disabled = false;
      button.disabled = false;
      input.focus();
    }
  });

  document.addEventListener('click', async event => {
    const link = event.target.closest('.thread-link');
    const map = event.target.closest('.map-link');
    const bot = event.target.closest('.bot-open');
    if ((link || map || bot) && !event.ctrlKey && !event.metaKey && !event.shiftKey && !event.altKey && event.button === 0) {
      event.preventDefault();
      event.stopPropagation();
      if (map) showMap();
      else if (bot) {
        const links = $$('.map-thread', bot.closest('.map-bot'));
        let target = links.find(matchesFilter);
        if (!target) {
          target = links[0];
          if (target) filter = target.dataset.active === 'true' ? 'active' : 'past';
        }
        if (target) selectThread(target.dataset.thread, target.dataset.title);
      } else selectThread(link.dataset.thread, link.dataset.title);
      return;
    }
    if (event.target.closest('[data-filter], .toggle-filter')) {
      const button = event.target.closest('[data-filter], .toggle-filter');
      filter = button.dataset.filter || (filter === 'active' ? 'past' : 'active');
      filterThreads();
      return;
    }
    if (event.target.closest('#menu')) openSidebar();
    if (event.target.closest('#close-drawer, #drawer-scrim')) closeSidebar();
    if (event.target.closest('.retry-open') && failedSelection) selectThread(failedSelection.id, failedSelection.title);
    if (event.target.closest('.pair-device')) {
      const dialog = $('#pair-dialog');
      const status = $('#pair-status');
      const qr = $('#pair-qr');
      qr.removeAttribute('src');
      status.textContent = 'Creating one-time pairing code…';
      dialog.showModal();
      const response = await fetch('api/pairings', { method: 'POST' });
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        status.textContent = body.error || 'Pairing authorization failed.';
        return;
      }
      const pairing = await response.json();
      qr.src = pairing.qr;
      status.textContent = 'Scan now. This code expires in two minutes and works once.';
      return;
    }
    const panel = event.target.closest('.thread-panel');
    if (!panel) return;
    const state = view(panel);
    const tabButton = event.target.closest('.detail-tab');
    if (tabButton) {
      detailTab = tabButton.dataset.detailTab;
      if (detailTab === 'tasks') state.tasksOpen = true;
      refreshControls(panel);
      applyDetailTab(panel, true);
      if (detailTab === 'transcript' && state.following && !$('.transcript-search', panel).value.trim()) {
        setScroll(panel, $('.transcript-scroll', panel).scrollHeight);
      }
      return;
    }
    if (event.target.closest('.pause')) {
      state.paused = !state.paused;
      if (state.paused) {
        state.resuming = false;
        announce('Transcript paused. Collection continues in the background.');
      } else {
        state.resuming = true;
        announce('Resuming transcript. Fetching records collected in the background.');
        htmx.trigger($('.poller', panel), 'refresh');
      }
      refreshControls(panel);
    }
    if (event.target.closest('.jump')) follow(panel);
    if (event.target.closest('.toggle-todos, .mobile-todos')) {
      state.tasksOpen = !state.tasksOpen;
      refreshControls(panel);
      if (state.following && !$('.transcript-search', panel).value.trim()) setScroll(panel, $('.transcript-scroll', panel).scrollHeight);
    }
    if (event.target.closest('.search-older')) $('.older', panel)?.click();
    if (event.target.closest('.clear-search')) {
      $('.transcript-search', panel).value = '';
      filterEvents(panel);
      refreshControls(panel);
      $('.transcript-search', panel).focus();
    }
    if (event.target.closest('.dismiss-history')) {
      $('.history-notice', panel).hidden = true;
      $('.transcript-scroll', panel).focus({ preventScroll: true });
    }
  }, true);

  document.addEventListener('keydown', event => {
    const tab = event.target.closest('.detail-tab');
    if (tab && ['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) {
      event.preventDefault();
      const panel = tab.closest('.thread-panel');
      const state = view(panel);
      detailTab = event.key === 'ArrowRight' || event.key === 'End' ? 'tasks' : 'transcript';
      if (detailTab === 'tasks') state.tasksOpen = true;
      refreshControls(panel);
      applyDetailTab(panel, true);
      return;
    }
    if (!$('#thread-sidebar').classList.contains('is-open')) return;
    if (event.key === 'Escape') { event.preventDefault(); closeSidebar(); }
    if (event.key === 'Tab') {
      const stops = $$('a[href], button:not(:disabled), input, [tabindex="0"]', $('#thread-sidebar'))
        .filter(element => element.getClientRects().length > 0);
      const first = stops[0];
      const last = stops.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault(); last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault(); first?.focus();
      }
    }
  });
  document.addEventListener('input', event => {
    if (event.target.matches('.transcript-search')) {
      const panel = event.target.closest('.thread-panel');
      filterEvents(panel);
      refreshControls(panel);
    }
  });
  document.addEventListener('error', event => {
    if (event.target.matches?.('.avatar img')) event.target.hidden = true;
  }, true);

  document.addEventListener('htmx:configRequest', event => {
    if (origin(event)?.id === 'index-data' && current && mode === 'thread' && view(current).paused) {
      event.detail.parameters.id = current.dataset.thread;
    }
    const panel = panelFor(event);
    if (!panel) return;
    const state = view(panel);
    if (panel.hidden || mode !== 'thread' || state.paused || state.loadingOlder) { event.preventDefault(); return; }
    event.detail.parameters.version = panel.dataset.version;
    event.detail.parameters.revision = panel.dataset.revision;
    event.detail.parameters.after = $('.events', panel).lastElementChild?.dataset.event || '';
  });

  document.addEventListener('htmx:beforeRequest', event => {
    if (event.detail.target?.id === 'detail-stage') {
      event.detail.xhr.twToken = navigationToken;
      event.detail.xhr.twThread = requestedThread;
      navigationXHR = event.detail.xhr;
    }
    const panel = panelFor(event);
    if (!panel) return;
    const state = view(panel);
    if (panel.hidden || mode !== 'thread' || state.paused) { event.preventDefault(); return; }
    if (origin(event).classList.contains('older')) state.loadingOlder = true;
    refreshControls(panel);
  });

  document.addEventListener('htmx:beforeSwap', event => {
    const target = event.detail.target;
    if (target.id === 'detail-stage' &&
        (event.detail.xhr.twToken !== navigationToken || event.detail.xhr.twThread !== requestedThread || mode !== 'opening')) {
      event.detail.shouldSwap = false;
      return;
    }
    const panel = panelFor(event);
    if (panel) {
      const state = view(panel);
      if (panel.hidden || mode !== 'thread' || state.paused) {
        event.detail.shouldSwap = false;
        return;
      }
      if (!event.detail.shouldSwap) return;
      const scroll = $('.transcript-scroll', panel);
      state.oldHeight = scroll.scrollHeight;
      state.oldTop = scroll.scrollTop;
      const top = scroll.getBoundingClientRect().top;
      const anchor = $$('.events > .event:not([hidden])', panel)
        .find(record => record.getBoundingClientRect().bottom > top);
      state.scrollAnchor = anchor ? { id: anchor.dataset.event, offset: anchor.getBoundingClientRect().top - top } : null;
      state.oldCount = loadedCount(panel);
      state.metaTop = $('.thread-header', panel).scrollTop;
      state.todosTop = $('.todo-list', panel)?.scrollTop || 0;
      state.focus = document.activeElement?.matches('.discord-link, .mobile-todos, .toggle-todos, .older, .todo-list')
        ? document.activeElement.className : null;
      state.adjusting = true;
    }
    if (target.id === 'index-data' && event.detail.shouldSwap) {
      const focused = document.activeElement;
      indexRestore = {
        botId: requestedBot,
        nav: railFor(requestedBot) ? $('.thread-list', railFor(requestedBot)).scrollTop : 0,
        map: $('#agent-map').scrollTop,
        thread: focused?.closest('.thread-link')?.dataset.thread,
        botLink: focused?.closest('.bot-open')?.dataset.botId,
        inMap: Boolean(focused?.closest('#map-data')),
        filter: focused?.closest('[data-filter]')?.dataset.filter,
        toggleFilter: Boolean(focused?.closest('.toggle-filter')),
        panel: current,
        metaTop: current ? $('.thread-header', current).scrollTop : 0,
        metaFocus: focused?.matches('.mobile-todos') ? '.mobile-todos' : focused?.matches('.discord-link') ? '.discord-link' : focused?.matches('.thread-header .map-link') ? '.thread-header .map-link' : null,
      };
    }
  });

  document.addEventListener('htmx:afterSwap', event => {
    const target = event.detail.target;
    if (target.id === 'detail-stage') {
      const panel = $('.thread-panel', target);
      if (!panel) return;
      $('#panels').append(panel);
      navigationXHR = null;
      failedSelection = null;
      activate(panel, true);
    } else if (target.id === 'index-data') {
      indexChecked = Date.now();
      indexFailed = false;
      filterThreads();
      updatePending();
      if (indexRestore) {
        const rail = railFor(requestedBot);
        if (rail && indexRestore.botId === requestedBot) $('.thread-list', rail).scrollTop = indexRestore.nav;
        $('#agent-map').scrollTop = indexRestore.map;
        const scope = indexRestore.inMap ? $('#map-data') : rail;
        if (scope) {
          if (indexRestore.thread) $$('.thread-link', scope).find(link => link.dataset.thread === indexRestore.thread && !link.hidden)?.focus({ preventScroll: true });
          else if (indexRestore.botLink) $$('.bot-open', scope).find(link => link.dataset.botId === indexRestore.botLink)?.focus({ preventScroll: true });
          else if (indexRestore.filter) $$('[data-filter]', scope).find(button => button.dataset.filter === indexRestore.filter)?.focus({ preventScroll: true });
          else if (indexRestore.toggleFilter) $('.toggle-filter', scope)?.focus({ preventScroll: true });
        }
        if (indexRestore.panel) {
          $('.thread-header', indexRestore.panel).scrollTop = indexRestore.metaTop;
          if (indexRestore.metaFocus) $(indexRestore.metaFocus, indexRestore.panel)?.focus({ preventScroll: true });
        }
        indexRestore = null;
      }
      timestamps($('#map-data'));
      refreshConnection();
    } else if (target.classList.contains('events')) {
      const panel = target.closest('.thread-panel');
      let state = view(panel);
      const xhr = event.detail.xhr;
      const oldVersion = panel.dataset.version;
      const version = xhr.getResponseHeader('TW-Version') ?? oldVersion;
      const updateMode = xhr.getResponseHeader('TW-Mode');
      const oldTop = state.oldTop || 0;
      const oldHeight = state.oldHeight || 0;
      const oldCount = state.oldCount || 0;
      const resuming = state.resuming;
      const focus = state.focus;
      panel.dataset.version = version;
      panel.dataset.revision = xhr.getResponseHeader('TW-Revision') ?? panel.dataset.revision;
      if (updateMode === 'replace') {
        // The replaced event tree drops older pages and expanded-tool state.
        // Reset the rest of the view as one generation, never merge it.
        views.set(panel, newView(panel));
        state = view(panel);
        $('.transcript-search', panel).value = '';
        const notice = $('.history-notice', panel);
        $('span', notice).textContent = `History was replaced by the source (version ${oldVersion} → ${version}). Showing the new generation only; records from the previous version were cleared rather than merged.`;
        notice.hidden = false;
        announce(`History replaced. Showing version ${version} only.`);
      }
      const seen = new Set();
      $$('.event', target).forEach(record => {
        if (seen.has(record.dataset.event)) record.remove();
        else seen.add(record.dataset.event);
      });
      // Older pages describe the total at their fetch time, but do not advance the
      // visible tail. Only a live catch-up can move the pending-count baseline.
      if (updateMode !== 'older') {
        state.total = Number(xhr.getResponseHeader('TW-Event-Count') ?? panel.dataset.eventCount);
        panel.dataset.eventCount = String(state.total);
        state.pending = 0;
        state.generationPending = false;
      }
      state.checked = Date.now();
      state.failed = false;
      state.loadingOlder = false;
      state.resuming = false;
      const added = Math.max(0, loadedCount(panel) - oldCount);
      if (updateMode === 'older') {
        state.following = false;
        announce(`Loaded ${added} older records.`);
      } else if (updateMode !== 'replace') {
        if (!state.following || $('.transcript-search', panel).value.trim()) state.newBelow += added;
        if (resuming) announce(`Resumed. Caught up ${added} records.`);
      }
      const error = $('.view-error', panel);
      if (updateMode === 'replace' || error.dataset.request === updateMode) error.hidden = true;
      groupTools(panel);
      filterEvents(panel);
      refreshControls(panel);
      $('.thread-header', panel).scrollTop = state.metaTop || 0;
      if ($('.todo-list', panel)) $('.todo-list', panel).scrollTop = state.todosTop || 0;
      if (focus && updateMode !== 'replace') {
        const replacement = $(`.${focus.trim().split(/\s+/).join('.')}`, panel);
        (replacement || $('.transcript-scroll', panel)).focus({ preventScroll: true });
      }
      const scroll = $('.transcript-scroll', panel);
      const anchor = updateMode !== 'replace' && state.scrollAnchor
        ? $$('.events > .event:not([hidden])', panel).find(record => record.dataset.event === state.scrollAnchor.id) : null;
      if (updateMode !== 'older' && state.following && !$('.transcript-search', panel).value.trim()) setScroll(panel, scroll.scrollHeight);
      else if (anchor) setScroll(panel, scroll.scrollTop + anchor.getBoundingClientRect().top - scroll.getBoundingClientRect().top - state.scrollAnchor.offset);
      else if (updateMode === 'older') setScroll(panel, oldTop + scroll.scrollHeight - oldHeight);
      else setScroll(panel, updateMode === 'replace' ? 0 : oldTop);
    }
    if (target.classList.contains('todo-panel')) {
      const panel = target.closest('.thread-panel');
      if (panel) refreshControls(panel);
    }
    timestamps(target.closest('.thread-panel') || target);
  });

  document.addEventListener('htmx:afterRequest', event => {
    const panel = panelFor(event);
    if (panel && origin(event).classList.contains('older')) {
      view(panel).loadingOlder = false;
      refreshControls(panel);
    }
  });

  function failed(event) {
    const xhr = event.detail.xhr;
    if (event.detail.target?.id === 'detail-stage' || origin(event)?.id === 'detail-stage') {
      if (xhr?.twToken !== navigationToken || mode !== 'opening') return;
      failedSelection = { id: requestedThread, title: $('#opening h1').textContent };
      navigationXHR = null;
      if (current) activate(current);
      else showMap(false);
      const error = $('#request-error');
      $('span', error).textContent = xhr?.status === 401 || xhr?.status === 403
        ? 'Access denied. Reconnect through the configured LAN address.'
        : 'Could not open that thread. The last loaded view is retained.';
      $('.retry-open', error).hidden = false;
      error.hidden = false;
      return;
    }
    const panel = panelFor(event);
    if (panel) {
      const state = view(panel);
      const older = origin(event).classList.contains('older');
      state.failed = true;
      state.adjusting = false;
      state.loadingOlder = false;
      refreshControls(panel);
      const error = $('.view-error', panel);
      error.dataset.request = older ? 'older' : 'poll';
      error.textContent = xhr?.status === 401 || xhr?.status === 403
        ? 'Access denied. Reconnect through the configured LAN address. Existing records are retained.'
        : older
          ? 'Older history could not be loaded. Existing records are retained; use Load older records to retry.'
          : 'Transcript refresh failed. Existing records are retained; requests retry every two seconds.';
      error.hidden = false;
      announce(error.textContent);
    }
    if (origin(event)?.id === 'index-data') { indexFailed = true; refreshConnection(); }
  }
  document.addEventListener('htmx:responseError', failed);
  document.addEventListener('htmx:sendError', failed);
  document.addEventListener('htmx:timeout', failed);

  function refreshConnection() {
    const elapsed = Math.max(0, Math.floor((Date.now() - indexChecked) / 1000));
    const stale = elapsed >= 15;
    const unhealthy = indexFailed || stale;
    const label = unhealthy ? stale ? '◌ Stale' : '↻ Retrying' : '● Connected';
    const detail = `${unhealthy ? 'last refresh' : 'refreshed'} ${elapsed}s ago`;
    for (const element of [$('#connection'), $('#rail-connection'), $('#mobile-connection')]) {
      element.textContent = element.id === 'mobile-connection' && !unhealthy ? '● Live' : label;
      element.title = `Server ${unhealthy ? stale ? 'Stale' : 'Retrying' : 'Connected'} · ${detail}`;
      element.setAttribute('aria-label', element.title);
      element.classList.toggle('warning', unhealthy);
      element.classList.toggle('healthy', !unhealthy);
    }
    $('#connection-age').textContent = detail;
    $('#server-error').hidden = !unhealthy;
    $('#server-error').textContent = `${stale ? 'Stale' : 'Retrying'} · Can't reach the Threadwatch server. Records below are the last ones loaded; requests retry every 2 seconds.`;
  }

  function resize(width) {
    root.classList.toggle('is-mobile', width < 760);
    if (width >= 760) closeSidebar(false);
    $$('.thread-panel', $('#panels')).forEach(panel => refreshControls(panel));
  }
  resize(root.getBoundingClientRect().width);
  new ResizeObserver(entries => resize(entries[0].contentRect.width)).observe(root);
  filterThreads();
  timestamps();
  const initial = $('.thread-panel');
  if (initial) activate(initial);
  else showMap(false);
  updatePending();
  setInterval(() => {
    timestamps($('#index-data'));
    if (mode === 'map') timestamps($('#map-data'));
    if (current) {
      timestamps($('.thread-header', current));
      refreshControls(current);
    }
    refreshConnection();
  }, 1000);
})();
