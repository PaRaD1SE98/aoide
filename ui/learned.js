// 词条入库提示（屏幕右下角的小条）：列出从用户修改中学到的纠正，提供撤销；约 8 秒后自动关闭，鼠标悬停时暂停
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const SHOW_MS = 8000;
const countdown = createCountdown($("bar"), () => invoke("close_learned"));

function render(items) {
  $("items").replaceChildren(...items.map(({ wrong, right }) => {
    const row = document.createElement("div");
    row.className = "item";
    row.title = `${wrong} → ${right}`;
    const from = Object.assign(document.createElement("span"), { className: "from", textContent: wrong });
    const arrow = Object.assign(document.createElement("span"), { className: "arrow", innerHTML: "&#xE72A;" });
    const to = Object.assign(document.createElement("span"), { className: "to", textContent: right });
    row.append(from, arrow, to);
    return row;
  }));
  $("items").scrollTop = $("items").scrollHeight; // 追加的词条在最后
  countdown.restart(SHOW_MS); // 新学到词条时从满格重新开始
}

$("undo").addEventListener("click", async () => {
  await invoke("undo_learned");
  $("title").textContent = t("learned.undone");
  $("items").style.opacity = 0.4;
  $("undo").disabled = true;
  countdown.stop();
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
