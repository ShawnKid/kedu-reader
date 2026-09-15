/**
 * 书架侧 UI 基础件（从 shelf.ts 机械提取）：
 * 标题截断、敏感词、格式占位、模态/Toast。外部入口仍由 shelf.ts 再导出。
 */
import { FOLLOW_BOOK_FONT } from "./types";
import type { BookFormat, ShelfBook } from "./types";

/** 每种格式的占位封面（无封面书 / PDF 用） */
export const FORMAT_LABEL: Record<BookFormat, string> = {
  epub: "EPUB",
  mobi: "MOBI",
  azw3: "AZW3",
  txt: "TXT",
  markdown: "MD",
  fb2: "FB2",
  cbz: "CBZ",
  pdf: "PDF",
};

/** 单本字体在右键菜单里的显示名：「跟随图书设定」哨兵值转义 */
export function bookFontMenuLabel(fontFamily: string | null): string | null {
  return fontFamily === FOLLOW_BOOK_FONT ? "跟随图书设定" : fontFamily;
}

/**
 * 隐私敏感词：书名命中且用户未手动设置统计开关时，该书自动不计入阅读统计。
 * 需与后端 src-tauri/src/model.rs 的 SENSITIVE_WORDS 保持一致。
 */
export const SENSITIVE_WORDS = ["淫荡", "少妇白洁", "少年阿宾"];

/** 该书当前是否不计入阅读统计：用户手动设置优先，否则按书名敏感词自动判定 */
export function isStatsExcluded(b: ShelfBook): boolean {
  return b.statsExcluded ?? SENSITIVE_WORDS.some((w) => b.title.includes(w));
}

/**
 * 书名展示截断：默认隐藏 (、（、【、[ 及其后的内容。
 * 仅影响界面显示——书架数据与重命名弹窗始终保存/预填完整书名。
 * 书名以括号开头（截断后会变空）时不截断。
 */
const TITLE_CUT_CHARS = ["(", "（", "【", "["];

export function displayTitle(raw: string): string {
  let cut = -1;
  for (const ch of TITLE_CUT_CHARS) {
    const i = raw.indexOf(ch);
    if (i !== -1 && (cut === -1 || i < cut)) cut = i;
  }
  if (cut <= 0) return raw;
  return raw.slice(0, cut).trim() || raw;
}

/** 五角星 SVG 路径（Material star，viewBox 0 0 24 24） */
const STAR_PATH =
  "M12 17.27L18.18 21l-1.64-7.03L22 9.24l-7.19-.61L12 2 9.19 8.63 2 9.24l5.46 4.73L5.82 21z";

/** 构建一行 10 个五角星（评分弹层的底层/遮罩层共用） */
export function starRow(cls: string): HTMLDivElement {
  const row = document.createElement("div");
  row.className = cls;
  for (let i = 0; i < 10; i++) {
    row.insertAdjacentHTML(
      "beforeend",
      `<svg viewBox="0 0 24 24"><path d="${STAR_PATH}" fill="currentColor"/></svg>`
    );
  }
  return row;
}

/**
 * 自定义模态输入框（替代原生 window.prompt 的丑默认 UI）。
 * 返回输入内容（已 trim）；取消 / Escape / 点击遮罩返回 null。
 * 空输入时「确定」禁用，Enter 确认。
 */
export function promptModal(opts: {
  title: string;
  value?: string;
  placeholder?: string;
  okText?: string;
}): Promise<string | null> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "modal-mask";

    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";

    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = opts.title;

    const input = document.createElement("input");
    input.className = "modal-input";
    input.value = opts.value ?? "";
    if (opts.placeholder) input.placeholder = opts.placeholder;

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const btnCancel = document.createElement("button");
    btnCancel.className = "modal-btn";
    btnCancel.textContent = "取消";
    const btnOk = document.createElement("button");
    btnOk.className = "modal-btn primary";
    btnOk.textContent = opts.okText ?? "确定";
    actions.append(btnCancel, btnOk);

    dialog.append(title, input, actions);
    mask.appendChild(dialog);
    document.body.appendChild(mask);

    let done = false;
    const finish = (value: string | null): void => {
      if (done) return;
      done = true;
      mask.remove();
      resolve(value);
    };
    const syncOk = (): void => {
      btnOk.disabled = input.value.trim().length === 0;
    };

    btnOk.addEventListener("click", () => finish(input.value.trim()));
    btnCancel.addEventListener("click", () => finish(null));
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) finish(null);
    });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") finish(input.value.trim());
      else if (e.key === "Escape") finish(null);
      e.stopPropagation();
    });
    input.addEventListener("input", syncOk);
    syncOk();
    input.focus();
    input.select();
  });
}

/**
 * 自定义模态确认框（替代 window.confirm——Tauri 2 WebView2 下原生 confirm 静默返回 false）。
 * 确定 → true；取消 / Escape / 点击遮罩 → false。
 */
export function confirmModal(opts: {
  title: string;
  message: string;
  okText?: string;
  danger?: boolean;
}): Promise<boolean> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "modal-mask";

    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";

    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = opts.title;

    const message = document.createElement("div");
    message.className = "modal-message";
    message.textContent = opts.message;

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const btnCancel = document.createElement("button");
    btnCancel.className = "modal-btn";
    btnCancel.textContent = "取消";
    const btnOk = document.createElement("button");
    btnOk.className = "modal-btn primary" + (opts.danger ? " danger" : "");
    btnOk.textContent = opts.okText ?? "确定";
    actions.append(btnCancel, btnOk);

    dialog.append(title, message, actions);
    mask.appendChild(dialog);
    document.body.appendChild(mask);

    let done = false;
    const finish = (value: boolean): void => {
      if (done) return;
      done = true;
      mask.remove();
      resolve(value);
    };

    btnOk.addEventListener("click", () => finish(true));
    btnCancel.addEventListener("click", () => finish(false));
    for (const b of [btnOk, btnCancel]) {
      b.addEventListener("keydown", (e) => {
        if (e.key === "Enter") finish(b === btnOk);
        else if (e.key === "Escape") finish(false);
      });
    }
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) finish(false);
    });
    btnOk.focus();
  });
}

/**
 * 轻提示（替代 window.alert——Tauri 2 WebView2 下原生 alert 不弹出）。
 * 非阻塞，3s 自动淡出。main.ts 导入失败提示等也复用。
 */
export function toast(message: string): void {
  const el = document.createElement("div");
  el.className = "shelf-toast";
  el.textContent = message;
  document.body.appendChild(el);
  window.setTimeout(() => {
    el.classList.add("hide");
    window.setTimeout(() => el.remove(), 300);
  }, 3000);
}
