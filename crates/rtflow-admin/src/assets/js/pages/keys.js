import { apiV1 } from "../api.js";
import { dateTime, empty, esc } from "../format.js";
import { confirmDialog, secretDialog, toast } from "../ui.js";
import { openKeyDrawer } from "../forms/key.js";

let query = "";
let sort = { field: "expires_at", dir: 1 };

export async function render(params, ctx) {
  query = params?.get("q") || "";
  const field = params?.get("sort");
  if (["kid", "name", "expires_at"].includes(field)) {
    sort = { field, dir: params?.get("dir") === "-1" ? -1 : 1 };
  } else {
    sort = { field: "expires_at", dir: 1 };
  }
  const keys = await apiV1("/keys");
  const all = Array.isArray(keys) ? keys : keys.items || [];
  const initial = view(all);
  return {
    title: "推流密钥",
    subtitle: "签发一次性 secret，可限定角色、协议与路径。",
    actions: `<button class="btn small primary" data-action="issue">签发密钥</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-key-search value="${esc(query)}" placeholder="搜索 Key ID 或名称"></div><span class="meta" data-key-count>${initial.length} 条 · ${initial.filter((item) => item.enabled && !isExpired(item)).length} 有效</span></div>
      <section class="panel" data-key-panel>${initial.length ? table(initial) : empty("暂未签发密钥", "点击“签发密钥”创建第一条")}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-key-panel]");
      const count = root.querySelector("[data-key-count]");
      const search = root.querySelector("[data-key-search]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.enabled && !isExpired(item)).length} 有效`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的密钥", "调整搜索条件");
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (sort.field !== "expires_at" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/config/keys${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=issue]").onclick = () => openKeyDrawer((result) => { secretDialog("推流密钥（只显示一次）", result.secret); ctx.reload(); });
      if (params?.get("new") === "1") {
        history.replaceState(null, "", "#/config/keys");
        openKeyDrawer((result) => { secretDialog("推流密钥（只显示一次）", result.secret); ctx.reload(); });
      }
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
        const item = visible.find((entry) => entry.kid === target.dataset.kid);
        if (!item) return;
        if (target.dataset.action === "copy") {
          try { await navigator.clipboard.writeText(item.kid); toast("Key ID 已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        } else if (target.dataset.action === "revoke") {
          if (target.disabled) return;
          if (!await confirmDialog({ title: "吊销密钥", message: `吊销 ${item.kid}？`, confirmLabel: "吊销", danger: true })) return;
          try { await apiV1("/keys/" + encodeURIComponent(item.kid), { method: "DELETE" }); toast("密钥已吊销", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function view(all) {
  const q = query.trim().toLowerCase();
  const items = all.filter((item) => !q || item.name.toLowerCase().includes(q) || item.kid.toLowerCase().includes(q));
  const value = (item) => sort.field === "name" ? item.name.toLowerCase() : sort.field === "kid" ? item.kid : Number(item.expires_at || Number.MAX_SAFE_INTEGER);
  return items.sort((left, right) => {
    const a = value(left), b = value(right);
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
  });
}

function table(items) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data"><thead><tr>
    <th><button class="sort-btn" data-sort="kid">Key ID${arrow("kid")}</button></th>
    <th><button class="sort-btn" data-sort="name">名称${arrow("name")}</button></th><th>角色</th><th>协议</th><th>路径范围</th>
    <th><button class="sort-btn" data-sort="expires_at">过期${arrow("expires_at")}</button></th><th>状态</th><th></th></tr></thead><tbody>
    ${items.map((item) => `<tr><td class="mono">${esc(item.kid)}</td><td>${esc(item.name)}</td><td>${esc((item.roles || []).join(", "))}</td>
      <td>${esc((item.protocols || []).join(", ") || "全部")}</td><td class="mono">${esc((item.paths || []).join(", ") || "全部")}</td>
      <td>${dateTime(item.expires_at)}</td><td>${keyStatus(item)}</td>
      <td><div class="row-actions"><button class="btn small ghost" data-action="copy" data-kid="${esc(item.kid)}">复制</button><button class="btn small danger" data-action="revoke" data-kid="${esc(item.kid)}" ${item.enabled ? "" : "disabled"}>吊销</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}


function isExpired(item) {
  return Boolean(item.expires_at && Number(item.expires_at) * 1000 < Date.now());
}

function keyStatus(item) {
  if (!item.enabled) return '<span class="badge bad">已吊销</span>';
  if (isExpired(item)) return '<span class="badge bad">已过期</span>';
  const remaining = Number(item.expires_at || 0) * 1000 - Date.now();
  if (item.expires_at && remaining < 24 * 3600 * 1000) return '<span class="badge warn">即将过期</span>';
  return '<span class="badge ok">有效</span>';
}
