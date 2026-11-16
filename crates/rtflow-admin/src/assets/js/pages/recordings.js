import { api } from "../api.js";
import { bytes, dateTime, empty, esc, num } from "../format.js";
import { openDrawer, syncSortAria, toast } from "../ui.js";

let query = "";
let status = "all";
let sort = { field: "path", dir: 1 };
let all = [];

function row(item) {
  return `<tr data-recording="${esc(item.path)}">
    <td><button class="link-cell mono" data-action="detail" data-path="${esc(item.path)}">${esc(item.path)}</button></td>
    <td data-cell="status">${item.active ? '<span class="badge ok">录制中</span>' : '<span class="badge">已停止</span>'}</td>
    <td class="mono" data-cell="segments">${num(item.segments)}</td>
    <td class="mono" data-cell="bytes">${bytes(item.bytes)}</td>
    <td class="cell-sub" data-cell="error">${esc(item.last_error || "—")}</td>
    <td><div class="row-actions"><button class="btn small ghost" data-action="vod" data-path="${esc(item.path)}">点播</button><button class="btn small" data-action="${item.active ? "stop" : "start"}" data-path="${esc(item.path)}" data-cell="action">${item.active ? "停止" : "开始"}</button></div></td>
  </tr>`;
}

function view() {
  const q = query.trim().toLowerCase();
  const filtered = all.filter((item) => {
    if (status === "active" && !item.active) return false;
    if (status === "inactive" && item.active) return false;
    return !q || item.path.toLowerCase().includes(q);
  });
  const value = (item) => sort.field === "segments" ? Number(item.segments || 0) : sort.field === "bytes" ? Number(item.bytes || 0) : String(item.path || "");
  return filtered.sort((left, right) => {
    const a = value(left), b = value(right);
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
  });
}

function table(items) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data">
    <thead><tr><th><button class="sort-btn" data-sort="path">路径${arrow("path")}</button></th><th>状态</th>
      <th><button class="sort-btn" data-sort="segments">分段${arrow("segments")}</button></th>
      <th><button class="sort-btn" data-sort="bytes">大小${arrow("bytes")}</button></th><th>错误</th><th></th></tr></thead>
    <tbody id="recording-rows">${items.length ? items.map(row).join("") : `<tr><td colspan="6" class="empty-cell">没有匹配的录制任务</td></tr>`}</tbody></table></div>`;
}

export async function render(params) {
  query = params?.get("q") || "";
  const linkedStatus = params?.get("status");
  status = ["all", "active", "inactive"].includes(linkedStatus) ? linkedStatus : "all";
  const field = params?.get("sort");
  if (["path", "segments", "bytes"].includes(field)) {
    sort = { field, dir: params?.get("dir") === "-1" ? -1 : 1 };
  } else {
    sort = { field: "path", dir: 1 };
  }
  all = await api("/recordings");
  const initial = view();
  return {
    title: "录制",
    subtitle: "当前录制任务，数据每 2 秒原地刷新；按路径的录制开关在“路径”中设置。",
    actions: `<button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-rec-search value="${esc(query)}" placeholder="搜索录制路径"></div>
      <div class="seg"><button data-rec-status="all" class="${status === "all" ? "active" : ""}">全部</button><button data-rec-status="active" class="${status === "active" ? "active" : ""}">录制中</button><button data-rec-status="inactive" class="${status === "inactive" ? "active" : ""}">已停止</button></div>
      <span class="meta" data-rec-count>${initial.length} 条 · ${initial.filter((item) => item.active).length} 录制中</span></div>
      <section class="panel">${initial.length ? table(initial) : empty("暂无录制任务", "在路径中开启录制，或等待发布者上线", "inbox", `<a class="btn small" href="#/config/paths">前往路径配置</a>`)}</section>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-rec-search]");
      syncSortAria(root.querySelector(".panel"), sort);
      const count = root.querySelector("[data-rec-count]");
      const panel = root.querySelector("section.panel");
      let visible = initial;
      const sync = () => root.querySelectorAll("[data-rec-status]").forEach((button) => button.classList.toggle("active", button.dataset.recStatus === status));
      const paint = () => {
        visible = view();
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.active).length} 录制中`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的录制任务", "调整搜索或状态筛选", "inbox", (!query && status === "all") ? `<a class="btn small" href="#/config/paths">前往路径配置</a>` : "");
        syncSortAria(panel, sort);
        sync();
      };
      const refresh = async () => {
        try { all = await api("/recordings"); } catch (error) { toast(error.message, "bad"); return; }
        const tbody = panel.querySelector("#recording-rows");
        const next = view();
        const existing = tbody ? [...tbody.querySelectorAll("tr[data-recording]")].map((item) => item.dataset.recording) : [];
        const nextPaths = next.map((item) => item.path);
        if (!tbody || existing.length !== nextPaths.length || existing.some((path, index) => path !== nextPaths[index])) {
          visible = next;
          count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.active).length} 录制中`;
          panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的录制任务", "调整搜索或状态筛选");
          syncSortAria(panel, sort);
          sync();
          return;
        }
        for (const item of next) {
          const row = tbody.querySelector(`tr[data-recording="${CSS.escape(item.path)}"]`);
          if (!row) continue;
          row.querySelector('[data-cell="status"]').innerHTML = item.active ? '<span class="badge ok">录制中</span>' : '<span class="badge">已停止</span>';
          row.querySelector('[data-cell="segments"]').textContent = num(item.segments);
          row.querySelector('[data-cell="bytes"]').textContent = bytes(item.bytes);
          row.querySelector('[data-cell="error"]').textContent = item.last_error || "—";
          const action = row.querySelector('[data-cell="action"]');
          action.dataset.action = item.active ? "stop" : "start";
          action.textContent = item.active ? "停止" : "开始";
        }
        visible = next;
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (status !== "all") params.set("status", status);
        if (sort.field !== "path" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/config/recordings${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-rec-status]").forEach((button) => button.onclick = () => { status = button.dataset.recStatus; paint(); syncUrl(); });
      root.addEventListener("click", async (event) => {
        const sorter = event.target.closest("[data-sort]");
        if (sorter) {
          const field = sorter.dataset.sort;
          sort = { field, dir: sort.field === field ? -sort.dir : 1 };
          paint();
          syncUrl();
          return;
        }
        const target = event.target.closest("[data-action]");
        if (!target) return;
        if (target.dataset.action === "detail") {
          const item = all.find((entry) => entry.path === target.dataset.path);
          if (item) openRecordingDetail(item, ctx);
          return;
        }
        if (target.dataset.action === "vod") { location.hash = `#/vod?q=${encodeURIComponent(target.dataset.path)}`; return; }
        if (!["start", "stop"].includes(target.dataset.action)) return;
        try {
          await api(`/recordings/${target.dataset.action}`, { method: "POST", body: { path: target.dataset.path } });
          toast("操作已提交", "ok");
          await refresh();
        } catch (error) { toast(error.message, "bad"); }
      });
      root.querySelector("[data-action=refresh]").onclick = () => refresh();
      const timer = setInterval(refresh, 2000);
      return () => clearInterval(timer);
    },
  };
}


function openRecordingDetail(item, ctx) {
  let timer = null;
  openDrawer({
    title: item.path,
    description: item.active ? "录制中的任务" : "已停止的录制任务",
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong data-rec-detail="status">${item.active ? "录制中" : "已停止"}</strong></div>
        <div class="detail-item"><span>开始时间</span><strong>${esc(dateTime(item.started_at))}</strong></div>
        <div class="detail-item"><span>分段</span><strong class="mono" data-rec-detail="segments">${num(item.segments)}</strong></div>
        <div class="detail-item"><span>已写入</span><strong class="mono" data-rec-detail="bytes">${bytes(item.bytes)}</strong></div>
        <div class="detail-item"><span>丢弃</span><strong class="mono" data-rec-detail="dropped">${num(item.dropped)}</strong></div>
        <div class="detail-item"><span>错误</span><strong class="cell-sub" data-rec-detail="error">${esc(item.last_error || "—")}</strong></div>
      </div>
      <section class="drawer-section"><div style="display:flex;align-items:center;gap:8px"><h3 style="margin:0">录制目录</h3><button class="btn small ghost" data-rec-copy style="margin-left:auto">复制</button></div><pre class="code wrap">${esc(item.directory || "—")}</pre></section>
      <div class="actions"><button class="btn small" data-rec-detail-toggle>${item.active ? "停止录制" : "开始录制"}</button>
        <button class="btn small ghost" data-rec-detail-vod>查看点播</button></div>
    </div>`,
    onClose() { if (timer) clearInterval(timer); },
    onMount(root) {
      let active = item.active;
      const update = async () => {
        if (document.hidden) return;
        let current;
        try { current = (await api("/recordings")).find((entry) => entry.path === item.path); } catch (_) { return; }
        const set = (name, value) => { const node = root.querySelector(`[data-rec-detail="${name}"]`); if (node) node.textContent = value; };
        if (!current) {
          set("status", "已结束");
          active = false;
          root.querySelector("[data-rec-detail-toggle]").textContent = "开始录制";
          return;
        }
        active = current.active;
        set("status", active ? "录制中" : "已停止");
        set("segments", num(current.segments));
        set("bytes", bytes(current.bytes));
        set("dropped", num(current.dropped));
        set("error", current.last_error || "—");
        root.querySelector("[data-rec-detail-toggle]").textContent = active ? "停止录制" : "开始录制";
      };
      timer = setInterval(update, 2000);
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      root.querySelector("[data-rec-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(item.directory || ""); toast("目录已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-rec-detail-toggle]").onclick = async () => {
        const action = active ? "stop" : "start";
        try { await api(`/recordings/${action}`, { method: "POST", body: { path: item.path } }); toast("操作已提交", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
      root.querySelector("[data-rec-detail-vod]").onclick = () => {
        close();
        location.hash = `#/vod?q=${encodeURIComponent(item.path)}`;
      };
    },
  });
}
