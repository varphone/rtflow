import { api, apiV1 } from "../api.js";
import { bytes, dateTime, esc, iconSvg, num, uptime } from "../format.js";

export async function render() {
  const [overview, streamsPayload, proxies, pulls, pushes, recordings, system, audit] = await Promise.all([
    api("/overview"), api("/streams"), apiV1("/proxies"), apiV1("/srt/pulls"), apiV1("/srt/pushes"), api("/recordings"), api("/system"), api("/audit?limit=100"),
  ]);
  const streams = streamsPayload.items || streamsPayload;
  const attention = [];
  for (const proxy of proxies.proxies || proxies || []) {
    if (proxy.state === "backoff") attention.push({ kind: "代理", name: proxy.path, detail: proxy.last_error || "上游不可达", level: "bad", href: `#/config/proxies?q=${encodeURIComponent(proxy.path)}` });
  }
  for (const endpoint of [...(pulls.items || []), ...(pushes.items || [])]) {
    if (endpoint.enabled && !endpoint.connected) attention.push({ kind: "SRT", name: endpoint.name || endpoint.id, detail: endpoint.last_error || "未连接", level: "warn", href: `#/config/srt?q=${encodeURIComponent(endpoint.name || endpoint.id)}` });
  }
  for (const rec of recordings) {
    if (rec.last_error) attention.push({ kind: "录制", name: rec.path, detail: rec.last_error, level: "warn", href: `#/config/recordings?q=${encodeURIComponent(rec.path)}` });
  }
  const active = streams.filter((stream) => stream.publisher).slice(0, 6);
  const changes = (audit.events || []).filter((event) => !String(event.action).startsWith("auth.")).slice(0, 4);
  const actionNames = { "path.create": "创建路径", "path.update": "更新路径", "path.delete": "删除路径", "path.kick": "断开路径", "key.issue": "签发密钥", "key.revoke": "吊销密钥", "proxy.upsert": "保存代理", "proxy.delete": "删除代理", "proxy.restart": "重启代理", "srt.upsert": "保存 SRT", "srt.delete": "删除 SRT", "vod.refresh": "刷新点播缓存", "record.start": "开始录制", "record.stop": "停止录制" };
  return {
    title: "概览",
    subtitle: "运行状态、异常与活跃流。",
    actions: `<button class="btn small" data-overview-refresh>刷新</button>`,
    html: `
      <div class="stat-grid">
        <div class="stat"><div class="label">在线流</div><div class="value">${num(overview.mounts)}</div><div class="hint">${num(overview.publishers)} 个发布中</div></div>
        <div class="stat"><div class="label">观看连接</div><div class="value">${num(overview.subscribers)}</div><div class="hint">RTP ${bytes(overview.rtp_octets)}</div></div>
        <div class="stat"><div class="label">代理 streaming</div><div class="value">${num((proxies.proxies || proxies || []).filter((item) => item.state === "streaming").length)}</div><div class="hint">共 ${num((proxies.proxies || proxies || []).length)} 个代理</div></div>
        <div class="stat"><div class="label">需要关注</div><div class="value">${num(attention.length)}</div><div class="hint">代理 / SRT / 录制异常</div></div>
      </div>
      <div class="quick-actions">
        <a class="quick-action" href="#/config/paths"><span class="kicker">配置</span><strong>新建路径</strong><small>注册路径与发布策略</small></a>
        <a class="quick-action" href="#/config/keys"><span class="kicker">安全</span><strong>签发密钥</strong><small>一次性 secret 与权限范围</small></a>
        <a class="quick-action" href="#/config/proxies"><span class="kicker">回源</span><strong>新增代理</strong><small>拉取 RTSP/RTSPS 上游</small></a>
        <a class="quick-action" href="#/system"><span class="kicker">诊断</span><strong>服务与存储</strong><small>TOML、DB schema、版本</small></a>
      </div>
      <div class="two-col">
        <section class="panel">
          <div class="panel-head"><h2>活跃流</h2><span class="sub">${active.length} 条</span><div style="margin-left:auto"><a class="btn small ghost" href="#/live">查看全部</a></div></div>
          ${active.length ? `<div class="table-wrap"><table class="data"><thead><tr><th>流</th><th>编码</th><th>订阅</th><th>接收 / 丢弃</th></tr></thead><tbody>
            ${active.map((stream) => `<tr><td><a class="link-cell" href="#/live?q=${encodeURIComponent(stream.path)}">${esc(stream.name)}</a><div class="cell-sub mono">${esc(stream.path)}</div></td>
              <td><span class="badge codec">${esc(stream.video?.codec || "—")}</span></td><td class="mono">${num(stream.subscribers)}</td>
              <td class="mono"><span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span></td></tr>`).join("")}
          </tbody></table></div>` : `<div class="empty">${iconSvg("inbox")}<h3>暂无直播流</h3>
            <div class="faint">使用 RTMP / RTSP / SRT 推流后出现在这里；也可以先注册一条路径。</div>
            <div class="actions" style="justify-content:center;margin-top:14px"><a class="btn small" href="#/config/paths">新建路径</a>
              <button class="btn small ghost" data-copy-push>复制推流地址</button></div>
            <pre class="code" style="text-align:left;max-width:520px;margin:14px auto 0">${esc(system.rtmp_base || `rtmp://${location.hostname}:1935`)}/live/your-stream</pre>
          </div>`}
        </section>
        <div class="stack">
          <section class="panel"><div class="panel-head"><h2>需要关注</h2><span class="sub">${attention.length} 项</span></div>
            <div class="panel-body">${attention.length ? `<div class="stack" style="gap:8px">${attention.slice(0, 8).map((item) => `
              <div class="attention-item">
                <div><div style="display:flex;gap:7px;align-items:center"><span class="badge ${item.level}">${esc(item.kind)}</span><b>${esc(item.name)}</b></div>
                <div class="cell-sub">${esc(item.detail)}</div></div>
                <a class="btn small ghost" href="${item.href}">查看</a></div>`).join("")}</div>` : `<div class="note">当前没有需要处理的异常。</div>`}</div>
          </section>
          <section class="panel"><div class="panel-head"><h2>运行</h2></div><div class="panel-body">
            <dl class="kv"><dt>运行时长</dt><dd>${esc(uptime(overview.uptime_seconds))}</dd>
            <dt>点播资源</dt><dd>${num(overview.vods)}</dd><dt>接收帧</dt><dd class="mono">${num(overview.accepted_frames)}</dd>
            <dt>RTP 包</dt><dd class="mono">${num(overview.rtp_packets)}</dd></dl>
          </div></section>
          <section class="panel"><div class="panel-head"><h2>最近变更</h2><span class="sub">${changes.length} 项</span><div style="margin-left:auto"><a class="btn small ghost" href="#/audit">审计</a></div></div>
            <div class="panel-body">${changes.length ? `<div class="stack" style="gap:6px">${changes.map((event) => `
              <div class="event-row"><span class="badge">${esc(actionNames[event.action] || event.action)}</span><div class="event-main"><b>${esc(event.path || event.key_id || event.detail || "—")}</b><span class="cell-sub">${esc(event.user || event.role || "admin")} · ${esc(dateTime(event.ts))}</span></div></div>`).join("")}</div>` : `<div class="note">暂无管理变更。</div>`}</div>
          </section>

        </div>
      </div>`,
    mount(root, ctx) {
      root.querySelector("[data-overview-refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-copy-push]")?.addEventListener("click", async () => {
        const value = `${system.rtmp_base || `rtmp://${location.hostname}:1935`}/live/your-stream`;
        try { await navigator.clipboard.writeText(value); ctx.toast("推流地址已复制", "ok"); }
        catch (_) { ctx.toast("复制失败", "bad"); }
      });
    },
  };
}
