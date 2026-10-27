window.onerror = function(msg, url, line, col, error) {
      console.error(`JS error at ${line}:${col} - ${msg}`, error);
    };

    // Safe In-Memory / LocalStorage Abstraction (Prevents WebView2 SecurityError on opaque origin)
    const appStorage = {
      _data: {},
      getItem: function(key) {
        try {
          const ls = window.localStorage;
          if (ls) {
            const v = ls.getItem(key);
            if (v !== null) return v;
          }
        } catch (_) {}
        return this._data[key] || null;
      },
      setItem: function(key, val) {
        this._data[key] = val;
        try {
          const ls = window.localStorage;
          if (ls) ls.setItem(key, val);
        } catch (_) {}
      },
      removeItem: function(key) {
        delete this._data[key];
        try {
          const ls = window.localStorage;
          if (ls) ls.removeItem(key);
        } catch (_) {}
      }
    };

    // State
    let currentLocalPath = '';
    let currentRemotePath = '';
    let localFiles = [];
    let remoteFiles = [];
    let selectedLocalFile = null;
    let selectedRemoteFile = null;
    let isConnected = false;
    let savedSites = [];
    let selectedQuickSite = null;
    let currentConnectedProfile = null;
    let lastConnectedProfile = null;
    let keepAliveTimer = null;
    let silentReconnectPromise = null;
    let isSyncNavigating = false;

    // Software Remote Update Global State
    let APP_CURRENT_VERSION = '0.0.68';
    const GITHUB_REPO_PATH = 'aljupcom/90FTP';
    let currentDownloadUrl = '';
    let currentDownloadFilename = '';
    let currentLatestVersion = '';
    let currentDownloadedInstallerPath = '';

    // Editor state
    let activeEditingPath = '';
    let activeEditingIsRemote = false;

    // Resizing State (Panes & Log Console)
    let isDraggingVertical = false;
    let isDraggingHorizontal = false;

    // IPC Communication
    let reqCounter = 0;
    const pendingCallbacks = new Map();

    async function ensureIpcReady() {
      if (window.ipc && typeof window.ipc.postMessage === 'function') return true;
      if (window.chrome && window.chrome.webview && typeof window.chrome.webview.postMessage === 'function') {
        window.ipc = { postMessage: (msg) => window.chrome.webview.postMessage(msg) };
        return true;
      }
      for (let i = 0; i < 80; i++) {
        if (window.ipc && typeof window.ipc.postMessage === 'function') return true;
        if (window.chrome && window.chrome.webview && typeof window.chrome.webview.postMessage === 'function') {
          window.ipc = { postMessage: (msg) => window.chrome.webview.postMessage(msg) };
          return true;
        }
        await new Promise(r => setTimeout(r, 25));
      }
      return false;
    }

    async function sendRust(action, payload = {}) {
      await ensureIpcReady();
      return new Promise((resolve, reject) => {
        const id = 'req_' + (++reqCounter);
        const timeoutMs = ['upload', 'download', 'remote_compress', 'remote_extract', 'sync_plan', 'sync_apply'].includes(action)
          ? 600000 
          : (['remote_install_sshpass'].includes(action)
              ? 180000
              : (['connect', 'reconnect', 'ping_keepalive', 'remote_read', 'remote_write', 'remote_list', 'remote_cd', 'terminal_exec', 'local_compress', 'local_extract', 'remote_check_sshpass'].includes(action) 
                  ? 60000 
                  : 15000));

        const timer = setTimeout(() => {
          if (pendingCallbacks.has(id)) {
            pendingCallbacks.delete(id);
            reject(new Error(`Request '${action}' timed out after ${timeoutMs / 1000}s`));
          }
        }, timeoutMs);

        pendingCallbacks.set(id, {
          resolve: (data) => {
            clearTimeout(timer);
            resolve(data);
          },
          reject: (err) => {
            clearTimeout(timer);
            reject(err);
          }
        });

        try {
          if (window.ipc && typeof window.ipc.postMessage === 'function') {
            window.ipc.postMessage(JSON.stringify({ id, action, payload }));
          } else if (window.chrome && window.chrome.webview && typeof window.chrome.webview.postMessage === 'function') {
            window.chrome.webview.postMessage(JSON.stringify({ id, action, payload }));
          } else {
            clearTimeout(timer);
            pendingCallbacks.delete(id);
            reject(new Error('IPC bridge between WebView and Native Rust engine is not ready.'));
          }
        } catch (e) {
          clearTimeout(timer);
          pendingCallbacks.delete(id);
          reject(e);
        }
      });
    }

    window.onRustResponse = function(resp) {
      if (typeof resp === 'string') {
        try {
          resp = JSON.parse(resp);
        } catch (e) {
          console.error('Failed to parse onRustResponse JSON string:', e);
          return;
        }
      }
      if (!resp || !resp.id) return;
      const cb = pendingCallbacks.get(resp.id);
      if (cb) {
        pendingCallbacks.delete(resp.id);
        if (resp.success) {
          if (resp.data && resp.data.reconnected === true) {
            try {
              onBackendAutoReconnected(resp.data);
            } catch (_) {}
          }
          cb.resolve(resp.data);
        } else {
          cb.reject(new Error(resp.error || 'Unknown error'));
        }
      }
    };

    // Logging (Supports bilingual output with Arabic/English)
    function addLog(msg, type = 'info') {
      const body = document.getElementById('logBody');
      if (!body) return;
      const timeStr = (appOptions && appOptions.logTimestamps === false) ? '' : '<span class="log-time">[' + new Date().toLocaleTimeString() + ']</span> ';
      const div = document.createElement('div');
      div.className = 'log-line';
      div.innerHTML = timeStr + '<span class="log-msg ' + type + '">' + escapeHtml(msg) + '</span>';
      body.appendChild(div);

      const maxLines = (appOptions && appOptions.maxLogLines) || 500;
      while (body.children.length > maxLines) {
        body.removeChild(body.firstChild);
      }
      body.scrollTop = body.scrollHeight;
    }

    function copyTextToClipboard(text, notifyMsg = 'Copied to clipboard!') {
      if (!text) return;
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(() => {
          addLog(notifyMsg, 'success');
        }).catch(() => {
          fallbackCopyText(text, notifyMsg);
        });
      } else {
        fallbackCopyText(text, notifyMsg);
      }
    }

    function fallbackCopyText(text, notifyMsg = 'Copied to clipboard!') {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      document.body.removeChild(ta);
      addLog(notifyMsg, 'success');
    }

    function copyAllLogs() {
      const body = document.getElementById('logBody');
      if (!body) return;
      const lines = Array.from(body.querySelectorAll('.log-line'))
        .map(l => l.innerText)
        .join('\n');
      if (!lines) {
        alert('Activity log is currently empty.');
        return;
      }
      copyTextToClipboard(lines, 'All console logs copied to clipboard!');
    }

    function copyErrorsOnly() {
      const body = document.getElementById('logBody');
      if (!body) return;
      const errorLines = Array.from(body.querySelectorAll('.log-line'))
        .filter(l => l.querySelector('.log-msg.error'))
        .map(l => l.innerText);
      if (errorLines.length === 0) {
        alert('No error logs found in the current activity log.');
        return;
      }
      copyTextToClipboard(errorLines.join('\n'), `Copied ${errorLines.length} error line(s) to clipboard!`);
    }

    function exportLogsToFile() {
      const body = document.getElementById('logBody');
      if (!body) return;
      const text = Array.from(body.querySelectorAll('.log-line')).map(l => l.innerText).join('\n');
      if (!text.trim()) {
        alert('Activity log is currently empty.');
        return;
      }
      const blob = new Blob([text], { type: 'text/plain;charset=utf-8' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = `90ftp_activity_log_${new Date().toISOString().replace(/[:.]/g, '-')}.log`;
      a.click();
      URL.revokeObjectURL(a.href);
      addLog('Activity log downloaded to local file.', 'success');
    }

    function copyEditorContent() {
      const textarea = document.getElementById('editorTextarea');
      if (!textarea || !textarea.value) {
        alert('Editor is currently empty.');
        return;
      }
      copyTextToClipboard(textarea.value, 'Editor content copied to clipboard!');
    }

    function copyRtLog() {
      const box = document.getElementById('rtLogBox');
      if (!box) return;
      const text = Array.from(box.querySelectorAll('.rt-log-entry, div')).map(d => d.innerText).join('\n');
      if (!text.trim()) {
        alert('Remote transfer log is currently empty.');
        return;
      }
      copyTextToClipboard(text, 'Remote transfer logs copied to clipboard!');
    }

    function copyRtErrorsOnly() {
      const box = document.getElementById('rtLogBox');
      if (!box) return;
      const errorEntries = Array.from(box.querySelectorAll('.rt-log-entry, div'))
        .filter(d => d.textContent.includes('✕') || d.textContent.toLowerCase().includes('failed') || d.textContent.toLowerCase().includes('error'))
        .map(d => d.innerText);
      if (errorEntries.length === 0) {
        alert('No errors found in the current remote transfer session.');
        return;
      }
      copyTextToClipboard(errorEntries.join('\n'), `Copied ${errorEntries.length} transfer error(s) to clipboard!`);
    }

    function clearRtLog() {
      const box = document.getElementById('rtLogBox');
      if (box) box.innerHTML = '<div style="color: var(--text-dim);">(Log cleared)</div>';
    }

    function handleRtLogContextMenu(e) {
      e.preventDefault();
      openContextMenu(e, 'rt_log');
    }

    function copyAllTransferQueueErrors() {
      if (!transferTasks || transferTasks.length === 0) {
        alert('Transfer queue is empty.');
        return;
      }
      const errorLines = transferTasks
        .filter(t => t.state === 'failed' && t.error)
        .map(t => `[${t.kind.toUpperCase()}] ${transferName(t)}: ${t.error} (${transferSource(t)} -> ${transferDestination(t)})`);
      if (errorLines.length === 0) {
        alert('No transfer errors in the queue.');
        return;
      }
      copyTextToClipboard(errorLines.join('\n'), `Copied ${errorLines.length} transfer error(s) to clipboard!`);
    }

    function clearLogs() {
      document.getElementById('logBody').innerHTML = '';
      addLog('Console cleared.', 'info');
    }

    // Escapes text for BOTH element and attribute context. The previous version
    // round-tripped through innerText/innerHTML, which leaves `"` and `'` intact —
    // any remote-controlled name interpolated into a `title="..."` could therefore
    // break out of the attribute and inject markup.
    function escapeHtml(text) {
      if (text === null || text === undefined) return '';
      return String(text)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    function formatBytes(bytes) {
      const b = Number(bytes);
      if (!b || b <= 0 || isNaN(b)) return '0 B';
      const k = 1024;
      const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
      const i = Math.max(0, Math.min(sizes.length - 1, Math.floor(Math.log(b) / Math.log(k))));
      return parseFloat((b / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
    }

    // Remote History Management
    function updateRemoteHistory(historyList) {
      if (!Array.isArray(historyList)) return;
      recentRemoteHistoryList = historyList;
      const datalist = document.getElementById('remoteHistoryDatalist');
      const select = document.getElementById('remoteHistorySelect');
      if (datalist) {
        datalist.innerHTML = '';
        historyList.forEach(path => {
          const opt = document.createElement('option');
          opt.value = path;
          datalist.appendChild(opt);
        });
      }
      if (select) {
        select.innerHTML = '<option value="" disabled selected>Recent Paths</option>';
        historyList.forEach(path => {
          const opt = document.createElement('option');
          opt.value = path;
          opt.textContent = path;
          select.appendChild(opt);
        });
      }
    }

    function onRemoteHistorySelected(path) {
      if (!path) return;
      document.getElementById('remotePathInput').value = path;
      navigateRemote(path);
    }

    function isProfileMatchingSite(profile, site) {
      if (!profile || !site) return false;
      if (profile.site_id && site.id && profile.site_id === site.id) {
        return profile.host === site.host &&
               String(profile.port || 22) === String(site.port || 22) &&
               profile.username === site.username &&
               (profile.protocol || 'sftp') === (site.protocol || 'sftp');
      }
      return profile.host === site.host &&
             String(profile.port || 22) === String(site.port || 22) &&
             profile.username === site.username &&
             (profile.protocol || 'sftp') === (site.protocol || 'sftp') &&
             (profile.remote_dir || '') === (site.remote_dir || '');
    }

    // Quick Connect Server Selector (Sites from Site Manager)
    function updateQuickSiteSelect() {
      const sel = document.getElementById('quickSiteSelect');
      if (!sel) return;
      const currentVal = sel.value;
      sel.innerHTML = '<option value="manual">Manual Input...</option>';

      if (savedSites && savedSites.length > 0) {
        savedSites.forEach(s => {
          const opt = document.createElement('option');
          opt.value = s.id;
          opt.textContent = `${s.name} (${s.host})`;
          sel.appendChild(opt);
        });
      }

      if (currentVal && currentVal !== 'manual' && savedSites.some(s => s.id === currentVal)) {
        sel.value = currentVal;
        onQuickSiteSelect(currentVal, false);
      } else if (selectedQuickSite && savedSites.some(s => s.id === selectedQuickSite.id)) {
        sel.value = selectedQuickSite.id;
        onQuickSiteSelect(selectedQuickSite.id, false);
      } else {
        sel.value = 'manual';
        onQuickSiteSelect('manual', false);
      }
    }

    function onQuickSiteSelect(val, userTriggered = false) {
      const manualGroup = document.getElementById('manualFieldsGroup');
      const siteBadge = document.getElementById('selectedSiteBadge');
      const badgeInfo = document.getElementById('selectedSiteBadgeInfo');
      const btnConn = document.getElementById('btnConnect');
      const btnRec = document.getElementById('btnReconnect');
      const btnDisc = document.getElementById('btnDisconnect');

      if (val === 'manual' || !val) {
        selectedQuickSite = null;
        if (manualGroup) manualGroup.style.display = 'flex';
        if (siteBadge) siteBadge.style.display = 'none';
        if (userTriggered) {
          if (isConnected) {
            stopKeepAliveTimer();
            sendRust('disconnect', { wipe_credentials: true }).catch(() => {});
            isConnected = false;
            currentConnectedProfile = null;
          }
          lastConnectedProfile = null;
          remoteFiles = [];
          selectedRemoteFile = null;
          currentRemotePath = '';
          if (typeof clearQuickFilter === 'function') clearQuickFilter(true);
          const rInp = document.getElementById('remotePathInput');
          if (rInp) rInp.value = 'Not connected';
          const rList = document.getElementById('remoteFileList');
          if (rList) {
            rList.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 50px;">Enter server credentials above and click Connect.</td></tr>`;
          }
          updatePaneStatsBadge(true);
          setConnectedState(false);
        }
        if (!isConnected) {
          if (btnConn) btnConn.style.display = 'inline-flex';
          if (btnRec) btnRec.style.display = 'none';
          if (btnDisc) btnDisc.style.display = 'none';
        }
        return;
      }

      const site = savedSites.find(s => s.id === val);
      if (!site) return;

      const activeProfile = currentConnectedProfile || lastConnectedProfile;
      const isDifferentServer = !isProfileMatchingSite(activeProfile, site);

      selectedQuickSite = site;
      if (manualGroup) manualGroup.style.display = 'none';

      // When user explicitly selects a different server in the dropdown, clear the old server's state & files immediately
      if (userTriggered && isDifferentServer) {
        if (isConnected) {
          stopKeepAliveTimer();
          sendRust('disconnect', { wipe_credentials: true }).catch(() => {});
          isConnected = false;
          currentConnectedProfile = null;
          addLog(`Switched target server to '${site.name}' (${site.host}). Previous session closed.`, 'info');
        }
        lastConnectedProfile = null;
        remoteFiles = [];
        selectedRemoteFile = null;
        currentRemotePath = site.remote_dir || '/';
        if (typeof clearQuickFilter === 'function') clearQuickFilter(true);

        const rInp = document.getElementById('remotePathInput');
        if (rInp) rInp.value = site.remote_dir || '/';
        const rList = document.getElementById('remoteFileList');
        if (rList) {
          rList.innerHTML = `
            <tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 42px 15px;">
              <div style="display:flex; flex-direction:column; align-items:center; gap:10px;">
                <span style="color: var(--text-main); font-weight: 700; font-size: 13px;">Selected Server: ${escapeHtml(site.name)} (${escapeHtml(site.host)})</span>
                <span style="font-size: 11.5px; color: var(--text-muted);">Ready to connect (${site.protocol.toUpperCase()}://${escapeHtml(site.username)}@${escapeHtml(site.host)}:${site.port})</span>
                <button type="button" class="btn btn-primary" onclick="handleConnectClick()" style="padding: 5px 16px; font-weight: 600; margin-top: 2px;">
                  <svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg> Connect to ${escapeHtml(site.name)}
                </button>
              </div>
            </td></tr>
          `;
        }
        updatePaneStatsBadge(true);
        setConnectedState(false);
      }

      if (!isConnected) {
        if (!isDifferentServer && lastConnectedProfile) {
          if (btnRec) btnRec.style.display = 'inline-flex';
          if (btnConn) btnConn.style.display = 'none';
          if (btnDisc) btnDisc.style.display = 'none';
        } else {
          if (btnConn) btnConn.style.display = 'inline-flex';
          if (btnRec) btnRec.style.display = 'none';
          if (btnDisc) btnDisc.style.display = 'none';
        }
      } else if (isDifferentServer) {
        if (btnConn) btnConn.style.display = 'inline-flex';
        if (btnRec) btnRec.style.display = 'none';
      }

      if (siteBadge && badgeInfo) {
        siteBadge.style.display = 'flex';
        const authDesc = site.auth_type === 'key' ? 'SSH Key' : 'Password';
        badgeInfo.innerHTML = `
          <svg class="icon icon-sm" viewBox="0 0 24 24" style="color:#38bdf8; flex-shrink:0;"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>
          <span style="color:#f8fafc; font-weight:700;">${escapeHtml(site.name)}</span>
          <span style="color:var(--text-dim);">&bull;</span>
          <span style="color:#38bdf8; font-family:var(--font-mono); font-size:12px;">${site.protocol.toUpperCase()}://${site.username}@${site.host}:${site.port}</span>
          <span style="background:rgba(34,197,94,0.12); border:1px solid rgba(34,197,94,0.3); padding:1px 7px; border-radius:4px; font-size:11px; color:#4ade80;">${authDesc}</span>
        `;
      }
    }

    function resetQuickSiteSelect() {
      const sel = document.getElementById('quickSiteSelect');
      if (sel) sel.value = 'manual';
      onQuickSiteSelect('manual', true);
    }

    // Loading overlay controls & live step progression
    let connectionStepTimers = [];

    function clearLoadingTimers() {
      connectionStepTimers.forEach(t => clearTimeout(t));
      connectionStepTimers = [];
    }

    function setLoadStep(step, text, state) {
      const el = document.getElementById('loadStep' + step);
      if (!el) return;
      if (state === 'active') {
        el.className = 'loading-log-line active';
        el.innerHTML = `<span class="spin-icon">↻</span> <span>${text}</span>`;
      } else if (state === 'done') {
        el.className = 'loading-log-line done';
        el.innerHTML = `<span style="color:#22c55e;">✓</span> <span>${text}</span>`;
      } else if (state === 'success') {
        el.className = 'loading-log-line final-success';
        el.innerHTML = `<span><svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg></span> <span style="color:#4ade80; font-weight:700;">${text}</span>`;
      } else {
        el.className = 'loading-log-line';
        el.innerHTML = `<span style="opacity:0.35;">○</span> <span>${text}</span>`;
      }
    }

    function showConnectionLoading(params) {
      clearLoadingTimers();
      const overlay = document.getElementById('connectionLoadingOverlay');
      if (!overlay) return;

      const host = params.host || 'Server';
      const port = params.port || 22;
      const proto = (params.protocol || 'SFTP').toUpperCase();
      const user = params.username || 'user';

      document.getElementById('loadingTitle').innerText = `Connecting to ${host}...`;
      document.getElementById('loadingSubtitle').innerText = `Authenticating (${user}) via ${proto} protocol`;

      const spinner = document.getElementById('loadingSpinner');
      spinner.style.borderColor = 'rgba(59, 130, 246, 0.2)';
      spinner.style.borderTopColor = '#38bdf8';

      const existingFinal = document.getElementById('loadFinalSuccess');
      if (existingFinal) existingFinal.remove();

      setLoadStep(1, `<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> Initializing socket connection: ${host}:${port}`, 'active');
      setLoadStep(2, `<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg> Negotiating ${proto} cryptographic handshake`, 'pending');
      setLoadStep(3, `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg> Authenticating credentials & verifying host key`, 'pending');
      setLoadStep(4, `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg> Retrieving remote directory tree`, 'pending');

      overlay.classList.add('active');

      connectionStepTimers.push(setTimeout(() => {
        if (pendingConnection) {
          setLoadStep(1, `<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> Connected to port ${port} (${host})`, 'done');
          setLoadStep(2, `<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg> Negotiating ${proto} protocol handshake...`, 'active');
        }
      }, 350));

      connectionStepTimers.push(setTimeout(() => {
        if (pendingConnection) {
          setLoadStep(2, `<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg> Cryptographic handshake complete with server`, 'done');
          setLoadStep(3, `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg> Verifying credentials for user '${user}'...`, 'active');
        }
      }, 750));
    }

    function finishConnectionSuccess(remotePwd) {
      clearLoadingTimers();
      setLoadStep(1, `<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> Connected to remote port successfully`, 'done');
      setLoadStep(2, `<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg> Cryptographic handshake secured`, 'done');
      setLoadStep(3, `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg> User credentials authenticated`, 'done');
      setLoadStep(4, `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg> Remote working directory loaded: ${remotePwd || '/'}`, 'done');

      document.getElementById('loadingTitle').innerText = 'Connection Established!';
      document.getElementById('loadingSubtitle').innerText = 'Session initialized and ready for file transfers';

      const spinner = document.getElementById('loadingSpinner');
      if (spinner) {
        spinner.style.borderColor = 'rgba(34, 197, 94, 0.2)';
        spinner.style.borderTopColor = '#22c55e';
      }

      const box = document.getElementById('loadingLogBox');
      let finalEl = document.getElementById('loadFinalSuccess');
      if (!finalEl) {
        finalEl = document.createElement('div');
        finalEl.id = 'loadFinalSuccess';
        finalEl.className = 'loading-log-line final-success';
        finalEl.innerHTML = `<span><svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg></span> <span>Connection Linked Successfully</span>`;
        box.appendChild(finalEl);
      }

      setTimeout(() => {
        hideConnectionLoading();
      }, 850);
    }

    function hideConnectionLoading() {
      clearLoadingTimers();
      const overlay = document.getElementById('connectionLoadingOverlay');
      if (overlay) {
        overlay.classList.remove('active');
      }
    }

    // Drag and Drop Initialization
    function setupDragAndDrop() {
      const leftPane = document.getElementById('leftPane');
      const rightPane = document.getElementById('rightPane');

      // Local Drop Target (Downloads from Remote)
      leftPane.addEventListener('dragover', (e) => {
        e.preventDefault();
        if (activeDragPayload && activeDragPayload.source === 'local') {
          e.dataTransfer.dropEffect = 'move';
          return;
        }
        e.dataTransfer.dropEffect = 'copy';
        leftPane.classList.add('drag-target');
      });
      leftPane.addEventListener('dragleave', (e) => {
        if (!leftPane.contains(e.relatedTarget)) {
          leftPane.classList.remove('drag-target');
        }
      });
      leftPane.addEventListener('drop', async (e) => {
        e.preventDefault();
        leftPane.classList.remove('drag-target');
        const text = e.dataTransfer.getData('text/plain');
        if (!text) return;
        try {
          const data = JSON.parse(text);
          if (data.source === 'remote') {
            const batch = Array.isArray(data.items) && data.items.length > 0
              ? data.items
              : [{ path: data.path, name: data.name, is_dir: data.is_dir }];
            for (const item of batch) {
              if (item.is_dir) {
                await downloadFolderDirect(item.path, item.name);
              } else {
                await downloadFileDirect(item.path, item.name);
              }
            }
          }
        } catch (_) {}
      });

      // Remote Drop Target (Uploads from Local)
      rightPane.addEventListener('dragover', (e) => {
        e.preventDefault();
        if (activeDragPayload && activeDragPayload.source === 'remote') {
          e.dataTransfer.dropEffect = 'move';
          return;
        }
        e.dataTransfer.dropEffect = 'copy';
        rightPane.classList.add('drag-target');
      });
      rightPane.addEventListener('dragleave', (e) => {
        if (!rightPane.contains(e.relatedTarget)) {
          rightPane.classList.remove('drag-target');
        }
      });
      rightPane.addEventListener('drop', async (e) => {
        e.preventDefault();
        rightPane.classList.remove('drag-target');

        if (!isConnected) {
          alert('Please connect to a server before dropping files to upload');
          return;
        }

        // 1. Internal drag from left pane
        const text = e.dataTransfer.getData('text/plain');
        if (text) {
          try {
            const data = JSON.parse(text);
            if (data.source === 'local') {
              const batch = Array.isArray(data.items) && data.items.length > 0
                ? data.items
                : [{ path: data.path, name: data.name, is_dir: data.is_dir }];
              for (const item of batch) {
                if (item.is_dir) {
                  await uploadFolderDirect(item.path, item.name);
                } else {
                  await uploadFileDirect(item.path, item.name);
                }
              }
              return;
            }
          } catch (_) {}
        }

        // 2. External drag & drop from Windows Explorer / Desktop
        if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
          const dropped = Array.from(e.dataTransfer.files);
          for (const f of dropped) {
            // WebView2 does not expose File.path for externally dropped files.
            // The old code guessed "<current local folder>/<name>", which
            // silently uploaded a *different* file that happened to share the
            // name. Only trust the guess when it points at a real file of the
            // same size; otherwise say so instead of transferring the wrong data.
            let resolvedPath = f.path || '';
            if (!resolvedPath && currentLocalPath) {
              const guess = currentLocalPath.endsWith('\\')
                ? currentLocalPath + f.name
                : currentLocalPath + '\\' + f.name;
              try {
                const st = await sendRust('local_stat', { path: guess });
                if (st && st.exists && !st.is_dir && st.size === f.size) {
                  resolvedPath = guess;
                }
              } catch (_) {}
            }
            if (!resolvedPath) {
              addLog(`Dropped item '${f.name}': its local path could not be resolved. Drag it from the 90FTP local pane, or use the Upload button.`, 'error');
              continue;
            }
            await uploadFileDirect(resolvedPath, f.name);
          }
        }
      });
    }

    // App Initialization
    async function initApp() {
      initTheme();
      initOptions();
      loadExclusionMaskConfig();
      addLog('Starting 90FTP Modern Site Manager...', 'info');
      setupDragAndDrop();

      // Setup Pane-level right click for blank areas
      const leftPane = document.getElementById('leftPane');
      const rightPane = document.getElementById('rightPane');
      if (leftPane) {
        leftPane.addEventListener('contextmenu', (e) => {
          if (e.target.closest('tr')) return;
          e.preventDefault();
          openContextMenu(e, 'local_blank');
        });
      }
      if (rightPane) {
        rightPane.addEventListener('contextmenu', (e) => {
          if (e.target.closest('tr')) return;
          e.preventDefault();
          openContextMenu(e, 'remote_blank');
        });
      }

      const logBodyEl = document.getElementById('logBody');
      if (logBodyEl) {
        logBodyEl.addEventListener('contextmenu', (e) => {
          e.preventDefault();
          const targetLine = e.target.closest('.log-line');
          openContextMenu(e, 'log', targetLine);
        });
      }

      // Close menubar dropdowns when clicking anywhere outside
      document.addEventListener('click', (e) => {
        if (!e.target.closest('.desktop-menubar')) {
          closeAllMenus();
        }
      });

      // Brief delay to allow WebView2 native message loop to attach
      await new Promise(r => setTimeout(r, 80));

      let loaded = false;
      try {
        const data = await sendRust('init');
        if (data) {
          if (data.initial_dir) {
            currentLocalPath = data.initial_dir;
            const pathInp = document.getElementById('localPathInput');
            if (pathInp) pathInp.value = currentLocalPath;
          }

          if (data.drives && data.drives.length > 0) {
            const driveSelect = document.getElementById('driveSelect');
            if (driveSelect) {
              driveSelect.innerHTML = '';
              data.drives.forEach(d => {
                const opt = document.createElement('option');
                opt.value = d;
                opt.innerText = d;
                driveSelect.appendChild(opt);
              });
              if (currentLocalPath) {
                const dl = currentLocalPath.substring(0, 3).toUpperCase();
                driveSelect.value = dl;
              }
            }
          }

          if (data.drives_info && data.drives_info.length > 0) {
            globalDrivesInfo = data.drives_info;
          }
          if (data.desktop_dir) {
            globalDesktopDir = data.desktop_dir;
          }

          if (data.app_version) {
            APP_CURRENT_VERSION = data.app_version;
            const curBadge = document.getElementById('updateCurrentBadge');
            if (curBadge) curBadge.textContent = 'Current: v' + APP_CURRENT_VERSION;
            const aboutVer = document.getElementById('aboutAppVersion');
            if (aboutVer) aboutVer.textContent = 'Version ' + APP_CURRENT_VERSION + ' Dev (Windows x64)';
            const topBadge = document.getElementById('topBrandBadge');
            if (topBadge) topBadge.textContent = 'Modern Site Manager v' + APP_CURRENT_VERSION;
          }

          if (data.remote_history) {
            updateRemoteHistory(data.remote_history);
          }

          savedSites = data.sites || [];
          updateQuickSiteSelect();
          if (data.local_files && Array.isArray(data.local_files)) {
            renderLocalFiles(data.local_files);
            loaded = true;
          }
          syncCurrentLocalTabState();
          renderRemoteTabsBar();
          addLog('Local file manager initialized. Ready to connect.', 'success');
        }
      } catch (err) {
        addLog('Initialization notice: ' + err.message, 'warning');
      }

      // Wire drag-and-drop onto Parent Folder (↑) buttons in path bars
      const btnLocUp = document.getElementById('btnLocalUpFolder');
      if (btnLocUp) {
        btnLocUp.addEventListener('dragover', (e) => {
          if (activeDragPayload && activeDragPayload.source === 'local' && !isLocalRoot(currentLocalPath)) {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'move';
            btnLocUp.style.borderColor = '#22c55e';
            btnLocUp.style.background = 'rgba(34, 197, 94, 0.18)';
          }
        });
        btnLocUp.addEventListener('dragleave', () => {
          btnLocUp.style.borderColor = '';
          btnLocUp.style.background = '';
        });
        btnLocUp.addEventListener('drop', async (e) => {
          e.preventDefault();
          btnLocUp.style.borderColor = '';
          btnLocUp.style.background = '';
          if (activeDragPayload && activeDragPayload.source === 'local' && !isLocalRoot(currentLocalPath)) {
            const parentDir = getLocalParentDir(currentLocalPath);
            const items = Array.isArray(activeDragPayload.items) && activeDragPayload.items.length > 0
              ? activeDragPayload.items
              : [{ path: activeDragPayload.path, name: activeDragPayload.name, is_dir: activeDragPayload.is_dir }];
            await executeInteractiveMove(false, items, parentDir, e.ctrlKey || e.altKey, false);
          }
        });
      }

      const btnRemUp = document.getElementById('btnRemoteUpFolder');
      if (btnRemUp) {
        btnRemUp.addEventListener('dragover', (e) => {
          if (activeDragPayload && activeDragPayload.source === 'remote' && !isRemoteRoot(currentRemotePath)) {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'move';
            btnRemUp.style.borderColor = '#22c55e';
            btnRemUp.style.background = 'rgba(34, 197, 94, 0.18)';
          }
        });
        btnRemUp.addEventListener('dragleave', () => {
          btnRemUp.style.borderColor = '';
          btnRemUp.style.background = '';
        });
        btnRemUp.addEventListener('drop', async (e) => {
          e.preventDefault();
          btnRemUp.style.borderColor = '';
          btnRemUp.style.background = '';
          if (activeDragPayload && activeDragPayload.source === 'remote' && !isRemoteRoot(currentRemotePath)) {
            const parentDir = getRemoteParentDir(currentRemotePath);
            const items = Array.isArray(activeDragPayload.items) && activeDragPayload.items.length > 0
              ? activeDragPayload.items
              : [{ path: activeDragPayload.path, name: activeDragPayload.name, is_dir: activeDragPayload.is_dir }];
            await executeInteractiveMove(true, items, parentDir, e.ctrlKey || e.altKey, false);
          }
        });
      }

      // If initial files were not loaded, trigger resilient navigateLocal fallback
      // Instant background update check on app startup
      setTimeout(() => {
        checkUpdatesSilentlyOnStartup();
      }, 1000);

      if (!loaded || !localFiles || localFiles.length === 0) {
        try {
          const fallbackPath = currentLocalPath || 'C:\\Users\\aljailane\\Desktop';
          await navigateLocal(fallbackPath);
        } catch (_) {
          try {
            await navigateLocal('C:\\');
          } catch (_) {}
        }
      }
    }

    function isLocalRoot(path) {
      if (!path) return true;
      const p = path.trim().replace(/[/\\]+$/, '');
      return /^[a-zA-Z]:?$/.test(p) || p === '' || p === '\\' || p === '/';
    }

    function isRemoteRoot(path) {
      if (!path) return true;
      const p = path.trim();
      return p === '/' || p === '.' || p === '';
    }

    // ========================================================
    // Interactive File Movement, Quick Filter & Multi-Selection State
    // ========================================================
    let localQuickFilterText = '';
    let remoteQuickFilterText = '';
    let lastLocalClickedIdx = -1;
    let lastRemoteClickedIdx = -1;
    let activeDragPayload = null;
    let lastInteractiveMoveUndo = null;
    let interactiveClipboard = null;
    let interactiveMoveContext = null;
    let recentLocalMoveDirs = [];
    let recentRemoteMoveDirs = [];
    let interactiveToastTimer = null;

    function getLocalParentDir(path) {
      if (!path) return 'C:\\';
      const cleaned = path.replace(/[\\/]+$/, '');
      const lastSep = Math.max(cleaned.lastIndexOf('\\'), cleaned.lastIndexOf('/'));
      if (lastSep <= 2) {
        return cleaned.substring(0, 2) + '\\';
      }
      return cleaned.substring(0, lastSep);
    }

    function getRemoteParentDir(path) {
      if (!path || path === '/') return '/';
      const cleaned = path.replace(/\/+$/, '');
      const lastSep = cleaned.lastIndexOf('/');
      if (lastSep <= 0) return '/';
      return cleaned.substring(0, lastSep);
    }

    let localSortState = { col: 'name', asc: true };
    let remoteSortState = { col: 'name', asc: true };
    const calculatedFolderSizes = new Map();

    function getHumanFileType(fileName, isDir) {
      if (isDir) return 'Directory';
      const n = (fileName || '').toLowerCase();
      if (n.startsWith('.') && !n.slice(1).includes('.')) return 'Dotfile Config';
      const ext = n.includes('.') ? n.split('.').pop() : '';
      const map = {
        php: 'PHP Script', js: 'JavaScript', ts: 'TypeScript', json: 'JSON Data',
        html: 'HTML Doc', htm: 'HTML Doc', css: 'Stylesheet', scss: 'SASS Style',
        py: 'Python Script', rs: 'Rust Source', go: 'Go Source', sh: 'Shell Script',
        ps1: 'PowerShell', sql: 'SQL Database', env: 'Env Config', yml: 'YAML Config',
        yaml: 'YAML Config', toml: 'TOML Config', xml: 'XML Data', md: 'Markdown',
        txt: 'Text Document', log: 'Log File', ini: 'Config File', conf: 'Config File',
        zip: 'ZIP Archive', gz: 'GZ Archive', tgz: 'TAR.GZ Archive', tar: 'TAR Archive',
        rar: 'RAR Archive', '7z': '7-Zip Archive', pdf: 'PDF Document',
        png: 'PNG Image', jpg: 'JPEG Image', jpeg: 'JPEG Image', gif: 'GIF Image',
        svg: 'Vector SVG', webp: 'WebP Image', ico: 'Icon File',
        mp4: 'MP4 Video', mp3: 'MP3 Audio', exe: 'Executable', dll: 'System Library'
      };
      return map[ext] || (ext ? `${ext.toUpperCase()} File` : 'File');
    }

    function applyPaneSort(list, isRemote) {
      if (!Array.isArray(list) || list.length <= 1) return list;
      const st = isRemote ? remoteSortState : localSortState;
      const dirMult = st.asc ? 1 : -1;
      list.sort((a, b) => {
        if (!!a.is_dir !== !!b.is_dir) {
          return a.is_dir ? -1 : 1;
        }
        if (st.col === 'size') {
          const sa = a.is_dir ? (calculatedFolderSizes.get(a.path) ?? 0) : (Number(a.size) || 0);
          const sb = b.is_dir ? (calculatedFolderSizes.get(b.path) ?? 0) : (Number(b.size) || 0);
          if (sa !== sb) return (sa - sb) * dirMult;
        } else if (st.col === 'modified') {
          const ma = (a.modified || '').toString();
          const mb = (b.modified || '').toString();
          const cmp = ma.localeCompare(mb);
          if (cmp !== 0) return cmp * dirMult;
        } else if (st.col === 'type') {
          const ta = getHumanFileType(a.name, a.is_dir);
          const tb = getHumanFileType(b.name, b.is_dir);
          const cmp = ta.localeCompare(tb);
          if (cmp !== 0) return cmp * dirMult;
        } else if (st.col === 'permissions') {
          const pa = (a.permissions || '').toString();
          const pb = (b.permissions || '').toString();
          const cmp = pa.localeCompare(pb);
          if (cmp !== 0) return cmp * dirMult;
        }
        return (a.name || '').toString().localeCompare((b.name || '').toString(), undefined, { numeric: true, sensitivity: 'base' }) * dirMult;
      });
      return list;
    }

    function updateSortHeaderIndicators(isRemote) {
      const st = isRemote ? remoteSortState : localSortState;
      const prefix = isRemote ? 'remoteSort_' : 'localSort_';
      const cols = isRemote ? ['name', 'size', 'modified', 'permissions'] : ['name', 'size', 'modified', 'type'];
      cols.forEach(c => {
        const el = document.getElementById(prefix + c);
        if (el) {
          el.textContent = (st.col === c) ? (st.asc ? '▲' : '▼') : '';
        }
      });
    }

    function sortFilePane(isRemote, col) {
      const st = isRemote ? remoteSortState : localSortState;
      if (st.col === col) {
        st.asc = !st.asc;
      } else {
        st.col = col;
        st.asc = (col === 'size' || col === 'modified') ? false : true;
      }
      updateSortHeaderIndicators(isRemote);
      if (isRemote) {
        renderRemoteFiles(remoteFiles);
      } else {
        renderLocalFiles(localFiles);
      }
    }

    function updateCommanderStatusFooter() {
      const paneBadge = document.getElementById('fkeyActivePaneBadge');
      const statusText = document.getElementById('fkeySelectionStatus');
      const leftPane = document.getElementById('leftPane');
      const rightPane = document.getElementById('rightPane');
      const isRem = !!lastActivePaneIsRemote;

      if (leftPane) leftPane.classList.toggle('pane-focused', !isRem);
      if (rightPane) rightPane.classList.toggle('pane-focused', isRem);

      if (paneBadge) {
        paneBadge.textContent = isRem ? 'REMOTE PANE' : 'LOCAL PANE';
        paneBadge.style.color = isRem ? '#22c55e' : '#38bdf8';
        paneBadge.style.borderColor = isRem ? 'rgba(34, 197, 94, 0.35)' : 'rgba(56, 189, 248, 0.35)';
        paneBadge.style.background = isRem ? 'rgba(34, 197, 94, 0.14)' : 'rgba(56, 189, 248, 0.14)';
      }

      if (statusText) {
        const selItems = isRem ? getSelectedRemoteItems() : getSelectedLocalItems();
        const allItems = isRem ? (remoteFiles || []) : (localFiles || []);
        if (selItems.length > 0) {
          const totalBytes = selItems.reduce((acc, it) => acc + (it.is_dir ? (calculatedFolderSizes.get(it.path) || 0) : (Number(it.size) || 0)), 0);
          const dirsCount = selItems.filter(i => i.is_dir).length;
          const filesCount = selItems.length - dirsCount;
          statusText.innerHTML = `<strong style="color: var(--text-main);">${selItems.length} selected</strong> (<span style="color: #38bdf8; font-weight: 600;">${filesCount} file${filesCount === 1 ? '' : 's'}</span>, <span style="color: #fbbf24; font-weight: 600;">📁 ${dirsCount} folder${dirsCount === 1 ? '' : 's'}</span> — <span style="color: var(--accent); font-family: monospace;">${formatBytes(totalBytes)}</span>)`;
        } else {
          const totalDirBytes = allItems.reduce((acc, it) => acc + (it.is_dir ? 0 : (Number(it.size) || 0)), 0);
          statusText.innerHTML = `${allItems.length} item${allItems.length === 1 ? '' : 's'} in current folder (${formatBytes(totalDirBytes)}) — <span style="opacity:0.8;">Use ↑↓ Enter Backspace Tab F2–F8</span>`;
        }
      }
    }

    function updatePaneStatsBadge(isRemote) {
      const badge = document.getElementById(isRemote ? 'remoteStatsBadge' : 'localStatsBadge');
      if (!badge) return;
      const allList = isRemote ? (remoteFiles || []) : (localFiles || []);
      const filterVal = (isRemote ? remoteQuickFilterText : localQuickFilterText).trim().toLowerCase();
      const visibleCount = filterVal
        ? allList.filter(f => (f.name || '').toLowerCase().includes(filterVal)).length
        : allList.length;
      const selSelector = isRemote ? '#remoteFileList tr.selected:not(.parent-dir-row)' : '#localFileList tr.selected:not(.parent-dir-row)';
      const selRows = document.querySelectorAll(selSelector);
      const selCount = selRows.length;

      if (selCount > 0) {
        let selDirs = 0;
        let selFiles = 0;
        let selBytes = 0;
        selRows.forEach(r => {
          const idx = parseInt(r.getAttribute('data-idx'));
          const item = allList[idx];
          if (item) {
            if (item.is_dir) {
              selDirs++;
            } else {
              selFiles++;
              selBytes += (Number(item.size) || 0);
            }
          }
        });

        if (selDirs > 0 && selFiles > 0) {
          badge.innerHTML = `<span style="color:#fbbf24; font-weight:700;">📁 ${selDirs} dir${selDirs===1?'':'s'}</span> + <span style="color:#38bdf8; font-weight:700;">📄 ${selFiles} file${selFiles===1?'':'s'}</span> <span style="opacity:0.75; font-size: 10px;">(${selCount}/${allList.length})</span>`;
        } else if (selDirs > 0) {
          badge.innerHTML = `<span style="color:#fbbf24; font-weight:700;">📁 ${selDirs} folder${selDirs===1?'':'s'}</span> <span style="opacity:0.75; font-size: 10px;">(${selCount}/${allList.length})</span>`;
        } else {
          badge.innerHTML = `<span style="color:#38bdf8; font-weight:700;">📄 ${selFiles} file${selFiles===1?'':'s'}</span> <span style="opacity:0.75; font-size: 10px;">(${selCount}/${allList.length})</span>`;
        }
        badge.title = `${selCount} selected (${selFiles} files, ${selDirs} folders — ${formatBytes(selBytes)}). Click to cycle selection (All / Files / Folders / Clear)`;
        badge.classList.add('has-selection');
      } else if (filterVal) {
        badge.textContent = `${visibleCount} / ${allList.length} items`;
        badge.title = '';
        badge.classList.remove('has-selection');
      } else {
        badge.textContent = `${allList.length} item${allList.length === 1 ? '' : 's'}`;
        badge.title = '';
        badge.classList.remove('has-selection');
      }
      updateCommanderStatusFooter();
    }

    function cyclePaneSelection(isRemote) {
      const selSelector = isRemote ? '#remoteFileList tr.selected:not(.parent-dir-row)' : '#localFileList tr.selected:not(.parent-dir-row)';
      const selRows = document.querySelectorAll(selSelector);
      const allList = isRemote ? (remoteFiles || []) : (localFiles || []);
      let hasDirs = false;
      let hasFiles = false;
      selRows.forEach(r => {
        const idx = parseInt(r.getAttribute('data-idx'));
        const item = allList[idx];
        if (item && item.is_dir) hasDirs = true;
        if (item && !item.is_dir) hasFiles = true;
      });

      if (selRows.length === 0) {
        selectAllItemsInPane(isRemote, 'all');
      } else if (hasDirs && hasFiles) {
        selectAllItemsInPane(isRemote, 'files');
      } else if (hasFiles && !hasDirs) {
        selectAllItemsInPane(isRemote, 'folders');
      } else {
        const rows = document.querySelectorAll(isRemote ? '#remoteFileList tr' : '#localFileList tr');
        rows.forEach(r => r.classList.remove('selected'));
        if (isRemote) selectedRemoteFile = null;
        else selectedLocalFile = null;
        updatePaneStatsBadge(isRemote);
      }
    }

    function onQuickFilterChange(isRemote, val) {
      const clean = (val || '').toString();
      const clearBtn = document.getElementById(isRemote ? 'remoteQuickFilterClear' : 'localQuickFilterClear');
      if (clearBtn) {
        clearBtn.style.display = clean.length > 0 ? 'inline-block' : 'none';
      }
      if (isRemote) {
        remoteQuickFilterText = clean;
        renderRemoteFiles(remoteFiles);
      } else {
        localQuickFilterText = clean;
        renderLocalFiles(localFiles);
      }
    }

    function clearQuickFilter(isRemote) {
      const inp = document.getElementById(isRemote ? 'remoteQuickFilter' : 'localQuickFilter');
      if (inp) inp.value = '';
      onQuickFilterChange(isRemote, '');
    }

    function attachInteractiveRowDropTarget(rowEl, isRemoteTarget, targetDirPath, targetDisplayName, isParentRow = false) {
      rowEl.addEventListener('dragover', (e) => {
        e.preventDefault();
        e.stopPropagation();

        const targetScope = isRemoteTarget ? 'remote' : 'local';
        const payload = activeDragPayload;
        if (payload && Array.isArray(payload.items)) {
          // Prevent dropping a folder onto itself
          if (payload.source === targetScope && payload.items.some(it => it.path === targetDirPath)) {
            e.dataTransfer.dropEffect = 'none';
            return;
          }
        }

        const isCopyHold = e.ctrlKey || e.altKey;
        const isSamePane = payload && payload.source === targetScope;
        e.dataTransfer.dropEffect = (isSamePane && !isCopyHold) ? 'move' : 'copy';

        rowEl.classList.add(isParentRow ? 'row-drop-target-parent' : 'row-drop-target-folder');

        const cell = rowEl.querySelector('.file-cell');
        if (cell) {
          let pill = cell.querySelector('.drop-hint-pill');
          if (!pill) {
            pill = document.createElement('span');
            pill.className = 'drop-hint-pill';
            cell.appendChild(pill);
          }
          const count = (payload && Array.isArray(payload.items) && payload.items.length > 0) ? payload.items.length : 1;
          const itemLabel = count === 1 ? (payload?.items?.[0]?.name || '1 item') : `${count} items`;
          if (isSamePane || !payload) {
            if (isParentRow) {
              pill.className = 'drop-hint-pill';
              pill.textContent = `${isCopyHold ? 'Copy' : '⤴ Move'} ${itemLabel} to .. (${targetDisplayName})`;
            } else {
              pill.className = 'drop-hint-pill folder-pill';
              pill.textContent = `${isCopyHold ? 'Copy' : 'Move'} ${itemLabel} into "${targetDisplayName}"`;
            }
          } else {
            pill.className = 'drop-hint-pill folder-pill';
            pill.textContent = `${isRemoteTarget ? 'Upload' : 'Download'} ${itemLabel} into "${targetDisplayName}"`;
          }
        }
      });

      rowEl.addEventListener('dragleave', (e) => {
        if (!rowEl.contains(e.relatedTarget)) {
          rowEl.classList.remove('row-drop-target-parent', 'row-drop-target-folder');
          const pill = rowEl.querySelector('.drop-hint-pill');
          if (pill) pill.remove();
        }
      });

      rowEl.addEventListener('drop', async (e) => {
        e.preventDefault();
        e.stopPropagation();

        rowEl.classList.remove('row-drop-target-parent', 'row-drop-target-folder');
        const pill = rowEl.querySelector('.drop-hint-pill');
        if (pill) pill.remove();

        const leftPane = document.getElementById('leftPane');
        const rightPane = document.getElementById('rightPane');
        if (leftPane) leftPane.classList.remove('drag-target');
        if (rightPane) rightPane.classList.remove('drag-target');

        let data = activeDragPayload;
        if (!data) {
          const text = e.dataTransfer.getData('text/plain');
          if (text) {
            try { data = JSON.parse(text); } catch (_) {}
          }
        }
        if (!data) return;

        const items = Array.isArray(data.items) && data.items.length > 0
          ? data.items
          : [{ path: data.path, name: data.name, is_dir: data.is_dir }];

        const targetScope = isRemoteTarget ? 'remote' : 'local';
        const isCopyHold = e.ctrlKey || e.altKey;

        if (data.source === targetScope) {
          // Same-pane Interactive Move or Copy into `..` or subfolder
          await executeInteractiveMove(isRemoteTarget, items, targetDirPath, isCopyHold, false);
        } else {
          // Cross-pane targeted transfer directly into the hovered subfolder or `..` parent folder!
          await executeTargetedCrossPaneTransfer(items, isRemoteTarget, targetDirPath);
        }
      });
    }

    // Local Directory Listing
    function renderLocalFiles(files) {
      localFiles = applyPaneSort([...(files || [])], false);
      selectedLocalFile = null;
      lastLocalClickedIdx = -1;
      const activeTab = getActiveLocalTab();
      if (activeTab) {
        activeTab.files = [...localFiles];
        activeTab.path = currentLocalPath || activeTab.path;
        activeTab.title = getLocalTabDisplayTitle(activeTab.path);
      }
      const tbody = document.getElementById('localFileList');
      if (!tbody) return;
      tbody.innerHTML = '';
      const frag = document.createDocumentFragment();

      const canGoUp = !isLocalRoot(currentLocalPath);
      if (canGoUp) {
        const parentDirPath = getLocalParentDir(currentLocalPath);
        const parentBaseName = parentDirPath.split(/[\\/]/).filter(Boolean).pop() || parentDirPath;
        const trUp = document.createElement('tr');
        trUp.className = 'parent-dir-row';
        trUp.title = `Parent Directory: ${parentDirPath} (Double-click to go up, or Drag & Drop files here to move up)`;
        trUp.ondblclick = (e) => {
          e.preventDefault();
          localNavigateUp();
        };
        trUp.onclick = () => {
          const rows = document.querySelectorAll('#localFileList tr');
          rows.forEach(r => r.classList.remove('selected'));
          trUp.classList.add('selected');
          selectedLocalFile = null;
          updatePaneStatsBadge(false);
        };

        const { svg, iconClass } = getFileIcon('..', true);
        trUp.innerHTML = `
          <td>
            <div class="file-cell">
              <span class="file-icon ${iconClass}">${svg}</span>
              <span class="parent-dir-dots">..</span>
              <span style="font-size: 10.5px; color: var(--text-dim); margin-left: 6px;">(${escapeHtml(parentBaseName)})</span>
            </div>
          </td>
          <td class="mono-cell">-</td>
          <td class="mono-cell">-</td>
          <td class="mono-cell">Parent Directory</td>
        `;
        attachInteractiveRowDropTarget(trUp, false, parentDirPath, parentBaseName, true);
        frag.appendChild(trUp);
      }

      const filterLower = (localQuickFilterText || '').trim().toLowerCase();
      let renderedCount = 0;

      if (localFiles.length === 0) {
        const emptyTr = document.createElement('tr');
        emptyTr.innerHTML = `<td colspan="4" style="text-align:center; color: var(--text-dim); padding: ${canGoUp ? '16px' : '30px'} 10px; font-style: italic;">Folder is empty</td>`;
        frag.appendChild(emptyTr);
        tbody.appendChild(frag);
        updatePaneStatsBadge(false);
        updateFsWatcher();
        return;
      }

      localFiles.forEach((f, idx) => {
        try {
          const fileName = (f.name || '').toString();
          if (!showHiddenFilesState && fileName.startsWith('.')) {
            return;
          }
          if (filterLower && !fileName.toLowerCase().includes(filterLower)) {
            return;
          }
          renderedCount++;

          const tr = document.createElement('tr');
          tr.draggable = true;
          tr.setAttribute('data-idx', idx);
          tr.setAttribute('data-path', f.path || fileName);
          tr.setAttribute('data-is-dir', f.is_dir ? '1' : '0');
          tr.classList.add(f.is_dir ? 'row-folder' : 'row-file');

          if (interactiveClipboard && !interactiveClipboard.isRemote && interactiveClipboard.mode === 'cut') {
            if (interactiveClipboard.items.some(it => it.path === f.path)) {
              tr.classList.add('row-cut-pending');
            }
          }

          tr.ondragstart = (e) => {
            if (!tr.classList.contains('selected')) {
              selectLocalRow(idx);
            }
            const selectedBatch = getSelectedLocalItems();
            const batchItems = selectedBatch.length > 0 ? selectedBatch : [f];
            activeDragPayload = {
              source: 'local',
              path: f.path,
              name: f.name,
              is_dir: f.is_dir,
              items: batchItems.map(it => ({ path: it.path, name: it.name, is_dir: !!it.is_dir }))
            };
            e.dataTransfer.effectAllowed = 'copyMove';
            e.dataTransfer.setData('text/plain', JSON.stringify(activeDragPayload));
            tr.classList.add('dragging');
          };
          tr.ondragend = () => {
            tr.classList.remove('dragging');
            activeDragPayload = null;
          };

          if (f.is_dir) {
            attachInteractiveRowDropTarget(tr, false, f.path, fileName, false);
          }

          tr.onclick = (e) => {
            if (e.shiftKey && lastLocalClickedIdx >= 0) {
              const start = Math.min(lastLocalClickedIdx, idx);
              const end = Math.max(lastLocalClickedIdx, idx);
              const rows = document.querySelectorAll('#localFileList tr:not(.parent-dir-row)');
              rows.forEach(r => {
                const rIdx = parseInt(r.getAttribute('data-idx'));
                if (!isNaN(rIdx)) {
                  r.classList.toggle('selected', rIdx >= start && rIdx <= end);
                }
              });
              selectedLocalFile = f;
              updatePaneStatsBadge(false);
            } else if (e.ctrlKey || e.metaKey) {
              const isNowSelected = tr.classList.toggle('selected');
              if (isNowSelected) {
                selectedLocalFile = f;
              } else {
                const remaining = document.querySelectorAll('#localFileList tr.selected:not(.parent-dir-row)');
                if (remaining.length > 0) {
                  const firstIdx = parseInt(remaining[remaining.length - 1].getAttribute('data-idx'));
                  selectedLocalFile = !isNaN(firstIdx) ? (localFiles[firstIdx] || null) : null;
                } else {
                  selectedLocalFile = null;
                }
              }
              lastLocalClickedIdx = idx;
              updatePaneStatsBadge(false);
            } else {
              selectLocalRow(idx);
              lastLocalClickedIdx = idx;
            }
          };
          tr.ondblclick = () => openLocalItem(f, tr);
          if (f.is_dir) {
            tr.onauxclick = (e) => {
              if (e.button === 1) {
                e.preventDefault();
                e.stopPropagation();
                openFolderInNewTab(false, f.path);
              }
            };
          }
          tr.oncontextmenu = (e) => {
            e.preventDefault();
            e.stopPropagation();
            if (!tr.classList.contains('selected')) {
              selectLocalRow(idx);
            }
            openContextMenu(e, 'local', f, idx);
          };

          const { svg, iconClass } = getFileIcon(fileName, !!f.is_dir);

          let compareBadgeHtml = '';
          if (typeof compareDirsActive !== 'undefined' && compareDirsActive) {
            const cmp = getComparisonStatus(f, false);
            if (cmp && cmp.tag) {
              compareBadgeHtml = `<span class="compare-tag" style="color:${cmp.color}; background:${cmp.bg}; border:1px solid ${cmp.border};">${cmp.tag}</span>`;
            }
            if (cmp && cmp.rowClass) {
              tr.classList.add(cmp.rowClass);
            }
          }

          const dirSizeVal = f.is_dir ? calculatedFolderSizes.get(f.path) : null;
          const sizeCellText = f.is_dir
            ? (typeof dirSizeVal === 'number' ? `<span style="color: #38bdf8; font-weight: 600;">${formatBytes(dirSizeVal)}</span>` : '-')
            : formatBytes(f.size || 0);

          tr.innerHTML = `
            <td>
              <div class="file-cell">
                <span class="file-icon ${iconClass}">${svg}</span>
                <span>${escapeHtml(fileName)}</span>
                ${compareBadgeHtml}
              </div>
            </td>
            <td class="mono-cell">${sizeCellText}</td>
            <td class="mono-cell">${f.modified || '-'}</td>
            <td class="mono-cell">${getHumanFileType(fileName, !!f.is_dir)}</td>
          `;
          frag.appendChild(tr);
        } catch (itemErr) {
          console.error('Error rendering file item:', f, itemErr);
        }
      });

      if (renderedCount === 0 && filterLower) {
        const noMatchTr = document.createElement('tr');
        noMatchTr.innerHTML = `<td colspan="4" style="text-align:center; color: var(--text-dim); padding: 20px 10px; font-style: italic;">No items match filter "${escapeHtml(localQuickFilterText)}"</td>`;
        frag.appendChild(noMatchTr);
      }

      tbody.appendChild(frag);
      updatePaneStatsBadge(false);
      updateFsWatcher();
    }

    function selectLocalRow(idx) {
      const rows = document.querySelectorAll('#localFileList tr:not(.parent-dir-row)');
      rows.forEach((r) => {
        const rIdx = parseInt(r.getAttribute('data-idx'));
        r.classList.toggle('selected', rIdx === idx);
      });
      const upRow = document.querySelector('#localFileList tr.parent-dir-row');
      if (upRow) upRow.classList.remove('selected');
      selectedLocalFile = localFiles[idx];
      updatePaneStatsBadge(false);
    }

    async function openLocalItem(item, rowEl = null) {
      if (rowEl) {
        rowEl.classList.add('item-loading');
        const cell = rowEl.querySelector('.file-cell');
        if (cell && !cell.querySelector('.file-opening-spinner')) {
          const sp = document.createElement('span');
          sp.className = 'file-opening-spinner';
          cell.appendChild(sp);
        }
      }
      try {
        if (item.is_dir) {
          await navigateLocal(item.path);
        } else {
          if (appOptions && appOptions.dblLocal === 'transfer') {
            await uploadFileDirect(item.path, item.name);
          } else {
            await openLocalFileInEditor(item.path);
          }
        }
      } finally {
        if (rowEl) {
          rowEl.classList.remove('item-loading');
          const sp = rowEl.querySelector('.file-opening-spinner');
          if (sp) sp.remove();
        }
      }
    }

    // ============================================================================
    // Multi-Tab Commander Engine (Local & Remote Tabs, Back/Forward, Bookmarks)
    // ============================================================================
    let localTabCounter = 1;
    let remoteTabCounter = 1;
    let activeLocalTabId = 'ltab_1';
    let activeRemoteTabId = 'rtab_1';
    let isHistoryNavigating = false;
    let recentRemoteHistoryList = [];

    let localTabs = [
      {
        id: 'ltab_1',
        title: 'Desktop',
        path: '',
        files: [],
        historyBack: [],
        historyForward: [],
        filter: ''
      }
    ];

    let remoteTabs = [
      {
        id: 'rtab_1',
        title: 'Remote Server',
        isConnected: false,
        currentProfile: null,
        lastProfile: null,
        selectedSite: null,
        path: '',
        files: [],
        listError: null,
        historyBack: [],
        historyForward: [],
        filter: ''
      }
    ];

    function getActiveLocalTab() {
      return localTabs.find(t => t.id === activeLocalTabId) || localTabs[0];
    }

    function getActiveRemoteTab() {
      return remoteTabs.find(t => t.id === activeRemoteTabId) || remoteTabs[0];
    }

    function isDesktopPath(path) {
      if (!path) return false;
      const norm = path.replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase();
      const deskNorm = (globalDesktopDir || '').replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase();
      if (deskNorm && norm === deskNorm) return true;
      return norm.endsWith('\\desktop');
    }

    function getLocalTabDisplayTitle(path) {
      if (!path) return 'Desktop';
      if (isDesktopPath(path)) return 'Desktop';
      const cleaned = path.replace(/\//g, '\\').replace(/\\+$/, '');
      if (/^[a-zA-Z]:$/.test(cleaned)) {
        const drv = (globalDrivesInfo || []).find(d => d.letter.toUpperCase() === cleaned.toUpperCase());
        return drv ? drv.display : cleaned + '\\';
      }
      const parts = cleaned.split('\\').filter(Boolean);
      return parts.length > 0 ? parts[parts.length - 1] : cleaned;
    }

    function getRemoteTabDisplayTitle(tab) {
      if (!tab) return 'Remote Server';
      const prof = tab.currentProfile || tab.lastProfile;
      if (prof && prof.host) {
        const baseHost = prof.host;
        const sameHostCount = remoteTabs.filter(rt => {
          const p = rt.currentProfile || rt.lastProfile;
          return p && p.host === baseHost;
        }).length;
        if (sameHostCount > 1 && tab.path && tab.path !== '/') {
          const seg = tab.path.replace(/\/+$/, '').split('/').filter(Boolean).pop() || '/';
          return `${baseHost} (${seg})`;
        }
        return baseHost;
      }
      if (tab.selectedSite && tab.selectedSite.host) {
        return tab.selectedSite.host;
      }
      return tab.title || 'New Remote Tab';
    }

    function updateLocalAddressBarUI(path) {
      const inp = document.getElementById('localPathInput');
      const leadIcon = document.getElementById('localPathLeadIcon');
      const tab = getActiveLocalTab();
      const btnBack = document.getElementById('btnLocalBack');
      const btnFwd = document.getElementById('btnLocalForward');

      if (btnBack && tab) btnBack.disabled = !(tab.historyBack && tab.historyBack.length > 0);
      if (btnFwd && tab) btnFwd.disabled = !(tab.historyForward && tab.historyForward.length > 0);

      const isDesk = isDesktopPath(path);
      const cleaned = (path || '').replace(/\//g, '\\').replace(/\\+$/, '');
      const isDrv = /^[a-zA-Z]:$/.test(cleaned);

      if (leadIcon) {
        if (isDesk) {
          leadIcon.innerHTML = `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
            <rect x="2" y="3" width="16" height="11" rx="1.5" fill="#0284c7" stroke="#38bdf8" stroke-width="1.2"/>
            <line x1="6" y1="17" x2="14" y2="17" stroke="#38bdf8" stroke-width="1.4" stroke-linecap="round"/>
            <line x1="10" y1="14" x2="10" y2="17" stroke="#38bdf8" stroke-width="1.4"/>
          </svg>`;
        } else if (isDrv) {
          leadIcon.innerHTML = getTreeIconSvg('drive', cleaned.toUpperCase() === 'C:');
        } else {
          leadIcon.innerHTML = getTreeIconSvg('folder');
        }
      }

      if (inp) {
        inp.dataset.fullPath = path || '';
        if (document.activeElement === inp) {
          inp.value = path || '';
        } else {
          inp.value = isDesk ? 'Desktop' : (path || '');
        }
      }
    }

    function onLocalPathInputFocus(inp) {
      if (!inp) return;
      if (inp.dataset.fullPath) {
        inp.value = inp.dataset.fullPath;
      }
      setTimeout(() => {
        try { inp.select(); } catch (_) {}
      }, 10);
    }

    function onLocalPathInputBlur(inp) {
      if (!inp) return;
      const val = (inp.value || '').trim();
      if (!val || val.toLowerCase() === 'desktop' || val === inp.dataset.fullPath) {
        inp.value = isDesktopPath(currentLocalPath) ? 'Desktop' : (currentLocalPath || '');
      }
    }

    function updateRemoteAddressBarUI() {
      const tab = getActiveRemoteTab();
      const btnBack = document.getElementById('btnRemoteBack');
      const btnFwd = document.getElementById('btnRemoteForward');
      if (btnBack && tab) btnBack.disabled = !(tab.historyBack && tab.historyBack.length > 0);
      if (btnFwd && tab) btnFwd.disabled = !(tab.historyForward && tab.historyForward.length > 0);
    }

    function syncCurrentLocalTabState() {
      const tab = getActiveLocalTab();
      if (!tab) return;
      tab.path = currentLocalPath || '';
      tab.files = Array.isArray(localFiles) ? localFiles : [];
      tab.filter = localQuickFilterText || '';
      tab.title = getLocalTabDisplayTitle(tab.path);
      renderLocalTabsBar();
      updateLocalAddressBarUI(tab.path);
    }

    function syncCurrentRemoteTabState() {
      const tab = getActiveRemoteTab();
      if (!tab) return;
      tab.isConnected = !!isConnected;
      tab.currentProfile = currentConnectedProfile ? { ...currentConnectedProfile } : null;
      tab.lastProfile = lastConnectedProfile ? { ...lastConnectedProfile } : null;
      tab.selectedSite = selectedQuickSite ? { ...selectedQuickSite } : null;
      tab.path = currentRemotePath || '';
      tab.files = Array.isArray(remoteFiles) ? remoteFiles : [];
      tab.filter = remoteQuickFilterText || '';
      tab.title = getRemoteTabDisplayTitle(tab);
      renderRemoteTabsBar();
      updateRemoteAddressBarUI();
    }

    function renderLocalTabsBar() {
      const container = document.getElementById('localTabsContainer');
      if (!container) return;
      let html = '';
      localTabs.forEach(tab => {
        const isActive = tab.id === activeLocalTabId;
        const title = getLocalTabDisplayTitle(tab.path) || tab.title || 'Desktop';
        const tip = tab.path || title;
        html += `
          <div class="commander-tab local-tab ${isActive ? 'active' : ''}" onclick="switchLocalTab('${tab.id}')" title="${escapeHtml(tip)}">
            <span class="commander-tab-dot"></span>
            <span class="commander-tab-title">${escapeHtml(title)}</span>
            <span class="commander-tab-close" onclick="closeLocalTab('${tab.id}', event)" title="Close Tab">×</span>
          </div>
        `;
      });
      html += `<button type="button" class="commander-new-tab-btn" onclick="createNewLocalTab()" title="Open New Local Tab">+</button>`;
      container.innerHTML = html;
    }

    function renderRemoteTabsBar() {
      const container = document.getElementById('remoteTabsContainer');
      if (!container) return;
      let html = '';
      remoteTabs.forEach(tab => {
        const isActive = tab.id === activeRemoteTabId;
        const title = getRemoteTabDisplayTitle(tab);
        const prof = tab.currentProfile || tab.lastProfile || tab.selectedSite;
        const tip = prof && prof.host
          ? `${(prof.protocol || 'sftp').toUpperCase()}://${prof.username || ''}@${prof.host}:${prof.port || 22}${tab.path || '/'}`
          : title;
        const dotClass = tab.isConnected ? 'connected' : 'disconnected';
        html += `
          <div class="commander-tab remote-tab ${isActive ? 'active' : ''}" onclick="switchRemoteTab('${tab.id}')" title="${escapeHtml(tip)}">
            <span class="commander-tab-dot ${dotClass}"></span>
            <span class="commander-tab-title">${escapeHtml(title)}</span>
            <span class="commander-tab-close" onclick="closeRemoteTab('${tab.id}', event)" title="Close Remote Tab">×</span>
          </div>
        `;
      });
      html += `<button type="button" class="commander-new-tab-btn" onclick="createNewRemoteTab()" title="Open New Remote Tab (Connect Another Server or Folder)">+</button>`;
      container.innerHTML = html;
    }

    async function switchLocalTab(tabId) {
      closeAllCommanderPopups();
      const target = localTabs.find(t => t.id === tabId);
      if (!target) return;
      if (tabId !== activeLocalTabId) {
        syncCurrentLocalTabState();
        activeLocalTabId = tabId;
      }
      currentLocalPath = target.path || globalDesktopDir || 'C:\\';
      localQuickFilterText = target.filter || '';
      localFiles = Array.isArray(target.files) ? [...target.files] : [];

      const fInp = document.getElementById('localQuickFilter');
      const fClr = document.getElementById('localQuickFilterClear');
      if (fInp) fInp.value = localQuickFilterText;
      if (fClr) fClr.style.display = localQuickFilterText ? 'inline-block' : 'none';

      renderLocalTabsBar();
      updateLocalAddressBarUI(currentLocalPath);

      if (target.files && target.files.length > 0) {
        renderLocalFiles(target.files);
      } else {
        const tbody = document.getElementById('localFileList');
        if (tbody) {
          tbody.innerHTML = `<tr><td colspan="4" style="text-align:center; color: var(--text-dim); padding: 30px 10px; font-style: italic;">Loading directory...</td></tr>`;
        }
        await navigateLocal(currentLocalPath, true);
      }
    }

    async function createNewLocalTab(initialPath = null) {
      closeAllCommanderPopups();
      syncCurrentLocalTabState();
      localTabCounter++;
      const newId = 'ltab_' + localTabCounter;
      // Opening a new tab via '+' opens Desktop/Home freshly without inheriting previous tab's path
      const startPath = initialPath || globalDesktopDir || 'C:\\';
      const newTab = {
        id: newId,
        title: getLocalTabDisplayTitle(startPath),
        path: startPath,
        files: [],
        historyBack: [],
        historyForward: [],
        filter: ''
      };
      localTabs.push(newTab);
      activeLocalTabId = newId;
      currentLocalPath = startPath;
      localFiles = [];
      localQuickFilterText = '';

      const tbody = document.getElementById('localFileList');
      if (tbody) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align:center; color: var(--text-dim); padding: 30px 10px; font-style: italic;">Loading directory...</td></tr>`;
      }

      const fInp = document.getElementById('localQuickFilter');
      const fClr = document.getElementById('localQuickFilterClear');
      if (fInp) fInp.value = '';
      if (fClr) fClr.style.display = 'none';

      renderLocalTabsBar();
      updateLocalAddressBarUI(startPath);
      await navigateLocal(startPath, true);
    }

    async function closeLocalTab(tabId, e) {
      if (e) e.stopPropagation();
      if (localTabs.length <= 1) {
        const onlyTab = localTabs[0];
        onlyTab.historyBack = [];
        onlyTab.historyForward = [];
        await navigateLocal(globalDesktopDir || 'C:\\', true);
        return;
      }
      const idx = localTabs.findIndex(t => t.id === tabId);
      if (idx === -1) return;
      const wasActive = (tabId === activeLocalTabId);
      localTabs.splice(idx, 1);
      if (wasActive) {
        const nextTab = localTabs[Math.max(0, idx - 1)] || localTabs[0];
        activeLocalTabId = nextTab.id;
        await switchLocalTab(nextTab.id);
      } else {
        renderLocalTabsBar();
      }
    }

    function renderRemoteTabLauncherCard() {
      const tbody = document.getElementById('remoteFileList');
      if (!tbody) return;
      const otherConnectedTab = remoteTabs.find(t => t.id !== activeRemoteTabId && (t.isConnected || t.currentProfile || t.lastProfile));
      const cloneProfile = otherConnectedTab ? (otherConnectedTab.currentProfile || otherConnectedTab.lastProfile) : null;

      let sitesHtml = '';
      if (savedSites && savedSites.length > 0) {
        sitesHtml = savedSites.map(s => `
          <button type="button" class="btn btn-secondary" onclick="connectRemoteTabToSavedSite('${escapeJsString(s.id)}')" style="display: inline-flex; align-items: center; gap: 7px; padding: 6px 12px; height: auto; font-size: 12px; border-color: rgba(56, 189, 248, 0.3);">
            <span style="width: 7px; height: 7px; border-radius: 50%; background: #38bdf8;"></span>
            <span style="font-weight: 700;">${escapeHtml(s.name)}</span>
            <span style="color: var(--text-dim); font-family: var(--font-mono); font-size: 11px;">(${escapeHtml(s.host)})</span>
          </button>
        `).join('');
      }

      tbody.innerHTML = `
        <tr>
          <td colspan="4" style="text-align: center; color: var(--text-dim); padding: 38px 18px;">
            <div style="display: flex; flex-direction: column; align-items: center; gap: 12px; max-width: 560px; margin: 0 auto;">
              <div style="font-size: 13.5px; font-weight: 700; color: var(--text-main);">
                <svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg> Remote Tab Ready — Connect to a Server or Folder
              </div>
              <div style="font-size: 11.5px; color: var(--text-muted); line-height: 1.5;">
                Each remote tab maintains its own independent live server session and directory path. Select a saved server below or use the Quick Connect bar above.
              </div>
              ${cloneProfile ? `
                <div style="display: flex; gap: 8px; flex-wrap: wrap; justify-content: center;">
                  <button type="button" class="btn btn-primary" onclick="cloneServerIntoActiveRemoteTab()" style="padding: 6px 16px; height: auto; font-weight: 600;">
                    <svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="2" y1="10" x2="22" y2="10"/></svg> Open Folder on ${escapeHtml(cloneProfile.host)} (${escapeHtml(otherConnectedTab.path || '/')})
                  </button>
                  <button type="button" class="btn btn-secondary" onclick="cloneServerIntoActiveRemoteTab('/')" style="padding: 6px 14px; height: auto;">
                    Open Root (/)
                  </button>
                </div>
              ` : ''}
              ${sitesHtml ? `
                <div style="display: flex; flex-wrap: wrap; gap: 8px; justify-content: center; margin-top: 4px;">
                  ${sitesHtml}
                </div>
              ` : `
                <button type="button" class="btn btn-secondary" onclick="openSitesModal()" style="margin-top: 4px;">
                  Open Site Manager
                </button>
              `}
            </div>
          </td>
        </tr>
      `;
      updatePaneStatsBadge(true);
    }

    async function connectRemoteTabToSavedSite(siteId, customRemoteDir = null) {
      const site = (savedSites || []).find(s => s.id === siteId);
      if (!site) return;
      selectedQuickSite = site;
      const sel = document.getElementById('quickSiteSelect');
      if (sel) {
        sel.value = site.id;
        onQuickSiteSelect(site.id, false);
      }
      await executeConnect({
        site_id: site.id,
        protocol: site.protocol,
        host: site.host,
        port: site.port,
        username: site.username,
        password: site.password,
        auth_type: site.auth_type || 'password',
        key_path: site.key_path,
        passphrase: site.passphrase,
        remote_dir: customRemoteDir || site.remote_dir || '',
        site_name: site.name
      });
    }

    async function cloneServerIntoActiveRemoteTab(customPath = null) {
      const otherTab = remoteTabs.find(t => t.id !== activeRemoteTabId && (t.currentProfile || t.lastProfile));
      if (!otherTab) return;
      const baseProf = otherTab.currentProfile || otherTab.lastProfile;
      if (!baseProf) return;
      const targetDir = customPath || otherTab.path || baseProf.remote_dir || '/';
      await executeConnect({
        ...baseProf,
        remote_dir: targetDir,
        trust_host: true
      });
    }

    async function switchRemoteTab(tabId) {
      closeAllCommanderPopups();
      const target = remoteTabs.find(t => t.id === tabId);
      if (!target) return;

      if (tabId !== activeRemoteTabId) {
        syncCurrentRemoteTabState();
        activeRemoteTabId = tabId;
      }

      try {
        await sendRust('switch_remote_tab', {
          tab_id: tabId,
          path: target.path || ''
        });
      } catch (_) {}

      isConnected = !!target.isConnected;
      currentConnectedProfile = target.currentProfile ? { ...target.currentProfile } : null;
      lastConnectedProfile = target.lastProfile ? { ...target.lastProfile } : null;
      selectedQuickSite = target.selectedSite ? { ...target.selectedSite } : null;
      currentRemotePath = target.path || '';
      remoteFiles = Array.isArray(target.files) ? [...target.files] : [];
      remoteQuickFilterText = target.filter || '';

      const fInp = document.getElementById('remoteQuickFilter');
      const fClr = document.getElementById('remoteQuickFilterClear');
      if (fInp) fInp.value = remoteQuickFilterText;
      if (fClr) fClr.style.display = remoteQuickFilterText ? 'inline-block' : 'none';

      const rInp = document.getElementById('remotePathInput');
      if (rInp) {
        rInp.value = currentRemotePath || (isConnected ? '/' : 'Not connected');
      }

      // Sync Quick Connect dropdown visually without disconnecting
      const sel = document.getElementById('quickSiteSelect');
      const activeProf = currentConnectedProfile || lastConnectedProfile;
      if (sel) {
        if (activeProf && activeProf.site_id && savedSites.some(s => s.id === activeProf.site_id)) {
          sel.value = activeProf.site_id;
          onQuickSiteSelect(activeProf.site_id, false);
        } else if (selectedQuickSite && savedSites.some(s => s.id === selectedQuickSite.id)) {
          sel.value = selectedQuickSite.id;
          onQuickSiteSelect(selectedQuickSite.id, false);
        }
      }

      if (isConnected && activeProf) {
        const label = activeProf.site_name
          ? `${activeProf.site_name} (${activeProf.host})`
          : `${(activeProf.protocol || 'sftp').toUpperCase()}://${activeProf.host}`;
        setConnectedState(true, label);
        startKeepAliveTimer();
      } else {
        stopKeepAliveTimer();
        setConnectedState(false);
      }

      renderRemoteTabsBar();
      updateRemoteAddressBarUI();

      if (isConnected) {
        if (remoteFiles && remoteFiles.length > 0) {
          renderRemoteFiles(remoteFiles, target.listError || null);
        } else {
          const tbody = document.getElementById('remoteFileList');
          if (tbody) {
            tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 45px;">Loading remote directory...</td></tr>`;
          }
          await navigateRemote(currentRemotePath || '/', true);
        }
      } else {
        renderRemoteTabLauncherCard();
      }
    }

    async function createNewRemoteTab(options = {}) {
      closeAllCommanderPopups();
      syncCurrentRemoteTabState();
      remoteTabCounter++;
      const newId = 'rtab_' + remoteTabCounter;
      const newTab = {
        id: newId,
        title: options.title || 'New Remote Tab',
        isConnected: false,
        currentProfile: null,
        lastProfile: null,
        selectedSite: options.site || null,
        path: options.path || '',
        files: [],
        listError: null,
        historyBack: [],
        historyForward: [],
        filter: ''
      };
      remoteTabs.push(newTab);
      activeRemoteTabId = newId;

      // Cleanly reset active remote state for the new tab
      isConnected = false;
      currentConnectedProfile = null;
      lastConnectedProfile = null;
      selectedQuickSite = options.site || null;
      currentRemotePath = options.path || '';
      remoteFiles = [];
      remoteQuickFilterText = '';

      const fInp = document.getElementById('remoteQuickFilter');
      const fClr = document.getElementById('remoteQuickFilterClear');
      if (fInp) fInp.value = '';
      if (fClr) fClr.style.display = 'none';

      const rInp = document.getElementById('remotePathInput');
      if (rInp) {
        rInp.value = currentRemotePath || 'Not connected';
      }

      setConnectedState(false);
      stopKeepAliveTimer();

      try {
        await sendRust('switch_remote_tab', {
          tab_id: newId,
          path: options.path || ''
        });
      } catch (_) {}

      renderRemoteTabsBar();
      updateRemoteAddressBarUI();

      if (options.connectProfile) {
        const tbody = document.getElementById('remoteFileList');
        if (tbody) {
          tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 45px;">Connecting to ${escapeHtml(options.connectProfile.host)}...</td></tr>`;
        }
        await executeConnect({
          ...options.connectProfile,
          remote_dir: options.path || options.connectProfile.remote_dir || '/',
          trust_host: true
        });
      } else if (options.siteId) {
        await connectRemoteTabToSavedSite(options.siteId, options.path || null);
      } else {
        renderRemoteTabLauncherCard();
      }
    }

    async function closeRemoteTab(tabId, e) {
      if (e) e.stopPropagation();
      closeAllCommanderPopups();

      if (remoteTabs.length <= 1) {
        if (isConnected) {
          await handleDisconnect(true);
        } else {
          const onlyTab = remoteTabs[0];
          onlyTab.isConnected = false;
          onlyTab.currentProfile = null;
          onlyTab.lastProfile = null;
          onlyTab.path = '';
          onlyTab.files = [];
          onlyTab.historyBack = [];
          onlyTab.historyForward = [];
          renderRemoteTabsBar();
          renderRemoteTabLauncherCard();
        }
        return;
      }

      const idx = remoteTabs.findIndex(t => t.id === tabId);
      if (idx === -1) return;
      const wasActive = (tabId === activeRemoteTabId);
      remoteTabs.splice(idx, 1);

      try {
        await sendRust('close_remote_tab', { tab_id: tabId });
      } catch (_) {}

      if (wasActive) {
        const nextTab = remoteTabs[Math.max(0, idx - 1)] || remoteTabs[0];
        activeRemoteTabId = nextTab.id;
        await switchRemoteTab(nextTab.id);
      } else {
        renderRemoteTabsBar();
      }
    }

    async function openFolderInNewTab(isRemote, folderPath, folderName = '') {
      closeContextMenu();
      closeAllCommanderPopups();
      if (!folderPath) return;
      if (isRemote) {
        const baseProf = currentConnectedProfile || lastConnectedProfile || (selectedQuickSite ? {
          site_id: selectedQuickSite.id,
          protocol: selectedQuickSite.protocol,
          host: selectedQuickSite.host,
          port: selectedQuickSite.port,
          username: selectedQuickSite.username,
          password: selectedQuickSite.password,
          auth_type: selectedQuickSite.auth_type || 'password',
          key_path: selectedQuickSite.key_path,
          passphrase: selectedQuickSite.passphrase,
          remote_dir: folderPath,
          site_name: selectedQuickSite.name
        } : null);
        if (!baseProf) {
          alert('No active or saved profile to open this remote folder in a new tab.');
          return;
        }
        const displayName = folderName || folderPath.replace(/\/+$/, '').split('/').filter(Boolean).pop() || folderPath;
        await createNewRemoteTab({
          title: displayName,
          path: folderPath,
          connectProfile: { ...baseProf, remote_dir: folderPath }
        });
      } else {
        await createNewLocalTab(folderPath);
      }
    }

    // Back / Forward Navigation History
    async function localHistoryBack() {
      closeAllCommanderPopups();
      const tab = getActiveLocalTab();
      if (!tab || !tab.historyBack || tab.historyBack.length === 0) return;
      const prev = tab.historyBack.pop();
      if (currentLocalPath) {
        tab.historyForward.push(currentLocalPath);
      }
      isHistoryNavigating = true;
      try {
        await navigateLocal(prev, true);
      } finally {
        isHistoryNavigating = false;
        updateLocalAddressBarUI(currentLocalPath);
      }
    }

    async function localHistoryForward() {
      closeAllCommanderPopups();
      const tab = getActiveLocalTab();
      if (!tab || !tab.historyForward || tab.historyForward.length === 0) return;
      const next = tab.historyForward.pop();
      if (currentLocalPath) {
        tab.historyBack.push(currentLocalPath);
      }
      isHistoryNavigating = true;
      try {
        await navigateLocal(next, true);
      } finally {
        isHistoryNavigating = false;
        updateLocalAddressBarUI(currentLocalPath);
      }
    }

    async function remoteHistoryBack() {
      closeAllCommanderPopups();
      const tab = getActiveRemoteTab();
      if (!tab || !tab.historyBack || tab.historyBack.length === 0) return;
      const prev = tab.historyBack.pop();
      if (currentRemotePath) {
        tab.historyForward.push(currentRemotePath);
      }
      isHistoryNavigating = true;
      try {
        await navigateRemote(prev, true);
      } finally {
        isHistoryNavigating = false;
        updateRemoteAddressBarUI();
      }
    }

    async function remoteHistoryForward() {
      closeAllCommanderPopups();
      const tab = getActiveRemoteTab();
      if (!tab || !tab.historyForward || tab.historyForward.length === 0) return;
      const next = tab.historyForward.pop();
      if (currentRemotePath) {
        tab.historyBack.push(currentRemotePath);
      }
      isHistoryNavigating = true;
      try {
        await navigateRemote(next, true);
      } finally {
        isHistoryNavigating = false;
        updateRemoteAddressBarUI();
      }
    }

    function closeAllCommanderPopups() {
      closeLocalPathTree();
      const ids = ['remotePathTreePopup', 'localHistoryPopup', 'remoteHistoryPopup', 'localBookmarksPopup', 'remoteBookmarksPopup'];
      ids.forEach(id => {
        const el = document.getElementById(id);
        if (el) el.classList.remove('active');
      });
    }

    function toggleHistoryPopup(e, isRemote) {
      if (e) e.stopPropagation();
      const popupId = isRemote ? 'remoteHistoryPopup' : 'localHistoryPopup';
      const popup = document.getElementById(popupId);
      if (!popup) return;
      const wasActive = popup.classList.contains('active');
      closeAllCommanderPopups();
      if (wasActive) return;

      const tab = isRemote ? getActiveRemoteTab() : getActiveLocalTab();
      const backList = (tab && tab.historyBack) ? [...tab.historyBack].reverse() : [];
      const fwdList = (tab && tab.historyForward) ? [...tab.historyForward].reverse() : [];

      let html = `<div class="commander-popup-header">${isRemote ? 'Remote Tab History' : 'Local Tab History'}</div>`;
      if (fwdList.length === 0 && backList.length === 0) {
        html += `<div style="padding: 10px 12px; color: var(--text-dim); font-style: italic;">No folder history in this tab yet</div>`;
      } else {
        fwdList.slice(0, 6).forEach(p => {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); ${isRemote ? `navigateRemote('${escapeJsString(p)}')` : `navigateLocal('${escapeJsString(p)}')`}">
              <span style="overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">→ ${escapeHtml(p)}</span>
            </div>
          `;
        });
        if (isRemote ? currentRemotePath : currentLocalPath) {
          html += `
            <div class="commander-popup-item" style="font-weight: 700; color: var(--accent);">
              <span style="overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">● ${escapeHtml(isRemote ? currentRemotePath : currentLocalPath)}</span>
            </div>
          `;
        }
        backList.slice(0, 10).forEach(p => {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); ${isRemote ? `navigateRemote('${escapeJsString(p)}')` : `navigateLocal('${escapeJsString(p)}')`}">
              <span style="overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">← ${escapeHtml(p)}</span>
            </div>
          `;
        });
      }
      popup.innerHTML = html;
      popup.classList.add('active');
    }

    function getSavedBookmarks(isRemote) {
      try {
        const key = isRemote ? '90ftp_remote_bookmarks' : '90ftp_local_bookmarks';
        const raw = appStorage.getItem(key);
        return raw ? JSON.parse(raw) : [];
      } catch (_) {
        return [];
      }
    }

    function addCurrentFolderBookmark(isRemote) {
      const path = isRemote ? currentRemotePath : currentLocalPath;
      if (!path) return;
      const list = getSavedBookmarks(isRemote);
      if (!list.includes(path)) {
        list.unshift(path);
        try {
          const key = isRemote ? '90ftp_remote_bookmarks' : '90ftp_local_bookmarks';
          appStorage.setItem(key, JSON.stringify(list.slice(0, 25)));
        } catch (_) {}
        addLog(`Bookmarked ${isRemote ? 'remote' : 'local'} directory: ${path}`, 'success');
      }
      closeAllCommanderPopups();
    }

    function removeSavedBookmark(isRemote, path, e) {
      if (e) e.stopPropagation();
      const list = getSavedBookmarks(isRemote).filter(p => p !== path);
      try {
        const key = isRemote ? '90ftp_remote_bookmarks' : '90ftp_local_bookmarks';
        appStorage.setItem(key, JSON.stringify(list));
      } catch (_) {}
      toggleBookmarksPopup(null, isRemote);
    }

    function toggleBookmarksPopup(e, isRemote) {
      if (e) e.stopPropagation();
      const popupId = isRemote ? 'remoteBookmarksPopup' : 'localBookmarksPopup';
      const popup = document.getElementById(popupId);
      if (!popup) return;
      const wasActive = e ? popup.classList.contains('active') : false;
      closeAllCommanderPopups();
      if (wasActive) return;

      const curPath = isRemote ? currentRemotePath : currentLocalPath;
      const bookmarks = getSavedBookmarks(isRemote);

      let html = `
        <div class="commander-popup-header">${isRemote ? 'Remote Bookmarks & Tabs' : 'Local Bookmarks & Tabs'}</div>
        <div class="commander-popup-item" onclick="addCurrentFolderBookmark(${isRemote})">
          <span><svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg> Bookmark Current Directory</span>
        </div>
        <div class="commander-popup-item" onclick="openFolderInNewTab(${isRemote}, '${escapeJsString(curPath || (isRemote ? '/' : 'C:\\\\'))}')">
          <span><svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg> Duplicate Current Folder in New Tab</span>
        </div>
      `;

      if (bookmarks.length > 0) {
        html += `<div class="commander-popup-header" style="margin-top: 4px;">Saved Bookmarks</div>`;
        bookmarks.forEach(b => {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); ${isRemote ? `navigateRemote('${escapeJsString(b)}')` : `navigateLocal('${escapeJsString(b)}')`}">
              <span style="overflow:hidden; text-overflow:ellipsis; white-space:nowrap;"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/></svg> ${escapeHtml(b)}</span>
              <span onclick="removeSavedBookmark(${isRemote}, '${escapeJsString(b)}', event)" style="opacity:0.65; padding: 0 4px;" title="Remove Bookmark">✕</span>
            </div>
          `;
        });
      }

      if (isRemote) {
        if (savedSites && savedSites.length > 0) {
          html += `<div class="commander-popup-header" style="margin-top: 4px;">Open Server in New Tab</div>`;
          savedSites.forEach(s => {
            html += `
              <div class="commander-popup-item" onclick="closeAllCommanderPopups(); createNewRemoteTab({ siteId: '${escapeJsString(s.id)}', title: '${escapeJsString(s.host)}' })">
                <span style="overflow:hidden; text-overflow:ellipsis; white-space:nowrap;"><svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> ${escapeHtml(s.name)} (${escapeHtml(s.host)})</span>
                <span style="font-size: 10px; opacity: 0.7;">New Tab</span>
              </div>
            `;
          });
        }
        html += `<div class="commander-popup-header" style="margin-top: 4px;">Quick Server Paths</div>`;
        ['/', '/home', '/var/www', '/etc', '/tmp'].forEach(qp => {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); navigateRemote('${qp}')">
              <span><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg> ${qp}</span>
            </div>
          `;
        });
      } else {
        html += `<div class="commander-popup-header" style="margin-top: 4px;">Quick Local Locations</div>`;
        if (globalDesktopDir) {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); navigateLocal('${escapeJsString(globalDesktopDir)}')">
              <span><svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg> Desktop</span>
            </div>
          `;
        }
        (globalDrivesInfo || []).forEach(drv => {
          html += `
            <div class="commander-popup-item" onclick="closeAllCommanderPopups(); navigateLocal('${escapeJsString(drv.path)}')">
              <span><svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="3"/></svg> ${escapeHtml(drv.display)}</span>
            </div>
          `;
        });
      }

      popup.innerHTML = html;
      popup.classList.add('active');
    }

    function toggleRemotePathTree(e) {
      if (e) e.stopPropagation();
      const popup = document.getElementById('remotePathTreePopup');
      if (!popup) return;
      const wasOpen = popup.classList.contains('active');
      closeAllCommanderPopups();
      if (!wasOpen) {
        renderRemotePathTree();
        popup.classList.add('active');
      }
    }

    function renderRemotePathTree() {
      const popup = document.getElementById('remotePathTreePopup');
      if (!popup) return;
      const norm = (currentRemotePath || '/').replace(/\/+$/, '') || '/';
      const parts = norm === '/' ? [] : norm.split('/').filter(Boolean);

      let html = `
        <div class="path-tree-item ${norm === '/' ? 'active' : ''}" style="padding-left: 10px;" onclick="closeAllCommanderPopups(); navigateRemote('/')">
          <span class="tree-icon">${getTreeIconSvg('folder')}</span>
          <span class="path-tree-label">/ (Root Directory)</span>
        </div>
      `;

      let acc = '';
      parts.forEach((seg, idx) => {
        acc += '/' + seg;
        const isAct = (idx === parts.length - 1);
        const indent = 10 + (idx + 1) * 16;
        html += `
          <div class="path-tree-item ${isAct ? 'active' : ''}" style="padding-left: ${indent}px;" onclick="closeAllCommanderPopups(); navigateRemote('${escapeJsString(acc)}')">
            <span class="tree-icon">${getTreeIconSvg('folder')}</span>
            <span class="path-tree-label">${escapeHtml(seg)}</span>
          </div>
        `;
      });

      const subDirs = (remoteFiles || []).filter(f => f && f.is_dir).slice(0, 15);
      if (subDirs.length > 0) {
        const subIndent = 10 + (parts.length + 1) * 16;
        subDirs.forEach(sd => {
          html += `
            <div class="path-tree-item" style="padding-left: ${subIndent}px; opacity: 0.9;" onclick="closeAllCommanderPopups(); navigateRemote('${escapeJsString(sd.path)}')">
              <span class="tree-icon">${getTreeIconSvg('folder')}</span>
              <span class="path-tree-label">${escapeHtml(sd.name)}</span>
            </div>
          `;
        });
      }

      if (recentRemoteHistoryList && recentRemoteHistoryList.length > 0) {
        html += `<div class="commander-popup-header" style="margin-top: 4px;">Recent Remote Directories</div>`;
        recentRemoteHistoryList.slice(0, 8).forEach(rp => {
          html += `
            <div class="path-tree-item" style="padding-left: 12px;" onclick="closeAllCommanderPopups(); navigateRemote('${escapeJsString(rp)}')">
              <span class="tree-icon"><svg class="icon icon-lg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg></span>
              <span class="path-tree-label">${escapeHtml(rp)}</span>
            </div>
          `;
        });
      }

      popup.innerHTML = html;
    }

    async function navigateLocal(path, skipHistory = false) {
      if (!path || path.trim().toLowerCase() === 'desktop') {
        path = globalDesktopDir || currentLocalPath || 'C:\\Users\\aljailane\\Desktop';
      }
      const prevLocal = (currentLocalPath || '').replace(/[\\/]+$/, '');
      try {
        const data = await sendRust('local_list', { path });
        const newResolved = (data.path || '').replace(/[\\/]+$/, '');
        if (!skipHistory && !isHistoryNavigating && prevLocal && newResolved && prevLocal.toLowerCase() !== newResolved.toLowerCase()) {
          const tab = getActiveLocalTab();
          if (tab) {
            tab.historyBack.push(currentLocalPath);
            tab.historyForward = [];
          }
        }
        currentLocalPath = data.path;
        const driveSelect = document.getElementById('driveSelect');
        if (driveSelect && currentLocalPath) {
          const dl = currentLocalPath.substring(0, 3).toUpperCase();
          for (let opt of driveSelect.options) {
            if (opt.value.toUpperCase() === dl) {
              driveSelect.value = opt.value;
              break;
            }
          }
        }
        renderLocalFiles(data.files);
        syncCurrentLocalTabState();
        const treePopup = document.getElementById('localPathTreePopup');
        if (treePopup && treePopup.classList.contains('active')) {
          renderLocalPathTree();
        }

        // Synchronized Browsing (Local -> Remote)
        if (appOptions && appOptions.syncBrowsing && !isSyncNavigating && prevLocal) {
          const newLocal = (currentLocalPath || '').replace(/[\\/]+$/, '');
          if (newLocal && newLocal.toLowerCase() !== prevLocal.toLowerCase() && (isConnected || lastConnectedProfile)) {
            isSyncNavigating = true;
            try {
              if (newLocal.toLowerCase().startsWith(prevLocal.toLowerCase() + '\\')) {
                const subName = newLocal.substring(prevLocal.length + 1);
                if (subName && !subName.includes('\\')) {
                  const matchRemote = (remoteFiles || []).find(f => f.is_dir && f.name.toLowerCase() === subName.toLowerCase());
                  if (matchRemote) {
                    addLog(`[Sync Browsing] Following folder '${subName}' on Remote Server`, 'info');
                    await navigateRemote(matchRemote.path);
                  }
                }
              } else if (prevLocal.toLowerCase().startsWith(newLocal.toLowerCase() + '\\')) {
                const remSlash = prevLocal.substring(newLocal.length + 1);
                if (remSlash && !remSlash.includes('\\') && !isRemoteRoot(currentRemotePath)) {
                  addLog(`[Sync Browsing] Navigating up on Remote Server`, 'info');
                  remoteNavigateUp();
                }
              }
            } finally {
              isSyncNavigating = false;
            }
          }
        }
      } catch (err) {
        addLog('Cannot open local folder: ' + err.message, 'error');
      }
    }

    function localNavigateUp() {
      if (!currentLocalPath) {
        navigateLocal('C:\\');
        return;
      }
      const parent = currentLocalPath.replace(/\\[^\\]+$/, '') || 'C:\\';
      navigateLocal(parent);
    }

    function refreshLocal() {
      navigateLocal(currentLocalPath || 'C:\\', true);
    }

    function onDriveChange() {
      const drive = document.getElementById('driveSelect').value;
      navigateLocal(drive);
    }

    // ========================================================
    // Hierarchical Local Path Tree Dropdown (FileZilla Style)
    // ========================================================
    let globalDrivesInfo = [];
    let globalDesktopDir = '';

    function toggleLocalPathTree(e) {
      if (e) {
        e.stopPropagation();
      }
      const popup = document.getElementById('localPathTreePopup');
      if (!popup) return;
      const isOpen = popup.classList.contains('active');
      closeAllCommanderPopups();
      if (!isOpen) {
        renderLocalPathTree();
        popup.classList.add('active');
      }
    }

    function closeLocalPathTree() {
      const popup = document.getElementById('localPathTreePopup');
      if (popup) popup.classList.remove('active');
    }

    window.addEventListener('click', (e) => {
      if (!e.target.closest('.path-combobox-wrapper') &&
          !e.target.closest('.local-path-tree-popup') &&
          !e.target.closest('.commander-dropdown-popup') &&
          !e.target.closest('.commander-nav-caret') &&
          !e.target.closest('.commander-bar-icon-btn')) {
        closeAllCommanderPopups();
      }
    });

    window.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        closeAllCommanderPopups();
      }
    });

    function getTreeIconSvg(type, isSystem = false) {
      if (type === 'desktop') {
        return `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
          <rect x="2" y="3" width="16" height="11" rx="1.5" stroke="#38bdf8" stroke-width="1.3" fill="rgba(56,189,248,0.2)"/>
          <line x1="6" y1="17" x2="14" y2="17" stroke="#38bdf8" stroke-width="1.4" stroke-linecap="round"/>
          <line x1="10" y1="14" x2="10" y2="17" stroke="#38bdf8" stroke-width="1.4"/>
        </svg>`;
      }
      if (type === 'thispc') {
        return `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
          <rect x="2" y="2.5" width="16" height="11.5" rx="1.5" fill="#0284c7" stroke="#38bdf8" stroke-width="1.2"/>
          <rect x="3.5" y="4" width="13" height="8.5" rx="0.8" fill="#0f172a"/>
          <path d="M7 17.5h6M10 14v3.5" stroke="#94a3b8" stroke-width="1.3" stroke-linecap="round"/>
        </svg>`;
      }
      if (type === 'drive') {
        if (isSystem) {
          return `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
            <rect x="1.5" y="1.5" width="3" height="3" fill="#0ea5e9"/>
            <rect x="5" y="1.5" width="3" height="3" fill="#0ea5e9"/>
            <rect x="1.5" y="5" width="3" height="3" fill="#0ea5e9"/>
            <rect x="5" y="5" width="3" height="3" fill="#0ea5e9"/>
            <rect x="2" y="6.5" width="16" height="8" rx="1.5" fill="#334155" stroke="#64748b" stroke-width="1.2"/>
            <circle cx="4.8" cy="10.5" r="1.1" fill="#22c55e"/>
            <line x1="8" y1="10.5" x2="15.5" y2="10.5" stroke="#475569" stroke-width="1" stroke-linecap="round"/>
          </svg>`;
        } else {
          return `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
            <rect x="2" y="5.5" width="16" height="9" rx="1.5" fill="#334155" stroke="#64748b" stroke-width="1.2"/>
            <circle cx="4.8" cy="10" r="1.1" fill="#22c55e"/>
            <line x1="8" y1="10" x2="15.5" y2="10" stroke="#475569" stroke-width="1" stroke-linecap="round"/>
          </svg>`;
        }
      }
      return `<svg viewBox="0 0 20 20" width="16" height="16" fill="none">
        <path d="M2.5 4A1.5 1.5 0 0 1 4 2.5h3.38a1.5 1.5 0 0 1 1.06.44l1.62 1.62H16A1.5 1.5 0 0 1 17.5 6V7H4A1.5 1.5 0 0 0 2.5 8.5V4z" fill="#d97706"/>
        <path d="M2.5 8.5A1.5 1.5 0 0 1 4 7h12a1.5 1.5 0 0 1 1.5 1.5v6.5A1.5 1.5 0 0 1 16 16.5H4A1.5 1.5 0 0 1 2.5 15V8.5z" fill="#f59e0b"/>
        <path d="M4 8h12v1H4z" fill="#fef3c7" opacity="0.6"/>
      </svg>`;
    }

    function renderLocalPathTree() {
      const popup = document.getElementById('localPathTreePopup');
      if (!popup) return;

      const normCurrent = (currentLocalPath || '').replace(/\//g, '\\').replace(/\\+$/, '');
      const currentUpper = normCurrent.toUpperCase();
      const desktopNorm = (globalDesktopDir || '').replace(/\//g, '\\').replace(/\\+$/, '');
      const isDesktopActive = normCurrent && desktopNorm && (currentUpper === desktopNorm.toUpperCase());

      let html = '';

      // 1. Desktop
      html += `
        <div class="path-tree-item ${isDesktopActive ? 'active' : ''}" style="padding-left: 10px;" onclick="onPathTreeItemClick('${escapeJsString(globalDesktopDir || 'Desktop')}')">
          <span class="tree-icon">${getTreeIconSvg('desktop')}</span>
          <span class="path-tree-label">Desktop</span>
        </div>
      `;

      // 2. This PC
      html += `
        <div class="path-tree-item" style="padding-left: 10px; cursor: default;">
          <span class="tree-icon">${getTreeIconSvg('thispc')}</span>
          <span class="path-tree-label" style="font-weight: 600;">This PC</span>
        </div>
      `;

      // 3. Logical Drives & Hierarchical Active Branch
      const drives = (globalDrivesInfo && globalDrivesInfo.length > 0)
        ? globalDrivesInfo
        : [{ path: 'C:\\', letter: 'C:', label: 'Local Disk', display: 'Local Disk (C:)', is_system: true }];

      drives.forEach(drive => {
        const driveLetter = drive.letter.toUpperCase();
        const driveRoot = drive.path.replace(/\//g, '\\');
        const isCurrentDrive = currentUpper.startsWith(driveLetter);
        const isDriveActive = isCurrentDrive && (currentUpper === driveLetter || currentUpper === driveRoot.replace(/\\+$/, '').toUpperCase());

        html += `
          <div class="path-tree-item ${isDriveActive ? 'active' : ''}" style="padding-left: 28px;" onclick="onPathTreeItemClick('${escapeJsString(drive.path)}')">
            <span class="tree-icon">${getTreeIconSvg('drive', drive.is_system)}</span>
            <span class="path-tree-label">${escapeHtml(drive.display)}</span>
          </div>
        `;

        if (isCurrentDrive) {
          const rest = normCurrent.substring(driveLetter.length).replace(/^[\\/]+/, '');
          if (rest) {
            const parts = rest.split(/[\\/]+/).filter(Boolean);
            let accumulatedPath = drive.path.replace(/[\\/]+$/, '');

            parts.forEach((seg, idx) => {
              accumulatedPath += '\\' + seg;
              const isSegActive = (idx === parts.length - 1);
              const indent = 28 + (idx + 1) * 16;

              html += `
                <div class="path-tree-item ${isSegActive ? 'active' : ''}" style="padding-left: ${indent}px;" onclick="onPathTreeItemClick('${escapeJsString(accumulatedPath)}')">
                  <span class="tree-icon">${getTreeIconSvg('folder')}</span>
                  <span class="path-tree-label">${escapeHtml(seg)}</span>
                </div>
              `;
            });
          }
        }
      });

      popup.innerHTML = html;
    }

    function onPathTreeItemClick(path) {
      closeLocalPathTree();
      if (!path) return;
      navigateLocal(path);
    }

    // Remote Directory Listing
    function renderRemoteFiles(files, listError = null) {
      remoteFiles = applyPaneSort([...(files || [])], true);
      selectedRemoteFile = null;
      lastRemoteClickedIdx = -1;
      const activeTab = getActiveRemoteTab();
      if (activeTab) {
        activeTab.files = [...remoteFiles];
        activeTab.path = currentRemotePath || activeTab.path;
        activeTab.listError = listError;
        activeTab.isConnected = !!isConnected;
        if (currentConnectedProfile) {
          activeTab.currentProfile = { ...currentConnectedProfile };
        }
        if (lastConnectedProfile) {
          activeTab.lastProfile = { ...lastConnectedProfile };
        }
        activeTab.title = getRemoteTabDisplayTitle(activeTab);
      }
      const tbody = document.getElementById('remoteFileList');
      if (!tbody) return;
      tbody.innerHTML = '';
      const frag = document.createDocumentFragment();

      const canGoUp = !isRemoteRoot(currentRemotePath);
      if (canGoUp) {
        const parentDirPath = getRemoteParentDir(currentRemotePath);
        const parentBaseName = parentDirPath === '/' ? '/' : (parentDirPath.split('/').filter(Boolean).pop() || '/');
        const trUp = document.createElement('tr');
        trUp.className = 'parent-dir-row';
        trUp.title = `Parent Directory: ${parentDirPath} (Double-click to go up, or Drag & Drop files here to move up)`;
        trUp.ondblclick = (e) => {
          e.preventDefault();
          remoteNavigateUp();
        };
        trUp.onclick = () => {
          const rows = document.querySelectorAll('#remoteFileList tr');
          rows.forEach(r => r.classList.remove('selected'));
          trUp.classList.add('selected');
          selectedRemoteFile = null;
          updatePaneStatsBadge(true);
        };

        const { svg, iconClass } = getFileIcon('..', true);
        trUp.innerHTML = `
          <td>
            <div class="file-cell">
              <span class="file-icon ${iconClass}">${svg}</span>
              <span class="parent-dir-dots">..</span>
              <span style="font-size: 10.5px; color: var(--text-dim); margin-left: 6px;">(${escapeHtml(parentBaseName)})</span>
            </div>
          </td>
          <td class="mono-cell">-</td>
          <td class="mono-cell">-</td>
          <td class="mono-cell">Parent Directory</td>
        `;
        attachInteractiveRowDropTarget(trUp, true, parentDirPath, parentBaseName, true);
        frag.appendChild(trUp);
      }

      if (listError) {
        const errTr = document.createElement('tr');
        errTr.innerHTML = `
          <td colspan="4" style="text-align: center; color: #ef4444; padding: ${canGoUp ? '20px' : '35px'} 15px;">
            <div style="display: flex; flex-direction: column; align-items: center; gap: 8px;">
              <div style="display: flex; align-items: center; gap: 8px; font-weight: 600; font-size: 13px;">
                <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="#ef4444" stroke-width="2">
                  <circle cx="12" cy="12" r="10"/>
                  <line x1="12" y1="8" x2="12" y2="12"/>
                  <line x1="12" y1="16" x2="12.01" y2="16"/>
                </svg>
                <span>Directory Access Notice</span>
              </div>
              <div style="font-size: 11.5px; color: var(--text-muted); font-family: monospace; max-width: 90%; word-break: break-all;">
                ${escapeHtml(listError)}
              </div>
            </div>
          </td>
        `;
        frag.appendChild(errTr);
        tbody.appendChild(frag);
        updatePaneStatsBadge(true);
        updateFsWatcher();
        return;
      }

      const filterLower = (remoteQuickFilterText || '').trim().toLowerCase();
      let renderedCount = 0;

      if (remoteFiles.length === 0) {
        const emptyTr = document.createElement('tr');
        emptyTr.innerHTML = `<td colspan="4" style="text-align:center; color: var(--text-dim); padding: ${canGoUp ? '16px' : '25px'}; font-style: italic;">Empty Directory</td>`;
        frag.appendChild(emptyTr);
        tbody.appendChild(frag);
        updatePaneStatsBadge(true);
        updateFsWatcher();
        return;
      }

      remoteFiles.forEach((f, idx) => {
        try {
          const fileName = (f.name || '').toString();
          if (!showHiddenFilesState && fileName.startsWith('.')) {
            return;
          }
          if (filterLower && !fileName.toLowerCase().includes(filterLower)) {
            return;
          }
          renderedCount++;

          const tr = document.createElement('tr');
          tr.draggable = true;
          tr.setAttribute('data-idx', idx);
          tr.setAttribute('data-path', f.path || fileName);
          tr.setAttribute('data-is-dir', f.is_dir ? '1' : '0');
          tr.classList.add(f.is_dir ? 'row-folder' : 'row-file');

          if (interactiveClipboard && interactiveClipboard.isRemote && interactiveClipboard.mode === 'cut') {
            if (interactiveClipboard.items.some(it => it.path === f.path)) {
              tr.classList.add('row-cut-pending');
            }
          }

          tr.ondragstart = (e) => {
            if (!tr.classList.contains('selected')) {
              selectRemoteRow(idx);
            }
            const selectedBatch = getSelectedRemoteItems();
            const batchItems = selectedBatch.length > 0 ? selectedBatch : [f];
            activeDragPayload = {
              source: 'remote',
              path: f.path,
              name: f.name,
              is_dir: f.is_dir,
              items: batchItems.map(it => ({ path: it.path, name: it.name, is_dir: !!it.is_dir }))
            };
            e.dataTransfer.effectAllowed = 'copyMove';
            e.dataTransfer.setData('text/plain', JSON.stringify(activeDragPayload));
            tr.classList.add('dragging');
          };
          tr.ondragend = () => {
            tr.classList.remove('dragging');
            activeDragPayload = null;
          };

          if (f.is_dir) {
            attachInteractiveRowDropTarget(tr, true, f.path, fileName, false);
          }

          tr.onclick = (e) => {
            if (e.shiftKey && lastRemoteClickedIdx >= 0) {
              const start = Math.min(lastRemoteClickedIdx, idx);
              const end = Math.max(lastRemoteClickedIdx, idx);
              const rows = document.querySelectorAll('#remoteFileList tr:not(.parent-dir-row)');
              rows.forEach(r => {
                const rIdx = parseInt(r.getAttribute('data-idx'));
                if (!isNaN(rIdx)) {
                  r.classList.toggle('selected', rIdx >= start && rIdx <= end);
                }
              });
              selectedRemoteFile = f;
              updatePaneStatsBadge(true);
            } else if (e.ctrlKey || e.metaKey) {
              const isNowSelected = tr.classList.toggle('selected');
              if (isNowSelected) {
                selectedRemoteFile = f;
              } else {
                const remaining = document.querySelectorAll('#remoteFileList tr.selected:not(.parent-dir-row)');
                if (remaining.length > 0) {
                  const firstIdx = parseInt(remaining[remaining.length - 1].getAttribute('data-idx'));
                  selectedRemoteFile = !isNaN(firstIdx) ? (remoteFiles[firstIdx] || null) : null;
                } else {
                  selectedRemoteFile = null;
                }
              }
              lastRemoteClickedIdx = idx;
              updatePaneStatsBadge(true);
            } else {
              selectRemoteRow(idx);
              lastRemoteClickedIdx = idx;
            }
          };
          tr.ondblclick = () => openRemoteItem(f, tr);
          if (f.is_dir) {
            tr.onauxclick = (e) => {
              if (e.button === 1) {
                e.preventDefault();
                e.stopPropagation();
                openFolderInNewTab(true, f.path);
              }
            };
          }
          tr.oncontextmenu = (e) => {
            e.preventDefault();
            e.stopPropagation();
            if (!tr.classList.contains('selected')) {
              selectRemoteRow(idx);
            }
            openContextMenu(e, 'remote', f, idx);
          };

          const { svg, iconClass } = getFileIcon(fileName, !!f.is_dir);

          let compareBadgeHtml = '';
          if (typeof compareDirsActive !== 'undefined' && compareDirsActive) {
            const cmp = getComparisonStatus(f, true);
            if (cmp && cmp.tag) {
              compareBadgeHtml = `<span class="compare-tag" style="color:${cmp.color}; background:${cmp.bg}; border:1px solid ${cmp.border};">${cmp.tag}</span>`;
            }
            if (cmp && cmp.rowClass) {
              tr.classList.add(cmp.rowClass);
            }
          }

          const rDirSizeVal = f.is_dir ? calculatedFolderSizes.get(f.path) : null;
          const rSizeCellText = f.is_dir
            ? (typeof rDirSizeVal === 'number' ? `<span style="color: #38bdf8; font-weight: 600;">${formatBytes(rDirSizeVal)}</span>` : '-')
            : formatBytes(f.size || 0);

          tr.innerHTML = `
            <td>
              <div class="file-cell">
                <span class="file-icon ${iconClass}">${svg}</span>
                <span>${escapeHtml(fileName)}</span>
                ${compareBadgeHtml}
              </div>
            </td>
            <td class="mono-cell">${rSizeCellText}</td>
            <td class="mono-cell">${f.modified || '-'}</td>
            <td class="mono-cell">${f.permissions || '-'}</td>
          `;
          frag.appendChild(tr);
        } catch (itemErr) {
          console.error('Error rendering remote file item:', f, itemErr);
        }
      });

      if (renderedCount === 0 && filterLower) {
        const noMatchTr = document.createElement('tr');
        noMatchTr.innerHTML = `<td colspan="4" style="text-align:center; color: var(--text-dim); padding: 20px 10px; font-style: italic;">No items match filter "${escapeHtml(remoteQuickFilterText)}"</td>`;
        frag.appendChild(noMatchTr);
      }

      tbody.appendChild(frag);
      updatePaneStatsBadge(true);
      updateFsWatcher();
    }

    function selectRemoteRow(idx) {
      const rows = document.querySelectorAll('#remoteFileList tr:not(.parent-dir-row)');
      rows.forEach((r) => {
        const rIdx = parseInt(r.getAttribute('data-idx'));
        r.classList.toggle('selected', rIdx === idx);
      });
      const upRow = document.querySelector('#remoteFileList tr.parent-dir-row');
      if (upRow) upRow.classList.remove('selected');
      selectedRemoteFile = remoteFiles[idx];
      updatePaneStatsBadge(true);
    }

    // ========================================================
    // Interactive File Move, Copy, Undo, Toast & Clipboard Engine
    // ========================================================
    function showInteractiveActionToast(title, subtitle, targetDir, isRemote, allowUndo = false) {
      const toast = document.getElementById('interactiveActionToast');
      if (!toast) return;
      if (interactiveToastTimer) clearTimeout(interactiveToastTimer);

      const tEl = document.getElementById('iatTitle');
      const sEl = document.getElementById('iatSubtitle');
      const undoBtn = document.getElementById('iatUndoBtn');
      const gotoBtn = document.getElementById('iatGotoBtn');

      if (tEl) tEl.textContent = title;
      if (sEl) sEl.textContent = subtitle;
      if (undoBtn) undoBtn.style.display = allowUndo ? 'inline-flex' : 'none';
      if (gotoBtn) gotoBtn.style.display = targetDir ? 'inline-flex' : 'none';

      toast.dataset.targetDir = targetDir || '';
      toast.dataset.isRemote = isRemote ? 'true' : 'false';
      toast.classList.add('active');

      interactiveToastTimer = setTimeout(() => {
        closeInteractiveActionToast();
      }, 9500);
    }

    function closeInteractiveActionToast() {
      const toast = document.getElementById('interactiveActionToast');
      if (toast) toast.classList.remove('active');
      if (interactiveToastTimer) {
        clearTimeout(interactiveToastTimer);
        interactiveToastTimer = null;
      }
    }

    async function openLastInteractiveTarget() {
      const toast = document.getElementById('interactiveActionToast');
      if (!toast) return;
      const targetDir = toast.dataset.targetDir;
      const isRemote = toast.dataset.isRemote === 'true';
      closeInteractiveActionToast();
      if (!targetDir) return;
      if (isRemote) {
        await navigateRemote(targetDir);
      } else {
        await navigateLocal(targetDir);
      }
    }

    async function undoLastInteractiveMove() {
      if (!lastInteractiveMoveUndo || !Array.isArray(lastInteractiveMoveUndo.moves) || lastInteractiveMoveUndo.moves.length === 0) {
        closeInteractiveActionToast();
        return;
      }
      const undoData = lastInteractiveMoveUndo;
      lastInteractiveMoveUndo = null;
      closeInteractiveActionToast();

      const isRemote = undoData.isRemote;
      addLog(`Reverting last move (${undoData.moves.length} item(s))...`, 'cmd');

      try {
        if (isRemote) {
          for (const m of undoData.moves) {
            await sendRust('remote_rename', { old_path: m.to, new_path: m.from });
          }
          await navigateRemote(currentRemotePath);
        } else {
          for (const m of undoData.moves) {
            await sendRust('local_rename', { old_path: m.to, new_path: m.from });
          }
          await refreshLocal();
        }
        addLog(`Undo complete: Restored ${undoData.moves.length} item(s) to their original folder.`, 'success');
        showInteractiveActionToast(
          `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10"/></svg> Undo Completed (${undoData.moves.length} item${undoData.moves.length === 1 ? '' : 's'})`,
          `Restored back to ${undoData.prevDir || 'original folder'}`,
          undoData.prevDir,
          isRemote,
          false
        );
      } catch (err) {
        addLog(`Undo move failed: ${err.message || err}`, 'error');
        alert(`Failed to undo move:\n${err.message || err}`);
      }
    }

    async function executeInteractiveMove(isRemote, items, targetDir, isCopy = false, openAfter = false) {
      closeContextMenu();
      if (!items || items.length === 0) {
        alert('Please select one or more files or folders first.');
        return;
      }
      const cleanTarget = (targetDir || '').trim();
      if (!cleanTarget) return;

      const sourceDir = isRemote ? currentRemotePath : currentLocalPath;
      if (!isCopy && cleanTarget.replace(/[\\/]+$/, '') === (sourceDir || '').replace(/[\\/]+$/, '')) {
        addLog('Source and destination folders are identical.', 'info');
        return;
      }

      // Prevent moving a folder into itself or its own descendant
      for (const it of items) {
        if (it.is_dir) {
          const normIt = it.path.replace(/[\\/]+$/, '').toLowerCase();
          const normTgt = cleanTarget.replace(/[\\/]+$/, '').toLowerCase();
          const sep = isRemote ? '/' : '\\';
          if (normTgt === normIt || normTgt.startsWith(normIt + sep)) {
            alert(`Cannot ${isCopy ? 'copy' : 'move'} folder "${it.name}" into itself.`);
            return;
          }
        }
      }

      const sources = items.map(it => it.path);
      const sep = isRemote ? '/' : '\\';
      const baseDir = cleanTarget.replace(/[\\/]+$/, '');
      // Explicit (old_path -> new_path) pairs. The backend accepts this shape as
      // well as `sources` + `target_dir`, so a contract drift between the two
      // sides can never again turn a drag-and-drop move into a silent no-op.
      const moveItems = items.map(it => {
        const base = it.name || (it.path || '').split(/[\\/]/).filter(Boolean).pop() || '';
        return {
          old_path: it.path,
          new_path: baseDir ? `${baseDir}${sep}${base}` : `${sep}${base}`
        };
      });
      const actionVerb = isCopy ? 'Copying' : 'Moving';
      const pastVerb = isCopy ? 'Copied' : 'Moved';
      const shortDest = cleanTarget.split(/[\\/]/).filter(Boolean).pop() || cleanTarget;

      // Turn a backend response into an honest verdict. `res.moved_count || n`
      // used to treat a real 0 as "falsy" and print a success message for a
      // batch that moved nothing.
      const reportOutcome = (res) => {
        const requested = typeof res.requested_count === 'number' ? res.requested_count : sources.length;
        const movedCount = typeof res.moved_count === 'number' ? res.moved_count : requested;
        const errs = Array.isArray(res.errors) ? res.errors : [];
        if (movedCount === 0) {
          addLog(`${actionVerb} failed: no item was ${pastVerb.toLowerCase()}. ${errs.join(' | ') || 'No reason reported by the server.'}`, 'error');
          return 0;
        }
        if (movedCount < requested) {
          addLog(`${pastVerb} ${movedCount} of ${requested} item(s) to ${cleanTarget} — ${errs.join(' | ')}`, 'warning');
        } else {
          addLog(`${pastVerb} ${movedCount} item(s) to ${cleanTarget}`, 'success');
        }
        if (errs.length > 0 && movedCount >= requested) {
          addLog(`Notes: ${errs.join(' | ')}`, 'warning');
        }
        return movedCount;
      };

      addLog(`${actionVerb} ${sources.length} ${isRemote ? 'remote' : 'local'} item(s) to '${cleanTarget}'...`, 'cmd');

      try {
        if (isRemote) {
          if (!isConnected && appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile) {
            const ok = await ensureSilentConnected(currentRemotePath);
            if (!ok) return;
          }
          const res = await sendRust('remote_move_batch', {
            items: moveItems,
            sources,
            target_dir: cleanTarget,
            copy: !!isCopy
          });
          if (!recentRemoteMoveDirs.includes(cleanTarget)) {
            recentRemoteMoveDirs.unshift(cleanTarget);
            if (recentRemoteMoveDirs.length > 6) recentRemoteMoveDirs.pop();
          }
          if (!isCopy && res.moves && res.moves.length > 0) {
            lastInteractiveMoveUndo = {
              isRemote: true,
              moves: res.moves,
              targetDir: cleanTarget,
              prevDir: currentRemotePath
            };
          }
          if (openAfter) {
            await navigateRemote(cleanTarget);
          } else if (res.files) {
            renderRemoteFiles(res.files);
          } else {
            await refreshRemote();
          }
          const movedCount = reportOutcome(res);
          if (movedCount > 0) showInteractiveActionToast(
            `${isCopy ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1" ry="1"/></svg> Copied' : '⤴ Moved'} ${movedCount} item${movedCount === 1 ? '' : 's'} to "${shortDest}"`,
            `Destination: ${cleanTarget}`,
            cleanTarget,
            true,
            !isCopy && res.moves && res.moves.length > 0
          );
        } else {
          const res = await sendRust('local_move_batch', {
            items: moveItems,
            sources,
            target_dir: cleanTarget,
            copy: !!isCopy
          });
          if (!recentLocalMoveDirs.includes(cleanTarget)) {
            recentLocalMoveDirs.unshift(cleanTarget);
            if (recentLocalMoveDirs.length > 6) recentLocalMoveDirs.pop();
          }
          if (!isCopy && res.moves && res.moves.length > 0) {
            lastInteractiveMoveUndo = {
              isRemote: false,
              moves: res.moves,
              targetDir: cleanTarget,
              prevDir: currentLocalPath
            };
          }
          if (openAfter) {
            await navigateLocal(cleanTarget);
          } else {
            await refreshLocal();
          }
          const movedCount = reportOutcome(res);
          if (movedCount > 0) showInteractiveActionToast(
            `${isCopy ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1" ry="1"/></svg> Copied' : '⤴ Moved'} ${movedCount} item${movedCount === 1 ? '' : 's'} to "${shortDest}"`,
            `Destination: ${cleanTarget}`,
            cleanTarget,
            false,
            !isCopy && res.moves && res.moves.length > 0
          );
        }
      } catch (err) {
        addLog(`${actionVerb} failed: ${err.message || err}`, 'error');
        alert(`Failed to ${isCopy ? 'copy' : 'move'} items:\n${err.message || err}`);
        return false;
      }
      return true;
    }

    async function executeTargetedCrossPaneTransfer(items, isRemoteTarget, targetDirPath) {
      if (!items || items.length === 0) return;
      if (isRemoteTarget) {
        if (!isConnected) {
          if (appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile) {
            const ok = await ensureSilentConnected(currentRemotePath);
            if (!ok) return;
          } else {
            alert('Please connect to a remote server before uploading.');
            return;
          }
        }
        const savedRemotePath = currentRemotePath;
        currentRemotePath = targetDirPath;
        fsWatcherSuspended = true;
        try {
          addLog(`Targeted Upload: Sending ${items.length} item(s) directly into remote '${targetDirPath}'...`, 'cmd');
          for (const it of items) {
            if (it.is_dir) {
              await uploadFolderDirect(it.path, it.name);
            } else {
              await uploadFileDirect(it.path, it.name);
            }
          }
          showInteractiveActionToast(
            `<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg> Uploaded ${items.length} item${items.length === 1 ? '' : 's'} into folder`,
            `Remote Target: ${targetDirPath}`,
            targetDirPath,
            true,
            false
          );
        } finally {
          fsWatcherSuspended = false;
          currentRemotePath = savedRemotePath;
          await navigateRemote(savedRemotePath);
        }
      } else {
        const savedLocalPath = currentLocalPath;
        currentLocalPath = targetDirPath;
        fsWatcherSuspended = true;
        try {
          addLog(`Targeted Download: Saving ${items.length} item(s) directly into local '${targetDirPath}'...`, 'cmd');
          for (const it of items) {
            if (it.is_dir) {
              await downloadFolderDirect(it.path, it.name);
            } else {
              await downloadFileDirect(it.path, it.name);
            }
          }
          showInteractiveActionToast(
            `<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg> Downloaded ${items.length} item${items.length === 1 ? '' : 's'} into folder`,
            `Local Target: ${targetDirPath}`,
            targetDirPath,
            false,
            false
          );
        } finally {
          fsWatcherSuspended = false;
          currentLocalPath = savedLocalPath;
          await navigateLocal(savedLocalPath);
        }
      }
    }

    async function moveSelectedToParent(isRemote) {
      closeContextMenu();
      closeAllMenus();
      if (isRemote) {
        if (isRemoteRoot(currentRemotePath)) {
          alert('Already at the root directory (/). Cannot move items higher.');
          return;
        }
        const items = getSelectedRemoteItems();
        if (items.length === 0) {
          alert('Please select one or more remote files or folders to move to the parent directory (..).');
          return;
        }
        const parentDir = getRemoteParentDir(currentRemotePath);
        await executeInteractiveMove(true, items, parentDir, false, false);
      } else {
        if (isLocalRoot(currentLocalPath)) {
          alert('Already at the drive root directory. Cannot move items higher.');
          return;
        }
        const items = getSelectedLocalItems();
        if (items.length === 0) {
          alert('Please select one or more local files or folders to move to the parent directory (..).');
          return;
        }
        const parentDir = getLocalParentDir(currentLocalPath);
        await executeInteractiveMove(false, items, parentDir, false, false);
      }
    }

    function openInteractiveMoveModal(isRemote, initialMode = 'move') {
      closeContextMenu();
      closeAllMenus();
      const items = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
      if (items.length === 0) {
        alert(`Please select one or more ${isRemote ? 'remote' : 'local'} files or folders first.`);
        return;
      }

      const sourceDir = isRemote ? currentRemotePath : currentLocalPath;
      const defaultDest = isRemote
        ? (!isRemoteRoot(currentRemotePath) ? getRemoteParentDir(currentRemotePath) : currentRemotePath)
        : (!isLocalRoot(currentLocalPath) ? getLocalParentDir(currentLocalPath) : currentLocalPath);

      interactiveMoveContext = {
        isRemote: !!isRemote,
        items,
        sourceDir,
        mode: initialMode === 'copy' ? 'copy' : 'move'
      };

      const scopeBadge = document.getElementById('imScopeBadge');
      const summaryEl = document.getElementById('imSelectedSummary');
      const srcPathEl = document.getElementById('imSourcePathDisplay');
      const destInput = document.getElementById('imDestPathInput');
      const chipsBox = document.getElementById('imQuickChipsContainer');

      if (scopeBadge) {
        scopeBadge.textContent = isRemote ? 'Remote Server' : 'Local Computer';
        scopeBadge.style.color = isRemote ? '#22c55e' : '#38bdf8';
        scopeBadge.style.borderColor = isRemote ? 'rgba(34, 197, 94, 0.35)' : 'rgba(56, 189, 248, 0.35)';
        scopeBadge.style.background = isRemote ? 'rgba(34, 197, 94, 0.14)' : 'rgba(56, 189, 248, 0.14)';
      }

      if (summaryEl) {
        summaryEl.textContent = items.length === 1
          ? `${items[0].is_dir ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg> ' : '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><polyline points="13 2 13 9 20 9"/></svg> '}${items[0].name}`
          : `${items.length} items selected (${items.slice(0, 3).map(i => i.name).join(', ')}${items.length > 3 ? '...' : ''})`;
      }
      if (srcPathEl) srcPathEl.textContent = `From: ${sourceDir}`;
      if (destInput) destInput.value = defaultDest;

      // Build Quick Destination Chips
      if (chipsBox) {
        let chipsHtml = '';
        const canUp = isRemote ? !isRemoteRoot(currentRemotePath) : !isLocalRoot(currentLocalPath);
        if (canUp) {
          const parentPath = isRemote ? getRemoteParentDir(currentRemotePath) : getLocalParentDir(currentLocalPath);
          const parentName = parentPath.split(/[\\/]/).filter(Boolean).pop() || parentPath;
          chipsHtml += `
            <span class="quick-dest-chip parent-chip" onclick="selectInteractiveDestChip('${escapeJsString(parentPath)}')" title="${escapeHtml(parentPath)}">
              <span>⤴</span>
              <span>.. Parent (${escapeHtml(parentName)})</span>
            </span>
          `;
        }

        // Subfolders in current directory (excluding selected folders themselves)
        const allFiles = isRemote ? (remoteFiles || []) : (localFiles || []);
        const subFolders = allFiles.filter(f => f.is_dir && !items.some(it => it.path === f.path));
        subFolders.slice(0, 14).forEach(sub => {
          chipsHtml += `
            <span class="quick-dest-chip" onclick="selectInteractiveDestChip('${escapeJsString(sub.path)}')" title="${escapeHtml(sub.path)}">
              <span><svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg></span>
              <span>${escapeHtml(sub.name)}</span>
            </span>
          `;
        });

        // Recent move destinations
        const recents = isRemote ? recentRemoteMoveDirs : recentLocalMoveDirs;
        recents.forEach(rPath => {
          if (rPath !== sourceDir && !subFolders.some(s => s.path === rPath)) {
            const rLabel = rPath.split(/[\\/]/).filter(Boolean).pop() || rPath;
            chipsHtml += `
              <span class="quick-dest-chip" onclick="selectInteractiveDestChip('${escapeJsString(rPath)}')" title="Recent: ${escapeHtml(rPath)}">
                <span><svg class="icon icon-lg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg></span>
                <span>${escapeHtml(rLabel)}</span>
              </span>
            `;
          }
        });

        if (!chipsHtml) {
          chipsHtml = `<span style="font-size: 11px; color: var(--text-dim); font-style: italic;">Type any target directory path below</span>`;
        }
        chipsBox.innerHTML = chipsHtml;
      }

      setInteractiveMoveMode(interactiveMoveContext.mode);
      updateInteractiveMovePreview();

      const modal = document.getElementById('interactiveMoveModal');
      if (modal) modal.classList.add('active');
      setTimeout(() => {
        if (destInput) {
          destInput.focus();
          destInput.select();
        }
      }, 60);
    }

    function closeInteractiveMoveModal() {
      const modal = document.getElementById('interactiveMoveModal');
      if (modal) modal.classList.remove('active');
      interactiveMoveContext = null;
    }

    function selectInteractiveDestChip(path) {
      const destInput = document.getElementById('imDestPathInput');
      if (destInput) {
        destInput.value = path;
        updateInteractiveMovePreview();
      }
      const chips = document.querySelectorAll('#imQuickChipsContainer .quick-dest-chip');
      chips.forEach(c => {
        c.classList.toggle('active', c.getAttribute('title') === path || c.getAttribute('title') === 'Recent: ' + path);
      });
    }

    function setInteractiveMoveMode(mode) {
      if (interactiveMoveContext) {
        interactiveMoveContext.mode = mode === 'copy' ? 'copy' : 'move';
      }
      const movePill = document.getElementById('imModeMovePill');
      const copyPill = document.getElementById('imModeCopyPill');
      const submitBtn = document.getElementById('imSubmitBtn');
      const radios = document.getElementsByName('imActionMode');
      for (const r of radios) {
        r.checked = (r.value === mode);
      }
      if (movePill) {
        movePill.style.borderColor = mode === 'move' ? 'var(--accent)' : 'var(--border-color)';
        movePill.style.background = mode === 'move' ? 'rgba(56, 189, 248, 0.12)' : 'rgba(15, 23, 42, 0.6)';
      }
      if (copyPill) {
        copyPill.style.borderColor = mode === 'copy' ? 'var(--accent)' : 'var(--border-color)';
        copyPill.style.background = mode === 'copy' ? 'rgba(56, 189, 248, 0.12)' : 'rgba(15, 23, 42, 0.6)';
      }
      if (submitBtn) {
        submitBtn.innerHTML = mode === 'copy' ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1" ry="1"/></svg> Copy Now' : '<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg> Move Now';
      }
      updateInteractiveMovePreview();
    }

    function updateInteractiveMovePreview() {
      const destInput = document.getElementById('imDestPathInput');
      const previewEl = document.getElementById('imPreviewTargetText');
      if (!destInput || !previewEl || !interactiveMoveContext) return;
      const rawDest = (destInput.value || '').trim() || '/';
      const sep = interactiveMoveContext.isRemote ? '/' : '\\';
      const firstItem = interactiveMoveContext.items[0]?.name || 'file';
      const cleanedDest = rawDest.replace(/[\\/]+$/, '');
      const sampleFull = cleanedDest ? (cleanedDest + sep + firstItem) : (sep + firstItem);
      if (interactiveMoveContext.items.length === 1) {
        previewEl.textContent = sampleFull;
      } else {
        previewEl.textContent = `${sampleFull} (+${interactiveMoveContext.items.length - 1} more)`;
      }
    }

    async function submitInteractiveMoveModal() {
      if (!interactiveMoveContext) return;
      const destInput = document.getElementById('imDestPathInput');
      const openAfter = document.getElementById('imOpenDestAfterCheck')?.checked || false;
      const targetDir = (destInput?.value || '').trim();
      if (!targetDir) {
        alert('Please enter or select a destination folder.');
        if (destInput) destInput.focus();
        return;
      }
      const ctx = interactiveMoveContext;
      closeInteractiveMoveModal();
      await executeInteractiveMove(ctx.isRemote, ctx.items, targetDir, ctx.mode === 'copy', openAfter);
    }

    // ========================================================
    // Interactive Clipboard (Cut / Copy / Paste / Duplicate)
    // ========================================================
    function clipboardCutSelected(isRemote) {
      closeContextMenu();
      const items = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
      if (items.length === 0) {
        addLog(`Cut: nothing selected in the ${isRemote ? 'remote' : 'local'} pane.`, 'warning');
        return;
      }
      interactiveClipboard = {
        isRemote: !!isRemote,
        mode: 'cut',
        items: items.map(i => ({ path: i.path, name: i.name, is_dir: !!i.is_dir })),
        sourceDir: isRemote ? currentRemotePath : currentLocalPath
      };
      if (isRemote) renderRemoteFiles(remoteFiles);
      else renderLocalFiles(localFiles);
      addLog(`Cut ${items.length} ${isRemote ? 'remote' : 'local'} item(s) to clipboard. Navigate to destination and press Ctrl+V or Paste.`, 'info');
    }

    function clipboardCopySelected(isRemote) {
      closeContextMenu();
      const items = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
      if (items.length === 0) {
        addLog(`Copy: nothing selected in the ${isRemote ? 'remote' : 'local'} pane.`, 'warning');
        return;
      }
      interactiveClipboard = {
        isRemote: !!isRemote,
        mode: 'copy',
        items: items.map(i => ({ path: i.path, name: i.name, is_dir: !!i.is_dir })),
        sourceDir: isRemote ? currentRemotePath : currentLocalPath
      };
      if (isRemote) renderRemoteFiles(remoteFiles);
      else renderLocalFiles(localFiles);
      addLog(`Copied ${items.length} ${isRemote ? 'remote' : 'local'} item(s) to clipboard. Press Ctrl+V or Paste in any folder.`, 'info');
    }

    async function clipboardPasteHere(isRemoteTarget) {
      closeContextMenu();
      if (!interactiveClipboard || !interactiveClipboard.items || interactiveClipboard.items.length === 0) {
        addLog(`Paste: the clipboard is empty. Use Ctrl+X or Ctrl+C on a file first.`, 'warning');
        return;
      }
      const clip = interactiveClipboard;
      const targetDir = isRemoteTarget ? currentRemotePath : currentLocalPath;

      if (clip.isRemote === isRemoteTarget) {
        const isSameDir = clip.sourceDir.replace(/[\\/]+$/, '') === (targetDir || '').replace(/[\\/]+$/, '');
        if (clip.mode === 'copy' && isSameDir) {
          await duplicateItemsInPlace(isRemoteTarget, clip.items);
          return;
        }
        const ok = await executeInteractiveMove(isRemoteTarget, clip.items, targetDir, clip.mode === 'copy', false);
        // Keep the clipboard alive when the operation failed, so the user can retry
        // instead of losing the cut selection.
        if (clip.mode === 'cut' && ok) {
          interactiveClipboard = null;
        }
      } else {
        // Cross-pane paste transfers files between Local and Remote!
        await executeTargetedCrossPaneTransfer(clip.items, isRemoteTarget, targetDir);
        if (clip.mode === 'cut') {
          interactiveClipboard = null;
        }
      }
    }

    async function duplicateItemsInPlace(isRemote, items) {
      closeContextMenu();
      if (!items || items.length === 0) return;

      const targetDir = isRemote ? currentRemotePath : currentLocalPath;
      const sources = items.map(it => it.path);
      addLog(`Duplicating ${sources.length} ${isRemote ? 'remote' : 'local'} item(s) in '${targetDir}'...`, 'cmd');

      // Route through the batch copy endpoints: they handle directories and
      // binary files, and the backend appends " (2)" when the name is taken.
      // The previous local implementation used local_read/local_write, which is
      // text-only and silently failed on binaries and folders.
      try {
        const res = await sendRust(isRemote ? 'remote_move_batch' : 'local_move_batch', {
          sources,
          target_dir: targetDir,
          copy: true
        });
        const made = typeof res.moved_count === 'number' ? res.moved_count : sources.length;
        const errs = Array.isArray(res.errors) ? res.errors : [];
        if (made === 0) {
          addLog(`Duplicate failed: ${errs.join(' | ') || 'nothing was copied'}.`, 'error');
        } else {
          addLog(`Created ${made} duplicate(s) in '${targetDir}'.`, 'success');
          if (errs.length > 0) addLog(`Notes: ${errs.join(' | ')}`, 'warning');
        }
      } catch (err) {
        addLog(`Duplicate failed: ${err.message || err}`, 'error');
        alert('Duplicate failed:\n' + (err.message || err));
      }

      if (isRemote) await refreshRemote();
      else await refreshLocal();
    }

    function selectAllItemsInPane(isRemote, filterType = 'all') {
      setActivePaneFocus(isRemote);
      const selector = isRemote ? '#remoteFileList tr:not(.parent-dir-row)' : '#localFileList tr:not(.parent-dir-row)';
      const rows = document.querySelectorAll(selector);
      const filesList = isRemote ? (remoteFiles || []) : (localFiles || []);
      let firstIdx = -1;
      rows.forEach(r => {
        if (r.hasAttribute('data-idx')) {
          const idx = parseInt(r.getAttribute('data-idx'));
          const item = filesList[idx];
          let shouldSelect = true;
          if (filterType === 'files') {
            shouldSelect = item && !item.is_dir;
          } else if (filterType === 'folders') {
            shouldSelect = item && !!item.is_dir;
          } else if (filterType === 'invert') {
            shouldSelect = !r.classList.contains('selected');
          }
          r.classList.toggle('selected', !!shouldSelect);
          if (shouldSelect && firstIdx === -1) {
            firstIdx = idx;
          }
        }
      });
      if (firstIdx >= 0) {
        if (isRemote) {
          selectedRemoteFile = remoteFiles[firstIdx] || null;
        } else {
          selectedLocalFile = localFiles[firstIdx] || null;
        }
      } else {
        if (isRemote) selectedRemoteFile = null;
        else selectedLocalFile = null;
      }
      updatePaneStatsBadge(isRemote);
      updateCommanderStatusFooter();
    }

    async function openRemoteItem(item, rowEl = null) {
      if (rowEl) {
        rowEl.classList.add('item-loading');
        const cell = rowEl.querySelector('.file-cell');
        if (cell && !cell.querySelector('.file-opening-spinner')) {
          const sp = document.createElement('span');
          sp.className = 'file-opening-spinner';
          cell.appendChild(sp);
        }
      }
      try {
        if (!isConnected && appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile) {
          if (item.is_dir) {
            await navigateRemote(item.path);
            return;
          } else {
            const ok = await ensureSilentConnected(currentRemotePath);
            if (!ok) return;
          }
        }
        if (item.is_dir) {
          await navigateRemote(item.path);
        } else {
          if (appOptions && appOptions.dblRemote === 'transfer') {
            await downloadFileDirect(item.path, item.name);
          } else {
            await openRemoteFileInEditor(item.path);
          }
        }
      } finally {
        if (rowEl) {
          rowEl.classList.remove('item-loading');
          const sp = rowEl.querySelector('.file-opening-spinner');
          if (sp) sp.remove();
        }
      }
    }

    const remoteDirCache = new Map();
    const REMOTE_DIR_CACHE_TTL_MS = 60000;

    function getRemoteDirCacheKey(path) {
      const prof = currentConnectedProfile || lastConnectedProfile;
      const hostKey = prof && prof.host ? `${prof.host}:${prof.port || 22}:${prof.username || ''}` : 'active';
      const normPath = (path || '/').replace(/\/+$/, '') || '/';
      return `${hostKey}|${normPath}`;
    }

    function invalidateRemoteDirCache(path = null) {
      if (!path) {
        remoteDirCache.clear();
        return;
      }
      const key = getRemoteDirCacheKey(path);
      remoteDirCache.delete(key);
    }

    async function navigateRemote(path, skipHistory = false, forceFresh = false) {
      if (!path) path = currentRemotePath || '/';
      const prevRemote = (currentRemotePath || '').replace(/\/+$/, '') || '/';

      if (!isConnected) {
        if (appOptions && appOptions.autoReconnectOnClick !== false && (lastConnectedProfile || currentConnectedProfile)) {
          const rec = await ensureSilentConnected(path);
          if (!rec || !rec.ok) return;
          if (rec.alreadyNavigated) {
            if (!skipHistory && !isHistoryNavigating && prevRemote && currentRemotePath && prevRemote !== ((currentRemotePath || '').replace(/\/+$/, '') || '/')) {
              const tab = getActiveRemoteTab();
              if (tab) {
                tab.historyBack.push(prevRemote);
                tab.historyForward = [];
              }
            }
            syncCurrentRemoteTabState();
            await triggerRemoteToLocalSync(prevRemote, currentRemotePath);
            return;
          }
        } else {
          return;
        }
      }

      // Optimistic Stale-While-Revalidate instant rendering from cache when available
      const cacheKey = getRemoteDirCacheKey(path);
      const cachedEntry = !forceFresh ? remoteDirCache.get(cacheKey) : null;
      if (cachedEntry && (Date.now() - cachedEntry.ts < REMOTE_DIR_CACHE_TTL_MS) && Array.isArray(cachedEntry.files)) {
        const normTarget = (cachedEntry.pwd || path || '/').replace(/\/+$/, '') || '/';
        currentRemotePath = normTarget;
        const rInput = document.getElementById('remotePathInput');
        if (rInput) rInput.value = currentRemotePath;
        renderRemoteFiles(cachedEntry.files, null);
      }

      try {
        addLog('CWD -> ' + path, 'cmd');
        const data = await sendRust('remote_cd', { path });
        const newResolved = (data.pwd || '').replace(/\/+$/, '') || '/';
        if (!skipHistory && !isHistoryNavigating && prevRemote && newResolved && prevRemote !== newResolved) {
          const tab = getActiveRemoteTab();
          if (tab) {
            tab.historyBack.push(prevRemote);
            tab.historyForward = [];
          }
        }
        currentRemotePath = data.pwd;
        if (lastConnectedProfile) lastConnectedProfile.remote_dir = currentRemotePath;
        if (currentConnectedProfile) currentConnectedProfile.remote_dir = currentRemotePath;
        document.getElementById('remotePathInput').value = currentRemotePath;
        if (data.remote_history) updateRemoteHistory(data.remote_history);
        if (data.list_error) {
          addLog('Remote directory notice: ' + data.list_error, 'error');
        } else if (Array.isArray(data.files)) {
          remoteDirCache.set(getRemoteDirCacheKey(currentRemotePath), {
            pwd: currentRemotePath,
            files: data.files,
            ts: Date.now()
          });
          if (remoteDirCache.size > 60) {
            const oldestKey = remoteDirCache.keys().next().value;
            if (oldestKey) remoteDirCache.delete(oldestKey);
          }
        }
        renderRemoteFiles(data.files, data.list_error);
        syncCurrentRemoteTabState();
        const rTree = document.getElementById('remotePathTreePopup');
        if (rTree && rTree.classList.contains('active')) {
          renderRemotePathTree();
        }
        await triggerRemoteToLocalSync(prevRemote, currentRemotePath);
      } catch (err) {
        addLog('Remote navigation error: ' + err.message, 'error');
        if (!remoteFiles || remoteFiles.length === 0) {
          renderRemoteFiles([], err.message);
        }
      }
    }

    async function triggerRemoteToLocalSync(prevRemote, newRemoteRaw) {
      if (!(appOptions && appOptions.syncBrowsing && !isSyncNavigating && prevRemote)) return;
      const newRemote = (newRemoteRaw || '').replace(/\/+$/, '') || '/';
      if (newRemote === prevRemote) return;
      isSyncNavigating = true;
      try {
        const prefix = prevRemote === '/' ? '/' : prevRemote + '/';
        if (newRemote.startsWith(prefix)) {
          const subName = newRemote.substring(prefix.length);
          if (subName && !subName.includes('/')) {
            const matchLocal = (localFiles || []).find(f => f.is_dir && f.name.toLowerCase() === subName.toLowerCase());
            if (matchLocal) {
              addLog(`[Sync Browsing] Following folder '${subName}' on Local Computer`, 'info');
              await navigateLocal(matchLocal.path);
            }
          }
        } else {
          const revPrefix = newRemote === '/' ? '/' : newRemote + '/';
          if (prevRemote.startsWith(revPrefix)) {
            const remPart = prevRemote.substring(revPrefix.length);
            if (remPart && !remPart.includes('/') && !isLocalRoot(currentLocalPath)) {
              addLog(`[Sync Browsing] Navigating up on Local Computer`, 'info');
              localNavigateUp();
            }
          }
        }
      } finally {
        isSyncNavigating = false;
      }
    }

    function remoteNavigateUp() {
      if (!isConnected && !(appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile)) return;
      let parent = (currentRemotePath || '/').replace(/\/[^\/]+$/, '');
      if (!parent) parent = '/';
      navigateRemote(parent);
    }

    function refreshRemote() {
      if (!isConnected && !(appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile)) return;
      invalidateRemoteDirCache(currentRemotePath || '/');
      navigateRemote(currentRemotePath || '/', true, true);
    }

    // ========================================================
    // Live pane synchronisation (background change detector)
    // --------------------------------------------------------
    // Rust fingerprints the visible local and remote directories and pushes
    // `onFsChanged` whenever something outside the app modifies them. The panes
    // are then re-read in place: selection, scroll position and quick filters are
    // preserved, so the refresh is invisible except for the new data.
    // ========================================================
    let fsWatcherKey = '';
    // Set while a targeted cross-pane transfer temporarily swaps the pane paths,
    // so the live watcher cannot re-target or repaint mid-operation.
    let fsWatcherSuspended = false;
    let fsWatcherTimer = null;
    let fsLocalRefreshBusy = false;
    let fsRemoteRefreshBusy = false;
    const fsLastRefreshAt = { local: 0, remote: 0 };

    function selectedPathsInPane(isRemote) {
      const selector = isRemote
        ? '#remoteFileList tr.selected:not(.parent-dir-row)'
        : '#localFileList tr.selected:not(.parent-dir-row)';
      const out = [];
      document.querySelectorAll(selector).forEach(r => {
        const p = r.getAttribute('data-path');
        if (p) out.push(p);
      });
      return out;
    }

    function restorePaneSelection(isRemote, paths) {
      if (!paths || !paths.length) return;
      const wanted = new Set(paths);
      const tbody = document.getElementById(isRemote ? 'remoteFileList' : 'localFileList');
      if (!tbody) return;
      const list = isRemote ? remoteFiles : localFiles;
      let firstItem = null;
      tbody.querySelectorAll('tr[data-path]').forEach(r => {
        if (!wanted.has(r.getAttribute('data-path'))) return;
        r.classList.add('selected');
        const idx = parseInt(r.getAttribute('data-idx'));
        if (firstItem === null && !isNaN(idx) && list[idx]) firstItem = list[idx];
      });
      if (firstItem) {
        if (isRemote) {
          selectedRemoteFile = firstItem;
        } else {
          selectedLocalFile = firstItem;
        }
      }
    }

    // A cached recursive folder size becomes a lie the moment anything inside
    // that folder changes, so drop every cached entry at or below `dirPath`.
    // Anything outside the refreshed directory is untouched.
    function invalidateFolderSizeCache(dirPath) {
      if (!dirPath || typeof calculatedFolderSizes === 'undefined') return;
      const norm = dirPath.replace(/[\\/]+$/, '');
      Array.from(calculatedFolderSizes.keys()).forEach(key => {
        const k = key.replace(/[\\/]+$/, '');
        if (k === norm || k.startsWith(norm + '\\') || k.startsWith(norm + '/')) {
          calculatedFolderSizes.delete(key);
        }
      });
    }

    async function silentRefreshLocal() {
      if (fsLocalRefreshBusy) return;
      const path = currentLocalPath;
      if (!path) return;
      fsLocalRefreshBusy = true;
      try {
        const tbody = document.getElementById('localFileList');
        const scroller = tbody ? tbody.parentElement : null;
        const scrollTop = scroller ? scroller.scrollTop : 0;
        const selected = selectedPathsInPane(false);
        const data = await sendRust('local_list', { path });
        if (!data || !Array.isArray(data.files)) return;
        if ((currentLocalPath || '') !== path) return;
        invalidateFolderSizeCache(path);
        renderLocalFiles(data.files);
        restorePaneSelection(false, selected);
        if (scroller) scroller.scrollTop = scrollTop;
        updatePaneStatsBadge(false);
      } catch (_) {
        // A transient failure must stay silent — the next tick retries.
      } finally {
        fsLocalRefreshBusy = false;
      }
    }

    async function silentRefreshRemote() {
      if (fsRemoteRefreshBusy) return;
      if (!isConnected) return;
      const path = currentRemotePath;
      if (!path) return;
      fsRemoteRefreshBusy = true;
      try {
        const tbody = document.getElementById('remoteFileList');
        const scroller = tbody ? tbody.parentElement : null;
        const scrollTop = scroller ? scroller.scrollTop : 0;
        const selected = selectedPathsInPane(true);
        const data = await sendRust('remote_list', { path });
        if (!data || !Array.isArray(data.files)) return;
        if ((currentRemotePath || '') !== path) return;
        invalidateFolderSizeCache(path);
        remoteDirCache.set(getRemoteDirCacheKey(path), {
          pwd: path,
          files: data.files,
          ts: Date.now()
        });
        renderRemoteFiles(data.files);
        restorePaneSelection(true, selected);
        if (scroller) scroller.scrollTop = scrollTop;
        updatePaneStatsBadge(true);
      } catch (_) {
        // Silent by design.
      } finally {
        fsRemoteRefreshBusy = false;
      }
    }

    window.onFsChanged = function (scope) {
      // Targeted drag-and-drop temporarily retargets the pane paths; a watcher
      // tick landing in that window would repaint the wrong directory.
      if (fsWatcherSuspended) return;
      const now = Date.now();
      if (scope === 'local') {
        if (now - fsLastRefreshAt.local < 300) return;
        fsLastRefreshAt.local = now;
        silentRefreshLocal();
      } else if (scope === 'remote') {
        if (now - fsLastRefreshAt.remote < 500) return;
        fsLastRefreshAt.remote = now;
        silentRefreshRemote();
      }
    };

    function updateFsWatcher(force) {
      if (fsWatcherSuspended) return;
      const localDir = currentLocalPath || '';
      const remoteDir = (isConnected && currentRemotePath) ? currentRemotePath : '';
      const key = localDir + '\u0000' + remoteDir;
      if (!force && key === fsWatcherKey) return;
      fsWatcherKey = key;
      if (fsWatcherTimer) clearTimeout(fsWatcherTimer);
      fsWatcherTimer = setTimeout(() => {
        sendRust('watch_start', {
          local_dir: localDir,
          remote_dir: remoteDir,
          interval_ms: 1200
        }).catch(() => {});
      }, 200);
    }

    // Connect & Disconnect Logic with Host Key Verification Support
    let pendingConnection = null;

    function promptHostKeyVerification(info) {
      hideConnectionLoading();
      document.getElementById('hostKeyServer').textContent = `${info.host}:${info.port}`;
      document.getElementById('hostKeyAlgorithm').textContent = info.key_type;
      document.getElementById('hostKeyFingerprint').textContent = info.fingerprint;
      document.getElementById('hostKeyModal').classList.add('active');
    }

    async function acceptHostKey() {
      document.getElementById('hostKeyModal').classList.remove('active');
      if (!pendingConnection) return;
      addLog(`Host key accepted for ${pendingConnection.host}. Saving to cache and connecting...`, 'success');
      pendingConnection.trust_host = true;
      executeConnect(pendingConnection);
    }

    function rejectHostKey() {
      document.getElementById('hostKeyModal').classList.remove('active');
      hideConnectionLoading();
      setConnectedState(false);
      addLog('Connection cancelled: Server host key was not trusted.', 'error');
      pendingConnection = null;
    }

    async function executeConnect(params) {
      stopKeepAliveTimer();
      isConnected = false;
      currentConnectedProfile = null;
      remoteFiles = [];
      selectedRemoteFile = null;
      currentRemotePath = params.remote_dir || '';
      if (typeof clearQuickFilter === 'function' && remoteQuickFilterText) {
        clearQuickFilter(true);
      }

      const rInp = document.getElementById('remotePathInput');
      if (rInp) rInp.value = `Connecting to ${params.host}...`;
      const rList = document.getElementById('remoteFileList');
      if (rList) {
        rList.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 45px;">Connecting to ${escapeHtml(params.host)}...</td></tr>`;
      }
      updatePaneStatsBadge(true);

      setConnectingState(true);
      showConnectionLoading(params);
      pendingConnection = params;

      try {
        const res = await sendRust('connect', params);

        if (res && res.status === 'host_key_prompt') {
          // Open Host Key Verification Dialog (Yes / No)
          promptHostKeyVerification(res);
          return;
        }

        isConnected = true;
        currentConnectedProfile = { ...params, remote_dir: res.pwd };
        lastConnectedProfile = { ...params, remote_dir: res.pwd };
        currentRemotePath = res.pwd;
        document.getElementById('remotePathInput').value = currentRemotePath;
        if (res.remote_history) updateRemoteHistory(res.remote_history);
        if (res.list_error) {
          addLog('Remote directory notice: ' + res.list_error, 'error');
        }
        renderRemoteFiles(res.files || [], res.list_error);
        syncCurrentRemoteTabState();
        renderRemoteTabsBar();

        const label = params.site_name ? `${params.site_name} (${params.host})` : `${params.protocol.toUpperCase()}://${params.host}`;
        setConnectedState(true, label);
        startKeepAliveTimer();
        addLog(`Connected successfully to ${params.host}! Remote directory: ${currentRemotePath}`, 'success');
        if (res.encrypted === false) {
          addLog('WARNING: this session is NOT encrypted. Credentials and file contents travel in cleartext — switch the protocol to SFTP or FTPS.', 'error');
        } else if (res.encrypted === true) {
          addLog(`Secure channel established (${params.protocol.toUpperCase()}).`, 'info');
        }
        pendingConnection = null;
        finishConnectionSuccess(res.pwd);
        try {
          if (typeof ensureTerminalActiveSession === 'function') {
            ensureTerminalActiveSession(params.host, params.username || params.user || 'root', currentRemotePath, 'remote');
          }
        } catch (termErr) {
          console.warn('Terminal session sync notice:', termErr);
        }
      } catch (err) {
        hideConnectionLoading();
        setConnectedState(false);
        pendingConnection = null;
        if (rInp) rInp.value = 'Connection failed';
        if (rList) {
          rList.innerHTML = `<tr><td colspan="4" style="text-align: center; color: #ef4444; padding: 45px;">Connection failed to ${escapeHtml(params.host)}: ${escapeHtml(err.message || String(err))}</td></tr>`;
        }
        addLog(`Connection failed: ${err.message}`, 'error');
        setTimeout(() => {
          alert('Connection Failed:\n' + err.message);
        }, 60);
      }
    }

    async function handleConnectClick() {
      const selEl = document.getElementById('quickSiteSelect');
      const selVal = selEl ? selEl.value : 'manual';
      if (selVal && selVal !== 'manual') {
        const liveSite = savedSites.find(s => s.id === selVal);
        if (liveSite) {
          selectedQuickSite = liveSite;
        }
      } else {
        selectedQuickSite = null;
      }

      if (selectedQuickSite) {
        addLog(`Connecting to saved site: ${selectedQuickSite.name} (${selectedQuickSite.host})...`, 'info');
        executeConnect({
          site_id: selectedQuickSite.id,
          protocol: selectedQuickSite.protocol,
          host: selectedQuickSite.host,
          port: selectedQuickSite.port,
          username: selectedQuickSite.username,
          password: selectedQuickSite.password,
          auth_type: selectedQuickSite.auth_type || 'password',
          key_path: selectedQuickSite.key_path,
          passphrase: selectedQuickSite.passphrase,
          remote_dir: selectedQuickSite.remote_dir || '',
          site_name: selectedQuickSite.name
        });
        return;
      }

      const protocol = document.getElementById('connProtocol').value;
      const host = document.getElementById('connHost').value.trim();
      const port = parseInt(document.getElementById('connPort').value) || 22;
      const username = document.getElementById('connUser').value.trim();
      const password = document.getElementById('connPass').value;

      if (!host) {
        alert('Please enter a Hostname or IP address');
        return;
      }

      addLog(`Connecting to ${protocol.toUpperCase()}://${host}:${port} as '${username || 'anonymous'}'...`, 'info');
      executeConnect({
        protocol,
        host,
        port,
        username,
        password,
        auth_type: 'password'
      });
    }

    async function handleDisconnect(isManual = true) {
      if (isManual && isConnected && appOptions && appOptions.confirmDisconnect) {
        if (!confirm('Are you sure you want to disconnect from the active server?')) {
          return;
        }
      }
      stopKeepAliveTimer();
      const wipe = !!(appOptions && appOptions.wipeOnDisconnect);
      if (!wipe && currentConnectedProfile) {
        if (!selectedQuickSite || isProfileMatchingSite(currentConnectedProfile, selectedQuickSite)) {
          lastConnectedProfile = { ...currentConnectedProfile, remote_dir: currentRemotePath || currentConnectedProfile.remote_dir || '/' };
        } else {
          lastConnectedProfile = null;
        }
      } else if (wipe) {
        lastConnectedProfile = null;
      }
      try {
        await sendRust('disconnect', { wipe_credentials: wipe });
      } catch (e) {}
      isConnected = false;
      currentConnectedProfile = null;
      setConnectedState(false);
      syncCurrentRemoteTabState();
      renderRemoteTabsBar();

      const keepView = !wipe && (appOptions ? appOptions.keepViewOnDisconnect !== false : true) && !!lastConnectedProfile;
      if (keepView && remoteFiles && remoteFiles.length > 0) {
        addLog('Disconnected from server. Folder view cached (click any folder or Reconnect to resume silently).', 'info');
      } else {
        remoteFiles = [];
        selectedRemoteFile = null;
        document.getElementById('remotePathInput').value = lastConnectedProfile ? (currentRemotePath || '/') : 'Not connected';
        document.getElementById('remoteFileList').innerHTML = lastConnectedProfile
          ? `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 45px;">
               <div style="display:flex; flex-direction:column; align-items:center; gap:10px;">
                 <span>Connection closed (${escapeHtml(lastConnectedProfile.host)}).</span>
                 <button type="button" class="btn" onclick="handleReconnect(true)" style="background: rgba(34, 197, 94, 0.16); color: #4ade80; border: 1px solid rgba(34, 197, 94, 0.45); font-weight: 600; padding: 5px 14px;">
                   ↻ Reconnect Now
                 </button>
               </div>
             </td></tr>`
          : `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 50px;">Disconnected from server.</td></tr>`;
        updatePaneStatsBadge(true);
        addLog('Disconnected from server.', 'info');
      }
    }

    function onBackendAutoReconnected(data) {
      const profile = currentConnectedProfile || lastConnectedProfile || selectedQuickSite;
      isConnected = true;
      if (profile && !currentConnectedProfile) {
        currentConnectedProfile = { ...profile };
      }
      if (data && data.pwd) {
        currentRemotePath = data.pwd;
        const inp = document.getElementById('remotePathInput');
        if (inp) inp.value = currentRemotePath;
      }
      const host = (profile && profile.host) || (data && data.host) || 'Server';
      const proto = ((profile && profile.protocol) || (data && data.protocol) || 'SFTP').toUpperCase();
      const label = (profile && profile.site_name) ? `${profile.site_name} (${host})` : `${proto}://${host}`;
      setConnectedState(true, label);
      startKeepAliveTimer();
      addLog(`Session automatically reconnected to ${host} in background.`, 'success');
    }

    async function ensureSilentConnected(targetPath = null) {
      if (isConnected) return { ok: true, alreadyNavigated: false };
      if (silentReconnectPromise) return silentReconnectPromise;

      const selEl = document.getElementById('quickSiteSelect');
      const selVal = selEl ? selEl.value : 'manual';
      if (selVal && selVal !== 'manual') {
        const liveSite = savedSites.find(s => s.id === selVal);
        if (liveSite) selectedQuickSite = liveSite;
      }

      const switchedToNewQuickSite = selectedQuickSite && !isProfileMatchingSite(lastConnectedProfile, selectedQuickSite);

      const profile = switchedToNewQuickSite
        ? {
            site_id: selectedQuickSite.id,
            protocol: selectedQuickSite.protocol,
            host: selectedQuickSite.host,
            port: selectedQuickSite.port,
            username: selectedQuickSite.username,
            password: selectedQuickSite.password,
            auth_type: selectedQuickSite.auth_type || 'password',
            key_path: selectedQuickSite.key_path,
            passphrase: selectedQuickSite.passphrase,
            remote_dir: selectedQuickSite.remote_dir || '',
            site_name: selectedQuickSite.name
          }
        : (lastConnectedProfile || currentConnectedProfile || (selectedQuickSite ? {
            site_id: selectedQuickSite.id,
            protocol: selectedQuickSite.protocol,
            host: selectedQuickSite.host,
            port: selectedQuickSite.port,
            username: selectedQuickSite.username,
            password: selectedQuickSite.password,
            auth_type: selectedQuickSite.auth_type || 'password',
            key_path: selectedQuickSite.key_path,
            passphrase: selectedQuickSite.passphrase,
            remote_dir: selectedQuickSite.remote_dir || '',
            site_name: selectedQuickSite.name
          } : null));

      if (!profile || !profile.host) {
        return { ok: false, alreadyNavigated: false };
      }

      const desiredDir = switchedToNewQuickSite
        ? (profile.remote_dir || '/')
        : (targetPath || currentRemotePath || profile.remote_dir || '/');
      const dot = document.getElementById('statusDot');
      const text = document.getElementById('statusText');
      const btnRec = document.getElementById('btnReconnect');
      if (dot) dot.className = 'status-dot connecting';
      if (text) text.innerText = `Connecting to ${profile.host}...`;
      if (btnRec) btnRec.disabled = true;

      silentReconnectPromise = (async () => {
        try {
          addLog(`Connecting silently to ${profile.host} (${desiredDir})...`, 'info');
          let res;
          if (switchedToNewQuickSite) {
            res = await sendRust('connect', {
              ...profile,
              remote_dir: desiredDir,
              trust_host: true
            });
          } else {
            try {
              res = await sendRust('reconnect', { path: desiredDir });
            } catch (_) {
              res = await sendRust('connect', {
                ...profile,
                remote_dir: desiredDir,
                trust_host: true
              });
            }
          }

          isConnected = true;
          currentConnectedProfile = { ...profile, remote_dir: res.pwd || desiredDir };
          lastConnectedProfile = { ...currentConnectedProfile };
          currentRemotePath = res.pwd || desiredDir;
          const inp = document.getElementById('remotePathInput');
          if (inp) inp.value = currentRemotePath;
          if (res.remote_history) updateRemoteHistory(res.remote_history);
          if (res.files) {
            renderRemoteFiles(res.files || [], res.list_error);
        syncCurrentRemoteTabState();
        renderRemoteTabsBar();
          }

          const label = profile.site_name
            ? `${profile.site_name} (${profile.host})`
            : `${(profile.protocol || 'sftp').toUpperCase()}://${profile.host}`;
          setConnectedState(true, label);
          startKeepAliveTimer();
          addLog(`Connected silently to ${profile.host} (${currentRemotePath})`, 'success');
          return { ok: true, alreadyNavigated: true, data: res };
        } catch (err) {
          setConnectedState(false);
          addLog(`Silent connect failed: ${err.message || err}`, 'error');
          return { ok: false, alreadyNavigated: false };
        } finally {
          if (btnRec) btnRec.disabled = false;
          silentReconnectPromise = null;
        }
      })();

      return silentReconnectPromise;
    }

    async function handleReconnect(silent = true, targetPath = null) {
      closeAllMenus();
      const selEl = document.getElementById('quickSiteSelect');
      const selVal = selEl ? selEl.value : 'manual';
      if (selVal && selVal !== 'manual') {
        const liveSite = savedSites.find(s => s.id === selVal);
        if (liveSite) selectedQuickSite = liveSite;
      }

      if (selectedQuickSite && !isProfileMatchingSite(lastConnectedProfile, selectedQuickSite)) {
        handleConnectClick();
        return;
      }

      if (silent && (lastConnectedProfile || currentConnectedProfile || selectedQuickSite)) {
        const res = await ensureSilentConnected(targetPath || currentRemotePath);
        if (res && res.ok) return;
      }
      handleConnectClick();
    }

    function stopKeepAliveTimer() {
      if (keepAliveTimer) {
        clearInterval(keepAliveTimer);
        keepAliveTimer = null;
      }
    }

    function startKeepAliveTimer() {
      stopKeepAliveTimer();
      if (appOptions && appOptions.keepAliveEnabled === false) return;
      const sec = Math.max(10, parseInt((appOptions && appOptions.keepAliveInterval) || 25, 10) || 25);
      keepAliveTimer = setInterval(async () => {
        if (!isConnected) return;
        try {
          const res = await sendRust('ping_keepalive', {
            auto_reconnect: !!(appOptions && appOptions.autoReconnectOnClick !== false)
          });
          if (res && res.reconnected && res.files) {
            renderRemoteFiles(res.files);
          }
        } catch (err) {
          console.warn('Keep-alive heartbeat notice:', err);
          if (appOptions && appOptions.autoReconnectOnClick === false) {
            isConnected = false;
            setConnectedState(false);
          }
        }
      }, sec * 1000);
    }

    function restartKeepAliveTimer() {
      if (isConnected) {
        startKeepAliveTimer();
      } else {
        stopKeepAliveTimer();
      }
    }

    function toggleSyncBrowsingToolbar() {
      if (!appOptions) return;
      appOptions.syncBrowsing = !appOptions.syncBrowsing;
      try {
        appStorage.setItem('90ftp_options', JSON.stringify(appOptions));
      } catch (_) {}
      updateSyncBrowsingToolbarUI();
      addLog(`Synchronized Browsing ${appOptions.syncBrowsing ? 'ENABLED (Local & Remote folders locked)' : 'DISABLED'}`, 'info');
    }

    function updateSyncBrowsingToolbarUI() {
      const btn = document.getElementById('btnToolbarSyncBrowsing');
      if (!btn) return;
      const active = !!(appOptions && appOptions.syncBrowsing);
      btn.style.color = active ? '#22c55e' : 'var(--text-dim)';
      btn.style.borderColor = active ? 'rgba(34, 197, 94, 0.45)' : '';
      btn.style.background = active ? 'rgba(34, 197, 94, 0.12)' : '';
      btn.title = active
        ? 'Synchronized Browsing: ACTIVE (Click to disable)'
        : 'Toggle Synchronized Browsing (Lock Local & Remote folder navigation)';
    }

    function setConnectingState(isConnecting) {
      const dot = document.getElementById('statusDot');
      const text = document.getElementById('statusText');
      if (isConnecting) {
        dot.className = 'status-dot connecting';
        text.innerText = 'Connecting...';
      }
    }

    function setConnectedState(connected, title = '') {
      const dot = document.getElementById('statusDot');
      const text = document.getElementById('statusText');
      const btnConn = document.getElementById('btnConnect');
      const btnRec = document.getElementById('btnReconnect');
      const btnDisc = document.getElementById('btnDisconnect');
      const btnToolRec = document.getElementById('btnToolbarReconnect');
      const menuRec = document.getElementById('menuItemReconnect');
      const menuDisc = document.getElementById('menuItemDisconnect');
      const termBadge = document.getElementById('terminalBadge');

      if (termBadge) {
        termBadge.style.display = 'inline-block';
        termBadge.textContent = connected ? 'SSH Online' : 'Local PS';
        termBadge.style.color = connected ? '#22c55e' : '#a855f7';
        termBadge.style.background = connected ? 'rgba(34, 197, 94, 0.18)' : 'rgba(168, 85, 247, 0.18)';
      }

      if (connected) {
        dot.className = 'status-dot connected';
        text.innerText = 'Connected: ' + title;
        btnConn.style.display = 'none';
        if (btnRec) btnRec.style.display = 'none';
        btnDisc.style.display = 'inline-flex';
        if (btnToolRec) btnToolRec.style.display = 'none';
        if (menuRec) menuRec.style.display = 'none';
        if (menuDisc) menuDisc.style.display = 'flex';
      } else {
        const hasMatchingPrev = !!(lastConnectedProfile && lastConnectedProfile.host && (!selectedQuickSite || isProfileMatchingSite(lastConnectedProfile, selectedQuickSite)));
        dot.className = 'status-dot';
        text.innerText = hasMatchingPrev
          ? `Disconnected (${lastConnectedProfile.host})`
          : (selectedQuickSite ? `Ready: ${selectedQuickSite.name} (${selectedQuickSite.host})` : 'Disconnected');
        btnDisc.style.display = 'none';
        if (hasMatchingPrev) {
          if (btnRec) btnRec.style.display = 'inline-flex';
          btnConn.style.display = 'none';
          if (btnToolRec) btnToolRec.style.display = 'inline-flex';
          if (menuRec) menuRec.style.display = 'flex';
          if (menuDisc) menuDisc.style.display = 'none';
        } else {
          if (btnRec) btnRec.style.display = 'none';
          btnConn.style.display = 'inline-flex';
          if (btnToolRec) btnToolRec.style.display = 'none';
          if (menuRec) menuRec.style.display = 'none';
          if (menuDisc) menuDisc.style.display = 'flex';
        }
      }
      if (typeof syncCurrentRemoteTabState === 'function') {
        syncCurrentRemoteTabState();
      }
      // Start or stop remote polling as soon as the connection state flips.
      if (typeof updateFsWatcher === 'function') {
        updateFsWatcher(true);
      }
    }

    function updateDefaultPort() {
      const proto = document.getElementById('connProtocol').value;
      const portInput = document.getElementById('connPort');
      portInput.value = proto === 'sftp' ? '22' : '21';
    }

    function onSiteProtocolChange() {
      const proto = document.getElementById('siteProtocol').value;
      const portInput = document.getElementById('sitePort');
      portInput.value = proto === 'sftp' ? '22' : '21';
    }

    // Transfers & Drag-Drop Helpers
    function disconnect() {
      handleDisconnect();
    }

    // Interactive File Collision Resolution System
    let pendingCollisionResolver = null;
    let transferCollisionScope = {
      applyAllAction: null,
      cancelled: false
    };

    function resetCollisionScope() {
      transferCollisionScope = {
        applyAllAction: null,
        cancelled: false
      };
    }

    function resolveCollisionModal(action) {
      const modal = document.getElementById('collisionModal');
      if (modal) modal.classList.remove('active');
      const applyAll = document.getElementById('colApplyAllCheckbox')?.checked || false;

      if (action === 'overwrite_all') {
        action = 'overwrite';
        transferCollisionScope.applyAllAction = 'overwrite';
      } else if (action === 'skip_all') {
        action = 'skip';
        transferCollisionScope.applyAllAction = 'skip';
      } else if (applyAll) {
        transferCollisionScope.applyAllAction = action;
      }

      if (action === 'cancel') {
        transferCollisionScope.cancelled = true;
      }

      if (pendingCollisionResolver) {
        const resolver = pendingCollisionResolver;
        pendingCollisionResolver = null;
        resolver({ action, applyAll });
      }
    }

    async function promptCollision(srcInfo, destInfo, direction) {
      if (transferCollisionScope.cancelled) {
        return { action: 'cancel' };
      }
      if (transferCollisionScope.applyAllAction) {
        return { action: transferCollisionScope.applyAllAction };
      }

      const modalTitle = document.getElementById('collisionModalTitle');
      const modalSubtitle = document.getElementById('collisionModalSubtitle');
      if (modalTitle) {
        modalTitle.textContent = direction === 'upload' 
          ? 'Remote File Collision: Target Already Exists' 
          : 'Local File Collision: Target Already Exists';
      }
      if (modalSubtitle) {
        modalSubtitle.textContent = direction === 'upload'
          ? 'The destination server already contains a file with this name.'
          : 'Your local destination folder already contains a file with this name.';
      }

      document.getElementById('colSourceName').textContent = srcInfo.name || '-';
      document.getElementById('colSourcePath').textContent = srcInfo.path || '-';
      document.getElementById('colSourceSize').textContent = formatBytes(srcInfo.size || 0);
      document.getElementById('colSourceDate').textContent = srcInfo.modified || '-';

      document.getElementById('colDestName').textContent = destInfo.name || '-';
      document.getElementById('colDestPath').textContent = destInfo.path || '-';
      document.getElementById('colDestSize').textContent = formatBytes(destInfo.size || 0);
      document.getElementById('colDestDate').textContent = destInfo.modified || '-';

      const chk = document.getElementById('colApplyAllCheckbox');
      if (chk) chk.checked = false;

      const diffEl = document.getElementById('colDiffText');
      const sSize = Number(srcInfo.size || 0);
      const dSize = Number(destInfo.size || 0);

      let dateComparison = '';
      if (srcInfo.modified && destInfo.modified && srcInfo.modified !== '-' && destInfo.modified !== '-') {
        const sDate = new Date(srcInfo.modified).getTime();
        const dDate = new Date(destInfo.modified).getTime();
        if (!isNaN(sDate) && !isNaN(dDate)) {
          if (sDate > dDate) {
            dateComparison = 'Source is newer.';
          } else if (sDate < dDate) {
            dateComparison = 'Destination is newer.';
          } else {
            dateComparison = 'Same modification timestamp.';
          }
        }
      }

      let sizeComparison = '';
      if (sSize > dSize) {
        sizeComparison = `Source is larger (+${formatBytes(sSize - dSize)}).`;
      } else if (sSize < dSize) {
        sizeComparison = `Source is smaller (-${formatBytes(dSize - sSize)}).`;
      } else {
        sizeComparison = 'Identical size.';
      }

      const diffMsg = [dateComparison, sizeComparison].filter(Boolean).join(' ') || 'File already exists at destination.';
      if (diffEl) diffEl.textContent = diffMsg;

      const modal = document.getElementById('collisionModal');
      if (modal) modal.classList.add('active');

      return new Promise((resolve) => {
        pendingCollisionResolver = resolve;
      });
    }

    function generateAlternativeFilename(originalName) {
      const lastDot = originalName.lastIndexOf('.');
      if (lastDot > 0) {
        const base = originalName.substring(0, lastDot);
        const ext = originalName.substring(lastDot);
        return `${base} (1)${ext}`;
      }
      return `${originalName} (1)`;
    }

    async function uploadFolderDirect(localFolderPath, folderName) {
      if (!isConnected) {
        alert('Please connect to a server first');
        return;
      }
      addLog(`Scanning local folder '${folderName}' for upload...`, 'cmd');
      try {
        const rules = typeof getActiveExclusionRules === 'function' ? getActiveExclusionRules() : [];
        const scanRes = await sendRust('local_scan_dir_recursive', {
          dir_path: localFolderPath,
          exclude_patterns: rules
        });
        const items = scanRes.items || [];
        const skipped = scanRes.skipped_count || 0;
        if (skipped > 0) {
          addLog(`[Exclusion Mask] Skipped ${skipped} item(s) matching exclusion rules`, 'info');
        }
        const baseRemote = currentRemotePath.endsWith('/') 
          ? currentRemotePath + folderName 
          : currentRemotePath + '/' + folderName;

        // The root folder must exist before any file is queued into it; the
        // queue itself only moves files.
        try {
          await sendRust('remote_create_dir', { path: baseRemote });
        } catch (_) {}

        if (items.length === 0) {
          addLog(`Created empty remote directory: ${baseRemote}`, 'success');
          refreshRemote();
          return;
        }

        addLog(`Starting recursive upload of folder '${folderName}' (${items.length} items)...`, 'info');
        let uploadedFiles = 0;
        let createdDirs = 0;
        resetCollisionScope();

        for (const item of items) {
          if (transferCollisionScope.cancelled) {
            addLog('Folder upload cancelled by user.', 'warning');
            break;
          }
          let itemDest = baseRemote + '/' + item.rel_path;
          if (item.is_dir) {
            try {
              await sendRust('remote_create_dir', { path: itemDest });
              createdDirs++;
            } catch (dirErr) {
              addLog(`Could not create remote folder ${itemDest}: ${dirErr.message || dirErr}`, 'error');
            }
          } else {
            let fileName = item.rel_path.split('/').pop();

            // Collision check on remote
            let remoteExists = false;
            let destMeta = null;
            try {
              const chk = await sendRust('remote_stat', { path: itemDest });
              if (chk && chk.exists && chk.item) {
                remoteExists = true;
                destMeta = chk.item;
              }
            } catch (_) {}

            if (remoteExists && destMeta) {
              const srcMeta = { name: fileName, path: item.abs_path, size: item.size, modified: '-' };
              const decision = await promptCollision(srcMeta, destMeta, 'upload');
              if (decision.action === 'cancel') {
                addLog('Folder upload cancelled by user.', 'warning');
                break;
              }
              if (decision.action === 'skip') {
                addLog(`[Skip] ${item.rel_path} skipped (already exists on remote)`, 'info');
                continue;
              }
              if (decision.action === 'overwrite_newer') {
                if (destMeta.size === item.size) {
                  addLog(`[Skip] ${item.rel_path} skipped (identical size on remote)`, 'info');
                  continue;
                }
              }
              if (decision.action === 'rename') {
                const alt = generateAlternativeFilename(fileName);
                itemDest = itemDest.substring(0, itemDest.lastIndexOf('/') + 1) + alt;
                fileName = alt;
              }
            }

            // Queued rather than awaited: the queue runs them with the
            // configured parallelism, so a folder of 200 files no longer uploads
            // strictly one at a time while the UI sits idle.
            const queued = await enqueueTransfer('upload', item.abs_path, itemDest);
            if (queued) uploadedFiles++;
          }
        }

        refreshRemote();
        addLog(`Folder upload queued: '${folderName}' (${uploadedFiles} files, ${createdDirs} folders) — progress is in the File Transfers tab`, 'success');
      } catch (err) {
        addLog(`Folder upload error: ${err.message || err}`, 'error');
        alert(`Folder Upload Failed:\n${err.message || err}`);
      }
    }

    async function downloadFolderDirect(remoteFolderPath, folderName) {
      addLog(`Scanning remote folder '${folderName}' for download...`, 'cmd');
      try {
        const rules = typeof getActiveExclusionRules === 'function' ? getActiveExclusionRules() : [];
        const scanRes = await sendRust('remote_scan_dir_recursive', {
          remote_dir: remoteFolderPath,
          exclude_patterns: rules
        });
        const items = scanRes.items || [];
        const skipped = scanRes.skipped_count || 0;
        if (skipped > 0) {
          addLog(`[Exclusion Mask] Skipped ${skipped} item(s) matching exclusion rules`, 'info');
        }
        const baseLocal = currentLocalPath.endsWith('\\') 
          ? currentLocalPath + folderName 
          : currentLocalPath + '\\' + folderName;

        // The destination tree has to exist before any file is queued into it.
        try {
          await sendRust('local_create_dir', { path: baseLocal });
        } catch (_) {}

        if (items.length === 0) {
          addLog(`Created empty local directory: ${baseLocal}`, 'success');
          refreshLocal();
          return;
        }

        addLog(`Starting recursive download of folder '${folderName}' (${items.length} items)...`, 'info');
        let downloadedFiles = 0;
        let createdDirs = 0;
        resetCollisionScope();

        for (const item of items) {
          if (transferCollisionScope.cancelled) {
            addLog('Folder download cancelled by user.', 'warning');
            break;
          }
          const localRel = item.rel_path.replace(/\//g, '\\');
          let itemDest = baseLocal + '\\' + localRel;

          if (item.is_dir) {
            try {
              await sendRust('local_create_dir', { path: itemDest });
              createdDirs++;
            } catch (dirErr) {
              addLog(`Could not create local folder ${itemDest}: ${dirErr.message || dirErr}`, 'error');
            }
          } else {
            // Ensure parent directory exists locally
            const parentDir = itemDest.substring(0, itemDest.lastIndexOf('\\'));
            try {
              await sendRust('local_create_dir', { path: parentDir });
            } catch (_) {}

            let fileName = item.rel_path.split('/').pop();

            // Collision check locally
            let localExists = false;
            let destMeta = null;
            try {
              const chk = await sendRust('local_stat', { path: itemDest });
              if (chk && chk.exists) {
                localExists = true;
                destMeta = chk;
              }
            } catch (_) {}

            if (localExists && destMeta) {
              const srcMeta = { name: fileName, path: item.remote_path, size: item.size, modified: '-' };
              const decision = await promptCollision(srcMeta, destMeta, 'download');
              if (decision.action === 'cancel') {
                addLog('Folder download cancelled by user.', 'warning');
                break;
              }
              if (decision.action === 'skip') {
                addLog(`[Skip] ${item.rel_path} skipped (already exists locally)`, 'info');
                continue;
              }
              if (decision.action === 'overwrite_newer') {
                if (destMeta.size === item.size) {
                  addLog(`[Skip] ${item.rel_path} skipped (identical size locally)`, 'info');
                  continue;
                }
              }
              if (decision.action === 'rename') {
                const alt = generateAlternativeFilename(fileName);
                itemDest = itemDest.substring(0, itemDest.lastIndexOf('\\') + 1) + alt;
                fileName = alt;
              }
            }

            const queued = await enqueueTransfer('download', itemDest, item.remote_path);
            if (queued) downloadedFiles++;
          }
        }

        refreshLocal();
        addLog(`Folder download queued: '${folderName}' (${downloadedFiles} files, ${createdDirs} folders) — progress is in the File Transfers tab`, 'success');
      } catch (err) {
        addLog(`Folder download error: ${err.message || err}`, 'error');
        alert(`Folder Download Failed:\n${err.message || err}`);
      }
    }

    async function uploadFileDirect(localPath, fileName) {
      if (!isConnected) {
        alert('Please connect to a server first');
        return;
      }

      if (typeof isItemExcluded === 'function' && isItemExcluded(fileName, fileName)) {
        if (!confirm(`File '${fileName}' matches your active Transfer Exclusion Mask rules (.env, node_modules, etc.).\n\nDo you really want to upload it?`)) {
          addLog(`[Exclusion Mask] Upload skipped for: ${fileName}`, 'info');
          return;
        }
      }

      let remoteDest = currentRemotePath.endsWith('/') 
        ? currentRemotePath + fileName 
        : currentRemotePath + '/' + fileName;

      // Check remote collision
      let existingRemote = remoteFiles.find(f => f.name.toLowerCase() === fileName.toLowerCase());
      if (!existingRemote) {
        try {
          const statRes = await sendRust('remote_stat', { path: remoteDest });
          if (statRes && statRes.exists && statRes.item) {
            existingRemote = statRes.item;
          }
        } catch (_) {}
      }

      if (existingRemote) {
        let srcStat = { name: fileName, path: localPath, size: selectedLocalFile?.size || 0, modified: selectedLocalFile?.modified || '-' };
        if (!srcStat.size) {
          try {
            const ls = await sendRust('local_stat', { path: localPath });
            if (ls && ls.exists) srcStat = ls;
          } catch (_) {}
        }

        const decision = await promptCollision(srcStat, existingRemote, 'upload');
        if (decision.action === 'cancel') {
          addLog("Upload cancelled by user: " + fileName, 'info');
          return;
        }
        if (decision.action === 'skip') {
          addLog("Skipped upload for existing file: " + fileName, 'info');
          return;
        }
        if (decision.action === 'overwrite_newer') {
          const sTime = new Date(srcStat.modified).getTime();
          const dTime = new Date(existingRemote.modified).getTime();
          if (!isNaN(sTime) && !isNaN(dTime) && sTime <= dTime && srcStat.size === existingRemote.size) {
            addLog("Skipped upload (destination is already up to date): " + fileName, 'info');
            return;
          }
        }
        if (decision.action === 'rename') {
          const altName = generateAlternativeFilename(fileName);
          remoteDest = currentRemotePath.endsWith('/') 
            ? currentRemotePath + altName 
            : currentRemotePath + '/' + altName;
          fileName = altName;
        }
      }

      addLog("Queued upload: '" + fileName + "' to " + remoteDest, 'cmd');
      // The queue owns the transfer from here: real byte progress, retries and a
      // per-row pause all come from the backend, so nothing is simulated.
      await enqueueTransfer('upload', localPath, remoteDest);
    }

    async function downloadFileDirect(remotePath, fileName) {
      let localDest = currentLocalPath.endsWith('\\') 
        ? currentLocalPath + fileName 
        : currentLocalPath + '\\' + fileName;

      // Check local collision
      let existingLocal = localFiles.find(f => f.name.toLowerCase() === fileName.toLowerCase());
      if (!existingLocal) {
        try {
          const statRes = await sendRust('local_stat', { path: localDest });
          if (statRes && statRes.exists) {
            existingLocal = statRes;
          }
        } catch (_) {}
      }

      if (existingLocal) {
        let srcStat = { name: fileName, path: remotePath, size: selectedRemoteFile?.size || 0, modified: selectedRemoteFile?.modified || '-' };
        if (!srcStat.size) {
          try {
            const rs = await sendRust('remote_stat', { path: remotePath });
            if (rs && rs.exists && rs.item) srcStat = rs.item;
          } catch (_) {}
        }

        const decision = await promptCollision(srcStat, existingLocal, 'download');
        if (decision.action === 'cancel') {
          addLog("Download cancelled by user: " + fileName, 'info');
          return;
        }
        if (decision.action === 'skip') {
          addLog("Skipped download for existing file: " + fileName, 'info');
          return;
        }
        if (decision.action === 'overwrite_newer') {
          const sTime = new Date(srcStat.modified).getTime();
          const dTime = new Date(existingLocal.modified).getTime();
          if (!isNaN(sTime) && !isNaN(dTime) && sTime <= dTime && srcStat.size === existingLocal.size) {
            addLog("Skipped download (local file is already up to date): " + fileName, 'info');
            return;
          }
        }
        if (decision.action === 'rename') {
          const altName = generateAlternativeFilename(fileName);
          localDest = currentLocalPath.endsWith('\\') 
            ? currentLocalPath + altName 
            : currentLocalPath + '\\' + altName;
          fileName = altName;
        }
      }

      addLog("Queued download: '" + fileName + "' to " + localDest, 'cmd');
      await enqueueTransfer('download', localDest, remotePath);
    }



    // Code Editor & Formatter Suite
    let editorFontSize = 13;
    let findMatches = [];
    let currentMatchIdx = -1;
    let isEditorMaximized = false;

    function toggleEditorMaximize() {
      const frame = document.getElementById('editorWindowFrame');
      if (!frame) return;
      isEditorMaximized = !isEditorMaximized;
      if (isEditorMaximized) {
        frame.style.width = '100vw';
        frame.style.maxWidth = '100vw';
        frame.style.height = '100vh';
        frame.style.maxHeight = '100vh';
        frame.style.borderRadius = '0';
      } else {
        frame.style.width = '1060px';
        frame.style.maxWidth = '96vw';
        frame.style.height = '86vh';
        frame.style.maxHeight = '96vh';
        frame.style.borderRadius = '8px';
      }
    }

    function updateEditorCursorPos() {
      const ta = document.getElementById('editorTextarea');
      const posEl = document.getElementById('editorCursorPos');
      if (!ta || !posEl) return;
      const selStart = ta.selectionStart;
      const val = ta.value.substring(0, selStart);
      const lines = val.split('\n');
      const row = lines.length;
      const col = lines[lines.length - 1].length + 1;
      posEl.innerText = 'Ln ' + row + ', Col ' + col;
    }

    function cancelPendingConnection() {
      const overlay = document.getElementById('connectionLoadingOverlay');
      if (overlay) overlay.classList.remove('active');
      addLog('Connection cancelled by user.', 'info');
    }

    async function viewErrorLogs() {
      try {
        const res = await sendRust('get_error_logs');
        if (!res || !res.logs) {
          alert('Error log file is currently empty or not found.\nLocation: ' + (res?.path || 'app data'));
          return;
        }
        activeEditingPath = res.path || '90ftp_errors.log';
        activeEditingIsRemote = false;
        document.getElementById('editorTitle').innerText = 'Log: ' + activeEditingPath;
        document.getElementById('editorTextarea').value = res.logs;
        document.getElementById('editorStatus').innerText = 'Persistent Error Log';
        onEditorInput();
        // The tools window stays open behind the editor so closing it lands on
        // the tool's page instead of dismissing the whole suite. The editor
        // sits at the default overlay depth, which is below the tools window,
        // so it has to be lifted while it is the one on top.
        const editorEl = document.getElementById('editorModal');
        if (editorEl) {
          const toolsEl = document.getElementById('toolsModal');
          const toolsOpen = !!toolsEl && toolsEl.classList.contains('active');
          editorEl.style.zIndex = toolsOpen ? '1085' : '';
          editorEl.classList.add('active');
        }
      } catch (err) {
        addLog('Failed to read error logs: ' + (err.message || err), 'error');
        alert('Could not read error logs: ' + (err.message || err));
      }
    }

    function onEditorInput() {
      updateEditorStats();
      updateEditorLineNumbers();
      updateEditorCursorPos();
    }

    function updateEditorStats() {
      const textarea = document.getElementById('editorTextarea');
      const text = textarea ? textarea.value : '';
      const lines = text ? text.split('\n').length : 0;
      const chars = text.length;
      const words = text.trim() ? text.trim().split(/\s+/).length : 0;
      const statsEl = document.getElementById('editorStats');
      if (statsEl) {
        statsEl.innerText = 'Lines: ' + lines + ' | Words: ' + words + ' | Chars: ' + chars.toLocaleString();
      }
    }

    function updateEditorLineNumbers() {
      const ta = document.getElementById('editorTextarea');
      const gutter = document.getElementById('editorGutter');
      if (!ta || !gutter) return;
      const lineCount = ta.value.split('\n').length;
      let s = '';
      for (let i = 1; i <= lineCount; i++) {
        s += i + '\n';
      }
      gutter.textContent = s;
    }

    function syncEditorScroll() {
      const ta = document.getElementById('editorTextarea');
      const gutter = document.getElementById('editorGutter');
      if (ta && gutter) {
        gutter.scrollTop = ta.scrollTop;
      }
    }

    function toggleWordWrap() {
      const ta = document.getElementById('editorTextarea');
      const btn = document.getElementById('btnWordWrap');
      if (!ta) return;
      const isWrapped = ta.style.whiteSpace === 'pre-wrap';
      ta.style.whiteSpace = isWrapped ? 'pre' : 'pre-wrap';
      if (btn) btn.classList.toggle('editor-btn-active', !isWrapped);
    }

    function changeEditorFontSize(delta) {
      editorFontSize = Math.min(22, Math.max(10, editorFontSize + delta));
      const ta = document.getElementById('editorTextarea');
      const gutter = document.getElementById('editorGutter');
      if (ta) ta.style.fontSize = editorFontSize + 'px';
      if (gutter) gutter.style.fontSize = editorFontSize + 'px';
    }

    function convertEditorCase(type) {
      const ta = document.getElementById('editorTextarea');
      if (!ta) return;
      const start = ta.selectionStart;
      const end = ta.selectionEnd;
      const text = ta.value;
      const selected = text.substring(start, end);
      if (!selected) {
        ta.value = type === 'upper' ? text.toUpperCase() : text.toLowerCase();
      } else {
        const rep = type === 'upper' ? selected.toUpperCase() : selected.toLowerCase();
        ta.value = text.substring(0, start) + rep + text.substring(end);
        ta.setSelectionRange(start, start + rep.length);
      }
      onEditorInput();
    }

    function toggleFindBar(showReplace = false) {
      const bar = document.getElementById('editorFindBar');
      if (!bar) return;
      const isHidden = bar.style.display === 'none' || bar.style.display === '';
      if (isHidden) {
        bar.style.display = 'flex';
        document.getElementById('findReplaceGroup').style.display = showReplace ? 'flex' : 'none';
        const inp = document.getElementById('editorFindInput');
        if (inp) {
          inp.focus();
          inp.select();
        }
        executeFind();
      } else {
        if (showReplace && document.getElementById('findReplaceGroup').style.display === 'none') {
          document.getElementById('findReplaceGroup').style.display = 'flex';
          document.getElementById('editorReplaceInput').focus();
        } else {
          closeFindBar();
        }
      }
    }

    function closeFindBar() {
      const bar = document.getElementById('editorFindBar');
      if (bar) bar.style.display = 'none';
      const ta = document.getElementById('editorTextarea');
      if (ta) ta.focus();
    }

    function toggleMatchCase() {
      const btn = document.getElementById('findMatchCase');
      if (btn) {
        btn.classList.toggle('editor-btn-active');
        executeFind(0);
      }
    }

    function handleFindInputKey(e) {
      if (e.key === 'Enter') {
        executeFind(e.shiftKey ? -1 : 1);
      } else if (e.key === 'Escape') {
        closeFindBar();
      } else {
        setTimeout(() => executeFind(0), 10);
      }
    }

    function executeFind(dir = 0) {
      const query = document.getElementById('editorFindInput').value;
      const matchCase = document.getElementById('findMatchCase').classList.contains('editor-btn-active');
      const countEl = document.getElementById('findCountBadge');
      const ta = document.getElementById('editorTextarea');
      if (!query || !ta) {
        if (countEl) countEl.textContent = 'No text';
        return;
      }
      const text = ta.value;
      const regexFlags = matchCase ? 'g' : 'gi';
      const escaped = query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const regex = new RegExp(escaped, regexFlags);

      findMatches = [];
      let m;
      while ((m = regex.exec(text)) !== null) {
        findMatches.push({ start: m.index, end: m.index + m[0].length });
      }

      if (findMatches.length === 0) {
        if (countEl) countEl.textContent = '0 of 0';
        currentMatchIdx = -1;
        return;
      }

      if (dir === 1) {
        currentMatchIdx = (currentMatchIdx + 1) % findMatches.length;
      } else if (dir === -1) {
        currentMatchIdx = (currentMatchIdx - 1 + findMatches.length) % findMatches.length;
      } else {
        const curPos = ta.selectionStart;
        const nextIdx = findMatches.findIndex(item => item.start >= curPos);
        currentMatchIdx = nextIdx !== -1 ? nextIdx : 0;
      }

      const match = findMatches[currentMatchIdx];
      ta.focus();
      ta.setSelectionRange(match.start, match.end);

      const linesBefore = text.substring(0, match.start).split('\n').length;
      const lineHeight = 20.8;
      ta.scrollTop = Math.max(0, (linesBefore - 6) * lineHeight);

      if (countEl) countEl.textContent = (currentMatchIdx + 1) + ' of ' + findMatches.length;
    }

    function executeReplace() {
      const ta = document.getElementById('editorTextarea');
      const repVal = document.getElementById('editorReplaceInput').value;
      if (currentMatchIdx >= 0 && currentMatchIdx < findMatches.length) {
        const match = findMatches[currentMatchIdx];
        const text = ta.value;
        ta.value = text.substring(0, match.start) + repVal + text.substring(match.end);
        onEditorInput();
        executeFind(0);
      }
    }

    function executeReplaceAll() {
      const query = document.getElementById('editorFindInput').value;
      const repVal = document.getElementById('editorReplaceInput').value;
      const matchCase = document.getElementById('findMatchCase').classList.contains('editor-btn-active');
      const ta = document.getElementById('editorTextarea');
      if (!query || !ta) return;
      const escaped = query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const regex = new RegExp(escaped, matchCase ? 'g' : 'gi');
      ta.value = ta.value.replace(regex, repVal);
      onEditorInput();
      executeFind(0);
    }

    function formatEditorCode() {
      const textarea = document.getElementById('editorTextarea');
      const code = textarea.value;
      const status = document.getElementById('editorStatus');
      if (!code.trim()) return;

      const ext = (activeEditingPath.split('.').pop() || '').toLowerCase();

      // JSON Beautifier
      if (ext === 'json' || code.trim().startsWith('{') || code.trim().startsWith('[')) {
        try {
          const parsed = JSON.parse(code);
          textarea.value = JSON.stringify(parsed, null, 2);
          onEditorInput();
          status.innerText = 'JSON Formatted ✓';
          setTimeout(() => status.innerText = '', 2000);
          return;
        } catch (_) {}
      }

      // XML / HTML / SVG Beautifier
      if (['html', 'htm', 'xml', 'svg'].includes(ext) || code.trim().startsWith('<')) {
        try {
          textarea.value = formatXmlHtml(code);
          onEditorInput();
          status.innerText = 'HTML/XML Formatted ✓';
          setTimeout(() => status.innerText = '', 2000);
          return;
        } catch (_) {}
      }

      // Generic C/JS/CSS Formatter
      textarea.value = formatGenericCode(code);
      onEditorInput();
      status.innerText = 'Code Formatted ✓';
      setTimeout(() => status.innerText = '', 2000);
    }

    function formatXmlHtml(xml) {
      let formatted = '';
      let indent = 0;
      const tab = '  ';
      const nodes = xml.split(/>\s*</);
      nodes.forEach(function(node) {
        if (node.match(/^\/\w/)) indent = Math.max(0, indent - 1);
        formatted += tab.repeat(indent) + '<' + node + '>\r\n';
        if (node.match(/^<?\w[^>]*[^\/]$/) && !node.startsWith("input") && !node.startsWith("img") && !node.startsWith("br") && !node.startsWith("hr") && !node.startsWith("meta") && !node.startsWith("link")) {
          indent++;
        }
      });
      return formatted.substring(1, formatted.length - 3);
    }

    function formatGenericCode(code) {
      const lines = code.split('\n');
      let indent = 0;
      const tab = '  ';
      const out = [];

      for (let rawLine of lines) {
        let line = rawLine.trim();
        if (!line) {
          out.push('');
          continue;
        }

        if (line.startsWith('}') || line.startsWith(']') || line.startsWith(')')) {
          indent = Math.max(0, indent - 1);
        }

        out.push(tab.repeat(indent) + line);

        const opens = (line.match(/[{([]/g) || []).length;
        const closes = (line.match(/[})]]/g) || []).length;
        indent += Math.max(0, opens - closes);
      }
      return out.join('\n');
    }

    async function openRemoteFileInEditor(path) {
      addLog("Loading remote file: " + path + "...", 'cmd');
      try {
        const res = await sendRust('remote_read', { path });
        activeEditingPath = path;
        activeEditingIsRemote = true;
        document.getElementById('editorTitle').innerText = 'Remote: ' + path;
        document.getElementById('editorTextarea').value = res.content;
        document.getElementById('editorStatus').innerText = 'Loaded (' + formatBytes(res.size) + ')';
        onEditorInput();
        document.getElementById('editorModal').classList.add('active');
      } catch (err) {
        addLog('Cannot open remote file: ' + (err.message || err), 'error');
        alert('Failed to open file:\n' + (err.message || err));
      }
    }

    async function openLocalFileInEditor(path) {
      try {
        const res = await sendRust('local_read', { path });
        activeEditingPath = path;
        activeEditingIsRemote = false;
        document.getElementById('editorTitle').innerText = 'Local: ' + path;
        document.getElementById('editorTextarea').value = res.content;
        document.getElementById('editorStatus').innerText = 'Local File';
        onEditorInput();
        document.getElementById('editorModal').classList.add('active');
      } catch (err) {
        alert('Cannot open file:\n' + (err.message || err));
      }
    }

    async function saveEditorContent() {
      const content = document.getElementById('editorTextarea').value;
      const status = document.getElementById('editorStatus');
      status.innerText = 'Saving...';

      try {
        if (activeEditingIsRemote) {
          await sendRust('remote_write', { path: activeEditingPath, content });
          addLog("Saved and uploaded changes to " + activeEditingPath, 'success');
        } else {
          await sendRust('local_write', { path: activeEditingPath, content });
          addLog("Saved local file " + activeEditingPath, 'success');
        }
        status.innerText = 'Saved successfully ✓';
        setTimeout(() => status.innerText = '', 2000);
      } catch (err) {
        status.innerText = 'Save error!';
        addLog('Save failed: ' + (err.message || err), 'error');
        alert('Error saving file:\n' + (err.message || err));
      }
    }

    function closeEditorModal() {
      closeFindBar();
      document.getElementById('editorModal').classList.remove('active');
    }

    const editorAreaEl = document.getElementById('editorTextarea');
    if (editorAreaEl) editorAreaEl.addEventListener('keydown', function(e) {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
        e.preventDefault();
        saveEditorContent();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
        e.preventDefault();
        toggleFindBar(false);
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'h') {
        e.preventDefault();
        toggleFindBar(true);
      } else if (e.key === 'Escape') {
        closeFindBar();
      } else if (e.key === 'Tab') {
        e.preventDefault();
        const start = this.selectionStart;
        const end = this.selectionEnd;
        this.value = this.value.substring(0, start) + '  ' + this.value.substring(end);
        this.selectionStart = this.selectionEnd = start + 2;
        onEditorInput();
      }
    });

    // Custom Context Menu & Properties Management
    let contextTarget = null;

    function getVerbIconSvg(name) {
      const lower = name.toLowerCase();
      if (lower === 'open' || lower === 'open in new process') {
        return '<svg class="icon" viewBox="0 0 24 24"><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/></svg>';
      }
      if (lower.includes('terminal') || lower.includes('powershell') || lower.includes('bash')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg>';
      }
      if (lower.includes('code') || lower.includes('visual studio') || lower.includes('cursor') || lower.includes('trae') || lower.includes('qoder') || lower.includes('edit')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #60a5fa;"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>';
      }
      if (lower.includes('vlc') || lower.includes('play')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #fb923c;"><polygon points="5 3 19 12 5 21 5 3"/></svg>';
      }
      if (lower.includes('git')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #f87171;"><circle cx="12" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><circle cx="18" cy="6" r="3"/><path d="M18 9v1a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V9"/><line x1="12" y1="12" x2="12" y2="15"/></svg>';
      }
      if (lower.includes('pin') || lower.includes('favorite')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #fbbf24;"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>';
      }
      if (lower.includes('glary') || lower.includes('utilities') || lower.includes('shred') || lower.includes('clean')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #34d399;"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>';
      }
      if (lower.includes('copy as path') || lower.includes('copy path')) {
        return '<svg class="icon" viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>';
      }
      if (lower.includes('copy')) {
        return '<svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>';
      }
      if (lower.includes('cut')) {
        return '<svg class="icon" viewBox="0 0 24 24"><circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><line x1="20" y1="4" x2="8.12" y2="15.88"/><line x1="14.47" y1="14.48" x2="20" y2="20"/><line x1="8.12" y1="8.12" x2="12" y2="12"/></svg>';
      }
      if (lower.includes('delete')) {
        return '<svg class="icon" viewBox="0 0 24 24" style="color: #f87171;"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>';
      }
      if (lower.includes('rename')) {
        return '<svg class="icon" viewBox="0 0 24 24"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>';
      }
      if (lower.includes('properties')) {
        return '<svg class="icon" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>';
      }
      return '<svg class="icon" viewBox="0 0 24 24"><circle cx="12" cy="12" r="2"/></svg>';
    }

    async function invokeShellVerb(path, index, name) {
      closeContextMenu();
      try {
        await sendRust('invoke_local_shell_verb', { path, index, name });
        // Refresh local if it could be a delete or rename
        const lower = (name || '').toLowerCase();
        if (lower.includes('delete') || lower.includes('rename')) {
          setTimeout(refreshLocal, 400);
        }
      } catch (err) {
        alert('Action error: ' + (err.message || err));
      }
    }

    async function openInFileExplorer(path) {
      closeContextMenu();
      try {
        await sendRust('open_in_file_explorer', { path });
      } catch (err) {
        alert('Explorer error: ' + (err.message || err));
      }
    }

    async function openContextMenu(e, type, item = null, index = null) {
      e.preventDefault();
      contextTarget = { type, item, index };
      const menu = document.getElementById('customContextMenu');
      if (!menu) return;

      renderContextMenuItems(type, item);

      // Measure actual rendered height & width before showing to prevent any bottom/right clipping
      menu.style.visibility = 'hidden';
      menu.style.left = '0px';
      menu.style.top = '0px';
      menu.classList.add('active');

      const rect = menu.getBoundingClientRect();
      const menuWidth = rect.width || 255;
      const menuHeight = rect.height || 320;
      const pad = 8;
      const vw = window.innerWidth;
      const vh = window.innerHeight;

      let x = e.clientX;
      let y = e.clientY;

      if (x + menuWidth + pad > vw) {
        x = Math.max(pad, e.clientX - menuWidth);
        if (x + menuWidth + pad > vw) {
          x = Math.max(pad, vw - menuWidth - pad);
        }
      }

      if (y + menuHeight + pad > vh) {
        if (e.clientY - menuHeight >= pad) {
          y = e.clientY - menuHeight;
        } else {
          y = Math.max(pad, vh - menuHeight - pad);
        }
      }

      menu.style.left = `${Math.max(pad, Math.round(x))}px`;
      menu.style.top = `${Math.max(pad, Math.round(y))}px`;
      menu.style.visibility = 'visible';

      // Dynamic flyout collision detection for cascading submenus
      const subItems = menu.querySelectorAll('.context-menu-sub');
      subItems.forEach(sub => {
        sub.addEventListener('mouseenter', () => {
          sub.classList.remove('open-left', 'open-up');
          const submenu = sub.querySelector('.context-submenu');
          if (!submenu) return;
          const subRect = submenu.getBoundingClientRect();
          const parentRect = sub.getBoundingClientRect();
          const subW = subRect.width || 245;
          const subH = subRect.height || 180;

          if (parentRect.right + subW + pad > window.innerWidth) {
            sub.classList.add('open-left');
          }
          if (parentRect.top + subH + pad > window.innerHeight) {
            sub.classList.add('open-up');
          }
        });
      });
    }

    function renderLocalShellContextMenu(item, verbs) {
      const menu = document.getElementById('customContextMenu');
      if (!menu) return;

      let html = '';

      // 1. FTP Transfer & 90FTP Quick Actions section
      html += `
        <div class="context-menu-item" onclick="uploadFileDirect('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')">
          <svg class="icon" viewBox="0 0 24 24" style="color: var(--accent);"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><line x1="12" y1="3" x2="12" y2="15"/></svg>
          <span style="font-weight: 600;">Transfer / Upload</span>
        </div>
        <div class="context-menu-item" onclick="closeContextMenu(); openRemoteTransferModal('${escapeJsString(item.path)}', false)">
          <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>
          <span style="font-weight: 600;">Remote Transfer / Dispatch...</span>
        </div>
        <div class="context-menu-item" onclick="openLocalFileInEditor('${escapeJsString(item.path)}')">
          <svg class="icon" viewBox="0 0 24 24" style="color: var(--accent);"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>
          <span>Edit in Code Editor</span>
        </div>
        <div class="context-menu-item" onclick="openInFileExplorer('${escapeJsString(item.path)}')">
          <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
          <span>Open in File Explorer</span>
        </div>
        <div class="context-menu-separator"></div>
      `;

      // 2. Windows Shell Context Menu Verbs
      const primaryVerbs = [];
      const appVerbs = [];
      const toolVerbs = [];
      const fsVerbs = [];

      verbs.forEach(v => {
        const nameLower = v.name.toLowerCase();
        if (nameLower === 'open' || nameLower === 'open in new process' || nameLower === 'pin to quick access') {
          primaryVerbs.push(v);
        } else if (nameLower.includes('visual studio') || nameLower.includes('cursor') || nameLower.includes('qoder') || nameLower.includes('trae') || nameLower.includes('code') || nameLower.includes('zcode') || nameLower.includes('vlc') || nameLower.includes('terminal') || nameLower.includes('git')) {
          appVerbs.push(v);
        } else if (nameLower.includes('glary') || nameLower.includes('shred') || nameLower.includes('defrag') || nameLower.includes('split') || nameLower.includes('encrypt') || nameLower.includes('acronis') || nameLower.includes('7-zip')) {
          toolVerbs.push(v);
        } else {
          fsVerbs.push(v);
        }
      });

      function renderGroup(group) {
        group.forEach(v => {
          const iconSvg = getVerbIconSvg(v.name);
          const isDanger = v.name.toLowerCase() === 'delete';
          const isBold = v.name.toLowerCase() === 'open';
          html += `
            <div class="context-menu-item ${isDanger ? 'danger' : ''}" onclick="invokeShellVerb('${escapeJsString(item.path)}', ${v.index}, '${escapeJsString(v.name)}')">
              ${iconSvg}
              <span style="${isBold ? 'font-weight: 700;' : ''}">${escapeHtml(v.name)}</span>
            </div>
          `;
        });
      }

      if (primaryVerbs.length > 0) {
        renderGroup(primaryVerbs);
        html += `<div class="context-menu-separator"></div>`;
      }
      if (appVerbs.length > 0) {
        renderGroup(appVerbs);
        html += `<div class="context-menu-separator"></div>`;
      }
      if (toolVerbs.length > 0) {
        renderGroup(toolVerbs);
        html += `<div class="context-menu-separator"></div>`;
      }
      if (fsVerbs.length > 0) {
        renderGroup(fsVerbs);
      }

      menu.innerHTML = html;
    }

    function closeContextMenu() {
      const menu = document.getElementById('customContextMenu');
      if (menu) menu.classList.remove('active');
    }

    window.addEventListener('click', () => closeContextMenu());
    window.addEventListener('blur', () => closeContextMenu());

    function renderContextMenuItems(type, item) {
      const menu = document.getElementById('customContextMenu');
      if (!menu) return;

      if (type === 'log') {
        const selText = window.getSelection() ? window.getSelection().toString() : '';
        const lineText = item?.innerText || '';
        const errorCount = document.querySelectorAll('#logBody .log-msg.error').length;

        let html = `<div class="context-menu-header">Activity Log & Diagnostics</div>`;
        if (selText && selText.trim()) {
          html += `
            <div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(selText)}')">
              <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
              <span style="font-weight: 600;">Copy Selected Text</span>
              <span class="shortcut">Ctrl+C</span>
            </div>
            <div class="context-menu-separator"></div>
          `;
        }
        if (lineText && lineText.trim()) {
          html += `
            <div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(lineText)}')">
              <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
              <span>Copy Current Log Line</span>
            </div>
          `;
        }
        html += `
          <div class="context-menu-item" onclick="closeContextMenu(); copyErrorsOnly()" style="color: #ef4444;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #ef4444;"><polygon points="7.86 2 16.14 2 22 7.86 22 16.14 16.14 22 7.86 22 2 16.14 2 7.86 7.86 2"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>
            <span style="font-weight: 600;">Copy Errors Only (${errorCount})</span>
          </div>
          <div class="context-menu-item" onclick="closeContextMenu(); copyAllLogs()">
            <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
            <span>Copy Entire Console Log</span>
          </div>
          <div class="context-menu-item" onclick="closeContextMenu(); exportLogsToFile()">
            <svg class="icon" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
            <span>Export Log to File...</span>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick="closeContextMenu(); clearLogs()">
            <svg class="icon" viewBox="0 0 24 24"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
            <span>Clear Console</span>
          </div>
        `;
        menu.innerHTML = html;
        return;
      }

      if (type === 'rt_log') {
        const selText = window.getSelection() ? window.getSelection().toString() : '';
        let html = `<div class="context-menu-header">Remote Transfer Logs</div>`;
        if (selText && selText.trim()) {
          html += `
            <div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(selText)}')">
              <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
              <span style="font-weight: 600;">Copy Selected Text</span>
            </div>
            <div class="context-menu-separator"></div>
          `;
        }
        html += `
          <div class="context-menu-item" onclick="closeContextMenu(); copyRtLog()">
            <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
            <span>Copy All Transfer Logs</span>
          </div>
          <div class="context-menu-item" onclick="closeContextMenu(); copyRtErrorsOnly()" style="color: #ef4444;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #ef4444;"><polygon points="7.86 2 16.14 2 22 7.86 22 16.14 16.14 22 7.86 22 2 16.14 2 7.86 7.86 2"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>
            <span style="font-weight: 600;">Copy Errors Only</span>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick="closeContextMenu(); clearRtLog()">
            <svg class="icon" viewBox="0 0 24 24"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
            <span>Clear Transfer Log</span>
          </div>
        `;
        menu.innerHTML = html;
        return;
      }

      if (type === 'transfer_item') {
        const tr = item;
        if (!tr) { menu.innerHTML = ''; return; }
        const name = transferName(tr);
        const src  = transferSource(tr);
        const dst  = transferDestination(tr);
        let html = '<div class="context-menu-header">' + escapeHtml(name) + '</div>';

        if (tr.error) {
          html += `<div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(tr.error)}')">
            <svg class="icon" viewBox="0 0 24 24" style="color: #ef4444;"><polygon points="7.86 2 16.14 2 22 7.86 22 16.14 16.14 22 7.86 22 2 16.14 2 7.86 7.86 2"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>
            <span style="font-weight: 600; color: #ef4444;">Copy Error Details</span>
          </div>`;
        }
        html += `<div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(name)}')">
          <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
          <span>Copy File Name</span>
        </div>`;
        html += `<div class="context-menu-item" onclick="closeContextMenu(); copyTextToClipboard('${escapeJsString(src + ' -> ' + dst)}')">
          <svg class="icon" viewBox="0 0 24 24"><polyline points="17 8 12 3 7 8"/><line x1="12" y1="3" x2="12" y2="15"/></svg>
          <span>Copy Full Transfer Route</span>
        </div>`;

        html += '<div class="context-menu-separator"></div>';

        if (tr.state === 'running' || tr.state === 'pending') {
          html += `<div class="context-menu-item" onclick="closeContextMenu(); transferAction('transfer_pause', ${tr.id})">
            <svg class="icon" viewBox="0 0 24 24"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg>
            <span>Pause</span>
          </div>`;
          html += `<div class="context-menu-item" onclick="closeContextMenu(); transferAction('transfer_cancel_task', ${tr.id})" style="color: #ef4444;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #ef4444;"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>
            <span>Cancel</span>
          </div>`;
        } else if (tr.state === 'paused') {
          html += `<div class="context-menu-item" onclick="closeContextMenu(); transferAction('transfer_resume', ${tr.id})" style="color: #22c55e;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #22c55e;"><polygon points="5 3 19 12 5 21 5 3"/></svg>
            <span>Resume</span>
          </div>`;
          html += `<div class="context-menu-item" onclick="closeContextMenu(); transferAction('transfer_cancel_task', ${tr.id})" style="color: #ef4444;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #ef4444;"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>
            <span>Cancel</span>
          </div>`;
        } else if (tr.state === 'failed' || tr.state === 'cancelled') {
          html += `<div class="context-menu-item" onclick="closeContextMenu(); transferAction('transfer_requeue', ${tr.id})" style="color: #38bdf8;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="23 4 23 10 17 10"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/></svg>
            <span>Retry</span>
          </div>`;
        }

        html += `<div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick="closeContextMenu(); copyAllTransferQueueErrors()">
            <svg class="icon" viewBox="0 0 24 24"><polygon points="7.86 2 16.14 2 22 7.86 22 16.14 16.14 22 7.86 22 2 16.14 2 7.86 7.86 2"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>
            <span>Copy All Queue Errors</span>
          </div>`;
        menu.innerHTML = html;
        return;
      }

      const isRemote = type.startsWith('remote');
      const isBlank = type.endsWith('_blank');
      const chevronSvg = `<svg class="context-menu-sub-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6"/></svg>`;

      let html = '';

      if (isBlank) {
        const dirPath = isRemote ? currentRemotePath : currentLocalPath;
        const hasClipboard = interactiveClipboard && Array.isArray(interactiveClipboard.items) && interactiveClipboard.items.length > 0;
        html += `<div class="context-menu-header">${isRemote ? 'Remote Directory' : 'Local Directory'}</div>`;
        if (hasClipboard) {
          html += `
            <div class="context-menu-item" onclick="clipboardPasteHere(${isRemote})" style="color: #22c55e;">
              <svg class="icon" viewBox="0 0 24 24" style="color: #22c55e;"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1" ry="1"/></svg>
              <span style="font-weight: 700;">Paste ${interactiveClipboard.items.length} Item(s) (${interactiveClipboard.mode === 'cut' ? 'Move' : 'Copy'})</span>
              <span class="shortcut">Ctrl+V</span>
            </div>
            <div class="context-menu-separator"></div>
          `;
        }
        html += `
          <div class="context-menu-item" onclick="closeContextMenu(); ${isRemote ? 'createNewRemoteTab()' : 'createNewLocalTab()'}">
            <svg class="icon" viewBox="0 0 24 24" style="color: var(--accent);"><rect x="3" y="3" width="18" height="18" rx="2"/><line x1="12" y1="8" x2="12" y2="16"/><line x1="8" y1="12" x2="16" y2="12"/></svg>
            <span style="font-weight: 600;">Open New ${isRemote ? 'Remote' : 'Local'} Tab</span>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick="promptNewFile(${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="12" y1="18" x2="12" y2="12"/><line x1="9" y1="15" x2="15" y2="15"/></svg>
            <span>New File...</span>
          </div>
          <div class="context-menu-item" onclick="promptNewFolder(${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg>
            <span>New Folder...</span>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="9 11 12 14 22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></svg>
              <span>Selection</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'all');">
                <svg class="icon" viewBox="0 0 24 24"><polyline points="9 11 12 14 22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></svg>
                <span>Select All (Files & Folders)</span>
                <span class="shortcut">Ctrl+A</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'files');">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>
                <span>Select Files Only</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'folders');">
                <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                <span>Select Folders Only</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'invert');">
                <svg class="icon" viewBox="0 0 24 24"><polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg>
                <span>Invert Selection</span>
              </div>
            </div>
          </div>
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #a855f7;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/></svg>
              <span>Directory Tools</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              <div class="context-menu-item" onclick="copyItemPath('${escapeJsString(dirPath)}')">
                <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
                <span>Copy Directory Path</span>
              </div>
              ${isRemote ? `
              <div class="context-menu-item" onclick="openRemoteTerminal('${escapeJsString(dirPath)}')">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg>
                <span>Open Terminal Here</span>
              </div>
              <div class="context-menu-item" onclick="remoteCompressCurrentDirectory()">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
                <span>Compress Current Remote Directory...</span>
              </div>
              ` : `
              <div class="context-menu-item" onclick="compressCurrentLocalDirectory()">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
                <span>Compress Current Directory...</span>
              </div>
              `}
            </div>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick="${isRemote ? 'refreshRemote()' : 'refreshLocal()'}">
            <svg class="icon" viewBox="0 0 24 24"><polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg>
            <span>Refresh</span>
            <span class="shortcut">F5</span>
          </div>
        `;
      } else if (item) {
        const selectedItems = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
        const selCount = selectedItems.length > 1 ? selectedItems.length : 1;
        const canGoUp = isRemote ? !isRemoteRoot(currentRemotePath) : !isLocalRoot(currentLocalPath);
        const hasClipboard = interactiveClipboard && Array.isArray(interactiveClipboard.items) && interactiveClipboard.items.length > 0;
        const isArchive = !item.is_dir && (isRemote ? isArchiveFilenameJs(item.name) : item.name.toLowerCase().endsWith('.zip'));
        const otherFiles = isRemote ? localFiles : remoteFiles;
        const counterpart = !item.is_dir ? otherFiles.find(f => !f.is_dir && f.name.toLowerCase() === item.name.toLowerCase()) : null;

        html += `<div class="context-menu-header">${selCount > 1 ? `${selCount} Items Selected` : ((item.is_dir ? 'Folder: ' : 'File: ') + escapeHtml(item.name.length > 24 ? item.name.substring(0, 22) + '...' : item.name))}</div>`;

        // 1. Primary Open / Edit Actions
        if (item.is_dir) {
          html += `
            <div class="context-menu-item" onclick="${isRemote ? `navigateRemote('${escapeJsString(item.path)}')` : `navigateLocal('${escapeJsString(item.path)}')`}">
              <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
              <span style="font-weight: 600;">Open Folder</span>
              <span class="shortcut">Enter</span>
            </div>
            <div class="context-menu-item" onclick="closeContextMenu(); openFolderInNewTab(${isRemote}, '${escapeJsString(item.path)}')">
              <svg class="icon" viewBox="0 0 24 24" style="color: var(--accent);"><rect x="3" y="3" width="18" height="18" rx="2"/><line x1="12" y1="8" x2="12" y2="16"/><line x1="8" y1="12" x2="16" y2="12"/></svg>
              <span>Open in New Tab</span>
              <span class="shortcut">Middle-Click</span>
            </div>
          `;
        } else {
          html += `
            <div class="context-menu-item" onclick="${isRemote ? `openRemoteFileInEditor('${escapeJsString(item.path)}')` : `openLocalFileInEditor('${escapeJsString(item.path)}')`}">
              <svg class="icon" viewBox="0 0 24 24" style="color: var(--accent);"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>
              <span style="font-weight: 600;">Edit in Code Editor</span>
              <span class="shortcut">F4</span>
            </div>
          `;
        }

        // 2. Primary Transfer & Server-to-Server Dispatch Actions
        if (isRemote) {
          html += `
            <div class="context-menu-item" onclick="${selCount > 1 ? `transferDownload()` : (item.is_dir ? `downloadFolderDirect('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')` : `downloadFileDirect('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')`)}">
              <svg class="icon" viewBox="0 0 24 24" style="color:var(--accent);"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
              <span style="font-weight: 600;">${selCount > 1 ? `Download ${selCount} Items to Local` : 'Download to Local'}</span>
            </div>
            <div class="context-menu-item" onclick="closeContextMenu(); openRemoteTransferModal('${escapeJsString(item.path)}', true)">
              <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>
              <span style="font-weight: 600;">Remote Transfer / Dispatch...</span>
            </div>
          `;
        } else {
          html += `
            <div class="context-menu-item" onclick="${selCount > 1 ? `transferUpload()` : (item.is_dir ? `uploadFolderDirect('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')` : `uploadFileDirect('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')`)}">
              <svg class="icon" viewBox="0 0 24 24" style="color:var(--accent);"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><line x1="12" y1="3" x2="12" y2="15"/></svg>
              <span style="font-weight: 600;">${selCount > 1 ? `Upload ${selCount} Items to Server` : 'Upload to Server'}</span>
            </div>
            <div class="context-menu-item" onclick="closeContextMenu(); openRemoteTransferModal('${escapeJsString(item.path)}', false)">
              <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>
              <span style="font-weight: 600;">Remote Transfer / Dispatch...</span>
            </div>
          `;
        }

        html += `<div class="context-menu-separator"></div>`;

        // 3. Quick Clipboard Actions + Cascading Submenu Groups (7-Zip / Desktop Explorer Style)
        html += `
          <div class="context-menu-item" onclick="clipboardCutSelected(${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><line x1="20" y1="4" x2="8.12" y2="15.88"/><line x1="14.47" y1="14.48" x2="20" y2="20"/><line x1="8.12" y1="8.12" x2="12" y2="12"/></svg>
            <span>Cut</span>
            <span class="shortcut">Ctrl+X</span>
          </div>
          <div class="context-menu-item" onclick="clipboardCopySelected(${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
            <span>Copy</span>
            <span class="shortcut">Ctrl+C</span>
          </div>
          ${hasClipboard ? `
          <div class="context-menu-item" onclick="clipboardPasteHere(${isRemote})" style="color: #22c55e;">
            <svg class="icon" viewBox="0 0 24 24" style="color: #22c55e;"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1" ry="1"/></svg>
            <span style="font-weight: 600;">Paste ${interactiveClipboard.items.length} Item(s) Here</span>
            <span class="shortcut">Ctrl+V</span>
          </div>
          ` : ''}
          <div class="context-menu-separator"></div>
        `;

        // Group A: Move & Organize ▸
        html += `
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #fbbf24;"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></svg>
              <span>Move & Organize</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              ${canGoUp ? `
              <div class="context-menu-item" onclick="moveSelectedToParent(${isRemote})">
                <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><path d="M9 14 4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5v0a5.5 5.5 0 0 1-5.5 5.5H11"/></svg>
                <span style="font-weight: 600; color: #fbbf24;">Move to .. (Parent Directory)</span>
                <span class="shortcut">Alt+↑</span>
              </div>
              ` : ''}
              <div class="context-menu-item" onclick="openInteractiveMoveModal(${isRemote}, 'move')">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></svg>
                <span style="font-weight: 600;">Interactive Move / Copy To...</span>
                <span class="shortcut">F6</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); openBatchRenameModal(${isRemote})">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"/></svg>
                <span>Batch Multi-Rename (${selCount > 1 ? selCount + ' Selected' : 'Regex'})...</span>
                <span class="shortcut">Ctrl+M</span>
              </div>
              ${!item.is_dir ? `
              <div class="context-menu-item" onclick="duplicateItemsInPlace(${isRemote}, [{ path: '${escapeJsString(item.path)}', name: '${escapeJsString(item.name)}', is_dir: false }])">
                <svg class="icon" viewBox="0 0 24 24" style="color: #a855f7;"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></svg>
                <span>Duplicate File</span>
              </div>
              ` : `
              <div class="context-menu-item" onclick="closeContextMenu(); quickBookmarkFolder('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${isRemote})">
                <svg class="icon" viewBox="0 0 24 24" style="color: #fbbf24;"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>
                <span>Bookmark This Folder</span>
              </div>
              `}
            </div>
          </div>
        `;

        // Group B: Archive & Compression ▸
        html += `
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
              <span>Archive & Compression</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              ${!isRemote ? `
                ${isArchive ? `
                <div class="context-menu-item" onclick="extractLocalHere('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><polyline points="8 17 12 21 16 17"/><line x1="12" y1="12" x2="12" y2="21"/><path d="M20.88 18.09A5 5 0 0 0 18 9h-1.26A8 8 0 1 0 3 16.29"/></svg>
                  <span style="font-weight: 600;">Extract Here</span>
                </div>
                <div class="context-menu-item" onclick="extractLocalToSubfolder('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg>
                  <span>Extract to ${escapeHtml(item.name.replace(/\.[^/.]+$/, ''))}/</span>
                </div>
                <div class="context-menu-item" onclick="promptExtractLocalFolder('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                  <span>Extract to Custom Folder...</span>
                </div>
                <div class="context-menu-separator"></div>
                ` : ''}
                <div class="context-menu-item" onclick="promptCompressLocalSelected('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
                  <span>Compress Archive (ZIP, TAR, GZ)...</span>
                </div>
              ` : `
                ${isArchive ? `
                <div class="context-menu-item" onclick="remoteExtractHere('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><polyline points="8 17 12 21 16 17"/><line x1="12" y1="12" x2="12" y2="21"/><path d="M20.88 18.09A5 5 0 0 0 18 9h-1.26A8 8 0 1 0 3 16.29"/></svg>
                  <span style="font-weight: 600;">Extract Archive Here (Remote)</span>
                </div>
                <div class="context-menu-item" onclick="remoteExtractToSubfolder('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg>
                  <span>Extract to /${escapeHtml(item.name.replace(/\.[^/.]+$/, ''))}/</span>
                </div>
                <div class="context-menu-item" onclick="remoteExtractCustomPath('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                  <span>Extract to Custom Path...</span>
                </div>
                <div class="context-menu-separator"></div>
                ` : ''}
                <div class="context-menu-item" onclick="remoteCompressSelected('${escapeJsString(item.path)}', '${escapeJsString(item.name)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>
                  <span>Compress Archive (ZIP, TAR, GZ)...</span>
                </div>
              `}
            </div>
          </div>
        `;

        // Group C: Inspect & Tools ▸
        html += `
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #10b981;"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
              <span>Inspect & Tools</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              ${item.is_dir ? `
                <div class="context-menu-item" onclick="closeContextMenu(); calculateFolderSize('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${isRemote})">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #10b981;"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/></svg>
                  <span>Calculate Folder Size...</span>
                </div>
                <div class="context-menu-item" onclick="closeContextMenu(); openContentGrepAtPath('${escapeJsString(item.path)}', ${isRemote})">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #a855f7;"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
                  <span>Search Text Inside Folder (Grep)...</span>
                </div>
                ${isRemote ? `
                <div class="context-menu-item" onclick="openRemoteTerminal('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg>
                  <span>Open Terminal Here</span>
                </div>
                ` : `
                <div class="context-menu-item" onclick="openInFileExplorer('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                  <span>Open in File Explorer</span>
                </div>
                `}
              ` : `
                <div class="context-menu-item" onclick="closeContextMenu(); openFileInspectorModal('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${isRemote})">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #10b981;"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="16" y1="13" x2="8" y2="13"/><line x1="16" y1="17" x2="8" y2="17"/></svg>
                  <span>Live Log Tail / Hex Inspector...</span>
                </div>
                <div class="context-menu-item" onclick="openChecksumModal('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${isRemote})">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #a855f7;"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>
                  <span>Calculate Checksum (SHA-256 / MD5)...</span>
                </div>
                ${counterpart ? `
                <div class="context-menu-item" onclick="closeContextMenu(); openFileDiffModal('${escapeJsString(isRemote ? counterpart.path : item.path)}', '${escapeJsString(isRemote ? item.path : counterpart.path)}', '${escapeJsString(item.name)}')">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M16 3h5v5"/><path d="M8 3H3v5"/><path d="M12 22v-8.3a4 4 0 0 0-1.172-2.872L3 3"/><path d="m15 9 6-6"/></svg>
                  <span style="font-weight: 600;">Compare File Contents (Diff)</span>
                </div>
                <div class="context-menu-item" onclick="openChecksumCompareModal('${escapeJsString(item.path)}', '${escapeJsString(counterpart.path)}', '${escapeJsString(item.name)}', ${isRemote})">
                  <svg class="icon" viewBox="0 0 24 24" style="color: #10b981;"><polyline points="20 6 9 17 4 12"/></svg>
                  <span>Verify Checksum with ${isRemote ? 'Local' : 'Remote'}</span>
                </div>
                ` : ''}
                ${!isRemote ? `
                <div class="context-menu-item" onclick="openInFileExplorer('${escapeJsString(item.path)}')">
                  <svg class="icon" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                  <span>Reveal in File Explorer</span>
                </div>
                ` : ''}
              `}
              <div class="context-menu-separator"></div>
              <div class="context-menu-item" onclick="copyItemPath('${escapeJsString(item.path)}')">
                <svg class="icon" viewBox="0 0 24 24"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
                <span>Copy Full Path</span>
              </div>
            </div>
          </div>
        `;

        // Group D: Selection ▸
        html += `
          <div class="context-menu-sub">
            <div class="context-menu-item">
              <svg class="icon" viewBox="0 0 24 24" style="color: #a855f7;"><polyline points="9 11 12 14 22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></svg>
              <span>Selection</span>
              ${chevronSvg}
            </div>
            <div class="context-submenu">
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'all');">
                <svg class="icon" viewBox="0 0 24 24"><polyline points="9 11 12 14 22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></svg>
                <span>Select All (Files & Folders)</span>
                <span class="shortcut">Ctrl+A</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'files');">
                <svg class="icon" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>
                <span>Select Files Only</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'folders');">
                <svg class="icon" viewBox="0 0 24 24" style="color: #f59e0b;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                <span>Select Folders Only</span>
              </div>
              <div class="context-menu-item" onclick="closeContextMenu(); selectAllItemsInPane(${isRemote}, 'invert');">
                <svg class="icon" viewBox="0 0 24 24"><polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg>
                <span>Invert Selection</span>
              </div>
            </div>
          </div>
        `;

        html += `<div class="context-menu-separator"></div>`;

        // 4. Direct Item Management (Rename, Permissions, Delete, Properties)
        html += `
          <div class="context-menu-item" onclick="promptRename('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>
            <span>Rename...</span>
            <span class="shortcut">F2</span>
          </div>
        `;

        if (isRemote) {
          html += `
            <div class="context-menu-item" onclick="openChmodForItem('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', '${escapeJsString(item.permissions || '0644')}')">
              <svg class="icon" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg>
              <span>Permissions (CHMOD)...</span>
            </div>
          `;
        }

        html += `
          <div class="context-menu-item danger" onclick="confirmDelete('${escapeJsString(item.path)}', '${escapeJsString(item.name)}', ${item.is_dir}, ${isRemote})">
            <svg class="icon" viewBox="0 0 24 24"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
            <span>${selCount > 1 ? `Delete ${selCount} Selected Items` : 'Delete'}</span>
            <span class="shortcut">Del</span>
          </div>
          <div class="context-menu-separator"></div>
          <div class="context-menu-item" onclick='showPropertiesModal(${JSON.stringify(item)}, ${isRemote})'>
            <svg class="icon" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
            <span>Properties & Info...</span>
          </div>
        `;
      }

      menu.innerHTML = html;
    }

    // Used exclusively inside inline `onclick="fn('...')"` handlers, so it must
    // escape BOTH the JS string literal and the surrounding HTML attribute.
    // `"` becomes &quot; so a path containing a double quote cannot terminate the
    // attribute and inject a new one (onmouseover=...).
    function escapeJsString(str) {
      if (!str) return '';
      return String(str)
        .replace(/&/g, '&amp;')
        .replace(/\\/g, '\\\\')
        .replace(/'/g, "\\'")
        .replace(/"/g, '&quot;')
        .replace(/\r/g, '\\r')
        .replace(/\n/g, '\\n');
    }

    async function promptNewFile(isRemote) {
      closeContextMenu();
      const fileName = prompt('Enter new file name (e.g. index.php, config.json):');
      if (!fileName || !fileName.trim()) return;
      const cleanName = fileName.trim();

      if (isRemote) {
        if (!isConnected) { alert('Please connect to server first'); return; }
        const fullPath = currentRemotePath.endsWith('/') ? currentRemotePath + cleanName : currentRemotePath + '/' + cleanName;
        try {
          const res = await sendRust('remote_create_file', { path: fullPath });
          renderRemoteFiles(res.files || []);
          addLog(`Created remote file: ${fullPath}`, 'success');
        } catch (e) {
          alert('Failed to create file: ' + e.message);
        }
      } else {
        const fullPath = currentLocalPath.endsWith('\\') ? currentLocalPath + cleanName : currentLocalPath + '\\' + cleanName;
        try {
          await sendRust('local_create_file', { path: fullPath });
          refreshLocal();
          addLog(`Created local file: ${fullPath}`, 'success');
        } catch (e) {
          alert('Failed to create file: ' + e.message);
        }
      }
    }

    async function promptNewFolder(isRemote) {
      closeContextMenu();
      const folderName = prompt('Enter new folder name:');
      if (!folderName || !folderName.trim()) return;
      const cleanName = folderName.trim();

      if (isRemote) {
        if (!isConnected) { alert('Please connect to server first'); return; }
        const fullPath = currentRemotePath.endsWith('/') ? currentRemotePath + cleanName : currentRemotePath + '/' + cleanName;
        try {
          const res = await sendRust('remote_create_dir', { path: fullPath });
          renderRemoteFiles(res.files || []);
          addLog(`Created remote directory: ${fullPath}`, 'success');
        } catch (e) {
          alert('Failed to create directory: ' + e.message);
        }
      } else {
        const fullPath = currentLocalPath.endsWith('\\') ? currentLocalPath + cleanName : currentLocalPath + '\\' + cleanName;
        try {
          await sendRust('local_create_dir', { path: fullPath });
          refreshLocal();
          addLog(`Created local directory: ${fullPath}`, 'success');
        } catch (e) {
          alert('Failed to create directory: ' + e.message);
        }
      }
    }

    async function promptRename(oldPath, oldName, isRemote) {
      closeContextMenu();
      const newName = prompt(`Rename '${oldName}' to:`, oldName);
      if (!newName || !newName.trim() || newName.trim() === oldName) return;
      const cleanName = newName.trim();

      if (isRemote) {
        const parent = oldPath.replace(/\/[^\/]+$/, '') || '/';
        const newPath = parent.endsWith('/') ? parent + cleanName : parent + '/' + cleanName;
        try {
          const res = await sendRust('remote_rename', { old_path: oldPath, new_path: newPath });
          renderRemoteFiles(res.files || []);
          addLog(`Renamed remote '${oldName}' to '${cleanName}'`, 'success');
        } catch (e) {
          alert('Rename failed: ' + e.message);
        }
      } else {
        const parent = oldPath.replace(/\\[^\\]+$/, '');
        const newPath = parent ? parent + '\\' + cleanName : cleanName;
        try {
          await sendRust('local_rename', { old_path: oldPath, new_path: newPath });
          refreshLocal();
          addLog(`Renamed local '${oldName}' to '${cleanName}'`, 'success');
        } catch (e) {
          alert('Rename failed: ' + e.message);
        }
      }
    }

    async function deleteSelectedItems(isRemote, explicitItem = null) {
      closeContextMenu();
      const selected = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
      let itemsToDelete = [];

      if (explicitItem && explicitItem.path) {
        if (selected.length > 1 && selected.some(it => it.path === explicitItem.path)) {
          itemsToDelete = selected;
        } else {
          itemsToDelete = [explicitItem];
        }
      } else {
        itemsToDelete = selected;
      }

      // Filter out any invalid or parent directory entries
      itemsToDelete = itemsToDelete.filter(it => it && it.path && it.name !== '.' && it.name !== '..');

      if (itemsToDelete.length === 0) {
        alert(`Please select one or more ${isRemote ? 'remote' : 'local'} files or folders to delete.`);
        return;
      }

      let confirmMsg = '';
      if (itemsToDelete.length === 1) {
        const single = itemsToDelete[0];
        const kind = single.is_dir ? 'directory' : 'file';
        confirmMsg = `Are you sure you want to permanently delete this ${isRemote ? 'remote' : 'local'} ${kind}?\n\n${single.name}`;
      } else {
        const previewList = itemsToDelete.slice(0, 8).map(it => `• ${it.name}${it.is_dir ? '/' : ''}`).join('\n');
        const extra = itemsToDelete.length > 8 ? `\n...and ${itemsToDelete.length - 8} more item(s)` : '';
        confirmMsg = `Are you sure you want to permanently delete these ${itemsToDelete.length} selected ${isRemote ? 'remote' : 'local'} items?\n\n${previewList}${extra}`;
      }

      if (!confirm(confirmMsg)) return;

      if (isRemote) {
        if (!isConnected) {
          if (appOptions && appOptions.autoReconnectOnClick !== false && lastConnectedProfile) {
            const ok = await ensureSilentConnected(currentRemotePath);
            if (!ok) return;
          } else {
            alert('Please connect to a remote server first.');
            return;
          }
        }
        try {
          if (itemsToDelete.length === 1) {
            const single = itemsToDelete[0];
            const res = await sendRust('remote_delete', {
              path: single.path,
              is_dir: !!single.is_dir,
              current_dir: currentRemotePath
            });
            selectedRemoteFile = null;
            lastRemoteClickedIdx = -1;
            renderRemoteFiles(res.files || []);
            addLog(`Deleted remote ${single.is_dir ? 'directory' : 'file'}: ${single.name}`, 'success');
          } else {
            const res = await sendRust('remote_delete_batch', {
              items: itemsToDelete.map(it => ({ path: it.path, name: it.name, is_dir: !!it.is_dir })),
              current_dir: currentRemotePath
            });
            selectedRemoteFile = null;
            lastRemoteClickedIdx = -1;
            renderRemoteFiles(res.files || []);
            const delCount = res.deleted_count ?? itemsToDelete.length;
            addLog(`Deleted ${delCount} remote item(s) successfully.`, 'success');
            if (Array.isArray(res.errors) && res.errors.length > 0) {
              addLog(`Some remote items could not be deleted: ${res.errors.join(' | ')}`, 'error');
              alert(`Deleted ${delCount} item(s), with ${res.errors.length} error(s):\n${res.errors.slice(0, 5).join('\n')}`);
            }
          }
        } catch (e) {
          const msg = e.message || String(e);
          addLog('Remote deletion failed: ' + msg, 'error');
          alert('Delete Error:\n' + msg);
          await refreshRemote();
        }
      } else {
        try {
          if (itemsToDelete.length === 1) {
            const single = itemsToDelete[0];
            await sendRust('local_delete', { path: single.path });
            selectedLocalFile = null;
            lastLocalClickedIdx = -1;
            await refreshLocal();
            addLog(`Deleted local ${single.is_dir ? 'directory' : 'file'}: ${single.name}`, 'success');
          } else {
            const res = await sendRust('local_delete_batch', {
              items: itemsToDelete.map(it => ({ path: it.path, name: it.name, is_dir: !!it.is_dir }))
            });
            selectedLocalFile = null;
            lastLocalClickedIdx = -1;
            await refreshLocal();
            const delCount = res.deleted_count ?? itemsToDelete.length;
            addLog(`Deleted ${delCount} local item(s) successfully.`, 'success');
            if (Array.isArray(res.errors) && res.errors.length > 0) {
              addLog(`Some local items could not be deleted: ${res.errors.join(' | ')}`, 'error');
              alert(`Deleted ${delCount} item(s), with ${res.errors.length} error(s):\n${res.errors.slice(0, 5).join('\n')}`);
            }
          }
        } catch (e) {
          const msg = e.message || String(e);
          addLog('Local deletion failed: ' + msg, 'error');
          alert('Delete Error:\n' + msg);
          await refreshLocal();
        }
      }
    }

    async function confirmDelete(path, name, isDir, isRemote) {
      await deleteSelectedItems(isRemote, { path, name, is_dir: !!isDir });
    }

    function copyItemPath(path) {
      closeContextMenu();
      if (!path) return;
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(path).then(() => {
          addLog(`Copied to clipboard: ${path}`, 'info');
        }).catch(() => fallbackCopyText(path));
      } else {
        fallbackCopyText(path);
      }
    }

    function showPropertiesModal(item, isRemote) {
      closeContextMenu();
      if (!item) return;
      document.getElementById('propTitle').innerText = `${isRemote ? 'Remote' : 'Local'} Properties`;
      const { svg, iconClass } = getFileIcon(item.name, !!item.is_dir);
      const iconContainer = document.getElementById('propIcon');
      if (iconContainer) {
        iconContainer.innerHTML = `<span class="file-icon ${iconClass}" style="display: inline-flex; width: 36px; height: 36px; align-items: center; justify-content: center;">${svg}</span>`;
      }
      document.getElementById('propName').innerText = item.name;
      document.getElementById('propType').innerText = item.is_dir ? 'Folder / Directory' : (item.is_symlink ? 'Symlink' : 'Regular File');
      document.getElementById('propPath').innerText = item.path;
      document.getElementById('propSize').innerText = item.is_dir ? '-' : `${formatBytes(item.size)} (${item.size.toLocaleString()} bytes)`;
      document.getElementById('propPerms').innerText = item.permissions || '-';
      document.getElementById('propModified').innerText = item.modified || '-';
      document.getElementById('propertiesModal').classList.add('active');
    }

    function closePropertiesModal() {
      document.getElementById('propertiesModal').classList.remove('active');
    }

    // ========================================================
    // Local Compression & Extraction (ZIP Engine)
    // ========================================================
    function joinLocalPath(parent, child) {
      if (!parent) return child || '';
      const p = parent.replace(/[\\/]+$/, '');
      const sep = p.includes('/') ? '/' : '\\';
      return p + sep + (child || '').replace(/^[\\/]+/, '');
    }

    function getSelectedLocalItems() {
      const selectedRows = document.querySelectorAll('#localFileList tr.selected:not(.parent-dir-row)');
      const items = [];
      selectedRows.forEach(r => {
        const idx = parseInt(r.getAttribute('data-idx'));
        if (!isNaN(idx) && localFiles[idx]) {
          items.push(localFiles[idx]);
        }
      });
      if (items.length === 0 && selectedLocalFile && localFiles.some(f => f.path === selectedLocalFile.path)) {
        items.push(selectedLocalFile);
      }
      return items;
    }

    // Advanced Compression Settings Dialog state & handlers
    let currentCompressConfig = null;

    function openCompressDialog(config) {
      currentCompressConfig = config;
      const modal = document.getElementById('compressDialogModal');
      const title = document.getElementById('compressModalTitle');
      const subtitle = document.getElementById('compressModalSubtitle');
      const summary = document.getElementById('compressSourceSummary');
      const scopeBadge = document.getElementById('compressScopeBadge');
      const baseNameInput = document.getElementById('compressArchiveBaseName');
      const formatSelect = document.getElementById('compressArchiveFormat');
      const customBox = document.getElementById('compressCustomExtBox');
      const customInput = document.getElementById('compressCustomExtInput');
      const destDirInput = document.getElementById('compressDestinationDir');

      if (config.scope === 'local') {
        title.textContent = 'Compress Local Files & Folders';
        subtitle.textContent = 'Select archive format, filename, and compression level';
        scopeBadge.textContent = 'Local Computer';
        scopeBadge.style.color = '#38bdf8';
        scopeBadge.style.borderColor = 'rgba(56, 189, 248, 0.3)';
        scopeBadge.style.background = 'rgba(56, 189, 248, 0.15)';
      } else {
        title.textContent = 'Compress Remote Files & Folders';
        subtitle.textContent = 'Select archive format, filename, and compression level on server';
        scopeBadge.textContent = 'Remote Site';
        scopeBadge.style.color = '#22c55e';
        scopeBadge.style.borderColor = 'rgba(34, 197, 94, 0.3)';
        scopeBadge.style.background = 'rgba(34, 197, 94, 0.15)';
      }

      summary.textContent = config.summaryText || `${config.sources.length} item(s) selected`;
      baseNameInput.value = config.defaultBaseName || 'archive';
      formatSelect.value = '.zip';
      customBox.style.display = 'none';
      customInput.value = '';
      destDirInput.value = config.destDir || '/';

      selectCompressLevel('standard');
      updateCompressPreview();

      modal.classList.add('active');
      setTimeout(() => {
        baseNameInput.focus();
        baseNameInput.select();
      }, 60);
    }

    function closeCompressDialogModal() {
      const modal = document.getElementById('compressDialogModal');
      if (modal) modal.classList.remove('active');
      currentCompressConfig = null;
    }

    function onCompressFormatChanged() {
      const select = document.getElementById('compressArchiveFormat');
      const customBox = document.getElementById('compressCustomExtBox');
      if (select.value === 'custom') {
        customBox.style.display = 'block';
        document.getElementById('compressCustomExtInput').focus();
      } else {
        customBox.style.display = 'none';
      }
      updateCompressPreview();
    }

    function getChosenCompressExtension() {
      const select = document.getElementById('compressArchiveFormat');
      if (select.value === 'custom') {
        let custom = (document.getElementById('compressCustomExtInput').value || '').trim();
        if (!custom) return '.zip';
        if (!custom.startsWith('.')) custom = '.' + custom;
        return custom;
      }
      return select.value;
    }

    function updateCompressPreview() {
      const baseNameInput = document.getElementById('compressArchiveBaseName');
      const preview = document.getElementById('compressFullFilenamePreview');
      let base = (baseNameInput.value || '').trim();
      if (!base) base = 'archive';
      base = base.replace(/\.(zip|tar\.gz|tgz|tar\.bz2|tbz2|tar\.xz|txz|tar|gz)$/i, '');
      const ext = getChosenCompressExtension();
      preview.textContent = base + ext;
    }

    function selectCompressLevel(level) {
      const radios = document.getElementsByName('compressLevel');
      for (const r of radios) {
        r.checked = (r.value === level);
        const pill = r.closest('.compress-level-pill');
        if (pill) {
          if (r.value === level) {
            pill.classList.add('active');
            pill.style.borderColor = 'var(--accent)';
          } else {
            pill.classList.remove('active');
            pill.style.borderColor = 'var(--border-color)';
          }
        }
      }
    }

    async function submitCompressDialogModal() {
      if (!currentCompressConfig) return;
      const baseNameInput = document.getElementById('compressArchiveBaseName');
      let base = (baseNameInput.value || '').trim();
      if (!base) {
        alert('Please enter an archive name.');
        baseNameInput.focus();
        return;
      }
      base = base.replace(/\.(zip|tar\.gz|tgz|tar\.bz2|tbz2|tar\.xz|txz|tar|gz)$/i, '');
      const ext = getChosenCompressExtension();
      const archiveFilename = base + ext;

      let chosenLevel = 'standard';
      const levelRadios = document.getElementsByName('compressLevel');
      for (const r of levelRadios) {
        if (r.checked) { chosenLevel = r.value; break; }
      }

      const config = currentCompressConfig;
      closeCompressDialogModal();

      if (config.scope === 'local') {
        const destZipPath = joinLocalPath(config.destDir, archiveFilename);
        openArchiveModal('compress', config.summaryText, destZipPath, false);
        try {
          addLog(`Compressing ${config.sources.length} local item(s) to '${archiveFilename}' (${chosenLevel})...`, 'info');
          const res = await sendRust('local_compress', {
            sources: config.sources,
            dest_zip: destZipPath,
            level: chosenLevel
          });
          addLog(`Successfully compressed ${res.count || config.sources.length} item(s) to ${archiveFilename}`, 'success');
          await refreshLocal();
          setTimeout(() => closeArchiveModal(), 1200);
        } catch (err) {
          const msg = err.message || String(err);
          alert('Local compression failed: ' + msg);
          addLog('Local compression error: ' + msg, 'error');
          closeArchiveModal();
        }
      } else {
        // Remote compression
        const destZipPath = config.destDir === '/' ? '/' + archiveFilename : config.destDir + '/' + archiveFilename;
        openArchiveModal('compress', config.summaryText, destZipPath, false);
        try {
          addLog(`Compressing ${config.sources.length} remote item(s) to '${archiveFilename}' (${chosenLevel})...`, 'info');
          const res = await sendRust('remote_compress', {
            sources: config.sources,
            dest_zip: destZipPath,
            level: chosenLevel
          });
          addLog(`Successfully created ${archiveFilename} with ${res.count || config.sources.length} item(s) (${res.method || 'remote'})`, 'success');
          await refreshRemote();
          setTimeout(() => closeArchiveModal(), 1200);
        } catch (err) {
          const msg = err.message || String(err);
          alert('Remote compression failed: ' + msg);
          addLog('Remote compression error: ' + msg, 'error');
          closeArchiveModal();
        }
      }
    }

    function promptCompressLocalSelected(targetPath, targetName) {
      closeContextMenu();
      let sources = [];
      let defaultBaseName = 'archive';

      const selected = getSelectedLocalItems();
      if (selected.length > 1 && selected.some(s => s.path === targetPath)) {
        sources = selected.map(s => s.path);
        defaultBaseName = selected[0].name.replace(/\.[^/.]+$/, '') + '_compressed';
      } else if (targetPath && targetName) {
        sources = [targetPath];
        defaultBaseName = targetName.replace(/\.[^/.]+$/, '');
      } else if (selected.length > 0) {
        sources = selected.map(s => s.path);
        defaultBaseName = selected[0].name.replace(/\.[^/.]+$/, '');
      }

      if (sources.length === 0) {
        alert('Please select one or more files or folders to compress.');
        return;
      }

      openCompressDialog({
        scope: 'local',
        sources: sources,
        defaultBaseName: defaultBaseName,
        destDir: currentLocalPath,
        summaryText: sources.length === 1 ? (targetName || defaultBaseName) : `${sources.length} selected local items`
      });
    }

    function compressSelectedLocalToolbar() {
      const selected = getSelectedLocalItems();
      if (selected.length === 0) {
        alert('Please select one or more local files or folders from the list to compress.');
        return;
      }
      const defaultBase = selected.length === 1 ? selected[0].name.replace(/\.[^/.]+$/, '') : (selected[0].name.replace(/\.[^/.]+$/, '') + '_archive');
      openCompressDialog({
        scope: 'local',
        sources: selected.map(s => s.path),
        defaultBaseName: defaultBase,
        destDir: currentLocalPath,
        summaryText: selected.length === 1 ? selected[0].name : `${selected.length} selected local items`
      });
    }

    function compressCurrentLocalDirectory() {
      closeContextMenu();
      if (!localFiles || localFiles.length === 0) {
        alert('Current directory is empty.');
        return;
      }
      const dirName = currentLocalPath.split(/[\\/]/).filter(Boolean).pop() || 'folder';
      openCompressDialog({
        scope: 'local',
        sources: localFiles.map(f => f.path),
        defaultBaseName: dirName,
        destDir: currentLocalPath,
        summaryText: `All items in current folder (${localFiles.length} items)`
      });
    }

    async function extractLocalHere(archivePath) {
      closeContextMenu();
      if (!archivePath) return;
      const fileName = archivePath.split(/[\\/]/).pop() || 'archive.zip';
      try {
        addLog(`Extracting ${fileName} to current directory...`, 'info');
        const res = await sendRust('local_extract', {
          archive_path: archivePath,
          dest_dir: currentLocalPath
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) from ${fileName}`, 'success');
        await refreshLocal();
      } catch (err) {
        const msg = err.message || String(err);
        alert('Extraction failed: ' + msg);
        addLog('Extraction error: ' + msg, 'error');
      }
    }

    async function extractLocalToSubfolder(archivePath, archiveName) {
      closeContextMenu();
      if (!archivePath) return;
      const subName = (archiveName || archivePath.split(/[\\/]/).pop() || 'extracted').replace(/\.[^/.]+$/, '');
      const destDir = joinLocalPath(currentLocalPath, subName);

      try {
        addLog(`Extracting ${archiveName || 'archive'} to subfolder /${subName}...`, 'info');
        const res = await sendRust('local_extract', {
          archive_path: archivePath,
          dest_dir: destDir
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) into /${subName}`, 'success');
        await refreshLocal();
      } catch (err) {
        const msg = err.message || String(err);
        alert('Extraction failed: ' + msg);
        addLog('Extraction error: ' + msg, 'error');
      }
    }

    async function promptExtractLocalFolder(archivePath) {
      closeContextMenu();
      if (!archivePath) return;
      const defaultDest = currentLocalPath;
      const targetDir = prompt('Extract archive to destination folder path:', defaultDest);
      if (!targetDir || !targetDir.trim()) return;

      try {
        addLog(`Extracting to ${targetDir.trim()}...`, 'info');
        const res = await sendRust('local_extract', {
          archive_path: archivePath,
          dest_dir: targetDir.trim()
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) into ${targetDir.trim()}`, 'success');
        await refreshLocal();
      } catch (err) {
        const msg = err.message || String(err);
        alert('Extraction failed: ' + msg);
        addLog('Extraction error: ' + msg, 'error');
      }
    }

    async function extractSelectedLocalToolbar() {
      const selected = getSelectedLocalItems();
      const archive = selected.find(s => !s.is_dir && s.name.toLowerCase().endsWith('.zip')) || (selectedLocalFile && !selectedLocalFile.is_dir && selectedLocalFile.name.toLowerCase().endsWith('.zip') ? selectedLocalFile : null);

      if (!archive) {
        alert('Please select a .zip archive file in the local file list to extract.');
        return;
      }

      await extractLocalToSubfolder(archive.path, archive.name);
    }

    function getSelectedRemoteItems() {
      const selectedRows = document.querySelectorAll('#remoteFileList tr.selected:not(.parent-dir-row)');
      const items = [];
      selectedRows.forEach(r => {
        const idx = parseInt(r.getAttribute('data-idx'));
        if (!isNaN(idx) && remoteFiles[idx]) {
          items.push(remoteFiles[idx]);
        }
      });
      if (items.length === 0 && selectedRemoteFile && remoteFiles.some(f => f.path === selectedRemoteFile.path)) {
        items.push(selectedRemoteFile);
      }
      return items;
    }

    function isArchiveFilenameJs(name) {
      if (!name) return false;
      const l = name.toLowerCase();
      return l.endsWith('.zip') || l.endsWith('.tar.gz') || l.endsWith('.tgz') || l.endsWith('.tar.bz2') || l.endsWith('.tbz2') || l.endsWith('.tar.xz') || l.endsWith('.txz') || l.endsWith('.tar') || l.endsWith('.gz');
    }

    let currentArchiveOperationActive = false;

    window.onArchiveProgress = function(data) {
      if (!data) return;
      const statusBadge = document.getElementById('archiveStatusBadge');
      const itemText = document.getElementById('archiveCurrentItemText');
      const progressBar = document.getElementById('archiveProgressBar');
      const percentText = document.getElementById('archivePercentText');
      const countProcessed = document.getElementById('archiveCountProcessed');
      const countSkipped = document.getElementById('archiveCountSkipped');
      const countStatus = document.getElementById('archiveCountStatus');

      if (data.status && statusBadge) statusBadge.textContent = data.status;
      if (data.currentItem && itemText) itemText.textContent = data.currentItem;

      if (typeof data.count === 'number' && countProcessed) {
        countProcessed.textContent = data.count;
      }
      if (typeof data.skipped === 'number' && countSkipped) {
        countSkipped.textContent = data.skipped;
      }

      if (typeof data.percent === 'number' && data.percent >= 0) {
        const p = Math.min(100, Math.max(0, data.percent));
        if (progressBar) progressBar.style.width = p + '%';
        if (percentText) percentText.textContent = Math.round(p) + '%';
      } else {
        if (progressBar) progressBar.style.width = '100%';
        if (percentText) percentText.textContent = 'Active...';
      }

      if (data.percent === 100) {
        if (countStatus) {
          countStatus.textContent = 'Completed';
          countStatus.style.color = '#22c55e';
        }
        const doneBtn = document.getElementById('archiveDoneBtn');
        const cancelBtn = document.getElementById('archiveCancelBtn');
        if (doneBtn) doneBtn.style.display = 'inline-flex';
        if (cancelBtn) cancelBtn.style.display = 'none';
      }
    };

    function openArchiveModal(mode, sourceDisplay, targetDisplay, isExtract = false) {
      const modal = document.getElementById('archiveProgressModal');
      if (!modal) return;
      currentArchiveOperationActive = true;

      const icon = document.getElementById('archiveModalIcon');
      const title = document.getElementById('archiveModalTitle');
      const subtitle = document.getElementById('archiveModalSubtitle');
      const srcDisp = document.getElementById('archiveSourceDisplay');
      const tgtDisp = document.getElementById('archiveTargetDisplay');
      const policyBox = document.getElementById('archiveExtractionPolicyBox');
      const statusBadge = document.getElementById('archiveStatusBadge');
      const itemText = document.getElementById('archiveCurrentItemText');
      const progressBar = document.getElementById('archiveProgressBar');
      const percentText = document.getElementById('archivePercentText');
      const countProcessed = document.getElementById('archiveCountProcessed');
      const countSkipped = document.getElementById('archiveCountSkipped');
      const countStatus = document.getElementById('archiveCountStatus');
      const doneBtn = document.getElementById('archiveDoneBtn');
      const cancelBtn = document.getElementById('archiveCancelBtn');

      if (isExtract) {
        if (icon) icon.innerHTML = '<svg class="icon icon-lg" viewBox="0 0 24 24"><line x1="16.5" y1="9.4" x2="7.5" y2="4.21"/><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>';
        if (title) title.textContent = 'Remote Archive Extraction';
        if (subtitle) subtitle.textContent = 'Extracting archive directly on remote server';
        if (policyBox) policyBox.style.display = 'flex';
      } else {
        if (icon) icon.innerHTML = '<svg class="icon icon-lg" viewBox="0 0 24 24"><polyline points="21 8 21 21 3 21 3 8"/><rect x="1" y="3" width="22" height="5"/><line x1="10" y1="12" x2="14" y2="12"/></svg>';
        if (title) title.textContent = 'Remote Archive Compression';
        if (subtitle) subtitle.textContent = 'Compressing remote files into archive';
        if (policyBox) policyBox.style.display = 'none';
      }

      if (srcDisp) srcDisp.textContent = sourceDisplay || '-';
      if (tgtDisp) tgtDisp.textContent = targetDisplay || '-';
      if (statusBadge) statusBadge.textContent = 'Starting...';
      if (itemText) itemText.textContent = 'Preparing archive operation...';
      if (progressBar) progressBar.style.width = '0%';
      if (percentText) percentText.textContent = '0%';
      if (countProcessed) countProcessed.textContent = '0';
      if (countSkipped) countSkipped.textContent = '0';
      if (countStatus) {
        countStatus.textContent = 'Active';
        countStatus.style.color = '#22c55e';
      }
      if (doneBtn) doneBtn.style.display = 'none';
      if (cancelBtn) cancelBtn.style.display = 'inline-flex';

      modal.classList.add('active');
    }

    function closeArchiveModal() {
      const modal = document.getElementById('archiveProgressModal');
      if (modal) modal.classList.remove('active');
      currentArchiveOperationActive = false;
    }

    async function cancelArchiveOperation() {
      if (!currentArchiveOperationActive) {
        closeArchiveModal();
        return;
      }
      try {
        await sendRust('cancel_archive', {});
        addLog('Cancelling archive operation...', 'warning');
      } catch (err) {
        console.warn('cancel_archive error:', err);
      }
      closeArchiveModal();
    }

    function getSelectedOverwriteMode() {
      const radios = document.getElementsByName('archiveOverwriteMode');
      for (const r of radios) {
        if (r.checked) return r.value;
      }
      return 'overwrite';
    }

    function remoteCompressSelected(path, name) {
      closeContextMenu();
      if (!isConnected) {
        alert('Please connect to a remote server first.');
        return;
      }
      const defaultBase = (name ? name.replace(/[\\/]/g, '').replace(/\.[^/.]+$/, '') : 'archive');
      openCompressDialog({
        scope: 'remote',
        sources: [path],
        defaultBaseName: defaultBase,
        destDir: currentRemotePath,
        summaryText: name || path
      });
    }

    function remoteCompressToolbar() {
      if (!isConnected) {
        alert('Please connect to a remote server first.');
        return;
      }
      const selected = getSelectedRemoteItems();
      if (selected.length === 0) {
        if (selectedRemoteFile) {
          remoteCompressSelected(selectedRemoteFile.path, selectedRemoteFile.name);
          return;
        }
        alert('Please select one or more remote files or folders to compress.');
        return;
      }
      const defaultBase = (selected[0].name ? selected[0].name.replace(/[\\/]/g, '').replace(/\.[^/.]+$/, '') : 'archive');
      openCompressDialog({
        scope: 'remote',
        sources: selected.map(s => s.path),
        defaultBaseName: defaultBase,
        destDir: currentRemotePath,
        summaryText: selected.length === 1 ? selected[0].name : `${selected.length} selected remote items`
      });
    }

    function remoteCompressCurrentDirectory() {
      closeContextMenu();
      if (!isConnected) {
        alert('Please connect to a remote server first.');
        return;
      }
      if (!remoteFiles || remoteFiles.length === 0) {
        alert('Current remote directory is empty.');
        return;
      }
      const dirName = currentRemotePath.split('/').filter(Boolean).pop() || 'remote_files';
      openCompressDialog({
        scope: 'remote',
        sources: remoteFiles.map(f => f.path),
        defaultBaseName: dirName,
        destDir: currentRemotePath,
        summaryText: `All items in current remote folder (${remoteFiles.length} items)`
      });
    }

    async function remoteExtractHere(archivePath) {
      closeContextMenu();
      if (!archivePath) return;
      const fileName = archivePath.split('/').pop() || 'archive.zip';
      const targetDir = currentRemotePath;
      const overwriteMode = getSelectedOverwriteMode();

      openArchiveModal('extract', fileName, targetDir, true);

      try {
        addLog(`Extracting remote archive '${fileName}' to '${targetDir}' (${overwriteMode})...`, 'info');
        const res = await sendRust('remote_extract', {
          archive_path: archivePath,
          dest_dir: targetDir,
          overwrite_mode: overwriteMode
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) (Skipped: ${res.skipped || 0}) from ${fileName} (${res.method || 'remote'})`, 'success');
        await refreshRemote();
        setTimeout(() => closeArchiveModal(), 1500);
      } catch (err) {
        const msg = err.message || String(err);
        alert('Remote extraction failed: ' + msg);
        addLog('Remote extraction error: ' + msg, 'error');
        closeArchiveModal();
      }
    }

    async function remoteExtractToSubfolder(archivePath, archiveName) {
      closeContextMenu();
      if (!archivePath) return;
      const base = archiveName || archivePath.split('/').pop() || 'archive';
      const subName = base.replace(/\.(zip|tar\.gz|tgz|tar\.bz2|tbz2|tar\.xz|txz|tar|gz)$/i, '');
      const targetDir = currentRemotePath === '/' ? '/' + subName : currentRemotePath + '/' + subName;
      const overwriteMode = getSelectedOverwriteMode();

      openArchiveModal('extract', base, targetDir, true);

      try {
        addLog(`Extracting remote archive '${base}' to subfolder '/${subName}' (${overwriteMode})...`, 'info');
        const res = await sendRust('remote_extract', {
          archive_path: archivePath,
          dest_dir: targetDir,
          overwrite_mode: overwriteMode
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) (Skipped: ${res.skipped || 0}) into /${subName}`, 'success');
        await refreshRemote();
        setTimeout(() => closeArchiveModal(), 1500);
      } catch (err) {
        const msg = err.message || String(err);
        alert('Remote extraction failed: ' + msg);
        addLog('Remote extraction error: ' + msg, 'error');
        closeArchiveModal();
      }
    }

    async function remoteExtractCustomPath(archivePath) {
      closeContextMenu();
      if (!archivePath) return;
      const fileName = archivePath.split('/').pop() || 'archive.zip';
      const defaultDest = currentRemotePath;
      const targetDir = prompt('Extract remote archive to destination path on server:', defaultDest);
      if (!targetDir || !targetDir.trim()) return;

      const cleanDest = targetDir.trim();
      const overwriteMode = getSelectedOverwriteMode();

      openArchiveModal('extract', fileName, cleanDest, true);

      try {
        addLog(`Extracting remote archive '${fileName}' to '${cleanDest}' (${overwriteMode})...`, 'info');
        const res = await sendRust('remote_extract', {
          archive_path: archivePath,
          dest_dir: cleanDest,
          overwrite_mode: overwriteMode
        });
        addLog(`Successfully extracted ${res.count || 0} item(s) (Skipped: ${res.skipped || 0}) into ${cleanDest}`, 'success');
        await refreshRemote();
        setTimeout(() => closeArchiveModal(), 1500);
      } catch (err) {
        const msg = err.message || String(err);
        alert('Remote extraction failed: ' + msg);
        addLog('Remote extraction error: ' + msg, 'error');
        closeArchiveModal();
      }
    }

    async function remoteExtractSelectedToolbar() {
      if (!isConnected) {
        alert('Please connect to a remote server first.');
        return;
      }
      const selected = getSelectedRemoteItems();
      const archive = selected.find(s => !s.is_dir && isArchiveFilenameJs(s.name))
        || (selectedRemoteFile && !selectedRemoteFile.is_dir && isArchiveFilenameJs(selectedRemoteFile.name) ? selectedRemoteFile : null);

      if (!archive) {
        alert('Please select an archive file (.zip, .tar.gz, .tgz, .tar) in the remote list to extract.');
        return;
      }

      await remoteExtractToSubfolder(archive.path, archive.name);
    }

    // CHMOD Modal
    let currentChmodTargets = [];

    function parseUnixPermToOctal(permStr, isDir) {
      if (!permStr) return isDir ? '0755' : '0644';
      const clean = permStr.trim();
      if (/^0?[0-7]{3,4}$/.test(clean)) {
        return clean.length === 3 ? '0' + clean : clean;
      }
      if (clean.length >= 10) {
        let u = 0, g = 0, o = 0;
        if (clean[1] === 'r') u |= 4;
        if (clean[2] === 'w') u |= 2;
        if (clean[3] === 'x' || clean[3] === 's') u |= 1;
        if (clean[4] === 'r') g |= 4;
        if (clean[5] === 'w') g |= 2;
        if (clean[6] === 'x' || clean[6] === 's') g |= 1;
        if (clean[7] === 'r') o |= 4;
        if (clean[8] === 'w') o |= 2;
        if (clean[9] === 'x' || clean[9] === 't') o |= 1;
        return `0${u}${g}${o}`;
      }
      return isDir ? '0755' : '0644';
    }

    function openChmodForItem(path, name, perms) {
      closeContextMenu();
      const selected = getSelectedRemoteItems();
      const inSelected = selected.some(i => i.path === path);
      if (inSelected && selected.length > 1) {
        currentChmodTargets = selected;
      } else {
        const found = remoteFiles.find(f => f.path === path);
        currentChmodTargets = [found || { path, name, permissions: perms, is_dir: false }];
      }
      showChmodModalWithTargets(currentChmodTargets);
    }

    function openChmodModal() {
      const selected = getSelectedRemoteItems();
      if (selected.length === 0 && !selectedRemoteFile) {
        alert('Please select at least one remote file or folder first');
        return;
      }
      currentChmodTargets = selected.length > 0 ? selected : [selectedRemoteFile];
      showChmodModalWithTargets(currentChmodTargets);
    }

    function showChmodModalWithTargets(targets) {
      if (!targets || targets.length === 0) return;
      const first = targets[0];
      const labelEl = document.getElementById('chmodFilename');
      if (labelEl) {
        labelEl.innerText = targets.length > 1
          ? `${targets.length} selected items (${first.name}, ...)`
          : first.path;
      }
      const hasDir = targets.some(t => t.is_dir);
      const recCheck = document.getElementById('chmodRecursiveCheck');
      if (recCheck) {
        recCheck.checked = false;
        recCheck.disabled = !hasDir;
      }
      const octal = parseUnixPermToOctal(first.permissions, first.is_dir);
      document.getElementById('chmodOctal').value = octal;
      updateCheckboxesFromOctal(octal);
      document.getElementById('chmodModal').classList.add('active');
    }

    function closeChmodModal() {
      document.getElementById('chmodModal').classList.remove('active');
    }

    function updateCheckboxesFromOctal(octalStr) {
      const val = parseInt(octalStr.replace('0o', ''), 8) || 0o644;
      document.getElementById('cm_ur').checked = (val & 0o400) !== 0;
      document.getElementById('cm_uw').checked = (val & 0o200) !== 0;
      document.getElementById('cm_ux').checked = (val & 0o100) !== 0;
      document.getElementById('cm_gr').checked = (val & 0o040) !== 0;
      document.getElementById('cm_gw').checked = (val & 0o020) !== 0;
      document.getElementById('cm_gx').checked = (val & 0o010) !== 0;
      document.getElementById('cm_or').checked = (val & 0o004) !== 0;
      document.getElementById('cm_ow').checked = (val & 0o002) !== 0;
      document.getElementById('cm_ox').checked = (val & 0o001) !== 0;
    }

    function calculateOctalFromCheckboxes() {
      let val = 0;
      if (document.getElementById('cm_ur').checked) val |= 0o400;
      if (document.getElementById('cm_uw').checked) val |= 0o200;
      if (document.getElementById('cm_ux').checked) val |= 0o100;
      if (document.getElementById('cm_gr').checked) val |= 0o040;
      if (document.getElementById('cm_gw').checked) val |= 0o020;
      if (document.getElementById('cm_gx').checked) val |= 0o010;
      if (document.getElementById('cm_or').checked) val |= 0o004;
      if (document.getElementById('cm_ow').checked) val |= 0o002;
      if (document.getElementById('cm_ox').checked) val |= 0o001;
      return '0' + val.toString(8);
    }

    ['cm_ur','cm_uw','cm_ux','cm_gr','cm_gw','cm_gx','cm_or','cm_ow','cm_ox'].forEach(id => {
      const el = document.getElementById(id);
      if (el) el.addEventListener('change', () => {
        const oct = document.getElementById('chmodOctal');
        if (oct) oct.value = calculateOctalFromCheckboxes();
      });
    });

    function updateChmodFromOctal() {
      const oct = document.getElementById('chmodOctal').value;
      updateCheckboxesFromOctal(oct);
    }

    async function applyChmod() {
      const mode = document.getElementById('chmodOctal').value;
      const recursive = !!(document.getElementById('chmodRecursiveCheck')?.checked);
      const targets = (currentChmodTargets && currentChmodTargets.length > 0)
        ? currentChmodTargets
        : (selectedRemoteFile ? [selectedRemoteFile] : []);
      if (targets.length === 0) return;
      try {
        const res = await sendRust('remote_chmod', {
          path: targets[0].path,
          paths: targets.map(t => t.path),
          mode: mode,
          recursive: recursive,
          current_dir: currentRemotePath || '/'
        });
        renderRemoteFiles(res.files || []);
        addLog(`Updated permissions for ${targets.length} item(s) to ${mode}${recursive ? ' (recursive)' : ''}`, 'success');
        closeChmodModal();
      } catch (err) {
        addLog('Failed to change permissions: ' + err.message, 'error');
        alert('CHMOD Error:\n' + err.message);
      }
    }

    // ==========================================
    // 1. TRANSFER EXCLUSION MASK ENGINE
    // ==========================================
    const DEFAULT_EXCLUSION_RULES = [
      'node_modules', '.git', '.svn', '*.log', '.env', '.env.*',
      'Thumbs.db', '.DS_Store', '*.tmp', '*.bak', '__pycache__', '.idea', '.vscode'
    ];

    let exclusionMaskState = {
      enabled: true,
      rules: [...DEFAULT_EXCLUSION_RULES]
    };

    function loadExclusionMaskConfig() {
      try {
        const stored = localStorage.getItem('90ftp_exclusion_mask');
        if (stored) {
          const parsed = JSON.parse(stored);
          if (typeof parsed.enabled === 'boolean') exclusionMaskState.enabled = parsed.enabled;
          if (Array.isArray(parsed.rules)) exclusionMaskState.rules = parsed.rules;
        }
      } catch (_) {}
      updateExclusionMaskUI();
    }

    function saveExclusionMaskConfig() {
      try {
        localStorage.setItem('90ftp_exclusion_mask', JSON.stringify(exclusionMaskState));
      } catch (_) {}
      updateExclusionMaskUI();
    }

    function updateExclusionMaskUI() {
      const btn = document.getElementById('btnToolbarMask');
      if (btn) {
        btn.style.color = exclusionMaskState.enabled ? '#38bdf8' : 'var(--text-dim)';
        btn.title = `Transfer Exclusion Mask: ${exclusionMaskState.enabled ? 'ACTIVE (' + exclusionMaskState.rules.length + ' rules)' : 'DISABLED'}`;
      }
      const optCheck = document.getElementById('optMaskEnabled');
      if (optCheck) optCheck.checked = exclusionMaskState.enabled;
    }

    function openExclusionMaskModal() {
      const modal = document.getElementById('exclusionMaskModal');
      if (!modal) return;
      document.getElementById('maskEnabledCheckbox').checked = exclusionMaskState.enabled;
      renderMaskRulesBadges();
      modal.classList.add('active');
    }

    function closeExclusionMaskModal() {
      const modal = document.getElementById('exclusionMaskModal');
      if (modal) modal.classList.remove('active');
    }

    function toggleMaskEnabledFromModal() {
      exclusionMaskState.enabled = document.getElementById('maskEnabledCheckbox').checked;
      updateExclusionMaskUI();
    }

    function renderMaskRulesBadges() {
      const container = document.getElementById('maskRulesTagContainer');
      if (!container) return;
      container.innerHTML = '';
      if (exclusionMaskState.rules.length === 0) {
        container.innerHTML = '<span style="color: var(--text-dim); font-style: italic; font-size: 11.5px;">No exclusion rules defined (all files will be transferred)</span>';
        return;
      }
      exclusionMaskState.rules.forEach((rule, idx) => {
        const tag = document.createElement('span');
        tag.className = 'mask-rule-badge';
        tag.innerHTML = `<span>${escapeHtml(rule)}</span><span class="remove-btn" onclick="removeExclusionRule(${idx})" title="Remove rule">✕</span>`;
        container.appendChild(tag);
      });
    }

    function removeExclusionRule(idx) {
      exclusionMaskState.rules.splice(idx, 1);
      renderMaskRulesBadges();
    }

    function addExclusionRuleFromInput() {
      const input = document.getElementById('maskNewRuleInput');
      if (!input) return;
      const val = input.value.trim();
      if (!val) return;
      const parts = val.split(/[,\s]+/).map(s => s.trim()).filter(Boolean);
      for (const p of parts) {
        if (!exclusionMaskState.rules.includes(p)) {
          exclusionMaskState.rules.push(p);
        }
      }
      input.value = '';
      renderMaskRulesBadges();
    }

    function applyMaskPreset(type) {
      if (type === 'web') {
        exclusionMaskState.rules = ['node_modules', '.git', '.env', '.env.*', '*.log', 'vendor', '.DS_Store', 'Thumbs.db'];
      } else if (type === 'python') {
        exclusionMaskState.rules = ['__pycache__', '.venv', 'venv', '*.pyc', '*.pyo', '.pytest_cache', '.mypy_cache', '.env'];
      } else {
        exclusionMaskState.rules = [...DEFAULT_EXCLUSION_RULES];
      }
      renderMaskRulesBadges();
    }

    function saveExclusionMaskFromModal() {
      exclusionMaskState.enabled = document.getElementById('maskEnabledCheckbox').checked;
      saveExclusionMaskConfig();
      closeExclusionMaskModal();
      addLog(`[Exclusion Mask] Configuration saved: ${exclusionMaskState.enabled ? 'ON' : 'OFF'} (${exclusionMaskState.rules.length} rules active)`, 'info');
    }

    function getActiveExclusionRules() {
      return exclusionMaskState.enabled ? exclusionMaskState.rules : [];
    }

    function isItemExcluded(name, relPath) {
      if (!exclusionMaskState.enabled) return false;
      const nLower = (name || '').toLowerCase();
      const rLower = (relPath || name || '').toLowerCase().replace(/\\/g, '/');
      for (const rule of exclusionMaskState.rules) {
        const pat = rule.trim().replace(/\/+$/, '').toLowerCase();
        if (!pat) continue;
        if (pat.startsWith('*.')) {
          if (nLower.endsWith(pat.substring(1))) return true;
        } else if (pat.includes('*')) {
          const parts = pat.split('*');
          if (parts.length === 2 && nLower.startsWith(parts[0]) && nLower.endsWith(parts[1])) return true;
        } else {
          if (nLower === pat) return true;
          const segs = rLower.split('/');
          if (segs.includes(pat)) return true;
        }
      }
      return false;
    }

    // ==========================================
    // 2. DUAL-SCOPE FIND FILES ENGINE (REMOTE + LOCAL)
    // ==========================================
    let findFilesResults = [];

    function openFindFilesModal() {
      const modal = document.getElementById('findFilesModal');
      if (!modal) return;
      const scopeSel = document.getElementById('findScopeSelect');
      if (scopeSel) {
        scopeSel.value = isConnected ? 'remote' : 'local';
      }
      onFindScopeChange();
      const qInput = document.getElementById('findSearchQueryInput');
      if (qInput) qInput.value = '';
      clearFindResults();
      modal.classList.add('active');
      setTimeout(() => qInput && qInput.focus(), 60);
    }

    function onFindScopeChange() {
      const scope = document.getElementById('findScopeSelect')?.value || 'remote';
      const pathInput = document.getElementById('findSearchPathInput');
      if (!pathInput) return;
      if (scope === 'local') {
        pathInput.value = currentLocalPath || 'C:\\';
      } else {
        pathInput.value = currentRemotePath || '/';
      }
    }

    function closeFindFilesModal() {
      const modal = document.getElementById('findFilesModal');
      if (modal) modal.classList.remove('active');
    }

    function setFindPathCurrent() {
      const scope = document.getElementById('findScopeSelect')?.value || 'remote';
      document.getElementById('findSearchPathInput').value = scope === 'local' ? (currentLocalPath || 'C:\\') : (currentRemotePath || '/');
    }

    function setFindPathRoot() {
      const scope = document.getElementById('findScopeSelect')?.value || 'remote';
      if (scope === 'local') {
        const m = (currentLocalPath || 'C:\\').match(/^[A-Za-z]:\\/);
        document.getElementById('findSearchPathInput').value = m ? m[0] : 'C:\\';
      } else {
        document.getElementById('findSearchPathInput').value = '/';
      }
    }

    function clearFindResults() {
      findFilesResults = [];
      const tbody = document.getElementById('findResultsTableBody');
      if (tbody) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 40px; font-style: italic;">Enter search query and click Search Files</td></tr>`;
      }
      const summary = document.getElementById('findResultsSummary');
      if (summary) summary.innerText = 'Ready to search';
    }

    async function executeRemoteFind() {
      const scope = document.getElementById('findScopeSelect')?.value || 'remote';
      if (scope === 'remote' && !isConnected) {
        alert('Not connected to a remote server. Switch Scope to "Local Computer" or connect to a server first.');
        return;
      }
      const path = (document.getElementById('findSearchPathInput').value || (scope === 'local' ? currentLocalPath : '/')).trim();
      const query = (document.getElementById('findSearchQueryInput').value || '').trim();
      const recursive = document.getElementById('findRecursiveCheckbox').checked;
      const file_type = document.getElementById('findTypeSelect').value;

      if (!query) {
        alert('Please enter a file name pattern to search (e.g. *.php or config)');
        return;
      }

      const spinner = document.getElementById('findSpinner');
      const btn = document.getElementById('btnExecuteFind');
      const summary = document.getElementById('findResultsSummary');
      if (spinner) spinner.style.display = 'inline-block';
      if (btn) btn.disabled = true;
      if (summary) summary.innerText = `Searching ${scope === 'local' ? 'local disk' : 'remote server'} for '${query}' in ${path}...`;

      try {
        const cmd = scope === 'local' ? 'local_find_files' : 'remote_find_files';
        const res = await sendRust(cmd, {
          path,
          query,
          recursive,
          file_type,
          max_results: 400
        });

        findFilesResults = (res.results || []).map(r => ({ ...r, _scope: scope }));
        renderFindResults(findFilesResults);
        if (summary) {
          summary.innerText = `Found ${findFilesResults.length} ${scope} item(s) matching '${query}'`;
        }
      } catch (err) {
        if (summary) summary.innerText = `Search error: ${err.message || err}`;
        alert(`Search Failed:\n${err.message || err}`);
      } finally {
        if (spinner) spinner.style.display = 'none';
        if (btn) btn.disabled = false;
      }
    }

    function renderFindResults(items) {
      const tbody = document.getElementById('findResultsTableBody');
      if (!tbody) return;
      tbody.innerHTML = '';
      if (items.length === 0) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; color: var(--text-dim); padding: 40px; font-style: italic;">No items found matching criteria</td></tr>`;
        return;
      }

      items.forEach((item, idx) => {
        const tr = document.createElement('tr');
        tr.style.cursor = 'pointer';
        tr.style.borderBottom = '1px solid rgba(255,255,255,0.04)';
        tr.ondblclick = () => jumpToFoundRemoteItem(item);
        tr.onmouseenter = () => tr.style.background = 'rgba(56, 189, 248, 0.08)';
        tr.onmouseleave = () => tr.style.background = 'transparent';

        const { svg, iconClass } = getFileIcon(item.name, !!item.is_dir);
        const isLocalScope = item._scope === 'local';

        tr.innerHTML = `
          <td style="padding: 6px 10px;">
            <div style="display: flex; align-items: center; gap: 6px;">
              <span class="file-icon ${iconClass}">${svg}</span>
              <span style="font-weight: 600; color: var(--text-main); word-break: break-all;">${escapeHtml(item.name)}</span>
            </div>
          </td>
          <td style="padding: 6px 10px; color: var(--text-dim); font-family: monospace; font-size: 11px; word-break: break-all;">
            ${escapeHtml(item.dir || item.path)}
          </td>
          <td style="padding: 6px 10px; text-align: right; color: var(--text-muted); font-family: monospace;">
            ${item.is_dir ? 'DIR' : formatBytes(item.size || 0)}
          </td>
          <td style="padding: 6px 10px; text-align: center;">
            <div style="display: flex; gap: 4px; justify-content: center;">
              <button type="button" class="btn btn-secondary btn-sm" onclick="jumpToFoundRemoteItem(findFilesResults[${idx}])" title="Open folder in ${isLocalScope ? 'Local' : 'Remote'} Pane" style="padding: 1px 7px; font-size: 10.5px;">Go</button>
              ${!item.is_dir ? `
                ${!isLocalScope ? `<button type="button" class="btn btn-secondary btn-sm" onclick="downloadFoundItem(findFilesResults[${idx}])" title="Download to local directory" style="padding: 1px 7px; font-size: 10.5px;"><svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></button>` : ''}
                <button type="button" class="btn btn-secondary btn-sm" onclick="editFoundItem(findFilesResults[${idx}])" title="Edit in code editor" style="padding: 1px 7px; font-size: 10.5px;"><svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"/></svg></button>
              ` : ''}
            </div>
          </td>
        `;
        tbody.appendChild(tr);
      });
    }

    async function jumpToFoundRemoteItem(item) {
      if (!item) return;
      closeFindFilesModal();
      const targetDir = item.is_dir ? item.path : item.dir;
      if (item._scope === 'local') {
        await navigateLocal(targetDir);
        setTimeout(() => {
          const rows = document.querySelectorAll('#localFileList tr');
          rows.forEach(r => {
            const nameSpan = r.querySelector('.file-cell span:nth-child(2)');
            if (nameSpan && nameSpan.innerText.trim() === item.name) {
              r.classList.add('selected');
              r.scrollIntoView({ behavior: 'smooth', block: 'center' });
            }
          });
        }, 250);
      } else {
        await navigateRemote(targetDir);
        setTimeout(() => {
          const rows = document.querySelectorAll('#remoteFileList tr');
          rows.forEach(r => {
            const nameSpan = r.querySelector('.file-cell span:nth-child(2)');
            if (nameSpan && nameSpan.innerText.trim() === item.name) {
              r.classList.add('selected');
              r.scrollIntoView({ behavior: 'smooth', block: 'center' });
            }
          });
        }, 300);
      }
    }

    async function downloadFoundItem(item) {
      if (!item || item.is_dir) return;
      closeFindFilesModal();
      downloadFileDirect(item.path, item.name);
    }

    async function editFoundItem(item) {
      if (!item || item.is_dir) return;
      closeFindFilesModal();
      if (item._scope === 'local') {
        openLocalFileInEditor(item.path);
      } else {
        openRemoteFileInEditor(item.path);
      }
    }

    // ==========================================
    // 3. DIRECTORY COMPARISON ENGINE (WinSCP-style)
    // ==========================================
    let compareDirsActive = false;

    function toggleCompareDirectories(forceState) {
      compareDirsActive = typeof forceState === 'boolean' ? forceState : !compareDirsActive;
      const btn = document.getElementById('btnToolbarCompare');
      const bar = document.getElementById('compareBar');

      if (btn) {
        btn.style.color = compareDirsActive ? '#22c55e' : '#a855f7';
      }

      if (compareDirsActive) {
        if (bar) bar.style.display = 'flex';
        updateCompareStats();
        addLog('Directory Comparison activated (WinSCP mode)', 'info');
      } else {
        if (bar) bar.style.display = 'none';
        addLog('Directory Comparison deactivated', 'info');
      }

      renderLocalFiles(localFiles);
      renderRemoteFiles(remoteFiles);
    }

    function getComparisonStatus(item, isRemote) {
      const counterpartList = isRemote ? localFiles : remoteFiles;
      const itemName = (item.name || '').toLowerCase();
      const match = counterpartList.find(f => (f.name || '').toLowerCase() === itemName);

      if (!match) {
        return {
          status: 'missing',
          tag: isRemote ? 'Remote Only' : 'Local Only',
          color: '#22c55e',
          bg: 'rgba(34, 197, 94, 0.15)',
          border: 'rgba(34, 197, 94, 0.35)',
          rowClass: 'tr-compare-missing'
        };
      }

      if (!item.is_dir && !match.is_dir) {
        if (item.size !== match.size) {
          return {
            status: 'diff',
            tag: 'Size Diff',
            color: '#eab308',
            bg: 'rgba(234, 179, 8, 0.15)',
            border: 'rgba(234, 179, 8, 0.35)',
            rowClass: 'tr-compare-diff'
          };
        }
      }

      return {
        status: 'match',
        tag: '',
        color: '#94a3b8',
        bg: 'transparent',
        border: 'transparent',
        rowClass: ''
      };
    }

    function updateCompareStats() {
      const statsEl = document.getElementById('compareStats');
      if (!statsEl) return;

      let localOnly = 0;
      let remoteOnly = 0;
      let sizeDiff = 0;
      let identical = 0;

      const remoteMap = new Map();
      remoteFiles.forEach(f => remoteMap.set((f.name || '').toLowerCase(), f));

      localFiles.forEach(lf => {
        const nameLower = (lf.name || '').toLowerCase();
        const rf = remoteMap.get(nameLower);
        if (!rf) {
          localOnly++;
        } else {
          if (!lf.is_dir && !rf.is_dir && lf.size !== rf.size) {
            sizeDiff++;
          } else {
            identical++;
          }
        }
      });

      const localMap = new Map();
      localFiles.forEach(f => localMap.set((f.name || '').toLowerCase(), f));

      remoteFiles.forEach(rf => {
        const nameLower = (rf.name || '').toLowerCase();
        if (!localMap.has(nameLower)) {
          remoteOnly++;
        }
      });

      statsEl.innerHTML = `
        <span style="color:#22c55e; font-weight:600;">${localOnly} Local Only</span> • 
        <span style="color:#22c55e; font-weight:600;">${remoteOnly} Remote Only</span> • 
        <span style="color:#eab308; font-weight:600;">${sizeDiff} Size Diff</span> • 
        <span style="color:var(--text-dim);">${identical} Matching</span>
      `;
    }

    async function syncLocalToRemoteMissing() {
      if (!isConnected) { alert('Please connect to server first'); return; }
      const remoteMap = new Map();
      remoteFiles.forEach(f => remoteMap.set((f.name || '').toLowerCase(), f));

      const toUpload = localFiles.filter(lf => {
        const rf = remoteMap.get((lf.name || '').toLowerCase());
        return !rf || (!lf.is_dir && !rf.is_dir && lf.size !== rf.size);
      });

      if (toUpload.length === 0) {
        alert('All local files match the remote directory. Nothing to upload.');
        return;
      }

      if (!confirm(`Upload ${toUpload.length} missing/modified file(s) to remote server?`)) return;

      addLog(`Starting sync upload of ${toUpload.length} items to remote...`, 'info');
      for (const item of toUpload) {
        if (item.is_dir) {
          await uploadFolderDirect(item.path, item.name);
        } else {
          await uploadFileDirect(item.path, item.name);
        }
      }
      refreshRemote();
    }

    async function syncRemoteToLocalMissing() {
      if (!isConnected) { alert('Please connect to server first'); return; }
      const localMap = new Map();
      localFiles.forEach(f => localMap.set((f.name || '').toLowerCase(), f));

      const toDownload = remoteFiles.filter(rf => {
        const lf = localMap.get((rf.name || '').toLowerCase());
        return !lf || (!rf.is_dir && !lf.is_dir && rf.size !== lf.size);
      });

      if (toDownload.length === 0) {
        alert('All remote files match the local directory. Nothing to download.');
        return;
      }

      if (!confirm(`Download ${toDownload.length} missing/modified file(s) to local directory?`)) return;

      addLog(`Starting sync download of ${toDownload.length} items to local...`, 'info');
      for (const item of toDownload) {
        if (item.is_dir) {
          await downloadFolderDirect(item.path, item.name);
        } else {
          await downloadFileDirect(item.path, item.name);
        }
      }
      refreshLocal();
    }

    // ==========================================
    // 4. CHECKSUM CALCULATION & VERIFICATION ENGINE
    // ==========================================
    let currentChecksumState = {
      path: '',
      name: '',
      isRemote: false,
      size: 0,
      algo: 'sha256',
      hash: '',
      counterpartPath: '',
      counterpartHash: ''
    };

    async function openChecksumModal(filePath, fileName, isRemote, counterpartPath = '') {
      currentChecksumState.path = filePath;
      currentChecksumState.name = fileName;
      currentChecksumState.isRemote = isRemote;
      currentChecksumState.algo = 'sha256';
      currentChecksumState.hash = '';
      currentChecksumState.counterpartPath = counterpartPath;
      currentChecksumState.counterpartHash = '';

      const modal = document.getElementById('checksumModal');
      if (!modal) return;

      document.getElementById('csFileName').innerText = fileName;
      document.getElementById('csFilePath').innerText = filePath;
      document.getElementById('csFileTarget').innerText = isRemote ? 'Remote Server' : 'Local PC';
      document.getElementById('csFileTarget').style.color = isRemote ? 'var(--accent)' : '#22c55e';
      document.getElementById('csVerifyInput').value = '';
      document.getElementById('csMatchResult').style.display = 'none';

      const fileList = isRemote ? remoteFiles : localFiles;
      const fileObj = fileList.find(f => (f.name || '') === fileName);
      document.getElementById('csFileSize').innerText = fileObj ? formatBytes(fileObj.size || 0) : '-';

      const otherList = isRemote ? localFiles : remoteFiles;
      const counterpart = otherList.find(f => !f.is_dir && (f.name || '').toLowerCase() === fileName.toLowerCase());
      const cpSection = document.getElementById('csCounterpartSection');
      if (counterpart) {
        currentChecksumState.counterpartPath = counterpart.path;
        document.getElementById('csCounterpartName').innerText = `${counterpart.name} (${formatBytes(counterpart.size)})`;
        if (cpSection) cpSection.style.display = 'flex';
      } else {
        if (cpSection) cpSection.style.display = 'none';
      }

      switchChecksumAlgo('sha256', false);
      modal.classList.add('active');
      await computeActiveChecksum();
    }

    function openChecksumCompareModal(filePath, counterpartPath, fileName, isRemote) {
      openChecksumModal(filePath, fileName, isRemote, counterpartPath);
    }

    function closeChecksumModal() {
      const modal = document.getElementById('checksumModal');
      if (modal) modal.classList.remove('active');
    }

    async function switchChecksumAlgo(algo, recompute = true) {
      currentChecksumState.algo = algo;
      const btnSha = document.getElementById('btnAlgoSha256');
      const btnMd5 = document.getElementById('btnAlgoMd5');
      const hashLabel = document.getElementById('csHashLabel');

      if (algo === 'sha256') {
        btnSha.className = 'btn btn-sm btn-primary';
        btnMd5.className = 'btn btn-secondary btn-sm';
        hashLabel.innerText = 'Calculated SHA-256 Hash:';
      } else {
        btnMd5.className = 'btn btn-sm btn-primary';
        btnSha.className = 'btn btn-secondary btn-sm';
        hashLabel.innerText = 'Calculated MD5 Hash:';
      }

      if (recompute) {
        await computeActiveChecksum();
      }
    }

    async function computeActiveChecksum() {
      const spinner = document.getElementById('csComputingSpinner');
      const hashBox = document.getElementById('csHashBox');
      if (spinner) spinner.style.display = 'inline-block';
      if (hashBox) hashBox.innerText = 'Computing hash (please wait)...';

      try {
        const res = await sendRust('calculate_checksum', {
          target: currentChecksumState.isRemote ? 'remote' : 'local',
          path: currentChecksumState.path,
          algorithm: currentChecksumState.algo
        });
        currentChecksumState.hash = res.hash || '';
        if (hashBox) hashBox.innerText = currentChecksumState.hash;
        checkChecksumMatch();
      } catch (err) {
        if (hashBox) hashBox.innerText = `Error: ${err.message || err}`;
      } finally {
        if (spinner) spinner.style.display = 'none';
      }
    }

    function copyChecksumHash() {
      if (!currentChecksumState.hash) return;
      navigator.clipboard.writeText(currentChecksumState.hash).then(() => {
        const btn = document.getElementById('btnCopyHash');
        if (btn) {
          const orig = btn.innerHTML;
          btn.innerHTML = '<span>✓ Copied!</span>';
          setTimeout(() => btn.innerHTML = orig, 1500);
        }
      });
    }

    function checkChecksumMatch() {
      const input = document.getElementById('csVerifyInput');
      const resultBox = document.getElementById('csMatchResult');
      if (!input || !resultBox) return;

      const expected = input.value.trim().toLowerCase();
      const actual = (currentChecksumState.hash || '').toLowerCase();

      if (!expected || !actual) {
        resultBox.style.display = 'none';
        return;
      }

      resultBox.style.display = 'flex';
      if (expected === actual) {
        resultBox.style.background = 'rgba(34, 197, 94, 0.15)';
        resultBox.style.border = '1px solid rgba(34, 197, 94, 0.4)';
        resultBox.style.color = '#22c55e';
        resultBox.innerHTML = `<span>✓</span> <span>HASH MATCHES PERFECTLY! File integrity and authenticity verified.</span>`;
      } else {
        resultBox.style.background = 'rgba(239, 68, 68, 0.15)';
        resultBox.style.border = '1px solid rgba(239, 68, 68, 0.4)';
        resultBox.style.color = '#f87171';
        resultBox.innerHTML = `<span>✗</span> <span>HASH MISMATCH! Expected and calculated checksums do not match.</span>`;
      }
    }

    async function compareWithCounterpartChecksum() {
      if (!currentChecksumState.counterpartPath) return;
      const btn = document.getElementById('btnCompareCounterpart');
      if (btn) btn.innerHTML = '<span class="spinner" style="width:10px; height:10px; display:inline-block;"></span> Computing...';

      try {
        const res = await sendRust('calculate_checksum', {
          target: currentChecksumState.isRemote ? 'local' : 'remote',
          path: currentChecksumState.counterpartPath,
          algorithm: currentChecksumState.algo
        });
        currentChecksumState.counterpartHash = res.hash || '';
        const verifyInput = document.getElementById('csVerifyInput');
        if (verifyInput) {
          verifyInput.value = currentChecksumState.counterpartHash;
          checkChecksumMatch();
        }
      } catch (err) {
        alert(`Failed to compute counterpart checksum: ${err.message || err}`);
      } finally {
        if (btn) btn.innerHTML = '<span>Compute & Compare</span>';
      }
    }

    // Delete and Edit actions
    async function deleteSelectedRemote() {
      await deleteSelectedItems(true);
    }

    async function deleteSelectedLocal() {
      await deleteSelectedItems(false);
    }

    function editSelectedRemoteFile() {
      if (!selectedRemoteFile) {
        alert('Please select a file to edit');
        return;
      }
      if (selectedRemoteFile.is_dir) {
        alert('Selected item is a directory, not a file!');
        return;
      }
      openRemoteFileInEditor(selectedRemoteFile.path);
    }

    async function promptNewRemoteFolder() {
      if (!isConnected) return;
      const name = prompt('Enter new remote folder name:');
      if (!name) return;
      const newPath = currentRemotePath.endsWith('/') ? currentRemotePath + name : currentRemotePath + '/' + name;
      try {
        const res = await sendRust('remote_create_dir', { path: newPath });
        renderRemoteFiles(res.files || []);
        addLog(`Created remote directory: ${name}`, 'success');
      } catch (err) {
        alert('Error creating folder:\n' + err.message);
      }
    }

    async function promptNewLocalFolder() {
      const name = prompt('Enter new local folder name:');
      if (!name) return;
      const newPath = currentLocalPath.endsWith('\\') ? currentLocalPath + name : currentLocalPath + '\\' + name;
      try {
        await sendRust('local_create_dir', { path: newPath });
        refreshLocal();
      } catch (err) {
        alert('Error creating folder:\n' + err.message);
      }
    }

    async function promptNewLocalFile() {
      const name = prompt('Enter new local file name (e.g. index.html):');
      if (!name) return;
      const newPath = currentLocalPath.endsWith('\\') ? currentLocalPath + name : currentLocalPath + '\\' + name;
      try {
        await sendRust('local_create_file', { path: newPath });
        refreshLocal();
      } catch (err) {
        alert('Error creating file:\n' + err.message);
      }
    }


        // Site Manager Modal Logic
    function openSitesModal() {
      renderSitesList();
      refreshVaultKeySelect();
      const n = document.getElementById('siteModalNotice');
      if (n) n.className = 'site-notice';
      document.getElementById('sitesModal').classList.add('active');
      sendRust('get_storage_info').then(info => {
        if (info && info.path) {
          const lbl = document.getElementById('storagePathLabel');
          if (lbl) lbl.textContent = info.path;
        }
      }).catch(() => {});
    }

    function closeSitesModal() {
      document.getElementById('sitesModal').classList.remove('active');
    }

    async function browseSshKeyFile() {
      try {
        const res = await sendRust('pick_ssh_key', {});
        if (res && res.path) {
          document.getElementById('siteKeyPath').value = res.path;
          addLog(`Selected SSH Key: ${res.path}`, 'info');
        }
      } catch (err) {
        addLog(`Error picking SSH Key file: ${err.message}`, 'error');
      }
    }

    async function openStorageFolder() {
      try {
        await sendRust('open_storage_folder', {});
        addLog('Opened configuration directory in Explorer', 'info');
      } catch (err) {
        addLog(`Error opening folder: ${err.message}`, 'error');
      }
    }

    let selectedSiteId = null;

    function switchSiteAuth(type) {
      document.getElementById('siteAuthType').value = type;
      const tabPass = document.getElementById('tabAuthPass');
      const tabKey = document.getElementById('tabAuthKey');
      const blockPass = document.getElementById('sitePassField');
      const blockKey = document.getElementById('siteKeyBlock');

      if (type === 'key') {
        if (tabPass) tabPass.classList.remove('active');
        if (tabKey) tabKey.classList.add('active');
        if (blockPass) blockPass.style.display = 'none';
        if (blockKey) blockKey.style.display = 'block';
      } else {
        if (tabKey) tabKey.classList.remove('active');
        if (tabPass) tabPass.classList.add('active');
        if (blockKey) blockKey.style.display = 'none';
        if (blockPass) blockPass.style.display = 'flex';
      }
    }

    function renderSitesList() {
      const container = document.getElementById('sitesList');
      container.innerHTML = '';

      // Permanent top item: New Site
      const newSiteItem = document.createElement('div');
      newSiteItem.className = 'site-tree-item new-site-entry' + (selectedSiteId === null ? ' active' : '');
      newSiteItem.innerHTML = `
        <svg class="icon icon-sm site-tree-icon" viewBox="0 0 24 24"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>
        <span style="font-weight:600;">New Site</span>
      `;
      newSiteItem.onclick = () => {
        selectedSiteId = null;
        resetSiteForm();
      };
      container.appendChild(newSiteItem);

      if (savedSites.length === 0) {
        return;
      }

      savedSites.forEach(s => {
        const item = document.createElement('div');
        item.className = 'site-tree-item' + (selectedSiteId === s.id ? ' active' : '');
        item.title = `${s.name} (${s.protocol}://${s.host}:${s.port})\nDouble click to connect`;
        item.innerHTML = `
          <svg class="icon icon-sm site-tree-icon" viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="8" rx="2" ry="2"/><rect x="2" y="14" width="20" height="8" rx="2" ry="2"/><line x1="6" y1="6" x2="6.01" y2="6"/><line x1="6" y1="18" x2="6.01" y2="18"/></svg>
          <span style="overflow:hidden; text-overflow:ellipsis; flex:1;">${escapeHtml(s.name)}</span>
          <span class="site-proto-badge">${escapeHtml((s.protocol || 'sftp').toUpperCase())}</span>
        `;
        item.onclick = () => {
          selectedSiteId = s.id;
          fillSiteForm(s);
          renderSitesList();
        };
        item.ondblclick = () => {
          selectedSiteId = s.id;
          fillSiteForm(s);
          connectCurrentFormSite();
        };
        container.appendChild(item);
      });
    }

    function fillSiteForm(s) {
      document.getElementById('siteId').value = s.id;
      document.getElementById('siteName').value = s.name;
      document.getElementById('siteProtocol').value = s.protocol;
      document.getElementById('siteHost').value = s.host;
      document.getElementById('sitePort').value = s.port;
      document.getElementById('siteUser').value = s.username;
      document.getElementById('sitePass').value = s.password || '';
      document.getElementById('siteKeyPath').value = s.key_path || '';
      document.getElementById('sitePassphrase').value = s.passphrase || '';
      document.getElementById('siteRemoteDir').value = s.remote_dir || '';

      switchSiteAuth(s.auth_type === 'key' ? 'key' : 'password');
    }

    function resetSiteForm() {
      selectedSiteId = null;
      document.getElementById('siteId').value = '';
      document.getElementById('siteName').value = '';
      document.getElementById('siteProtocol').value = 'sftp';
      document.getElementById('siteHost').value = '';
      document.getElementById('sitePort').value = '22';
      document.getElementById('siteUser').value = '';
      document.getElementById('sitePass').value = '';
      document.getElementById('siteKeyPath').value = '';
      document.getElementById('sitePassphrase').value = '';
      document.getElementById('siteRemoteDir').value = '';
      switchSiteAuth('password');
      renderSitesList();
    }

    function deleteCurrentSelectedSite() {
      const id = document.getElementById('siteId').value || selectedSiteId;
      if (!id) {
        alert('Please select a saved site from the list to delete.');
        return;
      }
      deleteSite(id);
    }

    async function connectCurrentFormSite() {
      const host = document.getElementById('siteHost').value.trim();
      if (!host) {
        alert('Please enter a Host Name before connecting.');
        return;
      }

      const siteId = document.getElementById('siteId').value;
      const siteConfig = {
        site_id: siteId || '',
        protocol: document.getElementById('siteProtocol').value,
        host: host,
        port: parseInt(document.getElementById('sitePort').value) || 22,
        username: document.getElementById('siteUser').value.trim(),
        password: document.getElementById('sitePass').value,
        auth_type: document.getElementById('siteAuthType').value,
        key_path: document.getElementById('siteKeyPath').value.trim(),
        passphrase: document.getElementById('sitePassphrase').value,
        remote_dir: document.getElementById('siteRemoteDir').value.trim(),
        site_name: document.getElementById('siteName').value.trim() || host
      };

      closeSitesModal();

      const sel = document.getElementById('quickSiteSelect');
      if (sel && siteId && savedSites.some(s => s.id === siteId)) {
        sel.value = siteId;
        onQuickSiteSelect(siteId, false);
      }

      addLog(`Connecting to: ${siteConfig.site_name} (${siteConfig.host})...`, 'info');
      executeConnect(siteConfig);
    }

    let siteNoticeTimer = null;
    function showSiteNotice(msg, kind = 'success', ms = 3500) {
      const el = document.getElementById('siteModalNotice');
      if (!el) return;
      const icon = kind === 'success' ? '✓' : (kind === 'error' ? '✕' : 'ℹ');
      el.className = 'site-notice show ' + kind;
      el.textContent = `${icon} ${msg}`;
      if (siteNoticeTimer) clearTimeout(siteNoticeTimer);
      if (ms > 0) siteNoticeTimer = setTimeout(() => { el.className = 'site-notice'; }, ms);
    }

    function togglePwVisibility(inputId, btn) {
      const inp = document.getElementById(inputId);
      if (!inp) return;
      const show = inp.type === 'password';
      inp.type = show ? 'text' : 'password';
      if (btn) btn.innerHTML = show
        ? '<svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24"/><line x1="1" y1="1" x2="23" y2="23"/></svg>'
        : '<svg class="icon icon-lg" viewBox="0 0 24 24"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>';
    }

    async function refreshVaultKeySelect() {
      const sel = document.getElementById('siteVaultKeySelect');
      if (!sel) return;
      try {
        const res = await sendRust('ssh_list_keys', {});
        const keys = res.keys || [];
        window._cachedVaultKeys = keys;
        sel.innerHTML = '<option value="">Vault keys...</option>' +
          keys.map((k, i) => `<option value="${i}">${escapeHtml(k.name)}</option>`).join('');
      } catch (_) {}
    }

    function useVaultKeyForSite(idx) {
      if (idx === '' || idx === null) return;
      const k = (window._cachedVaultKeys || [])[parseInt(idx, 10)];
      if (!k) return;
      document.getElementById('siteKeyPath').value = k.private_key_path || '';
      showSiteNotice(`Using vault key '${k.name}'`, 'info');
    }

    async function testSiteFormConnection() {
      const host = document.getElementById('siteHost').value.trim();
      const port = parseInt(document.getElementById('sitePort').value, 10) || 22;
      if (!host) {
        showSiteNotice('Please enter a Host Name to test.', 'error');
        return;
      }
      const btn = document.getElementById('btnSiteTest');
      if (btn) { btn.disabled = true; btn.textContent = 'Testing...'; }
      showSiteNotice(`Testing ${host}:${port}...`, 'info', 0);
      try {
        const res = await sendRust('network_probe', { host, port, count: 2 });
        if (res.reachable) showSiteNotice(`${host}:${port} is reachable (${res.avg_ms} ms avg)`, 'success');
        else showSiteNotice(`${host}:${port} is unreachable or port is closed`, 'error', 6000);
      } catch (err) {
        showSiteNotice('Test failed: ' + (err.message || err), 'error', 6000);
      } finally {
        if (btn) { btn.disabled = false; btn.textContent = 'Test'; }
      }
    }

    async function saveSiteFromForm() {
      const name = document.getElementById('siteName').value.trim();
      const host = document.getElementById('siteHost').value.trim();
      const btn = document.getElementById('btnSiteSave');

      if (!host) {
        showSiteNotice('Please enter a Host Name before saving.', 'error', 5000);
        document.getElementById('siteHost').focus();
        return;
      }

      const user = document.getElementById('siteUser').value.trim();
      const id = document.getElementById('siteId').value || 'site_' + Date.now();
      const isNew = !savedSites.some(s => s.id === id);
      const site = {
        id,
        name: name || (user ? `${user}@${host}` : host),
        protocol: document.getElementById('siteProtocol').value,
        host: host,
        port: parseInt(document.getElementById('sitePort').value) || 22,
        username: user,
        password: document.getElementById('sitePass').value,
        auth_type: document.getElementById('siteAuthType').value,
        key_path: document.getElementById('siteKeyPath').value.trim(),
        passphrase: document.getElementById('sitePassphrase').value,
        remote_dir: document.getElementById('siteRemoteDir').value.trim(),
        local_dir: '',
        created_at: new Date().toISOString()
      };

      if (btn) { btn.disabled = true; btn.textContent = 'Saving...'; }
      try {
        const res = await sendRust('save_site', site);
        savedSites = res.sites || [];
        selectedSiteId = site.id;
        document.getElementById('siteId').value = site.id;
        if (!name) document.getElementById('siteName').value = site.name;
        updateQuickSiteSelect();
        renderSitesList();
        showSiteNotice(`${isNew ? 'Site added' : 'Site updated'}: ${site.name}`, 'success');
        addLog(`Site configuration saved: ${site.name}`, 'success');
        if (btn) {
          btn.classList.add('btn-saved');
          btn.textContent = '✓ Saved!';
          setTimeout(() => { btn.classList.remove('btn-saved'); btn.textContent = 'Save'; }, 1600);
        }
      } catch (err) {
        showSiteNotice('Failed to save site: ' + (err.message || err), 'error', 7000);
        if (btn) btn.textContent = 'Save';
      } finally {
        if (btn) btn.disabled = false;
      }
    }

    async function deleteSite(id) {
      if (!confirm('Are you sure you want to delete this saved site?')) return;
      try {
        const res = await sendRust('delete_site', { id });
        savedSites = res.sites || [];
        updateQuickSiteSelect();
        selectedSiteId = null;
        renderSitesList();
        resetSiteForm();
        showSiteNotice('Site deleted.', 'success');
      } catch (err) {
        showSiteNotice('Failed to delete: ' + (err.message || err), 'error', 6000);
      }
    }

    async function loadSiteIntoBar(siteId) {
      const site = savedSites.find(s => s.id === siteId);
      if (!site) return;

      document.getElementById('connProtocol').value = site.protocol;
      document.getElementById('connHost').value = site.host;
      document.getElementById('connPort').value = site.port;
      document.getElementById('connUser').value = site.username;
      document.getElementById('connPass').value = site.password || '';
      closeSitesModal();

      const sel = document.getElementById('quickSiteSelect');
      if (sel) {
        sel.value = site.id;
        onQuickSiteSelect(site.id, false);
      }

      addLog(`Connecting to saved site: ${site.name} (${site.host})...`, 'info');

      executeConnect({
        site_id: site.id,
        protocol: site.protocol,
        host: site.host,
        port: site.port,
        username: site.username,
        password: site.password,
        auth_type: site.auth_type || 'password',
        key_path: site.key_path,
        passphrase: site.passphrase,
        remote_dir: site.remote_dir || '',
        site_name: site.name
      });
    }

    let lastLogHeight = 160;

    // Dual-Tab Bottom Panel Management (Transfers & Activity Log)
    let activeBottomTab = 'transfers';
    // ── Transfer queue (backend-driven) ─────────────────────────────────────
    //
    // The queue itself lives in Rust: it owns parallelism, retries and the speed
    // limit, and pushes a full snapshot on every change. The UI keeps no state
    // beyond the last snapshot, so the table cannot drift from reality — which
    // is exactly what used to happen when the progress bar was animated by a
    // `setInterval` guess while the transfer was one blocking IPC call.

    let transferTasks = [];
    let transferSettings = { max_parallel: 2, speed_limit_bps: 0, max_attempts: 3, retry_delay_ms: 900 };
    let transferAutoClearTimer = null;
    let transferPaneRefreshTimer = null;

    window.onTransferEvent = function (snap) {
      applyTransferSnapshot(snap);
    };

    function transferName(task) {
      const source = task.kind === 'upload' ? task.local_path : task.remote_path;
      return String(source || '').split(/[\\/]/).pop() || source || '(unnamed)';
    }

    function transferSource(task) {
      const raw = task.kind === 'upload' ? task.local_path : task.remote_path;
      return String(raw || '').replace(/\\/g, '/');
    }

    function transferDestination(task) {
      const raw = task.kind === 'upload' ? task.remote_path : task.local_path;
      return String(raw || '').replace(/\\/g, '/');
    }

    function transferPercent(task) {
      if (task.state === 'completed') return 100;
      if (!task.size) return 0;
      return Math.min(100, Math.round((task.bytes || 0) * 100 / task.size));
    }

    function applyTransferSnapshot(snap) {
      if (!snap) return;

      const previous = new Map();
      for (const task of transferTasks) previous.set(task.id, task.state);

      transferTasks = Array.isArray(snap.tasks) ? snap.tasks : [];
      if (snap.settings) transferSettings = snap.settings;

      let sawCompletion = false;
      for (const task of transferTasks) {
        const was = previous.get(task.id);
        if (task.state === 'completed' && was !== 'completed') {
          sawCompletion = true;
          addLog('Transfer complete: ' + transferName(task), 'success');
        }
        if (task.state === 'failed' && was !== 'failed') {
          addLog('Transfer failed: ' + transferName(task) + ' — ' + (task.error || 'unknown error'), 'error');
        }
      }

      renderTransfersTable();
      updateTransfersBadge();
      syncTransferSettingsControls();

      if (sawCompletion) {
        // The pane listings are stale now. Refreshing on every file would thrash
        // the server during a folder upload, so it is debounced.
        scheduleTransferPaneRefresh();
        if (appOptions && appOptions.delFinished) scheduleTransferAutoClear();
      }
    }

    function scheduleTransferPaneRefresh() {
      if (transferPaneRefreshTimer) clearTimeout(transferPaneRefreshTimer);
      transferPaneRefreshTimer = setTimeout(() => {
        transferPaneRefreshTimer = null;
        if (isQueueIdle()) {
          try { refreshLocal(); } catch (_) {}
          if (isConnected) {
            try { refreshRemote(); } catch (_) {}
          }
        } else {
          // Still busy: check again shortly so the panes catch up at the end.
          scheduleTransferPaneRefresh();
        }
      }, 700);
    }

    function scheduleTransferAutoClear() {
      if (transferAutoClearTimer) return;
      transferAutoClearTimer = setTimeout(async () => {
        transferAutoClearTimer = null;
        if (!isQueueIdle()) {
          scheduleTransferAutoClear();
          return;
        }
        try {
          const res = await sendRust('transfer_clear_finished', {});
          if (res && res.queue) applyTransferSnapshot(res.queue);
        } catch (_) {}
      }, 4000);
    }

    function isQueueIdle() {
      return !transferTasks.some(t => t.state === 'running' || t.state === 'pending' || t.state === 'paused');
    }

    async function enqueueTransfer(kind, localPath, remotePath) {
      if (!isConnected) {
        alert('Please connect to a server first');
        return null;
      }
      try {
        const res = await sendRust('transfer_enqueue', {
          kind: kind,
          local_path: localPath,
          remote_path: remotePath
        });
        if (res && res.queue) applyTransferSnapshot(res.queue);

        if (appOptions && appOptions.autoSwitchTransfers !== false) {
          switchBottomTab('transfers');
        }
        return res ? res.task_id : null;
      } catch (err) {
        addLog('Could not queue ' + kind + ' of ' + localPath + ': ' + (err.message || err), 'error');
        alert('Could not queue transfer:/n' + (err.message || err));
        return null;
      }
    }

    async function transferAction(action, taskId) {
      try {
        const res = await sendRust(action, { task_id: taskId });
        if (res && res.queue) applyTransferSnapshot(res.queue);
      } catch (err) {
        addLog('Transfer action "' + action + '" failed: ' + (err.message || err), 'error');
      }
    }

    async function pauseAllTransfers() {
      for (const task of transferTasks) {
        if (task.state === 'running' || task.state === 'pending') {
          await transferAction('transfer_pause', task.id);
        }
      }
    }

    async function resumeAllTransfers() {
      for (const task of transferTasks) {
        if (task.state === 'paused') {
          await transferAction('transfer_resume', task.id);
        }
      }
    }

    async function cancelAllTransfers() {
      if (!transferTasks.some(t => t.state === 'running' || t.state === 'pending' || t.state === 'paused')) {
        return;
      }
      if (!confirm('Cancel every queued and running transfer?')) return;
      try {
        const res = await sendRust('transfer_cancel_all', {});
        if (res && res.queue) applyTransferSnapshot(res.queue);
      } catch (err) {
        addLog('Cancel-all failed: ' + (err.message || err), 'error');
      }
    }

    async function clearCompletedTransfers() {
      try {
        const res = await sendRust('transfer_clear_finished', {});
        if (res && res.queue) applyTransferSnapshot(res.queue);
      } catch (err) {
        addLog('Clear finished failed: ' + (err.message || err), 'error');
      }
    }

    function updateTransfersBadge() {
      const badge = document.getElementById('transfersBadge');
      const summary = document.getElementById('transfersSummaryText');
      const activeCount = transferTasks.filter(t => t.state === 'running').length;
      const pendingCount = transferTasks.filter(t => t.state === 'pending').length;
      const pausedCount = transferTasks.filter(t => t.state === 'paused').length;
      const completedCount = transferTasks.filter(t => t.state === 'completed').length;
      const failedCount = transferTasks.filter(t => t.state === 'failed').length;

      const btnCopyErrs = document.getElementById('btnCopyTransferErrors');
      if (btnCopyErrs) {
        btnCopyErrs.style.display = failedCount > 0 ? 'inline-flex' : 'none';
      }

      if (badge) {
        const live = activeCount + pendingCount + pausedCount;
        badge.textContent = live > 0 ? live : transferTasks.length;
      }
      if (summary) {
        if (activeCount > 0 || pendingCount > 0) {
          let text = activeCount + ' transferring';
          if (pendingCount > 0) text += ', ' + pendingCount + ' queued';
          if (pausedCount > 0) text += ', ' + pausedCount + ' paused';
          summary.innerHTML = '<span style="color: var(--accent); font-weight: 600;"><svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg> ' + text + '</span>';
        } else if (transferTasks.length > 0) {
          let text = completedCount + ' completed';
          if (pausedCount > 0) text += ', ' + pausedCount + ' paused';
          if (failedCount > 0) text += ', ' + failedCount + ' failed';
          summary.textContent = text;
        } else {
          summary.textContent = 'Queue idle';
        }
      }
    }

    function syncTransferSettingsControls() {
      const parallel = document.getElementById('transferParallelSelect');
      if (parallel && String(transferSettings.max_parallel) !== parallel.value) {
        parallel.value = String(transferSettings.max_parallel);
      }
      const limit = document.getElementById('transferSpeedLimitSelect');
      if (limit && String(transferSettings.speed_limit_bps) !== limit.value) {
        limit.value = String(transferSettings.speed_limit_bps);
      }
    }

    async function applyTransferSettings() {
      const parallel = document.getElementById('transferParallelSelect');
      const limit = document.getElementById('transferSpeedLimitSelect');
      const payload = {};
      if (parallel) payload.max_parallel = parseInt(parallel.value, 10) || 2;
      if (limit) payload.speed_limit_bps = parseInt(limit.value, 10) || 0;

      try {
        const res = await sendRust('transfer_settings', payload);
        if (res && res.queue) applyTransferSnapshot(res.queue);
        addLog('Transfer settings updated: ' + (payload.max_parallel || '—') + ' parallel, '
          + (payload.speed_limit_bps ? formatBytes(payload.speed_limit_bps) + '/s' : 'no speed limit'), 'info');
      } catch (err) {
        addLog('Could not update transfer settings: ' + (err.message || err), 'error');
      }
    }

    function transferRowActions(task) {
      const btn = (label, title, action, color) =>
        '<button type="button" class="btn btn-secondary" style="padding: 1px 6px; font-size: 10px;'
        + (color ? ' color: ' + color + '; border-color: ' + color + '55;' : '')
        + '" title="' + title + '" onclick="transferAction(\'' + action + '\', ' + task.id + ')">'
        + label + '</button>';

      const parts = [];
      if (task.state === 'running' || task.state === 'pending') {
        parts.push(btn('Pause', 'Pause this transfer', 'transfer_pause'));
        parts.push(btn('Stop', 'Cancel this transfer', 'transfer_cancel_task', '#ef4444'));
      } else if (task.state === 'paused') {
        parts.push(btn('Resume', 'Continue this transfer', 'transfer_resume', '#22c55e'));
        parts.push(btn('Stop', 'Cancel this transfer', 'transfer_cancel_task', '#ef4444'));
      } else if (task.state === 'failed' || task.state === 'cancelled') {
        parts.push(btn('Retry', 'Queue this transfer again', 'transfer_requeue', '#38bdf8'));
      } else {
        parts.push('<span style="color: var(--text-dim); font-size: 11px;">—</span>');
      }
      return '<div style="display: flex; gap: 4px; justify-content: center;">' + parts.join('') + '</div>';
    }

    function renderTransfersTable() {
      const tbody = document.getElementById('transfersList');
      if (!tbody) return;
      if (transferTasks.length === 0) {
        tbody.innerHTML = '<tr><td colspan="8" style="text-align: center; color: var(--text-dim); padding: 22px 10px;">No active or queued file transfers</td></tr>';
        return;
      }

      tbody.innerHTML = transferTasks.map((task, idx) => {
        const isUpload = task.kind === 'upload';
        const typeBadge = isUpload
          ? '<span style="color: #38bdf8; font-weight: 700; font-size: 11px;">▲ UP</span>'
          : '<span style="color: #4ade80; font-weight: 700; font-size: 11px;">▼ DOWN</span>';

        const percent = transferPercent(task);
        let statusBadge = '';
        let barClass = '';
        if (task.state === 'running') {
          statusBadge = '<span class="badge-status running"><span class="spin-icon">↻</span> ' + percent + '%</span>';
        } else if (task.state === 'pending') {
          statusBadge = '<span class="badge-status" style="color: var(--text-muted);">⋯ Queued</span>';
        } else if (task.state === 'paused') {
          statusBadge = '<span class="badge-status" style="color: #f59e0b;">❙❙ Paused</span>';
        } else if (task.state === 'completed') {
          statusBadge = '<span class="badge-status completed">✓ Done</span>';
          barClass = 'completed';
        } else if (task.state === 'cancelled') {
          statusBadge = '<span class="badge-status" style="color: var(--text-muted);">⊘ Stopped</span>';
          barClass = 'failed';
        } else {
          const detail = escapeHtml(task.error || 'Transfer failed');
          statusBadge = '<span class="badge-status failed" style="cursor: pointer;" onclick="copyTextToClipboard(\''
            + escapeJsString(task.error || 'Transfer failed')
            + '\', \'Transfer error copied!\')" title="Click to copy: ' + detail + '">✕ Failed'
            + (task.attempts > 1 ? ' (' + task.attempts + '×)' : '') + '</span>';
          barClass = 'failed';
        }

        const iconPrefix = isUpload
          ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><polyline points="13 2 13 9 20 9"/></svg> '
          : '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg> ';

        const sizeText = task.size ? formatBytes(task.size) : (task.bytes ? formatBytes(task.bytes) : '—');
        const progressLabel = task.state === 'completed' ? '100%'
          : (task.state === 'failed' || task.state === 'cancelled') ? 'Stopped'
          : (task.size ? percent + '%' : formatBytes(task.bytes || 0));

        return '<tr oncontextmenu="openContextMenu(event, \'transfer_item\', transferTasks[' + idx + '])" style="cursor: context-menu;">'
          + '<td style="text-align: center; color: var(--text-dim); font-size: 11px;">' + (idx + 1) + '</td>'
          + '<td>' + typeBadge + '</td>'
          + '<td style="font-weight: 600; color: var(--text-main); max-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" title="' + escapeHtml(transferName(task)) + '">'
          + iconPrefix + escapeHtml(transferName(task))
          + '</td>'
          + '<td style="color: var(--text-dim); font-size: 11px; max-width: 240px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" title="' + escapeHtml(transferSource(task)) + ' ➔ ' + escapeHtml(transferDestination(task)) + '">'
          + escapeHtml(transferSource(task)) + ' &rarr; ' + escapeHtml(transferDestination(task))
          + '</td>'
          + '<td style="text-align: right; font-family: var(--font-mono); color: var(--text-muted); font-size: 11px;">' + escapeHtml(sizeText) + '</td>'
          + '<td>'
          + '<div style="display: flex; justify-content: space-between; font-size: 10px; color: var(--text-dim); font-family: var(--font-mono);">'
          + '<span>' + progressLabel + '</span>'
          + '<span>' + (task.state === 'running' ? formatBytes(task.bytes || 0) : '') + '</span>'
          + '</div>'
          + '<div class="transfer-progress-track">'
          + '<div class="transfer-progress-fill ' + barClass + '" style="width: ' + percent + '%;"></div>'
          + '</div>'
          + '</td>'
          + '<td style="text-align: center;">' + statusBadge + '</td>'
          + '<td>' + transferRowActions(task) + '</td>'
          + '</tr>';
      }).join('');
    }

    function toggleLogPanel() {
      const panel = document.getElementById('logPanel');
      const iconSpan = document.getElementById('logToggleIcon');
      if (panel.classList.contains('collapsed')) {
        panel.classList.remove('collapsed');
        panel.style.height = (lastLogHeight || 160) + 'px';
        iconSpan.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="6 9 12 15 18 9"/></svg>`;
      } else {
        const cur = parseInt(panel.style.height);
        if (cur && cur > 50) lastLogHeight = cur;
        panel.classList.add('collapsed');
        panel.style.height = '34px';
        iconSpan.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="18 15 12 9 6 15"/></svg>`;
      }
    }

    // Dynamic Drag-to-Resize Logic for Left/Right Panes and Bottom Console
    const workspace = document.getElementById('workspaceArea');
    const leftPane = document.getElementById('leftPane');
    const vGutter = document.getElementById('verticalSplitGutter');
    const hGutter = document.getElementById('horizontalSplitGutter');
    const logPanel = document.getElementById('logPanel');

    // Vertical Gutter (Left/Right resizing)
    vGutter.addEventListener('mousedown', function(e) {
      isDraggingVertical = true;
      vGutter.classList.add('dragging');
      document.body.style.cursor = 'col-resize';
      e.preventDefault();
    });

    // Horizontal Gutter (Up/Down console resizing)
    hGutter.addEventListener('mousedown', function(e) {
      isDraggingHorizontal = true;
      hGutter.classList.add('dragging');
      document.body.style.cursor = 'row-resize';
      e.preventDefault();
    });

    document.addEventListener('mousemove', function(e) {
      if (isDraggingVertical) {
        const rect = workspace.getBoundingClientRect();
        const offsetX = e.clientX - rect.left;
        const totalW = rect.width;
        let percentage = (offsetX / totalW) * 100;
        if (percentage < 20) percentage = 20;
        if (percentage > 80) percentage = 80;
        leftPane.style.width = percentage + '%';
      }

      if (isDraggingHorizontal) {
        const windowHeight = window.innerHeight;
        const newHeight = windowHeight - e.clientY;
        const iconSpan = document.getElementById('logToggleIcon');
        if (newHeight <= 45) {
          logPanel.classList.add('collapsed');
          logPanel.style.height = '34px';
          if (iconSpan) iconSpan.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="18 15 12 9 6 15"/></svg>`;
        } else if (newHeight <= windowHeight - 150) {
          logPanel.classList.remove('collapsed');
          logPanel.style.height = newHeight + 'px';
          lastLogHeight = newHeight;
          if (iconSpan) iconSpan.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="6 9 12 15 18 9"/></svg>`;
        }
      }
    });

    document.addEventListener('mouseup', function() {
      if (isDraggingVertical) {
        isDraggingVertical = false;
        vGutter.classList.remove('dragging');
        document.body.style.cursor = '';
      }
      if (isDraggingHorizontal) {
        isDraggingHorizontal = false;
        hGutter.classList.remove('dragging');
        document.body.style.cursor = '';
      }
    });

    // Theme Management (Light / Dark)
    function initTheme() {
      const savedTheme = appStorage.getItem('90ftp_theme') || 'dark';
      applyTheme(savedTheme);
    }

    function toggleTheme() {
      const cur = document.documentElement.getAttribute('data-theme') || 'dark';
      const next = cur === 'dark' ? 'light' : 'dark';
      applyTheme(next);
    }

    function applyTheme(theme) {
      document.documentElement.setAttribute('data-theme', theme);
      appStorage.setItem('90ftp_theme', theme);
      const icon = document.getElementById('themeIcon');
      if (icon) {
        if (theme === 'light') {
          icon.innerHTML = `<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>`;
        } else {
          icon.innerHTML = `<circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>`;
        }
      }
      const brandLogoRect = document.getElementById('brandLogoRect');
      const brandLogoArrow = document.getElementById('brandLogoArrow');
      const aboutLogoRect = document.getElementById('aboutLogoRect');
      const aboutLogoArrow = document.getElementById('aboutLogoArrow');
      if (theme === 'light') {
        if (brandLogoRect) {
          brandLogoRect.setAttribute('fill', 'url(#logoGradLight)');
          brandLogoRect.setAttribute('stroke', '#dc2626');
        }
        if (brandLogoArrow) brandLogoArrow.setAttribute('fill', '#dc2626');
        if (aboutLogoRect) {
          aboutLogoRect.setAttribute('fill', 'url(#aboutLogoGradLight)');
          aboutLogoRect.setAttribute('stroke', '#dc2626');
        }
        if (aboutLogoArrow) aboutLogoArrow.setAttribute('fill', '#dc2626');
      } else {
        if (brandLogoRect) {
          brandLogoRect.setAttribute('fill', 'url(#logoGrad)');
          brandLogoRect.setAttribute('stroke', '#38bdf8');
        }
        if (brandLogoArrow) brandLogoArrow.setAttribute('fill', '#38bdf8');
        if (aboutLogoRect) {
          aboutLogoRect.setAttribute('fill', 'url(#aboutLogoGrad)');
          aboutLogoRect.setAttribute('stroke', '#38bdf8');
        }
        if (aboutLogoArrow) aboutLogoArrow.setAttribute('fill', '#38bdf8');
      }
    }

    // High-Fidelity Desktop Vector Icons
    function getFileIcon(name, isDir) {
      if (isDir) {
        return {
          iconClass: 'folder',
          svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
            <path d="M2.5 4A1.5 1.5 0 0 1 4 2.5h3.38a1.5 1.5 0 0 1 1.06.44l1.62 1.62H16A1.5 1.5 0 0 1 17.5 6V7H4A1.5 1.5 0 0 0 2.5 8.5V4z" fill="#d97706"/>
            <path d="M2.5 8.5A1.5 1.5 0 0 1 4 7h12a1.5 1.5 0 0 1 1.5 1.5v6.5A1.5 1.5 0 0 1 16 16.5H4A1.5 1.5 0 0 1 2.5 15V8.5z" fill="#f59e0b"/>
            <path d="M4 8h12v1H4z" fill="#fef3c7" opacity="0.6"/>
          </svg>`
        };
      }

      const safeName = (name || '').toString();
      const ext = safeName.includes('.') ? safeName.split('.').pop().toLowerCase() : '';
      switch (ext) {
        case 'php':
        case 'phtml':
          return {
            iconClass: 'code-php',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#7c3aed" opacity="0.15"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#7c3aed" stroke-width="1.2" fill="none"/>
              <rect x="5" y="9" width="10" height="5.5" rx="1.5" fill="#7c3aed"/>
              <text x="10" y="13.2" font-size="3.8" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">PHP</text>
            </svg>`
          };
        case 'html':
        case 'htm':
          return {
            iconClass: 'code-html',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#ea580c" opacity="0.15"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#ea580c" stroke-width="1.2" fill="none"/>
              <rect x="5" y="9" width="10" height="5.5" rx="1.5" fill="#ea580c"/>
              <text x="10" y="13.2" font-size="3.6" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">HTML</text>
            </svg>`
          };
        case 'js':
        case 'mjs':
        case 'cjs':
          return {
            iconClass: 'code-js',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#facc15" opacity="0.2"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#ca8a04" stroke-width="1.2" fill="none"/>
              <rect x="5.5" y="9" width="9" height="5.5" rx="1.5" fill="#eab308"/>
              <text x="10" y="13.2" font-size="4" font-weight="900" fill="#000" text-anchor="middle" font-family="system-ui, sans-serif">JS</text>
            </svg>`
          };
        case 'ts':
          return {
            iconClass: 'code-ts',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#0284c7" opacity="0.18"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#0284c7" stroke-width="1.2" fill="none"/>
              <rect x="5.5" y="9" width="9" height="5.5" rx="1.5" fill="#0284c7"/>
              <text x="10" y="13.2" font-size="4" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">TS</text>
            </svg>`
          };
        case 'css':
        case 'scss':
        case 'sass':
        case 'less':
          return {
            iconClass: 'code-css',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#06b6d4" opacity="0.18"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#0891b2" stroke-width="1.2" fill="none"/>
              <rect x="5" y="9" width="10" height="5.5" rx="1.5" fill="#0891b2"/>
              <text x="10" y="13.2" font-size="3.6" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">CSS</text>
            </svg>`
          };
        case 'py':
          return {
            iconClass: 'code-py',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#10b981" opacity="0.15"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#059669" stroke-width="1.2" fill="none"/>
              <rect x="5.5" y="9" width="9" height="5.5" rx="1.5" fill="#059669"/>
              <text x="10" y="13.2" font-size="4" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">PY</text>
            </svg>`
          };
        case 'sql':
        case 'db':
        case 'sqlite':
          return {
            iconClass: 'db',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <ellipse cx="10" cy="5" rx="6" ry="2.5" fill="#0284c7"/>
              <path d="M4 5v5c0 1.38 2.69 2.5 6 2.5s6-1.12 6-2.5V5" stroke="#0284c7" stroke-width="1.3" fill="none"/>
              <path d="M4 10v5c0 1.38 2.69 2.5 6 2.5s6-1.12 6-2.5V10" stroke="#0284c7" stroke-width="1.3" fill="none"/>
            </svg>`
          };
        case 'json':
        case 'xml':
        case 'yaml':
        case 'yml':
        case 'env':
        case 'conf':
        case 'htaccess':
        case 'ini':
        case 'toml':
          return {
            iconClass: 'config',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#ec4899" opacity="0.15"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#db2777" stroke-width="1.2" fill="none"/>
              <circle cx="10" cy="11.5" r="2.2" stroke="#db2777" stroke-width="1.2" fill="none"/>
              <path d="M10 8v1.2M10 13.8V15M6.5 11.5h1.2M12.3 11.5h1.2" stroke="#db2777" stroke-width="1.2" stroke-linecap="round"/>
            </svg>`
          };
        case 'zip':
        case 'tar':
        case 'gz':
        case 'rar':
        case '7z':
        case 'bz2':
          return {
            iconClass: 'archive',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <rect x="3.5" y="3" width="13" height="14" rx="2" fill="#f97316" opacity="0.2"/>
              <rect x="3.5" y="3" width="13" height="14" rx="2" stroke="#ea580c" stroke-width="1.2" fill="none"/>
              <path d="M10 3v14M8.5 5h3M8.5 7h3M8.5 9h3M8.5 11h3" stroke="#ea580c" stroke-width="1"/>
              <rect x="8.5" y="13" width="3" height="3" rx="0.5" fill="#ea580c"/>
            </svg>`
          };
        case 'png':
        case 'jpg':
        case 'jpeg':
        case 'svg':
        case 'gif':
        case 'webp':
        case 'ico':
        case 'bmp':
          return {
            iconClass: 'image',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <rect x="3" y="3.5" width="14" height="13" rx="1.5" fill="#10b981" opacity="0.18"/>
              <rect x="3" y="3.5" width="14" height="13" rx="1.5" stroke="#059669" stroke-width="1.2" fill="none"/>
              <circle cx="7" cy="7.5" r="1.3" fill="#059669"/>
              <path d="M4 14.5l4-4 3 3 2.5-2.5 3.5 3.5" stroke="#059669" stroke-width="1.2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>
            </svg>`
          };
        case 'pdf':
          return {
            iconClass: 'pdf',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#ef4444" opacity="0.15"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#dc2626" stroke-width="1.2" fill="none"/>
              <rect x="5.5" y="9" width="9" height="5.5" rx="1.5" fill="#dc2626"/>
              <text x="10" y="13.2" font-size="3.8" font-weight="900" fill="#fff" text-anchor="middle" font-family="system-ui, sans-serif">PDF</text>
            </svg>`
          };
        case 'pem':
        case 'key':
        case 'pub':
        case 'ppk':
        case 'crt':
          return {
            iconClass: 'key',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <circle cx="7.5" cy="8" r="4" stroke="#d97706" stroke-width="1.3" fill="none"/>
              <circle cx="7.5" cy="8" r="1.5" fill="#d97706"/>
              <path d="M10.5 10.5L16 16M13.5 13.5l1.5-1.5M15 15l1.5-1.5" stroke="#d97706" stroke-width="1.3" stroke-linecap="round"/>
            </svg>`
          };
        case 'sh':
        case 'bash':
        case 'bat':
        case 'cmd':
        case 'ps1':
        case 'exe':
          return {
            iconClass: 'exec',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <rect x="2.5" y="3.5" width="15" height="13" rx="1.5" fill="#334155"/>
              <path d="M5 8l3 2.5-3 2.5M9.5 13h4" stroke="#4ade80" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>
            </svg>`
          };
        case 'zip':
        case 'tar':
        case 'gz':
        case 'tgz':
        case 'rar':
        case '7z':
        case 'bz2':
          return {
            iconClass: 'archive',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="#f59e0b" opacity="0.18"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="#d97706" stroke-width="1.2" fill="none"/>
              <rect x="5.5" y="8" width="9" height="7.5" rx="1" fill="#d97706"/>
              <path d="M8 8v7M12 8v7M5.5 11h9" stroke="#fff" stroke-width="0.8" opacity="0.85"/>
              <text x="10" y="6" font-size="2.9" font-weight="900" fill="#d97706" text-anchor="middle" font-family="system-ui, sans-serif">ZIP</text>
            </svg>`
          };
        default:
          return {
            iconClass: 'doc',
            svg: `<svg viewBox="0 0 20 20" width="16" height="16" style="vertical-align:middle; flex-shrink:0;">
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" fill="currentColor" opacity="0.08"/>
              <path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h6l4.5 4.5v12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 4 17.5v-15z" stroke="currentColor" stroke-width="1.2" fill="none" opacity="0.5"/>
              <path d="M11 1v4a1 1 0 0 0 1 1h4" stroke="currentColor" stroke-width="1.2" fill="none" opacity="0.5"/>
              <line x1="7" y1="10" x2="13" y2="10" stroke="currentColor" stroke-width="1" opacity="0.4"/>
              <line x1="7" y1="13" x2="11" y2="13" stroke="currentColor" stroke-width="1" opacity="0.4"/>
            </svg>`
          };
      }
    }

    // Options & Preferences Management (100% Functional, Reactive & Persistent)
    const defaultOptions = {
      showHidden: true,
      showFullPath: true,
      autoCheckUpdates: true,
      confirmDisconnect: true,
      dblLocal: 'edit',
      dblRemote: 'edit',
      autoReconnectOnClick: true,
      keepViewOnDisconnect: true,
      keepAliveEnabled: true,
      keepAliveInterval: 25,
      syncBrowsing: false,
      autoRefreshOnFocus: false,
      autoRefreshAfterTransfer: true,
      delFinished: false,
      autoSwitchTransfers: true,
      fileCollision: 'overwrite',
      wipeOnDisconnect: false,
      maxLogLines: 500,
      logTimestamps: true
    };

    let appOptions = Object.assign({}, defaultOptions);
    let showHiddenFilesState = true;
    let optionsFormSnapshot = '';

    function initOptions() {
      try {
        const saved = appStorage.getItem('90ftp_options');
        if (saved) {
          appOptions = Object.assign({}, defaultOptions, JSON.parse(saved));
        }
      } catch (_) {}
      loadOptionsToForm();
      showHiddenFilesState = !!appOptions.showHidden;
      const menuCheck = document.getElementById('menuCheckHidden');
      if (menuCheck) menuCheck.textContent = showHiddenFilesState ? '✓' : ' ';
      updateSyncBrowsingToolbarUI();
    }

    function getCurrentFormOptions() {
      const getCheck = (id, def) => {
        const el = document.getElementById(id);
        return el ? el.checked : def;
      };
      const getVal = (id, def) => {
        const el = document.getElementById(id);
        return el ? el.value : def;
      };

      return {
        showHidden: getCheck('optShowHidden', true),
        showFullPath: getCheck('optShowFullPath', true),
        autoCheckUpdates: getCheck('optAutoCheckUpdates', true),
        confirmDisconnect: getCheck('optConfirmDisconnect', true),
        dblLocal: getVal('optDblLocal', 'edit'),
        dblRemote: getVal('optDblRemote', 'edit'),
        autoReconnectOnClick: getCheck('optAutoReconnectOnClick', true),
        keepViewOnDisconnect: getCheck('optKeepViewOnDisconnect', true),
        keepAliveEnabled: getCheck('optKeepAliveEnabled', true),
        keepAliveInterval: parseInt(getVal('optKeepAliveInterval', '25'), 10) || 25,
        syncBrowsing: getCheck('optSyncBrowsing', false),
        autoRefreshOnFocus: getCheck('optAutoRefreshOnFocus', false),
        autoRefreshAfterTransfer: getCheck('optAutoRefreshAfterTransfer', true),
        delFinished: getCheck('optDelFinished', false),
        autoSwitchTransfers: getCheck('optAutoSwitchTransfers', true),
        fileCollision: getVal('optFileCollision', 'overwrite'),
        wipeOnDisconnect: getCheck('optWipeOnDisconnect', false),
        maxLogLines: parseInt(getVal('optMaxLogLines', '500'), 10) || 500,
        logTimestamps: getCheck('optLogTimestamps', true)
      };
    }

    function checkOptionsFormDirty() {
      const current = JSON.stringify(getCurrentFormOptions());
      const isDirty = current !== optionsFormSnapshot;
      const statusEl = document.getElementById('optionsStatusMsg');
      const applyBtn = document.getElementById('btnApplyOptions');

      if (isDirty) {
        if (statusEl) {
          statusEl.innerHTML = '<span style="color: #f59e0b; font-weight: 600; display: inline-flex; align-items: center; gap: 5px;"><span>●</span> <span>Unsaved changes</span></span>';
        }
        if (applyBtn) {
          applyBtn.style.borderColor = 'var(--accent)';
          applyBtn.style.color = 'var(--accent)';
          applyBtn.style.fontWeight = '700';
        }
      } else {
        if (statusEl) {
          statusEl.innerHTML = '<span style="color: var(--text-dim); display: inline-flex; align-items: center; gap: 5px;"><span>✓</span> <span>All settings up to date</span></span>';
        }
        if (applyBtn) {
          applyBtn.style.borderColor = '';
          applyBtn.style.color = '';
          applyBtn.style.fontWeight = '';
        }
      }
    }

    function loadOptionsToForm() {
      const setCheck = (id, val) => {
        const el = document.getElementById(id);
        if (el) el.checked = !!val;
      };
      const setVal = (id, val) => {
        const el = document.getElementById(id);
        if (el && val !== undefined) el.value = val;
      };

      // Tab 1: General
      setCheck('optShowHidden', appOptions.showHidden);
      setCheck('optShowFullPath', appOptions.showFullPath);
      setCheck('optAutoCheckUpdates', appOptions.autoCheckUpdates);
      setCheck('optConfirmDisconnect', appOptions.confirmDisconnect);
      setVal('optDblLocal', appOptions.dblLocal);
      setVal('optDblRemote', appOptions.dblRemote);

      // Tab 1.5: Connection & Sync
      setCheck('optAutoReconnectOnClick', appOptions.autoReconnectOnClick !== false);
      setCheck('optKeepViewOnDisconnect', appOptions.keepViewOnDisconnect !== false);
      setCheck('optKeepAliveEnabled', appOptions.keepAliveEnabled !== false);
      setVal('optKeepAliveInterval', String(appOptions.keepAliveInterval || 25));
      setCheck('optSyncBrowsing', !!appOptions.syncBrowsing);
      setCheck('optAutoRefreshOnFocus', !!appOptions.autoRefreshOnFocus);
      setCheck('optAutoRefreshAfterTransfer', appOptions.autoRefreshAfterTransfer !== false);

      // Tab 2: Transfers
      setCheck('optDelFinished', appOptions.delFinished);
      setCheck('optAutoSwitchTransfers', appOptions.autoSwitchTransfers);
      setVal('optFileCollision', appOptions.fileCollision);

      // Tab 3: Security & Storage
      setCheck('optWipeOnDisconnect', appOptions.wipeOnDisconnect);

      // Tab 4: Interface & Console
      setVal('optMaxLogLines', appOptions.maxLogLines);
      setCheck('optLogTimestamps', appOptions.logTimestamps);

      // Take snapshot of loaded clean state
      optionsFormSnapshot = JSON.stringify(getCurrentFormOptions());
      checkOptionsFormDirty();
    }

    function openOptionsModal(tab) {
      closeAllMenus();
      loadOptionsToForm();
      if (tab) {
        switchOptionsTab(tab);
      }
      const modal = document.getElementById('optionsModal');
      if (modal) modal.classList.add('active');
    }

    function closeOptionsModal() {
      const modal = document.getElementById('optionsModal');
      if (modal) modal.classList.remove('active');
    }

    function switchOptionsTab(tabName) {
      const tabs = ['general', 'connection', 'transfer', 'security', 'interface'];
      tabs.forEach(t => {
        const btn = document.getElementById('tabOpt' + t.charAt(0).toUpperCase() + t.slice(1));
        const pane = document.getElementById('paneOpt' + t.charAt(0).toUpperCase() + t.slice(1));
        if (btn) btn.classList.toggle('active', t === tabName);
        if (pane) pane.classList.toggle('active', t === tabName);
      });
    }

    function applyOptions() {
      const newOpts = getCurrentFormOptions();
      const prevOpts = Object.assign({}, appOptions);
      appOptions = Object.assign({}, defaultOptions, newOpts);

      try {
        appStorage.setItem('90ftp_options', JSON.stringify(appOptions));
      } catch (_) {}

      // Update snapshot
      optionsFormSnapshot = JSON.stringify(newOpts);

      // 1. Reactive Hidden Files
      showHiddenFilesState = !!appOptions.showHidden;
      const menuCheck = document.getElementById('menuCheckHidden');
      if (menuCheck) menuCheck.textContent = showHiddenFilesState ? '✓' : ' ';
      if (prevOpts.showHidden !== appOptions.showHidden) {
        if (currentLocalPath) navigateLocal(currentLocalPath);
        if (isConnected && currentRemotePath) navigateRemote(currentRemotePath);
      }

      // 2. Reactive Full Path
      if (prevOpts.showFullPath !== appOptions.showFullPath && currentLocalPath) {
        const pathInp = document.getElementById('localPathInput');
        if (pathInp) {
          pathInp.value = appOptions.showFullPath ? currentLocalPath : (currentLocalPath.split('\\').pop() || currentLocalPath);
        }
      }

      // 3. Reactive Keep-Alive & Sync Browsing
      restartKeepAliveTimer();
      updateSyncBrowsingToolbarUI();

      // 3. Visual feedback on Apply button and status
      const statusEl = document.getElementById('optionsStatusMsg');
      const applyBtn = document.getElementById('btnApplyOptions');
      if (statusEl) {
        statusEl.innerHTML = '<span style="color: #22c55e; font-weight: 600; display: inline-flex; align-items: center; gap: 5px;"><span>✓</span> <span>Settings applied successfully</span></span>';
      }
      if (applyBtn) {
        const origText = 'Apply';
        applyBtn.textContent = '✓ Applied!';
        applyBtn.style.borderColor = '#22c55e';
        applyBtn.style.color = '#22c55e';
        setTimeout(() => {
          applyBtn.textContent = origText;
          checkOptionsFormDirty();
        }, 1500);
      }

      addLog('Preferences applied and saved successfully', 'success');
    }

    function saveOptions(showNotice = true) {
      applyOptions();
    }

    function saveOptionsAndClose() {
      applyOptions();
      closeOptionsModal();
    }

    function resetSettingsToDefault() {
      if (confirm('Are you sure you want to reset all preferences to factory defaults?')) {
        appOptions = Object.assign({}, defaultOptions);
        try {
          appStorage.removeItem('90ftp_options');
        } catch (_) {}
        loadOptionsToForm();
        addLog('Preferences reset to default values', 'info');
      }
    }

    // ==========================================
    // Embedded SSH Terminal (Xshell-inspired)
    // ==========================================
    let terminalSessions = [];
    let activeTerminalSessionId = null;
    let terminalSeq = 0;
    let terminalFontSize = 12.5;
    let isTerminalMaximized = false;
    let preMaximizeHeight = 320;

    function ansiToHtml(text) {
      if (!text) return '';
      const colorMap = {
        30: '#0f172a', 31: '#ef4444', 32: '#22c55e', 33: '#eab308', 34: '#38bdf8', 35: '#a855f7', 36: '#06b6d4', 37: '#f8fafc',
        90: '#64748b', 91: '#f87171', 92: '#4ade80', 93: '#fde047', 94: '#60a5fa', 95: '#c084fc', 96: '#22d3ee', 97: '#ffffff'
      };
      const bgMap = {
        40: '#000000', 41: '#7f1d1d', 42: '#14532d', 43: '#713f12', 44: '#1e3a8a', 45: '#581c87', 46: '#164e63', 47: '#334155'
      };
      let html = text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');

      html = html.replace(/\x1b\[([0-9;]*)m/g, (match, seq) => {
        if (!seq || seq === '0') return '</span>';
        const codes = seq.split(';').map(n => parseInt(n, 10));
        let styles = [];
        codes.forEach(c => {
          if (colorMap[c]) styles.push('color:' + colorMap[c]);
          if (bgMap[c]) styles.push('background-color:' + bgMap[c]);
          if (c === 1) styles.push('font-weight:bold');
          if (c === 4) styles.push('text-decoration:underline');
          if (c === 2) styles.push('opacity:0.75');
        });
        if (styles.length === 0) return '</span>';
        return `<span style="${styles.join(';')}">`;
      });
      // Strip any other terminal escape sequences (cursor movements, clear screen/line, window title, etc.)
      html = html.replace(/\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07\x1b]*(\x07|\x1b\\))/g, '');
      return html;
    }

    function initTerminalSession(targetCwd) {
      if (terminalSessions.length > 0) return;
      addNewTerminalTab(targetCwd, isConnected ? 'remote' : 'local');
    }

    function addNewTerminalTab(initialCwd, preferredMode) {
      terminalSeq++;
      const id = 'sess_' + terminalSeq;
      const isRemoteConn = isConnected;
      const mode = preferredMode || (isRemoteConn ? 'remote' : 'local');
      
      let host = 'localhost';
      let user = 'user';
      let cwd = initialCwd;

      if (mode === 'remote' && isRemoteConn) {
        host = (currentConnectedProfile && currentConnectedProfile.host) || (selectedQuickSite && selectedQuickSite.host) || 'connected-server';
        user = (currentConnectedProfile && (currentConnectedProfile.username || currentConnectedProfile.user)) || (selectedQuickSite && (selectedQuickSite.username || selectedQuickSite.user)) || 'root';
        if (!cwd) cwd = currentRemotePath || '~';
      } else {
        host = 'LocalPC';
        user = 'PowerShell';
        if (!cwd) cwd = currentLocalPath || '.';
      }

      const session = {
        id: id,
        mode: mode,
        title: mode === 'remote' ? `${user}@${host}` : `Local: ${host}`,
        host: host,
        user: user,
        cwd: cwd,
        history: [],
        historyIndex: -1,
        logs: [],
        busy: false
      };

      if (mode === 'remote') {
        session.logs.push({
          type: 'banner',
          text: `\x1b[36m======================================================================\x1b[0m\n` +
                `\x1b[32m  90FTP Embedded Terminal - Remote SSH  [Tab #${terminalSeq}]\x1b[0m\n` +
                `  Host: \x1b[33m${user}@${host}\x1b[0m | Directory: \x1b[34m${cwd}\x1b[0m\n` +
                `  Type any shell command (e.g. ls, top, df, cd, git, docker).\n` +
                `\x1b[36m======================================================================\x1b[0m\n`
        });
      } else {
        session.logs.push({
          type: 'banner',
          text: `\x1b[36m======================================================================\x1b[0m\n` +
                `\x1b[35m  90FTP Embedded Terminal - Local PowerShell  [Tab #${terminalSeq}]\x1b[0m\n` +
                `  Environment: \x1b[33mWindows (${user})\x1b[0m | Directory: \x1b[34m${cwd}\x1b[0m\n` +
                `  Type local commands (dir, git, ping, python, etc.) or connect to SSH.\n` +
                `\x1b[36m======================================================================\x1b[0m\n`
        });
      }

      terminalSessions.push(session);
      renderTerminalTabs();
      switchTerminalSession(id);
    }

    function renderTerminalTabs() {
      const container = document.getElementById('terminalTabsList');
      if (!container) return;
      container.innerHTML = terminalSessions.map(s => `
        <div class="terminal-tab-pill ${s.id === activeTerminalSessionId ? 'active' : ''}" onclick="switchTerminalSession('${s.id}')">
          <span style="font-size: 8px; color: ${s.busy ? '#38bdf8' : (s.mode === 'remote' ? '#22c55e' : '#a855f7')};">●</span>
          <span>${escapeHtml(s.title)} (${escapeHtml(s.cwd.length > 15 ? '...' + s.cwd.slice(-12) : s.cwd)})</span>
          ${terminalSessions.length > 1 ? `<span class="terminal-tab-close" onclick="closeTerminalSession('${s.id}', event)" title="Close tab">✕</span>` : ''}
        </div>
      `).join('');
    }

    function switchTerminalSession(id) {
      activeTerminalSessionId = id;
      renderTerminalTabs();
      if (typeof renderPuttyTabs === 'function') renderPuttyTabs();
      if (typeof updatePuttyHeader === 'function') updatePuttyHeader();
      const session = terminalSessions.find(s => s.id === id);
      if (!session) return;

      const userSpan = document.getElementById('terminalUserSpan');
      const hostSpan = document.getElementById('terminalHostSpan');
      const promptCwd = document.getElementById('terminalPromptCwd');
      const cwdDisplay = document.getElementById('terminalCwdDisplay');
      const sessionInfo = document.getElementById('terminalSessionInfo');
      const badge = document.getElementById('terminalBadge');

      if (userSpan) {
        userSpan.textContent = session.user;
        userSpan.style.color = session.mode === 'remote' ? '#22c55e' : '#c084fc';
      }
      if (hostSpan) {
        hostSpan.textContent = session.host;
        hostSpan.style.color = session.mode === 'remote' ? '#38bdf8' : '#60a5fa';
      }
      if (promptCwd) promptCwd.textContent = session.cwd;
      if (cwdDisplay) cwdDisplay.textContent = session.cwd;
      if (sessionInfo) sessionInfo.textContent = session.mode === 'remote' ? `SSH2 - ${session.host}` : `Local - PowerShell`;
      if (badge) {
        badge.style.display = 'inline-block';
        if (session.mode === 'remote') {
          badge.textContent = 'SSH Online';
          badge.style.color = '#22c55e';
          badge.style.background = 'rgba(34, 197, 94, 0.18)';
        } else {
          badge.textContent = 'Local PS';
          badge.style.color = '#a855f7';
          badge.style.background = 'rgba(168, 85, 247, 0.18)';
        }
      }

      renderTerminalScreen();
      focusTerminalInput();
    }

    function closeTerminalSession(id, event) {
      if (event) event.stopPropagation();
      const idx = terminalSessions.findIndex(s => s.id === id);
      if (idx === -1) return;
      terminalSessions.splice(idx, 1);
      if (terminalSessions.length === 0) {
        addNewTerminalTab();
      } else {
        const nextId = terminalSessions[Math.max(0, idx - 1)].id;
        switchTerminalSession(nextId);
      }
      if (typeof renderPuttyTabs === 'function') renderPuttyTabs();
      if (typeof updatePuttyHeader === 'function') updatePuttyHeader();
    }

    function resolveCarriageReturns(str) {
      if (!str || !str.includes('\r')) return str;
      return str.split('\n').map(line => {
        if (!line.includes('\r')) return line;
        const parts = line.split('\r');
        let res = '';
        for (const p of parts) {
          if (p.length >= res.length) {
            res = p;
          } else {
            res = p + res.slice(p.length);
          }
        }
        return res;
      }).join('\n');
    }

    function renderTerminalScreen() {
      const screen = document.getElementById('terminalScreen');
      if (!screen) return;
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) {
        screen.innerHTML = '';
        return;
      }

      let html = '';
      session.logs.forEach(item => {
        if (item.type === 'cmd') {
          const userColor = session.mode === 'remote' ? '#22c55e' : '#c084fc';
          const sep = session.mode === 'remote' ? '$' : '>';
          html += `<div class="terminal-cmd-entry"><span style="color:${userColor};">${escapeHtml(session.user)}</span>@<span style="color:#38bdf8;">${escapeHtml(session.host)}</span>:<span style="color:#fbbf24;">${escapeHtml(item.cwd)}</span>${sep} <span style="color:#f8fafc; font-weight:600;">${escapeHtml(item.command)}</span></div>`;
        } else if (item.type === 'output') {
          html += `<div style="color:#cbd5e1; white-space:pre-wrap;">${ansiToHtml(resolveCarriageReturns(item.text))}</div>`;
        } else if (item.type === 'error') {
          html += `<div style="color:#f87171; white-space:pre-wrap;">${ansiToHtml(item.text)}</div>`;
        } else if (item.type === 'banner') {
          html += `<div>${ansiToHtml(item.text)}</div>`;
        }
      });

      screen.innerHTML = html;
      screen.scrollTop = screen.scrollHeight;
    }

    function focusTerminalInput() {
      const input = document.getElementById('terminalInput');
      if (input) input.focus();
    }

    function clearTerminalScreen() {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (session) {
        session.logs = [];
        renderTerminalScreen();
      }
      focusTerminalInput();
    }

    function toggleCurrentSessionMode() {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;
      if (session.mode === 'remote') {
        session.mode = 'local';
        session.host = 'LocalPC';
        session.user = 'PowerShell';
        session.cwd = currentLocalPath || '.';
        session.title = `Local: ${session.host}`;
        session.logs.push({
          type: 'banner',
          text: `\x1b[35m[Switched to Local Windows PowerShell mode]\x1b[0m\n`
        });
      } else {
        session.mode = 'remote';
        session.host = (currentConnectedProfile && currentConnectedProfile.host) || (selectedQuickSite && selectedQuickSite.host) || 'connected-server';
        session.user = (currentConnectedProfile && (currentConnectedProfile.username || currentConnectedProfile.user)) || (selectedQuickSite && (selectedQuickSite.username || selectedQuickSite.user)) || 'root';
        session.cwd = currentRemotePath || '~';
        session.title = `${session.user}@${session.host}`;
        session.logs.push({
          type: 'banner',
          text: `\x1b[32m[Switched to Remote SSH mode (${session.user}@${session.host})]\x1b[0m\n`
        });
      }
      renderTerminalTabs();
      if (typeof renderPuttyTabs === 'function') renderPuttyTabs();
      if (typeof updatePuttyHeader === 'function') updatePuttyHeader();
      switchTerminalSession(session.id);
    }

    function adjustTerminalFontSize(delta) {
      terminalFontSize = Math.max(10, Math.min(22, terminalFontSize + delta));
      const screen = document.getElementById('terminalScreen');
      const input = document.getElementById('terminalInput');
      if (screen) screen.style.fontSize = terminalFontSize + 'px';
      if (input) input.style.fontSize = terminalFontSize + 'px';
    }

    function toggleTerminalMaximize() {
      const panel = document.getElementById('logPanel');
      if (!panel) return;
      if (!isTerminalMaximized) {
        isTerminalMaximized = true;
        preMaximizeHeight = parseInt(panel.style.height) || 300;
        panel.classList.remove('collapsed');
        panel.style.height = 'calc(100vh - 120px)';
        const btn = document.getElementById('btnTerminalMaximize');
        if (btn) btn.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="4 14 10 14 10 20"/><polyline points="20 10 14 10 14 4"/><line x1="14" y1="10" x2="21" y2="3"/><line x1="3" y1="21" x2="10" y2="14"/></svg>`;
      } else {
        isTerminalMaximized = false;
        panel.style.height = preMaximizeHeight + 'px';
        const btn = document.getElementById('btnTerminalMaximize');
        if (btn) btn.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="3" width="18" height="18" rx="2" ry="2"/></svg>`;
      }
    }

    function syncTerminalPathWithRemote() {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;
      if (session.mode === 'remote') {
        if (!currentRemotePath) return;
        runQuickCommand(`cd "${currentRemotePath}"`);
      } else {
        if (!currentLocalPath) return;
        runQuickCommand(`Set-Location "${currentLocalPath}"`);
      }
    }

    function runQuickCommand(cmd) {
      const input = document.getElementById('terminalInput');
      if (input) {
        input.value = cmd;
        executeTerminalCommand();
      }
    }

    function handleTerminalKey(e) {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;

      if (e.key === 'Enter') {
        e.preventDefault();
        executeTerminalCommand();
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        if (session.history.length > 0) {
          if (session.historyIndex === -1) {
            session.historyIndex = session.history.length - 1;
          } else if (session.historyIndex > 0) {
            session.historyIndex--;
          }
          e.target.value = session.history[session.historyIndex] || '';
        }
      } else if (e.key === 'ArrowDown') {
        e.preventDefault();
        if (session.historyIndex !== -1) {
          if (session.historyIndex < session.history.length - 1) {
            session.historyIndex++;
            e.target.value = session.history[session.historyIndex] || '';
          } else {
            session.historyIndex = -1;
            e.target.value = '';
          }
        }
      } else if (e.ctrlKey && e.key.toLowerCase() === 'c') {
        e.preventDefault();
        session.logs.push({
          type: 'cmd',
          cwd: session.cwd,
          command: e.target.value + ' ^C'
        });
        e.target.value = '';
        session.historyIndex = -1;
        renderTerminalScreen();
      } else if (e.ctrlKey && e.key.toLowerCase() === 'l') {
        e.preventDefault();
        clearTerminalScreen();
      }
    }

    async function executeTerminalCommand() {
      const input = document.getElementById('terminalInput');
      if (!input) return;
      const cmd = input.value.trim();
      input.value = '';
      if (!cmd) return;

      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;

      session.history.push(cmd);
      session.historyIndex = -1;

      if (cmd === 'clear') {
        clearTerminalScreen();
        return;
      }
      if (cmd === 'exit') {
        closeTerminalSession(session.id);
        return;
      }
      if (cmd === 'sync') {
        syncTerminalPathWithRemote();
        return;
      }
      if (cmd === 'help') {
        session.logs.push({
          type: 'cmd',
          cwd: session.cwd,
          command: cmd
        });
        session.logs.push({
          type: 'output',
          text: `\x1b[36m90FTP Embedded Terminal Commands:\x1b[0m\n` +
                `  \x1b[33mclear\x1b[0m               Clear terminal screen (Ctrl+L)\n` +
                `  \x1b[33mcd <path>\x1b[0m           Change directory and update prompt\n` +
                `  \x1b[33msync\x1b[0m                Sync directory to active file pane\n` +
                `  \x1b[33mexit\x1b[0m                Close current terminal session tab\n` +
                `  \x1b[32mAny command\x1b[0m         Executes directly in current session mode\n`
        });
        renderTerminalScreen();
        return;
      }

      session.logs.push({
        type: 'cmd',
        cwd: session.cwd,
        command: cmd
      });
      renderTerminalScreen();

      const spinner = document.getElementById('terminalBusySpinner');
      if (spinner) spinner.style.display = 'flex';
      session.busy = true;
      renderTerminalTabs();

      try {
        const res = await sendRust('terminal_exec', {
          command: cmd,
          cwd: session.cwd,
          mode: session.mode,
          session_id: session.id,
          cols: (puttyTerm ? puttyTerm.cols : (session.cols || 100)),
          rows: (puttyTerm ? puttyTerm.rows : (session.rows || 30))
        });

        if (res && res.started) {
          // Command is running asynchronously in streaming mode.
          // Chunks are received via window.onTerminalStream, and completion is signaled by window.onTerminalDone.
          return;
        }

        // Synchronous / immediate completion
        session.busy = false;
        if (spinner) spinner.style.display = 'none';

        if (res) {
          if (res.cwd) {
            session.cwd = res.cwd;
            const promptCwd = document.getElementById('terminalPromptCwd');
            const cwdDisplay = document.getElementById('terminalCwdDisplay');
            if (promptCwd) promptCwd.textContent = res.cwd;
            if (cwdDisplay) cwdDisplay.textContent = res.cwd;
          }

          if (res.output && res.output.trim().length > 0) {
            session.logs.push({
              type: 'output',
              text: res.output
            });
          }
        }
        renderTerminalTabs();
        renderTerminalScreen();
        focusTerminalInput();
      } catch (err) {
        session.busy = false;
        if (spinner) spinner.style.display = 'none';
        session.logs.push({
          type: 'error',
          text: `Command error: ${err.message || err}\n`
        });
        renderTerminalTabs();
        renderTerminalScreen();
        focusTerminalInput();
      }
    }

    let terminalStreamRaf = null;
    function scheduleTerminalScreenRender() {
      if (terminalStreamRaf) return;
      terminalStreamRaf = requestAnimationFrame(() => {
        terminalStreamRaf = null;
        renderTerminalScreen();
      });
    }

    window.onTerminalStream = function(sessionId, text) {
      if (!text || (sessionId && String(sessionId).startsWith('native_term_'))) return;
      // Filter out internal pwd marker if it leaks into stream chunks
      let cleanText = text.replace(/[\r\n]*__90FTP_PWD__[\s\S]*$/, '');
      if (!cleanText) return;

      // Stream directly into xterm.js if active
      if (puttyTerm) {
        puttyTerm.write(cleanText.replace(/\r?\n/g, '\r\n'));
      }

      let session = terminalSessions.find(s => s.id === sessionId);
      if (!session) {
        session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      }
      if (!session) return;

      const last = session.logs[session.logs.length - 1];
      if (last && last.type === 'output') {
        last.text += cleanText;
      } else {
        session.logs.push({
          type: 'output',
          text: cleanText
        });
      }

      if (session.id === activeTerminalSessionId) {
        scheduleTerminalScreenRender();
      }
    };

    window.onTerminalDone = function(sessionId, newCwd, exitCode) {
      if (sessionId && String(sessionId).startsWith('native_term_')) return;
      if (terminalStreamRaf) {
        cancelAnimationFrame(terminalStreamRaf);
        terminalStreamRaf = null;
      }
      let session = terminalSessions.find(s => s.id === sessionId);
      if (!session) {
        session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      }
      if (session) {
        session.busy = false;
        if (newCwd && newCwd.trim()) {
          session.cwd = newCwd.trim();
          if (session.id === activeTerminalSessionId) {
            const promptCwd = document.getElementById('terminalPromptCwd');
            const cwdDisplay = document.getElementById('terminalCwdDisplay');
            if (promptCwd) promptCwd.textContent = session.cwd;
            if (cwdDisplay) cwdDisplay.textContent = session.cwd;
          }
        }
      }

      const spinner = document.getElementById('terminalBusySpinner');
      if (spinner) spinner.style.display = 'none';
      const puttyBusy = document.getElementById('puttyBusy');
      if (puttyBusy) puttyBusy.style.display = 'none';

      if (typeof updatePuttyHeader === 'function') updatePuttyHeader();
      if (typeof renderPuttyTabs === 'function') renderPuttyTabs();

      if (puttyTerm && session) {
        const userColor = session.mode === 'remote' ? '\x1b[32m' : '\x1b[35m';
        const sep = session.mode === 'remote' ? '$' : '>';
        puttyTerm.write(`\r\n${userColor}${session.user}@${session.host}\x1b[0m:\x1b[34m${session.cwd}\x1b[0m${sep} `);
      }

      renderTerminalTabs();
      renderTerminalScreen();
      focusTerminalInput();
      if (typeof focusPuttyInput === 'function') focusPuttyInput();
    };

    function ensureTerminalActiveSession(host, user, remotePath, mode) {
      const targetMode = mode || (isConnected ? 'remote' : 'local');
      if (terminalSessions.length === 0) {
        addNewTerminalTab(remotePath, targetMode);
      } else {
        let active = terminalSessions.find(s => s.id === activeTerminalSessionId);
        if (!active) active = terminalSessions[0];
        if (active) {
          active.mode = targetMode;
          if (host) active.host = host;
          if (user) active.user = user;
          active.title = targetMode === 'remote' ? `${user}@${host}` : `Local: ${host}`;
          if (remotePath && remotePath !== active.cwd) {
            active.cwd = remotePath;
          }
          switchTerminalSession(active.id);
        }
      }
    }

    async function openRemoteTerminal(targetPath) {
      closeAllMenus();
      const remoteInput = document.getElementById('remotePathInput');
      const activeRemotePath = (remoteInput && remoteInput.value.trim()) || currentRemotePath;
      const path = targetPath || (isConnected ? activeRemotePath : currentLocalPath) || '.';
      const host = (currentConnectedProfile && currentConnectedProfile.host) || (selectedQuickSite && selectedQuickSite.host) || '';
      const user = (currentConnectedProfile && (currentConnectedProfile.username || currentConnectedProfile.user)) || (selectedQuickSite && (selectedQuickSite.username || selectedQuickSite.user)) || '';
      const mode = isConnected ? 'remote' : 'local';

      try {
        await sendRust('open_native_terminal', {
          cwd: path,
          host: host,
          user: user,
          mode: mode,
          session_id: 'native_sess_1'
        });
      } catch (err) {
        console.error('Failed to open native terminal window:', err);
      }
    }

    // ========================================================
    // PuTTY Detached Floating Window (Powered by xterm.js)
    // ========================================================
    let puttyTerm = null;
    let puttyFitAddon = null;
    let puttyWindowVisible = false;
    let puttyIsMaximized = false;
    let puttyPreMaxRect = null;
    let puttyHistoryIndex = -1;

    function initPuttyXterm() {
      if (puttyTerm) return;
      const container = document.getElementById('puttyXtermContainer');
      if (!container) return;

      if (typeof Terminal === 'undefined') {
        console.error('xterm.js Terminal class is not loaded');
        return;
      }

      puttyTerm = new Terminal({
        cursorBlink: true,
        cursorStyle: 'block',
        fontSize: terminalFontSize || 13,
        fontFamily: 'Consolas, "Lucida Console", "Courier New", monospace',
        fontWeight: 'normal',
        fontWeightBold: 'bold',
        letterSpacing: 0,
        lineHeight: 1.15,
        theme: {
          background: '#0c0d12',
          foreground: '#cbd5e1',
          cursor: '#22c55e',
          cursorAccent: '#000000',
          selectionBackground: 'rgba(56, 189, 248, 0.35)',
          black: '#000000',
          red: '#ef4444',
          green: '#22c55e',
          yellow: '#eab308',
          blue: '#38bdf8',
          magenta: '#c084fc',
          cyan: '#06b6d4',
          white: '#f8fafc',
          brightBlack: '#64748b',
          brightRed: '#f87171',
          brightGreen: '#4ade80',
          brightYellow: '#fde047',
          brightBlue: '#60a5fa',
          brightMagenta: '#d8b4fe',
          brightCyan: '#22d3ee',
          brightWhite: '#ffffff'
        },
        convertEol: true,
        scrollback: 10000,
        tabStopWidth: 4
      });

      if (typeof FitAddon !== 'undefined' && FitAddon.FitAddon) {
        puttyFitAddon = new FitAddon.FitAddon();
        puttyTerm.loadAddon(puttyFitAddon);
      }

      puttyTerm.open(container);
      if (puttyFitAddon) {
        setTimeout(() => {
          try { puttyFitAddon.fit(); } catch (_) {}
        }, 50);
      }

      // Welcome Banner in PuTTY style
      puttyTerm.writeln('\x1b[1;36m======================================================================\x1b[0m');
      puttyTerm.writeln('\x1b[1;32m  90FTP PuTTY Interactive Terminal  (Source: xterm.js)\x1b[0m');
      puttyTerm.writeln('\x1b[90m  Real-time streaming SSH & PowerShell terminal window\x1b[0m');
      puttyTerm.writeln('\x1b[1;36m======================================================================\x1b[0m\r\n');

      container.addEventListener('click', () => {
        focusPuttyInput();
      });

      const input = document.getElementById('puttyInput');
      if (input) {
        input.addEventListener('keydown', handlePuttyInputKey);
      }

      setupPuttyWindowDragAndResize();
    }

    function setupPuttyWindowDragAndResize() {
      const titlebar = document.getElementById('puttyTitlebar');
      const win = document.getElementById('puttyWindow');
      const resizeHandle = document.getElementById('puttyResizeSE');
      if (!titlebar || !win) return;

      let isDragging = false;
      let startX = 0, startY = 0;
      let startLeft = 0, startTop = 0;

      titlebar.addEventListener('mousedown', (e) => {
        if (e.target.closest('button')) return;
        if (puttyIsMaximized) return;
        isDragging = true;
        startX = e.clientX;
        startY = e.clientY;
        const rect = win.getBoundingClientRect();
        startLeft = rect.left;
        startTop = rect.top;
        win.style.transition = 'none';

        function onMouseMove(moveEvent) {
          if (!isDragging) return;
          const dx = moveEvent.clientX - startX;
          const dy = moveEvent.clientY - startY;
          const maxLeft = Math.max(0, window.innerWidth - 80);
          const maxTop = Math.max(0, window.innerHeight - 40);
          const newLeft = Math.max(0, Math.min(maxLeft, startLeft + dx));
          const newTop = Math.max(0, Math.min(maxTop, startTop + dy));
          win.style.left = newLeft + 'px';
          win.style.top = newTop + 'px';
        }

        function onMouseUp() {
          if (isDragging) {
            isDragging = false;
            document.removeEventListener('mousemove', onMouseMove);
            document.removeEventListener('mouseup', onMouseUp);
            savePuttyWindowState();
          }
        }

        document.addEventListener('mousemove', onMouseMove);
        document.addEventListener('mouseup', onMouseUp);
      });

      if (resizeHandle) {
        let isResizing = false;
        let rStartX = 0, rStartY = 0;
        let rStartW = 0, rStartH = 0;

        resizeHandle.addEventListener('mousedown', (e) => {
          e.preventDefault();
          if (puttyIsMaximized) return;
          isResizing = true;
          rStartX = e.clientX;
          rStartY = e.clientY;
          rStartW = win.offsetWidth;
          rStartH = win.offsetHeight;
          win.style.transition = 'none';

          function onResizeMove(moveEvent) {
            if (!isResizing) return;
            const dw = moveEvent.clientX - rStartX;
            const dh = moveEvent.clientY - rStartY;
            const newW = Math.max(460, rStartW + dw);
            const newH = Math.max(300, rStartH + dh);
            win.style.width = newW + 'px';
            win.style.height = newH + 'px';
            if (puttyFitAddon) {
              try { puttyFitAddon.fit(); } catch (_) {}
            }
          }

          function onResizeUp() {
            if (isResizing) {
              isResizing = false;
              document.removeEventListener('mousemove', onResizeMove);
              document.removeEventListener('mouseup', onResizeUp);
              if (puttyFitAddon) {
                try {
                  puttyFitAddon.fit();
                  if (puttyTerm) {
                    sendRust('terminal_resize', {
                      cols: puttyTerm.cols,
                      rows: puttyTerm.rows
                    });
                  }
                } catch (_) {}
              }
              savePuttyWindowState();
            }
          }

          document.addEventListener('mousemove', onResizeMove);
          document.addEventListener('mouseup', onResizeUp);
        });
      }
    }

    function savePuttyWindowState() {
      const win = document.getElementById('puttyWindow');
      if (!win || puttyIsMaximized) return;
      const rect = {
        left: win.style.left,
        top: win.style.top,
        width: win.style.width,
        height: win.style.height
      };
      appStorage.setItem('90ftp_putty_geom', JSON.stringify(rect));
    }

    function openPuttyTerminalWindow(targetPath) {
      openRemoteTerminal(targetPath);
      return;
    }

    function updatePuttyHeader() {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;
      const titleText = document.getElementById('puttyTitleText');
      const modeBtn = document.getElementById('puttyModeBtn');
      const promptEl = document.getElementById('puttyPrompt');
      const dockTitle = document.getElementById('puttyDockTitle');

      const titleStr = `${session.user}@${session.host}:${session.cwd} - PuTTY Terminal`;
      if (titleText) titleText.textContent = titleStr;
      if (dockTitle) dockTitle.textContent = `${session.user}@${session.host}`;
      if (modeBtn) {
        if (session.mode === 'remote') {
          modeBtn.textContent = 'SSH';
          modeBtn.className = 'putty-btn-pill';
        } else {
          modeBtn.textContent = 'PowerShell';
          modeBtn.className = 'putty-btn-pill local';
        }
      }
      if (promptEl) {
        const sep = session.mode === 'remote' ? '$' : '>';
        promptEl.textContent = `${session.user}@${session.host}:${session.cwd}${sep}`;
        promptEl.style.color = session.mode === 'remote' ? '#22c55e' : '#c084fc';
      }
    }

    function renderPuttyTabs() {
      const container = document.getElementById('puttyTabsContainer');
      if (!container) return;
      container.innerHTML = terminalSessions.map(s => `
        <div class="terminal-tab-pill ${s.id === activeTerminalSessionId ? 'active' : ''}" onclick="switchTerminalSession('${s.id}')" style="padding: 1px 8px; font-size: 11px;">
          <span style="font-size: 7px; color: ${s.busy ? '#38bdf8' : (s.mode === 'remote' ? '#22c55e' : '#a855f7')};">●</span>
          <span>${escapeHtml(s.title)}</span>
          ${terminalSessions.length > 1 ? `<span class="terminal-tab-close" onclick="closeTerminalSession('${s.id}', event)" title="Close tab">✕</span>` : ''}
        </div>
      `).join('');
    }

    function focusPuttyInput() {
      const inp = document.getElementById('puttyInput');
      if (inp) inp.focus();
    }

    function minimizePuttyWindow() {
      const win = document.getElementById('puttyWindow');
      const dock = document.getElementById('puttyDockPill');
      if (win) win.style.display = 'none';
      if (dock) dock.style.display = 'flex';
      puttyWindowVisible = false;
    }

    function restorePuttyWindow() {
      openPuttyTerminalWindow();
    }

    function togglePuttyMaximize() {
      const win = document.getElementById('puttyWindow');
      const btn = document.getElementById('btnPuttyMaximize');
      if (!win) return;

      if (!puttyIsMaximized) {
        puttyIsMaximized = true;
        puttyPreMaxRect = {
          left: win.style.left,
          top: win.style.top,
          width: win.style.width,
          height: win.style.height
        };
        win.classList.add('maximized');
        if (btn) btn.innerHTML = '&#x25A3;';
      } else {
        puttyIsMaximized = false;
        win.classList.remove('maximized');
        if (puttyPreMaxRect) {
          win.style.left = puttyPreMaxRect.left;
          win.style.top = puttyPreMaxRect.top;
          win.style.width = puttyPreMaxRect.width;
          win.style.height = puttyPreMaxRect.height;
        }
        if (btn) btn.innerHTML = '&#x25A2;';
      }

      setTimeout(() => {
        if (puttyFitAddon) {
          try { puttyFitAddon.fit(); } catch (_) {}
        }
        focusPuttyInput();
      }, 50);
    }

    function dockPuttyWindow() {
      closePuttyWindow();
      switchBottomTab('terminal');
      const panel = document.getElementById('logPanel');
      if (panel && panel.classList.contains('collapsed')) {
        toggleLogPanel();
      }
    }

    function closePuttyWindow() {
      const win = document.getElementById('puttyWindow');
      const dock = document.getElementById('puttyDockPill');
      if (win) win.style.display = 'none';
      if (dock) dock.style.display = 'none';
      puttyWindowVisible = false;
    }

    function clearPuttyTerminal() {
      if (puttyTerm) {
        puttyTerm.clear();
      }
      clearTerminalScreen();
      focusPuttyInput();
    }

    function adjustPuttyFontSize(delta) {
      adjustTerminalFontSize(delta);
      if (puttyTerm) {
        puttyTerm.options.fontSize = terminalFontSize;
        if (puttyFitAddon) {
          try { puttyFitAddon.fit(); } catch (_) {}
        }
      }
    }

    function handlePuttyInputKey(e) {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;

      if (e.key === 'Enter') {
        e.preventDefault();
        const cmd = e.target.value.trim();
        e.target.value = '';
        if (!cmd) return;
        executePuttyCommand(cmd);
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        if (session.history.length > 0) {
          if (puttyHistoryIndex === -1) {
            puttyHistoryIndex = session.history.length - 1;
          } else if (puttyHistoryIndex > 0) {
            puttyHistoryIndex--;
          }
          e.target.value = session.history[puttyHistoryIndex] || '';
        }
      } else if (e.key === 'ArrowDown') {
        e.preventDefault();
        if (puttyHistoryIndex !== -1) {
          if (puttyHistoryIndex < session.history.length - 1) {
            puttyHistoryIndex++;
            e.target.value = session.history[puttyHistoryIndex] || '';
          } else {
            puttyHistoryIndex = -1;
            e.target.value = '';
          }
        }
      } else if (e.ctrlKey && e.key.toLowerCase() === 'c') {
        e.preventDefault();
        e.target.value = '';
        puttyHistoryIndex = -1;
        if (puttyTerm) puttyTerm.write('^C\r\n');
        updatePuttyHeader();
      } else if (e.ctrlKey && e.key.toLowerCase() === 'l') {
        e.preventDefault();
        clearPuttyTerminal();
      }
    }

    async function executePuttyCommand(cmd) {
      const session = terminalSessions.find(s => s.id === activeTerminalSessionId);
      if (!session) return;

      session.history.push(cmd);
      puttyHistoryIndex = -1;

      if (cmd === 'clear') {
        clearPuttyTerminal();
        return;
      }
      if (cmd === 'exit') {
        closePuttyWindow();
        return;
      }
      if (cmd === 'sync') {
        syncTerminalPathWithRemote();
        return;
      }

      if (puttyTerm) {
        const userColor = session.mode === 'remote' ? '\x1b[32m' : '\x1b[35m';
        const sep = session.mode === 'remote' ? '$' : '>';
        puttyTerm.writeln(`${userColor}${session.user}@${session.host}\x1b[0m:\x1b[34m${session.cwd}\x1b[0m${sep} \x1b[1;37m${cmd}\x1b[0m`);
      }

      session.logs.push({
        type: 'cmd',
        cwd: session.cwd,
        command: cmd
      });
      renderTerminalScreen();

      const puttyBusy = document.getElementById('puttyBusy');
      const spinner = document.getElementById('terminalBusySpinner');
      if (puttyBusy) puttyBusy.style.display = 'block';
      if (spinner) spinner.style.display = 'flex';
      session.busy = true;
      renderTerminalTabs();
      renderPuttyTabs();

      try {
        const res = await sendRust('terminal_exec', {
          command: cmd,
          cwd: session.cwd,
          mode: session.mode,
          session_id: session.id,
          cols: (puttyTerm ? puttyTerm.cols : (session.cols || 100)),
          rows: (puttyTerm ? puttyTerm.rows : (session.rows || 30))
        });

        if (res && res.started) {
          return;
        }

        session.busy = false;
        if (puttyBusy) puttyBusy.style.display = 'none';
        if (spinner) spinner.style.display = 'none';

        if (res) {
          if (res.cwd) {
            session.cwd = res.cwd;
            updatePuttyHeader();
          }
          if (res.output && res.output.trim().length > 0) {
            if (puttyTerm) puttyTerm.write(res.output.replace(/\r?\n/g, '\r\n'));
            session.logs.push({
              type: 'output',
              text: res.output
            });
          }
        }
        renderTerminalTabs();
        renderPuttyTabs();
        renderTerminalScreen();
        focusPuttyInput();
      } catch (err) {
        session.busy = false;
        if (puttyBusy) puttyBusy.style.display = 'none';
        if (spinner) spinner.style.display = 'none';
        const errStr = `\x1b[31mCommand error: ${err.message || err}\x1b[0m\r\n`;
        if (puttyTerm) puttyTerm.write(errStr);
        session.logs.push({
          type: 'error',
          text: `Command error: ${err.message || err}\n`
        });
        renderTerminalTabs();
        renderPuttyTabs();
        renderTerminalScreen();
        focusPuttyInput();
      }
    }

    /// The tools window is a two-level stack: the grid, then one tool's own
    /// page. Opening a tool pushes a page instead of dismissing the window, so
    /// Back pops it and the grid keeps its filter and scroll position.
    let toolsActiveTool = null;

    function toolsDetailIsOpen() {
      const pane = document.getElementById('toolsDetailPane');
      return !!pane && !pane.hidden;
    }

    /// True when the tools window is the front-most overlay. Escape has to know
    /// this: with a tool's own dialog open on top, Esc must close that dialog
    /// first, not step back to the grid underneath it.
    function toolsIsTopModal() {
      const tools = document.getElementById('toolsModal');
      if (!tools || !tools.classList.contains('active')) return false;
      const own = parseInt(tools.style.zIndex || '0', 10) || 0;
      let top = own;
      document.querySelectorAll('.modal-overlay.active').forEach((m) => {
        const z = parseInt(m.style.zIndex || '0', 10) || 0;
        if (z > top) top = z;
      });
      return own >= top;
    }

    /// Pushes a tool page, taking its title, icon, badge and description from
    /// the card itself so the grid stays the only place they are written down.
    function openToolDetail(card) {
      if (!card) return;

      const title = card.querySelector('.tool-card-title');
      const desc = card.querySelector('.tool-card-desc');
      const icon = card.querySelector('.tool-card-icon');
      const badge = card.querySelector('.tool-card-badge');

      const titleHost = document.getElementById('toolDetailTitle');
      if (titleHost) titleHost.textContent = title ? title.textContent : 'Tool';
      const descHost = document.getElementById('toolDetailDesc');
      if (descHost) descHost.textContent = desc ? desc.textContent : '';

      const iconHost = document.getElementById('toolDetailIcon');
      if (iconHost) iconHost.innerHTML = icon ? icon.innerHTML : '';

      const badgeHost = document.getElementById('toolDetailBadge');
      if (badgeHost) {
        badgeHost.textContent = badge ? badge.textContent : 'READY';
        // A shortcut badge keeps its monospace treatment on the tool page.
        badgeHost.classList.toggle(
          'tool-card-badge-kbd',
          !!badge && badge.classList.contains('tool-card-badge-kbd')
        );
      }

      const pointsHost = document.getElementById('toolDetailPoints');
      if (pointsHost) {
        pointsHost.textContent = '';
        (card.dataset.points || '').split('|').forEach((point) => {
          if (!point.trim()) return;
          const li = document.createElement('li');
          li.textContent = point.trim();
          pointsHost.appendChild(li);
        });
        pointsHost.hidden = pointsHost.children.length === 0;
      }

      toolsActiveTool = {
        action: card.dataset.action || '',
        arg: card.dataset.arg || '',
        closeOnRun: card.dataset.closeOnRun === 'true'
      };

      const run = document.getElementById('toolDetailRun');
      if (run) run.textContent = card.dataset.run || 'Open';

      const home = document.getElementById('toolsBody');
      const head = document.getElementById('toolsHead');
      const pane = document.getElementById('toolsDetailPane');
      if (home) home.hidden = true;
      if (head) head.hidden = true;
      if (pane) {
        pane.hidden = false;
        pane.scrollTop = 0;
      }

      const hint = document.getElementById('toolsFooterHint');
      if (hint) hint.innerHTML = '<kbd>Esc</kbd> back to tools';
    }

    /// Runs the tool's real action. The tools window deliberately stays open
    /// behind it: closing the tool drops you back on its page, and Back from
    /// there returns to the grid.
    function runToolAction() {
      if (!toolsActiveTool || !toolsActiveTool.action) return;
      const fn = window[toolsActiveTool.action];
      if (typeof fn !== 'function') return;
      try {
        if (toolsActiveTool.arg === 'true') fn(true);
        else fn();
      } catch (err) {
        console.error('Tool action failed: ' + toolsActiveTool.action, err);
      }
      // A few tools act on the main window rather than opening a dialog of
      // their own. Leaving the suite open would hide the result, so those
      // hand control back instead of stacking.
      if (toolsActiveTool && toolsActiveTool.closeOnRun) closeToolsModal();
    }

    function backToTools(restoreFocus) {
      const home = document.getElementById('toolsBody');
      const head = document.getElementById('toolsHead');
      const pane = document.getElementById('toolsDetailPane');
      if (pane) pane.hidden = true;
      if (home) home.hidden = false;
      if (head) head.hidden = false;
      toolsActiveTool = null;

      const hint = document.getElementById('toolsFooterHint');
      if (hint) hint.innerHTML = '<kbd>Esc</kbd> close &middot; <kbd>Ctrl</kbd>+<kbd>K</kbd> command palette';

      if (restoreFocus !== false) {
        const search = document.getElementById('toolsSearchInput');
        if (search) setTimeout(() => { try { search.focus(); } catch (_) {} }, 40);
      }
    }

    /// Clicking the dimmed area steps back one level rather than dismissing the
    /// window outright, so it does what Escape does.
    function toolsBackdropClick() {
      if (toolsDetailIsOpen()) backToTools();
      else closeToolsModal();
    }

    function openToolsModal() {
      closeAllMenus();
      const modal = document.getElementById('toolsModal');
      if (!modal) return;
      backToTools(false);
      modal.classList.add('active');

      const search = document.getElementById('toolsSearchInput');
      if (search) {
        // Always start from the full list: a filter left over from last time
        // would look like the suite had lost tools.
        search.value = '';
        filterTools('');
        // Focus after the pop-in animation, otherwise the caret fights it.
        setTimeout(() => { try { search.focus(); } catch (_) {} }, 70);
      }
    }

    function closeToolsModal() {
      const modal = document.getElementById('toolsModal');
      if (modal) modal.classList.remove('active');
      // Reset the stack so the next open starts on the grid rather than on
      // whichever tool happened to be open last time.
      backToTools(false);
    }

    /// Filters the tool cards by name, description or keyword. Sections whose
    /// cards are all hidden collapse too, so the grid never leaves an empty
    /// heading behind.
    function filterTools(query) {
      const body = document.getElementById('toolsBody');
      if (!body) return;

      const needle = String(query || '').trim().toLowerCase();
      let visible = 0;

      body.querySelectorAll('.tool-card').forEach((card) => {
        const haystack = ((card.dataset.tool || '') + ' ' + card.textContent).toLowerCase();
        const show = !needle || haystack.includes(needle);
        card.hidden = !show;
        if (show) visible++;
      });

      body.querySelectorAll('.tools-section').forEach((section) => {
        const anyVisible = Array.from(section.querySelectorAll('.tool-card'))
          .some((card) => !card.hidden);
        section.hidden = !anyVisible;
      });

      const empty = document.getElementById('toolsEmptyState');
      if (empty) empty.classList.toggle('visible', visible === 0);

      const count = document.getElementById('toolsCountLabel');
      if (count) count.textContent = visible === 1 ? '1 tool' : visible + ' tools';
    }

    async function clearKnownHostsCache() {
      if (!confirm('Clear all saved server host key fingerprints? Future connections will prompt to verify fingerprints.')) {
        return;
      }
      try {
        await sendRust('clear_known_hosts');
        addLog('Cleared server host keys from encrypted vault (%APPDATA%\\90FTP)', 'success');
        alert('Server host keys cache has been cleared successfully.');
      } catch (err) {
        addLog('Failed to clear known hosts cache: ' + err.message, 'error');
      }
    }

    async function clearRemoteHistoryCache() {
      if (!confirm('Clear all stored remote navigation path history?')) {
        return;
      }
      try {
        const res = await sendRust('clear_remote_history');
        if (res && res.history) {
          updateRemoteHistory(res.history);
        }
        addLog('Cleared remote path history from encrypted vault (%APPDATA%\\90FTP)', 'success');
        alert('Remote navigation history has been cleared.');
      } catch (err) {
        addLog('Failed to clear remote history: ' + err.message, 'error');
      }
    }

    // About Dialog & Desktop Property Sheet Management
    function openAboutModal() {
      closeAllMenus();
      switchAboutTab('overview');
      const verBadge = document.getElementById('aboutAppVersion');
      if (verBadge) verBadge.textContent = 'Version ' + APP_CURRENT_VERSION + ' (Windows x64)';
      const diagText = document.getElementById('aboutDiagnosticsText');
      if (diagText) {
        diagText.textContent =
          'Application: 90FTP Modern Site Manager & File Transfer Engine\n' +
          'Version: ' + APP_CURRENT_VERSION + ' (Windows x64 Release)\n' +
          'Architecture: x86_64-pc-windows-msvc (Tokio Multithreaded Engine)\n' +
          'User Agent: ' + (navigator.userAgent || 'Edge WebView2 Evergreen') + '\n' +
          'Protocols: SFTP (russh 0.45 async SSH-2.0), FTPS / FTP (suppaftp + rustls)\n' +
          'Security Vault: AES-256-GCM Hardware-Accelerated (Ring Cryptography)\n' +
          'Configuration Path: %APPDATA%\\90FTP\\sites.json\n' +
          'Repository: https://github.com/' + GITHUB_REPO_PATH;
      }
      const modal = document.getElementById('aboutModal');
      if (modal) modal.classList.add('active');
    }

    function closeAboutModal() {
      const modal = document.getElementById('aboutModal');
      if (modal) modal.classList.remove('active');
    }

    function switchAboutTab(tabName) {
      const tabs = ['overview', 'components', 'diagnostics', 'credits'];
      tabs.forEach(t => {
        const btnId = 'aboutTabBtn' + t.charAt(0).toUpperCase() + t.slice(1);
        const paneId = 'aboutPane' + t.charAt(0).toUpperCase() + t.slice(1);
        const btn = document.getElementById(btnId);
        const pane = document.getElementById(paneId);
        if (btn) btn.classList.toggle('active', t === tabName);
        if (pane) pane.classList.toggle('active', t === tabName);
      });
    }

    function openGithubRepository() {
      closeAllMenus();
      sendRust('open_external_url', { url: 'https://github.com/' + GITHUB_REPO_PATH });
      addLog('Navigating to GitHub repository: https://github.com/' + GITHUB_REPO_PATH, 'info');
    }

    function copySystemDiagnostics() {
      const diagEl = document.getElementById('aboutDiagnosticsText');
      const text = diagEl ? diagEl.innerText.trim() : (
        'Application: 90FTP Modern Site Manager\n' +
        'Version: ' + APP_CURRENT_VERSION + '\n' +
        'Platform: Windows x64\n' +
        'Repository: https://github.com/' + GITHUB_REPO_PATH
      );
      const copyBtn = document.getElementById('btnCopyAboutDiag');
      const origHtml = copyBtn ? copyBtn.innerHTML : '';
      
      const onCopied = () => {
        addLog('System diagnostics copied to clipboard', 'success');
        if (copyBtn) {
          copyBtn.innerHTML = '<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="20 6 9 17 4 12"/></svg> <span>Copied!</span>';
          setTimeout(() => { if (copyBtn) copyBtn.innerHTML = origHtml; }, 2000);
        }
      };

      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(onCopied).catch(() => {
          fallbackCopyText(text);
          onCopied();
        });
      } else {
        fallbackCopyText(text);
        onCopied();
      }
    }

    // Software Remote Update Modal Methods

    function openUpdateModal(autoCheck = true) {
      closeAllMenus();
      const modal = document.getElementById('updateModal');
      if (modal) modal.classList.add('active');
      if (autoCheck) {
        checkForUpdates(false);
      }
    }

    function closeUpdateModal() {
      const modal = document.getElementById('updateModal');
      if (modal) modal.classList.remove('active');
    }

    function parseSemVer(v) {
      if (!v) return [0, 0, 0, 0];
      const cleaned = v.trim().replace(/^[vV]/, '');
      const parts = cleaned.split('.').map(p => parseInt(p, 10) || 0);

      // Handle legacy v2.6.0 from old abandoned branch
      if (parts[0] === 2 && parts[1] === 6) {
        return [0, 0, 0, 0];
      }

      // Normalization: 3 parts like 0.0.39 map directly to 4-part 0.0.0.39
      if (parts.length === 3 && parts[0] === 0 && parts[1] === 0) {
        return [0, 0, 0, parts[2]];
      }

      while (parts.length < 4) parts.push(0);
      return parts;
    }

    function compareSemVer(v1, v2) {
      const a = parseSemVer(v1);
      const b = parseSemVer(v2);
      const maxLen = Math.max(a.length, b.length);
      for (let i = 0; i < maxLen; i++) {
        const valA = a[i] || 0;
        const valB = b[i] || 0;
        if (valA > valB) return 1;
        if (valA < valB) return -1;
      }
      return 0;
    }

    function copyUpdateDownloadUrl() {
      if (!currentDownloadUrl) return;
      navigator.clipboard.writeText(currentDownloadUrl).then(() => {
        addLog('Update download URL copied to clipboard: ' + currentDownloadUrl, 'success');
        const msg = document.getElementById('updateActionMsg');
        if (msg) msg.textContent = '✓ Download link copied to clipboard!';
      }).catch(err => {
        addLog('Failed to copy update URL: ' + err, 'error');
      });
    }

    async function startInAppDownload() {
      if (!currentDownloadUrl) {
        alert('No download URL available.');
        return;
      }

      const btnStart = document.getElementById('btnStartDownload');
      const btnCheckAgain = document.getElementById('btnCheckAgain');
      const dlCard = document.getElementById('updateDownloadCard');
      const readyCard = document.getElementById('updateReadyCard');
      const btnLaunch = document.getElementById('btnLaunchInstaller');
      const btnOpen = document.getElementById('btnOpenFolder');
      const pBar = document.getElementById('dlProgressBar');
      const pBadge = document.getElementById('dlPercentBadge');
      const pBytes = document.getElementById('dlBytesText');
      const pSpeed = document.getElementById('dlSpeedText');
      const statusText = document.getElementById('dlStatusText');
      const statusIcon = document.getElementById('dlStatusIcon');
      const actionMsg = document.getElementById('updateActionMsg');

      if (btnStart) btnStart.style.display = 'none';
      if (btnCheckAgain) btnCheckAgain.style.display = 'none';
      if (readyCard) readyCard.style.display = 'none';
      if (btnLaunch) btnLaunch.style.display = 'none';
      if (btnOpen) btnOpen.style.display = 'none';

      if (pBar) pBar.style.width = '0%';
      if (pBadge) pBadge.textContent = '0%';
      if (pBytes) pBytes.textContent = 'Connecting to download stream...';
      if (pSpeed) pSpeed.textContent = '';
      if (statusText) statusText.textContent = 'Downloading ' + (currentDownloadFilename || '90FTP-Setup.exe') + '...';
      if (statusIcon) statusIcon.className = 'spin-icon';
      if (actionMsg) actionMsg.textContent = 'Streaming package securely directly from GitHub...';

      if (dlCard) dlCard.style.display = 'flex';

      addLog('Starting native update download: ' + (currentDownloadFilename || 'installer') + ' from GitHub...', 'info');

      try {
        await sendRust('start_update_download', {
          url: currentDownloadUrl,
          version: currentLatestVersion,
          filename: currentDownloadFilename || '90FTP-Setup.exe'
        });
      } catch (err) {
        if (dlCard) dlCard.style.display = 'none';
        if (btnStart) btnStart.style.display = 'inline-flex';
        if (btnCheckAgain) btnCheckAgain.style.display = 'inline-flex';
        if (actionMsg) actionMsg.innerHTML = '<span style="color:#ef4444;"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg> Failed to start download: ' + escapeHtml(err.message || err) + '</span>';
        addLog('Update download failed to initialize: ' + (err.message || err), 'error');
      }
    }

    // Callbacks invoked from native Rust streaming thread
    window.onUpdateDownloadProgress = function(data) {
      const pBar = document.getElementById('dlProgressBar');
      const pBadge = document.getElementById('dlPercentBadge');
      const pBytes = document.getElementById('dlBytesText');
      const pSpeed = document.getElementById('dlSpeedText');

      const pct = Math.min(100, Math.max(0, parseFloat(data.percent) || 0));
      if (pBar) pBar.style.width = pct.toFixed(1) + '%';
      if (pBadge) pBadge.textContent = Math.round(pct) + '%';

      const dlMb = ((data.downloaded || 0) / (1024 * 1024)).toFixed(1);
      const totMb = (data.total && data.total > 0) ? ((data.total / (1024 * 1024)).toFixed(1) + ' MB') : '...';
      if (pBytes) pBytes.textContent = dlMb + ' MB / ' + totMb;
      if (pSpeed && data.speed) pSpeed.textContent = data.speed;
    };

    window.onUpdateDownloadComplete = function(data) {
      currentDownloadedInstallerPath = data.path || '';
      const dlCard = document.getElementById('updateDownloadCard');
      const readyCard = document.getElementById('updateReadyCard');
      const btnStart = document.getElementById('btnStartDownload');
      const btnLaunch = document.getElementById('btnLaunchInstaller');
      const btnOpen = document.getElementById('btnOpenFolder');
      const actionMsg = document.getElementById('updateActionMsg');

      if (dlCard) dlCard.style.display = 'none';
      if (readyCard) readyCard.style.display = 'flex';
      if (btnStart) btnStart.style.display = 'none';
      if (btnLaunch) btnLaunch.style.display = 'inline-flex';
      if (btnOpen) btnOpen.style.display = 'inline-flex';

      const sizeMb = ((data.size || 0) / (1024 * 1024)).toFixed(2);
      if (actionMsg) actionMsg.innerHTML = '<span style="color:#22c55e;">✓ Download verified (' + sizeMb + ' MB). Ready to install.</span>';
      addLog('Update download complete: saved to ' + (data.path || 'temp directory'), 'success');
    };

    window.onUpdateDownloadError = function(data) {
      const dlCard = document.getElementById('updateDownloadCard');
      const btnStart = document.getElementById('btnStartDownload');
      const btnCheckAgain = document.getElementById('btnCheckAgain');
      const actionMsg = document.getElementById('updateActionMsg');

      if (dlCard) dlCard.style.display = 'none';
      if (btnStart) btnStart.style.display = 'inline-flex';
      if (btnCheckAgain) btnCheckAgain.style.display = 'inline-flex';

      const errText = data.error || 'Network or disk error during download';
      if (actionMsg) actionMsg.innerHTML = '<span style="color:#ef4444;"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg> Download Error: ' + escapeHtml(errText) + '</span>';
      addLog('Update download error: ' + errText, 'error');
    };

    async function launchDownloadedInstaller() {
      if (!currentDownloadedInstallerPath) {
        alert('Installer path not found. Please re-download the update.');
        return;
      }
      const actionMsg = document.getElementById('updateActionMsg');
      if (actionMsg) actionMsg.innerHTML = '<span style="color:#38bdf8;">Launching setup installer... 90FTP will close now to allow the upgrade.</span>';
      addLog('Launching installer: ' + currentDownloadedInstallerPath, 'info');

      try {
        await sendRust('launch_installer', { path: currentDownloadedInstallerPath });
      } catch (err) {
        alert('Failed to launch installer: ' + (err.message || err));
        addLog('Failed to launch installer: ' + (err.message || err), 'error');
      }
    }

    async function openDownloadedFolder() {
      if (!currentDownloadedInstallerPath) return;
      try {
        await sendRust('open_installer_folder', { path: currentDownloadedInstallerPath });
      } catch (err) {
        addLog('Failed to open installer directory: ' + (err.message || err), 'error');
      }
    }


    function showFloatingUpdateBanner(latestTag, cleanLatest, data) {
      const toast = document.getElementById('floatingUpdateToast');
      if (!toast) return;
      const title = document.getElementById('futTitle');
      const desc = document.getElementById('futDesc');
      if (title) title.textContent = 'New Update Available: v' + cleanLatest;
      if (desc) desc.textContent = data.name || ('Version ' + latestTag + ' is ready to download.');
      toast.style.display = 'flex';
      
      const topBadge = document.getElementById('topBrandBadge');
      if (topBadge) {
        topBadge.innerHTML = 'Modern Site Manager v' + APP_CURRENT_VERSION + ' <span style="background: #22c55e; color: #fff; font-size: 9.5px; padding: 1px 6px; border-radius: 10px; margin-left: 5px; animation: pulse 1s infinite alternate;">Update v' + cleanLatest + '</span>';
        topBadge.style.cursor = 'pointer';
        topBadge.title = 'Click to open update manager';
        topBadge.onclick = () => openUpdateModal(true);
      }
    }

    function closeFloatingUpdateToast() {
      const toast = document.getElementById('floatingUpdateToast');
      if (toast) toast.style.display = 'none';
    }

    async function checkUpdatesSilentlyOnStartup() {
      if (appOptions && appOptions.autoCheckUpdates === false) {
        return; // User disabled auto updates check in Options
      }
      try {
        const resp = await fetch('https://api.github.com/repos/' + GITHUB_REPO_PATH + '/releases/latest', {
          headers: { 'Accept': 'application/vnd.github.v3+json' }
        });
        if (!resp.ok) return;
        const data = await resp.json();
        const latestTag = data.tag_name || data.name || '';
        const cleanLatest = latestTag.replace(/^[vV]/, '');
        const cmp = compareSemVer(cleanLatest, APP_CURRENT_VERSION);
        if (cmp > 0) {
          showFloatingUpdateBanner(latestTag, cleanLatest, data);
          addLog('Startup update check: New version ' + latestTag + ' is available on GitHub.', 'success');
        } else {
          addLog('Startup update check: 90FTP v' + APP_CURRENT_VERSION + ' is up to date.', 'info');
        }
      } catch (err) {
        console.warn('Startup update check skipped:', err);
      }
    }

    function formatChangelogHtml(rawText) {
      if (!rawText || !rawText.trim()) {
        return '<div style="color: var(--text-dim); text-align: center; padding: 8px;">Release notes are available on the official releases page.</div>';
      }
      const lines = rawText.split('\n');
      let out = '';
      lines.forEach(line => {
        const trimmed = line.trim();
        if (!trimmed) {
          return;
        } else if (trimmed.startsWith('### ') || trimmed.startsWith('## ') || trimmed.startsWith('# ')) {
          const rawHeading = trimmed.replace(/^#+\s*/, '');
          
          // Sanitize any mojibake characters and strip existing emojis
          const cleanHeading = rawHeading
            .replace(/ðŸš€|âš¡|ðŸ›¡ï¸?|ðŸ›¡/g, '')
            .replace(/[\u{1F300}-\u{1F9FF}\u{2600}-\u{26FF}\u{2700}-\u{27BF}]/gu, '')
            .trim();

          const lower = rawHeading.toLowerCase();
          let headingColor = 'var(--accent)';
          let headingIcon = '';
          let displayTitle = cleanHeading || rawHeading;

          if (lower.includes('added') || lower.includes('feature') || lower.includes('<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>') || lower.includes('ðÿš€')) {
            headingColor = '#22c55e';
            headingIcon = '<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg> ';
            displayTitle = 'Added';
          } else if (lower.includes('changed') || lower.includes('improv') || lower.includes('<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg>') || lower.includes('âš¡')) {
            headingColor = '#38bdf8';
            headingIcon = '<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg> ';
            displayTitle = 'Changed';
          } else if (lower.includes('fixed') || lower.includes('fix') || lower.includes('bug') || lower.includes('<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>') || lower.includes('ðÿ›¡')) {
            headingColor = '#f59e0b';
            headingIcon = '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg> ';
            displayTitle = 'Fixed';
          } else if (lower.includes('security')) {
            headingColor = '#ec4899';
            headingIcon = '<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg> ';
            displayTitle = 'Security';
          } else if (lower.includes("what's new") || lower.includes("whats new")) {
            headingColor = 'var(--text-main)';
            headingIcon = '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg> ';
          }

          // Render as clean colored text without stickers, borders or boxes
          out += `<div style="font-weight: 700; color: ${headingColor}; font-size: 12px; margin-top: 6px; margin-bottom: 2px;">${headingIcon}${escapeHtml(displayTitle)}</div>`;
        } else if (trimmed.startsWith('- ') || trimmed.startsWith('* ') || trimmed.startsWith('+ ')) {
          let bullet = trimmed.substring(2).trim();
          
          // Clean legacy bracket tags to keep text clean and minimal
          bullet = bullet.replace(/^(\[(added|changed|fixed|security)\]|(added|changed|fixed|security):)\s*/i, '');

          out += `<div style="display: flex; gap: 6px; margin-bottom: 2px; align-items: baseline; padding-left: 4px; line-height: 1.35;"><span style="color: var(--text-dim); font-size: 10px; flex-shrink: 0;">•</span><span style="color: var(--text-main); font-size: 11.5px;">${escapeHtml(bullet)}</span></div>`;
        } else {
          out += `<div style="margin-bottom: 3px; color: var(--text-muted); font-size: 11.5px; line-height: 1.35;">${escapeHtml(trimmed)}</div>`;
        }
      });
      return out;
    }

    async function checkForUpdates(manualTrigger = false) {
      const iconContainer = document.getElementById('updateIconContainer');
      const headline = document.getElementById('updateHeadline');
      const subtext = document.getElementById('updateSubtext');
      const detailsBox = document.getElementById('updateDetailsBox');
      const dlCard = document.getElementById('updateDownloadCard');
      const readyCard = document.getElementById('updateReadyCard');
      const btnCheckAgain = document.getElementById('btnCheckAgain');
      const btnStart = document.getElementById('btnStartDownload');
      const btnLaunch = document.getElementById('btnLaunchInstaller');

      if (iconContainer) iconContainer.innerHTML = '<span class="spin-icon">↻</span>';
      if (headline) headline.textContent = 'Checking for updates...';
      if (subtext) subtext.textContent = 'Please wait a moment...';
      if (detailsBox) detailsBox.style.display = 'none';
      if (dlCard) dlCard.style.display = 'none';
      if (readyCard) readyCard.style.display = 'none';
      if (btnCheckAgain) btnCheckAgain.style.display = 'none';
      if (btnStart) btnStart.style.display = 'none';
      if (btnLaunch) btnLaunch.style.display = 'none';

      try {
        const resp = await fetch('https://api.github.com/repos/' + GITHUB_REPO_PATH + '/releases/latest', {
          headers: {
            'Accept': 'application/vnd.github.v3+json'
          }
        });

        if (resp.status === 404) {
          if (iconContainer) iconContainer.innerHTML = '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>';
          if (headline) headline.textContent = 'You are running the latest version';
          if (subtext) subtext.textContent = '90FTP is completely up to date (v' + APP_CURRENT_VERSION + ').';
          if (btnCheckAgain) btnCheckAgain.style.display = 'inline-flex';
          addLog('Update check: 90FTP v' + APP_CURRENT_VERSION + ' is up to date.', 'info');
          return;
        }

        if (!resp.ok) {
          throw new Error('HTTP ' + resp.status + ' (' + resp.statusText + ')');
        }

        const data = await resp.json();
        const latestTag = data.tag_name || data.name || '';
        const cleanLatest = latestTag.replace(/^[vV]/, '');
        const cmp = compareSemVer(cleanLatest, APP_CURRENT_VERSION);

        if (cmp > 0) {
          // Newer version found
          if (iconContainer) iconContainer.innerHTML = '<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>';
          if (headline) headline.textContent = 'New Version Available: ' + (latestTag.startsWith('v') ? latestTag : 'v' + latestTag);
          if (subtext) subtext.innerHTML = 'Current: <strong>v' + APP_CURRENT_VERSION + '</strong> &nbsp;➔&nbsp; Latest: <strong style="color: #22c55e;">' + (latestTag.startsWith('v') ? latestTag : 'v' + latestTag) + '</strong>';

          const dateElem = document.getElementById('updateReleaseDate');
          const changelogElem = document.getElementById('updateChangelog');
          if (dateElem && data.published_at) {
            dateElem.textContent = new Date(data.published_at).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
          }
          if (changelogElem) {
            changelogElem.innerHTML = formatChangelogHtml(data.body);
          }
          if (detailsBox) detailsBox.style.display = 'flex';

          let downloadUrl = data.html_url;
          let assetName = '90FTP-Setup-v' + cleanLatest + '.exe';
          if (data.assets && data.assets.length > 0) {
            const exeAsset = data.assets.find(a => a.name.toLowerCase().endsWith('.exe')) || data.assets[0];
            if (exeAsset) {
              downloadUrl = exeAsset.browser_download_url;
              assetName = exeAsset.name;
            }
          }
          currentDownloadUrl = downloadUrl;
          currentDownloadFilename = assetName;
          currentLatestVersion = cleanLatest;

          if (btnStart) btnStart.style.display = 'inline-flex';
          addLog('New update available: ' + latestTag, 'success');
        } else {
          // Up to date
          if (iconContainer) iconContainer.innerHTML = '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>';
          if (headline) headline.textContent = 'You are running the latest version';
          if (subtext) subtext.textContent = '90FTP is completely up to date (v' + APP_CURRENT_VERSION + ').';
          if (btnCheckAgain) btnCheckAgain.style.display = 'inline-flex';
          addLog('Update check: 90FTP v' + APP_CURRENT_VERSION + ' is up to date.', 'info');
        }
      } catch (err) {
        if (iconContainer) iconContainer.innerHTML = '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>';
        if (headline) headline.textContent = 'Unable to check for updates';
        if (subtext) subtext.textContent = 'Please check your internet connection and try again.';
        if (btnCheckAgain) btnCheckAgain.style.display = 'inline-flex';
        addLog('Update check failed: ' + (err.message || err), 'warning');
      }
    }

    // Native Desktop Menubar
    function toggleMenu(e, menuId) {
      if (e) {
        e.stopPropagation();
        e.preventDefault();
      }
      const targetMenu = document.getElementById(menuId);
      if (!targetMenu) return;
      const parentItem = targetMenu.closest('.menu-item');
      const wasActive = parentItem && parentItem.classList.contains('active');
      closeAllMenus();
      if (!wasActive && parentItem) {
        parentItem.classList.add('active');
      }
    }

    function closeAllMenus() {
      document.querySelectorAll('.desktop-menubar .menu-item.active').forEach(item => {
        item.classList.remove('active');
      });
    }

    function focusQuickConnect() {
      closeAllMenus();
      const sel = document.getElementById('quickSiteSelect');
      if (sel && sel.value !== 'manual') {
        sel.focus();
      } else {
        const inp = document.getElementById('qcHost');
        if (inp) inp.focus();
      }
    }

    function refreshBothPanes() {
      closeAllMenus();
      if (currentLocalPath) {
        navigateLocal(currentLocalPath);
      }
      if (isConnected && currentRemotePath) {
        navigateRemote(currentRemotePath);
      }
      addLog('Refreshed file panes', 'info');
    }

    function toggleHiddenFiles(val) {
      if (val !== undefined) {
        showHiddenFilesState = !!val;
      } else {
        showHiddenFilesState = !showHiddenFilesState;
      }
      const chk = document.getElementById('optShowHidden');
      if (chk) chk.checked = showHiddenFilesState;
      const menuCheck = document.getElementById('menuCheckHidden');
      if (menuCheck) menuCheck.textContent = showHiddenFilesState ? '✓' : ' ';
      closeAllMenus();
      saveOptions(false);
      if (currentLocalPath) navigateLocal(currentLocalPath);
      if (isConnected && currentRemotePath) navigateRemote(currentRemotePath);
    }

    async function transferUpload() {
      closeAllMenus();
      closeContextMenu();
      if (!isConnected) {
        alert('Please connect to a server before uploading');
        return;
      }
      const items = getSelectedLocalItems();
      if (items.length === 0) {
        alert('Please select at least one local file or folder to upload');
        return;
      }
      for (const item of items) {
        if (item.is_dir) {
          await uploadFolderDirect(item.path, item.name);
        } else {
          await uploadFileDirect(item.path, item.name);
        }
      }
    }

    async function transferDownload() {
      closeAllMenus();
      closeContextMenu();
      if (!isConnected) {
        alert('Please connect to a server before downloading');
        return;
      }
      const items = getSelectedRemoteItems();
      if (items.length === 0) {
        alert('Please select at least one remote file or folder to download');
        return;
      }
      for (const item of items) {
        if (item.is_dir) {
          await downloadFolderDirect(item.path, item.name);
        } else {
          await downloadFileDirect(item.path, item.name);
        }
      }
    }

    // ==========================================
    // POWER TOOLS SUITE (FOLDER SIZE, PROBE, SSH KEYGEN, SERVER HEALTH, FILE DIFF)
    // ==========================================
    async function calculateFolderSize(path, name, isRemote) {
      addLog(`Calculating recursive size for folder '${name}'...`, 'info');
      try {
        const cmd = isRemote ? 'remote_folder_size' : 'local_folder_size';
        const res = await sendRust(cmd, { path });
        const bytes = res.size_bytes || 0;
        const human = res.human_size || formatBytes(bytes);
        if (typeof calculatedFolderSizes !== 'undefined') {
          calculatedFolderSizes.set(path, bytes);
        }
        const list = isRemote ? remoteFiles : localFiles;
        const target = list.find(f => f.path === path || f.name === name);
        if (target) {
          target.size = bytes;
          target._calculated_human = human;
        }
        if (isRemote) renderRemoteFiles(remoteFiles);
        else renderLocalFiles(localFiles);
        addLog(`Folder '${name}': ${human} (${(res.size_bytes || 0).toLocaleString()} bytes${res.files_count ? `, ${res.files_count} files, ${res.dirs_count} folders` : ''})`, 'success');
      } catch (err) {
        addLog(`Folder size calculation failed: ${err.message || err}`, 'error');
        alert(`Folder Size Calculation Failed:\n${err.message || err}`);
      }
    }

    // 1. Network Ping & Port Probe
    function openNetworkProbeModal() {
      closeAllMenus();
      const hostInp = document.getElementById('probeHostInput');
      const portInp = document.getElementById('probePortInput');
      const qcHost = document.getElementById('qcHost')?.value?.trim();
      const qcPort = document.getElementById('qcPort')?.value?.trim();
      if (hostInp && !hostInp.value) hostInp.value = qcHost || 'github.com';
      if (portInp && (!portInp.value || portInp.value === '22')) portInp.value = qcPort || '22';
      document.getElementById('networkProbeModal')?.classList.add('active');
    }

    function closeNetworkProbeModal() {
      document.getElementById('networkProbeModal')?.classList.remove('active');
    }

    async function runNetworkProbe() {
      const host = (document.getElementById('probeHostInput')?.value || '').trim();
      const port = parseInt(document.getElementById('probePortInput')?.value || '22', 10) || 22;
      if (!host) {
        alert('Please enter a hostname or IP address to probe.');
        return;
      }
      const btn = document.getElementById('btnRunProbe');
      const box = document.getElementById('probeResultsContainer');
      if (btn) {
        btn.disabled = true;
        btn.innerText = 'Probing...';
      }
      if (box) {
        box.innerHTML = `<div style="color: #38bdf8; text-align: center; padding: 30px 10px;">Running 4-sample TCP handshake & service banner inspection on <strong>${escapeHtml(host)}:${port}</strong>...</div>`;
      }

      try {
        const res = await sendRust('network_probe', { host, port, count: 4 });
        const ips = (res.resolved_ips || []).join(', ') || 'Unresolved';
        const statusColor = res.reachable ? '#10b981' : '#ef4444';
        const statusLabel = res.reachable ? `OPEN (${res.loss_percent || 0}% loss)` : 'CLOSED / UNREACHABLE';
        const rttText = res.avg_ms != null ? `${res.avg_ms} ms (min ${res.min_ms} / max ${res.max_ms})` : 'Timeout';
        const jitterText = res.jitter_ms != null ? `${res.jitter_ms} ms` : '—';
        const sampleRows = (res.samples || []).map(s =>
          s.connected
            ? `<div style="color: #10b981;">✓ Sample #${s.seq}: Connected to ${escapeHtml(s.ip || host)} in <strong>${s.rtt_ms} ms</strong></div>`
            : `<div style="color: #ef4444;">✕ Sample #${s.seq}: FAILED (${escapeHtml(s.error || 'Timeout')})</div>`
        ).join('');

        if (box) {
          box.innerHTML = `
            <div style="display: grid; grid-template-columns: repeat(4, 1fr); gap: 8px; margin-bottom: 10px;">
              <div style="background: rgba(255,255,255,0.03); padding: 7px; border-radius: 5px; border: 1px solid var(--border-color);">
                <div style="font-size: 10px; color: var(--text-dim);">PORT STATUS</div>
                <div style="font-weight: 700; color: ${statusColor}; font-size: 12px; margin-top: 2px;">${statusLabel}</div>
              </div>
              <div style="background: rgba(255,255,255,0.03); padding: 7px; border-radius: 5px; border: 1px solid var(--border-color);">
                <div style="font-size: 10px; color: var(--text-dim);">RESOLVED IP & DNS</div>
                <div style="font-weight: 600; font-family: monospace; font-size: 11px; margin-top: 2px;">${escapeHtml(ips)} (${res.dns_ms || 0}ms)</div>
              </div>
              <div style="background: rgba(255,255,255,0.03); padding: 7px; border-radius: 5px; border: 1px solid var(--border-color);">
                <div style="font-size: 10px; color: var(--text-dim);">TCP RTT LATENCY</div>
                <div style="font-weight: 700; color: #38bdf8; font-family: monospace; font-size: 11.5px; margin-top: 2px;">${escapeHtml(rttText)}</div>
              </div>
              <div style="background: rgba(255,255,255,0.03); padding: 7px; border-radius: 5px; border: 1px solid var(--border-color);">
                <div style="font-size: 10px; color: var(--text-dim);">JITTER</div>
                <div style="font-weight: 600; font-family: monospace; font-size: 11.5px; margin-top: 2px;">${escapeHtml(jitterText)}</div>
              </div>
            </div>
            <div style="background: #090d16; border: 1px solid var(--border-color); border-radius: 5px; padding: 8px 10px; font-family: monospace; font-size: 11px; line-height: 1.45;">
              <div style="color: #fbbf24; margin-bottom: 6px; font-weight: 600;">Banner: ${escapeHtml(res.banner || '(No service greeting banner returned)')}</div>
              ${sampleRows}
            </div>
          `;
        }
        addLog(`Network Probe [${host}:${port}]: ${res.reachable ? 'OPEN (' + res.avg_ms + ' ms avg)' : 'UNREACHABLE'}`, res.reachable ? 'success' : 'warning');
      } catch (err) {
        if (box) box.innerHTML = `<div style="color: #ef4444; padding: 20px;">Probe Error: ${escapeHtml(err.message || String(err))}</div>`;
      } finally {
        if (btn) {
          btn.disabled = false;
          btn.innerText = 'Run Probe';
        }
      }
    }

    // 2. SSH Keypair Generator & Vault
    let lastGeneratedSshKey = null;

    async function openSshKeygenModal() {
      closeAllMenus();
      document.getElementById('sshKeygenModal')?.classList.add('active');
      await refreshSshKeyVault();
    }

    function closeSshKeygenModal() {
      document.getElementById('sshKeygenModal')?.classList.remove('active');
    }

    async function refreshSshKeyVault() {
      const container = document.getElementById('sshKeysListContainer');
      if (!container) return;
      try {
        const res = await sendRust('ssh_list_keys', {});
        const keys = res.keys || [];
        window._cachedVaultKeys = keys;
        if (keys.length === 0) {
          container.innerHTML = `<div style="padding: 14px; text-align: center; color: var(--text-dim); font-size: 11.5px;">No SSH keys generated yet in <code>%APPDATA%\\90FTP\\ssh_keys</code></div>`;
          return;
        }
        container.innerHTML = keys.map((k, idx) => `
          <div style="display: flex; justify-content: space-between; align-items: center; padding: 6px 8px; border-bottom: 1px solid rgba(255,255,255,0.04); gap: 8px;">
            <div style="min-width: 0; flex: 1;">
              <div style="font-weight: 600; font-size: 11.5px; color: var(--text-main);"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg> ${escapeHtml(k.name)}</div>
              <div style="font-family: monospace; font-size: 10px; color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(k.private_key_path)}</div>
            </div>
            <button type="button" class="btn btn-secondary btn-sm" style="padding: 2px 8px; font-size: 10.5px;" onclick="selectVaultSshKey(${idx})">Select Key</button>
          </div>
        `).join('');
      } catch (err) {
        container.innerHTML = `<div style="padding: 14px; text-align: center; color: #ef4444;">Failed to load SSH key vault</div>`;
      }
    }

    function selectVaultSshKey(idx) {
      const k = (window._cachedVaultKeys || [])[idx];
      if (!k) return;
      lastGeneratedSshKey = k;
      const out = document.getElementById('sshPubKeyOutput');
      if (out) out.value = k.public_key || '';
      const meta = document.getElementById('sshKeyMetaInfo');
      if (meta) meta.innerText = `Selected Key: ${k.name} | Private Key Path: ${k.private_key_path}`;
    }

    async function generateNewSshKeypair() {
      const name = (document.getElementById('sshKeyNameInput')?.value || 'id_ed25519_90ftp').trim();
      const algorithm = document.getElementById('sshKeyAlgoSelect')?.value || 'ed25519';
      const comment = (document.getElementById('sshKeyCommentInput')?.value || '90ftp-admin').trim();
      try {
        const res = await sendRust('ssh_keygen', { name, algorithm, comment });
        lastGeneratedSshKey = res;
        const out = document.getElementById('sshPubKeyOutput');
        if (out) out.value = res.public_key || '';
        const meta = document.getElementById('sshKeyMetaInfo');
        if (meta) meta.innerText = `✓ Fingerprint: ${res.fingerprint || ''} | Saved to: ${res.private_key_path || ''}`;
        addLog(`Generated ${algorithm.toUpperCase()} SSH keypair '${name}' (${res.fingerprint})`, 'success');
        await refreshSshKeyVault();
      } catch (err) {
        alert('SSH Key Generation Failed:\n' + (err.message || err));
      }
    }

    function copyGeneratedSshPubKey() {
      const pub = document.getElementById('sshPubKeyOutput')?.value || '';
      if (!pub) {
        alert('Please generate or select an SSH key first.');
        return;
      }
      navigator.clipboard.writeText(pub).then(() => {
        addLog('Public SSH key copied to clipboard', 'success');
      });
    }

    async function deployGeneratedSshKeyToServer() {
      const pub = document.getElementById('sshPubKeyOutput')?.value?.trim() || '';
      if (!pub) {
        alert('Please generate or select a public key first.');
        return;
      }
      if (!isConnected) {
        alert('Please connect to an SFTP/SSH server first to deploy this key to ~/.ssh/authorized_keys.');
        return;
      }
      try {
        await sendRust('ssh_deploy_key', { public_key: pub });
        addLog('Public key deployed to remote ~/.ssh/authorized_keys', 'success');
        alert('Public key successfully appended to remote ~/.ssh/authorized_keys!');
      } catch (err) {
        alert('Key Deployment Error:\n' + (err.message || err));
      }
    }

    // 3. Remote Server Health & Disk Analyzer
    async function openServerHealthModal() {
      closeAllMenus();
      if (!isConnected) {
        alert('Please connect to a remote SFTP/SSH server first to inspect server telemetry.');
        return;
      }
      document.getElementById('serverHealthModal')?.classList.add('active');
      await refreshServerHealth();
    }

    function closeServerHealthModal() {
      document.getElementById('serverHealthModal')?.classList.remove('active');
    }

    async function refreshServerHealth() {
      const body = document.getElementById('serverHealthBody');
      if (body) body.innerHTML = `<div style="padding: 35px; text-align: center; color: var(--text-dim);">Querying live remote server telemetry...</div>`;
      try {
        const res = await sendRust('remote_server_health', {});
        const memPct = res.mem_total_mb > 0 ? Math.round((res.mem_used_mb / res.mem_total_mb) * 100) : 0;
        const ramStr = res.mem_total_mb > 0
          ? `${res.mem_used_mb} MB / ${res.mem_total_mb} MB (${memPct}%)`
          : 'N/A';
        const disks = res.disks || [];
        const diskRows = disks.map(d => {
          const pctNum = parseInt((d.use_percent || '0').replace('%', ''), 10) || 0;
          const barColor = pctNum > 85 ? '#ef4444' : (pctNum > 70 ? '#f59e0b' : '#10b981');
          return `
            <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
              <td style="padding: 6px 10px; font-weight: 600; font-family: monospace; color: var(--text-main);">${escapeHtml(d.mount)}</td>
              <td style="padding: 6px 10px; font-family: monospace; color: var(--text-dim); font-size: 11px;">${escapeHtml(d.filesystem)}</td>
              <td style="padding: 6px 10px; text-align: right; font-family: monospace;">${escapeHtml(d.used)} / ${escapeHtml(d.size)}</td>
              <td style="padding: 6px 10px; text-align: right; font-family: monospace; color: #10b981;">${escapeHtml(d.avail)}</td>
              <td style="padding: 6px 10px; width: 140px;">
                <div style="display: flex; align-items: center; gap: 6px;">
                  <div style="flex: 1; height: 6px; background: rgba(255,255,255,0.08); border-radius: 3px; overflow: hidden;">
                    <div style="height: 100%; width: ${Math.min(100, pctNum)}%; background: ${barColor};"></div>
                  </div>
                  <span style="font-family: monospace; font-size: 10.5px; min-width: 32px; text-align: right;">${escapeHtml(d.use_percent)}</span>
                </div>
              </td>
            </tr>
          `;
        }).join('');

        if (body) {
          body.innerHTML = `
            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 8px;">
              <div style="background: var(--bg-card); border: 1px solid var(--border-color); border-radius: 6px; padding: 10px;">
                <div style="font-size: 10px; color: var(--text-dim); text-transform: uppercase;">Kernel & Host</div>
                <div style="font-weight: 700; font-family: monospace; font-size: 12px; color: #38bdf8; margin-top: 3px;">${escapeHtml(res.uname || 'Linux Server')}</div>
              </div>
              <div style="background: var(--bg-card); border: 1px solid var(--border-color); border-radius: 6px; padding: 10px;">
                <div style="font-size: 10px; color: var(--text-dim); text-transform: uppercase;">Uptime & Load Average</div>
                <div style="font-weight: 600; font-family: monospace; font-size: 12px; color: #10b981; margin-top: 3px;">${escapeHtml(res.uptime || '—')} ${res.loadavg ? '(Load: ' + escapeHtml(res.loadavg.split(' ').slice(0, 3).join(' ')) + ')' : ''}</div>
              </div>
            </div>
            <div style="background: var(--bg-card); border: 1px solid var(--border-color); border-radius: 6px; padding: 10px;">
              <div style="display: flex; justify-content: space-between; font-size: 11px; margin-bottom: 5px;">
                <span style="color: var(--text-dim); font-weight: 600;">Physical RAM Utilization</span>
                <span style="font-family: monospace; font-weight: 700;">${escapeHtml(ramStr)}</span>
              </div>
              <div style="height: 8px; background: rgba(255,255,255,0.08); border-radius: 4px; overflow: hidden;">
                <div style="height: 100%; width: ${Math.min(100, memPct)}%; background: linear-gradient(90deg, #38bdf8, #818cf8);"></div>
              </div>
            </div>
            <div style="max-height: 220px; overflow-y: auto; background: var(--bg-card); border: 1px solid var(--border-color); border-radius: 6px;">
              <table class="file-table" style="width: 100%; border-collapse: collapse;">
                <thead style="position: sticky; top: 0; background: var(--bg-panel);">
                  <tr>
                    <th style="padding: 6px 10px;">Mount</th>
                    <th style="padding: 6px 10px;">Filesystem</th>
                    <th style="padding: 6px 10px; text-align: right;">Used / Total</th>
                    <th style="padding: 6px 10px; text-align: right;">Available</th>
                    <th style="padding: 6px 10px;">Usage %</th>
                  </tr>
                </thead>
                <tbody>
                  ${diskRows || `<tr><td colspan="5" style="padding: 16px; text-align: center; color: var(--text-dim);">No mount points reported</td></tr>`}
                </tbody>
              </table>
            </div>
          `;
        }
      } catch (err) {
        if (body) body.innerHTML = `<div style="padding: 20px; text-align: center; color: #ef4444;">${escapeHtml(err.message || String(err))}</div>`;
      }
    }

    // ==========================================
    // 5. Remote Transfer & Server-to-Server Dispatcher
    // ==========================================
    let currentRtSources = [];
    let currentRtIsRemote = true;
    let isRtTransferActive = false;

    function openRemoteTransferModal(sourcePath = null, isRemote = null) {
      closeAllMenus();

      if (isRemote === null) {
        isRemote = (currentActivePane === 'remote');
      }
      currentRtIsRemote = isRemote;

      if (sourcePath) {
        currentRtSources = [sourcePath];
      } else {
        const sel = isRemote ? getSelectedRemoteItems() : getSelectedLocalItems();
        if (sel && sel.length > 0) {
          currentRtSources = sel.map(item => item.path);
        } else {
          const curDir = isRemote ? (currentRemotePath || '/') : (currentLocalPath || 'C:\\');
          currentRtSources = curDir ? [curDir] : [];
        }
      }

      // Update Header info
      const locBadge = document.getElementById('rtSourceLocationBadge');
      if (locBadge) {
        const hostName = currentConnectedProfile?.host || currentConnectedProfile?.name || 'Remote Host';
        locBadge.textContent = isRemote ? `Remote Server (${hostName})` : 'Local PC';
      }

      const summaryEl = document.getElementById('rtSourceSummary');
      if (summaryEl) {
        if (currentRtSources.length === 0) {
          summaryEl.textContent = 'No files or folders selected';
        } else if (currentRtSources.length === 1) {
          summaryEl.textContent = currentRtSources[0];
          summaryEl.title = currentRtSources[0];
        } else {
          const names = currentRtSources.slice(0, 3).map(p => p.split(/[\\/]/).pop()).join(', ');
          summaryEl.textContent = `${currentRtSources.length} items (${names}${currentRtSources.length > 3 ? '...' : ''})`;
          summaryEl.title = currentRtSources.join('\n');
        }
      }

      const countBadge = document.getElementById('rtItemsCountBadge');
      if (countBadge) {
        countBadge.textContent = `${currentRtSources.length} Item${currentRtSources.length === 1 ? '' : 's'}`;
      }

      populateRtSavedSitesDropdown();

      // Reset verification & progress status
      const statusEl = document.getElementById('rtTargetVerifyStatus');
      if (statusEl) statusEl.innerHTML = '';
      const progSec = document.getElementById('rtProgressSection');
      if (progSec) progSec.style.display = 'none';
      const cancelBtn = document.getElementById('btnRtCancel');
      if (cancelBtn) cancelBtn.style.display = 'none';
      const startBtn = document.getElementById('btnRtStart');
      if (startBtn) startBtn.disabled = false;
      const logBox = document.getElementById('rtLogBox');
      if (logBox) logBox.innerHTML = '';
      const footerStatus = document.getElementById('rtFooterStatus');
      if (footerStatus) footerStatus.textContent = 'Ready to dispatch selected items';

      const sshpassRow = document.getElementById('rtSshpassRow');
      if (sshpassRow) {
        sshpassRow.style.display = isRemote ? 'flex' : 'none';
        if (isRemote) {
          checkRemoteSshpass(true);
        }
      }

      const modal = document.getElementById('remoteTransferModal');
      if (modal) modal.classList.add('active');
    }

    function closeRemoteTransferModal() {
      if (isRtTransferActive) {
        if (!confirm('A remote transfer is currently running. Do you want to cancel the transfer?')) {
          return;
        }
        cancelRemoteTransfer();
      }
      const modal = document.getElementById('remoteTransferModal');
      if (modal) modal.classList.remove('active');
    }

    function populateRtSavedSitesDropdown() {
      const sel = document.getElementById('rtTargetSiteSelect');
      if (!sel) return;
      sel.innerHTML = '<option value="manual">Custom Server (Manual Credentials)...</option>';

      if (savedSites && savedSites.length > 0) {
        savedSites.forEach(s => {
          const opt = document.createElement('option');
          opt.value = s.id;
          opt.textContent = `${s.name} (${s.protocol.toUpperCase()}://${s.username ? s.username + '@' : ''}${s.host}:${s.port})`;
          sel.appendChild(opt);
        });
      }
    }

    function onRtTargetSiteSelect(val) {
      if (val === 'manual') {
        return;
      }
      const site = (savedSites || []).find(s => s.id === val);
      if (!site) return;

      const protoEl = document.getElementById('rtProtocol');
      if (protoEl) protoEl.value = site.protocol || 'sftp';
      const hostEl = document.getElementById('rtHost');
      if (hostEl) hostEl.value = site.host || '';
      const portEl = document.getElementById('rtPort');
      if (portEl) portEl.value = site.port || (site.protocol === 'ftp' ? 21 : 22);
      const userEl = document.getElementById('rtUsername');
      if (userEl) userEl.value = site.username || '';

      const authType = site.auth_type || (site.key_path ? 'key' : 'password');
      const authEl = document.getElementById('rtAuthType');
      if (authEl) authEl.value = authType;
      toggleRtAuthFields();

      const passEl = document.getElementById('rtPassword');
      if (passEl) passEl.value = site.password || '';
      const keyEl = document.getElementById('rtKeyPath');
      if (keyEl) keyEl.value = site.key_path || '';
      const phraseEl = document.getElementById('rtPassphrase');
      if (phraseEl) phraseEl.value = site.passphrase || '';
      const pathEl = document.getElementById('rtTargetPath');
      if (pathEl && site.remote_dir) pathEl.value = site.remote_dir;
    }

    function onRtProtocolChange(proto) {
      const portEl = document.getElementById('rtPort');
      if (portEl) {
        if (proto === 'ftp') portEl.value = 21;
        else if (proto === 'ftps') portEl.value = 21;
        else portEl.value = 22;
      }
    }

    function toggleRtAuthFields() {
      const auth = document.getElementById('rtAuthType')?.value || 'password';
      const passRow = document.getElementById('rtPassRow');
      const keyRow = document.getElementById('rtKeyRow');
      if (passRow) passRow.style.display = (auth === 'password') ? 'block' : 'none';
      if (keyRow) keyRow.style.display = (auth === 'key') ? 'block' : 'none';
    }

    function buildRtTaskPayload() {
      const authType = document.getElementById('rtAuthType')?.value || 'password';
      return {
        source_is_remote: currentRtIsRemote,
        sources: currentRtSources,
        transfer_mode: document.getElementById('rtTransferMode')?.value || 'auto',
        target_protocol: document.getElementById('rtProtocol')?.value || 'sftp',
        target_host: (document.getElementById('rtHost')?.value || '').trim(),
        target_port: parseInt(document.getElementById('rtPort')?.value || '22', 10) || 22,
        target_user: (document.getElementById('rtUsername')?.value || '').trim(),
        target_pass: document.getElementById('rtPassword')?.value || '',
        target_auth_type: authType,
        target_key_path: (authType === 'key') ? ((document.getElementById('rtKeyPath')?.value || '').trim() || null) : null,
        target_passphrase: (authType === 'key') ? (document.getElementById('rtPassphrase')?.value || null) : null,
        target_path: (document.getElementById('rtTargetPath')?.value || '').trim() || '/',
        recursive: document.getElementById('rtOptRecursive')?.checked !== false,
        overwrite: document.getElementById('rtOptOverwrite')?.checked ? 'overwrite' : 'skip'
      };
    }

    async function testRemoteTransferTarget() {
      const statusEl = document.getElementById('rtTargetVerifyStatus');
      const btn = document.getElementById('btnVerifyRtPath');
      if (!statusEl) return;

      const task = buildRtTaskPayload();
      if (!task.target_host || !task.target_user) {
        statusEl.innerHTML = '<span class="rt-status-pill error">✕ Host and username are required</span>';
        return;
      }

      statusEl.innerHTML = '<span class="rt-status-pill warning">Testing target connection & verifying path...</span>';
      if (btn) btn.disabled = true;

      try {
        const res = await sendRust('remote_transfer_test_target', task);
        if (res.connected) {
          if (res.path_exists) {
            statusEl.innerHTML = `<span class="rt-status-pill success">✓ ${escapeHtml(res.message || 'Target path verified')}</span>`;
          } else {
            statusEl.innerHTML = `<span class="rt-status-pill success">✓ Connected: ${escapeHtml(res.message || 'Target directory will be auto-created')}</span>`;
          }
          addLog(`Remote transfer target verified: ${task.target_host}:${task.target_path}`, 'info');
        } else {
          statusEl.innerHTML = `<span class="rt-status-pill error">✕ Verification failed: ${escapeHtml(res.message || 'Check credentials')}</span>`;
        }
      } catch (err) {
        statusEl.innerHTML = `<span class="rt-status-pill error">✕ Connection error: ${escapeHtml(err.message || String(err))}</span>`;
        addLog(`Remote transfer target check failed: ${err.message}`, 'error');
      } finally {
        if (btn) btn.disabled = false;
      }
    }

    async function checkRemoteSshpass(quiet = false) {
      const statusText = document.getElementById('rtSshpassStatusText');
      const btnCheck = document.getElementById('btnCheckSshpass');
      const btnInstall = document.getElementById('btnInstallSshpass');
      if (statusText) statusText.innerHTML = '<span style="color: var(--accent);">Checking status...</span>';
      if (btnCheck) btnCheck.disabled = true;

      try {
        const res = await sendRust('remote_check_sshpass', {});
        if (res.installed) {
          if (statusText) {
            statusText.innerHTML = `<span style="color: #22c55e; font-weight: 700;">✓ Installed & Ready (${escapeHtml(res.path || 'sshpass')})</span>`;
          }
          if (btnInstall) {
            btnInstall.style.display = 'none';
          }
          if (!quiet) {
            addLog(`sshpass utility verified on remote server: ${res.path} (${res.os_name})`, 'success');
          }
        } else {
          if (statusText) {
            statusText.innerHTML = `<span style="color: #f59e0b; font-weight: 700;">✕ Not Installed (${escapeHtml(res.os_name || 'Linux')})</span>`;
          }
          if (btnInstall) {
            btnInstall.style.display = 'inline-flex';
            btnInstall.disabled = false;
            btnInstall.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg><span>Install sshpass</span>`;
          }
          if (!quiet) {
            addLog(`sshpass is not installed on remote server (${res.os_name}). Click 'Install sshpass' to install automatically.`, 'warning');
          }
        }
        return res;
      } catch (err) {
        if (statusText) {
          statusText.innerHTML = `<span style="color: #ef4444;">✕ Query error: ${escapeHtml(err.message || String(err))}</span>`;
        }
        if (!quiet) {
          addLog(`Failed to query sshpass status: ${err.message || err}`, 'error');
        }
        return null;
      } finally {
        if (btnCheck) btnCheck.disabled = false;
      }
    }

    async function installRemoteSshpass() {
      const statusText = document.getElementById('rtSshpassStatusText');
      const btnInstall = document.getElementById('btnInstallSshpass');
      const btnCheck = document.getElementById('btnCheckSshpass');

      if (!confirm('Install sshpass on the remote source server?\n\n90FTP will automatically detect the Linux distribution (Ubuntu, Debian, CentOS, AlmaLinux, Rocky, Alpine, Arch, openSUSE) and install sshpass via the system package manager.')) {
        return;
      }

      if (statusText) statusText.innerHTML = '<span style="color: var(--accent);"><span class="spin-icon">↻</span> Auto-detecting OS & installing sshpass...</span>';
      if (btnInstall) {
        btnInstall.disabled = true;
        btnInstall.innerHTML = '<span>Installing...</span>';
      }
      if (btnCheck) btnCheck.disabled = true;

      addLog('Detecting Linux OS and installing sshpass on remote server...', 'info');

      try {
        const res = await sendRust('remote_install_sshpass', {});
        if (res.success || res.installed) {
          if (statusText) {
            statusText.innerHTML = `<span style="color: #22c55e; font-weight: 700;">✓ Installed & Ready (${escapeHtml(res.path || 'sshpass')})</span>`;
          }
          if (btnInstall) {
            btnInstall.style.display = 'none';
          }
          addLog(`sshpass is ready on remote server: ${res.path || 'installed'}`, 'success');
          alert(res.already_installed
            ? `sshpass is already installed and verified on this server!\n\nLocation: ${res.path || '/usr/bin/sshpass'}\nDirect Server-to-Server SCP is fully ready.`
            : `sshpass was successfully installed on the server!\n\nLocation: ${res.path || '/usr/bin/sshpass'}\nDirect Server-to-Server SCP is now fully enabled.`);
          await checkRemoteSshpass(true);
        } else {
          if (statusText) {
            statusText.innerHTML = `<span style="color: #ef4444; font-weight: 700;">✕ Installation failed</span>`;
          }
          const errDetail = res.output ? `\nOutput:\n${res.output}` : '';
          addLog(`Failed to install sshpass: ${res.message || 'Unknown error'}${errDetail}`, 'error');
          alert(`Failed to install sshpass automatically:${errDetail}\n\nNote: You can still transfer files using 'Stream Relay Bridge' strategy without needing sshpass.`);
        }
      } catch (err) {
        if (statusText) {
          statusText.innerHTML = `<span style="color: #ef4444; font-weight: 700;">✕ ${escapeHtml(err.message || String(err))}</span>`;
        }
        addLog(`sshpass installation error: ${err.message || err}`, 'error');
        alert(`Error installing sshpass: ${err.message || err}\n\nNote: Stream Relay mode works reliably without sshpass.`);
      } finally {
        if (btnInstall) {
          btnInstall.disabled = false;
          btnInstall.innerHTML = `<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg><span>Install sshpass</span>`;
        }
        if (btnCheck) btnCheck.disabled = false;
      }
    }

    async function promptInstallSshpassOnServer() {
      if (!currentConnectedProfile) {
        alert('Please connect to a remote server first.');
        return;
      }
      openRemoteTransferModal();
      await checkRemoteSshpass();
      installRemoteSshpass();
    }

    async function startRemoteTransfer() {
      if (currentRtSources.length === 0) {
        alert('Please select at least one source file or folder to transfer.');
        return;
      }

      const task = buildRtTaskPayload();
      if (!task.target_host || !task.target_user) {
        alert('Destination Host and Username are required.');
        return;
      }

      const confirmMsg = `Start remote transfer of ${currentRtSources.length} item(s) to:\n${task.target_user}@${task.target_host}:${task.target_path}\n\nTransfer Strategy: ${task.transfer_mode.toUpperCase()}`;
      if (!confirm(confirmMsg)) return;

      isRtTransferActive = true;
      document.getElementById('rtProgressSection').style.display = 'block';
      document.getElementById('btnRtCancel').style.display = 'inline-flex';
      document.getElementById('btnRtStart').disabled = true;
      document.getElementById('rtFooterStatus').textContent = 'Transfer running...';
      document.getElementById('rtProgressBar').style.width = '0%';
      document.getElementById('rtPercentText').textContent = '0%';
      document.getElementById('rtCurrentFile').textContent = 'Initializing transfer session...';
      document.getElementById('rtLogBox').innerHTML = '<div style="color: #38bdf8;">[INIT] Starting remote transfer engine...</div>';

      try {
        await sendRust('remote_transfer_start', task);
        addLog(`Dispatched remote transfer to ${task.target_host}:${task.target_path}`, 'info');
      } catch (err) {
        isRtTransferActive = false;
        document.getElementById('btnRtStart').disabled = false;
        document.getElementById('btnRtCancel').style.display = 'none';
        document.getElementById('rtFooterStatus').textContent = 'Error starting transfer: ' + err.message;
        alert('Failed to start remote transfer: ' + err.message);
      }
    }

    async function cancelRemoteTransfer() {
      try {
        await sendRust('remote_transfer_cancel', {});
        addLog('Remote transfer cancellation requested', 'warning');
        const curFile = document.getElementById('rtCurrentFile');
        if (curFile) curFile.textContent = 'Cancelling transfer session...';
      } catch (err) {
        addLog('Failed to send cancel signal: ' + err.message, 'error');
      }
    }

    // Target Folder Browser Dialog
    async function browseRemoteTransferTarget(targetSubPath = null) {
      const task = buildRtTaskPayload();
      if (!task.target_host || !task.target_user) {
        alert('Please specify destination Host and Username to browse remote folders.');
        return;
      }

      const browseModal = document.getElementById('rtBrowseModal');
      const pathInput = document.getElementById('rtBrowsePathInput');
      const listContainer = document.getElementById('rtBrowseListContainer');
      if (browseModal) browseModal.classList.add('active');

      const reqPath = (targetSubPath !== null) ? targetSubPath : task.target_path;
      if (pathInput) pathInput.value = reqPath || '/';
      if (listContainer) listContainer.innerHTML = '<div style="padding: 12px; text-align: center; color: var(--text-muted);">Connecting to destination server...</div>';

      try {
        const res = await sendRust('remote_transfer_list_target_dir', {
          ...task,
          sub_path: reqPath
        });

        if (pathInput) pathInput.value = res.current_dir || reqPath;

        if (!res.items || res.items.length === 0) {
          listContainer.innerHTML = '<div style="padding: 12px; text-align: center; color: var(--text-dim);">(Empty directory)</div>';
          return;
        }

        let html = '';
        const dirs = res.items.filter(i => i.is_dir && i.name !== '.' && i.name !== '..');
        dirs.sort((a, b) => a.name.localeCompare(b.name));

        if (dirs.length === 0) {
          html += '<div style="padding: 10px; color: var(--text-dim); font-size: 11px;">No subdirectories found. Click "Select This Folder" to choose current directory.</div>';
        } else {
          dirs.forEach(d => {
            html += `
              <div class="tool-native-row" style="padding: 6px 10px; cursor: pointer; border-radius: 4px; display: flex; align-items: center; gap: 8px; margin-bottom: 2px;" onclick="browseRemoteTransferTarget('${escapeJsString(d.path)}')">
                <svg class="icon icon-sm" viewBox="0 0 24 24" style="color: #38bdf8;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                <span style="font-weight: 600; font-size: 11.5px; color: var(--text-main);">${escapeHtml(d.name)}</span>
              </div>
            `;
          });
        }
        listContainer.innerHTML = html;
      } catch (err) {
        if (listContainer) {
          listContainer.innerHTML = `<div style="padding: 12px; color: #ef4444; font-size: 11px;">✕ Error listing directory: ${escapeHtml(err.message || String(err))}</div>`;
        }
      }
    }

    function navigateRtBrowseUp() {
      const pathInput = document.getElementById('rtBrowsePathInput');
      if (!pathInput) return;
      let cur = pathInput.value.trim().replace(/\\/g, '/');
      if (cur.endsWith('/') && cur.length > 1) cur = cur.slice(0, -1);
      const idx = cur.lastIndexOf('/');
      const parent = idx > 0 ? cur.substring(0, idx) : (idx === 0 ? '/' : '');
      browseRemoteTransferTarget(parent || '/');
    }

    function selectRtBrowseCurrentPath() {
      const pathInput = document.getElementById('rtBrowsePathInput');
      if (pathInput && pathInput.value) {
        const targetInput = document.getElementById('rtTargetPath');
        if (targetInput) targetInput.value = pathInput.value;
        testRemoteTransferTarget();
      }
      closeRtBrowseModal();
    }

    function closeRtBrowseModal() {
      const browseModal = document.getElementById('rtBrowseModal');
      if (browseModal) browseModal.classList.remove('active');
    }

    // Global listener hooks called from Rust backend via exec_script
    window.onRemoteTransferLog = function(data) {
      const box = document.getElementById('rtLogBox');
      if (!box) return;
      const line = document.createElement('div');
      line.style.padding = '1px 0';
      if (data.kind === 'error') line.style.color = '#ef4444';
      else if (data.kind === 'warning') line.style.color = '#f59e0b';
      else if (data.kind === 'success') line.style.color = '#22c55e';
      else line.style.color = '#94a3b8';

      line.textContent = `[${new Date().toLocaleTimeString()}] ${data.text}`;
      box.appendChild(line);
      box.scrollTop = box.scrollHeight;
    };

    window.onRemoteTransferProgress = function(data) {
      const bar = document.getElementById('rtProgressBar');
      if (bar) bar.style.width = `${data.percent}%`;
      const pct = document.getElementById('rtPercentText');
      if (pct) pct.textContent = `${Math.round(data.percent)}%`;
      const cur = document.getElementById('rtCurrentFile');
      if (cur) cur.textContent = data.current_file;
      const files = document.getElementById('rtFilesRatio');
      if (files) files.textContent = `${data.done_files} / ${data.total_files} files`;
      const bytes = document.getElementById('rtBytesRatio');
      if (bytes) {
        if (data.total_bytes > 0) {
          bytes.textContent = `${formatBytes(data.done_bytes)} / ${formatBytes(data.total_bytes)}`;
        } else if (data.done_bytes > 0) {
          bytes.textContent = formatBytes(data.done_bytes);
        } else {
          bytes.textContent = '0 B';
        }
      }
      const spd = document.getElementById('rtSpeedText');
      if (spd) spd.textContent = data.speed;
    };

    window.onRemoteTransferComplete = function(data) {
      isRtTransferActive = false;
      const startBtn = document.getElementById('btnRtStart');
      if (startBtn) startBtn.disabled = false;
      const cancelBtn = document.getElementById('btnRtCancel');
      if (cancelBtn) cancelBtn.style.display = 'none';
      const footerStatus = document.getElementById('rtFooterStatus');
      if (footerStatus) footerStatus.textContent = data.success ? 'Transfer completed successfully' : 'Transfer ended';
      if (data.success) {
        const bar = document.getElementById('rtProgressBar');
        if (bar) bar.style.width = '100%';
        const pct = document.getElementById('rtPercentText');
        if (pct) pct.textContent = '100%';
        const cur = document.getElementById('rtCurrentFile');
        if (cur) cur.textContent = 'Transfer Completed!';
        addLog(`Remote transfer completed: ${data.count} items transferred in ${data.elapsed_secs}s`, 'success');
      }
    };

    window.onRemoteTransferError = function(data) {
      isRtTransferActive = false;
      const startBtn = document.getElementById('btnRtStart');
      if (startBtn) startBtn.disabled = false;
      const cancelBtn = document.getElementById('btnRtCancel');
      if (cancelBtn) cancelBtn.style.display = 'none';
      const footerStatus = document.getElementById('rtFooterStatus');
      if (footerStatus) footerStatus.textContent = 'Error: ' + data.error;
      addLog('Remote transfer error: ' + data.error, 'error');
      alert('Remote transfer error:\n' + data.error);
    };

    // 4. Side-by-Side File Content Diff Engine
    async function openFileDiffModal(localPath, remotePath, fileName) {
      const modal = document.getElementById('fileDiffModal');
      if (!modal) return;
      const titleEl = document.getElementById('fileDiffTitle');
      const summaryEl = document.getElementById('fileDiffSummaryBar');
      const containerEl = document.getElementById('fileDiffContainer');
      if (titleEl) titleEl.innerText = `Side-by-Side Diff: ${fileName || ''}`;
      if (summaryEl) summaryEl.innerHTML = `<span>Reading Local (<code>${escapeHtml(localPath)}</code>) & Remote (<code>${escapeHtml(remotePath)}</code>)...</span>`;
      if (containerEl) containerEl.innerHTML = `<div style="padding: 40px; text-align: center; color: var(--text-dim);">Loading file contents for comparison...</div>`;
      modal.classList.add('active');

      try {
        const [locRes, remRes] = await Promise.all([
          sendRust('local_read_file', { path: localPath }),
          sendRust('remote_read_file', { path: remotePath })
        ]);
        const leftLines = (locRes.content || '').replace(/\r\n/g, '\n').split('\n');
        const rightLines = (remRes.content || '').replace(/\r\n/g, '\n').split('\n');
        const maxLen = Math.max(leftLines.length, rightLines.length);

        let rowsHtml = '';
        let diffCount = 0;

        for (let i = 0; i < maxLen; i++) {
          const l = leftLines[i];
          const r = rightLines[i];
          const lNum = i + 1;
          const isSame = (l === r);
          if (!isSame) diffCount++;
          const leftBg = isSame ? 'transparent' : (l === undefined ? 'rgba(255,255,255,0.02)' : 'rgba(239, 68, 68, 0.14)');
          const rightBg = isSame ? 'transparent' : (r === undefined ? 'rgba(255,255,255,0.02)' : 'rgba(16, 185, 129, 0.14)');
          rowsHtml += `
            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03); font-family: 'Consolas', monospace; font-size: 11.5px;">
              <td style="width: 42px; padding: 2px 6px; text-align: right; color: var(--text-dim); user-select: none; border-right: 1px solid var(--border-color);">${l === undefined ? '' : lNum}</td>
              <td style="width: 50%; padding: 2px 8px; background: ${leftBg}; white-space: pre-wrap; word-break: break-all;">${escapeHtml(l ?? '')}</td>
              <td style="width: 42px; padding: 2px 6px; text-align: right; color: var(--text-dim); user-select: none; border-left: 1px solid var(--border-color); border-right: 1px solid var(--border-color);">${r === undefined ? '' : lNum}</td>
              <td style="width: 50%; padding: 2px 8px; background: ${rightBg}; white-space: pre-wrap; word-break: break-all;">${escapeHtml(r ?? '')}</td>
            </tr>
          `;
        }

        if (summaryEl) {
          summaryEl.innerHTML = `
            <span><svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg> <strong>Local:</strong> <code>${escapeHtml(localPath)}</code> (${leftLines.length} lines)</span>
            <span style="font-weight: 700; color: ${diffCount === 0 ? '#10b981' : '#fbbf24'};">${diffCount === 0 ? '✓ Identical Content' : diffCount + ' Line Difference(s)'}</span>
            <span><svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> <strong>Remote:</strong> <code>${escapeHtml(remotePath)}</code> (${rightLines.length} lines)</span>
          `;
        }
        if (containerEl) {
          containerEl.innerHTML = `<table style="width: 100%; border-collapse: collapse; table-layout: fixed;"><tbody>${rowsHtml}</tbody></table>`;
        }
      } catch (err) {
        if (containerEl) containerEl.innerHTML = `<div style="padding: 25px; color: #ef4444;">Diff Error: ${escapeHtml(err.message || String(err))}</div>`;
      }
    }

    function closeFileDiffModal() {
      document.getElementById('fileDiffModal')?.classList.remove('active');
    }

    // ==========================================
    // 5. SYNCHRONIZED BROWSING LOCK-STEP (Ctrl+Y)
    // ==========================================
    function updateSyncBrowsingButtonUI() {
      const btn = document.getElementById('btnSyncBrowsing');
      const active = !!(appOptions && appOptions.syncBrowsing);
      if (btn) {
        btn.style.background = active ? 'rgba(16, 185, 129, 0.18)' : '';
        btn.style.borderColor = active ? '#10b981' : '';
        btn.style.color = active ? '#10b981' : '';
      }
    }

    function updateSyncBrowsingToolbarUI() {
      updateSyncBrowsingButtonUI();
    }

    function toggleSynchronizedBrowsing() {
      closeAllMenus();
      if (!appOptions) appOptions = {};
      appOptions.syncBrowsing = !appOptions.syncBrowsing;
      const optChk = document.getElementById('optSyncBrowsing');
      if (optChk) optChk.checked = appOptions.syncBrowsing;
      updateSyncBrowsingButtonUI();
      saveOptions(false);
      addLog(`Synchronized Browsing (Lock-Step Navigation): ${appOptions.syncBrowsing ? 'ENABLED' : 'DISABLED'}`, appOptions.syncBrowsing ? 'success' : 'info');
    }

    // ==========================================
    // 6. PATH BOOKMARKS & PAIRED WORKSPACE FAVORITES (Ctrl+B)
    // ==========================================
    function getWorkspaceBookmarks() {
      try {
        const raw = localStorage.getItem('90ftp_workspace_bookmarks');
        return raw ? JSON.parse(raw) : [];
      } catch (_) {
        return [];
      }
    }

    function setWorkspaceBookmarks(list) {
      try {
        localStorage.setItem('90ftp_workspace_bookmarks', JSON.stringify(list));
      } catch (_) {}
    }

    function openBookmarksModal() {
      closeAllMenus();
      const lbl = document.getElementById('bmLabelInput');
      if (lbl && !lbl.value) {
        const remName = (currentRemotePath || '/').split('/').filter(Boolean).pop() || 'Root';
        const locName = (currentLocalPath || 'C:\\').split('\\').filter(Boolean).pop() || 'Local';
        lbl.value = `${locName} <svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg> ${remName}`;
      }
      renderBookmarksModalTable();
      document.getElementById('bookmarksModal')?.classList.add('active');
    }

    function closeBookmarksModal() {
      document.getElementById('bookmarksModal')?.classList.remove('active');
    }

    function saveCurrentBookmark() {
      const label = (document.getElementById('bmLabelInput')?.value || '').trim() || 'Saved Workspace';
      const scope = document.getElementById('bmScopeSelect')?.value || 'both';
      const bm = {
        id: 'bm_' + Date.now(),
        label,
        scope,
        local_path: (scope === 'both' || scope === 'local') ? (currentLocalPath || '') : '',
        remote_path: (scope === 'both' || scope === 'remote') ? (currentRemotePath || '') : '',
      };
      const list = getWorkspaceBookmarks();
      list.unshift(bm);
      setWorkspaceBookmarks(list.slice(0, 50));
      renderBookmarksModalTable();
      addLog(`Saved bookmark '${label}'`, 'success');
    }

    function quickBookmarkFolder(path, name, isRemote) {
      const list = getWorkspaceBookmarks();
      list.unshift({
        id: 'bm_' + Date.now(),
        label: name || path,
        scope: isRemote ? 'remote' : 'local',
        local_path: isRemote ? '' : path,
        remote_path: isRemote ? path : ''
      });
      setWorkspaceBookmarks(list.slice(0, 50));
      addLog(`Bookmarked ${isRemote ? 'remote' : 'local'} folder '${name}'`, 'success');
    }

    function renderBookmarksModalTable() {
      const tbody = document.getElementById('bookmarksTableBody');
      if (!tbody) return;
      const list = getWorkspaceBookmarks();
      if (list.length === 0) {
        tbody.innerHTML = `<tr><td colspan="3" style="padding: 35px; text-align: center; color: var(--text-dim); font-style: italic;">No saved workspace bookmarks yet. Click "<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg> Save Current" above to pin your active folders.</td></tr>`;
        return;
      }
      tbody.innerHTML = list.map((b, idx) => `
        <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
          <td style="padding: 7px 10px; font-weight: 600; color: var(--text-main);"><svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg> ${escapeHtml(b.label)}</td>
          <td style="padding: 7px 10px; font-family: monospace; font-size: 11px; color: var(--text-dim);">
            ${b.local_path ? `<div><svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg> ${escapeHtml(b.local_path)}</div>` : ''}
            ${b.remote_path ? `<div><svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg> ${escapeHtml(b.remote_path)}</div>` : ''}
          </td>
          <td style="padding: 7px 8px; text-align: center; white-space: nowrap;">
            <button type="button" class="btn btn-primary btn-sm" style="padding: 2px 8px; font-size: 10.5px;" onclick="jumpToSavedWorkspaceBookmark(${idx})">Go</button>
            <button type="button" class="btn btn-secondary btn-sm" style="padding: 2px 7px; font-size: 10.5px; color: #ef4444;" onclick="deleteSavedWorkspaceBookmark(${idx})">✕</button>
          </td>
        </tr>
      `).join('');
    }

    async function jumpToSavedWorkspaceBookmark(idx) {
      const list = getWorkspaceBookmarks();
      const b = list[idx];
      if (!b) return;
      closeBookmarksModal();
      if (b.local_path) await navigateLocal(b.local_path);
      if (b.remote_path && isConnected) await navigateRemote(b.remote_path);
      addLog(`Jumped to bookmark '${b.label}'`, 'info');
    }

    function deleteSavedWorkspaceBookmark(idx) {
      const list = getWorkspaceBookmarks();
      list.splice(idx, 1);
      setWorkspaceBookmarks(list);
      renderBookmarksModalTable();
    }

    // ==========================================
    // 7. FULL-TEXT CONTENT GREP INSIDE FILES (Ctrl+Shift+F)
    // ==========================================
    let currentGrepMatches = [];

    function openContentGrepModal() {
      closeAllMenus();
      const scopeSel = document.getElementById('grepScopeSelect');
      if (scopeSel) scopeSel.value = isConnected ? 'remote' : 'local';
      onGrepScopeChange();
      document.getElementById('contentGrepModal')?.classList.add('active');
      setTimeout(() => document.getElementById('grepPatternInput')?.focus(), 60);
    }

    function openContentGrepAtPath(path, isRemote) {
      closeAllMenus();
      const scopeSel = document.getElementById('grepScopeSelect');
      if (scopeSel) scopeSel.value = isRemote ? 'remote' : 'local';
      const pathInp = document.getElementById('grepPathInput');
      if (pathInp) pathInp.value = path;
      document.getElementById('contentGrepModal')?.classList.add('active');
      setTimeout(() => document.getElementById('grepPatternInput')?.focus(), 60);
    }

    function closeContentGrepModal() {
      document.getElementById('contentGrepModal')?.classList.remove('active');
    }

    function onGrepScopeChange() {
      const scope = document.getElementById('grepScopeSelect')?.value || 'remote';
      const pathInp = document.getElementById('grepPathInput');
      if (pathInp) {
        pathInp.value = scope === 'local' ? (currentLocalPath || 'C:\\') : (currentRemotePath || '/');
      }
    }

    async function executeContentGrep() {
      const scope = document.getElementById('grepScopeSelect')?.value || 'remote';
      if (scope === 'remote' && !isConnected) {
        alert('Not connected to a remote server. Switch Scope to "Local Computer" or connect to a server first.');
        return;
      }
      const path = (document.getElementById('grepPathInput')?.value || '').trim();
      const pattern = (document.getElementById('grepPatternInput')?.value || '').trim();
      const file_mask = (document.getElementById('grepFileMaskInput')?.value || '').trim();
      const case_sensitive = !!document.getElementById('grepCaseCheckbox')?.checked;

      if (!pattern) {
        alert('Please enter a text or code pattern to search inside files.');
        return;
      }

      const btn = document.getElementById('btnRunGrep');
      const status = document.getElementById('grepStatusSummary');
      const tbody = document.getElementById('grepResultsBody');
      if (btn) { btn.disabled = true; btn.innerText = 'Searching...'; }
      if (status) status.innerText = `Scanning files in ${path} for "${pattern}"...`;
      if (tbody) tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 40px; color: #38bdf8;">Scanning file contents...</td></tr>`;

      try {
        const cmd = scope === 'local' ? 'local_grep_content' : 'remote_grep_content';
        const res = await sendRust(cmd, { path, pattern, file_mask, case_sensitive, max_results: 200 });
        currentGrepMatches = (res.matches || []).map(m => ({ ...m, _scope: scope }));
        if (status) status.innerText = `Found ${currentGrepMatches.length} content match(es) for "${pattern}".`;
        renderContentGrepResults();
      } catch (err) {
        if (status) status.innerText = 'Grep error: ' + (err.message || err);
        if (tbody) tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 30px; color: #ef4444;">${escapeHtml(err.message || String(err))}</td></tr>`;
      } finally {
        if (btn) { btn.disabled = false; btn.innerText = 'Search Content'; }
      }
    }

    function renderContentGrepResults() {
      const tbody = document.getElementById('grepResultsBody');
      if (!tbody) return;
      if (currentGrepMatches.length === 0) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align: center; padding: 40px; color: var(--text-dim); font-style: italic;">No matching lines found inside files.</td></tr>`;
        return;
      }
      tbody.innerHTML = currentGrepMatches.map((m, idx) => `
        <tr style="border-bottom: 1px solid rgba(255,255,255,0.04); cursor: pointer;" ondblclick="openGrepMatchInEditor(${idx})">
          <td style="padding: 6px 10px;">
            <div style="font-weight: 600; color: var(--text-main); font-size: 11.5px;">${escapeHtml(m.name)}</div>
            <div style="font-family: monospace; font-size: 10px; color: var(--text-dim); word-break: break-all;">${escapeHtml(m.dir)}</div>
          </td>
          <td style="padding: 6px 8px; text-align: right; font-family: monospace; color: #fbbf24; font-weight: 700;">:${m.line}</td>
          <td style="padding: 6px 10px; font-family: monospace; font-size: 11px; color: #38bdf8; word-break: break-all;">${escapeHtml(m.snippet)}</td>
          <td style="padding: 6px 8px; text-align: center; white-space: nowrap;">
            <button type="button" class="btn btn-primary btn-sm" style="padding: 2px 7px; font-size: 10px;" onclick="openGrepMatchInEditor(${idx})"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"/></svg> Edit</button>
            <button type="button" class="btn btn-secondary btn-sm" style="padding: 2px 7px; font-size: 10px;" onclick="jumpToGrepMatchFolder(${idx})"><svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="2" y1="10" x2="22" y2="10"/></svg> Go</button>
          </td>
        </tr>
      `).join('');
    }

    function openGrepMatchInEditor(idx) {
      const m = currentGrepMatches[idx];
      if (!m) return;
      closeContentGrepModal();
      if (m._scope === 'local') openLocalFileInEditor(m.path);
      else openRemoteFileInEditor(m.path);
    }

    async function jumpToGrepMatchFolder(idx) {
      const m = currentGrepMatches[idx];
      if (!m) return;
      closeContentGrepModal();
      if (m._scope === 'local') await navigateLocal(m.dir);
      else await navigateRemote(m.dir);
    }

    // ==========================================
    // 8. BATCH MULTI-RENAME ENGINE (Ctrl+M)
    // ==========================================
    let batchRenameTargetItems = [];
    let batchRenameIsRemote = false;

    function openBatchRenameModal(forceRemote = null) {
      closeAllMenus();
      const useRem = forceRemote !== null ? !!forceRemote : lastActivePaneIsRemote;
      let items = useRem ? getSelectedRemoteItems() : getSelectedLocalItems();
      if (items.length === 0) {
        items = useRem ? (remoteFiles || []) : (localFiles || []);
      }
      if (items.length === 0) {
        alert(`No ${useRem ? 'remote' : 'local'} items available to batch rename.`);
        return;
      }
      batchRenameIsRemote = useRem;
      batchRenameTargetItems = items.map(it => ({ ...it }));
      document.getElementById('batchRenameTitle').innerText = `Batch Multi-Rename (${batchRenameTargetItems.length} ${useRem ? 'Remote' : 'Local'} Items)`;
      document.getElementById('brFindInput').value = '';
      document.getElementById('brReplaceInput').value = '';
      document.getElementById('brPrefixInput').value = '';
      document.getElementById('brSuffixInput').value = '';
      document.getElementById('brCaseSelect').value = 'none';
      updateBatchRenamePreview();
      document.getElementById('batchRenameModal')?.classList.add('active');
    }

    function closeBatchRenameModal() {
      document.getElementById('batchRenameModal')?.classList.remove('active');
    }

    function computeBatchRenamedFilename(origName, idx, isDir) {
      const findStr = document.getElementById('brFindInput')?.value || '';
      const repStr = document.getElementById('brReplaceInput')?.value || '';
      const useRegex = !!document.getElementById('brRegexCheckbox')?.checked;
      const prefix = document.getElementById('brPrefixInput')?.value || '';
      const suffix = document.getElementById('brSuffixInput')?.value || '';
      const caseMode = document.getElementById('brCaseSelect')?.value || 'none';
      const startNum = parseInt(document.getElementById('brCounterStart')?.value || '1', 10) || 1;
      const padDigits = parseInt(document.getElementById('brCounterPad')?.value || '3', 10) || 0;

      const counterVal = startNum + idx;
      const counterStr = padDigits > 0 ? String(counterVal).padStart(padDigits, '0') : String(counterVal);
      const injectCounter = (s) => s.replace(/\[N\]/gi, counterStr);

      let stem = origName;
      let ext = '';
      if (!isDir) {
        const lastDot = origName.lastIndexOf('.');
        if (lastDot > 0) {
          stem = origName.substring(0, lastDot);
          ext = origName.substring(lastDot);
        }
      }

      if (findStr) {
        try {
          if (useRegex) {
            const rx = new RegExp(findStr, 'g');
            stem = stem.replace(rx, injectCounter(repStr));
          } else {
            stem = stem.split(findStr).join(injectCounter(repStr));
          }
        } catch (_) {}
      }

      stem = injectCounter(prefix) + stem + injectCounter(suffix);

      if (caseMode === 'lower') {
        stem = stem.toLowerCase();
        ext = ext.toLowerCase();
      } else if (caseMode === 'upper') {
        stem = stem.toUpperCase();
        ext = ext.toUpperCase();
      } else if (caseMode === 'title') {
        stem = stem.replace(/\b\w/g, c => c.toUpperCase());
      }

      return stem + ext;
    }

    function updateBatchRenamePreview() {
      const tbody = document.getElementById('batchRenamePreviewBody');
      if (!tbody) return;
      let changedCount = 0;
      tbody.innerHTML = batchRenameTargetItems.map((it, idx) => {
        const newName = computeBatchRenamedFilename(it.name, idx, it.is_dir);
        const changed = newName !== it.name && newName.trim().length > 0;
        if (changed) changedCount++;
        return `
          <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
            <td style="padding: 5px 10px; font-family: monospace; color: var(--text-muted);">${escapeHtml(it.name)}</td>
            <td style="padding: 5px 10px; font-family: monospace; font-weight: ${changed ? '700' : '400'}; color: ${changed ? '#10b981' : 'var(--text-dim)'};">${escapeHtml(newName)}</td>
            <td style="padding: 5px 8px; text-align: center; font-size: 10.5px;">${changed ? '<span style="color:#10b981;">RENAME</span>' : '<span style="color:var(--text-dim);">—</span>'}</td>
          </tr>
        `;
      }).join('');
      const sum = document.getElementById('batchRenameSummaryText');
      if (sum) sum.innerText = `${changedCount} of ${batchRenameTargetItems.length} item(s) will be renamed.`;
    }

    async function executeBatchRename() {
      const ops = [];
      const sep = batchRenameIsRemote ? '/' : '\\';
      const baseDir = (batchRenameIsRemote ? currentRemotePath : currentLocalPath).replace(/[\\/]+$/, '');

      batchRenameTargetItems.forEach((it, idx) => {
        const newName = computeBatchRenamedFilename(it.name, idx, it.is_dir).trim();
        if (newName && newName !== it.name) {
          const newPath = baseDir ? `${baseDir}${sep}${newName}` : `${sep}${newName}`;
          ops.push({ old_path: it.path, new_path: newPath });
        }
      });

      if (ops.length === 0) {
        alert('No filenames have changed yet. Adjust the rename rules above.');
        return;
      }

      try {
        const cmd = batchRenameIsRemote ? 'remote_batch_rename' : 'local_batch_rename';
        const res = await sendRust(cmd, {
          current_dir: batchRenameIsRemote ? currentRemotePath : currentLocalPath,
          ops
        });
        if (batchRenameIsRemote) renderRemoteFiles(res.files || []);
        else renderLocalFiles(res.files || []);
        addLog(`Batch renamed ${res.renamed || ops.length} item(s) successfully`, 'success');
        closeBatchRenameModal();
      } catch (err) {
        alert('Batch Rename Failed:\n' + (err.message || err));
      }
    }

    // ==========================================
    // 9. REMOTE LIVE PROCESS, PORT & SERVICE MANAGER
    // ==========================================
    let pmActiveTab = 'procs';
    let pmCachedData = { processes: [], ports: [], services: [] };

    async function openProcessManagerModal() {
      closeAllMenus();
      if (!isConnected) {
        alert('Please connect to a remote SFTP/SSH server first.');
        return;
      }
      document.getElementById('processManagerModal')?.classList.add('active');
      await refreshProcessManagerData();
    }

    function closeProcessManagerModal() {
      document.getElementById('processManagerModal')?.classList.remove('active');
    }

    function switchProcessManagerTab(tab) {
      pmActiveTab = tab;
      ['procs', 'ports', 'services'].forEach(t => {
        const btn = document.getElementById(t === 'procs' ? 'pmTabProcs' : (t === 'ports' ? 'pmTabPorts' : 'pmTabServices'));
        if (btn) {
          btn.className = t === tab ? 'btn btn-primary btn-sm' : 'btn btn-secondary btn-sm';
        }
      });
      renderProcessManagerActiveTab();
    }

    async function refreshProcessManagerData() {
      const box = document.getElementById('processManagerContentBox');
      if (box) box.innerHTML = `<div style="text-align: center; padding: 50px; color: #38bdf8;">Querying live processes, listening ports, and systemd services...</div>`;
      try {
        const res = await sendRust('remote_process_manager', {});
        pmCachedData = {
          processes: res.processes || [],
          ports: res.ports || [],
          services: res.services || []
        };
        renderProcessManagerActiveTab();
      } catch (err) {
        if (box) box.innerHTML = `<div style="text-align: center; padding: 40px; color: #ef4444;">${escapeHtml(err.message || String(err))}</div>`;
      }
    }

    function renderProcessManagerActiveTab() {
      const box = document.getElementById('processManagerContentBox');
      if (!box) return;
      const filter = (document.getElementById('pmFilterInput')?.value || '').trim().toLowerCase();

      if (pmActiveTab === 'procs') {
        const list = (pmCachedData.processes || []).filter(p =>
          !filter || p.command.toLowerCase().includes(filter) || p.user.toLowerCase().includes(filter) || p.pid.includes(filter)
        );
        box.innerHTML = `
          <table class="file-table" style="width: 100%; border-collapse: collapse;">
            <thead style="position: sticky; top: 0; background: var(--bg-panel);">
              <tr>
                <th style="width: 70px; padding: 6px 8px;">PID</th>
                <th style="width: 90px; padding: 6px 8px;">User</th>
                <th style="width: 65px; padding: 6px 8px; text-align: right;">CPU %</th>
                <th style="width: 65px; padding: 6px 8px; text-align: right;">MEM %</th>
                <th style="padding: 6px 10px;">Command</th>
                <th style="width: 120px; padding: 6px 8px; text-align: center;">Signal</th>
              </tr>
            </thead>
            <tbody>
              ${list.map(p => `
                <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
                  <td style="padding: 5px 8px; font-family: monospace; color: #fbbf24; font-weight: 700;">${escapeHtml(p.pid)}</td>
                  <td style="padding: 5px 8px; font-family: monospace;">${escapeHtml(p.user)}</td>
                  <td style="padding: 5px 8px; text-align: right; font-family: monospace; color: ${parseFloat(p.cpu) > 25 ? '#ef4444' : '#10b981'}; font-weight: 600;">${escapeHtml(p.cpu)}%</td>
                  <td style="padding: 5px 8px; text-align: right; font-family: monospace;">${escapeHtml(p.mem)}%</td>
                  <td style="padding: 5px 10px; font-family: monospace; font-size: 11px; color: var(--text-main); word-break: break-all;">${escapeHtml(p.command)}</td>
                  <td style="padding: 5px 8px; text-align: center; white-space: nowrap;">
                    <button type="button" class="btn btn-secondary btn-sm" style="padding: 1px 6px; font-size: 10px;" onclick="killRemoteProcessByPid('${escapeJsString(p.pid)}', '15')">TERM</button>
                    <button type="button" class="btn btn-danger btn-sm" style="padding: 1px 6px; font-size: 10px;" onclick="killRemoteProcessByPid('${escapeJsString(p.pid)}', '9')">KILL</button>
                  </td>
                </tr>
              `).join('') || `<tr><td colspan="6" style="text-align: center; padding: 30px; color: var(--text-dim);">No processes match filter</td></tr>`}
            </tbody>
          </table>
        `;
      } else if (pmActiveTab === 'ports') {
        const list = (pmCachedData.ports || []).filter(p =>
          !filter || p.local_addr.toLowerCase().includes(filter) || p.process.toLowerCase().includes(filter) || p.proto.toLowerCase().includes(filter)
        );
        box.innerHTML = `
          <table class="file-table" style="width: 100%; border-collapse: collapse;">
            <thead style="position: sticky; top: 0; background: var(--bg-panel);">
              <tr>
                <th style="width: 80px; padding: 6px 10px;">Proto</th>
                <th style="width: 100px; padding: 6px 10px;">State</th>
                <th style="width: 220px; padding: 6px 10px;">Local Address : Port</th>
                <th style="padding: 6px 10px;">Process / Socket Owner</th>
              </tr>
            </thead>
            <tbody>
              ${list.map(p => `
                <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
                  <td style="padding: 6px 10px; font-family: monospace; font-weight: 700; color: #38bdf8;">${escapeHtml(p.proto.toUpperCase())}</td>
                  <td style="padding: 6px 10px; font-family: monospace; color: #10b981;">${escapeHtml(p.state)}</td>
                  <td style="padding: 6px 10px; font-family: monospace; font-weight: 600; color: var(--text-main);">${escapeHtml(p.local_addr)}</td>
                  <td style="padding: 6px 10px; font-family: monospace; font-size: 11px; color: var(--text-muted);">${escapeHtml(p.process)}</td>
                </tr>
              `).join('') || `<tr><td colspan="4" style="text-align: center; padding: 30px; color: var(--text-dim);">No listening sockets match filter</td></tr>`}
            </tbody>
          </table>
        `;
      } else {
        const list = (pmCachedData.services || []).filter(s =>
          !filter || s.unit.toLowerCase().includes(filter) || s.description.toLowerCase().includes(filter)
        );
        box.innerHTML = `
          <table class="file-table" style="width: 100%; border-collapse: collapse;">
            <thead style="position: sticky; top: 0; background: var(--bg-panel);">
              <tr>
                <th style="width: 220px; padding: 6px 10px;">Service Unit</th>
                <th style="width: 90px; padding: 6px 8px;">State</th>
                <th style="padding: 6px 10px;">Description</th>
                <th style="width: 165px; padding: 6px 8px; text-align: center;">Control</th>
              </tr>
            </thead>
            <tbody>
              ${list.map(s => `
                <tr style="border-bottom: 1px solid rgba(255,255,255,0.04);">
                  <td style="padding: 6px 10px; font-family: monospace; font-weight: 600; color: var(--text-main);">${escapeHtml(s.unit)}</td>
                  <td style="padding: 6px 8px; font-family: monospace; color: ${s.sub === 'running' ? '#10b981' : '#ef4444'}; font-weight: 700;">${escapeHtml(s.sub)}</td>
                  <td style="padding: 6px 10px; font-size: 11px; color: var(--text-muted);">${escapeHtml(s.description)}</td>
                  <td style="padding: 6px 8px; text-align: center; white-space: nowrap;">
                    <button type="button" class="btn btn-secondary btn-sm" style="padding: 1px 6px; font-size: 10px;" onclick="controlRemoteSystemdService('${escapeJsString(s.unit)}', 'restart')">↻ Restart</button>
                    <button type="button" class="btn btn-secondary btn-sm" style="padding: 1px 6px; font-size: 10px;" onclick="controlRemoteSystemdService('${escapeJsString(s.unit)}', 'reload')">Reload</button>
                  </td>
                </tr>
              `).join('') || `<tr><td colspan="4" style="text-align: center; padding: 30px; color: var(--text-dim);">No systemd services reported</td></tr>`}
            </tbody>
          </table>
        `;
      }
    }

    async function killRemoteProcessByPid(pid, signal) {
      if (!confirm(`Send signal -${signal} to remote process PID ${pid}?`)) return;
      try {
        await sendRust('remote_kill_process', { pid, signal });
        addLog(`Sent signal -${signal} to remote PID ${pid}`, 'success');
        await refreshProcessManagerData();
      } catch (err) {
        alert('Kill Process Failed:\n' + (err.message || err));
      }
    }

    async function controlRemoteSystemdService(unit, op) {
      if (!confirm(`Execute 'systemctl ${op} ${unit}' on remote server?`)) return;
      try {
        await sendRust('remote_service_action', { unit, op });
        addLog(`Executed systemctl ${op} ${unit}`, 'success');
        await refreshProcessManagerData();
      } catch (err) {
        alert('Service Action Failed:\n' + (err.message || err));
      }
    }

    // ==========================================
    // 10. LIVE LOG TAIL STREAMER & BINARY HEX INSPECTOR
    // ==========================================
    let inspectorState = { path: '', name: '', isRemote: false, mode: 'tail', rawContent: '', timer: null };

    async function openFileInspectorModal(path, name, isRemote) {
      closeAllMenus();
      if (inspectorState.timer) {
        clearInterval(inspectorState.timer);
        inspectorState.timer = null;
      }
      inspectorState = { path, name, isRemote: !!isRemote, mode: 'tail', rawContent: '', timer: null };
      document.getElementById('fileInspectorTitle').innerText = `Live Tail & Hex Inspector — ${name}`;
      const followChk = document.getElementById('inspAutoFollowCheck');
      if (followChk) followChk.checked = false;
      setFileInspectorMode('tail', false);
      document.getElementById('fileInspectorModal')?.classList.add('active');
      await refreshFileInspector();
    }

    function closeFileInspectorModal() {
      if (inspectorState.timer) {
        clearInterval(inspectorState.timer);
        inspectorState.timer = null;
      }
      const followChk = document.getElementById('inspAutoFollowCheck');
      if (followChk) followChk.checked = false;
      document.getElementById('fileInspectorModal')?.classList.remove('active');
    }

    function setFileInspectorMode(mode, shouldRefresh = true) {
      inspectorState.mode = mode;
      const btnTail = document.getElementById('inspBtnTail');
      const btnHex = document.getElementById('inspBtnHex');
      if (btnTail) btnTail.className = mode === 'tail' ? 'btn btn-primary btn-sm' : 'btn btn-secondary btn-sm';
      if (btnHex) btnHex.className = mode === 'hex' ? 'btn btn-primary btn-sm' : 'btn btn-secondary btn-sm';
      const linesSel = document.getElementById('inspLinesSelect');
      const followLbl = document.getElementById('inspFollowLabel');
      if (linesSel) linesSel.style.display = mode === 'tail' ? '' : 'none';
      if (followLbl) followLbl.style.display = mode === 'tail' ? 'flex' : 'none';
      if (shouldRefresh) refreshFileInspector();
    }

    function toggleInspectorAutoFollow() {
      const checked = !!document.getElementById('inspAutoFollowCheck')?.checked;
      if (inspectorState.timer) {
        clearInterval(inspectorState.timer);
        inspectorState.timer = null;
      }
      if (checked && inspectorState.mode === 'tail') {
        inspectorState.timer = setInterval(() => {
          refreshFileInspector(true);
        }, 2000);
      }
    }

    async function refreshFileInspector(silent = false) {
      if (!inspectorState.path) return;
      const pre = document.getElementById('fileInspectorPre');
      const meta = document.getElementById('fileInspectorMeta');
      const lines = parseInt(document.getElementById('inspLinesSelect')?.value || '150', 10) || 150;
      if (!silent && pre) pre.innerText = 'Loading file stream...';
      try {
        const cmd = inspectorState.isRemote ? 'remote_file_inspect' : 'local_file_inspect';
        const res = await sendRust(cmd, {
          path: inspectorState.path,
          mode: inspectorState.mode,
          lines,
          max_bytes: 4096
        });
        inspectorState.rawContent = res.content || '';
        if (meta) {
          meta.innerText = `${inspectorState.isRemote ? 'REMOTE' : 'LOCAL'}: ${inspectorState.path} ${res.size ? '(' + formatBytes(res.size) + ')' : ''} | Updated ${new Date().toLocaleTimeString()}`;
        }
        renderFileInspectorOutput();
      } catch (err) {
        if (pre) pre.innerText = 'Error inspecting file: ' + (err.message || err);
      }
    }

    function renderFileInspectorOutput() {
      const pre = document.getElementById('fileInspectorPre');
      if (!pre) return;
      const filter = (document.getElementById('inspFilterInput')?.value || '').trim().toLowerCase();
      const raw = inspectorState.rawContent || '';
      if (inspectorState.mode === 'hex') {
        pre.innerText = raw;
        return;
      }
      const lines = raw.split('\n').filter(l => !filter || l.toLowerCase().includes(filter));
      pre.innerHTML = lines.map(line => {
        const up = line.toUpperCase();
        if (up.includes('ERROR') || up.includes('FATAL') || up.includes('CRITICAL') || up.includes('EXCEPTION')) {
          return `<span style="color: #f87171;">${escapeHtml(line)}</span>`;
        } else if (up.includes('WARN') || up.includes('WARNING')) {
          return `<span style="color: #fbbf24;">${escapeHtml(line)}</span>`;
        } else if (up.includes('INFO') || up.includes('SUCCESS')) {
          return `<span style="color: #38bdf8;">${escapeHtml(line)}</span>`;
        }
        return escapeHtml(line);
      }).join('\n');
      pre.scrollTop = pre.scrollHeight;
    }

    // ==========================================
    // 11. OMNI COMMAND PALETTE (Ctrl+K / Ctrl+P)
    // ==========================================
    let cmdPaletteFiltered = [];
    let cmdPaletteSelectedIndex = 0;

    function openCommandPalette() {
      closeAllMenus();
      const modal = document.getElementById('commandPaletteModal');
      if (!modal) return;
      const inp = document.getElementById('cmdPaletteInput');
      if (inp) inp.value = '';
      cmdPaletteSelectedIndex = 0;
      renderCommandPaletteItems();
      modal.classList.add('active');
      setTimeout(() => inp?.focus(), 40);
    }

    function closeCommandPalette() {
      document.getElementById('commandPaletteModal')?.classList.remove('active');
    }

    function buildCommandPaletteCatalog() {
      const items = [
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>', category: 'Tool', title: 'Find Files (Remote Server & Local Disk)', shortcut: 'Ctrl+F', run: () => openFindFilesModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 2 7 12 12 22 7 12 2"/><polyline points="2 17 12 22 22 17"/><polyline points="2 12 12 17 22 12"/></svg>', category: 'Tool', title: 'Full-Text Content Grep (Search Inside Files)', shortcut: 'Ctrl+Shift+F', run: () => openContentGrepModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>', category: 'Tool', title: 'Remote Transfer & Server-to-Server Dispatcher (SCP / SFTP / Rsync)', shortcut: '', run: () => openRemoteTransferModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"/></svg>', category: 'Tool', title: 'Batch Multi-Rename Selected Items (Regex / Counter)', shortcut: 'Ctrl+M', run: () => openBatchRenameModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>', category: 'Navigation', title: `Toggle Synchronized Browsing (Lock-Step: ${appOptions?.syncBrowsing ? 'ON' : 'OFF'})`, shortcut: 'Ctrl+Y', run: () => toggleSynchronizedBrowsing() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>', category: 'Navigation', title: 'Path Bookmarks & Paired Workspace Favorites', shortcut: 'Ctrl+B', run: () => openBookmarksModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M12 3h7a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-7m0-18H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h7m0-18v18"/></svg>', category: 'Sync', title: 'Compare & Synchronize Directories', shortcut: 'Shift+F1', run: () => toggleCompareDirectories() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>', category: 'Sync', title: 'Two-Way Folder Sync (Dry Run Preview, Then Apply)', shortcut: 'Ctrl+Shift+Y', run: () => openSyncModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>', category: 'Server', title: 'Remote Process, Listening Port & Systemd Service Manager', shortcut: '', run: () => openProcessManagerModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="18" y1="20" x2="18" y2="10"/><line x1="12" y1="20" x2="12" y2="4"/><line x1="6" y1="20" x2="6" y2="14"/></svg>', category: 'Server', title: 'Remote Server Health & Disk Space Analyzer', shortcut: '', run: () => openServerHealthModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>', category: 'Network', title: 'Network Ping, TCP Port Probe & Service Banner Inspector', shortcut: '', run: () => openNetworkProbeModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg>', category: 'Security', title: 'SSH Keypair Generator & Public Key Vault', shortcut: '', run: () => openSshKeygenModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>', category: 'Server', title: 'Open Interactive SSH Terminal', shortcut: '', run: () => openRemoteTerminal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>', category: 'Transfer', title: 'Configure Transfer Exclusion Mask Rules', shortcut: '', run: () => openExclusionMaskModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="8" rx="2" ry="2"/><rect x="2" y="14" width="20" height="8" rx="2" ry="2"/><line x1="6" y1="6" x2="6.01" y2="6"/><line x1="6" y1="18" x2="6.01" y2="18"/></svg>', category: 'Connections', title: 'Open Site Manager', shortcut: 'Ctrl+S', run: () => openSitesModal() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>', category: 'View', title: 'Toggle Hidden Dotfiles (.env, .htaccess)', shortcut: 'Ctrl+H', run: () => toggleHiddenFiles() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M12 2.69l5.66 5.66a8 8 0 1 1-11.31 0z"/></svg>', category: 'View', title: 'Switch Application Theme (Dark / Light)', shortcut: 'Ctrl+T', run: () => toggleTheme() },
        { icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>', category: 'Updates', title: 'Check for Application Updates', shortcut: 'Ctrl+U', run: () => openUpdateModal(true) },
      ];

      // Add Saved Server Sites
      (savedSites || []).forEach(s => {
        items.push({
          icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>',
          category: 'Saved Server',
          title: `Connect to ${s.name} (${s.username || ''}@${s.host}:${s.port || 22})`,
          shortcut: (s.protocol || 'SFTP').toUpperCase(),
          run: () => {
            const sel = document.getElementById('quickSiteSelect');
            if (sel) {
              sel.value = s.id;
              onQuickSiteSelect(s.id, true);
            }
          }
        });
      });

      // Add Saved Workspace Bookmarks
      getWorkspaceBookmarks().forEach((b, idx) => {
        items.push({
          icon: '<svg class="icon icon-sm" viewBox="0 0 24 24"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>',
          category: 'Bookmark',
          title: `Bookmark: ${b.label}`,
          shortcut: b.remote_path || b.local_path || '',
          run: () => jumpToSavedWorkspaceBookmark(idx)
        });
      });

      // Add Current Remote Files (up to 40)
      (remoteFiles || []).slice(0, 40).forEach(f => {
        items.push({
          icon: f.is_dir ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>' : '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><polyline points="13 2 13 9 20 9"/></svg>',
          category: 'Remote Item',
          title: `Remote: ${f.name}`,
          shortcut: f.path,
          run: () => {
            if (f.is_dir) navigateRemote(f.path);
            else openRemoteFileInEditor(f.path);
          }
        });
      });

      // Add Current Local Files (up to 40)
      (localFiles || []).slice(0, 40).forEach(f => {
        items.push({
          icon: f.is_dir ? '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>' : '<svg class="icon icon-sm" viewBox="0 0 24 24"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><polyline points="13 2 13 9 20 9"/></svg>',
          category: 'Local Item',
          title: `Local: ${f.name}`,
          shortcut: f.path,
          run: () => {
            if (f.is_dir) navigateLocal(f.path);
            else openLocalFileInEditor(f.path);
          }
        });
      });

      return items;
    }

    function renderCommandPaletteItems() {
      const listEl = document.getElementById('cmdPaletteList');
      if (!listEl) return;
      const rawQ = (document.getElementById('cmdPaletteInput')?.value || '').trim().toLowerCase();
      const all = buildCommandPaletteCatalog();

      cmdPaletteFiltered = all.filter(it => {
        if (!rawQ) return true;
        if (rawQ.startsWith('>')) {
          const q = rawQ.substring(1).trim();
          return ['Tool', 'Navigation', 'Sync', 'Server', 'Network', 'Security', 'Transfer', 'View'].includes(it.category) &&
                 (!q || it.title.toLowerCase().includes(q));
        }
        if (rawQ.startsWith('@')) {
          const q = rawQ.substring(1).trim();
          return it.category === 'Saved Server' && (!q || it.title.toLowerCase().includes(q));
        }
        if (rawQ.startsWith('#')) {
          const q = rawQ.substring(1).trim();
          return (it.category === 'Remote Item' || it.category === 'Local Item') && (!q || it.title.toLowerCase().includes(q));
        }
        return it.title.toLowerCase().includes(rawQ) ||
               it.category.toLowerCase().includes(rawQ) ||
               (it.shortcut && it.shortcut.toLowerCase().includes(rawQ));
      }).slice(0, 35);

      if (cmdPaletteSelectedIndex >= cmdPaletteFiltered.length) {
        cmdPaletteSelectedIndex = 0;
      }

      if (cmdPaletteFiltered.length === 0) {
        listEl.innerHTML = `<div style="padding: 28px; text-align: center; color: var(--text-dim); font-style: italic;">No matching commands, servers, bookmarks, or files.</div>`;
        return;
      }

      listEl.innerHTML = cmdPaletteFiltered.map((it, idx) => {
        const isSel = idx === cmdPaletteSelectedIndex;
        return `
          <div onclick="executeCommandPaletteItem(${idx})" onmouseenter="cmdPaletteSelectedIndex=${idx}; renderCommandPaletteItems();" style="display: flex; align-items: center; gap: 10px; padding: 7px 10px; border-radius: 5px; cursor: pointer; background: ${isSel ? 'rgba(56, 189, 248, 0.16)' : 'transparent'}; border: 1px solid ${isSel ? 'rgba(56, 189, 248, 0.35)' : 'transparent'};">
            <span style="font-size: 14px; width: 20px; text-align: center;">${it.icon}</span>
            <span style="font-size: 10px; font-weight: 700; text-transform: uppercase; padding: 1px 6px; border-radius: 4px; background: rgba(255,255,255,0.06); color: #38bdf8;">${escapeHtml(it.category)}</span>
            <span style="flex: 1; font-size: 12px; font-weight: ${isSel ? '600' : '500'}; color: var(--text-main); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(it.title)}</span>
            ${it.shortcut ? `<span style="font-family: monospace; font-size: 10.5px; color: var(--text-dim); max-width: 190px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(it.shortcut)}</span>` : ''}
          </div>
        `;
      }).join('');
    }

    function handleCommandPaletteKey(e) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        if (cmdPaletteFiltered.length > 0) {
          cmdPaletteSelectedIndex = (cmdPaletteSelectedIndex + 1) % cmdPaletteFiltered.length;
          renderCommandPaletteItems();
        }
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        if (cmdPaletteFiltered.length > 0) {
          cmdPaletteSelectedIndex = (cmdPaletteSelectedIndex - 1 + cmdPaletteFiltered.length) % cmdPaletteFiltered.length;
          renderCommandPaletteItems();
        }
      } else if (e.key === 'Enter') {
        e.preventDefault();
        executeCommandPaletteItem(cmdPaletteSelectedIndex);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        closeCommandPalette();
      }
    }

    function executeCommandPaletteItem(idx) {
      const item = cmdPaletteFiltered[idx];
      if (!item) return;
      closeCommandPalette();
      item.run();
    }

    function copyAllLogs() {
      closeAllMenus();
      const logBox = document.getElementById('logContainer');
      if (!logBox) return;
      const text = logBox.innerText || '';
      navigator.clipboard.writeText(text).then(() => {
        addLog('All logs copied to clipboard', 'success');
      }).catch(() => {
        addLog('Failed to copy logs to clipboard', 'error');
      });
    }

    function openStorageFolder() {
      closeAllMenus();
      try {
        sendRust('open_storage_folder', {});
        addLog('Opened configuration folder %APPDATA%\\90FTP', 'info');
      } catch (err) {
        addLog('Storage folder located at %APPDATA%\\90FTP', 'info');
      }
    }

    let lastActivePaneIsRemote = false;

    function setActivePaneFocus(isRemote) {
      lastActivePaneIsRemote = !!isRemote;
      const leftWrapper = document.querySelector('#leftPane .file-pane-wrapper');
      const rightWrapper = document.querySelector('#rightPane .file-pane-wrapper');
      if (leftWrapper) leftWrapper.classList.toggle('pane-focused', !lastActivePaneIsRemote);
      if (rightWrapper) rightWrapper.classList.toggle('pane-focused', lastActivePaneIsRemote);
    }

    document.addEventListener('mousedown', (e) => {
      if (e.target.closest('#rightPane')) setActivePaneFocus(true);
      else if (e.target.closest('#leftPane')) setActivePaneFocus(false);
    });

    function triggerCommanderFkey(key) {
      const isRem = lastActivePaneIsRemote;
      if (key === 'F2') {
        const item = isRem ? (getSelectedRemoteItems()[0] || selectedRemoteFile) : (getSelectedLocalItems()[0] || selectedLocalFile);
        if (!item) {
          alert(`Please select a ${isRem ? 'remote' : 'local'} item to rename (F2).`);
          return;
        }
        promptRename(item.path, item.name, isRem);
      } else if (key === 'F4') {
        const item = isRem ? (getSelectedRemoteItems()[0] || selectedRemoteFile) : (getSelectedLocalItems()[0] || selectedLocalFile);
        if (!item || item.is_dir) {
          alert(`Please select a ${isRem ? 'remote' : 'local'} file to edit in Code Editor (F4).`);
          return;
        }
        if (isRem) openRemoteFileInEditor(item.path);
        else openLocalFileInEditor(item.path);
      } else if (key === 'F5') {
        if (isRem) transferDownload();
        else transferUpload();
      } else if (key === 'F6') {
        openInteractiveMoveModal(isRem, 'move');
      } else if (key === 'F7') {
        if (isRem) {
          if (!isConnected) { alert('Please connect to a remote server first to create a remote folder (F7).'); return; }
          promptNewRemoteFolder();
        } else {
          promptNewLocalFolder();
        }
      } else if (key === 'F8') {
        const remoteSel = getSelectedRemoteItems();
        const localSel = getSelectedLocalItems();
        if (isRem && remoteSel.length > 0) deleteSelectedItems(true);
        else if (!isRem && localSel.length > 0) deleteSelectedItems(false);
        else if (remoteSel.length > 0) deleteSelectedItems(true);
        else if (localSel.length > 0) deleteSelectedItems(false);
        else alert('Please select at least one item to delete.');
      }
    }

    function navigateFileRowByKeyboard(delta, toBoundary = null) {
      const isRem = lastActivePaneIsRemote;
      const tbody = document.getElementById(isRem ? 'remoteFileList' : 'localFileList');
      if (!tbody) return;
      const rows = Array.from(tbody.querySelectorAll('tr[data-name]'));
      if (rows.length === 0) return;
      let curIdx = rows.findIndex(r => r.classList.contains('selected'));
      let nextIdx = 0;
      if (toBoundary === 'home') nextIdx = 0;
      else if (toBoundary === 'end') nextIdx = rows.length - 1;
      else if (curIdx === -1) nextIdx = delta > 0 ? 0 : rows.length - 1;
      else nextIdx = Math.max(0, Math.min(rows.length - 1, curIdx + delta));

      rows.forEach(r => r.classList.remove('selected'));
      const targetRow = rows[nextIdx];
      if (!targetRow) return;
      targetRow.classList.add('selected');
      targetRow.scrollIntoView({ block: 'nearest' });

      const name = targetRow.dataset.name;
      const list = isRem ? remoteFiles : localFiles;
      const item = list.find(f => f.name === name);
      if (isRem) {
        selectedRemoteFile = item || null;
        lastSelectedRemoteIndex = nextIdx;
      } else {
        selectedLocalFile = item || null;
        lastSelectedLocalIndex = nextIdx;
      }
      updatePaneStatsBadge(isRem);
    }

    function activateSelectedRowByEnter() {
      const isRem = lastActivePaneIsRemote;
      const tbody = document.getElementById(isRem ? 'remoteFileList' : 'localFileList');
      if (!tbody) return;
      const selRow = tbody.querySelector('tr.selected');
      if (!selRow) return;
      const name = selRow.dataset.name;
      if (!name) {
        // Parent ".." row
        if (isRem) remoteNavigateUp();
        else localNavigateUp();
        return;
      }
      const list = isRem ? remoteFiles : localFiles;
      const item = list.find(f => f.name === name);
      if (!item) return;
      if (item.is_dir) {
        if (isRem) navigateRemote(item.path);
        else navigateLocal(item.path);
      } else {
        if (isRem) openRemoteFileInEditor(item.path);
        else openLocalFileInEditor(item.path);
      }
    }

    // Global Keyboard Shortcuts
    document.addEventListener('keydown', (e) => {
      const isInput = ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName);
      if (e.key === 'Escape') {
        // Inside a tool page, Escape steps back to the grid instead of
        // dismissing everything — but only when no tool dialog is on top of it.
        if (toolsDetailIsOpen() && toolsIsTopModal()) {
          backToTools();
          return;
        }
        closeAllMenus();
        closeContextMenu();
        closeOptionsModal();
        closeAboutModal();
        closeUpdateModal();
        closeSitesModal();
        closeEditorModal();
        closeExclusionMaskModal();
        closeFindFilesModal();
        closeChecksumModal();
        closeInteractiveMoveModal();
        closeNetworkProbeModal();
        closeSshKeygenModal();
        closeToolsModal();
        closeServerHealthModal();
        closeFileDiffModal();
        closeCommandPalette();
        closeContentGrepModal();
        closeBatchRenameModal();
        closeBookmarksModal();
        closeProcessManagerModal();
        closeFileInspectorModal();
        return;
      }

      if ((e.ctrlKey || e.metaKey) && (e.key.toLowerCase() === 'k' || e.key.toLowerCase() === 'p')) {
        e.preventDefault();
        openCommandPalette();
        return;
      }

      if (isInput) return;

      if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === 'f') {
        e.preventDefault();
        openContentGrepModal();
        return;
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'm') {
        e.preventDefault();
        openBatchRenameModal();
        return;
      } else if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === 'y') {
        e.preventDefault();
        toggleSynchronizedBrowsing();
        return;
      } else if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === 'y') {
        e.preventDefault();
        openSyncModal();
        return;
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'b') {
        e.preventDefault();
        openBookmarksModal();
        return;
      }

      const anyModalOpen = !!document.querySelector('.modal-overlay.active');

      if (!anyModalOpen && e.key === 'Tab') {
        e.preventDefault();
        setActivePaneFocus(!lastActivePaneIsRemote);
        return;
      }

      if (!anyModalOpen && (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Home' || e.key === 'End')) {
        if (!e.altKey && !e.ctrlKey && !e.metaKey) {
          e.preventDefault();
          if (e.key === 'ArrowDown') navigateFileRowByKeyboard(1);
          else if (e.key === 'ArrowUp') navigateFileRowByKeyboard(-1);
          else if (e.key === 'Home') navigateFileRowByKeyboard(0, 'home');
          else if (e.key === 'End') navigateFileRowByKeyboard(0, 'end');
          return;
        }
      }

      if (!anyModalOpen && e.key === 'Enter') {
        e.preventDefault();
        activateSelectedRowByEnter();
        return;
      }

      if (!anyModalOpen && e.key === 'Backspace') {
        e.preventDefault();
        if (lastActivePaneIsRemote) remoteNavigateUp();
        else localNavigateUp();
        return;
      }

      if (!anyModalOpen && e.key === 'F2') {
        e.preventDefault();
        triggerCommanderFkey('F2');
        return;
      } else if (!anyModalOpen && e.key === 'F4') {
        e.preventDefault();
        triggerCommanderFkey('F4');
        return;
      } else if (!anyModalOpen && e.key === 'F7') {
        e.preventDefault();
        triggerCommanderFkey('F7');
        return;
      } else if (!anyModalOpen && e.key === 'F8') {
        e.preventDefault();
        triggerCommanderFkey('F8');
        return;
      }

      if (e.shiftKey && e.key === 'F1') {
        e.preventDefault();
        toggleCompareDirectories();
      } else if (e.key === 'Delete') {
        if (!anyModalOpen) {
          e.preventDefault();
          triggerCommanderFkey('F8');
        }
      } else if (e.key === 'F6') {
        e.preventDefault();
        triggerCommanderFkey('F6');
      } else if (e.altKey && e.key === 'ArrowUp') {
        e.preventDefault();
        const useRemote = lastActivePaneIsRemote ? (getSelectedRemoteItems().length > 0) : (getSelectedLocalItems().length === 0 && getSelectedRemoteItems().length > 0);
        if ((useRemote && getSelectedRemoteItems().length > 0) || (!useRemote && getSelectedLocalItems().length > 0)) {
          moveSelectedToParent(useRemote);
        } else if (lastActivePaneIsRemote) {
          remoteNavigateUp();
        } else {
          localNavigateUp();
        }
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'i') {
        e.preventDefault();
        const filterInp = document.getElementById(lastActivePaneIsRemote ? 'remoteQuickFilter' : 'localQuickFilter');
        if (filterInp) {
          filterInp.focus();
          filterInp.select();
        }
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') {
        e.preventDefault();
        selectAllItemsInPane(lastActivePaneIsRemote);
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'x') {
        e.preventDefault();
        clipboardCutSelected(lastActivePaneIsRemote);
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c') {
        const selText = window.getSelection()?.toString();
        if (!selText) {
          e.preventDefault();
          clipboardCopySelected(lastActivePaneIsRemote);
        }
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'v') {
        e.preventDefault();
        clipboardPasteHere(lastActivePaneIsRemote);
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'z') {
        if (lastInteractiveMoveUndo) {
          e.preventDefault();
          undoLastInteractiveMove();
        }
      } else if (e.key === 'F3' || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f')) {
        e.preventDefault();
        openFindFilesModal();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'u') {
        e.preventDefault();
        openUpdateModal(true);
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
        e.preventDefault();
        openSitesModal();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'q') {
        e.preventDefault();
        focusQuickConnect();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'd') {
        e.preventDefault();
        disconnect();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'r') {
        e.preventDefault();
        handleReconnect(true);
      } else if ((e.ctrlKey || e.metaKey) && e.key === ',') {
        e.preventDefault();
        openOptionsModal('general');
      } else if (e.key === 'F5') {
        e.preventDefault();
        refreshBothPanes();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'h') {
        e.preventDefault();
        toggleHiddenFiles();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'l') {
        e.preventDefault();
        toggleLogPanel();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 't') {
        e.preventDefault();
        toggleTheme();
      }
    });

    // ── Two-Way Folder Sync (dry-run preview, then apply) ───────────────────
    //
    // The backend recomputes the plan inside `sync_apply` instead of trusting
    // what the preview returned, so a stale or edited table can never move a
    // file the user did not see. The UI therefore treats the preview purely as
    // information, and Apply re-sends the same options.

    let lastSyncPlan = null;
    let isSyncBusy = false;

    function openSyncModal() {
      closeAllMenus();
      if (!isConnected) {
        showInteractiveActionToast('Not connected', 'Connect to a server before syncing folders.', '', false, false);
        return;
      }

      const localInput = document.getElementById('syncLocalDir');
      const remoteInput = document.getElementById('syncRemoteDir');
      if (localInput && !localInput.value) localInput.value = currentLocalPath || '';
      if (remoteInput && !remoteInput.value) remoteInput.value = currentRemotePath || '';

      resetSyncResults();
      const modal = document.getElementById('syncPlanModal');
      if (modal) modal.classList.add('active');
    }

    function closeSyncModal() {
      if (isSyncBusy) {
        if (!confirm('A sync is still running. Close the window anyway?')) return;
      }
      const modal = document.getElementById('syncPlanModal');
      if (modal) modal.classList.remove('active');
    }

    function fillSyncLocalFromPane() {
      const el = document.getElementById('syncLocalDir');
      if (el) el.value = currentLocalPath || '';
      invalidateSyncPreview();
    }

    function fillSyncRemoteFromPane() {
      const el = document.getElementById('syncRemoteDir');
      if (el) el.value = currentRemotePath || '';
      invalidateSyncPreview();
    }

    // Any change to the inputs makes a previously shown plan a lie, so the
    // Apply button is disabled until the user previews again.
    function invalidateSyncPreview() {
      lastSyncPlan = null;
      const applyBtn = document.getElementById('btnSyncApply');
      if (applyBtn) applyBtn.disabled = true;
      const status = document.getElementById('syncFooterStatus');
      if (status) status.textContent = 'Options changed — preview again before applying.';
    }

    function resetSyncResults() {
      lastSyncPlan = null;
      const bar = document.getElementById('syncSummaryBar');
      if (bar) { bar.style.display = 'none'; bar.innerHTML = ''; }
      const wrap = document.getElementById('syncResultsWrap');
      if (wrap) wrap.style.display = 'none';
      const body = document.getElementById('syncResultsBody');
      if (body) body.innerHTML = '';
      const applyBtn = document.getElementById('btnSyncApply');
      if (applyBtn) applyBtn.disabled = true;
      const status = document.getElementById('syncFooterStatus');
      if (status) status.textContent = 'Preview first — nothing is written until you press Apply.';
    }

    function syncRequestPayload() {
      const localEl = document.getElementById('syncLocalDir');
      const remoteEl = document.getElementById('syncRemoteDir');
      return {
        local_dir: (localEl ? localEl.value : '').trim(),
        remote_dir: (remoteEl ? remoteEl.value : '').trim(),
        direction: (document.getElementById('syncDirection') || {}).value || 'both',
        delete_extraneous: !!(document.getElementById('syncDeleteExtraneous') || {}).checked,
        size_only: !!(document.getElementById('syncSizeOnly') || {}).checked,
        exclude: (document.getElementById('syncExclude') || {}).value || ''
      };
    }

    function formatSyncBytes(n) {
      if (n === null || n === undefined) return '—';
      if (n < 1024) return n + ' B';
      const units = ['KB', 'MB', 'GB', 'TB'];
      let value = n / 1024;
      let i = 0;
      while (value >= 1024 && i < units.length - 1) { value /= 1024; i++; }
      return value.toFixed(value >= 10 ? 0 : 1) + ' ' + units[i];
    }

    // Colours come from CSS variables so light and dark themes stay legible —
    // the dark-theme hex values are unreadable on a white surface.
    const SYNC_ACTION_LABELS = {
      upload_new:      { label: 'Upload new',      color: 'var(--sync-transfer)' },
      upload_update:   { label: 'Upload update',   color: 'var(--sync-update)' },
      download_new:    { label: 'Download new',    color: 'var(--sync-transfer)' },
      download_update: { label: 'Download update', color: 'var(--sync-update)' },
      delete_local:    { label: 'Delete local',    color: 'var(--sync-delete)' },
      delete_remote:   { label: 'Delete remote',   color: 'var(--sync-delete)' },
      conflict:        { label: 'Conflict',        color: 'var(--sync-conflict)' },
      unchanged:       { label: 'Unchanged',       color: 'var(--sync-muted)' }
    };

    function renderSyncPlan(data) {
      lastSyncPlan = data;

      const entries = Array.isArray(data.entries) ? data.entries : [];
      const summary = data.summary || {};
      const effective = entries.filter(e => e.effective);

      const bar = document.getElementById('syncSummaryBar');
      if (bar) {
        const chips = Object.keys(summary)
          .filter(action => summary[action] > 0)
          .map(action => {
            const meta = SYNC_ACTION_LABELS[action] || { label: action, color: 'var(--text-muted)' };
            return '<span style="display: inline-flex; align-items: center; gap: 5px; padding: 2px 8px; border-radius: 999px; background: rgba(148, 163, 184, 0.12); border: 1px solid var(--border-color);">'
              + '<span style="width: 7px; height: 7px; border-radius: 50%; background: ' + meta.color + ';"></span>'
              + '<span style="color: var(--text-main);">' + meta.label + '</span>'
              + '<strong style="color: ' + meta.color + ';">' + summary[action] + '</strong>'
              + '</span>';
          });
        chips.unshift('<span style="color: var(--text-muted);">' + data.local_count + ' local file(s) &middot; ' + data.remote_count + ' remote file(s)</span>');
        bar.innerHTML = chips.join('');
        bar.style.display = 'flex';
      }

      const body = document.getElementById('syncResultsBody');
      const wrap = document.getElementById('syncResultsWrap');
      if (body) {
        if (entries.length === 0) {
          body.innerHTML = '<tr><td colspan="4" style="padding: 18px; text-align: center; color: var(--text-muted);">Both folders are already in sync.</td></tr>';
        } else {
          // Effective actions first: a long "unchanged" tail must not bury the
          // handful of rows the user actually has to review.
          const ordered = entries.slice().sort((a, b) => (b.effective ? 1 : 0) - (a.effective ? 1 : 0));
          body.innerHTML = ordered.map(e => {
            const meta = SYNC_ACTION_LABELS[e.action] || { label: e.action, color: 'var(--text-muted)' };
            const dim = e.effective ? '' : 'opacity: 0.45;';
            return '<tr style="' + dim + ' border-bottom: 1px solid rgba(148, 163, 184, 0.10);">'
              + '<td style="padding: 5px 10px; font-family: var(--font-mono); word-break: break-all; color: var(--text-main);">' + escapeHtml(e.rel_path) + '</td>'
              + '<td style="padding: 5px 10px; white-space: nowrap; color: ' + meta.color + '; font-weight: 600;">' + meta.label + '</td>'
              + '<td style="padding: 5px 10px; text-align: right; white-space: nowrap; color: var(--text-muted); font-family: var(--font-mono);">' + formatSyncBytes(e.local_size) + '</td>'
              + '<td style="padding: 5px 10px; text-align: right; white-space: nowrap; color: var(--text-muted); font-family: var(--font-mono);">' + formatSyncBytes(e.remote_size) + '</td>'
              + '</tr>';
          }).join('');
        }
        if (wrap) wrap.style.display = 'block';
      }

      const conflicts = summary.conflict || 0;
      const deletions = (summary.delete_local || 0) + (summary.delete_remote || 0);
      const applyBtn = document.getElementById('btnSyncApply');
      const status = document.getElementById('syncFooterStatus');

      if (applyBtn) applyBtn.disabled = effective.length === 0;
      if (status) {
        if (effective.length === 0) {
          status.textContent = 'Nothing to do — the folders already match.';
        } else if (conflicts > 0) {
          status.textContent = effective.length + ' action(s) ready · ' + conflicts + ' conflict(s) will be skipped. Narrow the direction to resolve them.';
        } else if (deletions > 0) {
          status.textContent = effective.length + ' action(s) ready, including ' + deletions + ' deletion(s). Review the red rows before applying.';
        } else {
          status.textContent = effective.length + ' action(s) ready.';
        }
      }
    }

    async function previewSync() {
      if (isSyncBusy) return;
      const payload = syncRequestPayload();
      if (!payload.local_dir || !payload.remote_dir) {
        showInteractiveActionToast('Missing folders', 'Choose both a local and a remote folder.', '', false, false);
        return;
      }

      const previewBtn = document.getElementById('btnSyncPreview');
      const status = document.getElementById('syncFooterStatus');
      isSyncBusy = true;
      if (previewBtn) previewBtn.disabled = true;
      if (status) status.textContent = 'Scanning both folders…';

      try {
        const res = await sendRust('sync_plan', payload);
        renderSyncPlan(res);
        addLog('Sync preview: ' + res.local_count + ' local / ' + res.remote_count + ' remote file(s)', 'info');
      } catch (err) {
        resetSyncResults();
        if (status) status.textContent = 'Preview failed: ' + err.message;
        showInteractiveActionToast('Preview failed', err.message, '', false, false);
      } finally {
        isSyncBusy = false;
        if (previewBtn) previewBtn.disabled = false;
      }
    }

    async function applySync() {
      if (isSyncBusy) return;
      const payload = syncRequestPayload();
      if (!payload.local_dir || !payload.remote_dir) {
        showInteractiveActionToast('Missing folders', 'Choose both a local and a remote folder.', '', false, false);
        return;
      }

      const plan = lastSyncPlan;
      const effective = plan ? plan.entries.filter(e => e.effective) : [];
      const deletions = effective.filter(e => e.action === 'delete_local' || e.action === 'delete_remote').length;

      const warning = deletions > 0
        ? 'This will run ' + effective.length + ' action(s), including ' + deletions + ' deletion(s). Deleted files cannot be recovered. Continue?'
        : 'Run ' + effective.length + ' action(s) now?';
      if (!confirm(warning)) return;

      const applyBtn = document.getElementById('btnSyncApply');
      const previewBtn = document.getElementById('btnSyncPreview');
      const status = document.getElementById('syncFooterStatus');
      isSyncBusy = true;
      if (applyBtn) applyBtn.disabled = true;
      if (previewBtn) previewBtn.disabled = true;
      if (status) status.textContent = 'Applying… this can take a while for large folders.';

      try {
        const res = await sendRust('sync_apply', payload);
        const failed = res.failed_count || 0;
        const applied = res.applied_count || 0;

        addLog('Sync applied: ' + applied + ' succeeded, ' + failed + ' failed', failed > 0 ? 'error' : 'success');
        if (failed > 0 && Array.isArray(res.errors)) {
          res.errors.slice(0, 10).forEach(msg => addLog('  ' + msg, 'error'));
        }

        showInteractiveActionToast(
          failed > 0 ? 'Sync finished with errors' : 'Sync complete',
          applied + ' applied' + (failed > 0 ? ', ' + failed + ' failed — see the activity log' : ''),
          payload.remote_dir,
          true,
          false
        );

        // Re-preview so the table reflects reality rather than the stale plan.
        isSyncBusy = false;
        if (previewBtn) previewBtn.disabled = false;
        await previewSync();
      } catch (err) {
        if (status) status.textContent = 'Apply failed: ' + err.message;
        showInteractiveActionToast('Sync failed', err.message, '', false, false);
        addLog('Sync failed: ' + err.message, 'error');
      } finally {
        isSyncBusy = false;
        if (previewBtn) previewBtn.disabled = false;
        if (applyBtn) applyBtn.disabled = !lastSyncPlan || effective.length === 0;
      }
    }

    // Launch
    let hasInitialized = false;
    async function launchApp() {
      if (hasInitialized) return;
      hasInitialized = true;
      try {
        await initApp();
      } catch (err) {
        console.error('Fatal initialization error:', err);
      }
    }

    // Auto-refresh directories on window focus if enabled in Connection & Sync options
    window.addEventListener('focus', () => {
      if (appOptions && appOptions.autoRefreshOnFocus) {
        if (currentLocalPath) navigateLocal(currentLocalPath);
        if (isConnected && currentRemotePath) navigateRemote(currentRemotePath);
      }
    });

    // Launch immediately and attach lifecycle fallbacks
    launchApp();
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', launchApp);
    }
    window.addEventListener('load', launchApp);
