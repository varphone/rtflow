import { esc } from "./format.js";
import { icon } from "./icons.js";

const modalRoot = () => document.getElementById("modal-root");
const toastRoot = () => document.getElementById("toast-root");

export function toast(message, kind = "") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.setAttribute("role", "status");
  el.setAttribute("aria-live", "polite");
  el.textContent = message;
  const root = toastRoot();
  while (root.children.length >= 3) root.firstElementChild.remove();
  root.appendChild(el);
  let timer = setTimeout(() => el.remove(), 3600);
  el.addEventListener("mouseenter", () => clearTimeout(timer));
  el.addEventListener("mouseleave", () => { timer = setTimeout(() => el.remove(), 1200); });
  el.addEventListener("click", () => el.remove());
}

export async function copyText(text) {
  const value = String(text ?? "");
  if (navigator.clipboard?.writeText && window.isSecureContext) {
    try { await navigator.clipboard.writeText(value); return; } catch (_) {}
  }
  const active = document.activeElement;
  const area = document.createElement("textarea");
  area.value = value;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.top = "-1000px";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.focus();
  area.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (_) { ok = false; }
  area.remove();
  if (active instanceof HTMLElement) active.focus();
  if (!ok) throw new Error("复制失败");
}

function focusFirst(root) {
  const target = root.querySelector("[autofocus]") || root.querySelector("input, select, textarea")
    || root.querySelector(".drawer-body, .modal-body")
    || root.querySelector("button, [tabindex]");
  if (target) {
    if (!target.hasAttribute("tabindex")) target.tabIndex = -1;
    target.focus();
  }
}

let dialogEscape = null;
let dialogOnClose = null;
export function openDialog({ title, body, footer = "", onMount, onClose }) {
  closeDialog();
  dialogOnClose = onClose || null;
  modalRoot().innerHTML = `<div class="modal-backdrop" data-dialog-backdrop>
    <div class="modal" role="dialog" aria-modal="true" aria-label="${esc(title)}">
      <div class="modal-head"><h2>${esc(title)}</h2><button class="btn ghost small" data-close-dialog aria-label="关闭">${icon("close")}</button></div>
      <div class="modal-body">${body}</div>
      ${footer ? `<div class="modal-foot">${footer}</div>` : ""}
    </div></div>`;
  const root = modalRoot();
  root.querySelectorAll("[data-close-dialog]").forEach((button) => button.addEventListener("click", closeDialog));
  root.querySelector("[data-dialog-backdrop]")?.addEventListener("click", (event) => {
    if (event.target.dataset.dialogBackdrop !== undefined) closeDialog();
  });
  dialogEscape = (event) => { if (event.key === "Escape") { event.preventDefault(); closeDialog(); } };
  document.addEventListener("keydown", dialogEscape);
  root.addEventListener("keydown", (event) => trapFocus(root, event));
  focusFirst(root);
  onMount?.(root);
}

export function closeDialog() {
  if (dialogEscape) { document.removeEventListener("keydown", dialogEscape); dialogEscape = null; }
  if (dialogOnClose) { try { dialogOnClose(); } catch (_) {} dialogOnClose = null; }
  modalRoot().innerHTML = "";
}

export function confirmDialog({ title, message, confirmLabel = "确认", danger = false }) {
  return new Promise((resolve) => {
    openDialog({
      title,
      body: `<p class="note">${esc(message)}</p>`,
      footer: `<button class="btn" data-cancel autofocus>取消</button><button class="btn ${danger ? "danger" : "primary"}" data-confirm>${esc(confirmLabel)}</button>`,
      onMount(root) {
        root.querySelector("[data-cancel]").onclick = () => { closeDialog(); resolve(false); };
        root.querySelector("[data-confirm]").onclick = () => { closeDialog(); resolve(true); };
      },
    });
  });
}

export function secretDialog(title, secret) {
  openDialog({
    title,
    body: `<p class="note">请立即保存，关闭后无法再次查看。</p><pre class="code" id="secret-value">${esc(secret)}</pre>`,
    footer: `<button class="btn primary" data-copy-secret>复制并关闭</button>`,
    onMount(root) {
      root.querySelector("[data-copy-secret]").onclick = async () => {
        try { await navigator.clipboard.writeText(secret); } catch (_) {}
        closeDialog();
      };
    },
  });
}

export function openCommandPalette(commands) {
  const root = modalRoot();
  root.innerHTML = `<div class="modal-backdrop" data-palette-backdrop>
    <div class="command-palette" role="dialog" aria-modal="true" aria-label="命令面板">
      <div class="command-input"><span class="icon"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/></svg></span>
        <input id="command-search" class="input" placeholder="搜索页面、流、点播或路径…" autocomplete="off" spellcheck="false"></div>
      <div class="command-list" id="command-list"></div>
      <div class="command-foot"><span>↑↓ 选择</span><span>Enter 打开</span><span>Esc 关闭</span></div>
    </div></div>`;
  const input = root.querySelector("#command-search");
  const list = root.querySelector("#command-list");
  let all = commands;
  let filtered = commands;
  let selected = 0;
  let closed = false;
  const close = () => { closed = true; root.innerHTML = ""; };
  const run = (command) => { close(); command.run?.(); };
  const paint = () => {
    if (!filtered.length) { list.innerHTML = `<div class="command-empty">没有匹配项</div>`; return; }
    selected = Math.max(0, Math.min(selected, filtered.length - 1));
    let group = "";
    list.innerHTML = filtered.map((command, index) => {
      const heading = command.group && command.group !== group ? `<div class="command-group">${esc(command.group)}</div>` : "";
      group = command.group || group;
      return `${heading}<button class="command-item ${index === selected ? "active" : ""}" data-command="${index}">
        <span class="command-icon">${command.icon || "›"}</span>
        <span><strong>${esc(command.label)}</strong>${command.hint ? `<small>${esc(command.hint)}</small>` : ""}</span>
        ${command.shortcut ? `<span class="command-shortcut">${esc(command.shortcut)}</span>` : ""}
      </button>`;
    }).join("");
    list.querySelectorAll("[data-command]").forEach((button) => {
      button.onmouseenter = () => { selected = Number(button.dataset.command); paint(); };
      button.onclick = () => run(filtered[Number(button.dataset.command)]);
    });
  };
  const filter = () => {
    const query = input.value.trim().toLowerCase();
    filtered = !query ? all : all.filter((command) =>
      `${command.label} ${command.hint || ""} ${command.keywords || ""}`.toLowerCase().includes(query));
    selected = 0; paint();
  };
  input.oninput = filter;
  input.onkeydown = (event) => {
    if (event.key === "ArrowDown") { event.preventDefault(); selected = Math.min(selected + 1, filtered.length - 1); paint(); }
    else if (event.key === "ArrowUp") { event.preventDefault(); selected = Math.max(selected - 1, 0); paint(); }
    else if (event.key === "Enter" && filtered[selected]) { event.preventDefault(); run(filtered[selected]); }
    else if (event.key === "Escape") { event.preventDefault(); close(); }
  };
  root.querySelector("[data-palette-backdrop]").addEventListener("click", (event) => {
    if (event.target.dataset.paletteBackdrop !== undefined) close();
  });
  filter(); input.focus();
  return { setCommands(next) { if (closed) return; all = next; filter(); } };
}

export function openDrawer({ title, description = "", body, submitLabel = "保存", onSubmit, onMount, onClose, showSubmit = true }) {
  const root = modalRoot();
  root.innerHTML = `<div class="drawer-backdrop" data-drawer-backdrop>
    <aside class="drawer" role="dialog" aria-modal="true" aria-label="${esc(title)}">
      <div class="drawer-head"><div><h2>${esc(title)}</h2>${description ? `<p>${esc(description)}</p>` : ""}</div>
        <button class="btn ghost small" data-close-drawer aria-label="关闭" style="margin-left:auto">${icon("close")}</button></div>
      <form class="drawer-form" id="drawer-form" style="display:contents">
        <div class="drawer-body">${body}</div>
        <div class="drawer-foot"><button type="button" class="btn" data-close-drawer>${showSubmit ? "取消" : "关闭"}</button>${showSubmit ? `<button type="submit" class="btn primary">${esc(submitLabel)}</button>` : ""}</div>
      </form>
    </aside></div>`;
  let dirty = false;
  let closed = false;
  const forceClose = () => {
    if (closed) return;
    closed = true;
    onClose?.();
    root.innerHTML = "";
  };
  const close = () => {
    if (dirty && !window.confirm("有未保存的修改，确定放弃？")) return;
    forceClose();
  };
  const form = root.querySelector("#drawer-form");
  form.addEventListener("input", () => { dirty = true; }, { once: false });
  root.querySelectorAll("[data-close-drawer]").forEach((button) => button.onclick = close);
  root.querySelector("[data-drawer-backdrop]").addEventListener("click", (event) => {
    if (event.target.dataset.drawerBackdrop !== undefined) close();
  });
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector("button[type=submit]");
    button.disabled = true;
    button.classList.add("loading");
    button.setAttribute("aria-busy", "true");
    try {
      await onSubmit(new FormData(form), { close: forceClose });
    } catch (error) {
      let box = form.querySelector(".form-error");
      if (!box) { box = document.createElement("div"); box.className = "form-error"; box.setAttribute("role", "alert"); form.querySelector(".drawer-body").prepend(box); }
      box.textContent = error.message;
    } finally {
      button.disabled = false;
      button.classList.remove("loading");
      button.removeAttribute("aria-busy");
    }
  });
  onMount?.(root);
  root.addEventListener("keydown", (event) => trapFocus(root, event));
  focusFirst(root);
  document.addEventListener("keydown", function esc(event) {
    if (event.key === "Escape") { close(); document.removeEventListener("keydown", esc); }
  });
  return { close };
}

function trapFocus(root, event) {
  if (event.key !== "Tab") return;
  const nodes = [...root.querySelectorAll("button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex='-1'])")]
    .filter((node) => node.offsetParent !== null);
  if (!nodes.length) return;
  const first = nodes[0], last = nodes[nodes.length - 1];
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
}

export function syncSortAria(root, sort) {
  root.querySelectorAll("[data-sort]").forEach((button) => {
    const th = button.closest("th");
    if (!th) return;
    th.setAttribute("aria-sort", sort.field === button.dataset.sort
      ? (sort.dir > 0 ? "ascending" : "descending")
      : "none");
  });
}

let fieldSequence = 0;
export function field(label, input, { hint = "", className = "" } = {}) {
  const id = `field-${++fieldSequence}`;
  const tagged = input.replace(/^<(input|select|textarea)\b/, `<$1 id="${id}"`);
  return `<div class="field ${className}"><label for="${id}">${esc(label)}</label>${tagged}${hint ? `<span class="hint">${esc(hint)}</span>` : ""}</div>`;
}

export const checkbox = (name, label, checked = false) =>
  `<label class="checkline"><input type="checkbox" name="${name}" ${checked ? "checked" : ""}> ${esc(label)}</label>`;

export const textInput = (name, value = "", attrs = "") =>
  `<input class="input" name="${name}" value="${esc(value)}" ${attrs}>`;
