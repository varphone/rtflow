import { api } from "../api.js";
import { bytes, empty, esc, num } from "../format.js";
import { toast } from "../ui.js";

let query = "";
let status = "all";
let sort = { field: "path", dir: 1 };
let all = [];

function row(item) {
  return `<tr data-recording="${esc(item.path)}">
    <td class="mono">${esc(item.path)}</td>
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
  const linked = params?.get("q");
  if (linked !== null && linked !== undefined) query = linked;
  all = await api("/recordings");
  const initial = view();
  return {
    title: "录制",
    subtitle: "当前录制任务，数据每 2 秒原地刷新；按路径的录制开关在“路径”中设置。",
    actions: `<button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-rec-search value="${esc(query)}" placeholder="搜索录制路径"></div>
      <div class="seg"><button data-rec-status="all" class="${status === "all" ? "active" : ""}">全部</button><button data-rec-status="active" class="${status === "active" ? "active" : ""}">录制中</button><button data-rec-status="inactive" class="${status === "inactive" ? "active" : ""}">已停止</button></div>
      <span class="meta" data-rec-count>${initial.length} 条</span></div>
      <section class="panel">${initial.length ? table(initial) : empty("暂无录制任务", "在路径中开启录制，或等待发布者上线")}</section>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-rec-search]");
      const count = root.querySelector("[data-rec-count]");
      const panel = root.querySelector("section.panel");
      let visible = initial;
      const sync = () => root.querySelectorAll("[data-rec-status]").forEach((button) => button.classList.toggle("active", button.dataset.recStatus === status));
      const paint = () => {
        visible = view();
        count.textContent = `${visible.length} 条`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的录制任务", "调整搜索或状态筛选");
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
          count.textContent = `${visible.length} 条`;
          panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的录制任务", "调整搜索或状态筛选");
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
      search.oninput = () => { query = search.value; paint(); };
      root.querySelectorAll("[data-rec-status]").forEach((button) => button.onclick = () => { status = button.dataset.recStatus; paint(); });
      root.addEventListener("click", async (event) => {
        const sorter = event.target.closest("[data-sort]");
        if (sorter) {
          const field = sorter.dataset.sort;
          sort = { field, dir: sort.field === field ? -sort.dir : 1 };
          paint();
          return;
        }
        const target = event.target.closest("[data-action]");
        if (!target) return;
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
