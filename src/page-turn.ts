/** Temporary, on-demand page curl. Real DOM remains the source of layout and interaction.
 * Snapshot images are never taken during animation; the GPU only deforms a small mesh.
 */
const VERTEX = `
precision highp float;
attribute vec2 a_uv;
uniform vec2 u_size;
uniform vec2 u_region;
uniform float u_progress, u_direction, u_leaf, u_hinge, u_width;
varying vec2 v_front, v_back;
varying float v_facing, v_shade;
void main() {
  float x = mix(u_region.x, u_region.y, a_uv.x);
  float y = a_uv.y * u_size.y;
  float z = 0.0;
  v_front = vec2(x / u_size.x, a_uv.y);
  v_back = v_front;
  v_facing = 1.0;
  v_shade = 0.0;
  if (u_leaf > 0.5) {
    // A moving diagonal crease reaches the bottom-right corner first. Points
    // ahead of it stay exactly flat; the curled band follows a cylinder.
    float s = x - u_hinge;
    float tilt = 0.58 * (1.0 - u_progress);
    vec2 normal = vec2(cos(tilt), sin(tilt));
    float radius = max(0.0001, u_width * 0.075 * sin(u_progress * 3.14159265));
    float crease = max(0.0, normal.x * u_width * (1.0 - u_progress) - radius * 1.5707963);
    float distance = dot(vec2(s, y - u_size.y), normal) - crease;
    v_back.x = (u_hinge - s) / u_size.x;
    if (distance > 0.0) {
      float angle = min(distance / radius, 3.14159265);
      float mapped = radius * sin(angle) - max(0.0, distance - radius * 3.14159265);
      vec2 offset = normal * (mapped - distance);
      x += offset.x;
      y += offset.y;
      z = radius * (1.0 - cos(angle));
      v_facing = cos(angle);
      v_shade = 0.14 * sin(angle);
    }
  }
  // Orthographic projection leaves flat text at its original pixel positions.
  vec2 p = vec2(x / u_size.x * 2.0 - 1.0, 1.0 - y / u_size.y * 2.0);
  gl_Position = vec4(p, -z / u_size.x, 1.0);
}`;

const FRAGMENT = `
precision highp float;
uniform sampler2D u_front, u_back;
uniform vec3 u_paper;
uniform float u_leaf, u_spread, u_progress, u_hinge, u_direction;
uniform vec2 u_size;
varying vec2 v_front, v_back;
varying float v_facing, v_shade;
void main() {
  vec4 color = texture2D(u_front, v_front);
  if (u_leaf > 0.5) {
    if (v_facing < 0.0) {
      // Thin paper: only a faint reversed impression on the single-page back.
      // A spread has real content printed on its reverse and keeps that readable.
      color = u_spread > 0.5 ? texture2D(u_back, v_back)
        : vec4(mix(u_paper, color.rgb, 0.11), 1.0);
    }
    // Curvature shading is concentrated at the rim, not spread over the sheet.
    float rim = pow(max(0.0, 1.0 - abs(v_facing)), 2.0);
    float shade = rim * (v_facing < 0.0 ? 0.24 : 0.10);
    color.rgb *= 1.0 - shade;
    if (v_facing < 0.0) {
      float shoulder = exp(-pow((v_facing + 0.65) / 0.23, 2.0));
      color.rgb = mix(color.rgb, u_paper, shoulder * 0.06);
    }
  } else {
    // Cast shadow follows the projected curl silhouette. The former spine-based
    // exponential darkened stationary text even when the leaf was far away.
    float tilt = 0.58 * (1.0 - u_progress);
    vec2 normal = vec2(cos(tilt), sin(tilt));
    vec2 tangent = vec2(-normal.y, normal.x);
    float width = u_size.x - u_hinge;
    float lift = sin(u_progress * 3.14159265);
    float radius = max(0.0001, width * 0.075 * lift);
    float crease = max(0.0, normal.x * width * (1.0 - u_progress) - radius * 1.5707963);
    vec2 point = vec2(v_front.x * u_size.x - u_hinge, (v_front.y - 1.0) * u_size.y);
    float along = dot(point, tangent);
    float away = dot(point, normal) - crease - radius;
    // Clip the shadow to the finite sheet rather than an infinite diagonal stripe.
    vec2 caster = normal * (crease + radius * 1.5707963) + tangent * along;
    float margin = min(min(caster.x, width - caster.x), min(-caster.y, caster.y + u_size.y));
    float extent = smoothstep(-3.0, 7.0, margin);
    float softness = max(1.5, radius * 0.42);
    float penumbra = exp(-pow(max(0.0, away - radius * 0.06) / softness, 2.0));
    float contact = exp(-pow(away / max(0.9, radius * 0.075), 2.0));
    float fade = smoothstep(0.0, 0.12, lift);
    float shadow = (0.20 * penumbra + 0.12 * contact) * extent * fade * smoothstep(-1.0, 1.0, away);
    color.rgb *= 1.0 - shadow;
  }
  gl_FragColor = color;
}`;

function imageFromUrl(url: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    // 图文混排页首次快照需内联全部资源，预算放宽到 3s；配合资源缓存仅首翻承担。
    const timer = window.setTimeout(() => { img.src = ""; reject(new Error("Page image timed out")); }, 3000);
    img.onload = () => { clearTimeout(timer); resolve(img); };
    img.onerror = () => { clearTimeout(timer); reject(new Error("Page image could not be decoded")); };
    img.src = url;
  });
}

async function inlineImage(url: string): Promise<string> {
  if (url.startsWith("data:")) return url;
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), 1200);
  try {
    // Older reader-res responses were cached as immutable without CORS headers.
    // Refresh that HTTP entry once; PageTurn's resourceCache handles reuse.
    const response = await fetch(url, { signal: controller.signal, cache: "reload" });
    if (!response.ok) throw new Error("Page resource unavailable");
    const blob = await response.blob();
    return await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result));
      reader.onerror = reject;
      reader.readAsDataURL(blob);
    });
  } finally { clearTimeout(timer); }
}

/** Use the screen's physical pixel grid for both the snapshot and its overlay.
 * Rounding an SVG's size then stretching it back to fractional CSS dimensions
 * resamples every glyph, even where the sheet is completely flat.
 */
function pixelBounds(rect: DOMRect) {
  const scale = window.devicePixelRatio || 1;
  const left = Math.floor(rect.left * scale);
  const top = Math.floor(rect.top * scale);
  const pixelWidth = Math.ceil(rect.right * scale) - left;
  const pixelHeight = Math.ceil(rect.bottom * scale) - top;
  return { left: left / scale, top: top / scale,
    width: pixelWidth / scale, height: pixelHeight / scale, pixelWidth, pixelHeight };
}

/** Serialize the browser's existing columns, not an independently reflowed text layout.
 *  inline：资源内联回调（实例级缓存，见 PageTurn.inlineResource）。
 */
async function snapshot(
  viewport: HTMLElement,
  host: HTMLElement,
  inline: (url: string) => Promise<string>,
): Promise<HTMLImageElement> {
  const rect = viewport.getBoundingClientRect();
  const copy = viewport.cloneNode(true) as HTMLElement;
  copy.classList.remove("hidden");
  copy.style.cssText += `;position:absolute;inset:0;width:${rect.width}px;height:${rect.height}px;visibility:visible;`;
  const hostCopy = copy.querySelector<HTMLElement>("#page-host")!;
  hostCopy.style.transform = getComputedStyle(host).transform;
  hostCopy.style.transition = "none";

  // Images outside this screen keep their measured dimensions without fetching the book.
  const images = Array.from(host.querySelectorAll("img"));
  const copies = Array.from(hostCopy.querySelectorAll("img"));
  await Promise.all(images.map(async (img, index) => {
    const target = copies[index];
    const bounds = img.getBoundingClientRect();
    target.removeAttribute("srcset");
    target.removeAttribute("loading");
    target.style.width = getComputedStyle(img).width;
    target.style.height = getComputedStyle(img).height;
    // 未加载图没有实测尺寸：有宽高属性时用 aspect-ratio 保住预留盒，避免快照塌陷缺图。
    if (!img.complete) {
      const w = img.getAttribute("width");
      const h = img.getAttribute("height");
      if (w && h) target.style.aspectRatio = `${w} / ${h}`;
    }
    if (bounds.right <= rect.left || bounds.left >= rect.right || bounds.bottom <= rect.top || bounds.top >= rect.bottom) {
      target.src = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='1' height='1'/%3E";
    } else {
      const url = img.currentSrc || img.src;
      if (!url || url.startsWith("data:") || url.startsWith("#")) return;
      target.src = await inline(url);
    }
  }));

  const root = document.createElement("div");
  root.id = "reader";
  root.setAttribute("xmlns", "http://www.w3.org/1999/xhtml");
  const computed = getComputedStyle(viewport);
  // 快照里 :root / body[data-theme] 选择器不匹配（克隆体是 div 而非 html/body），
  // 主题与页边距全靠这里烤入 computed 自定义属性生效——正文新增样式务必只走
  // CSS 变量，不要依赖 body/html 祖先选择器，否则快照颜色会与真实页面不一致。
  for (const key of Array.from(computed)) {
    if (key.startsWith("--")) root.style.setProperty(key, computed.getPropertyValue(key));
  }
  root.style.cssText += `;position:relative;width:${rect.width}px;height:${rect.height}px;background:var(--bg);color:${computed.color};font:${computed.font};`;
  const style = document.createElement("style");
  style.textContent = Array.from(document.styleSheets).map(sheet => {
    try { return Array.from(sheet.cssRules).map(rule => rule.cssText).join("\n"); }
    catch { return ""; }
  }).join("\n") + "\n*{animation:none!important;transition:none!important;}";
  root.append(style, copy);
  // ForeignObject images cannot load external fonts/backgrounds. Embed book CSS
  // resources too, so a custom EPUB font cannot silently reflow the texture.
  for (const element of Array.from(root.querySelectorAll<HTMLElement>("style, [style]"))) {
    const isStyle = element.tagName === "STYLE";
    let text = isStyle ? element.textContent || "" : element.getAttribute("style") || "";
    const matches = Array.from(text.matchAll(/url\(\s*(['"]?)(.*?)\1\s*\)/g));
    for (const match of matches) {
      const url = match[2];
      if (!url || url.startsWith("data:") || url.startsWith("#")) continue;
      const absolute = new URL(url, document.baseURI).href;
      text = text.replace(match[0], `url("${await inline(absolute)}")`);
    }
    if (isStyle) element.textContent = text;
    else element.setAttribute("style", text);
  }
  // CSS Highlight ranges are not DOM nodes; bake their visible rectangles into
  // the temporary image without wrapping text or disturbing column line breaks.
  if ("highlights" in CSS) {
    const registry = CSS.highlights as unknown as Map<string, Set<Range>>;
    for (const [name, ranges] of registry) {
      const color = getComputedStyle(host, `::highlight(${name})`).backgroundColor;
      if (!color || color === "rgba(0, 0, 0, 0)") continue;
      for (const range of ranges) {
        if (!host.contains(range.commonAncestorContainer)) continue;
        for (const box of Array.from(range.getClientRects())) {
          if (box.right <= rect.left || box.left >= rect.right || box.bottom <= rect.top || box.top >= rect.bottom) continue;
          const mark = document.createElement("span");
          mark.style.cssText = `position:absolute;left:${box.left - rect.left}px;top:${box.top - rect.top}px;width:${box.width}px;height:${box.height}px;background:${color};pointer-events:none;`;
          copy.append(mark);
        }
      }
    }
  }
  const body = document.createElement("div");
  body.setAttribute("data-theme", document.body.dataset.theme || "light");
  body.append(root);
  const bounds = pixelBounds(rect);
  const markup = new XMLSerializer().serializeToString(body);
  // An explicit scale also avoids viewBox aspect-ratio rounding changing font
  // rasterization at fractional DPI (notably 175%).
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${bounds.pixelWidth}" height="${bounds.pixelHeight}"><foreignObject transform="scale(${window.devicePixelRatio || 1})" x="${rect.left - bounds.left}" y="${rect.top - bounds.top}" width="${rect.width}" height="${rect.height}">${markup}</foreignObject></svg>`;
  // 必须用 data URL，不能用 blob:：Chromium 把 blob 包装的 foreignObject SVG 当跨源
  // 图像（内部 data: 子资源传播污染），drawImage 后画布被污染，WebGL texImage2D 必抛
  // SecurityError → 每次翻页都降级直跳（无头 WebView 环境实测）。data: 则不污染。
  return imageFromUrl(`data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`);
}

class CurlRenderer {
  readonly canvas = document.createElement("canvas");
  private gl: WebGLRenderingContext;
  private program: WebGLProgram;
  private textures: WebGLTexture[] = [];
  private count = 0;

  constructor() {
    const gl = this.canvas.getContext("webgl", { alpha: false, antialias: true, powerPreference: "low-power", preserveDrawingBuffer: false });
    if (!gl) throw new Error("WebGL is unavailable");
    this.gl = gl;
    const program = gl.createProgram()!;
    for (const [kind, source] of [[gl.VERTEX_SHADER, VERTEX], [gl.FRAGMENT_SHADER, FRAGMENT]] as const) {
      const shader = gl.createShader(kind)!;
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader) || "Shader compilation failed");
      gl.attachShader(program, shader);
      gl.deleteShader(shader);
    }
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) || "Page shader linking failed");
    this.program = program;
    gl.useProgram(program);
    const vertices: number[] = [];
    for (let j = 0; j < 48; j++) {
      for (let i = 0; i < 64; i++) {
        const a = i / 64, b = (i + 1) / 64;
        const top = j / 48, bottom = (j + 1) / 48;
        vertices.push(a, top, a, bottom, b, top, b, top, a, bottom, b, bottom);
      }
    }
    this.count = vertices.length / 2;
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(vertices), gl.STATIC_DRAW);
    const attribute = gl.getAttribLocation(program, "a_uv");
    gl.enableVertexAttribArray(attribute);
    gl.vertexAttribPointer(attribute, 2, gl.FLOAT, false, 0, 0);
    gl.enable(gl.DEPTH_TEST);
    gl.depthFunc(gl.LEQUAL);
  }

  private uniform(name: string): WebGLUniformLocation | null { return this.gl.getUniformLocation(this.program, name); }

  prepare(front: HTMLImageElement, back: HTMLImageElement, width: number, height: number, paper: string): void {
    const gl = this.gl;
    if (gl.isContextLost()) throw new Error("Page graphics context was lost");
    this.release();
    const dpr = window.devicePixelRatio || 1;
    const limit = gl.getParameter(gl.MAX_TEXTURE_SIZE) as number;
    if (width * dpr > limit || height * dpr > limit) throw new Error("Native-resolution page exceeds GPU limits");
    this.canvas.width = Math.max(1, Math.round(width * dpr));
    this.canvas.height = Math.max(1, Math.round(height * dpr));
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.uniform2f(this.uniform("u_size"), width, height);
    const rgb = paper.match(/[\d.]+/g)?.slice(0, 3).map(Number) || [255, 255, 255];
    gl.uniform3f(this.uniform("u_paper"), rgb[0] / 255, rgb[1] / 255, rgb[2] / 255);
    gl.clearColor(rgb[0] / 255, rgb[1] / 255, rgb[2] / 255, 1);
    for (const [index, img] of [front, back].entries()) {
      const texture = gl.createTexture()!;
      this.textures.push(texture);
      gl.activeTexture(gl.TEXTURE0 + index);
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      // Explicit rasterization avoids Chromium's low-resolution SVG upload path.
      const raster = document.createElement("canvas");
      raster.width = img.naturalWidth;
      raster.height = img.naturalHeight;
      const context = raster.getContext("2d")!;
      context.drawImage(img, 0, 0, raster.width, raster.height);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, raster);
      raster.width = raster.height = 1;
    }
    gl.uniform1i(this.uniform("u_back"), 1);
  }

  draw(progress: number, direction: 1 | -1, spread: boolean, width: number): void {
    const gl = this.gl;
    const hinge = spread ? width / 2 : 0;
    // Previous-page navigation reverses the SAME right-hand sheet motion.
    // The sheet returns from the left spine, never from a right-hand hinge.
    const phase = direction === 1 ? progress : 1 - progress;
    const front = direction === 1 ? 0 : 1;
    const back = 1 - front;
    const leafWidth = spread ? width / 2 : width;
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    gl.uniform1f(this.uniform("u_progress"), phase);
    gl.uniform1i(this.uniform("u_back"), back);
    gl.uniform1f(this.uniform("u_direction"), direction);
    gl.uniform1f(this.uniform("u_hinge"), hinge);
    gl.uniform1f(this.uniform("u_width"), leafWidth);
    gl.uniform1f(this.uniform("u_spread"), spread ? 1 : 0);
    const plane = (left: number, right: number, texture: number, leaf: boolean) => {
      gl.uniform2f(this.uniform("u_region"), left, right);
      gl.uniform1i(this.uniform("u_front"), texture);
      gl.uniform1f(this.uniform("u_leaf"), leaf ? 1 : 0);
      gl.drawArrays(gl.TRIANGLES, 0, this.count);
    };
    plane(0, width, back, false);
    if (spread) plane(0, hinge, front, false);
    plane(hinge, hinge + leafWidth, front, true);
  }

  release(): void {
    for (const texture of this.textures) this.gl.deleteTexture(texture);
    this.textures = [];
  }

  /** GPU 重置/驱动恢复后上下文失效；调用方据此丢弃实例、下轮翻页换新 canvas 重建。 */
  get isLost(): boolean { return this.gl.isContextLost(); }
}

/** 一次排队翻页任务；连点合并时 steps 递增，单次动画内连续执行多步 move。 */
interface TurnJob {
  direction: 1 | -1;
  steps: number;
  move: () => Promise<void>;
  canMove: () => boolean;
  version: number;
  resolve: () => void;
  promise: Promise<void>;
}

export class PageTurn {
  active = false;
  moving = false;
  private generation = 0;
  private queueVersion = 0;
  private pending: Promise<void> = Promise.resolve();
  private layoutRevision = 0;
  /** 排队中（未开始执行）的翻页任务：同向连点合并到它，避免 N 连点跑 N 次完整动画。 */
  private queued?: TurnJob;
  /** 快照资源内联缓存（URL → dataURL promise）：同章连续翻页不重复 fetch+base64。
   *  失败结果即刻剔除，FIFO 上限防止跨书累积。 */
  private resourceCache = new Map<string, Promise<string>>();

  /** 通知临时视觉效果：导航或布局已使其失效。 */
  layoutChanged(): void { this.layoutRevision++; }
  private renderer?: CurlRenderer;
  private overlay?: HTMLDivElement;
  private finish?: () => void;
  private frame = 0;

  constructor(private viewport: HTMLElement, private host: HTMLElement) {
    new ResizeObserver(() => this.cancel()).observe(viewport);
    document.addEventListener("visibilitychange", () => { if (document.hidden) this.cancel(); });
  }

  cancel(): void {
    this.queueVersion++;
    this.queued = undefined;
    this.cleanup();
  }

  /** inlineImage 的实例级缓存封装；单条失败不缓存，FIFO 容量 64。 */
  private inlineResource(url: string): Promise<string> {
    let p = this.resourceCache.get(url);
    if (!p) {
      p = inlineImage(url);
      p.catch(() => this.resourceCache.delete(url));
      this.resourceCache.set(url, p);
      if (this.resourceCache.size > 64) {
        const oldest = this.resourceCache.keys().next().value;
        if (oldest !== undefined) this.resourceCache.delete(oldest);
      }
    }
    return p;
  }

  /** 书籍失效边界清理（E09）：丢弃跨书可能滞留的大图 dataURL Promise。 */
  clearResourceCache(): void {
    this.resourceCache.clear();
  }

  private cleanup(): void {
    this.generation++;
    cancelAnimationFrame(this.frame);
    this.finish?.();
    this.finish = undefined;
    this.overlay?.remove();
    this.overlay = undefined;
  }

  play(direction: 1 | -1, move: () => Promise<void>, canMove: () => boolean = () => true): Promise<void> {
    // 同向连点并入待执行任务：N 连点只跑「进行中 + 合并后」至多 2 次动画。
    const queued = this.queued;
    if (queued && queued.direction === direction) {
      queued.steps++;
      return queued.promise;
    }
    let resolve!: () => void;
    const promise = new Promise<void>(r => { resolve = r; });
    const job: TurnJob = { direction, steps: 1, move, canMove, version: this.queueVersion, resolve, promise };
    this.queued = job;
    const chain = this.pending.then(async () => {
      if (this.queued === job) this.queued = undefined;
      try {
        if (job.version === this.queueVersion && job.canMove()) {
          await this.run(job.direction, job.move, job.steps);
        }
      } finally {
        job.resolve(); // 必达：否则调用方 await 永久挂起、pending 链断裂
      }
    });
    this.pending = chain.catch(() => undefined);
    return promise;
  }

  private async run(direction: 1 | -1, move: () => Promise<void>, steps = 1): Promise<void> {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches || document.hidden) {
      for (let i = 0; i < steps; i++) await move();
      return;
    }
    this.active = true;
    const token = ++this.generation;
    let moved = false;
    const transition = this.host.style.transition;
    try {
      // 上下文丢失后整实例重建（新 canvas 换新 context），下一轮翻页自动恢复。
      if (this.renderer?.isLost) this.renderer = undefined;
      this.renderer ??= new CurlRenderer();
      this.host.style.transition = "none";
      const oldPage = await snapshot(this.viewport, this.host, u => this.inlineResource(u));
      if (token !== this.generation) return;
      const rect = pixelBounds(this.viewport.getBoundingClientRect());
      const spread = Number(getComputedStyle(this.host).columnCount) === 2;
      const overlay = document.createElement("div");
      overlay.className = "page-turn-overlay";
      overlay.setAttribute("aria-hidden", "true");
      overlay.inert = true;
      overlay.style.cssText = `position:fixed;left:${rect.left}px;top:${rect.top}px;width:${rect.width}px;height:${rect.height}px;`;
      // CSS layout quantizes fractional lengths to 1/64 px. Keep the image and
      // canvas at integer native sizes and scale in the compositor instead;
      // width/height:100% would introduce a second text-resampling pass.
      const pixelStyle = `width:${rect.pixelWidth}px;height:${rect.pixelHeight}px;max-width:none;display:block;transform-origin:0 0;transform:scale(${1 / (window.devicePixelRatio || 1)});`;
      oldPage.style.cssText = pixelStyle;
      overlay.append(oldPage);
      this.viewport.parentElement!.append(overlay);
      this.overlay = overlay;
      moved = true;
      this.moving = true;
      try {
        // 合并的连点在这里连续执行：每步 move 自带 busy/atEnd 守卫，多余步自动 no-op。
        for (let i = 0; i < steps; i++) {
          if (token !== this.generation) return;
          await move();
        }
      } finally { this.moving = false; }
      if (token !== this.generation || this.viewport.classList.contains("hidden")) return;
      // Visible lazy images get a chance to settle after the column translation.
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()));
      const visibleImages = Array.from(this.host.querySelectorAll("img")).filter(img => {
        const r = img.getBoundingClientRect();
        return r.right > rect.left && r.left < rect.left + rect.width && r.bottom > rect.top && r.top < rect.top + rect.height;
      });
      const decodeAll = Promise.all(visibleImages.map(img => img.decode().catch(() => undefined)));
      let imagesReady = await Promise.race([
        decodeAll.then(() => true),
        new Promise<boolean>(resolve => setTimeout(() => resolve(false), 500)),
      ]);
      if (token !== this.generation) return;
      let revision = this.layoutRevision;
      let newPage = await snapshot(this.viewport, this.host, u => this.inlineResource(u));
      // Image/font layout completion belongs to this turn, not a new navigation.
      // Refresh the prepared image if its geometry changed while it was captured;
      // 500ms 内没等完的慢图（decode 已完成但布局不变的也不重拍过）在这里补拍一次。
      for (let retry = 0; retry < 5 && (revision !== this.layoutRevision || !imagesReady); retry++) {
        if (token !== this.generation) return;
        if (!imagesReady) {
          await Promise.race([decodeAll, new Promise(resolve => setTimeout(resolve, 1000))]);
          imagesReady = true;
        }
        revision = this.layoutRevision;
        newPage = await snapshot(this.viewport, this.host, u => this.inlineResource(u));
      }
      if (token !== this.generation) return;
      // Resolve the theme color to RGB for the opaque paper back.
      overlay.style.backgroundColor = "var(--bg)";
      this.renderer.prepare(oldPage, newPage, rect.width, rect.height, getComputedStyle(overlay).backgroundColor);
      this.renderer.canvas.style.cssText = pixelStyle;
      this.renderer.draw(0, direction, spread, rect.width);
      overlay.replaceChildren(this.renderer.canvas);
      await new Promise<void>(resolve => {
        this.finish = resolve;
        let start = 0, last = -Infinity;
        const tick = (now: number) => {
          if (token !== this.generation) { resolve(); return; }
          if (!start) start = now;
          const t = Math.min(1, (now - start) / 380);
          if (now - last >= 15 || t === 1) {
            const eased = t * t * (3 - 2 * t);
            this.renderer!.draw(eased, direction, spread, rect.width);
            last = now;
          }
          if (t < 1) this.frame = requestAnimationFrame(tick);
          else resolve();
        };
        this.frame = requestAnimationFrame(tick);
      });
    } catch (error) {
      console.warn("Natural page turn unavailable; showing the standard page.", error);
      if (!moved && token === this.generation) {
        this.host.style.transition = transition;
        for (let i = 0; i < steps; i++) await move();
      }
    } finally {
      this.host.style.transition = transition;
      this.renderer?.release();
      this.cleanup();
      this.active = false;
      this.moving = false;
    }
  }
}
