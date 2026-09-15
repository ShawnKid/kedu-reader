// 强制清空 dist/。本机 Vite emptyOutDir 不可靠；Node rmSync 偶发删不掉残留，
// 回退 PowerShell Remove-Item。清理后仍有文件则退出码 1。
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const dist = path.join(__dirname, "..", "dist");

function listFiles(dir) {
  if (!fs.existsSync(dir)) return [];
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...listFiles(full));
    else out.push(full);
  }
  return out;
}

function tryRmSync() {
  if (!fs.existsSync(dist)) return;
  try {
    fs.rmSync(dist, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  } catch {
    /* fall through */
  }
}

function tryPowerShell() {
  if (process.platform !== "win32" || !fs.existsSync(dist)) return;
  spawnSync(
    "powershell.exe",
    ["-NoProfile", "-Command", `Remove-Item -LiteralPath '${dist.replaceAll("'", "''")}' -Recurse -Force -ErrorAction SilentlyContinue`],
    { stdio: "ignore" }
  );
}

tryRmSync();
if (listFiles(dist).length > 0) tryPowerShell();
tryRmSync();

const left = listFiles(dist);
if (left.length > 0) {
  console.error("dist 清理失败，残留文件:");
  for (const f of left) console.error(" ", f);
  process.exit(1);
}
console.log("cleaned dist/");
