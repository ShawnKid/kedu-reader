/**
 * 「跟随图书设定」书籍 CSS 作用域化（从 main.ts 机械提取，行为不变）。
 * 纯函数，不依赖应用状态。
 */

/** 作用域前缀：分页正文宿主 + 滚动模式章容器（与 index.html / styles.css 对应） */
export const BOOK_CSS_SCOPE = ":is(#page-host, .scroll-chapter)";

/**
 * 作用域根（html/body）上禁止书籍覆盖的布局属性。
 * 这些属性由阅读器框架持有：分页 clip-path / 步长按 host margin 计算，
 * 书籍 body{margin:0} 一旦盖掉 margin，可用宽度会撑满视口却仍被
 * clip-path 按页边距裁切 → 两侧文字被遮挡。
 */
const ROOT_FORBIDDEN_PROPS = new Set([
  "margin",
  "margin-inline",
  "margin-left",
  "margin-right",
  "margin-top",
  "margin-bottom",
  "margin-inline-start",
  "margin-inline-end",
  "padding",
  "padding-inline",
  "padding-left",
  "padding-right",
  "padding-top",
  "padding-bottom",
  "padding-inline-start",
  "padding-inline-end",
  "width",
  "height",
  "min-width",
  "min-height",
  "max-width",
  "max-height",
  "box-sizing",
  "position",
  "top",
  "left",
  "right",
  "bottom",
  "inset",
  "display",
  "float",
  "clear",
  "transform",
  "translate",
  "column-count",
  "column-width",
  "column-gap",
  "columns",
  "column-fill",
  "overflow",
  "overflow-x",
  "overflow-y",
  "contain",
  "content-visibility",
  // 根上的字号/行距由阅读器滑条控制；子元素上的绝对字号仍可跟随图书
  "font-size",
  "line-height",
  "font",
]);

/** 作用域根规则只保留字体/颜色/行高等排版外观，去掉布局属性 */
function filterRootStyleText(style: CSSStyleDeclaration): string {
  const parts: string[] = [];
  for (let i = 0; i < style.length; i++) {
    const prop = style.item(i);
    if (ROOT_FORBIDDEN_PROPS.has(prop.toLowerCase())) continue;
    const value = style.getPropertyValue(prop);
    const prio = style.getPropertyPriority(prop);
    parts.push(prio ? `${prop}:${value} ${prio}` : `${prop}:${value}`);
  }
  return parts.join(";");
}

/**
 * 「跟随图书设定」：书籍 CSS 作用域化。
 * 后端把净化后的书籍 CSS 以 <style data-book-css> 前置注入章节 HTML；
 * 这里用 CSSOM 解析后逐条选择器加前缀，避免书籍规则泄漏到整个应用界面。
 * CSSOM 不可用或解析失败时整体去样式（安全兜底：宁缺毋滥）。
 */
export function processBookStyles(html: string): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  const styleEls = Array.from(doc.body.querySelectorAll("style[data-book-css]"));
  if (styleEls.length === 0) return html;

  const cssText = styleEls.map((el) => el.textContent ?? "").join("\n");
  styleEls.forEach((el) => el.remove());
  // 兜底：其余来源的 style 节点一律不落地
  doc.body.querySelectorAll("style").forEach((el) => el.remove());

  const scoped = scopeBookCss(cssText);
  if (scoped) {
    const style = doc.createElement("style");
    style.setAttribute("data-book-css", "");
    style.textContent = scoped;
    doc.body.prepend(style);
  }
  return doc.body.innerHTML;
}

/** 书籍 CSS → 作用域化 CSS（解析失败返回空串 = 去样式兜底） */
export function scopeBookCss(css: string): string {
  try {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(css);
    const emit = (rules: CSSRuleList): string => {
      const parts: string[] = [];
      for (const rule of Array.from(rules)) {
        if (rule instanceof CSSStyleRule) {
          const sel = scopeSelector(rule.selectorText);
          if (!sel) continue;
          const body = sel === BOOK_CSS_SCOPE ? filterRootStyleText(rule.style) : rule.style.cssText;
          if (body) parts.push(`${sel}{${body}}`);
        } else if (rule instanceof CSSMediaRule || rule instanceof CSSSupportsRule) {
          const at = rule instanceof CSSMediaRule ? "@media" : "@supports";
          const inner = emit(rule.cssRules);
          if (inner && rule.conditionText) parts.push(`${at} ${rule.conditionText}{${inner}}`);
        } else if (
          rule instanceof CSSFontFaceRule ||
          rule instanceof CSSKeyframesRule ||
          rule instanceof CSSPageRule
        ) {
          parts.push(rule.cssText);
        }
      }
      return parts.join("");
    };
    return emit(sheet.cssRules);
  } catch {
    return "";
  }
}

/** 单条选择器作用域化：剥掉开头 html/body 段后加前缀；纯根选择器映射为作用域根 */
export function scopeSelector(sel: string): string | null {
  let t = sel.trim();
  if (!t) return null;
  for (;;) {
    const m = t.match(/^(html|body)\b[\s>+~]*/i);
    if (!m) break;
    t = t.slice(m[0].length);
  }
  if (!t) return BOOK_CSS_SCOPE;
  return `${BOOK_CSS_SCOPE} ${t}`;
}
