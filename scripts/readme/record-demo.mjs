// 逐帧录制 README 演示动画：无头浏览器打开 scene-demo.html，每帧调用 render(t) 后截图
// 通过 Chrome DevTools Protocol 直接通信（Node 22+ 自带 WebSocket），不依赖第三方库
//
// 用法：node record-demo.mjs <浏览器 exe> <场景 URL> <输出目录> <时长 ms> <fps> <宽> <高> <缩放>
//
// 无头 Edge 连续截图一分钟左右后会越来越慢直至卡住，所以分段录制：每段换一个全新的浏览器，
// 某帧超时也换浏览器从这一帧继续；已存在的帧直接跳过（中断后重跑即可续录）

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [browser, url, outDir, durationMs, fps, width, height, scale] = process.argv.slice(2);
const FRAMES_PER_BROWSER = 150;
const TIMEOUT_MS = 15000;
const total = Math.round((+durationMs / 1000) * +fps);
const framePath = (i) => join(outDir, `frame-${String(i).padStart(5, "0")}.png`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
mkdirSync(outDir, { recursive: true });

/** 启动一个无头浏览器并打开场景页，返回 { capture(i), close() } */
async function openBrowser() {
  const profile = mkdtempSync(join(tmpdir(), "aoide-demo-profile-"));
  // Edge 启动后会把自己交给子进程继续运行，stderr 拿不到调试地址；改读用户数据目录里的 DevToolsActivePort
  spawn(browser, [
    "--headless=new", "--disable-gpu", "--hide-scrollbars", "--remote-debugging-port=0",
    `--user-data-dir=${profile}`, "about:blank",
  ], { stdio: "ignore", detached: true }).unref();
  let wsUrl;
  for (let i = 0; i < 150 && !wsUrl; i++) {
    try {
      const [port, path] = readFileSync(join(profile, "DevToolsActivePort"), "utf8").trim().split(/\r?\n/);
      if (port && path) wsUrl = `ws://127.0.0.1:${port}${path}`;
    } catch {}
    if (!wsUrl) await sleep(100);
  }
  if (!wsUrl) throw new Error("browser did not start");

  const ws = new WebSocket(wsUrl);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  let nextId = 0;
  const pending = new Map();
  const waiters = [];
  ws.addEventListener("message", ({ data }) => {
    const msg = JSON.parse(data);
    if (msg.id !== undefined) {
      const p = pending.get(msg.id);
      if (!p) return; // 已超时放弃的命令，响应迟到
      pending.delete(msg.id);
      msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result);
    } else {
      for (const w of waiters.splice(0)) w(msg);
    }
  });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out`));
    }, TIMEOUT_MS);
    pending.set(id, {
      resolve: (v) => (clearTimeout(timer), resolve(v)),
      reject: (e) => (clearTimeout(timer), reject(e)),
    });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });
  const once = (method) => new Promise((resolve) => {
    const check = (msg) => (msg.method === method ? resolve(msg) : waiters.push(check));
    waiters.push(check);
  });

  // 记下浏览器的全部进程，关闭时逐个结束（卡住的浏览器不一定响应 Browser.close）
  const pids = [];
  const close = async () => {
    await send("Browser.close").catch(() => {});
    ws.close();
    await sleep(500);
    for (const pid of pids) {
      try { process.kill(pid); } catch {}
    }
    await sleep(300);
    // 浏览器的子进程有时退出得更晚，还占着用户数据目录里的文件：删不掉就留下，不能因此中断录制
    try {
      rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 });
    } catch (e) {
      console.log(`  could not remove ${profile} (${e.code}), leaving it`);
    }
  };

  try {
    const info = await send("SystemInfo.getProcessInfo").catch(() => ({ processInfo: [] }));
    pids.push(...info.processInfo.map((p) => p.id));
    const { targetInfos } = await send("Target.getTargets");
    const { targetId } = targetInfos.find((t) => t.type === "page");
    const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
    const s = (method, params) => send(method, params, sessionId);
    await s("Emulation.setDeviceMetricsOverride", { width: +width, height: +height, deviceScaleFactor: +scale, mobile: false });
    await s("Page.enable");
    const loaded = once("Page.loadEventFired");
    await s("Page.navigate", { url });
    await loaded;
    const ev = async (expression) => {
      const r = await s("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
      return r.result.value;
    };
    await ev("window.ready");

    const capture = async (i) => {
      await ev(`render(${(i * 1000) / +fps})`);
      const { data } = await s("Page.captureScreenshot", { format: "png" });
      writeFileSync(framePath(i), Buffer.from(data, "base64"));
    };
    return { capture, close };
  } catch (e) {
    await close();
    throw e;
  }
}

let failures = 0;
let i = 0;
while (i < total) {
  while (i < total && existsSync(framePath(i))) i++;
  if (i >= total) break;
  const b = await openBrowser();
  try {
    for (let n = 0; n < FRAMES_PER_BROWSER && i < total; n++, i++) {
      if (existsSync(framePath(i))) continue;
      await b.capture(i);
      failures = 0;
    }
    console.log(`  ${i}/${total}`);
  } catch (e) {
    if (++failures >= 3) throw new Error(`frame ${i}: ${e.message}`);
    console.log(`  frame ${i}: ${e.message}, restarting browser`);
  } finally {
    await b.close();
  }
}
console.log(`  ${total}/${total} frames`);
