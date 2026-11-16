import { apiV1 } from "../api.js";
import { dateTime, empty, esc } from "../format.js";
import { confirmDialog, toast } from "../ui.js";
import { openPathDrawer } from "../forms/path.js";

let query = "";
let sort = { field: "name", dir: 1 };

export async function render(_params, ctx) {
  const paths = await apiV1("/paths/config");
  const all = Array.isArray(paths) ? paths : paths.items || [];
  const initial = view(all);
  return {
    title: "路径",
    subtitle: "路径策略、发布/观看开关与录制。",
    actions: `<button class="btn small primary" data-action="create">新建路径</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-path-search value="${esc(query)}" placeholder="搜索路径"></div><span class="meta" data-path-count>${initial.length} 条</span></div>
      <section class="panel" data-path-panel>${initial.length ? table(initial) : empty("暂无路径记录", "新建路径或通过配置提供 glob 规则")}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-path-panel]");
      const count = root.querySelector("[data-path-count]");
      const search = root.querySelector("[data-path-search]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的路径", "调整搜索条件或新建路径");
      };
      search.oninput = () => { query = search.value; paint(); };
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openPathDrawer(null, () => ctx.reload());
      root.addEventListener("click", async (event) => {
        const sorter = event.target.closest("[data-sort]");
        if (sorter) {
          const field = sorter.dataset.sort;
          sort = { field, dir: sort.field === field ? -sort.dir : 1 };
          paint();
          return;
        }
        const target = event.target.closest("[data-action]");
        if (!target || !["edit", "kick", "delete"].includes(target.dataset.action)) return;
        const item = visible.find((entry) => entry.name === target.dataset.name);
        if (!item) return;
        if (target.dataset.action === "edit") return openPathDrawer(item, () => ctx.reload());
        if (target.dataset.action === "kick") {
          try { await apiV1("/kick?path=" + encodeURIComponent(item.name), { method: "POST" }); toast("已断开", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
        if (target.dataset.action === "delete") {
          if (!await confirmDialog({ title: "删除路径", message: `删除 ${item.name}？`, confirmLabel: "删除", danger: true })) return;
          try { await apiV1("/paths/" + encodeURIComponent(item.name), { method: "DELETE" }); toast("路径已删除", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function view(all) {
  const q = query.trim().toLowerCase();
  const items = all.filter((item) => !q || item.name.toLowerCase().includes(q));
  const value = (item) => {
    if (sort.field === "name") return item.name.toLowerCase();
    if (sort.field === "updated_at") return Number(item.updated_at || 0);
    if (sort.field === "max_readers") return Number(item.max_readers || Number.MAX_SAFE_INTEGER);
    return String(item.source || "");
  };
  return items.sort((left, right) => {
    const a = value(left), b = value(right);
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
  });
}

function table(items) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data"><caption>共 ${items.length} 条路径</caption><thead><tr>
    <th><button class="sort-btn" data-sort="name">路径${arrow("name")}</button></th>
    <th><button class="sort-btn" data-sort="source">来源${arrow("source")}</button></th>
    <th>发布 / 观看</th><th>录制</th>
    <th><button class="sort-btn" data-sort="max_readers">最大读者${arrow("max_readers")}</button></th>
    <th><button class="sort-btn" data-sort="updated_at">更新时间${arrow("updated_at")}</button></th><th></th></tr></thead><tbody>
    ${items.map((item) => `<tr><td><div class="cell-main mono">/${esc(item.name)}</div><div class="cell-sub">${item.enabled ? "启用" : "停用"}</div></td>
      <td><span class="badge">${esc(item.source)}</span></td><td>${item.publish ? '<span class="badge ok">发布</span>' : ""} ${item.read ? '<span class="badge ok">观看</span>' : '<span class="badge bad">不可读</span>'}</td>
      <td>${item.record ? '<span class="badge ok">开启</span>' : "—"}</td><td class="mono">${item.max_readers || "∞"}</td><td>${dateTime(item.updated_at)}</td>
      <td><div class="row-actions"><button class="btn small" data-action="edit" data-name="${esc(item.name)}">编辑</button>
        <button class="btn small ghost" data-action="kick" data-name="${esc(item.name)}">断开</button>
        <button class="btn small danger" data-action="delete" data-name="${esc(item.name)}">删除</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}
