/**
 * 统计面板（需求 5，基于需求 4 的阅读时长记录 + 阶段一元数据）。
 *
 * 数据全部来自后端 get_reading_stats 实时聚合，前端只做展示：
 * - 汇总卡两行：今日 / 本周 / 本月 / 累计 + 连续天数 / 阅读天数 / 30天日均 / 单日峰值；
 * - 近 30 天逐日柱状图（纯 CSS 柱，悬停 title 显示日期与时长）；
 * - 24 小时时段环形图（SVG，由会话推导的时段分布）；
 * - 会话与速度小卡（次数 / 平均单次 / 最长单次 / 累计读字 / 平均速度）；
 * - 题材分布 / 格式分布横条；
 * - 书籍时长排行（前 10）；
 * - 笔记 / 书签总数。
 */
import { api } from "./ipc";
import { displayTitle } from "./shelf";
import type { DailyPoint, NameValue, StatsSummary } from "./types";

const SVG_NS = "http://www.w3.org/2000/svg";

/** 秒 → 可读时长（如 2小时35分 / 45分钟 / 30秒） */
export function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds} 秒`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m} 分钟`;
  const h = Math.floor(m / 60);
  const rm = m % 60;
  return rm === 0 ? `${h} 小时` : `${h} 小时 ${rm} 分`;
}

/** 字数 → 可读字数（1.2 万字 / 1568.0 万字 / 3.4 亿字） */
function formatChars(n: number): string {
  if (n >= 100_000_000) return `${(n / 100_000_000).toFixed(1)} 亿字`;
  if (n >= 10_000) return `${(n / 10_000).toFixed(1)} 万字`;
  return `${n} 字`;
}

/** 极坐标 → 直角坐标（deg：0 = 正右方，顺时针为正） */
function polar(cx: number, cy: number, r: number, deg: number): [number, number] {
  const rad = (deg * Math.PI) / 180;
  return [cx + r * Math.cos(rad), cy + r * Math.sin(rad)];
}

/** 环形弧段 path（外半径 r+w/2，内半径 r-w/2，从 a0 到 a1，deg） */
function donutArc(cx: number, cy: number, r: number, w: number, a0: number, a1: number): SVGPathElement {
  const ro = r + w / 2;
  const ri = r - w / 2;
  const [x0, y0] = polar(cx, cy, ro, a0);
  const [x1, y1] = polar(cx, cy, ro, a1);
  const [x2, y2] = polar(cx, cy, ri, a1);
  const [x3, y3] = polar(cx, cy, ri, a0);
  const large = a1 - a0 > 180 ? 1 : 0;
  const p = document.createElementNS(SVG_NS, "path");
  p.setAttribute(
    "d",
    `M ${x0.toFixed(2)} ${y0.toFixed(2)} A ${ro} ${ro} 0 ${large} 1 ${x1.toFixed(2)} ${y1.toFixed(2)} ` +
      `L ${x2.toFixed(2)} ${y2.toFixed(2)} A ${ri} ${ri} 0 ${large} 0 ${x3.toFixed(2)} ${y3.toFixed(2)} Z`,
  );
  return p;
}

function svgText(x: number, y: number, str: string, cls: string): SVGTextElement {
  const t = document.createElementNS(SVG_NS, "text");
  t.setAttribute("x", String(x));
  t.setAttribute("y", String(y));
  t.setAttribute("text-anchor", "middle");
  t.setAttribute("class", cls);
  t.textContent = str;
  return t;
}

/** 时段分组的展示色相：凌晨紫 / 上午蓝 / 下午橙 / 晚上玫红（组内逐小时渐亮） */
function hourHsl(h: number): string {
  const base = h < 6 ? 262 : h < 12 ? 208 : h < 18 ? 28 : 340;
  const light = 46 + (h % 6) * 4;
  return `hsl(${base} 62% ${light}%)`;
}

/** 时段图例分组 */
const HOUR_GROUPS: Array<[string, number]> = [
  ["凌晨 0-6", 0],
  ["上午 6-12", 6],
  ["下午 12-18", 12],
  ["晚上 18-24", 18],
];

export class StatsView {
  private root: HTMLElement;

  constructor(root: HTMLElement) {
    this.root = root;
    const btnBack = root.querySelector("#btn-stats-back") as HTMLButtonElement;
    btnBack.addEventListener("click", () => this.onBack?.());
  }

  /** 返回书架（main.ts 注入，避免循环依赖） */
  onBack: (() => void) | null = null;

  /** 拉取并渲染（每次切入统计视图时调用，保证数据新鲜） */
  async refresh(): Promise<void> {
    let s: StatsSummary;
    try {
      s = await api.getReadingStats();
    } catch (err) {
      console.error(err);
      return;
    }
    this.render(s);
  }

  private render(s: StatsSummary): void {
    const set = (id: string, text: string) => {
      const n = this.root.querySelector(id);
      if (n) n.textContent = text;
    };

    // 汇总卡
    set("#st-today", formatDuration(s.todaySeconds));
    set("#st-week", formatDuration(s.weekSeconds));
    set("#st-month", formatDuration(s.monthSeconds));
    set("#st-total", formatDuration(s.totalSeconds));
    set("#st-books", `${s.bookCount} 本`);
    set("#st-finished", `${s.finishedCount} 本`);

    // 习惯卡
    set("#st-streak", `${s.streakDays} 天`);
    set("#st-read-days", `${s.totalReadDays} 天`);
    set("#st-avg-daily", formatDuration(s.avgDaily30));
    set("#st-peak", formatDuration(s.peakDaySeconds));
    const peakEl = this.root.querySelector("#st-peak");
    if (peakEl) peakEl.setAttribute("title", s.peakDayDate ?? "");

    // 30 天日历热力图（GitHub contributions 风格：行 = 周一~周日，列 = 周）
    this.renderHeatmap(s.daily);

    // 会话区（时段环形图 + 会话速度）：尚无会话整体隐藏，避免零值占位
    const hasSessions = s.sessionCount > 0;
    this.root.querySelector("#st-duo")?.classList.toggle("hidden", !hasSessions);
    if (hasSessions) {
      this.renderDonut(s.hourBuckets);
      set("#st-session-count", `${s.sessionCount} 次`);
      set("#st-session-avg", formatDuration(s.avgSessionSeconds));
      set("#st-session-max", formatDuration(s.maxSessionSeconds));
      set("#st-chars-read", s.totalCharsRead > 0 ? formatChars(s.totalCharsRead) : "—");
      set("#st-speed", s.avgSpeedCpm !== null ? `${s.avgSpeedCpm.toFixed(0)} 字/分` : "—");
    }

    // 分布横条（空则整节隐藏）
    this.root.querySelector("#st-genres-section")?.classList.toggle("hidden", s.genreDist.length === 0);
    this.root.querySelector("#st-formats-section")?.classList.toggle("hidden", s.formatDist.length === 0);
    this.renderBars(this.root.querySelector("#st-genres"), s.genreDist);
    this.renderBars(this.root.querySelector("#st-formats"), s.formatDist);

    // 书籍排行
    const list = this.root.querySelector("#st-top-books") as HTMLElement;
    list.innerHTML = "";
    if (s.topBooks.length === 0) {
      list.innerHTML = '<div class="st-empty">还没有阅读记录，打开一本书开始阅读吧</div>';
    } else {
      const top1 = Math.max(1, s.topBooks[0].seconds);
      for (const [i, b] of s.topBooks.entries()) {
        const row = document.createElement("div");
        row.className = "st-book-row";

        const rank = document.createElement("span");
        rank.className = "st-rank" + (i < 3 ? " top" : "");
        rank.textContent = String(i + 1);

        const info = document.createElement("div");
        info.className = "st-book-info";
        const name = document.createElement("div");
        name.className = "st-book-name";
        name.textContent = displayTitle(b.title);
        name.title = b.title;
        const barBg = document.createElement("div");
        barBg.className = "st-book-bar";
        const barFill = document.createElement("div");
        barFill.className = "st-book-bar-fill";
        barFill.style.width = `${Math.max(2, (b.seconds / top1) * 100)}%`;
        barBg.appendChild(barFill);
        info.append(name, barBg);

        const time = document.createElement("span");
        time.className = "st-book-time";
        time.textContent = formatDuration(b.seconds);

        row.append(rank, info, time);
        list.appendChild(row);
      }
    }

    // 笔记 / 书签底行（全为 0 时隐藏）
    const line = this.root.querySelector("#st-notes-line") as HTMLElement;
    line.textContent = `笔记 ${s.noteCount} 条 · 书签 ${s.bookmarkCount} 个`;
    line.classList.toggle("hidden", s.noteCount === 0 && s.bookmarkCount === 0);
  }

  /** 365 天日历热力图：行 = 周一~周日，列 = 周；时长分 5 档颜色（hm-0~hm-4） */
  private renderHeatmap(daily: DailyPoint[]): void {
    const chart = this.root.querySelector("#st-chart") as HTMLElement;
    chart.innerHTML = "";

    // 左侧行标签：7 行中标注 一 / 三 / 五 / 日（其余空）
    const labels = this.root.querySelector("#st-heatmap-labels") as HTMLElement;
    labels.innerHTML = "";
    for (let row = 0; row < 7; row++) {
      const span = document.createElement("span");
      if (row % 2 === 0) span.textContent = ["一", "三", "五", "日"][row / 2];
      labels.appendChild(span);
    }

    if (daily.length === 0) return;

    // 首列按第一天的星期对齐（周一 = 0），前置空占位
    const leading = (new Date(`${daily[0].date}T00:00:00`).getDay() + 6) % 7;
    for (let i = 0; i < leading; i++) {
      chart.appendChild(document.createElement("span"));
    }
    for (const d of daily) {
      const cell = document.createElement("span");
      const sec = d.seconds;
      // 绝对档位：0 / <15分 / <30分 / <1小时 / ≥1小时
      const level = sec <= 0 ? 0 : sec < 900 ? 1 : sec < 1800 ? 2 : sec < 3600 ? 3 : 4;
      cell.className = `hm-cell hm-${level}`;
      cell.title = `${d.date} · ${formatDuration(sec)}`;
      chart.appendChild(cell);
    }
  }

  /** 24 小时时段环形图（SVG）+ 四时段图例 */
  private renderDonut(buckets: number[]): void {
    const host = this.root.querySelector("#st-donut") as HTMLElement;
    host.innerHTML = "";
    const legend = this.root.querySelector("#st-donut-legend") as HTMLElement;
    legend.innerHTML = "";

    const size = 220;
    const cx = size / 2;
    const cy = size / 2;
    const r = 84;
    const w = 26;
    const total = buckets.reduce((a, b) => a + b, 0);

    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", `0 0 ${size} ${size}`);
    svg.classList.add("st-donut-svg");

    if (total > 0) {
      // 段间隙 1.2°，段太薄时按比例收缩避免重叠
      const gap = 1.2;
      let angle = -90; // 从正上方开始
      for (let h = 0; h < 24; h++) {
        const sweep = (buckets[h] / total) * 360;
        if (sweep <= 0) continue;
        const a0 = angle;
        const a1 = angle + sweep - Math.min(gap, sweep * 0.4);
        angle += sweep;
        const seg = donutArc(cx, cy, r, w, a0, a1);
        seg.setAttribute("style", `fill:${hourHsl(h)}`);
        seg.setAttribute("class", "st-donut-seg");
        const title = document.createElementNS(SVG_NS, "title");
        title.textContent = `${h} 点 ~ ${h + 1} 点 · ${formatDuration(buckets[h])}`;
        seg.appendChild(title);
        svg.appendChild(seg);
      }
    }

    // 中心文字
    svg.appendChild(svgText(cx, cy - 4, "总时长", "st-donut-sub"));
    svg.appendChild(svgText(cx, cy + 18, formatDuration(total), "st-donut-total"));
    host.appendChild(svg);

    // 图例：四时段占比
    for (const [label, start] of HOUR_GROUPS) {
      const sec = buckets.slice(start, start + 6).reduce((a, b) => a + b, 0);
      const item = document.createElement("div");
      item.className = "st-legend-item";
      const dot = document.createElement("span");
      dot.className = "st-legend-dot";
      dot.style.background = hourHsl(start);
      const pct = total > 0 ? `${Math.round((sec / total) * 100)}%` : "0%";
      item.title = `${label} · ${formatDuration(sec)}`;
      item.append(dot, Object.assign(document.createElement("span"), { textContent: `${label} ${pct}` }));
      legend.appendChild(item);
    }
  }

  /** 分布横条（题材/格式通用）：名称左 · 比例条中 · 时长右 */
  private renderBars(container: HTMLElement | null, items: NameValue[]): void {
    if (!container) return;
    container.innerHTML = "";
    if (items.length === 0) return;
    const top1 = Math.max(1, ...items.map((i) => i.value));
    for (const it of items) {
      const row = document.createElement("div");
      row.className = "st-bar-row";
      row.title = `${it.name} · ${formatDuration(it.value)}`;

      const name = document.createElement("span");
      name.className = "st-bar-name";
      name.textContent = it.name;

      const barBg = document.createElement("div");
      barBg.className = "st-book-bar";
      const fill = document.createElement("div");
      fill.className = "st-book-bar-fill";
      fill.style.width = `${Math.max(2, (it.value / top1) * 100)}%`;
      barBg.appendChild(fill);

      const val = document.createElement("span");
      val.className = "st-book-time";
      val.textContent = formatDuration(it.value);

      row.append(name, barBg, val);
      container.appendChild(row);
    }
  }
}
