import { api, apiV1 } from "../api.js";
import { bytes, esc, uptime } from "../format.js";

export async function render() {
  const [system, config, storage] = await Promise.all([api("/system"), apiV1("/config"), apiV1("/storage")]);
  const features = (system.features || []).map((feature) => `<span class="badge">${esc(feature)}</span>`).join("");
  return {
    title: "系统",
    subtitle: "服务配置与存储诊断，均只读。",
    actions: "",
    html: `<div class="stack">
      <div class="two-col">
        <section class="panel"><div class="panel-head"><h2>服务配置</h2><span class="sub">TOML · 只读</span></div>
          <div class="panel-body"><p class="note">监听、TLS、日志与鉴权仅通过 <code>rtflow.toml</code> 修改；保存后 SIGHUP 或重启。</p>
          <pre class="code" style="max-height:420px">${esc(config.toml || "")}</pre></div></section>
        <div class="stack">
          <section class="panel"><div class="panel-head"><h2>运行</h2></div><div class="panel-body"><dl class="kv">
            <dt>版本</dt><dd class="mono">${esc(system.version || "—")}</dd><dt>运行时长</dt><dd>${esc(uptime(system.uptime_seconds))}</dd>
            <dt>在线流</dt><dd class="mono">${Number(system.mounts || 0)}</dd><dt>控制台路径</dt><dd class="mono">${esc(system.base_path || "/admin")}</dd>
            <dt>HLS 前缀</dt><dd class="mono">${esc(system.hls_prefix || "/hls")}</dd></dl></div></section>
          <section class="panel"><div class="panel-head"><h2>存储</h2></div><div class="panel-body"><dl class="kv">
            <dt>Schema</dt><dd class="mono">v${Number(storage.schema_version || 0)}</dd><dt>数据库大小</dt><dd class="mono">${bytes(storage.bytes)}</dd>
            <dt>备份</dt><dd class="mono">rtflow-server --db-backup FILE</dd></dl></div></section>
          <section class="panel"><div class="panel-head"><h2>构建特性</h2></div><div class="panel-body"><div class="feature-list">${features || '<span class="faint">无</span>'}</div></div></section>
        </div>
      </div>
      <section class="panel"><div class="panel-head"><h2>安全</h2></div><div class="panel-body note">
        HttpOnly + SameSite=Strict 会话；写操作要求 CSRF；生产建议 HTTPS 反向代理并设置 <code>secure_cookies</code>。
      </div></section>
    </div>`,
  };
}
