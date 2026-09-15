/**
 * 阅读时长心跳（需求 4）。
 *
 * 策略：每 10 秒自检一次，满足计费条件则累积，攒够 30 秒向后端报送一次。
 * 计费条件：正在阅读 + 页面可见 + 窗口有焦点（或 5 秒内有过交互）+ 用户有近期交互。
 * 由「计费」转「不计费」的瞬间（失焦/隐藏/空闲/离开阅读视图）立即结算零头，
 * 最多多记 10 秒，杜绝挂机刷时长，也不丢真实阅读账。
 * 后端按本地日期落账（跨天自然切日），避免前端时钟不可信。
 */
import { api } from "./ipc";

const TICK_SECONDS = 10;
/** 累积到该秒数才向后端报送一次，控制 IPC 频率 */
const FLUSH_THRESHOLD_SECONDS = 30;
/** 最近一次用户交互距今超过该秒数则暂停计费（防挂机） */
const IDLE_AFTER_SECONDS = 120;
/** 窗口失焦后的交互宽限：刚有过交互（如悬停滚轮翻页）仍算在阅读 */
const RECENT_INTERACTION_SECONDS = 5;

export class ReadingTimer {
  private bookId: string | null = null;
  /** 当前书籍标题，随心跳上报，移除书架后统计页仍能显示书名 */
  private bookTitle: string | null = null;
  /** 全书进度查询（会话开始/结束时取 percent）；main 注入 buildProgress */
  private getPercent: (() => number | null) | null = null;
  private timer: number | undefined;
  private lastActivity = Date.now();
  private focused = document.hasFocus();
  /** 已计费但尚未报送的秒数 */
  private pending = 0;
  /** 当前会话：开始时刻（Unix 秒）；null = 无进行中会话 */
  private sessionStart: number | null = null;
  /** 当前会话累计计费秒数 */
  private sessionSeconds = 0;
  /** 会话开始时的全书进度 */
  private sessionPercentStart: number | null = null;

  constructor() {
    // 任何输入都算「在阅读」的活性信号
    for (const ev of ["mousemove", "keydown", "click", "wheel", "touchstart"] as const) {
      window.addEventListener(ev, () => (this.lastActivity = Date.now()), { passive: true });
    }
    window.addEventListener("focus", () => {
      this.focused = true;
    });
    window.addEventListener("blur", () => {
      this.focused = false;
      // 失焦即刻结算零头，随后暂停计费。
      // 注意：失焦只暂停计费、不拆会话——短失焦（如切去设置窗口）
      // 不把一次阅读碎成多段，恢复计费后会话继续累计。
      void this.flushPending();
    });
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") {
        void this.flushPending();
        // 页面隐藏 = 会话边界：拆会话并上报形状
        this.finalizeSession();
      }
    });
  }

  /**
   * 开始为某本书计时（切换书籍时重复调用安全）。
   * getPercent：全书进度查询（0~100），用于会话的进度起止（阅读速度推导）。
   */
  start(bookId: string, title?: string, getPercent?: () => number | null): void {
    if (this.bookId === bookId) return;
    this.stop();
    this.bookId = bookId;
    this.bookTitle = title ?? null;
    this.getPercent = getPercent ?? null;
    this.lastActivity = Date.now();
    this.focused = document.hasFocus();
    this.pending = 0;
    this.timer = window.setInterval(() => void this.tick(), TICK_SECONDS * 1000);
  }

  /** 暂停计时（离开阅读视图时调用），结算剩余零头并结束会话 */
  stop(): void {
    if (this.timer !== undefined) window.clearInterval(this.timer);
    this.timer = undefined;
    void this.flushPending();
    this.finalizeSession();
    this.bookId = null;
    this.bookTitle = null;
    this.getPercent = null;
  }

  private async tick(): Promise<void> {
    if (!this.bookId) return;
    const now = Date.now();
    const idleSec = (now - this.lastActivity) / 1000;
    const hidden = document.visibilityState === "hidden";
    const active = !hidden && idleSec <= IDLE_AFTER_SECONDS && (this.focused || idleSec <= RECENT_INTERACTION_SECONDS);
    if (!active) {
      // 转入不计费状态：结算已累积的有效阅读时长，随后暂停
      void this.flushPending();
      // 会话结束判定：页面隐藏 / 空闲超时才拆会话；单纯失焦（切窗口后
      // 120s 内回来）只暂停计费，不把一次阅读碎成多段
      if (hidden || idleSec > IDLE_AFTER_SECONDS) this.finalizeSession();
      return;
    }
    // 计费恢复且无进行中会话：记录会话开始时刻与起始进度
    if (this.sessionStart === null) {
      this.sessionStart = Math.floor(now / 1000);
      this.sessionPercentStart = this.getPercent ? this.getPercent() : null;
    }
    this.pending += TICK_SECONDS;
    this.sessionSeconds += TICK_SECONDS;
    if (this.pending >= FLUSH_THRESHOLD_SECONDS) void this.flushPending();
  }

  /** 立即上报已累积的零头 */
  private async flushPending(): Promise<void> {
    const bookId = this.bookId;
    const bookTitle = this.bookTitle;
    const seconds = this.pending;
    this.pending = 0;
    if (!bookId || seconds <= 0) return;
    try {
      await api.recordReadingTime(bookId, seconds, bookTitle ?? undefined);
    } catch (err) {
      console.error("阅读时长上报失败", err);
    }
  }

  /**
   * 结束当前会话并上报「形状」（开始时刻/时长/进度起止）。
   * 失败仅记日志：时长账已由心跳保底，会话只影响时段/速度等形状推导。
   */
  private finalizeSession(): void {
    const bookId = this.bookId;
    const startedAt = this.sessionStart;
    const seconds = this.sessionSeconds;
    const percentStart = this.sessionPercentStart;
    const getPercent = this.getPercent;
    const title = this.bookTitle;
    this.sessionStart = null;
    this.sessionSeconds = 0;
    this.sessionPercentStart = null;
    if (!bookId || startedAt === null || seconds <= 0) return;
    const percentEnd = getPercent ? getPercent() : null;
    void api
      .recordSession(bookId, startedAt, seconds, title ?? undefined, percentStart, percentEnd)
      .catch((err) => console.error("阅读会话上报失败", err));
  }
}
