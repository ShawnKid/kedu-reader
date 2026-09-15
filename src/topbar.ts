/**
 * 顶栏自适应收纳与「···」溢出菜单（从 main.ts 机械提取）。
 * 依赖 main 注入的 topbar / btnMore 元素；不持有应用状态。
 */

const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

/**
 * 窗口放不下顶栏时按此顺序把低频按钮收进「···」溢出菜单（越靠前越先收纳），
 * 保证留下的按钮保持原始大小，不被挤压变形。
 */
const TOPBAR_OVERFLOWABLE = [
  "btn-settings",
  "btn-mode",
  "btn-view-bookmarks",
  "btn-notes",
  "btn-pdf-fit",
  "btn-pdf-actual",
] as const;

/** 溢出菜单条目（label 与对应按钮的 title 语义一致） */
const TOPBAR_MORE_ITEMS: Array<{ id: (typeof TOPBAR_OVERFLOWABLE)[number]; label: string }> = [
  { id: "btn-settings", label: "阅读设置" },
  { id: "btn-mode", label: "切换滚动/分页" },
  { id: "btn-view-bookmarks", label: "查看书签" },
  { id: "btn-notes", label: "查看笔记" },
  { id: "btn-pdf-fit", label: "适合页面" },
  { id: "btn-pdf-actual", label: "实际大小" },
];

/** 当前被收纳进溢出菜单的按钮 id 集合 */
const collapsedTopbarBtns = new Set<string>();

let topbarEl: HTMLElement | null = null;
let btnMoreEl: HTMLButtonElement | null = null;

/** main boot 时注入顶栏相关 DOM */
export function bindTopbar(topbar: HTMLElement, btnMore: HTMLButtonElement): void {
  topbarEl = topbar;
  btnMoreEl = btnMore;
}

export function relayoutTopbar(): void {
  if (!topbarEl || !btnMoreEl) return;
  collapsedTopbarBtns.clear();
  for (const id of TOPBAR_OVERFLOWABLE) byId<HTMLElement>(id).classList.remove("tb-collapsed");
  // 「···」先占位参与测量：显示后若仍溢出，继续收纳下一颗
  btnMoreEl.classList.remove("hidden");
  let guard = 0;
  while (topbarEl.scrollWidth > topbarEl.clientWidth && guard++ < TOPBAR_OVERFLOWABLE.length + 1) {
    const next = TOPBAR_OVERFLOWABLE.find(
      (id) => !collapsedTopbarBtns.has(id) && byId<HTMLElement>(id).style.display !== "none",
    );
    if (!next) break;
    byId<HTMLElement>(next).classList.add("tb-collapsed");
    collapsedTopbarBtns.add(next);
  }
  // 没有实际收纳任何可见按钮时隐藏「···」
  btnMoreEl.classList.toggle("hidden", collapsedTopbarBtns.size === 0);
}

/** 打开顶栏溢出菜单（条目 = 当前被收纳的按钮，点击转发原按钮 click） */
export function openTopbarMenu(): void {
  if (!btnMoreEl) return;
  closeTopbarMenu();
  const menu = document.createElement("div");
  menu.id = "tb-menu";
  for (const it of TOPBAR_MORE_ITEMS) {
    if (!collapsedTopbarBtns.has(it.id)) continue;
    const target = byId<HTMLButtonElement>(it.id);
    const item = document.createElement("div");
    item.className = "ctx-item" + (target.classList.contains("tb-active") ? " active" : "");
    item.textContent = it.label;
    item.addEventListener("click", (e) => {
      e.stopPropagation();
      closeTopbarMenu();
      target.click();
    });
    menu.appendChild(item);
  }
  if (menu.childElementCount === 0) return;
  document.body.appendChild(menu);
  const btnRect = btnMoreEl.getBoundingClientRect();
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(btnRect.right - rect.width, window.innerWidth - rect.width - 8))}px`;
  menu.style.top = `${Math.min(btnRect.bottom + 6, window.innerHeight - rect.height - 8)}px`;
}

export function closeTopbarMenu(): void {
  document.getElementById("tb-menu")?.remove();
}
