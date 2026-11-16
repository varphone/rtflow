import { apiV1 } from "../api.js";
import { dateTime, empty, esc } from "../format.js";
import { confirmDialog, secretDialog, toast } from "../ui.js";
import { openKeyDrawer } from "../forms/key.js";

let query = "";
export async function render(_params, ctx) {
  const keys = await apiV1("/keys");
  const items = (Array.isArray(keys) ? keys : keys.items || []).filter((item) => !query || item.name.toLowerCase().includes(query.toLowerCase()) || item.kid.toLowerCase().includes(query.toLowerCase()));
  return {
    title: "推流密钥",
    subtitle: "签发一次性 secret，可限定角色、协议与路径。",
    actions: `<button class="btn small primary" data-action="issue">签发密钥</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-key-search value="${esc(query)}" placeholder="搜索 Key ID 或名称"></div><span class="meta">${items.length} 条</span></div>
      <section class="panel">${items.length ? `<div class="table-wrap"><table class="data"><thead><tr><th>Key ID</th><th>名称</th><th>角色</th><th>协议</th><th>路径范围</th><th>过期</th><th>状态</th><th></th></tr></thead><tbody>
        ${items.map((item) => `<tr><td class="mono">${esc(item.kid)}</td><td>${esc(item.name)}</td><td>${esc((item.roles || []).join(", "))}</td>
          <td>${esc((item.protocols || []).join(", ") || "全部")}</td><td class="mono">${esc((item.paths || []).join(", ") || "全部")}</td>
          <td>${dateTime(item.expires_at)}</td><td>${item.enabled ? '<span class="badge ok">有效</span>' : '<span class="badge bad">已吊销</span>'}</td>
          <td><div class="row-actions"><button class="btn small danger" data-action="revoke" data-kid="${esc(item.kid)}" ${item.enabled ? "" : "disabled"}>吊销</button></div></td></tr>`).join("")}
      </tbody></table></div>` : empty("暂未签发密钥", "点击“签发密钥”创建第一条")}</section>`,
    mount(root) {
      const search = root.querySelector("[data-key-search]");
      search.oninput = () => { query = search.value; ctx.reload({ keepFocus: true }); };
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=issue]").onclick = () => openKeyDrawer((result) => { secretDialog("推流密钥（只显示一次）", result.secret); ctx.reload(); });
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action=revoke]");
        if (!target || target.disabled) return;
        if (!await confirmDialog({ title: "吊销密钥", message: `吊销 ${target.dataset.kid}？`, confirmLabel: "吊销", danger: true })) return;
        try { await apiV1("/keys/" + encodeURIComponent(target.dataset.kid), { method: "DELETE" }); toast("密钥已吊销", "ok"); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      });
    },
  };
}
