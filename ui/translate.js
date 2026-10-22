const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let view = null;
let busy = false;

// 选语言（翻译录音时）：选定后存进设置，并把焦点还给原来的输入框
const pickerLang = languagePicker($("picker-lang"), $("picker-list"), {
  autoWidth: true,
  anchor: $("picker"),
  up: true,
  onToggle: reportShape,
  onPick: (language) => invoke("set_translate_language", { language, restoreFocus: true }).catch((e) => console.error(e)),
  onCancel: () => invoke("set_translate_language", { language: view.language, restoreFocus: true }).catch(() => {}),
});

// 核对（翻译输出后）：换语言只记下，点「重新翻译」时生效
const reviewLang = languagePicker($("review-lang"), $("review-list"), { up: true, onToggle: reportShape });

/**
 * 告诉程序小窗的哪些部分是可见的：小条 / 卡片，加上展开着的候选列表（各留出阴影的位置）。
 * 小窗上方一开始就给列表留好了空间，展开、收起时只改有效形状，不改窗口大小，不会闪烁；
 * 没用到的透明部分不挡鼠标，点击落到下面的程序
 */
function reportShape() {
  const shadow = 8;
  const main = view?.mode === "review" ? document.querySelector(".card") : $("picker");
  const parts = [main, ...[$("picker-list"), $("review-list")].filter((l) => !l.hidden)];
  const rects = parts.map((el) => {
    const r = el.getBoundingClientRect();
    return [r.left - shadow, r.top - shadow, r.width + shadow * 2, r.height + shadow * 2];
  });
  invoke("set_translate_shape", { rects }).catch(() => {});
}
addEventListener("resize", () => requestAnimationFrame(reportShape));

function setStatus(key, vars, isError = false) {
  const el = $("status");
  el.textContent = key ? t(key, vars) : "";
  el.classList.toggle("error", isError);
}

function render(v) {
  view = v;
  document.body.className = v.mode;
  pickerLang.set(v.language);
  reviewLang.set(v.language);
  requestAnimationFrame(reportShape);
  if (v.mode !== "review") return;
  $("original").value = v.original;
  $("translation").textContent = v.translation;
  $("copy").disabled = !v.translation;
  $("copy").textContent = t("translate.copy");
  if (v.error) {
    const el = $("status");
    el.textContent = v.error;
    el.classList.add("error");
  } else {
    setStatus(v.translation && !v.pasted ? "translate.notPasted" : "");
  }
}

async function retranslate() {
  if (busy) return;
  const original = $("original").value.trim();
  const language = $("review-lang").dataset.value || view.language;
  if (!original) return;
  busy = true;
  $("retranslate").disabled = true;
  setStatus("translate.working");
  try {
    const translation = await invoke("retranslate", { original, language });
    view = { ...view, original, language, translation, error: "" };
    $("translation").textContent = translation;
    $("copy").disabled = false;
    setStatus(view.pasted ? "translate.replaced" : "translate.done");
  } catch (e) {
    const el = $("status");
    el.textContent = String(e);
    el.classList.add("error");
    // 没能替换时新译文也已经生成，刷新显示
    invoke("get_translate").then((v) => {
      if (v.translation) $("translation").textContent = v.translation;
    });
  } finally {
    busy = false;
    $("retranslate").disabled = false;
  }
}

$("retranslate").addEventListener("click", retranslate);
$("original").addEventListener("keydown", (e) => {
  // Ctrl+Enter / ⌘Enter：重新翻译
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    retranslate();
  }
});
$("copy").addEventListener("click", async () => {
  if (await invoke("copy_translation")) $("copy").textContent = t("translate.copied");
});
$("close").addEventListener("click", () => invoke("close_translate"));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && view?.mode === "review") invoke("close_translate");
});

invoke("get_ui_language").then((lang) => {
  applyI18n(lang);
  return invoke("get_translate");
}).then(render);
listen("translate-updated", ({ payload }) => render(payload));
