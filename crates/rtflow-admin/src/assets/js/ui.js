import { esc } from "./format.js";
import { icon } from "./icons.js";

const modalRoot = () => document.getElementById("modal-root");
const toastRoot = () => document.getElementById("toast-root");

export function toast(message, kind = "") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = message;
  toastRoot().appendChild(el);
  setTimeout(() => el.remove(), 3600);
}

function focusFirst(root) {
  const target = root.querySelector("button, input, select, textarea, [tabindex]");
  if (target) target.focus();
}

export function openDialog({ title, body, footer = "", onMount }) {
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
  document.addEventListener("keydown", escDialog, { once: true });
  focusFirst(root);
  onMount?.(root);
}

function escDialog(event) {
  if (event.key === "Escape") closeDialog();
  else document.addEventListener("keydown", escDialog, { once: true });
}

export function closeDialog() {
  modalRoot().innerHTML = "";
}

export function confirmDialog({ title, message, confirmLabel = "确认", danger = false }) {
  return new Promise((resolve) => {
    openDialog({
      title,
      body: `<p class="note">${esc(message)}</p>`,
      footer: `<button class="btn" data-cancel>取消</button><button class="btn ${danger ? "danger" : "primary"}" data-confirm>${esc(confirmLabel)}</button>`,
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

export function openDrawer({ title, description = "", body, submitLabel = "保存", onSubmit, onMount }) {
  const root = modalRoot();
  root.innerHTML = `<div class="drawer-backdrop" data-drawer-backdrop>
    <aside class="drawer" role="dialog" aria-modal="true" aria-label="${esc(title)}">
      <div class="drawer-head"><div><h2>${esc(title)}</h2>${description ? `<p>${esc(description)}</p>` : ""}</div>
        <button class="btn ghost small" data-close-drawer aria-label="关闭" style="margin-left:auto">${icon("close")}</button></div>
      <form class="drawer-form" id="drawer-form" style="display:contents">
        <div class="drawer-body">${body}</div>
        <div class="drawer-foot"><button type="button" class="btn" data-close-drawer>取消</button><button type="submit" class="btn primary">${esc(submitLabel)}</button></div>
      </form>
    </aside></div>`;
  const close = () => { root.innerHTML = ""; };
  root.querySelectorAll("[data-close-drawer]").forEach((button) => button.onclick = close);
  root.querySelector("[data-drawer-backdrop]").addEventListener("click", (event) => {
    if (event.target.dataset.drawerBackdrop !== undefined) close();
  });
  const form = root.querySelector("#drawer-form");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector("button[type=submit]");
    button.disabled = true;
    try {
      await onSubmit(new FormData(form), { close });
    } catch (error) {
      let box = form.querySelector(".form-error");
      if (!box) { box = document.createElement("div"); box.className = "form-error"; form.querySelector(".drawer-body").prepend(box); }
      box.textContent = error.message;
    } finally {
      button.disabled = false;
    }
  });
  onMount?.(root);
  focusFirst(root);
  document.addEventListener("keydown", function esc(event) {
    if (event.key === "Escape") { close(); document.removeEventListener("keydown", esc); }
  });
  return { close };
}

export function field(label, input, { hint = "", className = "" } = {}) {
  return `<div class="field ${className}"><label>${esc(label)}</label>${input}${hint ? `<span class="hint">${esc(hint)}</span>` : ""}</div>`;
}

export const checkbox = (name, label, checked = false) =>
  `<label class="checkline"><input type="checkbox" name="${name}" ${checked ? "checked" : ""}> ${esc(label)}</label>`;

export const textInput = (name, value = "", attrs = "") =>
  `<input class="input" name="${name}" value="${esc(value)}" ${attrs}>`;
