// 提示小窗：显示一条消息（代替系统通知），几秒后自动关闭；消息越长停留越久，鼠标悬停时暂停
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const close = () => invoke("close_notice");
const countdown = createCountdown($("bar"), close);

function render(message) {
  $("text").textContent = message;
  $("text").scrollTop = 0;
  countdown.restart(Math.min(12000, 5000 + message.length * 80));
}

$("close").addEventListener("click", close);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") close();
});

invoke("get_ui_language").then((lang) => {
  applyI18n(lang);
  return invoke("get_notice");
}).then(render);
listen("notice-updated", ({ payload }) => render(payload));
