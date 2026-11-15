/* HTML comes from htmx. This file only keeps local view state and scroll anchors. */
(() => {
  'use strict';
  const $ = (selector, root = document) => root.querySelector(selector);
  const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
  const views = new WeakMap();
  let current = null;
  let requestedThread = null;
  let indexChecked = Date.now();
  let indexFailed = false;
  const view = panel => {
    if (!views.has(panel)) views.set(panel, { paused: false, following: true, top: 0, checked: Date.now(), failed: false, adjusting: false });
    return views.get(panel);
  };
  const origin = event => event.detail.requestConfig?.elt || event.detail.elt;
  const panelFor = event => origin(event)?.closest('.thread-panel');

  function timestamps(root = document) {
    $$('time[data-time]', root).forEach(element => {
      const event = element.closest('.event');
      if (event && element.dataset.formatted) return;
      const raw = element.dataset.time;
      const stamp = /^\d+$/.test(raw) ? Number(raw) : Date.parse(raw);
      if (!Number.isFinite(stamp)) { element.textContent = 'unknown'; return; }
      const seconds = Math.max(0, Math.floor((Date.now() - stamp) / 1000));
      if (event) element.dataset.formatted = 'true';
      element.textContent = event ? new Date(stamp).toLocaleTimeString() : seconds < 5 ? 'just now' : seconds < 60 ? `${seconds}s ago` : seconds < 3600 ? `${Math.floor(seconds / 60)}m ago` : seconds < 86400 ? `${Math.floor(seconds / 3600)}h ago` : `${Math.floor(seconds / 86400)}d ago`;
      element.title = new Date(stamp).toLocaleString();
    });
  }

  function filterThreads() {
    const query = $('#thread-search').value.trim().toLocaleLowerCase();
    const active = $('input[name="thread-filter"]:checked').value === 'active';
    let count = 0;
    $$('.thread-item').forEach(link => {
      link.hidden = (active && link.dataset.active !== 'true') || !link.dataset.search.toLocaleLowerCase().includes(query);
      if (!link.hidden) count++;
      if (link.dataset.thread === current?.dataset.thread) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    });
    $$('.bot-group').forEach(group => { group.hidden = !$('.thread-item:not([hidden])', group); });
    $('#no-threads').hidden = count > 0;
  }

  function filterEvents(panel) {
    const query = $('.transcript-search', panel).value.trim().toLocaleLowerCase();
    const events = $$('.event', panel);
    let matched = 0;
    events.forEach(event => {
      // Read the actual rendered label/output, not JSON or a truncated preview.
      const content = $('.tool-output, .message-text', event)?.textContent || '';
      event.hidden = Boolean(query) && !(`${$('.event-label', event).textContent} ${content}`).toLocaleLowerCase().includes(query);
      if (!event.hidden) matched++;
    });
    $('.record-count', panel).textContent = `${events.length} records`;
    $('.search-count', panel).textContent = query ? `${matched} matching records in ${events.length} loaded. Load older records to search further back.` : '';
    $('.empty-events', panel).hidden = matched > 0;
    $('.empty-events', panel).textContent = query ? 'No matching records in loaded history.' : 'Nothing recorded yet.';
  }

  function setScroll(panel, top) {
    const state = view(panel);
    state.adjusting = true;
    const scroll = $('.transcript-scroll', panel);
    scroll.scrollTop = top;
    state.top = scroll.scrollTop;
    requestAnimationFrame(() => { state.adjusting = false; });
  }

  function refreshControls(panel) {
    const state = view(panel);
    const paused = $('.pause', panel);
    paused.textContent = state.paused ? 'Resume' : 'Pause';
    paused.setAttribute('aria-pressed', String(state.paused));
    $$('.older', panel).forEach(button => { button.disabled = state.paused; });
    $('.jump', panel).setAttribute('aria-pressed', String(state.following));
    const stale = Date.now() - state.checked > 15000;
    const label = $('.freshness', panel);
    label.textContent = state.paused ? 'Paused · transcript frozen' : state.failed ? 'Disconnected · retrying' : stale ? 'Server data is stale' : state.following ? 'Live · following latest' : 'Live · scroll held';
    label.classList.toggle('warning', state.paused || state.failed || stale);
  }

  function initPanel(panel) {
    if (views.has(panel)) return;
    view(panel);
    const scroll = $('.transcript-scroll', panel);
    scroll.addEventListener('scroll', () => {
      const state = view(panel);
      if (state.adjusting || panel.hidden) return;
      state.top = scroll.scrollTop;
      state.following = scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop < 50;
      refreshControls(panel);
    }, { passive: true });
    filterEvents(panel);
    refreshControls(panel);
  }

  function closeSidebar() {
    $('#thread-sidebar').classList.remove('is-open');
    $('#menu').setAttribute('aria-expanded', 'false');
  }

  function activate(panel, focus = false) {
    if (current && current !== panel) view(current).top = $('.transcript-scroll', current).scrollTop;
    $$('.thread-panel').forEach(item => { item.hidden = item !== panel; });
    current = panel;
    requestedThread = panel.dataset.thread;
    initPanel(panel);
    $('#welcome').hidden = true;
    closeSidebar();
    filterThreads();
    timestamps(panel);
    const state = view(panel);
    requestAnimationFrame(() => setScroll(panel, state.following && !$('.transcript-search', panel).value ? $('.transcript-scroll', panel).scrollHeight : state.top));
    if (focus) {
      $('#main-content').focus({ preventScroll: true });
      history.replaceState(null, '', `./?thread=${encodeURIComponent(panel.dataset.thread)}`);
      if (!state.paused) htmx.trigger($('.poller', panel), 'refresh');
    }
  }

  document.addEventListener('click', event => {
    const link = event.target.closest('.thread-item');
    if (link && !event.ctrlKey && !event.metaKey && !event.shiftKey && event.button === 0) {
      requestedThread = link.dataset.thread;
      const cached = $$('.thread-panel').find(panel => panel.dataset.thread === link.dataset.thread);
      if (cached) { event.preventDefault(); event.stopPropagation(); activate(cached, true); return; }
    }
    const panel = event.target.closest('.thread-panel');
    if (panel && event.target.closest('.pause')) {
      const state = view(panel);
      state.paused = !state.paused;
      refreshControls(panel);
      if (!state.paused) htmx.trigger($('.poller', panel), 'refresh');
    }
    if (panel && event.target.closest('.jump')) {
      view(panel).following = true;
      setScroll(panel, $('.transcript-scroll', panel).scrollHeight);
      refreshControls(panel);
    }
    if (event.target.closest('#menu')) {
      const open = $('#thread-sidebar').classList.toggle('is-open');
      $('#menu').setAttribute('aria-expanded', String(open));
    }
  }, true);

  document.addEventListener('keydown', event => { if (event.key === 'Escape') closeSidebar(); });
  document.addEventListener('input', event => {
    if (event.target.id === 'thread-search') filterThreads();
    if (event.target.matches('.transcript-search')) {
      const panel = event.target.closest('.thread-panel');
      filterEvents(panel);
      if (view(panel).following && !event.target.value) setScroll(panel, $('.transcript-scroll', panel).scrollHeight);
    }
  });
  document.addEventListener('change', event => { if (event.target.name === 'thread-filter') filterThreads(); });

  document.addEventListener('htmx:configRequest', event => {
    const panel = panelFor(event);
    if (!panel) return;
    if (panel.hidden || view(panel).paused) { event.preventDefault(); return; }
    const tail = $('.events', panel).lastElementChild;
    event.detail.parameters.version = panel.dataset.version;
    event.detail.parameters.revision = panel.dataset.revision;
    event.detail.parameters.after = tail?.dataset.event || '';
  });

  document.addEventListener('htmx:beforeRequest', event => {
    const panel = panelFor(event);
    if (panel && (panel.hidden || view(panel).paused)) event.preventDefault();
  });

  document.addEventListener('htmx:beforeSwap', event => {
    if (event.detail.target.id === 'detail-stage' && origin(event)?.dataset.thread !== requestedThread) {
      event.detail.shouldSwap = false;
      return;
    }
    const panel = panelFor(event);
    if (panel) {
      if (panel.hidden || view(panel).paused) { event.detail.shouldSwap = false; return; }
      const scroll = $('.transcript-scroll', panel);
      const state = view(panel);
      state.oldHeight = scroll.scrollHeight;
      state.oldTop = scroll.scrollTop;
      state.metaTop = $('.thread-header', panel).scrollTop;
      state.todosTop = $('.todo-panel', panel).scrollTop;
      state.discordFocus = document.activeElement === $('.discord-link', panel);
      state.adjusting = true;
    }
    if (event.detail.target.id === 'index-data') {
      const nav = $('.thread-list');
      event.detail.target.dataset.scroll = nav?.scrollTop || 0;
      event.detail.target.dataset.focus = document.activeElement?.closest('.thread-item')?.dataset.thread || '';
      event.detail.target.dataset.sourceScroll = $('.sources')?.scrollTop || 0;
    }
  });

  document.addEventListener('htmx:afterSwap', event => {
    const target = event.detail.target;
    if (target.id === 'detail-stage') {
      const panel = $('.thread-panel', target);
      if (!panel) return;
      $('#panels').append(panel);
      activate(panel, true);
    } else if (target.id === 'index-data') {
      indexChecked = Date.now();
      indexFailed = false;
      filterThreads();
      $('.thread-list').scrollTop = Number(target.dataset.scroll || 0);
      $('.sources').scrollTop = Number(target.dataset.sourceScroll || 0);
      if (target.dataset.focus) $$('.thread-item').find(link => link.dataset.thread === target.dataset.focus)?.focus({ preventScroll: true });
    } else if (target.classList.contains('events')) {
      const panel = target.closest('.thread-panel');
      const state = view(panel);
      const xhr = event.detail.xhr;
      panel.dataset.version = xhr.getResponseHeader('TW-Version') ?? panel.dataset.version;
      panel.dataset.revision = xhr.getResponseHeader('TW-Revision') ?? panel.dataset.revision;
      state.checked = Date.now();
      state.failed = false;
      $('.thread-header', panel).scrollTop = state.metaTop;
      $('.todo-panel', panel).scrollTop = state.todosTop;
      if (state.discordFocus) $('.discord-link', panel)?.focus({ preventScroll: true });
      const mode = xhr.getResponseHeader('TW-Mode');
      const seen = new Set();
      $$('.event', target).forEach(record => {
        if (seen.has(record.dataset.event)) record.remove();
        else seen.add(record.dataset.event);
      });
      filterEvents(panel);
      const scroll = $('.transcript-scroll', panel);
      if (mode === 'older') {
        state.following = false;
        setScroll(panel, state.oldTop + scroll.scrollHeight - state.oldHeight);
      } else if (state.following && !$('.transcript-search', panel).value) setScroll(panel, scroll.scrollHeight);
      else setScroll(panel, mode === 'replace' ? 0 : state.oldTop);
      refreshControls(panel);
    }
    timestamps(target.closest('.thread-panel') || target);
    $('#request-error').hidden = true;
  });

  function failed(event) {
    const panel = panelFor(event);
    if (panel) { view(panel).failed = true; view(panel).adjusting = false; refreshControls(panel); }
    if (origin(event)?.id === 'index-data') indexFailed = true;
    const error = $('#request-error');
    error.textContent = event.detail.xhr?.status === 401 || event.detail.xhr?.status === 403 ? 'Access denied. Reconnect through the configured LAN address.' : 'Request failed. Existing records are retained; live requests retry every 2 seconds. Retry loading older history manually.';
    error.hidden = false;
  }
  document.addEventListener('htmx:responseError', failed);
  document.addEventListener('htmx:sendError', failed);
  document.addEventListener('htmx:timeout', failed);

  filterThreads();
  timestamps();
  const initial = $('.thread-panel');
  if (initial) activate(initial);
  setInterval(() => {
    timestamps($('#index-data'));
    if (current) { timestamps($('.thread-header', current)); refreshControls(current); }
    const stale = Date.now() - indexChecked > 15000;
    $('#connection').textContent = indexFailed ? 'Server disconnected · retrying' : stale ? 'Server data is stale · retrying' : 'Connected to server · LAN';
    $('#connection').classList.toggle('warning', indexFailed || stale);
  }, 1000);
})();
