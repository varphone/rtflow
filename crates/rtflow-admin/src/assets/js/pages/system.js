import { api, apiV1 } from "../api.js";
import { bytes, esc, uptime } from "../format.js";
import { toast } from "../ui.js";

export async function render() {
  const [system, config, storage] = await Promise.all([api("/system"), apiV1("/config"), apiV1("/storage")]);
  const features = (system.features || []).map((feature) => `<span class="badge">${esc(feature)}</span>`).join("");
  return {
    title: "系统",
    subtitle: "服务配置与存储诊断，均只读。",
    actions: "",
    html: `<div class="stack">
      <div class="two-col">
        <section class="panel"><div class="panel-head"><h2>服务配置</h2><span class="sub">TOML · 只读</span><div class="actions" style="margin-left:auto"><button class="btn small ghost" data-copy-config>复制</button><button class="btn small ghost" data-download-config>下载</button></div></div>
          <div class="panel-body"><p class="note">监听、TLS、日志与鉴权仅通过 <code>rtflow.toml</code> 修改；保存后 SIGHUP 或重启。</p>
          <pre class="code" style="max-height:420px">${esc(config.toml || "")}</pre></div></section>
        <div class="stack">
          <section class="panel"><div class="panel-head"><h2>运行</h2></div><div class="panel-body"><dl class="kv">
            <dt>版本</dt><dd class="mono">${esc(system.version || "—")}</dd><dt>运行时长</dt><dd data-system-uptime>${esc(uptime(system.uptime_seconds))}</dd>
            <dt>在线流</dt><dd class="mono" data-system-mounts>${Number(system.mounts || 0)}</dd><dt>控制台路径</dt><dd class="mono base-value"><span>${esc(system.base_path || "/admin")}</span><button class="copy-inline" data-copy-value="${esc(system.base_path || "/admin")}" aria-label="复制控制台路径" title="复制">⧉</button></dd>
            <dt>HLS 前缀</dt><dd class="mono base-value"><span>${esc(system.hls_prefix || "/hls")}</span><button class="copy-inline" data-copy-value="${esc(system.hls_prefix || "/hls")}" aria-label="复制 HLS 前缀" title="复制">⧉</button></dd>
            <dt>RTSP 基地址</dt><dd class="mono base-value"><span title="${esc(system.rtsp_base || "")}">${esc(system.rtsp_base || "—")}</span>${system.rtsp_base ? `<button class="copy-inline" data-copy-base="${esc(system.rtsp_base)}" aria-label="复制 RTSP 基地址" title="复制">⧉</button>` : ""}</dd>
            <dt>RTMP 基地址</dt><dd class="mono base-value"><span title="${esc(system.rtmp_base || "")}">${esc(system.rtmp_base || "—")}</span>${system.rtmp_base ? `<button class="copy-inline" data-copy-base="${esc(system.rtmp_base)}" aria-label="复制 RTMP 基地址" title="复制">⧉</button>` : ""}</dd></dl></div></section>
          <section class="panel"><div class="panel-head"><h2>存储</h2></div><div class="panel-body"><dl class="kv">
            <dt>Schema</dt><dd class="mono">v${Number(storage.schema_version || 0)}</dd><dt>数据库大小</dt><dd class="mono">${bytes(storage.bytes)}</dd>
            <dt>备份</dt><dd class="mono">rtflow-server --db-backup FILE</dd></dl>
            <div class="actions" style="margin-top:10px"><button class="btn small ghost" data-copy-backup>复制备份命令</button></div></div></section>
          <section class="panel"><div class="panel-head"><h2>构建特性</h2></div><div class="panel-body"><div class="feature-list">${features || '<span class="faint">无</span>'}</div></div></section>
        </div>
      </div>
      <section class="panel"><div class="panel-head"><h2>安全</h2></div><div class="panel-body">
        <div class="security-grid">
          <div><span>会话 Cookie</span><b class="badge ok">HttpOnly · SameSite=Strict</b></div>
          <div><span>CSRF</span><b class="badge ok">写操作校验</b></div>
          <div><span>控制台模式</span><b class="badge ${system.readonly ? "" : "ok"}">${system.readonly ? "只读" : "可写"}</b></div>
          <div><span>传输</span><b class="badge warn">建议 HTTPS 反向代理</b></div>
        </div>
        <p class="note" style="margin:12px 0 0">生产环境建议启用 HTTPS，并设置 <code>secure_cookies</code>；管理员密码与 TLS 证书仅通过 <code>rtflow.toml</code> 修改。</p>
      </div></section>
    </div>`,
    mount(root) {
      const configText = config.toml || "";
      const uptimeNode = root.querySelector("[data-system-uptime]");
      const mountsNode = root.querySelector("[data-system-mounts]");
      let startedAt = Date.now();
      let baseUptime = Number(system.uptime_seconds || 0);
      const tick = () => {
        if (document.hidden) return;
        if (uptimeNode) uptimeNode.textContent = uptime(baseUptime + (Date.now() - startedAt) / 1000);
      };
      const timer = setInterval(async () => {
        tick();
        if (document.hidden) return;
        try {
          const latest = await api("/system");
          if (mountsNode) mountsNode.textContent = String(Number(latest.mounts || 0));
          if (latest.uptime_seconds !== undefined) {
            baseUptime = Number(latest.uptime_seconds || 0);
            startedAt = Date.now();
          }
        } catch (_) { /* keep local uptime */ }
      }, 15000);
      root.querySelector("[data-copy-config]").onclick = async () => {
        try { await navigator.clipboard.writeText(configText); toast("配置已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelectorAll("[data-copy-base]").forEach((button) => button.onclick = async () => {
        try { await navigator.clipboard.writeText(button.dataset.copyBase); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      });
      root.querySelectorAll("[data-copy-value]").forEach((button) => button.onclick = async () => {
        try { await navigator.clipboard.writeText(button.dataset.copyValue); toast("已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      });
      root.querySelector("[data-copy-backup]").onclick = async () => {
        try { await navigator.clipboard.writeText("rtflow-server --db-backup backup.db"); toast("命令已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-download-config]").onclick = () => {
        const url = URL.createObjectURL(new Blob([configText], { type: "text/plain;charset=utf-8" }));
        const link = document.createElement("a");
        link.href = url;
        link.download = "rtflow.toml";
        document.body.appendChild(link);
        link.click();
        link.remove();
        URL.revokeObjectURL(url);
        toast("配置已下载", "ok");
      };
      return () => clearInterval(timer);
    },
  };
}
