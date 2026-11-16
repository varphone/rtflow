import { api } from "../api.js";
import { dateTime, empty, esc, iconSvg } from "../format.js";

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
    actions: `<button class="btn small" data-audit-refresh>刷新</button>`,
    html: `<div class="toolbar"><div class="search">${iconSvg("search")}<input class="input" data-audit-search placeholder="搜索动作、路径、操作者或详情"></div>
      <div class="seg"><button data-audit-kind="all" class="active">全部</button><button data-audit-kind="change">变更</button><button data-audit-kind="auth">认证</button></div>
      <span class="meta" data-audit-count>${events.length} 条</span></div>
      ${payload.enabled === false ? `<div class="error-banner">审计日志未启用：在服务配置中设置审计路径后重启。</div>` : ""}
      <section class="panel"><div class="table-wrap"><table class="data"><thead><tr><th>时间</th><th>动作</th><th>结果</th><th>对象</th><th>操作者</th><th>详情</th></tr></thead><tbody data-audit-rows>${rows(events)}</tbody></table></div></section>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-audit-search]");
      const tbody = root.querySelector("[data-audit-rows]");
      const count = root.querySelector("[data-audit-count]");
      let kind = "all";
      const paint = () => {
        const q = search.value.trim().toLowerCase();
        const filtered = events.filter((event) => {
          if (kind === "auth" && !String(event.action).startsWith("auth.")) return false;
          if (kind === "change" && String(event.action).startsWith("auth.")) return false;
          return !q || `${event.action} ${event.path || ""} ${event.user || ""} ${event.detail || ""} ${event.peer || ""}`.toLowerCase().includes(q);
        });
        count.textContent = `${filtered.length} 条`;
        tbody.innerHTML = filtered.length ? rows(filtered) : `<tr><td colspan="6">${empty("没有匹配的审计事件", "调整搜索条件或等待新的操作")}</td></tr>`;
      };
      search.oninput = paint;
      root.querySelectorAll("[data-audit-kind]").forEach((button) => button.onclick = () => {
        kind = button.dataset.auditKind;
        root.querySelectorAll("[data-audit-kind]").forEach((item) => item.classList.toggle("active", item === button));
        paint();
      });
      root.querySelector("[data-audit-refresh]").onclick = () => ctx.reload();
    },
  };
}

function rows(events) {
  if (!events.length) return `<tr><td colspan="6">${empty("暂无审计事件", "配置变更与认证决定会记录在这里")}</td></tr>`;
  return events.map((event) => `<tr>
    <td class="mono cell-sub">${esc(dateTime(event.ts))}</td>
    <td><div class="cell-main">${esc(labels[event.action] || event.action)}</div><div class="cell-sub mono">${esc(event.action)}</div></td>
    <td>${resultBadge(event.result)}</td>
    <td><div class="cell-main mono">${esc(detail(event.path || event.key_id || event.detail))}</div><div class="cell-sub">${esc(event.protocol || "")}</div></td>
    <td><div class="cell-main">${esc(detail(event.user || event.role))}</div><div class="cell-sub mono">${esc(event.peer || "")}</div></td>
    <td class="cell-sub">${esc(detail(event.detail))}</td>
  </tr>`).join("");
}
