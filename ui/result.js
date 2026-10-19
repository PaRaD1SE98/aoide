const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const copyBtn = $("copy");

function render({ title, text }) {
  $("title").textContent = title;
  $("text").textContent = text;
  $("text").scrollTop = 0;
  copyBtn.textContent = t("result.copy");
}

copyBtn.addEventListener("click", async () => {
  if (await invoke("copy_result")) {
    copyBtn.textContent = t("result.copied");
  }
});

$("close").addEventListener("click", () => invoke("close_result"));

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") invoke("close_result");
});

invoke("get_ui_language").then((lang) => {
  applyI18n(lang);
  return invoke("get_result");
}).then(render);
listen("result-updated", ({ payload }) => render(payload));
