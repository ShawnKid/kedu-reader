/**
 * 前端二次 XSS 过滤（防御纵深）。
 *
 * 后端已用 ammonia 白名单清洗过，这里基于 DOMParser 再做一层轻量过滤，
 * 防止恶意书籍 HTML 注入。不使用正则拼接（容易被绕过），而是解析后删节点/属性。
 */

/** 允许保留的属性（其余一律移除）。epub:type/role 为脚注语义标记，data-fn-text 为后端内嵌注释文本（均为惰性属性）；
 *  data-book-css 为「跟随图书设定」注入样式块的标记属性（普通模式无 style 标签，永不出现） */
const ALLOWED_ATTRS = new Set([
  "id",
  "class",
  "src",
  "alt",
  "title",
  "width",
  "height",
  "href",
  "colspan",
  "rowspan",
  "dir",
  "lang",
  "epub:type",
  "role",
  "data-fn-text",
  "data-book-css",
]);

/** 危险标签整棵移除 */
const REMOVE_TAGS = "script,style,iframe,frame,object,embed,link,meta,form,base,template,audio,video,svg";

/** javascript: 等危险协议 */
const DANGEROUS_URL = /^\s*(javascript|vbscript|data(?!:image\/)|jscript):/i;

/** style 属性/CSS 文本中的危险构造（与后端 harden_css 同规则，防御纵深第二层） */
const DANGEROUS_CSS = /expression\s*\(|behavior\s*:|-moz-binding|javascript\s*:/gi;

export function sanitizeHtml(html: string, opts: { preserveStyles?: boolean } = {}): string {
  // 「跟随图书设定」：保留 class/style 呈现属性与后端注入的 <style data-book-css>；
  // 无 data-book-css 的 style 标签仍按危险标签移除（非注入来源，一律不落地）
  const preserve = opts.preserveStyles === true;
  const doc = new DOMParser().parseFromString(html, "text/html");

  // 1. 移除危险标签
  doc.body.querySelectorAll(REMOVE_TAGS).forEach((el) => {
    if (preserve && el.tagName === "STYLE" && el.hasAttribute("data-book-css")) return;
    el.remove();
  });

  // 1.5 「跟随图书设定」兜底：HTML 解析会把文档开头的 <style> 归入 <head>
  // （"in head" 插入模式），随 body.innerHTML 序列化时整体丢失——领回 body
  if (preserve) {
    doc.head?.querySelectorAll("style[data-book-css]").forEach((el) => doc.body.prepend(el));
  }

  // 2. 逐元素过滤属性
  doc.body.querySelectorAll("*").forEach((el) => {
    for (const attr of Array.from(el.attributes)) {
      const name = attr.name.toLowerCase();
      // 事件处理器 on*
      if (name.startsWith("on")) {
        el.removeAttribute(attr.name);
        continue;
      }
      // 非 URL 属性但不在白名单（跟随模式额外放行 style 惰性呈现属性）
      if (!ALLOWED_ATTRS.has(name) && !(preserve && name === "style")) {
        el.removeAttribute(attr.name);
        continue;
      }
      // URL 属性协议检查（src/href，含内联 data:image 放行）
      if ((name === "src" || name === "href") && DANGEROUS_URL.test(attr.value)) {
        el.removeAttribute(attr.name);
        continue;
      }
      // style 属性值硬化：动态构造直接剔除（无命中时 replace 原样返回）
      if (name === "style" && DANGEROUS_CSS.test(attr.value)) {
        DANGEROUS_CSS.lastIndex = 0;
        el.setAttribute("style", attr.value.replace(DANGEROUS_CSS, ""));
      }
    }
  });

  return doc.body.innerHTML;
}
