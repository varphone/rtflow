import { openDrawer, field, textInput, toast } from "../ui.js";
import { apiV1 } from "../api.js";
import { esc } from "../format.js";

function upstreamRow(upstream = {}, index = 0) {
  return `<div class="form-section" data-upstream-row>
    <div style="display:flex;align-items:center;gap:8px"><h3 style="margin:0">上游 ${index + 1}</h3>
      <button type="button" class="btn ghost small" data-remove-upstream style="margin-left:auto">移除</button></div>
    ${field("RTSP URL", textInput("url", upstream.url || "", `required placeholder="rtsp://192.0.2.10/live"`))}
    <div class="field-row">
      ${field("传输", `<select class="input" name="transport">
        <option value="tcp" ${upstream.transport === "tcp" ? "selected" : ""}>TCP</option>
        <option value="udp" ${upstream.transport === "udp" ? "selected" : ""}>UDP</option>
        <option value="auto" ${upstream.transport === "auto" ? "selected" : ""}>Auto</option>
      </select>`)}
      ${field("用户名", textInput("username", upstream.username || ""))}
    </div>
    ${field("密码", `<input class="input" type="password" name="password" placeholder="${upstream.password ? "留空保持不变" : ""}">`)}
  </div>`;
}

export function openProxyDrawer(existing, onSaved, options = {}) {
  const clone = options.clone === true && Boolean(existing);
  const source = existing || { path: "", mode: "on-demand", idle_grace_ms: null, upstreams: [{ url: "", transport: "tcp" }] };
  const mount = clone ? { ...source, path: `${source.path}-copy` } : source;
  const editing = Boolean(existing) && !clone;
  const upstreams = mount.upstreams?.length ? mount.upstreams : [{ url: "", transport: "tcp" }];
  openDrawer({
    title: clone ? "复制 RTSP 代理" : (existing ? "编辑 RTSP 代理" : "新增 RTSP 代理"),
    description: clone ? "复制上游与模式；路径不能与现有代理重复。" : "上游按顺序尝试；密码只写不读。",
    body: `
      <div class="form-section">
        <h3>代理</h3>
        ${field("路径", textInput("path", mount.path, `required ${editing ? "disabled" : ""} placeholder="/cameras/front"`))}
        <div class="field-row">
          ${field("模式", `<select class="input" name="mode">
            <option value="on-demand" ${mount.mode === "on-demand" ? "selected" : ""}>按需</option>
            <option value="always" ${mount.mode === "always" ? "selected" : ""}>常连</option>
          </select>`)}
          ${field("空闲断开", textInput("idle_grace", mount.idle_grace_ms ? Math.round(mount.idle_grace_ms / 1000) + "s" : "", "placeholder=\"5s\""))}
        </div>
      </div>
      <div id="upstreams">${upstreams.map((upstream, index) => upstreamRow(upstream, index)).join("")}</div>
      <button type="button" class="btn small" data-add-upstream>添加上游</button>`,
    submitLabel: editing ? "保存修改" : (clone ? "创建副本" : "创建代理"),
    async onSubmit(data, { close }) {
      const rows = [...document.querySelectorAll("#upstreams [data-upstream-row]")];
      const urls = rows.map((row) => ({
        url: row.querySelector("[name=url]").value,
        transport: row.querySelector("[name=transport]").value,
        username: row.querySelector("[name=username]").value || null,
        password: row.querySelector("[name=password]").value || null,
      }));
      const body = {
        path: editing ? mount.path : data.get("path"),
        enabled: true,
        mode: data.get("mode"),
        idle_grace: data.get("idle_grace") || null,
        upstreams: urls,
      };
      await apiV1("/proxies", { method: editing ? "PATCH" : "POST", body });
      close();
      toast(editing ? "代理已保存" : "代理已创建", "ok");
      onSaved();
    },
    onMount(root) {
      root.querySelector("[data-add-upstream]").onclick = () => {
        const container = root.querySelector("#upstreams");
        container.insertAdjacentHTML("beforeend", upstreamRow({}, container.children.length));
      };
      root.addEventListener("click", (event) => {
        if (event.target.closest("[data-remove-upstream]")) {
          if (root.querySelectorAll("[data-upstream-row]").length > 1) event.target.closest("[data-upstream-row]").remove();
        }
      });
    },
  });
}
