(function () {
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const listen = T.event.listen;
  const $ = (id) => document.getElementById(id);
  const isMac = /Mac/.test(navigator.platform);
  let hits = [], sel = -1, timer = null, seq = 0, modelPresent = true, downloading = false;
  let status = { state: "idle" };
  let syncText = "", ocrText = "", adding = false, shortcutShown = false;
  let modelTotal = 0, fileDone = {}, fileTotal = {}, verifying = false;
  let folderCount = null, countsData = null, suggestions = [], liveData = null;

  function esc(s) {
    return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  }
  // Escape first, then turn the «match» markers into <mark>.
  function excerptHtml(s) {
    return esc(s).replace(/«/g, "<mark>").replace(/»/g, "</mark>");
  }

  const THUMB_KINDS = new Set(["image", "screenshot"]);
  const thumbCache = new Map(); // path -> data URL, or null when none
  const thumbIO = "IntersectionObserver" in window ? new IntersectionObserver((entries) => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      thumbIO.unobserve(en.target);
      loadThumb(en.target);
    }
  }, { root: document.querySelector("main"), rootMargin: "120px" }) : null;

  // Fetch one thumbnail for a visible row. seq is captured now, so a response for an older search is dropped.
  async function loadThumb(img) {
    const path = img.dataset.path, my = seq;
    let url = thumbCache.get(path);
    if (url === undefined) {
      try { url = await invoke("thumbnail", { path }); } catch (e) { url = null; }
      if (thumbCache.size > 500) thumbCache.clear();
      thumbCache.set(path, url);
    }
    if (my !== seq || !img.isConnected) return;
    if (url) img.src = url;
    else { img.closest("li").classList.remove("has-thumb"); img.remove(); }
  }

  function render() {
    const ul = $("results");
    if (thumbIO) thumbIO.disconnect();
    ul.innerHTML = hits.map((h, i) => {
      const thumb = THUMB_KINDS.has(h.kind)
        ? `<img class="thumb" alt="" data-path="${esc(h.path)}"${thumbCache.get(h.path) ? ` src="${thumbCache.get(h.path)}"` : ""}>` : "";
      return `<li data-i="${i}" class="${i === sel ? "sel" : ""}${thumb ? " has-thumb" : ""}">` + thumb +
      `<span class="badge">${esc(h.kind)}</span><span class="name" title="${esc(h.path)}">${esc(h.fileName)}</span>` +
      `<span class="place">${esc(h.place || "")} <button class="reveal" data-i="${i}" title="Reveal in Finder">&#8599;</button></span>` +
      `<span class="folder">${esc(h.folder)}</span>` +
      (h.excerpt ? `<span class="excerpt">${excerptHtml(h.excerpt)}</span>` : "") + `</li>`;
    }).join("");
    if (thumbIO) ul.querySelectorAll("img.thumb").forEach((img) => thumbIO.observe(img));
    const sl = ul.querySelector("li.sel");
    if (sl) sl.scrollIntoView({ block: "nearest" });
  }

  // Kind filter chips: "All" (empty selection) or any set of kind ids. Remembered in localStorage.
  const KIND_IDS = ["screenshot", "image", "pdf", "doc", "audio", "video"];
  const KIND_KEY = "dtfu.kinds";
  let kindSel = [];
  try {
    const saved = JSON.parse(localStorage.getItem(KIND_KEY) || "[]");
    if (Array.isArray(saved)) kindSel = saved.filter((k) => KIND_IDS.includes(k));
  } catch (e) { kindSel = []; }
  function paintChips() {
    document.querySelectorAll("#kinds button").forEach((b) => {
      const on = b.dataset.kind === "" ? kindSel.length === 0 : kindSel.includes(b.dataset.kind);
      b.setAttribute("aria-pressed", on ? "true" : "false");
    });
  }
  $("kinds").addEventListener("click", (ev) => {
    const b = ev.target.closest("button"); if (!b) return;
    const k = b.dataset.kind;
    setKinds(k === "" ? [] : kindSel.includes(k) ? kindSel.filter((x) => x !== k) : [...kindSel, k]);
  });
  function setKinds(list) {
    kindSel = list;
    try { localStorage.setItem(KIND_KEY, JSON.stringify(kindSel)); } catch (e) { /* storage unavailable */ }
    paintChips();
    doSearch();
  }
  $("empty").addEventListener("click", (ev) => { if (ev.target.closest("#clearKinds")) setKinds([]); });
  paintChips();

  async function doSearch() {
    const q = $("q").value.trim();
    const my = ++seq;
    if (!q) { hits = []; sel = -1; render(); $("empty").textContent = ""; return; }
    if (!modelPresent) { hits = []; sel = -1; render(); $("empty").textContent = "Download the search model first"; return; }
    try {
      const r = await invoke("search", { query: q, limit: 30, kinds: kindSel.length ? kindSel : null });
      if (my !== seq) return;
      hits = r; sel = r.length ? 0 : -1; render();
      $("empty").textContent = "";
      if (!r.length && folderCount !== 0) {
        const c = await invoke("counts").catch(() => null);
        if (my !== seq) return;
        if (kindSel.length) $("empty").innerHTML = 'No results in the selected types. <button id="clearKinds">Show all</button>';
        else $("empty").textContent = c && c.pending > 0 ? `Still indexing ${c.pending} files; results will improve` : "No results.";
      }
    } catch (e) {
      if (my !== seq) return;
      hits = []; render(); $("empty").textContent = "Search failed: " + e;
    }
  }

  const open = (h) => invoke("open_hit", { path: h.path, kind: h.kind, loc: h.loc ?? null, query: $("q").value }).catch((e) => showError(e));
  const reveal = (h) => invoke("reveal_path", { path: h.path }).catch((e) => showError(e));

  $("q").addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => { timer = null; doSearch(); }, 200); });
  $("results").addEventListener("click", (ev) => {
    const li = ev.target.closest("li"); if (!li) return;
    const h = hits[+li.dataset.i];
    if (ev.target.closest(".reveal") || ev.metaKey || ev.ctrlKey) reveal(h); else open(h);
  });
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      if (ev.target.tagName === "SELECT") return;
      if ($("q").value) { ev.preventDefault(); clearTimeout(timer); timer = null; $("q").value = ""; doSearch(); $("q").focus(); }
      else if (isMac) { ev.preventDefault(); T.window.getCurrentWindow().hide(); }
      return;
    }
    // Arrows and Enter belong to the search box and the result list only; buttons and selects keep their own keys.
    if (ev.target !== $("q") && !$("results").contains(ev.target)) return;
    if (ev.key === "ArrowDown" && hits.length) { ev.preventDefault(); sel = Math.min(hits.length - 1, sel + 1); render(); }
    else if (ev.key === "ArrowUp" && hits.length) { ev.preventDefault(); sel = Math.max(0, sel - 1); render(); }
    else if (ev.key === "Enter") {
      if (ev.target.tagName === "BUTTON") return;
      ev.preventDefault();
      if (ev.target === $("q") && timer) { clearTimeout(timer); timer = null; doSearch().then(() => {}); return; }
      if (sel >= 0) (ev.metaKey || ev.ctrlKey ? reveal : open)(hits[sel]);
    }
  });

  // Folders panel
  async function loadFolders() {
    let fs;
    try { fs = await invoke("folder_states"); } catch (e) { fs = (await invoke("folders")).map((path) => ({ path, state: "unknown" })); }
    const note = {
      offline: '<em class="dim" title="The drive is unplugged or the share is not answering. Its files stay searchable and are checked again when it is back.">offline</em>',
      different_drive: '<em class="warn" title="A different drive is mounted here. Its files are not indexed until you choose to use it.">different drive</em> <button data-use="1">Use this drive</button>',
    };
    $("folderList").innerHTML = fs.map((f, i) =>
      `<li data-i="${i}"><span>${esc(f.path)}</span> ${note[f.state] || ""}<button data-rm="1">Remove</button></li>`).join("") || '<li class="dim">No folders yet.</li>';
    $("folderList").querySelectorAll("button").forEach((b) => b.addEventListener("click", async () => {
      if (b.disabled) return;
      const path = fs[+b.closest("li").dataset.i].path, use = !!b.dataset.use, label = b.textContent;
      b.disabled = true; b.textContent = use ? "Switching…" : "Removing…";
      try {
        if (use) await invoke("use_drive_now", { path });
        else if (await invoke("ask", { message: `Stop searching "${path}"? Your files are not deleted.`, ok: "Remove" })) await invoke("remove_folder", { path });
      } catch (e) { showError(e); }
      b.disabled = false; b.textContent = label;
      loadFolders(); refreshStatus();
    }));
  }
  $("gear").addEventListener("click", () => { const p = $("folders"); p.hidden = !p.hidden; if (!p.hidden) loadFolders(); });
  // Choose (or take) one folder, add it, start indexing when the model is present. One add at a time.
  const addBtns = () => [$("addFolder"), $("welcomeAdd"), ...$("welcomeChoices").querySelectorAll("button")];
  async function addFlow(btn, getPath) {
    if (adding) return;
    adding = true;
    const label = btn.textContent;
    addBtns().forEach((b) => { b.disabled = true; });
    btn.textContent = "Adding…";
    try {
      const p = await getPath();
      if (p) {
        await invoke("add_folder", { path: p });
        if (modelPresent) await invoke("start_indexing");
      }
    } catch (e) { showError(e); }
    adding = false;
    addBtns().forEach((b) => { b.disabled = false; });
    btn.textContent = label;
    loadFolders(); refreshStatus();
  }
  const pickAndAdd = (ev) => addFlow(ev.currentTarget, () => invoke("pick_folder"));
  $("addFolder").addEventListener("click", pickAndAdd);
  $("welcomeAdd").addEventListener("click", pickAndAdd);
  $("welcomeChoices").addEventListener("click", (ev) => {
    const b = ev.target.closest("button"); if (!b) return;
    addFlow(b, async () => suggestions[+b.dataset.i]);
  });
  $("welcomeDl").addEventListener("click", () => $("dlBtn").click());
  $("pauseBtn").addEventListener("click", async () => {
    const b = $("pauseBtn"), resuming = status.state === "paused";
    b.disabled = true; b.textContent = resuming ? "Resuming…" : "Pausing…";
    try { await invoke(resuming ? "resume" : "pause"); } catch (e) { showError(e); }
    b.disabled = false;
    refreshStatus();
  });

  // First-run welcome: suggested folders that exist under home.
  async function loadSuggestions() {
    try { suggestions = await invoke("suggested_folders"); } catch (e) { suggestions = []; }
    $("welcomeChoices").innerHTML = suggestions.map((p, i) =>
      `<button data-i="${i}" title="${esc(p)}">${esc(p.split(/[\\/]/).pop())}</button>`).join("");
  }

  // Per-kind counts in plain words, e.g. "Images: 120 indexed, 3 waiting".
  const KIND_WORD = { screenshot: "Screenshots", image: "Images", pdf: "PDFs", doc: "Documents", audio: "Audio", video: "Videos" };
  const STATE_WORD = { done: "indexed", pending: "waiting", skipped: "skipped", failed: "could not be read", partial: "partly indexed", unavailable: "unavailable" };
  function countsHtml() {
    if (!countsData) return "";
    const kinds = Object.entries(countsData.filesByKindState || {});
    if (!kinds.length) return "<div>Nothing indexed yet.</div>";
    return kinds.map(([kind, st]) =>
      `<div>${esc(KIND_WORD[kind] || kind)}: ${Object.entries(st).map(([s, n]) => `${fmt(n)} ${esc(STATE_WORD[s] || s)}`).join(", ")}</div>`).join("");
  }
  // Files that can be found by search: fully or partly indexed.
  function searchable() {
    let n = 0;
    for (const st of Object.values((countsData && countsData.filesByKindState) || {})) n += (st.done || 0) + (st.partial || 0);
    return n;
  }

  // Welcome panel, index controls. Called from renderStatus, so it follows every status change.
  function renderControls() {
    // Stay visible until the model is there, so the download prompt cannot disappear.
    $("welcome").hidden = folderCount === null || (folderCount > 0 && modelPresent);
    $("welcomeStep").hidden = modelPresent || folderCount > 0;
    $("welcomeHave").hidden = modelPresent || !(folderCount > 0);
    $("welcomeChoices").hidden = $("welcomeAdd").parentElement.hidden = folderCount > 0;
    $("welcomeDl").hidden = modelPresent || downloading;
    $("pauseBtn").hidden = !(modelPresent && folderCount > 0 && (status.state === "indexing" || status.state === "paused" || syncText));
    if (!$("pauseBtn").disabled) $("pauseBtn").textContent = status.state === "paused" ? "Resume" : "Pause";
    $("indexCounts").innerHTML = countsHtml();
  }

  // Live updates: the pill in the header, the banner when changes are waiting, and the settings in the panel.
  function renderLive() {
    const l = liveData, pill = $("livePill"), banner = $("liveBanner");
    if (!l || !modelPresent || !folderCount) { pill.hidden = true; banner.hidden = true; return; }
    const n = l.pending;
    let tone, text;
    if (!l.watchEnabled) { tone = "off"; text = "Live updates off"; }
    else if (n > 0 && l.deferred) { tone = "wait"; text = `${n} change${n === 1 ? "" : "s"} waiting`; }
    else if (status.state === "indexing") { tone = "busy"; text = "Updating…"; }
    else if (!l.running) { tone = "off"; text = "Not watching"; }
    else { tone = "ok"; text = "Up to date"; }
    pill.hidden = false;
    pill.dataset.tone = tone;
    pill.textContent = (tone === "ok" ? "● " : tone === "wait" ? "⏸ " : tone === "busy" ? "⟳ " : "○ ") + text;
    pill.title = l.watchEnabled ? `Live updates: ${l.mode || "starting"}` : "Live updates are off. Click to change.";
    banner.hidden = !(n > 0 && l.deferred);
    if (!banner.hidden) $("liveBannerText").textContent =
      `${n} file change${n === 1 ? " is" : "s are"} waiting: ${l.deferred}. Searches still work; new and edited files appear after a sync.`;
    if (document.activeElement !== $("watchPolicy")) $("watchPolicy").value = l.policy;
    $("watchOn").checked = l.watchEnabled;
    $("watchPolicy").disabled = !l.watchEnabled;
    $("usageOn").checked = l.usageTracking;
    $("clearUsage").hidden = !l.usageTracking;
  }
  async function refreshLive() {
    try { liveData = await invoke("live"); } catch (e) { liveData = null; }
    renderLive();
  }
  $("livePill").addEventListener("click", () => { const p = $("folders"); p.hidden = false; loadFolders(); p.scrollIntoView({ block: "nearest" }); });
  $("syncNow").addEventListener("click", async () => {
    $("syncNow").disabled = true;
    try { await invoke("start_indexing"); } catch (e) { showError(e); }
    $("syncNow").disabled = false;
    refreshStatus();
  });
  async function saveWatch() {
    try { liveData = await invoke("set_watch", { enabled: $("watchOn").checked, policy: $("watchPolicy").value }); } catch (e) { showError(e); }
    renderLive();
  }
  $("watchOn").addEventListener("change", saveWatch);
  $("watchPolicy").addEventListener("change", saveWatch);
  $("usageOn").addEventListener("change", async () => {
    const on = $("usageOn").checked;
    if (!on && !(await invoke("ask", { message: "Turn off and delete the history of files you opened?", ok: "Turn off" }).catch(() => false))) { $("usageOn").checked = true; return; }
    try { liveData = await invoke("set_usage_tracking", { on }); } catch (e) { showError(e); }
    renderLive();
  });
  $("clearUsage").addEventListener("click", async () => {
    if (!(await invoke("ask", { message: "Delete the history of files you opened?", ok: "Delete" }).catch(() => false))) return;
    try { await invoke("clear_usage"); } catch (e) { showError(e); }
  });

  // Inline banner for errors and notices (replaces alert()).
  function showBanner(msg, kind) {
    $("banner").className = "banner " + kind;
    $("bannerText").textContent = String(msg);
    $("banner").hidden = false;
  }
  function showError(msg) { showBanner(msg, "error"); }
  function showNotice(msg) { showBanner(msg, "notice"); }
  function dismissBanner() { $("banner").hidden = true; }
  $("bannerClose").addEventListener("click", dismissBanner);

  // Model size comes from the pinned manifest in the app, not hard-coded here.
  invoke("model_info").then((m) => {
    modelTotal = m.totalBytes || 0;
    const label = `${Math.round(m.totalBytes / 1e6)} MB`;
    document.querySelectorAll(".modelSize").forEach((el) => { el.textContent = label; });
  }).catch(() => {});

  // Status bar
  function renderStatus() {
    renderControls();
    renderLive();
    $("dl").hidden = modelPresent;
    $("dlBtn").hidden = downloading;
    $("cancelDl").hidden = !downloading;
    if (!modelPresent) { if (!downloading) $("statusText").textContent = "Search model not downloaded"; return; }
    const s = status, pr = progress(), bar = $("idxBar");
    let t;
    if (syncText && s.state !== "paused") t = syncText;
    else if ((s.state === "indexing" || s.state === "paused") && pr.left > 0) {
      const eta = s.state === "indexing" ? etaText(pr.left) : "";
      t = `${s.state === "paused" ? "Paused" : "Indexing"}: ${fmt(pr.finished)} of ${fmt(pr.total)} files (${pr.finished && !pr.pct ? "<1" : pr.pct}%)` +
        (eta ? ` · ${eta}` : "") + (s.state === "indexing" && s.current ? ` · ${s.current}` : "");
    } else if (s.state === "indexing") t = `Indexing ${s.current || ""}`;
    else if (s.state === "paused") t = "Paused";
    else if (s.state === "waiting") t = "Waiting for the search model…";
    else if (s.state === "noModel") t = "Search model not downloaded";
    else if (ocrText) t = ocrText;
    else if (folderCount === 0) t = "Add a folder to start";
    else { const n = searchable(); t = `Ready · ${fmt(n)} ${n === 1 ? "file" : "files"} searchable`; }
    bar.hidden = !((s.state === "indexing" || s.state === "paused") && pr.left > 0);
    bar.max = pr.total || 1; bar.value = pr.finished;
    $("statusText").textContent = t;
    $("statusText").title = t;
  }

  // Whole-index progress from the per-kind counts: everything not "pending" is finished (done, skipped or failed).
  function progress() {
    let total = 0, left = 0;
    for (const st of Object.values((countsData && countsData.filesByKindState) || {}))
      for (const [state, n] of Object.entries(st)) { total += n; if (state === "pending") left += n; }
    const finished = total - left;
    return { total, left, finished, pct: total ? Math.floor(finished * 100 / total) : 0 };
  }
  const fmt = (n) => n.toLocaleString();
  // Time left from the finishing rate over the last few minutes; shown once there is enough to go on.
  let samples = [];
  function sampleProgress() {
    const now = Date.now(), f = progress().finished;
    if (status.state !== "indexing") { samples = []; return; }
    samples.push([now, f]);
    samples = samples.filter(([t]) => now - t <= 180000);
  }
  function etaText(left) {
    if (samples.length < 2) return "";
    const [t0, f0] = samples[0], [t1, f1] = samples[samples.length - 1];
    if (t1 - t0 < 20000 || f1 <= f0) return "";
    const secs = left / ((f1 - f0) / ((t1 - t0) / 1000));
    if (secs < 90) return "about a minute left";
    if (secs < 3600) return `about ${Math.round(secs / 60)} min left`;
    if (secs < 172800) return `about ${Math.round(secs / 3600)} h left`;
    return `about ${Math.round(secs / 86400)} days left`;
  }
  // Each call fails on its own; a failed one keeps the last good value.
  async function refreshStatus() {
    let failed = 0;
    const keep = async (cmd, old) => { try { return await invoke(cmd); } catch (e) { failed++; return old; } };
    modelPresent = await keep("model_present", modelPresent);
    status = await keep("status", status);
    countsData = await keep("counts", countsData);
    const fs = await keep("folders", null);
    if (fs) folderCount = fs.length;
    liveData = await keep("live", liveData);
    if (failed === 5) { $("statusText").textContent = "Can't reach the search engine. Trying again…"; return; }
    sampleProgress();
    renderStatus();
  }
  $("dlBtn").addEventListener("click", async () => {
    downloading = true; resetDl(); $("dlBar").hidden = false; $("dlBar").value = 0; $("cancelDl").hidden = false; renderStatus();
    dismissBanner();
    $("statusText").textContent = "Downloading…";
    try { await invoke("download_models"); } catch (e) { finishDownload(); showError(e); renderStatus(); }
  });

  $("cancelDl").addEventListener("click", async () => {
    $("cancelDl").disabled = true; $("statusText").textContent = "Cancelling…";
    try { await invoke("cancel_download"); } catch (e) { showError(e); }
  });
  function resetDl() { fileDone = {}; fileTotal = {}; verifying = false; }
  function finishDownload() { resetDl(); downloading = false; $("dlBar").hidden = true; $("cancelDl").hidden = true; $("cancelDl").disabled = false; }
  listen("model-cancelled", (ev) => { finishDownload(); showNotice(ev.payload); refreshStatus(); });

  const sum = (o) => Object.values(o).reduce((a, b) => a + b, 0);
  const mb = (n) => Math.round(n / 1048576);
  // One bar for the whole model: bytes done across files, total from the manifest (or the files seen so far).
  listen("model-progress", (ev) => {
    const p = ev.payload;
    fileDone[p.name] = p.done; fileTotal[p.name] = p.total;
    const done = sum(fileDone), total = modelTotal || sum(fileTotal);
    verifying = p.total > 0 && p.done >= p.total;
    $("dlBar").max = total || 1; $("dlBar").value = done;
    $("statusText").textContent = verifying ? "Verifying…" : `Downloading the search model… ${mb(done)}/${mb(total)} MB`;
  });
  listen("model-ready", () => { finishDownload(); dismissBanner(); refreshStatus(); });
  listen("model-error", (ev) => { finishDownload(); showError("Download failed: " + ev.payload); renderStatus(); });
  // Combined over the files of the download, like the model download.
  let ocrFiles = {};
  listen("ocr-progress", (ev) => {
    const p = ev.payload;
    ocrFiles[p.name] = p;
    const all = Object.values(ocrFiles), done = all.reduce((a, f) => a + f.done, 0), total = all.reduce((a, f) => a + f.total, 0);
    ocrText = `Downloading text reader… ${mb(done)}/${mb(total)} MB`;
    renderStatus();
  });
  listen("ocr-ready", () => { ocrText = ""; renderStatus(); });
  listen("ocr-done", () => { ocrText = ""; ocrFiles = {}; renderStatus(); });
  listen("ocr-error", (ev) => { ocrText = ""; showError("Text reader download failed: " + ev.payload); renderStatus(); });
  listen("shortcut-error", (ev) => { if (!shortcutShown) { shortcutShown = true; showNotice(ev.payload); } });

  // Folders from a "synced" event that cannot be reached, as a short list for a notice.
  const names = (v) => [].concat(v || []).map((x) => (x && x.path) || x).join(", ");
  let evTimer = null, lastAway = "", engineNotice = false;
  listen("engine-event", (ev) => {
    const p = ev.payload;
    if (p.type === "syncing") syncText = `Scanning folders… ${fmt(p.seen || 0)} files found`;
    else syncText = "";
    if (p.type === "progress" || p.type === "rest") status = { state: "indexing", done: p.done, total: p.total, current: (p.path || "").split(/[\\/]/).pop() };
    else if (p.type === "idle" || p.type === "stopped") { status = { state: "idle" }; if (p.type === "stopped") setTimeout(refreshStatus, 100); }
    else if (p.type === "waiting") status = { state: "waiting" };
    else if (p.type === "error") { status = { state: "idle" }; showError(p.message); }
    else if (p.type === "notice") { showNotice(p.message); engineNotice = true; }
    else if (p.type === "synced") {
      // Only when the set of unreachable folders changes, not on every resync.
      const off = names(p.offline), diff = names(p.different_drive), away = off + "|" + diff;
      if (away !== lastAway) {
        lastAway = away;
        if (off) showNotice(`Could not reach: ${off}. Its files stay searchable and are checked again when it is back.`);
        if (diff) showNotice(`A different drive is mounted at: ${diff}. Open Folders and settings to choose whether to use it.`);
      }
    }
    // An engine notice (waiting for another process) ends once indexing moves.
    if (engineNotice && (p.type === "syncing" || p.type === "progress" || p.type === "synced")) { engineNotice = false; dismissBanner(); }
    renderStatus();
    if (p.type === "synced" && !$("folders").hidden) loadFolders();
    if (p.type === "idle" || p.type === "synced") { clearTimeout(evTimer); evTimer = setTimeout(refreshStatus, 300); }
  });

  // The shortcut may have failed before this page was listening.
  invoke("shortcut_error").then((m) => { if (m && !shortcutShown) { shortcutShown = true; showNotice(m); } }).catch(() => {});
  loadSuggestions();
  refreshStatus();
  setInterval(() => { if (!downloading) refreshStatus(); }, 5000);
  $("q").focus();
})();
