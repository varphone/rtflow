import { apiV1 } from "../api.js";
import { dateTime, empty, esc, exportCsv, isoTime } from "../format.js";
import { confirmDialog, openDrawer, secretDialog, syncSortAria, toast } from "../ui.js";
import { openKeyDrawer } from "../forms/key.js";

let query = "";
let sort = { field: "expires_at", dir: 1 };
let stateFilter = "all";

export async function render(params, ctx) {
  query = params?.get("q") || "";
  const linkedState = params?.get("state");
  stateFilter = ["valid", "soon", "expired", "revoked"].includes(linkedState) ? linkedState : "all";
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
    actions: `<button class="btn small ghost" data-action="export">导出</button><button class="btn small primary" data-action="issue">签发密钥</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-key-search value="${esc(query)}" placeholder="搜索 Key ID 或名称"></div><div class="seg"><button data-key-state="all" class="${stateFilter === "all" ? "active" : ""}">全部</button><button data-key-state="valid" class="${stateFilter === "valid" ? "active" : ""}">有效</button><button data-key-state="soon" class="${stateFilter === "soon" ? "active" : ""}">将过期</button><button data-key-state="expired" class="${stateFilter === "expired" ? "active" : ""}">过期</button><button data-key-state="revoked" class="${stateFilter === "revoked" ? "active" : ""}">吊销</button></div><span class="meta" data-key-count>${initial.length} 条 · ${initial.filter((item) => item.enabled && !isExpired(item)).length} 有效</span></div>
      <section class="panel" data-key-panel>${initial.length ? table(initial) : empty("暂未签发密钥", "点击“签发密钥”创建第一条", "keys", `<a class="btn small" href="#/config/keys?new=1">签发密钥</a>`)}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-key-panel]");
      const count = root.querySelector("[data-key-count]");
      const search = root.querySelector("[data-key-search]");
      syncSortAria(root.querySelector(".panel"), sort);
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.enabled && !isExpired(item)).length} 有效`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的密钥", "调整搜索条件", "keys");
        syncSortAria(panel, sort);
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (stateFilter !== "all") params.set("state", stateFilter);
        if (sort.field !== "expires_at" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/config/keys${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-key-state]").forEach((button) => button.onclick = () => {
        stateFilter = button.dataset.keyState;
        root.querySelectorAll("[data-key-state]").forEach((item) => item.classList.toggle("active", item === button));
        paint(); syncUrl();
      });
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=export]").onclick = () => {
        exportCsv(`keys-${new Date().toISOString().slice(0, 10)}.csv`,
          ["kid", "name", "roles", "protocols", "paths", "max_connections", "created_at", "expires_at", "status"],
          visible.map((item) => [item.kid, item.name, (item.roles || []).join(" "), (item.protocols || []).join(" "), (item.paths || []).join(" "), item.max_connections || 0, isoTime(item.created_at), isoTime(item.expires_at), isExpired(item) ? "expired" : item.enabled ? "active" : "revoked"]));
        toast(`已导出 ${visible.length} 条密钥`, "ok");
      };
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
        if (target.dataset.action === "detail") {
          openKeyDetail(item, ctx);
        } else if (target.dataset.action === "copy") {
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
  const items = all.filter((item) => {
    const match = !q || item.name.toLowerCase().includes(q) || item.kid.toLowerCase().includes(q);
    return match && (stateFilter === "all" || keyState(item) === stateFilter);
  });
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
    ${items.map((item) => `<tr><td><button class="link-cell mono" data-action="detail" data-kid="${esc(item.kid)}">${esc(item.kid)}</button></td><td>${esc(item.name)}</td><td>${esc((item.roles || []).join(", "))}</td>
      <td>${esc((item.protocols || []).join(", ") || "全部")}</td><td class="mono">${esc((item.paths || []).join(", ") || "全部")}</td>
      <td>${dateTime(item.expires_at)}</td><td>${keyStatus(item)}</td>
      <td><div class="row-actions"><button class="btn small ghost" data-action="copy" data-kid="${esc(item.kid)}">复制</button><button class="btn small danger" data-action="revoke" data-kid="${esc(item.kid)}" ${item.enabled ? "" : "disabled"}>吊销</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}


function isExpired(item) {
  return Boolean(item.expires_at && Number(item.expires_at) * 1000 < Date.now());
}

function keyState(item) {
  if (!item.enabled) return "revoked";
  if (isExpired(item)) return "expired";
  const remaining = Number(item.expires_at || 0) * 1000 - Date.now();
  if (item.expires_at && remaining < 24 * 3600 * 1000) return "soon";
  return "valid";
}

function keyStatus(item) {
  const state = keyState(item);
  if (state === "revoked") return '<span class="badge bad">已吊销</span>';
  if (state === "expired") return '<span class="badge bad">已过期</span>';
  if (state === "soon") return '<span class="badge warn">即将过期</span>';
  return '<span class="badge ok">有效</span>';
}


function openKeyDetail(item, ctx) {
  openDrawer({
    title: item.name || item.kid,
    description: "推流密钥元数据；secret 仅在签发时显示一次。",
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong>${keyStatus(item)}</strong></div>
        <div class="detail-item"><span>Key ID</span><strong class="mono">${esc(item.kid)}</strong></div>
        <div class="detail-item"><span>角色</span><strong>${esc((item.roles || []).join(", ") || "—")}</strong></div>
        <div class="detail-item"><span>协议</span><strong>${esc((item.protocols || []).join(", ") || "全部")}</strong></div>
        <div class="detail-item"><span>路径范围</span><strong class="mono">${esc((item.paths || []).join(", ") || "全部")}</strong></div>
        <div class="detail-item"><span>最大连接</span><strong class="mono">${item.max_connections || "∞"}</strong></div>
        <div class="detail-item"><span>创建</span><strong>${esc(dateTime(item.created_at))}</strong></div>
        <div class="detail-item"><span>过期</span><strong>${esc(dateTime(item.expires_at))}</strong></div>
      </div>
      <div class="actions"><button class="btn small ghost" data-key-copy>复制 Key ID</button><button class="btn small danger" data-key-revoke ${item.enabled ? "" : "disabled"}>吊销</button></div>
    </div>`,
    onMount(root) {
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      root.querySelector("[data-key-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(item.kid); toast("Key ID 已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-key-revoke]").onclick = async () => {
        if (!await confirmDialog({ title: "吊销密钥", message: `吊销 ${item.kid}？`, confirmLabel: "吊销", danger: true })) return;
        try { await apiV1("/keys/" + encodeURIComponent(item.kid), { method: "DELETE" }); toast("密钥已吊销", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
    },
  });
}
