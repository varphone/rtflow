import { apiV1 } from "../api.js";
import { dateTime, empty, esc, num } from "../format.js";
import { confirmDialog, toast } from "../ui.js";
import { openPathDrawer } from "../forms/path.js";

let query = "";
export async function render(_params, ctx) {
  const paths = await apiV1("/paths/config");
  const items = (Array.isArray(paths) ? paths : paths.items || []).filter((item) => !query || item.name.toLowerCase().includes(query.toLowerCase()));
  return {
    title: "路径",
    subtitle: "路径策略、发布/观看开关与录制。",
    actions: `<button class="btn small primary" data-action="create">新建路径</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-path-search value="${esc(query)}" placeholder="搜索路径"></div><span class="meta">${items.length} 条</span></div>
      <section class="panel">${items.length ? `<div class="table-wrap"><table class="data"><caption>共 ${items.length} 条路径</caption><thead><tr><th>路径</th><th>来源</th><th>发布 / 观看</th><th>录制</th><th>最大读者</th><th>更新时间</th><th></th></tr></thead><tbody>
      ${items.map((item) => `<tr><td><div class="cell-main mono">/${esc(item.name)}</div><div class="cell-sub">${item.enabled ? "启用" : "停用"}</div></td>
        <td><span class="badge">${esc(item.source)}</span></td><td>${item.publish ? '<span class="badge ok">发布</span>' : ""} ${item.read ? '<span class="badge ok">观看</span>' : '<span class="badge bad">不可读</span>'}</td>
        <td>${item.record ? '<span class="badge ok">开启</span>' : "—"}</td><td class="mono">${item.max_readers || "∞"}</td><td>${dateTime(item.updated_at)}</td>
        <td><div class="row-actions"><button class="btn small" data-action="edit" data-name="${esc(item.name)}">编辑</button>
          <button class="btn small ghost" data-action="kick" data-name="${esc(item.name)}">断开</button>
          <button class="btn small danger" data-action="delete" data-name="${esc(item.name)}">删除</button></div></td></tr>`).join("")}
      </tbody></table></div>` : empty("暂无路径记录", "新建路径或通过配置提供 glob 规则")}</section>`,
    mount(root) {
      const search = root.querySelector("[data-path-search]");
      search.oninput = () => { query = search.value; ctx.reload({ keepFocus: true }); };
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openPathDrawer(null, () => ctx.reload());
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target || !["edit", "kick", "delete"].includes(target.dataset.action)) return;
        const item = items.find((entry) => entry.name === target.dataset.name);
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
