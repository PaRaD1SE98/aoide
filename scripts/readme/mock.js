// README 截图用：模拟 Tauri 接口，让真实的界面文件在普通浏览器里渲染。
// 语言取自 URL 参数 ?lang=en|zh|ja，设置页分区取自 ?section=。由 scripts/readme-images.ps1 注入到 ui/*.html 的副本中。
(() => {
  const params = new URLSearchParams(location.search);
  const lang = ["zh", "en", "ja"].includes(params.get("lang")) ? params.get("lang") : "en";
  const pick = (texts) => texts[lang];
  try {
    localStorage.setItem("aoide.section", params.get("section") || "hotkeys");
  } catch {}

  // 截图不要过渡动画（无头浏览器的虚拟时钟不推进动画，开关会停在半路）
  const style = document.createElement("style");
  style.textContent = "*, *::before, *::after { transition: none !important; animation: none !important; }";
  document.head.append(style);

  const models = [
    { id: "sense-voice", engine: "sense_voice", size_mb: 239, downloaded: true,
      note: ["推荐：中文更准、速度极快、不易幻觉（中英日韩粤）", "Recommended: accurate for Chinese, very fast, rarely hallucinates (zh/en/ja/ko/yue)", "おすすめ：中国語に強く、非常に高速で、幻覚が起きにくい（中英日韓粵）"] },
    { id: "sense-voice-yue", engine: "sense_voice", size_mb: 237, downloaded: false, note: ["粤语增强版", "Cantonese-tuned", "広東語強化版"] },
    { id: "large-v3-turbo", engine: "whisper", size_mb: 1620, downloaded: false, note: ["又快又准", "Fast and accurate", "速くて正確"] },
  ];

  const settings = {
    config: {
      ui_language: lang, hotkey: "right alt", revise_hotkey: "right alt+space", revise_stop_hotkey: "right alt",
      cancel_hotkey: "esc", model: "sense-voice", model_dir: "", language: lang, max_duration: 90, min_duration: 3,
      device: "gpu", mute_while_recording: true, polish: true, polish_api: "https://api.openai.com/v1",
      polish_api_key: "sk-example", polish_model: "gpt-4o-mini", polish_no_think: true, polish_min_chars: 12,
      polish_min_seconds: 10, polish_prompt: "", revise_prompt: "", insert_prompt: "",
      insert_smart: true, insert_min_seconds: 10, insert_min_chars: 12, vocab_enabled: true, learn_from_edits: true, auto_check_updates: true,
    },
    models,
    default_model_dir: "C:\\Users\\you\\AppData\\Local\\Aoide\\models",
    log_dir: "C:\\Users\\you\\AppData\\Roaming\\Aoide",
    autostart: true,
    version: "0.4.1",
    system_language: lang,
    defaults: {
      ui_language: "auto", hotkey: "right alt", revise_hotkey: "right alt+space", revise_stop_hotkey: "right alt",
      cancel_hotkey: "esc", model: "sense-voice", model_dir: "", language: "auto", max_duration: 90, min_duration: 3,
      device: "gpu", mute_while_recording: true, polish: true, polish_api: "", polish_api_key: "", polish_model: "",
      polish_no_think: true, polish_min_chars: 12, polish_min_seconds: 10, polish_prompt: "", revise_prompt: "", insert_prompt: "",
      insert_smart: true, insert_min_seconds: 10, insert_min_chars: 12, vocab_enabled: true, learn_from_edits: true, auto_check_updates: true,
    },
    default_polish_prompt: "…",
    default_revise_prompt: "…",
    default_insert_prompt: "…",
  };

  const result = pick({
    zh: { title: "没有检测到输入框，识别结果如下",
          text: "下周的发布计划调整如下：\n1. 周二完成安装包的最终测试\n2. 周三发布到 GitHub Releases\n3. 周四在社区发布更新说明" },
    en: { title: "No input box detected. Here is the transcript",
          text: "Here's the plan for next week's release:\n1. Finish final installer testing on Tuesday\n2. Publish to GitHub Releases on Wednesday\n3. Post the release notes to the community on Thursday" },
    ja: { title: "入力欄が見つからないため、認識結果をここに表示します",
          text: "来週のリリース予定は次のとおりです：\n1. 火曜日にインストーラーの最終テストを終える\n2. 水曜日に GitHub Releases で公開する\n3. 木曜日にコミュニティでリリースノートを告知する" },
  });

  const vocab = [
    { id: 3, term: "Aoide", manual: true, enabled: true, variants: [{ text: pick({ zh: "奥伊德", en: "Aida", ja: "あおいで" }), count: 0, manual: true }] },
    { id: 2, term: pick({ zh: "思维导图", en: "Tauri", ja: "議事録" }), manual: false, enabled: true,
      variants: [{ text: pick({ zh: "思维到图", en: "tori", ja: "義事録" }), count: 2, manual: false }] },
    { id: 1, term: "SenseVoice", manual: true, enabled: true, variants: [] },
  ];
  const responses = { get_settings: settings, list_models: models, get_ui_language: lang, get_result: result, get_vocab: vocab,
    get_learned: pick({
      zh: [{ wrong: "思维到图", right: "思维导图", count: 1 }, { wrong: "奥伊德", right: "Aoide", count: 2 }],
      en: [{ wrong: "tori", right: "Tauri", count: 1 }, { wrong: "Aida", right: "Aoide", count: 2 }],
      ja: [{ wrong: "義事録", right: "議事録", count: 1 }, { wrong: "あおいで", right: "Aoide", count: 2 }],
    }) };
  window.__TAURI__ = {
    core: { invoke: async (cmd) => responses[cmd] ?? null },
    event: { listen: async () => {} },
    dialog: { open: async () => null },
    window: { getCurrentWindow: () => ({ onCloseRequested: async () => {} }) },
  };
})();
