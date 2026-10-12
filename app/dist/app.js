(function () {
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const listen = T.event.listen;
  const $ = (id) => document.getElementById(id);
  const isMac = /Mac/.test(navigator.platform);
  let hits = [], sel = -1, timer = null, seq = 0, modelPresent = true, downloading = false;
  let status = { state: "idle" }, vectors = 0;
  let folderCount = null, countsData = null, suggestions = [];

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
  const KIND_KEY = "digup.kinds";
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
    if (k === "") kindSel = [];
    else kindSel = kindSel.includes(k) ? kindSel.filter((x) => x !== k) : [...kindSel, k];
    try { localStorage.setItem(KIND_KEY, JSON.stringify(kindSel)); } catch (e) { /* storage unavailable */ }
    paintChips();
    doSearch();
  });
  paintChips();

  async function doSearch() {
    const q = $("q").value.trim();
    const my = ++seq;
    if (!q) { hits = []; sel = -1; render(); $("empty").textContent = ""; return; }
    try {
      const r = await invoke("search", { query: q, limit: 30, kinds: kindSel.length ? kindSel : null });
      if (my !== seq) return;
      hits = r; sel = r.length ? 0 : -1; render();
      $("empty").textContent = "";
      if (!r.length && folderCount !== 0) {
        const c = await invoke("counts").catch(() => null);
        if (my !== seq) return;
        $("empty").textContent = c && c.pending > 0 ? `Still indexing ${c.pending} files; results will improve` : "No results.";
      }
    } catch (e) {
      if (my !== seq) return;
      hits = []; render(); $("empty").textContent = "Search failed: " + e;
    }
  }

  const open = (h) => invoke("open_hit", { path: h.path, kind: h.kind, loc: h.loc ?? null, query: $("q").value }).catch((e) => alert(e));
  const reveal = (h) => invoke("reveal_path", { path: h.path }).catch((e) => alert(e));

  $("q").addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => { timer = null; doSearch(); }, 200); });
  $("results").addEventListener("click", (ev) => {
    const li = ev.target.closest("li"); if (!li) return;
    const h = hits[+li.dataset.i];
    if (ev.target.closest(".reveal") || ev.metaKey || ev.ctrlKey) reveal(h); else open(h);
  });
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") { ev.preventDefault(); T.window.getCurrentWindow().hide(); }
    else if (ev.key === "ArrowDown" && hits.length) { ev.preventDefault(); sel = Math.min(hits.length - 1, sel + 1); render(); }
    else if (ev.key === "ArrowUp" && hits.length) { ev.preventDefault(); sel = Math.max(0, sel - 1); render(); }
    else if (ev.key === "Enter") {
      ev.preventDefault();
      if (ev.target === $("q") && timer) { clearTimeout(timer); timer = null; doSearch().then(() => {}); return; }
      if (sel >= 0) (ev.metaKey || ev.ctrlKey ? reveal : open)(hits[sel]);
    }
  });

  // Folders panel
  async function loadFolders() {
    const fs = await invoke("folders");
    $("folderList").innerHTML = fs.map((f, i) =>
      `<li><span>${esc(f)}</span><button data-i="${i}">Remove</button></li>`).join("") || '<li class="dim">No folders yet.</li>';
    $("folderList").querySelectorAll("button").forEach((b) => b.addEventListener("click", async () => {
      try { await invoke("remove_folder", { path: fs[+b.dataset.i] }); } catch (e) { alert(e); }
      loadFolders(); refreshStatus();
    }));
  }
  $("gear").addEventListener("click", () => { const p = $("folders"); p.hidden = !p.hidden; if (!p.hidden) loadFolders(); });
  // Add one folder (already chosen), start indexing when the model is present, then refresh the panels.
  async function addFolderPath(p) {
    try {
      await invoke("add_folder", { path: p });
      if (modelPresent) await invoke("start_indexing");
      $("welcome").hidden = true;
    } catch (e) { alert(e); }
    loadFolders(); refreshStatus();
  }
  async function pickAndAdd() {
    try {
      const p = await invoke("pick_folder");
      if (p) await addFolderPath(p);
    } catch (e) { alert(e); }
  }
  $("addFolder").addEventListener("click", pickAndAdd);
  $("welcomeAdd").addEventListener("click", pickAndAdd);
  $("welcomeChoices").addEventListener("click", (ev) => {
    const b = ev.target.closest("button"); if (!b) return;
    addFolderPath(suggestions[+b.dataset.i]);
  });
  $("welcomeDl").addEventListener("click", () => $("dlBtn").click());
  $("pauseBtn").addEventListener("click", async () => {
    try { await invoke(status.state === "paused" ? "resume" : "pause"); } catch (e) { alert(e); }
    refreshStatus();
  });

  // First-run welcome: suggested folders that exist under home.
  async function loadSuggestions() {
    try { suggestions = await invoke("suggested_folders"); } catch (e) { suggestions = []; }
    $("welcomeChoices").innerHTML = suggestions.map((p, i) =>
      `<button data-i="${i}" title="${esc(p)}">${esc(p.split(/[\\/]/).pop())}</button>`).join("");
  }

  // Per-kind counts, e.g. "image 120 done, 3 pending".
  function countsHtml() {
    if (!countsData) return "";
    const kinds = Object.entries(countsData.filesByKindState || {});
    if (!kinds.length) return "<div>Nothing indexed yet.</div>";
    return kinds.map(([kind, st]) =>
      `<div>${esc(kind)} ${Object.entries(st).map(([s, n]) => `${esc(n)} ${esc(s)}`).join(", ")}</div>`).join("");
  }

  // Welcome panel, index controls. Called from renderStatus, so it follows every status change.
  function renderControls() {
    $("welcome").hidden = folderCount !== 0;
    $("welcomeStep").hidden = modelPresent;
    $("welcomeDl").hidden = modelPresent || downloading;
    $("pauseBtn").hidden = !(modelPresent && folderCount > 0);
    $("pauseBtn").textContent = status.state === "paused" ? "Resume" : "Pause";
    $("indexCounts").innerHTML = countsHtml();
  }

  // Status bar
  function renderStatus() {
    renderControls();
    $("dl").hidden = modelPresent;
    $("dlBtn").hidden = downloading;
    if (!modelPresent) { if (!downloading) $("statusText").textContent = "No model"; return; }
    const s = status;
    let t;
    if (s.state === "indexing") t = `Indexing ${s.done}/${s.total} ${s.current || ""}`;
    else if (s.state === "paused") t = "Paused";
    else if (s.state === "noModel") t = "No model";
    else t = `Idle ${vectors} vectors`;
    $("statusText").textContent = t;
  }
  async function refreshStatus() {
    try {
      modelPresent = await invoke("model_present");
      status = await invoke("status");
      countsData = await invoke("counts");
      vectors = countsData.vectors;
      folderCount = (await invoke("folders")).length;
    } catch (e) { $("statusText").textContent = String(e); return; }
    renderStatus();
  }
  $("dlBtn").addEventListener("click", async () => {
    downloading = true; $("dlBar").hidden = false; $("dlBar").value = 0; renderStatus();
    $("statusText").textContent = "Downloading…";
    try { await invoke("download_models"); } catch (e) { downloading = false; alert(e); renderStatus(); }
  });

  listen("model-progress", (ev) => {
    const p = ev.payload;
    $("dlBar").max = p.total || 1; $("dlBar").value = p.done;
    $("statusText").textContent = `Downloading ${p.name} ${Math.round(p.done / 1048576)}/${Math.round(p.total / 1048576)} MB`;
  });
  listen("model-ready", () => { downloading = false; $("dlBar").hidden = true; refreshStatus(); });
  listen("model-error", (ev) => { downloading = false; $("dlBar").hidden = true; $("statusText").textContent = "Download failed: " + ev.payload; renderStatus(); $("statusText").textContent = "Download failed: " + ev.payload; });
  let evTimer = null;
  listen("engine-event", (ev) => {
    const p = ev.payload;
    if (p.type === "progress" || p.type === "rest") status = { state: "indexing", done: p.done, total: p.total, current: (p.path || "").split(/[\\/]/).pop() };
    else if (p.type === "idle" || p.type === "stopped") { status = { state: "idle" }; if (p.type === "stopped") setTimeout(refreshStatus, 100); }
    else if (p.type === "error") { $("statusText").textContent = "Error: " + p.message; return; }
    renderStatus();
    if (p.type === "idle" || p.type === "synced") { clearTimeout(evTimer); evTimer = setTimeout(refreshStatus, 300); }
  });

  loadSuggestions();
  refreshStatus();
  setInterval(() => { if (!downloading) refreshStatus(); }, 5000);
  $("q").focus();
})();
