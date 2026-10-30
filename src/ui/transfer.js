/* 90FTP - Site Transfer window.
 *
 * A dedicated tool for moving a site between two servers. It shares the Rust
 * backend with the main window but owns its whole interface: the engine picker
 * is the centre of the page, because the engine is the only thing that decides
 * how a 34,000-file tree actually travels.
 */
(function () {
  'use strict';

  const INIT = window.__INIT__ || { sources: [], is_remote: true, language: 'en' };
  // The host ships the saved sites inside __INIT__.sites; __SAVED_SITES__ is only
  // a fallback for previews. Reading one of them alone left the list empty.
  const SITES = Array.isArray(window.__SAVED_SITES__) && window.__SAVED_SITES__.length
    ? window.__SAVED_SITES__
    : (Array.isArray(INIT.sites) ? INIT.sites : []);

  let reqCounter = 0;
  const pending = new Map();
  let running = false;
  let lastProgressAt = 0;
  let startedAt = 0;

  /* ------------------------------------------------------------------ IPC */

  async function ensureIpcReady() {
    if (window.ipc && typeof window.ipc.postMessage === 'function') return true;
    if (window.chrome && window.chrome.webview && typeof window.chrome.webview.postMessage === 'function') {
      window.ipc = { postMessage: (m) => window.chrome.webview.postMessage(m) };
      return true;
    }
    for (let i = 0; i < 80; i++) {
      if (window.ipc && typeof window.ipc.postMessage === 'function') return true;
      if (window.chrome && window.chrome.webview) {
        window.ipc = { postMessage: (m) => window.chrome.webview.postMessage(m) };
        return true;
      }
      await new Promise((r) => setTimeout(r, 25));
    }
    return false;
  }

  function sendRust(action, payload = {}, timeoutMs = 60000) {
    return new Promise((resolve, reject) => {
      const id = 'tr_' + (++reqCounter);
      const timer = setTimeout(() => {
        if (pending.has(id)) {
          pending.delete(id);
          reject(new Error(action + ' timed out'));
        }
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      ensureIpcReady().then((ok) => {
        if (!ok) {
          clearTimeout(timer);
          pending.delete(id);
          reject(new Error('IPC is not available'));
          return;
        }
        window.ipc.postMessage(JSON.stringify({ id, action, payload }));
      });
    });
  }

  window.onRustResponse = function (res) {
    const entry = pending.get(res.id);
    if (!entry) return;
    pending.delete(res.id);
    clearTimeout(entry.timer);
    if (res.success) entry.resolve(res.data);
    else entry.reject(new Error(res.error || 'Request failed'));
  };

  /* ------------------------------------------------------------ utilities */

  function $(id) { return document.getElementById(id); }

  function formatSize(bytes) {
    const b = Number(bytes) || 0;
    if (b >= 1024 ** 4) return (b / 1024 ** 4).toFixed(2) + ' TB';
    if (b >= 1024 ** 3) return (b / 1024 ** 3).toFixed(2) + ' GB';
    if (b >= 1024 ** 2) return (b / 1024 ** 2).toFixed(2) + ' MB';
    if (b >= 1024) return (b / 1024).toFixed(1) + ' KB';
    return b + ' B';
  }

  function stamp() {
    const d = new Date();
    return d.toTimeString().slice(0, 8);
  }

  function log(text, kind) {
    const box = $('log');
    if (!box) return;
    const line = document.createElement('div');
    line.className = 'log-line';
    line.dataset.kind = kind || 'info';
    const ts = document.createElement('span');
    ts.className = 'ts';
    ts.textContent = stamp();
    const msg = document.createElement('span');
    msg.className = 'msg';
    msg.textContent = text;
    line.appendChild(ts);
    line.appendChild(msg);
    box.appendChild(line);
    while (box.childElementCount > 600) box.removeChild(box.firstElementChild);
    box.scrollTop = box.scrollHeight;
  }

  /* ---------------------------------------------------------------- engine */

  const MODES = ['auto', 'direct_scp', 'direct_rsync', 'stream_relay'];

  function currentMode() {
    const picked = document.querySelector('input[name="mode"]:checked');
    return picked ? picked.value : 'auto';
  }

  function isSshTarget() {
    const p = $('protocol').value;
    return p === 'sftp' || p === 'scp' || p === 'ssh';
  }

  function refreshEngines() {
    const ssh = isSshTarget();

    // Force the relay *before* the classes are recomputed, otherwise the radio
    // moves but no card gets the highlight and the page looks unselected.
    if (!ssh && currentMode() !== 'stream_relay') {
      const relay = document.querySelector('.engine[data-mode="stream_relay"] input');
      if (relay) { relay.checked = true; }
    }

    MODES.forEach((mode) => {
      const card = document.querySelector('.engine[data-mode="' + mode + '"]');
      if (!card) return;
      const radio = card.querySelector('input');
      const blocked = !ssh && mode !== 'stream_relay';
      card.classList.toggle('disabled', blocked);
      radio.disabled = blocked;
      card.classList.toggle('selected', !blocked && radio.checked);
    });

    const mode = currentMode();
    const direct = mode !== 'stream_relay';
    $('compressionField').classList.toggle('hidden', !direct);
    $('workersField').classList.toggle('hidden', direct);

    const zstd = $('compression').querySelector('option[value="zstd"]');
    if (zstd) zstd.disabled = mode === 'direct_rsync';
    if (mode === 'direct_rsync' && $('compression').value === 'zstd') $('compression').value = 'gzip';

    const chip = $('engineChip');
    const names = {
      auto: t('st.chip_auto'),
      direct_scp: t('st.chip_stream'),
      direct_rsync: t('st.chip_rsync'),
      stream_relay: t('st.chip_relay')
    };
    chip.textContent = names[mode] || mode;
    chip.classList.toggle('idle', false);

    const notes = {
      auto: t('st.note_auto'),
      direct_scp: t('st.note_stream'),
      direct_rsync: t('st.note_rsync'),
      stream_relay: t('st.note_relay')
    };
    $('engineNote').textContent = notes[mode] || '';
  }

  /* --------------------------------------------------------------- startup */

  function applyLanguage() {
    const lang = I18N.setLanguage(INIT.language || 'en');
    document.documentElement.setAttribute('lang', lang);
    document.documentElement.setAttribute('dir', I18N.isRtl && I18N.isRtl() ? 'rtl' : 'ltr');
    document.documentElement.setAttribute('data-lang', lang);
    I18N.applyTranslations(document);
  }

  function fillSources() {
    const list = Array.isArray(INIT.sources) ? INIT.sources : [];
    const box = $('sourceSummary');
    if (!list.length) {
      box.textContent = t('st.no_source');
      $('sourceHint').textContent = t('st.no_source_hint');
      return;
    }
    if (list.length === 1) {
      box.textContent = list[0];
      box.title = list[0];
    } else {
      box.textContent = t('st.items_selected', { count: list.length });
      box.title = list.join('\n');
    }
    $('sourceHint').textContent = INIT.is_remote
      ? t('st.source_hint')
      : t('st.source_hint_local');
  }

  function fillSites() {
    const sel = $('savedSite');
    sel.innerHTML = '';
    const manual = document.createElement('option');
    manual.value = 'manual';
    manual.textContent = t('st.manual');
    sel.appendChild(manual);
    SITES.forEach((s) => {
      const opt = document.createElement('option');
      opt.value = s.id;
      opt.textContent = s.name + ' (' + String(s.protocol || 'sftp').toUpperCase() + '://' + (s.username ? s.username + '@' : '') + s.host + ':' + s.port + ')';
      sel.appendChild(opt);
    });
  }

  function defaultPort(proto) {
    if (proto === 'ftp') return 21;
    if (proto === 'ftps') return 990;
    if (proto === 'ftpsi') return 21;
    if (proto === 'webdav') return 80;
    if (proto === 'webdavs') return 443;
    return 22;
  }

  function onProtocolChange() {
    $('port').value = defaultPort($('protocol').value);
    refreshEngines();
  }

  function onSavedSiteChange() {
    const id = $('savedSite').value;
    if (id === 'manual') return;
    const site = SITES.find((s) => s.id === id);
    if (!site) return;
    $('protocol').value = site.protocol || 'sftp';
    $('host').value = site.host || '';
    $('port').value = site.port || defaultPort(site.protocol || 'sftp');
    $('username').value = site.username || '';
    $('password').value = site.password || '';
    if (site.remote_dir) $('targetPath').value = site.remote_dir;
    refreshEngines();
  }

  /* ------------------------------------------------------------- payload */

  function buildTask() {
    const proto = $('protocol').value;
    return {
      source_is_remote: !!INIT.is_remote,
      sources: Array.isArray(INIT.sources) ? INIT.sources : [],
      transfer_mode: currentMode(),
      compression: currentMode() === 'stream_relay' ? 'none' : $('compression').value,
      workers: parseInt($('workers').value, 10) || 0,
      target_protocol: proto,
      target_host: $('host').value.trim(),
      target_port: parseInt($('port').value, 10) || defaultPort(proto),
      target_user: $('username').value.trim(),
      target_pass: $('password').value,
      target_auth_type: 'password',
      target_key_path: null,
      target_passphrase: null,
      target_path: $('targetPath').value.trim() || '/',
      recursive: $('recursive').checked,
      overwrite: $('overwrite').checked ? 'overwrite' : 'skip'
    };
  }

  async function verifyTarget() {
    const status = $('verifyStatus');
    const task = buildTask();
    if (!task.target_host || !task.target_user) {
      status.className = 'status-line err';
      status.textContent = t('st.need_host');
      return;
    }
    status.className = 'status-line';
    status.textContent = t('st.verifying');
    $('btnVerify').disabled = true;
    try {
      const res = await sendRust('remote_transfer_test_target', task, 60000);
      status.className = 'status-line ok';
      status.textContent = res.message || t('st.verified');
    } catch (e) {
      status.className = 'status-line err';
      status.textContent = String(e.message || e);
    } finally {
      $('btnVerify').disabled = false;
    }
  }

  function setRunning(on) {
    running = on;
    const start = $('btnStart');
    const cancel = $('btnCancel');
    start.disabled = on;
    start.innerHTML = on
      ? '<span class="spin"></span><span>' + t('st.running') + '</span>'
      : '<span>' + t('st.start') + '</span>';
    cancel.style.display = on ? 'inline-flex' : 'none';
    $('bar').classList.toggle('idle', !on);
    document.querySelectorAll('.engine').forEach((c) => {
      if (!c.classList.contains('disabled')) c.style.pointerEvents = on ? 'none' : '';
    });
  }

  async function startTransfer() {
    if (running) return;
    const task = buildTask();
    if (!task.target_host || !task.target_user) {
      $('footerStatus').className = 'status-line err';
      $('footerStatus').textContent = t('st.need_host');
      return;
    }
    if (!task.sources.length) {
      $('footerStatus').className = 'status-line err';
      $('footerStatus').textContent = t('st.no_source');
      return;
    }

    $('log').innerHTML = '';
    setProgress(0, 0, 0, 0, '—', '');
    startedAt = Date.now();
    setRunning(true);
    $('footerStatus').className = 'status-line';
    $('footerStatus').textContent = t('st.starting');

    try {
      await sendRust('remote_transfer_start', task, 3600000);
    } catch (e) {
      setRunning(false);
      $('footerStatus').className = 'status-line err';
      $('footerStatus').textContent = String(e.message || e);
    }
  }

  async function cancelTransfer() {
    if (!running) return;
    try {
      await sendRust('remote_transfer_cancel', {}, 20000);
    } catch (_) {}
    $('footerStatus').textContent = t('st.cancelling');
  }

  /* --------------------------------------------------------- engine events */

  function setProgress(pct, doneFiles, totalFiles, doneBytes, totalBytes, speed, currentFile) {
    const clamped = Math.max(0, Math.min(100, Number(pct) || 0));
    $('bar').style.width = clamped.toFixed(2) + '%';
    $('pct').textContent = clamped.toFixed(clamped >= 100 ? 0 : 1) + '%';
    if (totalFiles) $('filesRatio').textContent = doneFiles + ' / ' + totalFiles;
    if (totalBytes || doneBytes) {
      $('bytesRatio').textContent = formatSize(doneBytes) + ' / ' + formatSize(totalBytes);
    }
    if (speed) $('speed').textContent = speed;
    if (currentFile) {
      $('currentFile').textContent = currentFile;
      $('currentFile').title = currentFile;
    }
    const elapsed = startedAt ? (Date.now() - startedAt) / 1000 : 0;
    if (clamped > 0.5 && clamped < 100 && elapsed > 1) {
      const remaining = (elapsed / clamped) * (100 - clamped);
      $('eta').textContent = 'ETA ' + formatDuration(remaining);
    } else if (clamped >= 100) {
      $('eta').textContent = 'ETA 0s';
    }
  }

  function formatDuration(secs) {
    const s = Math.max(0, Math.round(secs));
    if (s < 60) return s + 's';
    if (s < 3600) return Math.floor(s / 60) + 'm ' + (s % 60) + 's';
    return Math.floor(s / 3600) + 'h ' + Math.floor((s % 3600) / 60) + 'm';
  }

  window.onRemoteTransferLog = function (payload) {
    if (!payload || !payload.text) return;
    if (payload.replace) {
      const box = $('log');
      const last = box.lastElementChild;
      if (last) {
        const msg = last.querySelector('.msg');
        if (msg) { msg.textContent = payload.text; return; }
      }
    }
    log(payload.text, payload.kind);
  };

  window.onRemoteTransferProgress = function (p) {
    if (!p) return;
    const now = Date.now();
    if (now - lastProgressAt < 120 && (p.percent || 0) < 100) return;
    lastProgressAt = now;
    setProgress(p.percent, p.done_files, p.total_files, p.done_bytes, p.total_bytes, p.speed, p.current_file);
  };

  window.onRemoteTransferComplete = function (p) {
    setRunning(false);
    if (!p) return;
    if (p.success) {
      setProgress(100, p.count || 0, p.count || 0, p.bytes || 0, p.bytes || 0, '', t('st.done'));
      $('footerStatus').className = 'status-line ok';
      $('footerStatus').textContent = t('st.done_summary', {
        count: p.count || 0,
        size: formatSize(p.bytes || 0),
        time: formatDuration(p.elapsed_secs || 0)
      });
    } else {
      $('footerStatus').className = 'status-line err';
      $('footerStatus').textContent = p.message || t('st.failed');
    }
  };

  window.onRemoteTransferError = function (p) {
    setRunning(false);
    $('footerStatus').className = 'status-line err';
    $('footerStatus').textContent = (p && p.error) || t('st.failed');
    if (p && p.error) log(p.error, 'error');
  };

  /* ----------------------------------------------------------------- boot */

  function boot() {
    applyLanguage();
    fillSources();
    fillSites();
    $('port').value = defaultPort($('protocol').value);

    document.querySelectorAll('.engine').forEach((card) => {
      card.addEventListener('click', (ev) => {
        if (card.classList.contains('disabled')) { ev.preventDefault(); return; }
        const radio = card.querySelector('input');
        if (radio && !radio.disabled) radio.checked = true;
        refreshEngines();
      });
    });

    refreshEngines();
    log(t('st.boot_log'), 'info');
    setRunning(false);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot, { once: true });
  } else {
    boot();
  }

  window.refreshEngines = refreshEngines;
  window.onProtocolChange = onProtocolChange;
  window.onSavedSiteChange = onSavedSiteChange;
  window.verifyTarget = verifyTarget;
  window.startTransfer = startTransfer;
  window.cancelTransfer = cancelTransfer;

  /* Called by the host when the window is reused for a new selection. */
  window.applyInit = function (params) {
    if (!params) return;
    INIT.sources = Array.isArray(params.sources) ? params.sources : [];
    if (typeof params.is_remote === 'boolean') INIT.is_remote = params.is_remote;
    if (params.language && params.language !== INIT.language) {
      INIT.language = params.language;
      applyLanguage();
    }
    if (Array.isArray(params.sites)) {
      SITES.length = 0;
      params.sites.forEach((s) => SITES.push(s));
    }
    // Always refill: the "manual" entry is JS-generated, so it does not follow
    // data-i18n when the language changes.
    fillSites();
    fillSources();
    refreshEngines();
    if (running) {
      log(t('st.new_selection'), 'warning');
    }
  };
})();
