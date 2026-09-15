/**
 * 独立更换封面窗口（Tauri 窗口 label = "cover"）。
 *
 * 与主窗口共用同一前端产物（index.html）：bootstrap.ts 按窗口 label 分流到这里。
 * 单例窗口：书目 id 由后端经 initialization_script 注入 window.__COVER_BOOK_ID__；
 * 窗口已开时后端改发 "cover-book-changed" 事件切换书目。
 *
 * 两段封面源：
 * - 系统默认封面行：默认 cover.dat（点选恢复默认）+ 自定义槽（虚线「+」或当前用户图）
 * - 内置封面：书内候选图，点选写入 cover-custom.dat
 *
 * 点选即应用并广播 cover-changed，主书架窗刷新缩略图。
 */
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import bookArtUrl from "./assets/book-art.svg";
import {
  api,
  bumpCoverVersion,
  coverCandidateUrl,
  coverCustomUrl,
  coverDefaultUrl,
} from "./ipc";
import { FORMAT_LABEL } from "./shelf-ui";
import type { BookCoverOptions, ReaderSettings } from "./types";

declare global {
  interface Window {
    __COVER_BOOK_ID__?: string;
  }
}

let bookId: string | null = null;
let options: BookCoverOptions | null = null;
/** 申请变更时的加载序号：丢弃过期异步结果，避免切书后旧数据覆盖新书 */
let loadSeq = 0;

const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

function buildDom(): void {
  document.title = "更换封面";
  document.body.classList.add("settings-page", "cover-page");
  document.body.innerHTML = `
    <header id="sw-titlebar" data-tauri-drag-region>
      <span class="sw-title" data-tauri-drag-region>更换封面</span>
      <button id="sw-close" class="win-btn close" title="关闭">
        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" stroke="currentColor" stroke-width="1"/></svg>
      </button>
    </header>
    <main id="sw-body">
      <div id="cv-head">
        <span id="cv-book" title=""></span>
        <span id="cv-hint">点选封面即可应用</span>
      </div>
      <section id="cv-default-sec" class="cv-sec">
        <h2 class="cv-sec-title">系统默认封面</h2>
        <div id="cv-default" class="cv-grid"></div>
      </section>
      <section id="cv-builtin-sec" class="cv-sec" hidden>
        <h2 class="cv-sec-title">内置封面</h2>
        <div id="cv-builtin" class="cv-grid"></div>
      </section>
      <div id="cv-status" hidden></div>
    </main>`;
}

function setStatus(text: string, isError = false): void {
  const el = byId<HTMLDivElement>("cv-status");
  el.textContent = text;
  el.classList.toggle("error", isError);
  el.hidden = !text;
}

function makeThumb(opts: {
  src?: string;
  label?: string;
  selected: boolean;
  isAdd?: boolean;
  isPlaceholder?: boolean;
  formatLabel?: string;
  onClick: () => void;
  title?: string;
}): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "cv-thumb";
  if (opts.selected) btn.classList.add("selected");
  if (opts.isAdd) btn.classList.add("cv-add");
  if (opts.isPlaceholder) btn.classList.add("cv-ph-cover");
  btn.title = opts.title ?? opts.label ?? "";
  btn.addEventListener("click", opts.onClick);

  if (opts.isAdd) {
    btn.setAttribute("aria-label", "从图片文件选择封面");
    const plus = document.createElement("span");
    plus.className = "cv-plus";
    plus.textContent = "+";
    btn.appendChild(plus);
  } else if (opts.isPlaceholder) {
    // 与书架无封面占位同构：线稿书本 + 底部格式印记
    const face = document.createElement("span");
    face.className = "cv-ph-face";
    const art = document.createElement("img");
    art.className = "ph-art";
    art.src = bookArtUrl;
    art.draggable = false;
    art.alt = "";
    face.appendChild(art);
    if (opts.formatLabel) {
      const fmt = document.createElement("span");
      fmt.className = "ph-format";
      fmt.textContent = opts.formatLabel;
      face.appendChild(fmt);
    }
    btn.appendChild(face);
  } else if (opts.src) {
    const img = document.createElement("img");
    img.alt = opts.label ?? "";
    img.draggable = false;
    img.src = opts.src;
    btn.appendChild(img);
  } else if (opts.label) {
    const ph = document.createElement("span");
    ph.className = "cv-ph";
    ph.textContent = opts.label;
    btn.appendChild(ph);
  }
  if (opts.label && !opts.isAdd && !opts.isPlaceholder) {
    const cap = document.createElement("span");
    cap.className = "cv-cap";
    cap.textContent = opts.label;
    btn.appendChild(cap);
  } else if (opts.isPlaceholder && opts.label) {
    const cap = document.createElement("span");
    cap.className = "cv-cap";
    cap.textContent = opts.label;
    btn.appendChild(cap);
  }
  return btn;
}

function isSelected(selection: string, kind: "default" | "candidate" | "custom" | "none", index?: number): boolean {
  if (kind === "candidate") return selection === `candidate:${index}`;
  return selection === kind;
}

async function applyDefault(): Promise<void> {
  if (!bookId) return;
  setStatus("应用中…");
  try {
    await api.setBookCover(bookId, null);
    bumpCoverVersion(bookId);
    await reload();
    setStatus("已恢复系统默认封面");
  } catch (err) {
    setStatus(String(err), true);
  }
}

async function applyNone(): Promise<void> {
  if (!bookId) return;
  setStatus("应用中…");
  try {
    await api.setBookCoverNone(bookId);
    bumpCoverVersion(bookId);
    await reload();
    setStatus("已设为无封面");
  } catch (err) {
    setStatus(String(err), true);
  }
}

async function applyCandidate(index: number): Promise<void> {
  if (!bookId) return;
  setStatus("应用中…");
  try {
    await api.setBookCoverCandidate(bookId, index);
    bumpCoverVersion(bookId);
    await reload();
    setStatus(`已选用内置封面 ${index + 1}`);
  } catch (err) {
    setStatus(String(err), true);
  }
}

async function pickCustom(): Promise<void> {
  if (!bookId) return;
  try {
    const path = await openFileDialog({
      title: "选择封面图片",
      multiple: false,
      filters: [{ name: "图片", extensions: ["jpg", "jpeg", "png", "gif", "webp"] }],
    });
    if (!path || Array.isArray(path)) return;
    setStatus("应用中…");
    await api.setBookCover(bookId, path);
    bumpCoverVersion(bookId);
    await reload();
    setStatus("封面已更新");
  } catch (err) {
    setStatus(String(err), true);
  }
}

function render(): void {
  const o = options;
  if (!o || !bookId) return;

  byId<HTMLSpanElement>("cv-book").textContent = o.title;
  byId<HTMLSpanElement>("cv-book").title = o.title;

  const defaultBox = byId<HTMLDivElement>("cv-default");
  const builtinBox = byId<HTMLDivElement>("cv-builtin");
  defaultBox.innerHTML = "";
  builtinBox.innerHTML = "";

  byId<HTMLElement>("cv-builtin-sec").hidden = o.candidateCount === 0;

  if (o.hasDefault) {
    defaultBox.appendChild(
      makeThumb({
        src: coverDefaultUrl(bookId),
        label: "默认",
        selected: isSelected(o.selection, "default"),
        title: "恢复系统默认封面",
        onClick: () => void applyDefault(),
      }),
    );
  }

  // 系统生成的无封面占位（与书架 404 兜底同图）
  defaultBox.appendChild(
    makeThumb({
      isPlaceholder: true,
      label: "无封面",
      formatLabel: FORMAT_LABEL[o.format],
      selected: isSelected(o.selection, "none"),
      title: "使用系统生成的无封面占位图",
      onClick: () => void applyNone(),
    }),
  );

  // 自定义槽：与默认封面同排。已有用户图则展示该图（点击可换），否则虚线「+」
  if (o.hasCustom && !o.customIsBuiltin) {
    defaultBox.appendChild(
      makeThumb({
        src: coverCustomUrl(bookId),
        label: "自定义",
        selected: isSelected(o.selection, "custom"),
        title: "更换自定义封面",
        onClick: () => void pickCustom(),
      }),
    );
  } else {
    defaultBox.appendChild(
      makeThumb({
        isAdd: true,
        selected: false,
        title: "从图片文件选择封面",
        onClick: () => void pickCustom(),
      }),
    );
  }

  for (let i = 0; i < o.candidateCount; i++) {
    const index = i;
    builtinBox.appendChild(
      makeThumb({
        src: coverCandidateUrl(bookId, index),
        label: String(index + 1),
        selected: isSelected(o.selection, "candidate", index),
        title: `选用内置封面 ${index + 1}`,
        onClick: () => void applyCandidate(index),
      }),
    );
  }
}

async function reload(): Promise<void> {
  const id = bookId;
  if (!id) return;
  const seq = ++loadSeq;
  try {
    const o = await api.getBookCoverOptions(id);
    if (seq !== loadSeq || id !== bookId) return;
    options = o;
    render();
  } catch (err) {
    if (seq !== loadSeq) return;
    setStatus(String(err), true);
  }
}

async function loadBook(id: string): Promise<void> {
  bookId = id;
  options = null;
  setStatus("");
  await reload();
}

function bindEvents(): void {
  const close = () => void getCurrentWindow().close();
  byId<HTMLButtonElement>("sw-close").addEventListener("click", close);
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
  });

  void listen<string>("cover-book-changed", (e) => {
    void loadBook(e.payload).catch(console.error);
  });
}

/** 更换封面窗口启动入口（bootstrap.ts 按窗口 label 调用） */
export async function bootCoverWindow(): Promise<void> {
  buildDom();
  const appWindow = getCurrentWindow();
  const syncMaximized = async () => {
    document.body.classList.toggle("win-maximized", await appWindow.isMaximized());
  };
  void syncMaximized();
  appWindow.onResized(() => void syncMaximized());
  try {
    const settings: ReaderSettings = await api.loadReaderSettings();
    document.body.dataset.theme = settings.theme;
  } catch {
    // 主题读取失败保持默认
  }
  bindEvents();
  const id = window.__COVER_BOOK_ID__ ?? "";
  if (id) {
    await loadBook(id).catch((err) => {
      console.error("封面窗口加载书目失败:", err);
      setStatus(String(err), true);
    });
  }
}
