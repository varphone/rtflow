import { openDrawer, field, textInput, toast } from "../ui.js";
import { uniqueName } from "../format.js";
import { apiV1 } from "../api.js";

export function openSrtDrawer(direction, existing, onSaved, options = {}) {
  const clone = options.clone === true && Boolean(existing);
  const source = existing || { name: "", enabled: true, remote: "", stream_id: "", mount: direction === "push" ? "/live/cam" : "", pbkeylen: 0, latency_ms: null };
  const item = clone ? {
    ...source,
    name: uniqueName(`${source.name || ""}-copy`, options.taken || []),
    mount: source.mount ? uniqueName(`${source.mount}-copy`, options.takenMounts || []) : "",
  } : source;
  const editing = Boolean(existing) && !clone;
  const route = direction === "push" ? "pushes" : "pulls";
  openDrawer({
    title: clone ? "复制 SRT 端点" : (existing ? "编辑 SRT 端点" : (direction === "push" ? "新增 SRT 推流" : "新增 SRT 拉流")),
    description: direction === "push" ? (clone ? "复制 SRT 推流端点；名称与 mount 不能与现有端点重复。" : "把本地 mount 推送到远端 SRT listener。") : (clone ? "复制 SRT 拉流端点；名称与 mount 不能与现有端点重复。" : "从远端 SRT listener 拉流并注册 mount。"),
    body: `
      <div class="form-section">
        <h3>连接</h3>
        ${field("名称", textInput("name", item.name || "", "placeholder=\"可选\""))}
        ${field("远端 host:port", textInput("remote", item.remote || "", `required placeholder="192.0.2.20:9000"`))}
        ${field("Stream ID", textInput("stream_id", item.stream_id || ""))}
        ${field(direction === "push" ? "源 mount" : "目标 mount", textInput("mount", item.mount || "", `placeholder="${direction === "push" ? "/live/cam" : "默认由 Stream ID 推导"}"`))}
      </div>
      <div class="form-section">
        <h3>加密与选项</h3>
        ${field("Passphrase", `<input class="input" type="password" name="passphrase" placeholder="${editing ? "留空保持原值" : "留空不加密，10..80 字节"}">`)}
        ${field("密钥长度", `<select class="input" name="pbkeylen">
          <option value="0" ${item.pbkeylen === 0 ? "selected" : ""}>自动 / 不设置</option>
          <option value="16" ${item.pbkeylen === 16 ? "selected" : ""}>16</option>
          <option value="24" ${item.pbkeylen === 24 ? "selected" : ""}>24</option>
          <option value="32" ${item.pbkeylen === 32 ? "selected" : ""}>32</option>
        </select>`)}
        ${field("Latency (ms)", `<input class="input" type="number" min="0" name="latency_ms" value="${item.latency_ms || ""}">`)}
        <label class="checkline"><input type="checkbox" name="enabled" ${item.enabled ? "checked" : ""}> 启用</label>
      </div>`,
    submitLabel: editing ? "保存修改" : (clone ? "创建副本" : "创建端点"),
    async onSubmit(data, { close }) {
      const body = {
        name: data.get("name") || "",
        enabled: data.get("enabled") === "on",
        remote: data.get("remote"),
        stream_id: data.get("stream_id") || null,
        mount: data.get("mount") || null,
        pbkeylen: Number(data.get("pbkeylen") || 0),
        latency_ms: Number(data.get("latency_ms") || 0) || null,
      };
      const passphrase = data.get("passphrase");
      if (passphrase || !editing) body.passphrase = passphrase || null;
      await apiV1("/srt/" + route + (editing ? "/" + encodeURIComponent(item.id) : ""),
        { method: editing ? "PATCH" : "POST", body });
      close();
      toast(editing ? "SRT 端点已保存" : "SRT 端点已创建", "ok");
      onSaved();
    },
    onMount(root) { void root; },
  });
}
