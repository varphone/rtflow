import { api } from "../api.js";
import { dateTime, empty, esc, iconSvg } from "../format.js";
import { openDrawer, toast } from "../ui.js";

const PAGE_SIZE = 25;
const labels = {
  "path.create": "创建路径", "path.update": "更新路径", "path.delete": "删除路径", "path.kick": "断开路径",
  "key.issue": "签发密钥", "key.revoke": "吊销密钥",
  "proxy.upsert": "保存代理", "proxy.delete": "删除代理", "proxy.restart": "重启代理",
  "srt.upsert": "保存 SRT", "srt.delete": "删除 SRT",
  "vod.refresh": "刷新点播缓存",
  "record.start": "开始录制", "record.stop": "停止录制",
  "auth.deny": "认证拒绝", "auth.allow": "认证通过",
};
const detail = (value) => value == null || value === "" ? "—" : String(value);
const resultBadge = (result) => {
  const cls = result === "ok" ? "ok" : result === "denied" || result === "failed" ? "bad" : "warn";
  return `<span class="badge ${cls}">${esc(result || "—")}</span>`;
};

export async function render() {
  const payload = await api("/audit?limit=300");
  const events = payload.events || [];
  return {
    title: "审计日志",
    subtitle: "谁在何时改了什么；配置变更、流控制与认证拒绝。",
    actions: `<button class="btn small ghost" data-audit-auto>自动刷新：开</button><button class="btn small ghost" data-audit-export>导出 CSV</button><button class="btn small" data-audit-refresh>刷新</button>`,
    html: `<div class="toolbar"><div class="search">${iconSvg("search")}<input class="input" data-audit-search placeholder="搜索动作、路径、操作者或详情"></div>
      <div class="seg"><button data-audit-kind="all" class="active">全部</button><button data-audit-kind="change">变更</button><button data-audit-kind="auth">认证</button></div>
      <span class="meta" data-audit-count>${events.length} 条</span>
      <button class="btn small ghost" data-audit-new hidden></button></div>
      ${payload.enabled === false ? `<div class="error-banner">审计日志未启用：在服务配置中设置审计路径后重启。</div>` : ""}
      <section class="panel"><div class="table-wrap audit-table"><table class="data audit-table"><thead><tr><th>时间</th><th>动作</th><th>结果</th><th>对象</th><th>操作者</th><th>详情</th><th></th></tr></thead><tbody data-audit-rows></tbody></table></div>
        <div class="pager"><span class="meta" data-audit-range></span><div class="pager-buttons"><button class="btn small ghost" data-audit-prev>上一页</button><span class="meta" data-audit-page></span><button class="btn small ghost" data-audit-next>下一页</button></div></div>
      </section>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-audit-search]");
      const tbody = root.querySelector("[data-audit-rows]");
      const count = root.querySelector("[data-audit-count]");
      const range = root.querySelector("[data-audit-range]");
      const pageLabel = root.querySelector("[data-audit-page]");
      const prev = root.querySelector("[data-audit-prev]");
      const next = root.querySelector("[data-audit-next]");
      const auto = root.querySelector("[data-audit-auto]");
      const newButton = root.querySelector("[data-audit-new]");
      let current = events;
      let pending = null;
      let newCount = 0;
      let kind = "all";
      let page = 1;
      let automatic = true;
      let timer = null;
      const signature = (event) => `${event.ts}|${event.action}|${event.path || ""}|${event.key_id || ""}|${event.peer || ""}`;
      const filtered = () => {
        const q = search.value.trim().toLowerCase();
        return current.filter((event) => {
          if (kind === "auth" && !String(event.action).startsWith("auth.")) return false;
          if (kind === "change" && String(event.action).startsWith("auth.")) return false;
          return !q || `${event.action} ${event.path || ""} ${event.user || ""} ${event.detail || ""} ${event.peer || ""}`.toLowerCase().includes(q);
        });
      };
      const paint = () => {
        const items = filtered();
        const pages = Math.max(1, Math.ceil(items.length / PAGE_SIZE));
        page = Math.min(Math.max(1, page), pages);
        const start = (page - 1) * PAGE_SIZE;
        const slice = items.slice(start, start + PAGE_SIZE);
        count.textContent = `${items.length} 条`;
        range.textContent = items.length ? `${start + 1}–${start + slice.length} / ${items.length}` : "0 条";
        pageLabel.textContent = `${page} / ${pages}`;
        prev.disabled = page <= 1;
        next.disabled = page >= pages;
        tbody.innerHTML = slice.length ? rows(slice) : `<tr><td colspan="7">${empty("没有匹配的审计事件", "调整搜索条件或等待新的操作")}</td></tr>`;
        tbody.querySelectorAll("[data-audit-detail]").forEach((button, index) => {
          button.onclick = () => openAuditEvent(slice[index]);
        });
      };
      const showNew = () => {
        newButton.hidden = newCount === 0;
        newButton.textContent = `${newCount} 条新事件`;
      };
      const refresh = async () => {
        let next;
        try { next = (await api("/audit?limit=300")).events || []; } catch (_) { return; }
        if (page > 1) {
          const known = new Set(current.map(signature));
          newCount += next.filter((event) => !known.has(signature(event))).length;
          pending = next;
          showNew();
          return;
        }
        current = next;
        newCount = 0;
        pending = null;
        showNew();
        paint();
      };
      const setAuto = (enabled) => {
        automatic = enabled;
        auto.textContent = enabled ? "自动刷新：开" : "自动刷新：关";
        auto.classList.toggle("active", enabled);
        if (timer) { clearInterval(timer); timer = null; }
        if (enabled) timer = setInterval(() => { if (!document.hidden) refresh(); }, 5000);
      };
      newButton.onclick = () => {
        if (pending) current = pending;
        pending = null;
        newCount = 0;
        page = 1;
        showNew();
        paint();
      };
      search.oninput = () => { page = 1; paint(); };
      root.querySelectorAll("[data-audit-kind]").forEach((button) => button.onclick = () => {
        kind = button.dataset.auditKind;
        page = 1;
        root.querySelectorAll("[data-audit-kind]").forEach((item) => item.classList.toggle("active", item === button));
        paint();
      });
      prev.onclick = () => { page -= 1; paint(); };
      next.onclick = () => { page += 1; paint(); };
      auto.onclick = () => setAuto(!automatic);
      root.querySelector("[data-audit-refresh]").onclick = () => refresh();
      root.querySelector("[data-audit-export]").onclick = () => exportCsv(filtered());
      paint();
      setAuto(true);
      return () => { if (timer) clearInterval(timer); };
    },
  };
}

function rows(events) {
  return events.map((event) => `<tr>
    <td class="mono cell-sub">${esc(dateTime(event.ts))}</td>
    <td><div class="cell-main">${esc(labels[event.action] || event.action)}</div><div class="cell-sub mono">${esc(event.action)}</div></td>
    <td>${resultBadge(event.result)}</td>
    <td><div class="cell-main mono">${esc(detail(event.path || event.key_id || event.detail))}</div><div class="cell-sub">${esc(event.protocol || "")}</div></td>
    <td><div class="cell-main">${esc(detail(event.user || event.role))}</div><div class="cell-sub mono">${esc(event.peer || "")}</div></td>
    <td class="cell-sub">${esc(detail(event.detail))}</td>
    <td><button class="btn small ghost" data-audit-detail>详情</button></td>
  </tr>`).join("");
}


function openAuditEvent(event) {
  const links = [];
  if (event.path) {
    links.push([`查看路径`, `#/config/paths?q=${encodeURIComponent(event.path)}`]);
    links.push([`查看直播`, `#/live?q=${encodeURIComponent(event.path)}`]);
  }
  if (String(event.action).startsWith("proxy.") && event.path) links.push([`查看代理`, `#/config/proxies?q=${encodeURIComponent(event.path)}`]);
  if (String(event.action).startsWith("record.") && event.path) links.push([`查看录制`, `#/config/recordings?q=${encodeURIComponent(event.path)}`]);
  if (String(event.action).startsWith("srt.") && event.detail) links.push([`查看 SRT`, `#/config/srt?q=${encodeURIComponent(event.detail)}`]);
  const fields = [
    ["时间", dateTime(event.ts)],
    ["动作", `${labels[event.action] || event.action} (${event.action})`],
    ["结果", event.result || "—"],
    ["用户", event.user || event.role || "—"],
    ["协议", event.protocol || "—"],
    ["路径", event.path || "—"],
    ["Key ID", event.key_id || "—"],
    ["来源", event.peer || "—"],
    ["详情", event.detail || "—"],
  ];
  openDrawer({
    title: "审计事件",
    description: `${labels[event.action] || event.action} · ${dateTime(event.ts)}`,
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">${fields.map(([label, value]) => `<div class="detail-item"><span>${esc(label)}</span><strong>${esc(value)}</strong></div>`).join("")}</div>
      ${links.length ? `<section class="drawer-section"><h3>关联资源</h3><div class="actions">${links.map(([label, href]) => `<a class="btn small" href="${href}" data-event-goto>${esc(label)}</a>`).join("")}</div></section>` : ""}
      <section class="drawer-section"><div style="display:flex;align-items:center;gap:8px"><h3 style="margin:0">原始记录</h3><button class="btn small ghost" data-audit-copy style="margin-left:auto">复制 JSON</button></div><pre class="code">${esc(JSON.stringify(event, null, 2))}</pre></section>
    </div>`,
    onMount(root) {
      root.querySelectorAll("[data-event-goto]").forEach((link) => link.onclick = () => {
        root.querySelector("[data-close-drawer]")?.click();
      });
      root.querySelector("[data-audit-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(JSON.stringify(event, null, 2)); toast("JSON 已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
    },
  });
}


function exportCsv(events) {
  if (!events.length) return;
  const quote = (value) => `"${String(value ?? "").replace(/"/g, '""')}"`;
  const header = ["time", "action", "result", "user", "role", "protocol", "path", "key_id", "peer", "detail"];
  const lines = [header.join(",")];
  for (const event of events) {
    const time = event.ts ? new Date(Number(event.ts) * 1000).toISOString() : "";
    lines.push([time, event.action, event.result, event.user, event.role, event.protocol, event.path, event.key_id, event.peer, event.detail].map(quote).join(","));
  }
  const blob = new Blob(["\uFEFF" + lines.join("\r\n")], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `audit-${new Date().toISOString().slice(0, 10)}.csv`;
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
  toast(`已导出 ${events.length} 条`, "ok");
}
