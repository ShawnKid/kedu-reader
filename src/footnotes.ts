/**
 * EPUB 脚注/尾注交互：悬浮预览 + 点击跳转（纯正文容器事件委托，无需按渲染打标）。
 *
 * 识别优先级：
 * 1. EPUB 3 语义标记：<a epub:type="noteref"> 或 <a role="doc-noteref">（清洗链已放行）；
 * 2. 老格式启发式（EPUB 2 时代无语义标注的书）：同文档锚点（href="#id"）+ 标记
 *    文本是典型脚注序号（[1]、1、①、*、† 等，或在 <sup> 内）+ 目标 id 在本章
 *    DOM 中存在。同章脚注占绝大多数，悬浮预览同样可用。
 *
 * 跨文件尾注（href="endnotes.xhtml#n1"，注释体在另一 spine 章节）：前端拿不到
 * href → 章号的映射，无法取注释体，不做悬浮/跳转；但一律 preventDefault 拦截
 * 原生导航（相对路径会让 webview 离开应用页面）。
 *
 * 后端内嵌文本回退：注释体与引用不在同一章时（FB2 notes body / Markdown 跨章
 * 定义），后端在渲染时把注释文本内嵌为引用元素的 data-fn-text，悬浮时兜底显示；
 * 此类引用无本章目标，点击不跳转。
 *
 * 定位注意（与 annotations.ts 同经验）：脚注标记几乎都是行内小元素，取首个
 * client rect 定位；弹窗内容为注释体纯文本（摘除回链 <a> 后取 textContent）。
 */

/** 带括号/圆圈/符号的脚注序号样式：[1] / （2） / ①-⒇ / * / † / ‡（①..⒇ 为 U+2460..2487 连续区段） */
const MARKER_RE = /^(?:[\[（〔［(]\s*\d{1,3}\s*[\)）〕］)]?|[①-⒇*†‡])$/u;

/** 预览文本上限（字符） */
const MAX_TEXT = 400;

interface LocatedRef {
  anchor: HTMLAnchorElement;
  /** 注释体目标 id（不含 #） */
  targetId: string;
  /** 注释体所在章节容器（ref 最近的 [data-chapter] 祖先） */
  host: HTMLElement;
}

export class FootnoteManager {
  private popup: HTMLElement;
  private containers: HTMLElement[] = [];
  /** 裸文本注释体分配 id 的自增序号（滚动流多章共存，document 级唯一） */
  private bodySeq = 0;

  /** 点击脚注标记时跳转到注释体（main.ts 注入：按阅读模式走分页/滚动定位） */
  constructor(private jump: (anchorId: string) => void) {
    this.popup = document.getElementById("fn-popup")!;
  }

  /**
   * 裸文本脚注配对（无任何标记的转换本，如 QQReader/Kindle 导出）：
   * 注释体 = 文本以 `[N]` 开头的叶子块（常见 class="fnote"），正文标记 = 裸文本
   * `[N]`。仅当同章内存在对应 N 的注释体时才包裹标记，杜绝误伤普通引用。
   * 对已包裹内容重复调用安全（跳过 .fn-text-ref 内部）。
   */
  enhance(host: HTMLElement): void {
    const bodies = this.collectNoteBodies(host);
    if (bodies.size === 0) return;
    this.wrapBareMarkers(host, bodies);
  }

  private collectNoteBodies(host: HTMLElement): Map<string, { el: HTMLElement; text: string }> {
    const map = new Map<string, { el: HTMLElement; text: string }>();
    host.querySelectorAll<HTMLElement>("p, li, blockquote, dd, aside, div").forEach((el) => {
      // 只认叶子块，避免把注释区的包装容器误当注释体
      if (el.querySelector("p, div, li, ul, ol, blockquote, aside, section, table")) return;
      const m = /^\s*\[(\d{1,3})\]\s*([\s\S]*)$/.exec(el.textContent ?? "");
      if (!m) return;
      const body = m[2].trim();
      if (!body) return;
      if (!map.has(m[1])) map.set(m[1], { el, text: body });
    });
    return map;
  }

  private wrapBareMarkers(
    host: HTMLElement,
    bodies: Map<string, { el: HTMLElement; text: string }>,
  ): void {
    // 注释体分配跳转 id
    const bodySet = new Set([...bodies.values()].map((v) => v.el));
    const ids = new Map<HTMLElement, string>();
    for (const { el } of bodies.values()) {
      if (!el.id) {
        let id: string;
        do {
          id = `fn-body-${++this.bodySeq}`;
        } while (document.getElementById(id));
        el.id = id;
      }
      ids.set(el, el.id);
    }

    // 先收集再改写：splitText 会打断 TreeWalker 遍历
    const MARK_RE = /\[(\d{1,3})\]/g;
    const walker = document.createTreeWalker(host, NodeFilter.SHOW_TEXT, {
      acceptNode: (node) => {
        const p = node.parentElement;
        if (!p || p.closest(".fn-text-ref") || p.closest("a[href]")) return NodeFilter.FILTER_REJECT;
        const block = p.closest<HTMLElement>("p, li, blockquote, dd, aside, div");
        if (block && bodySet.has(block)) return NodeFilter.FILTER_REJECT; // 注释体自身的 [N]
        return NodeFilter.FILTER_ACCEPT;
      },
    });
    const nodes: Text[] = [];
    for (let n = walker.nextNode(); n; n = walker.nextNode()) nodes.push(n as Text);

    for (const node of nodes) {
      let rest: Text | null = node;
      while (rest) {
        MARK_RE.lastIndex = 0;
        const m = MARK_RE.exec(rest.textContent ?? "");
        if (!m) break;
        const info = bodies.get(m[1]);
        const after = rest.splitText(m.index + m[0].length);
        if (info) {
          const marker = rest.splitText(m.index);
          const span = document.createElement("span");
          span.className = "fn-text-ref";
          // 注意：不再另设 textContent，移入的原文本节点就是标记本身（否则会出现 [1][1]）
          span.dataset.fnText = info.text.length > MAX_TEXT ? `${info.text.slice(0, MAX_TEXT)}…` : info.text;
          span.dataset.fnJump = ids.get(info.el)!;
          marker.parentNode?.insertBefore(span, marker);
          span.appendChild(marker);
        }
        rest = after;
      }
    }
  }

  /** 绑定正文容器（滚动视图 / 分页 host），委托 mouseover/mouseout/click */
  bindContent(containers: HTMLElement[]): void {
    for (const c of containers) {
      this.containers.push(c);
      c.addEventListener("mouseover", (e) => {
        const ref = (e.target as HTMLElement).closest?.("a[href], .fn-text-ref") as HTMLElement | null;
        if (!ref) return;
        if (ref.classList.contains("fn-text-ref")) {
          const text = ref.dataset.fnText;
          if (!text) {
            this.hide();
            return;
          }
          this.showText(text, ref.getBoundingClientRect());
        } else {
          this.onOver(ref as HTMLAnchorElement);
        }
      });
      c.addEventListener("mouseout", (e) => {
        const ref = (e.target as HTMLElement).closest?.("a[href], .fn-text-ref") as HTMLElement | null;
        // 仅当真正离开标记（而非在其子元素间移动）时收起
        if (ref && !(ref as HTMLElement).contains(e.relatedTarget as Node)) this.hide();
      });
      c.addEventListener("scroll", () => this.hide(), { passive: true });
      c.addEventListener("click", (e) => {
        const ref = (e.target as HTMLElement).closest?.("a[href], .fn-text-ref") as HTMLElement | null;
        if (!ref) return;
        e.preventDefault(); // 任何书内链接都不允许原生导航（会离开应用页面）
        this.hide();
        if (ref.classList.contains("fn-text-ref")) {
          const id = ref.dataset.fnJump;
          if (id) this.jump(id);
          return;
        }
        const loc = this.locate(ref as HTMLAnchorElement);
        if (loc) this.jump(loc.targetId);
      });
    }

    // 点弹窗外 / Esc 收起
    document.addEventListener("mousedown", (e) => {
      const t = e.target as HTMLElement;
      if (!t.closest("#fn-popup") && !t.closest("a[href], .fn-text-ref")) this.hide();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hide();
    });
  }

  hide(): void {
    this.popup.classList.add("hidden");
  }

  /** 判断锚点是否为脚注引用，并解析出注释体位置 */
  private locate(anchor: HTMLAnchorElement): LocatedRef | null {
    const semantic =
      anchor.getAttribute("epub:type")?.split(/\s+/).includes("noteref") ||
      anchor.getAttribute("role")?.split(/\s+/).includes("doc-noteref");
    const href = anchor.getAttribute("href") ?? "";
    const hash = href.indexOf("#");
    if (hash === -1) return null; // 纯外链（点击已被全局拦截导航）
    const host = anchor.closest<HTMLElement>("[data-chapter]");
    if (!host) return null;
    let targetId = href.slice(hash + 1);
    try {
      targetId = decodeURIComponent(targetId);
    } catch {
      /* 保留原值 */
    }
    if (!targetId) return null;
    // 带文件名的引用：Calibre/Kindle 转换常把同章引用写成 self.html#id 的
    // 自引用（如 <a href="part0016.html#ch1-back">(1)</a>）。目标 id 在当前
    // 文档中存在 → 按同文档处理；不存在 → 真正的跨文件尾注，不处理。
    if (hash > 0 && !host.querySelector(`#${CSS.escape(targetId)}`)) return null;
    if (!semantic) {
      // 老格式启发式：带括号/圆圈/符号的序号文本，或 <sup> 内的纯数字
      const text = (anchor.textContent ?? "").trim();
      const inSup = anchor.closest("sup") !== null;
      const markerLike = MARKER_RE.test(text) || (inSup && /^\d{1,3}$/.test(text));
      if (!markerLike) return null;
    }
    return { anchor, targetId, host };
  }

  private onOver(anchor: HTMLAnchorElement): void {
    // 1) 语义/启发式 + 本章 DOM 里的注释体（EPUB / Markdown 同章）
    const loc = this.locate(anchor);
    let text = loc ? this.footnoteText(loc.host, loc.targetId) : null;
    // 2) 后端内嵌文本回退：注释体不在本章（FB2 notes body / Markdown 跨章定义）
    if (!text) {
      const embedded = anchor.dataset.fnText;
      if (!embedded) {
        this.hide();
        return;
      }
      text = embedded;
    }
    // 标记是行内小元素：首个 client rect 即视觉位置（跨页碎片也取命中侧）
    this.showText(text, anchor.getClientRects()[0] ?? anchor.getBoundingClientRect());
  }

  private showText(text: string, rect: DOMRect): void {
    this.popup.querySelector("#fn-text")!.textContent = text;
    this.showAt(rect);
  }

  /** 取注释体纯文本：克隆后摘除回链与定义序号标签，折叠空白 */
  private footnoteText(host: HTMLElement, targetId: string): string | null {
    const target = host.querySelector(`#${CSS.escape(targetId)}`);
    if (!target) return null;
    // 目标 id 常挂在注释段内的行内锚（编号/回链，如 Calibre 双向自引用）上，
    // 向上取所在块级段落才是完整注释文本（不用 div：避免抓到整个注释区容器）
    const block = target.closest("p, li, blockquote, dd, aside") ?? target;
    const clone = block.cloneNode(true) as HTMLElement;
    clone.querySelectorAll("a").forEach((a) => a.remove());
    clone.querySelectorAll(".footnote-definition-label").forEach((n) => n.remove());
    const text = (clone.textContent ?? "").replace(/\s+/g, " ").trim();
    if (!text) return null;
    return text.length > MAX_TEXT ? `${text.slice(0, MAX_TEXT)}…` : text;
  }

  private showAt(rect: DOMRect): void {
    this.popup.classList.remove("hidden");
    const w = this.popup.offsetWidth;
    const h = this.popup.offsetHeight;
    const x = Math.max(8, Math.min(rect.left, window.innerWidth - w - 8));
    const y = rect.top - h - 8 > 0 ? rect.top - h - 8 : rect.bottom + 8;
    this.popup.style.left = `${x}px`;
    this.popup.style.top = `${y}px`;
  }
}
