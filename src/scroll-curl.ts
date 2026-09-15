/**
 * 固定切片网格映射到顶部圆柱面，滚动只移动网格内的内容。
 * 保留完整章节排版，不拆叶子块。源带长 πR/2，不是投影高度 R。
 */
const R = 33; // 与 --curl-radius 一致
const ARC = Math.PI * R / 2;
const SLICE = 4;
// 相邻合成层的抗锯齿边缘需要少量同源重叠，避免露出背景细线。
const BLEED = 0.5;
const clamp = (v: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, v));

interface Strip {
  window: HTMLDivElement;
  shell: HTMLDivElement;
  sourceTop: number;
  chapterTop: number;
}

export class ScrollCurl {
  private enabled = false;
  private raf = 0;
  private cur = 0;
  private target = 0;
  private lastTime = 0;
  private groups = new Map<HTMLElement, Strip[]>();
  private observed = new Set<HTMLElement>();
  private ro?: ResizeObserver;
  private mo?: MutationObserver;

  constructor(private layer: HTMLElement, private sv: HTMLElement) {
    layer.setAttribute("aria-hidden", "true");
    layer.inert = true;
  }

  get active(): boolean { return this.enabled; }

  setEnabled(on: boolean): void {
    if (on === this.enabled) {
      if (on) this.kick();
      return;
    }
    this.enabled = on;
    this.sv.classList.toggle("curl", on);
    this.layer.classList.toggle("hidden", !on);
    if (on) {
      this.cur = this.target = this.sv.scrollTop;
      this.sv.addEventListener("wheel", this.onWheel, { passive: false });
      this.sv.addEventListener("scroll", this.onScroll, { passive: true });
      this.sv.addEventListener("load", this.onLayout, true);
      document.fonts.addEventListener("loadingdone", this.onLayout);
      this.ro = new ResizeObserver(this.onLayout);
      this.ro.observe(this.sv);
      this.mo = new MutationObserver(this.onLayout);
      this.mo.observe(this.sv, { subtree: true, childList: true, characterData: true, attributes: true });
      this.kick();
    } else {
      this.sv.removeEventListener("wheel", this.onWheel);
      this.sv.removeEventListener("scroll", this.onScroll);
      this.sv.removeEventListener("load", this.onLayout, true);
      document.fonts.removeEventListener("loadingdone", this.onLayout);
      this.ro?.disconnect();
      this.mo?.disconnect();
      this.observed.clear();
      cancelAnimationFrame(this.raf);
      this.raf = this.lastTime = 0;
      this.clearAll();
    }
  }

  invalidate(): void {
    this.clearAll();
    this.kick();
  }

  nudge(dy: number): void {
    if (!this.enabled) return;
    this.target = clamp(this.target + dy, 0, this.max());
    this.kick();
  }

  private onLayout = (): void => { this.invalidate(); };

  private clearAll(): void {
    this.layer.replaceChildren();
    this.groups.clear();
  }

  private max(): number { return Math.max(0, this.sv.scrollHeight - this.sv.clientHeight); }

  private kick(): void {
    if (this.enabled && !this.raf) this.raf = requestAnimationFrame(this.tick);
  }

  private onWheel = (e: WheelEvent): void => {
    if (e.ctrlKey || !e.deltaY) return;
    e.preventDefault();
    this.nudge(e.deltaY * (e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? this.sv.clientHeight : 1));
  };

  private onScroll = (): void => {
    if (Math.abs(this.sv.scrollTop - this.cur) > 1) {
      this.cur = this.target = this.sv.scrollTop;
    }
    // 原生滚动在本次绘制前同步卷绕层，避免额外等一帧造成两层错位。
    if (this.enabled) this.renderNow();
  };

  private tick = (time: number): void => {
    this.raf = 0;
    if (!this.enabled) return;
    this.target = clamp(this.target, 0, this.max());
    const dt = this.lastTime ? Math.min(50, time - this.lastTime) : 1000 / 60;
    const d = this.target - this.cur;
    if (Math.abs(d) > 0.1) {
      this.cur += d * (1 - Math.pow(0.84, dt / (1000 / 60)));
      this.lastTime = time;
      this.kick();
    } else {
      this.cur = this.target;
      this.lastTime = 0;
    }
    this.sv.scrollTop = this.cur;
    this.renderNow();
  };

  private renderNow(): void {
    const rect = this.sv.getBoundingClientRect();
    const layerRect = this.layer.getBoundingClientRect();
    const sections = Array.from(this.sv.children) as HTMLElement[];
    const active = new Set<HTMLElement>();
    for (const sec of this.observed) {
      if (sec.parentElement !== this.sv) {
        this.ro?.unobserve(sec);
        this.observed.delete(sec);
      }
    }
    for (const sec of sections) {
      if (!this.observed.has(sec)) {
        this.observed.add(sec);
        this.ro?.observe(sec);
      }
      const secRect = sec.getBoundingClientRect();
      const top = secRect.top - rect.top;
      if (top >= R || secRect.bottom - rect.top <= R - ARC) continue;
      active.add(sec);
      let strips = this.groups.get(sec);
      if (!strips) {
        strips = this.buildStrips(sec, rect.left - layerRect.left);
        this.groups.set(sec, strips);
      }
      for (const strip of strips) {
        strip.shell.style.top = (top - strip.sourceTop - strip.chapterTop + BLEED) + "px";
      }
    }
    for (const [sec, strips] of this.groups) {
      if (!active.has(sec)) {
        strips.forEach(s => s.window.remove());
        this.groups.delete(sec);
      }
    }
  }

  private buildStrips(sec: HTMLElement, left: number): Strip[] {
    const strips: Strip[] = [];
    const style = getComputedStyle(this.sv);
    const fragment = document.createDocumentFragment();
    const transforms: string[] = [];
    for (let d = 0; d < ARC; d += SLICE) {
      const h = Math.min(SLICE, ARC - d);
      const angle = (d + h / 2) / R;
      const half = h / (2 * R);
      const scale = Math.sin(half) / half;
      const bottom = R - R * Math.sin(d / R);
      const z = R * (Math.cos(d / R) - 1);
      const win = document.createElement("div");
      win.className = "curl-line";
      win.style.height = (h + 2 * BLEED) + "px";
      win.style.transformOrigin = "50% calc(100% - " + BLEED + "px)";
      const shell = document.createElement("div");
      shell.className = "curl-shell";
      // 复刻正文宽度（排除滚动条），不在整窗重新居中。
      shell.style.width = this.sv.clientWidth + "px";
      shell.style.left = left + "px";
      shell.style.paddingLeft = style.paddingLeft;
      shell.style.paddingRight = style.paddingRight;
      // 保留 --reader-font 的无单位行高；computed font 会把它变成固定 px，
      // 导致 sup/small 等不同字号的子元素撑高行盒。
      shell.style.color = style.color;
      shell.style.direction = style.direction;
      const clone = sec.cloneNode(true) as HTMLElement;
      shell.append(clone);
      win.append(shell);
      fragment.append(win);
      strips.push({ window: win, shell, sourceTop: R - d - h, chapterTop: 0 });
      // 共用圆柱面端点，末片采用真实高度；不截断变换精度。
      transforms.push("translate3d(0," + (bottom - h - BLEED) + "px," + z
        + "px) rotateX(" + angle + "rad) scaleY(" + scale + ")");
    }
    this.layer.append(fragment);
    // 同批插入、只测量一次，避免每片完整章节都触发同步回流。
    const shell = strips[0].shell;
    const chapterTop = shell.firstElementChild!.getBoundingClientRect().top - shell.getBoundingClientRect().top;
    strips.forEach((strip, i) => {
      strip.chapterTop = chapterTop;
      strip.window.style.transform = transforms[i];
    });
    return strips;
  }
}
