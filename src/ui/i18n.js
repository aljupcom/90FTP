/* ==========================================================================
   90FTP i18n runtime
   --------------------------------------------------------------------------
   Design contract
   --------------
   * `src/ui/i18n.en.js` and `src/ui/i18n.ar.js` are pure data files. The
     English file is the key registry; the Arabic file is the only place in
     the repository where Arabic characters are allowed.
   * `t()` always returns raw text. It never escapes and never emits markup.
     Escaping happens at the sink (`escapeHtml`, `textContent`, `addLog`).
   * English is the default language and the fallback for any missing key.
   * Static markup is translated without touching the HTML: on every sweep the
     runtime promotes exact English text nodes and English `title` /
     `placeholder` / `aria-label` values into `data-i18n*` attributes, then
     resolves those attributes. The English literal therefore stays in the
     file as the on-disk source of truth, and switching back to English always
     works because the key is remembered on the element itself.
   ========================================================================== */

(function (global) {
  'use strict';

  var DICTS = {
    en: global.I18N_EN || {},
    ar: global.I18N_AR || {}
  };

  var SUPPORTED = ['en', 'ar'];
  var RTL_LANGS = { ar: true };
  var STORAGE_KEY = '90ftp_lang';

  var current = 'en';
  var warned = Object.create(null);
  var EN_TO_KEY = null;
  var reverseEnabled = true;

  /* Subtrees that must never be translated: machine output, terminals, tool
     descriptions, diagnostics detail and release notes. User data (file names,
     transfer queue rows, site names) is marked with data-i18n-skip at the point
     where it is rendered, because the surrounding cells still need translating. */
  var SKIP_SELECTOR = [
    '[data-i18n-skip]',
    '#logBody',
    '#toolsBody',
    '#terminalContainer',
    '#puttyXtermContainer',
    '#appDialogDetailBox',
    '#changelogContent',
    '#errorLogsBody',
    /* Transfer queue rows carry file names; the empty-state row does not and
       still needs translating. */
    '#transfersBodyContainer tbody tr:not(.transfers-empty-row)',
    '.xterm'
  ].join(', ');

  /* Elements whose text is written by the renderer and must not be promoted
     into a static key (the renderer re-applies the correct value afterwards). */
  var NO_PROMOTE_SELECTOR = '#statusText, #connStateLabel, #paneLocalStats, #paneRemoteStats';

  /* ------------------------------------------------------------------ */
  /* Language                                                            */
  /* ------------------------------------------------------------------ */

  function normalizeLang(lang) {
    var v = String(lang == null ? '' : lang).trim().toLowerCase();
    if (v === 'ar' || v === 'arabic' || v === 'ar-sa' || v === 'ar_sa') return 'ar';
    return 'en';
  }

  function setLanguage(lang) {
    current = normalizeLang(lang);
    var html = document.documentElement;
    if (html) {
      html.setAttribute('lang', current);
      html.setAttribute('dir', RTL_LANGS[current] ? 'rtl' : 'ltr');
      html.setAttribute('data-lang', current);
    }
    return current;
  }

  function getLanguage() { return current; }
  function isRtl() { return !!RTL_LANGS[current]; }

  /* ------------------------------------------------------------------ */
  /* Lookup                                                              */
  /* ------------------------------------------------------------------ */

  function warnMissing(key) {
    if (warned[key]) return;
    warned[key] = true;
    if (global.__I18N_STRICT__ === false) return;
    try {
      console.warn('[i18n] missing key: ' + key + ' (lang=' + current + ')');
    } catch (_) {}
  }

  function interpolate(text, params) {
    if (!params) return text;
    return String(text).replace(/\{(\w+)\}/g, function (match, name) {
      return Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : match;
    });
  }

  function t(key, params) {
    if (!key) return '';
    var dict = DICTS[current] || {};
    var value = dict[key];
    if (value === undefined) {
      var fallback = DICTS.en[key];
      warnMissing(key);
      if (fallback === undefined) return String(key);
      value = fallback;
    }
    return params ? interpolate(value, params) : value;
  }

  function has(key) {
    return Object.prototype.hasOwnProperty.call(DICTS[current] || {}, key) ||
           Object.prototype.hasOwnProperty.call(DICTS.en, key);
  }

  /* Keyword patterns used by the dialog severity heuristics live in the
     dictionaries (`re.*`), so the Arabic alternations stay inside the Arabic
     file instead of leaking into app.js. */
  var regexCache = Object.create(null);

  function keywordRegex(group) {
    var key = 're.' + group;
    var dict = DICTS[current] || {};
    var pattern = dict[key] !== undefined ? dict[key] : DICTS.en[key];
    if (!pattern) return null;
    var cacheKey = current + '|' + group;
    if (regexCache[cacheKey] === undefined) {
      try {
        regexCache[cacheKey] = new RegExp(pattern, 'i');
      } catch (_) {
        regexCache[cacheKey] = null;
      }
    }
    return regexCache[cacheKey];
  }

  function matches(group, text) {
    var re = keywordRegex(group);
    if (!re) return false;
    return re.test(String(text == null ? '' : text));
  }

  /* Arabic has six CLDR plural categories; English has two. */
  function pluralForm(n) {
    var abs = Math.abs(Number(n) || 0);
    if (current === 'ar') {
      if (abs === 0) return 'zero';
      if (abs === 1) return 'one';
      if (abs === 2) return 'two';
      var m = abs % 100;
      if (m >= 3 && m <= 10) return 'few';
      if (m >= 11 && m <= 99) return 'many';
      return 'other';
    }
    return abs === 1 ? 'one' : 'other';
  }

  function tn(base, n, params) {
    var form = pluralForm(n);
    var dict = DICTS[current] || {};
    var key = base + '.' + form;
    if (dict[key] === undefined && DICTS.en[key] === undefined) key = base + '.other';
    var merged = { n: n };
    if (params) {
      for (var k in params) {
        if (Object.prototype.hasOwnProperty.call(params, k)) merged[k] = params[k];
      }
    }
    return t(key, merged);
  }

  /* ------------------------------------------------------------------ */
  /* Reverse index: English text -> key                                  */
  /* ------------------------------------------------------------------ */

  function buildReverseIndex() {
    if (EN_TO_KEY) return EN_TO_KEY;
    EN_TO_KEY = new Map();
    var en = DICTS.en;
    for (var key in en) {
      if (!Object.prototype.hasOwnProperty.call(en, key)) continue;
      if (key.indexOf('re.') === 0) continue; // keyword patterns, not UI text
      var value = en[key];
      if (typeof value !== 'string') continue;
      var phrase = value.trim();
      if (!phrase) continue;
      if (!EN_TO_KEY.has(phrase)) EN_TO_KEY.set(phrase, key);
    }
    return EN_TO_KEY;
  }

  /* Markup wraps long labels across source lines, so comparisons must ignore
     internal runs of whitespace while replacements keep the outer spacing. */
  function normalizePhrase(value) {
    return String(value == null ? '' : value).replace(/\s+/g, ' ').trim();
  }

  function keyForPhrase(phrase) {
    if (!phrase) return null;
    return buildReverseIndex().get(normalizePhrase(phrase)) || null;
  }

  /* ------------------------------------------------------------------ */
  /* Sweep                                                               */
  /* ------------------------------------------------------------------ */

  function isSkipped(el) {
    if (!el || el.nodeType !== 1) return true;
    if (!el.closest) return false;
    return !!el.closest(SKIP_SELECTOR);
  }

  function promoteAttributes(root) {
    if (!reverseEnabled) return;
    var idx = buildReverseIndex();
    var nodes = root.querySelectorAll('[title], [placeholder], [aria-label]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (isSkipped(el)) continue;
      if (el.closest(NO_PROMOTE_SELECTOR)) continue;

      if (!el.hasAttribute('data-i18n-title')) {
        var titleKey = idx.get(normalizePhrase(el.getAttribute('title')));
        if (titleKey) el.setAttribute('data-i18n-title', titleKey);
      }
      if (!el.hasAttribute('data-i18n-placeholder')) {
        var phKey = idx.get(normalizePhrase(el.getAttribute('placeholder')));
        if (phKey) el.setAttribute('data-i18n-placeholder', phKey);
      }
      if (!el.hasAttribute('data-i18n-aria-label')) {
        var ariaKey = idx.get(normalizePhrase(el.getAttribute('aria-label')));
        if (ariaKey) el.setAttribute('data-i18n-aria-label', ariaKey);
      }
    }
  }

  function promoteTextNodes(root) {
    if (!reverseEnabled) return;
    var idx = buildReverseIndex();
    var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
    var pending = [];
    var node;
    while ((node = walker.nextNode())) {
      var raw = node.nodeValue;
      if (!raw) continue;
      var phrase = normalizePhrase(raw);
      if (!phrase || phrase.length > 220) continue;
      var parent = node.parentElement;
      if (!parent) continue;
      if (parent.hasAttribute('data-i18n')) continue;
      var tag = parent.tagName;
      if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'TEXTAREA') continue;
      if (isSkipped(parent)) continue;
      if (parent.closest(NO_PROMOTE_SELECTOR)) continue;
      /* Only promote when this is the element's sole piece of text. An element
         like `<th>Name <span class="sort-indicator">▲</span></th>` qualifies:
         the label is the only text, the span is decorative, and the swap below
         rewrites just the label. A label split across two text nodes
         (`<span>Hello <b>world</b> again</span>`) still fails this test, so
         multi-fragment labels are never half-translated. */
      var textNodeCount = 0;
      for (var c = 0; c < parent.childNodes.length; c++) {
        var child = parent.childNodes[c];
        if (child.nodeType === 3 && child.nodeValue && child.nodeValue.trim()) textNodeCount++;
      }
      if (textNodeCount !== 1) continue;
      var key = idx.get(phrase);
      if (key) pending.push([parent, key]);
    }
    for (var i = 0; i < pending.length; i++) {
      pending[i][0].setAttribute('data-i18n', pending[i][1]);
    }
  }

  function applyElementText(el) {
    var key = el.getAttribute('data-i18n');
    if (!key) return;
    var text = t(key);
    var kids = el.childNodes;
    for (var i = 0; i < kids.length; i++) {
      var node = kids[i];
      if (node.nodeType !== 3) continue;
      var raw = node.nodeValue;
      if (!raw || !raw.trim()) continue;
      if (normalizePhrase(raw) === text) return;
      var lead = raw.match(/^\s*/)[0];
      var trail = raw.match(/\s*$/)[0];
      node.nodeValue = lead + text + trail;
      return;
    }
    el.insertBefore(document.createTextNode(text), el.firstChild);
  }

  function applyExplicit(root) {
    var els = root.querySelectorAll('[data-i18n]');
    for (var i = 0; i < els.length; i++) {
      if (isSkipped(els[i])) continue;
      applyElementText(els[i]);
    }

    var titled = root.querySelectorAll('[data-i18n-title]');
    for (var j = 0; j < titled.length; j++) {
      var tEl = titled[j];
      if (isSkipped(tEl)) continue;
      tEl.setAttribute('title', t(tEl.getAttribute('data-i18n-title')));
    }

    var placed = root.querySelectorAll('[data-i18n-placeholder]');
    for (var k = 0; k < placed.length; k++) {
      var pEl = placed[k];
      if (isSkipped(pEl)) continue;
      pEl.setAttribute('placeholder', t(pEl.getAttribute('data-i18n-placeholder')));
    }

    var aria = root.querySelectorAll('[data-i18n-aria-label]');
    for (var m = 0; m < aria.length; m++) {
      var aEl = aria[m];
      if (isSkipped(aEl)) continue;
      aEl.setAttribute('aria-label', t(aEl.getAttribute('data-i18n-aria-label')));
    }
  }

  function applyTranslations(root) {
    var scope = root || document;
    if (!scope.querySelectorAll) return 0;
    promoteAttributes(scope);
    promoteTextNodes(scope);
    applyExplicit(scope);
    return 1;
  }

  /* ------------------------------------------------------------------ */
  /* Live re-sweep                                                       */
  /* ------------------------------------------------------------------ */
  /* Most of the interface is built at runtime: renderers assign innerHTML from
     template literals that carry English source text, and a one-shot sweep at
     boot only ever sees the skeleton. Without this observer every label a
     renderer rebuilds — the site picker's "Manual Input...", the file table's
     type column, tab titles — reverts to English on the next navigation.

     The sweep runs inside the mutation callback rather than on an animation
     frame, so it finishes before the browser paints: the user never sees the
     English literal appear and then flip to Arabic. Mutation records arrive in
     batches, so one render costs one pass. applyElementText() is a no-op when
     the text already matches, which is what makes the characterData
     notification that the sweep itself generates settle immediately. */

  var observer = null;
  var sweeping = false;
  var MAX_ROOTS = 40;

  function sweepRoots(roots) {
    if (sweeping) return; // re-entrant call from our own writes; already covered
    sweeping = true;
    try {
      for (var i = 0; i < roots.length; i++) {
        var root = roots[i];
        if (!root || !root.isConnected) continue;
        promoteAttributes(root);
        promoteTextNodes(root);
        applyExplicit(root);
      }
    } catch (_) {
      /* A detached or half-built subtree must never break the renderer that
         produced it. */
    } finally {
      sweeping = false;
    }
  }

  function startObserver() {
    if (observer || typeof MutationObserver === 'undefined') return;
    if (!document.body) return;
    observer = new MutationObserver(function (records) {
      /* In English every renderer already emits the right text, so sweeping
         would only burn CPU on large tables. Switching language re-sweeps the
         whole document, which is where the promotion for existing markup
         happens. */
      if (current === 'en') return;
      var roots = [];
      for (var i = 0; i < records.length; i++) {
        var rec = records[i];
        if (rec.type === 'characterData') {
          var parentEl = rec.target.parentElement;
          if (parentEl && !isSkipped(parentEl)) roots.push(parentEl);
          continue;
        }
        var added = rec.addedNodes;
        for (var j = 0; j < added.length; j++) {
          var node = added[j];
          if (node.nodeType === 1) {
            if (!isSkipped(node)) roots.push(node);
          } else if (node.nodeType === 3 && node.parentElement && !isSkipped(node.parentElement)) {
            roots.push(node.parentElement);
          }
        }
      }
      if (!roots.length) return;
      // A bulk render: one whole-document pass beats thousands of tiny ones.
      if (roots.length > MAX_ROOTS) roots = [document.body];
      sweepRoots(roots);
    });
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      characterData: true
    });
  }

  function stopObserver() {
    if (!observer) return;
    observer.disconnect();
    observer = null;
  }

  /* English source text of an element, used by search filters so keyword
     search keeps working after the visible text is translated. */
  function enSourceOf(el) {
    if (!el || !el.getAttribute) return '';
    var key = el.getAttribute('data-i18n');
    if (key && DICTS.en[key] !== undefined) return DICTS.en[key];
    return '';
  }

  function missingKeys() {
    var dict = DICTS[current] || {};
    var out = [];
    for (var key in DICTS.en) {
      if (!Object.prototype.hasOwnProperty.call(DICTS.en, key)) continue;
      if (dict[key] === undefined) out.push(key);
    }
    return out;
  }

  function extraKeys() {
    var dict = DICTS[current] || {};
    var out = [];
    for (var key in dict) {
      if (!Object.prototype.hasOwnProperty.call(dict, key)) continue;
      if (DICTS.en[key] === undefined) out.push(key);
    }
    return out;
  }

  var api = {
    t: t,
    tn: tn,
    has: has,
    matches: matches,
    setLanguage: setLanguage,
    getLanguage: getLanguage,
    isRtl: isRtl,
    normalizeLang: normalizeLang,
    applyTranslations: applyTranslations,
    startObserver: startObserver,
    stopObserver: stopObserver,
    enSourceOf: enSourceOf,
    keyForPhrase: keyForPhrase,
    missingKeys: missingKeys,
    extraKeys: extraKeys,
    supported: SUPPORTED.slice(),
    storageKey: STORAGE_KEY,
    dicts: DICTS
  };

  Object.defineProperty(api, 'reverseEnabled', {
    get: function () { return reverseEnabled; },
    set: function (v) { reverseEnabled = !!v; if (!v) EN_TO_KEY = null; }
  });

  global.I18N = api;
  global.t = t;
  global.tn = tn;

  /* Boot: start watching once the document has a body. */
  if (typeof document !== 'undefined') {
    if (document.body) {
      startObserver();
    } else if (document.addEventListener) {
      document.addEventListener('DOMContentLoaded', startObserver, { once: true });
    }
  }

})(typeof window !== 'undefined' ? window : this);
