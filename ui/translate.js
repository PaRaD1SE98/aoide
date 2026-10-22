const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let view = null;
let busy = false;

// 选语言（翻译录音时）：选定后存进设置，并把焦点还给原来的输入框
const pickerLang = languagePicker($("picker-lang"), $("picker-list"), {
  autoWidth: true,
  anchor: $("picker"),
  onPick: (language) => invoke("set_translate_language", { language, restoreFocus: true }).catch((e) => console.error(e)),
  onCancel: () => invoke("set_translate_language", { language: view.language, restoreFocus: true }).catch(() => {}),
  grow,
});

// 核对（翻译输出后）：换语言只记下，点「重新翻译」时生效。
// 卡片里语言框上方的空间放得下整个列表，就在卡片里往上展开、不改窗口大小：
// WebView 窗口改尺寸时旧画面会先按新尺寸显示一帧，整个小窗会闪一下
const reviewLang = languagePicker($("review-lang"), $("review-list"));

/** 选语言的小条只有一行高，放不下列表：小窗向上长高，列表超出原来的上边界（底边不动） */
function grow(extra) {
  invoke("resize_translate_popup", { extra }).catch(() => {});
}

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
