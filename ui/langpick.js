// 语言选择框（设置页、语音翻译小窗共用）：可以直接填写任意语言名，也可以从常用语言里选；输入时按名称过滤。
// value 是填进翻译提示词的语言名（用英文写，LLM 最容易理解），列表里按界面语言显示名称

const LANGUAGES = [
  { value: "English", zh: "英语", en: "English", ja: "英語" },
  { value: "Simplified Chinese", zh: "简体中文", en: "Simplified Chinese", ja: "簡体字中国語" },
  { value: "Traditional Chinese", zh: "繁体中文", en: "Traditional Chinese", ja: "繁体字中国語" },
  { value: "Japanese", zh: "日语", en: "Japanese", ja: "日本語" },
  { value: "Korean", zh: "韩语", en: "Korean", ja: "韓国語" },
  { value: "French", zh: "法语", en: "French", ja: "フランス語" },
  { value: "German", zh: "德语", en: "German", ja: "ドイツ語" },
  { value: "Spanish", zh: "西班牙语", en: "Spanish", ja: "スペイン語" },
  { value: "Portuguese", zh: "葡萄牙语", en: "Portuguese", ja: "ポルトガル語" },
  { value: "Italian", zh: "意大利语", en: "Italian", ja: "イタリア語" },
  { value: "Russian", zh: "俄语", en: "Russian", ja: "ロシア語" },
  { value: "Arabic", zh: "阿拉伯语", en: "Arabic", ja: "アラビア語" },
  { value: "Hindi", zh: "印地语", en: "Hindi", ja: "ヒンディー語" },
  { value: "Thai", zh: "泰语", en: "Thai", ja: "タイ語" },
  { value: "Vietnamese", zh: "越南语", en: "Vietnamese", ja: "ベトナム語" },
  { value: "Indonesian", zh: "印尼语", en: "Indonesian", ja: "インドネシア語" },
  { value: "Malay", zh: "马来语", en: "Malay", ja: "マレー語" },
  { value: "Filipino", zh: "菲律宾语", en: "Filipino", ja: "フィリピン語" },
  { value: "Turkish", zh: "土耳其语", en: "Turkish", ja: "トルコ語" },
  { value: "Dutch", zh: "荷兰语", en: "Dutch", ja: "オランダ語" },
  { value: "Polish", zh: "波兰语", en: "Polish", ja: "ポーランド語" },
  { value: "Ukrainian", zh: "乌克兰语", en: "Ukrainian", ja: "ウクライナ語" },
  { value: "Swedish", zh: "瑞典语", en: "Swedish", ja: "スウェーデン語" },
  { value: "Cantonese", zh: "粤语", en: "Cantonese", ja: "広東語" },
];

/** 语言名在当前界面语言下的显示名称；不在常用列表里的原样显示 */
function languageLabel(value) {
  const lang = LANGUAGES.find((l) => l.value.toLowerCase() === String(value).trim().toLowerCase());
  return lang ? lang[LANG] || lang.en : value;
}

/** 候选列表的最大高度、和输入框的间距、最小宽度（放得下「简体中文 Simplified Chinese」） */
const LIST_MAX = 220;
const LIST_GAP = 4;
const LIST_MIN_WIDTH = 240;

/**
 * 把 input 变成语言选择框：input 显示和编辑语言名，list（一个空的容器）放候选列表。
 * onPick(value) 在选定时调用（点选候选、或输入后按回车）；onCancel() 在按 Esc 收起时调用；
 * onToggle(open) 在列表展开、收起后调用（翻译小窗据此更新窗口的有效形状）。
 * 候选列表默认往下面放得下的一侧展开（下面不够就往上）；up：总是往上（翻译小窗上方留好了空间）。
 * anchor：列表按哪个元素水平对齐（默认输入框），列表居中于它
 */
function languagePicker(input, list, { onPick, onCancel, onToggle, up: alwaysUp = false, anchor = input, autoWidth = false } = {}) {
  let items = [];
  let active = -1;
  const canvas = autoWidth && document.createElement("canvas");

  /** autoWidth：输入框宽度跟着文字走（语言名长短不一，固定宽度时右边常常空着） */
  function fit() {
    if (!canvas) return;
    const cs = getComputedStyle(input);
    const ctx = canvas.getContext("2d");
    ctx.font = `${cs.fontSize} ${cs.fontFamily}`;
    const text = ctx.measureText(input.value || " ").width;
    const pad = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight) + 4;
    input.style.width = `${Math.ceil(Math.min(Math.max(text + pad, 64), 168))}px`;
  }

  const open = () => !list.hidden;

  function render() {
    const q = input.value.trim().toLowerCase();
    // 输入框里还是当前选中的语言时显示全部候选
    const showAll = !q || input.dataset.value?.toLowerCase() === q || languageLabel(input.dataset.value || "").toLowerCase() === q;
    items = LANGUAGES.filter((l) => showAll || [l.value, l.zh, l.en, l.ja].some((s) => s.toLowerCase().includes(q)));
    active = items.length ? 0 : -1;
    list.replaceChildren(
      ...items.map((l, i) => {
        const el = document.createElement("div");
        el.className = "lang-item";
        el.textContent = l[LANG] || l.en;
        if (l[LANG] !== l.value) {
          const sub = document.createElement("span");
          sub.className = "lang-sub";
          sub.textContent = l.value;
          el.append(sub);
        }
        // mousedown 而不是 click：先于输入框失去焦点触发
        el.addEventListener("mousedown", (e) => {
          e.preventDefault();
          pick(l.value);
        });
        el.addEventListener("mouseenter", () => highlight(i));
        return el;
      })
    );
    if (q && !showAll && !items.some((l) => l.value.toLowerCase() === q)) {
      // 不在列表里的语言：直接用输入的名字
      const custom = document.createElement("div");
      custom.className = "lang-item custom";
      custom.textContent = t("lang.useTyped", { name: input.value.trim() });
      custom.addEventListener("mousedown", (e) => {
        e.preventDefault();
        pick(input.value.trim());
      });
      list.append(custom);
      if (active < 0) active = items.length;
    }
    highlight(active);
  }

  function highlight(i) {
    active = i;
    [...list.children].forEach((el, k) => el.classList.toggle("active", k === i));
    // 只滚动列表本身（scrollIntoView 会连带滚动页面，小窗里整条会被推出窗口）
    const el = list.children[i];
    if (el) {
      if (el.offsetTop < list.scrollTop) list.scrollTop = el.offsetTop;
      else if (el.offsetTop + el.offsetHeight > list.scrollTop + list.clientHeight) {
        list.scrollTop = el.offsetTop + el.offsetHeight - list.clientHeight;
      }
    }
  }

  /** 定位列表：水平居中于 anchor（不超出窗口），竖直方向贴着输入框往上或往下 */
  function place() {
    const box = input.getBoundingClientRect();
    const a = anchor.getBoundingClientRect();
    const width = Math.min(Math.max(a.width, LIST_MIN_WIDTH), innerWidth - 16);
    const left = Math.min(Math.max(a.left + (a.width - width) / 2, 8), innerWidth - 8 - width);
    Object.assign(list.style, { left: `${left}px`, width: `${width}px`, top: "", bottom: "" });
    const below = innerHeight - box.bottom - LIST_GAP - 8;
    const above = box.top - LIST_GAP - 8;
    const up = alwaysUp || (below < Math.min(LIST_MAX, list.scrollHeight) && above > below);
    if (up) {
      list.style.bottom = `${innerHeight - box.top + LIST_GAP}px`;
      list.style.maxHeight = `${Math.min(LIST_MAX, above)}px`;
    } else {
      list.style.top = `${box.bottom + LIST_GAP}px`;
      list.style.maxHeight = `${Math.min(LIST_MAX, below)}px`;
    }
  }

  function show() {
    if (open()) return;
    list.hidden = false;
    render();
    place();
    onToggle?.(true);
  }

  function hide() {
    if (!open()) return;
    list.hidden = true;
    onToggle?.(false);
  }

  // 列表浮在页面上，不随页面滚动：页面滚动或窗口大小改变时跟着输入框重新定位（列表自己滚动不算）。
  // 点进输入框时浏览器可能把它滚进视野，这时不能收起列表
  const follow = (e) => open() && e?.target !== list && place();
  document.addEventListener("scroll", follow, true);
  addEventListener("resize", follow);

  function pick(value) {
    value = value.trim();
    if (!value) return;
    set(value);
    hide();
    input.blur();
    onPick?.(value);
  }

  /** 设置当前语言（显示名称，记下原始值） */
  function set(value) {
    input.dataset.value = value;
    input.value = languageLabel(value);
    fit();
  }

  input.addEventListener("focus", () => {
    input.select();
    show();
  });
  input.addEventListener("input", () => {
    fit();
    show();
    render();
  });
  input.addEventListener("blur", () => {
    hide();
    set(input.dataset.value || ""); // 没选就离开：恢复原来的语言
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      show();
      const n = list.children.length;
      if (n) highlight((active + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const el = list.children[active];
      if (el && open()) el.dispatchEvent(new MouseEvent("mousedown"));
      else pick(input.value);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      hide();
      set(input.dataset.value || "");
      input.blur();
      onCancel?.();
    }
  });

  list.hidden = true;
  return { set };
}
