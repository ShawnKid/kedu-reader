/**
 * PDF 渲染（需求 1：全格式兼容的 PDF 分支）。
 *
 * 后端不解析 PDF：文件字节经 book-file:// 协议按 book_id 直出，
 * 本模块用 pdf.js 逐页渲染到 canvas。章节维度映射为页码——
 * progress.chapterIndex 存当前页号，percent = 页号/总页数 × 100，
 * 进度持久化复用 save_progress / load_progress，与其他格式一致。
 *
 * 内存约束：只保留当前页 canvas，翻页即销毁重建，不做全文档位图缓存。
 */
import * as pdfjsLib from "pdfjs-dist";
// Vite 会把 worker 打包成同源资源（CSP worker-src 'self' 放行）
import workerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

pdfjsLib.GlobalWorkerOptions.workerSrc = workerUrl;

export interface PdfRenderResult {
  /** 总页数（用于 percent 计算与进度恢复钳制） */
  pageCount: number;
}

export interface PdfJumpOptions {
  page: number;
}

/** PDF 缩放模式：宽度自适应（默认）/ 适合页面（整页可见）/ 实际大小（100%） */
export type PdfZoomMode = "fit-width" | "fit-page" | "actual";

/**
 * PDF 渲染会话：一个 book 对应一个实例。
 * main.ts 持有 activePdf，切书/退书时调用 destroy()。
 */
export class PdfSession {
  private doc: pdfjsLib.PDFDocumentProxy | null = null;
  /** getDocument 的 loading task：destroy 时结束加载中的网络/解析工作 */
  private loadTask: pdfjsLib.PDFDocumentLoadingTask | null = null;
  /** 进行中的页渲染 task：destroy 时 cancel，避免迟到结果写 canvas */
  private renderTask: pdfjsLib.RenderTask | null = null;
  private container: HTMLElement;
  private canvas: HTMLCanvasElement;
  private rendering: Promise<void> | null = null;
  private pendingPage = -1;
  private destroyed = false;
  private zoom: PdfZoomMode = "fit-page";
  /** 最近渲染成功的页号（切缩放后重渲染用） */
  private lastPage = -1;

  /** 翻页后回调（main.ts 更新进度与百分比显示） */
  onPageRendered: ((page: number, pageCount: number) => void) | null = null;

  constructor(container: HTMLElement, canvas: HTMLCanvasElement) {
    this.container = container;
    this.canvas = canvas;
  }

  get pageCount(): number {
    return this.doc?.numPages ?? 0;
  }

  get zoomMode(): PdfZoomMode {
    return this.zoom;
  }

  /** 切换缩放模式并重渲染当前页（容器宽度自适应 / 适合页面 / 实际大小） */
  setZoom(mode: PdfZoomMode): void {
    if (this.zoom === mode) return;
    this.zoom = mode;
    if (this.lastPage >= 0) void this.render(this.lastPage);
  }

  /** 加载文档并渲染首页。url 为 book-file:// 协议地址。 */
  async load(url: string): Promise<PdfRenderResult> {
    const task = pdfjsLib.getDocument({ url, isEvalSupported: false });
    this.loadTask = task;
    let doc: pdfjsLib.PDFDocumentProxy;
    try {
      doc = await task.promise;
    } catch (err) {
      // 加载中被 destroy/cancel：吞掉，不向调用方抛「已取消」
      if (this.destroyed) return { pageCount: 0 };
      throw err;
    }
    if (this.destroyed) {
      // 迟到结果：立刻释放
      void doc.destroy().catch(() => {});
      return { pageCount: 0 };
    }
    this.doc = doc;
    return { pageCount: doc.numPages };
  }

  /** 渲染指定页（1-based）。渲染中重复调用只会保留最后一次请求。 */
  async render(page: number): Promise<void> {
    if (!this.doc || this.destroyed) return;
    const clamped = Math.max(1, Math.min(this.doc.numPages, Math.floor(page)));
    this.pendingPage = clamped;
    if (this.rendering) return; // 已有渲染在跑，结束后会检查 pendingPage

    this.rendering = this.renderLoop();
    await this.rendering;
  }

  private async renderLoop(): Promise<void> {
    while (!this.destroyed) {
      const page = this.pendingPage;
      this.pendingPage = -1;
      if (page < 0 || !this.doc) break;
      try {
        await this.renderOne(page);
        if (!this.destroyed) this.onPageRendered?.(page, this.doc.numPages);
      } catch (err) {
        // destroy 后的取消错误不记日志
        if (!this.destroyed) console.error("PDF 页渲染失败", err);
      }
      // 渲染期间又来了新请求 → 继续循环渲染最新页
      if (this.pendingPage < 0) break;
    }
    this.rendering = null;
  }

  private async renderOne(page: number): Promise<void> {
    const doc = this.doc!;
    const pdfPage = await doc.getPage(page);
    if (this.destroyed) return;

    const viewport = pdfPage.getViewport({ scale: 1 });
    // 按缩放模式计算比例；fit 系列限制最大缩放防止超大位图吃内存
    const fitW = (this.container.clientWidth - 24) / viewport.width;
    let scale: number;
    switch (this.zoom) {
      case "actual":
        scale = 1;
        break;
      case "fit-page":
        scale = Math.min(fitW, (this.container.clientHeight - 24) / viewport.height);
        break;
      default:
        scale = fitW;
    }
    if (this.zoom !== "actual") scale = Math.min(2, Math.max(0.1, scale));
    const vp = pdfPage.getViewport({ scale });

    const ctx = this.canvas.getContext("2d");
    if (!ctx) throw new Error("Canvas 2D 上下文不可用");
    const dpr = window.devicePixelRatio || 1;
    this.canvas.width = Math.floor(vp.width * dpr);
    this.canvas.height = Math.floor(vp.height * dpr);
    this.canvas.style.width = `${Math.floor(vp.width)}px`;
    this.canvas.style.height = `${Math.floor(vp.height)}px`;

    const task = pdfPage.render({
      canvasContext: ctx,
      viewport: vp,
      transform: dpr !== 1 ? [dpr, 0, 0, dpr, 0, 0] : undefined,
    });
    this.renderTask = task;
    try {
      await task.promise;
    } finally {
      if (this.renderTask === task) this.renderTask = null;
    }
    if (this.destroyed) return;
    this.lastPage = page;
  }

  /** 销毁会话：结束 loading/render task，释放 pdf.js 文档内存 */
  destroy(): void {
    this.destroyed = true;
    this.pendingPage = -1;
    try {
      this.renderTask?.cancel();
    } catch {
      /* ignore */
    }
    this.renderTask = null;
    try {
      void this.loadTask?.destroy();
    } catch {
      /* ignore */
    }
    this.loadTask = null;
    void this.doc?.destroy().catch(() => {});
    this.doc = null;
  }
}
