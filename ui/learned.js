// 词条入库提示：列出从用户修改中学到的纠正，提供撤销；约 8 秒后自动关闭，鼠标悬停时暂停
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const SHOW_MS = 8000;
const bar = $("bar");
let timer;
let remaining = SHOW_MS; // 距离自动关闭还剩多久
let startedAt = 0;
let paused = false;

function render(items) {
  $("items").replaceChildren(...items.map(({ wrong, right, count }) => {
    const row = document.createElement("div");
    row.className = "item";
    const from = Object.assign(document.createElement("span"), { className: "from", textContent: wrong });
    const arrow = Object.assign(document.createElement("span"), { className: "arrow", innerHTML: "&#xE72A;" });
    const to = Object.assign(document.createElement("span"), { className: "to", textContent: right });
    row.append(from, arrow, to);
    // 同一纠正学到第 2 次起，之后在本地直接替换
    if (count >= 2) {
      row.append(Object.assign(document.createElement("span"), { className: "note", textContent: t("learned.times", { n: count }) }));
    }
    return row;
  }));
  restart();
}

// === 自动关闭倒计时：进度条和计时器同步，悬停时一起暂停 ===
function run() {
  clearTimeout(timer);
  startedAt = performance.now();
  timer = setTimeout(() => invoke("close_learned"), remaining);
  bar.style.animationPlayState = paused ? "paused" : "running";
  if (paused) clearTimeout(timer);
}

/** 从满格重新开始（新学到词条时） */
function restart() {
  remaining = SHOW_MS;
  bar.style.animation = "none";
  void bar.offsetWidth; // 让动画从头播放
  bar.style.animation = "";
  bar.style.animationDuration = `${SHOW_MS}ms`;
  run();
}

document.body.addEventListener("mouseenter", () => {
  if (paused) return;
  paused = true;
  clearTimeout(timer);
  remaining = Math.max(0, remaining - (performance.now() - startedAt));
  bar.style.animationPlayState = "paused";
});
document.body.addEventListener("mouseleave", () => {
  paused = false;
  run();
});

$("undo").addEventListener("click", async () => {
  await invoke("undo_learned");
  $("title").textContent = t("learned.undone");
  $("items").style.opacity = 0.4;
  $("undo").disabled = true;
  bar.parentElement.style.visibility = "hidden";
  clearTimeout(timer);
  setTimeout(() => invoke("close_learned"), 900);
});

$("close").addEventListener("click", () => invoke("close_learned"));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") invoke("close_learned");
});

invoke("get_ui_language").then((lang) => {
  applyI18n(lang);
  return invoke("get_learned");
}).then(render);
listen("learned-updated", ({ payload }) => render(payload));
