import { apiV1 } from "../api.js";
import { empty, esc, num, stateBadge } from "../format.js";
import { confirmDialog, openDialog, toast } from "../ui.js";
import { openProxyDrawer } from "../forms/proxy.js";

let query = "";
let filter = "all";
export async function render(_params, ctx) {
  const [status, config] = await Promise.all([apiV1("/proxies"), apiV1("/proxies/config")]);
  const statuses = status.proxies || [];
  const configs = config.mounts || [];
  const items = statuses.map((entry) => Object.assign({}, configs.find((mount) => mount.path === entry.path) || {}, entry));
  const filtered = items.filter((item) => {
    const match = !query || item.path.toLowerCase().includes(query.toLowerCase()) || (item.upstream || "").toLowerCase().includes(query.toLowerCase());
    return match && (filter === "all" || item.state === filter);
  });
  return {
    title: "RTSP 代理",
    subtitle: "拉流代理配置与运行状态。",
    actions: `<button class="btn small primary" data-action="create">新增代理</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-proxy-search value="${esc(query)}" placeholder="搜索路径或上游"></div>
      <div class="seg">${["all", "streaming", "idle", "backoff", "describing"].map((state) => `<button data-proxy-filter="${state}" class="${filter === state ? "active" : ""}">${state === "all" ? "全部" : state}</button>`).join("")}</div>
      <span class="meta">${filtered.length} 条</span></div>
      <section class="panel">${filtered.length ? `<div class="table-wrap"><table class="data"><thead><tr><th>路径</th><th>状态</th><th>上游</th><th>订阅</th><th>轨道</th><th>重试</th><th>错误</th><th></th></tr></thead><tbody>
        ${filtered.map((item) => `<tr><td class="mono">${esc(item.path)}</td><td>${stateBadge(item.state)}</td><td class="mono">${esc(item.upstream || "—")}</td>
          <td class="mono">${num(item.subscribers)}</td><td class="mono">${num(item.tracks)}</td><td class="mono">${num(item.attempts)}</td><td class="cell-sub">${esc(item.last_error || "—")}</td>
          <td><div class="row-actions"><button class="btn small" data-action="edit" data-path="${esc(item.path)}">编辑</button>
            <button class="btn small ghost" data-action="restart" data-path="${esc(item.path)}">重启</button>
            <button class="btn small ghost" data-action="probe" data-path="${esc(item.path)}">探测</button>
            <button class="btn small danger" data-action="delete" data-path="${esc(item.path)}">删除</button></div></td></tr>`).join("")}
      </tbody></table></div>` : empty("暂无代理", "新增一个 RTSP 拉流代理")}</section>`,
    mount(root) {
      const search = root.querySelector("[data-proxy-search]");
      search.oninput = () => { query = search.value; ctx.reload({ keepFocus: true }); };
      root.querySelectorAll("[data-proxy-filter]").forEach((button) => button.onclick = () => { filter = button.dataset.proxyFilter; ctx.reload(); });
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openProxyDrawer(null, () => ctx.reload());
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const path = target.dataset.path;
        if (target.dataset.action === "edit") {
          const item = items.find((entry) => entry.path === path);
          if (item) openProxyDrawer(item, () => ctx.reload());
        } else if (target.dataset.action === "restart") {
          try { await apiV1("/proxies/restart?path=" + encodeURIComponent(path), { method: "POST" }); toast("重启已提交", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        } else if (target.dataset.action === "probe") {
          try {
            const result = await apiV1("/proxies/probe?path=" + encodeURIComponent(path), { method: "POST" });
            openDialog({ title: "上游探测", body: `<div class="note">${esc(result.session_name || "")}</div><pre class="code">${esc(JSON.stringify(result.tracks || [], null, 2))}</pre>`, footer: `<button class="btn" data-close-dialog>关闭</button>` });
          } catch (error) { toast(error.message, "bad"); }
        } else if (target.dataset.action === "delete") {
          if (!await confirmDialog({ title: "删除代理", message: `删除 ${path}？`, confirmLabel: "删除", danger: true })) return;
          try { await apiV1("/proxies?path=" + encodeURIComponent(path), { method: "DELETE" }); toast("代理已删除", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}
