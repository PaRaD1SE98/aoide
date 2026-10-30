//! 录音提示悬浮条：输入焦点所在屏幕底部居中的胶囊，录音时显示随音量跳动的波形，识别中显示波浪动画
//! 显示期间持续跟随焦点：切到另一块屏幕上的输入框，悬浮条也换到那块屏幕的同一位置
//!
//! 鼠标移到胶囊上时，两侧出现圆形按钮：左边取消（同取消键），右边完成（同再按一次主热键，结束录音开始识别）；
//! 识别、润色中只有取消。腾不出手按键盘时可以只用鼠标。
//!
//! 这里是与平台无关的部分：状态、淡入淡出、波形动画、按钮的悬停和点击、绘制（tiny-skia）。
//! 窗口本身是各平台的原生窗口（win/overlay.rs、macos/overlay.rs）：不抢焦点，点按钮也不会，不影响打字和粘贴；
//! 只有胶囊和按钮接收鼠标，窗口其余透明的部分鼠标穿透。

use crate::audio;
use std::sync::Mutex;
use std::time::Instant;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hidden,
    /// 普通录音
    Recording,
    /// 语音修订录音
    Revising,
    /// 语音翻译录音
    Translating,
    /// 录音结束，转写中
    Processing,
    /// LLM 润色 / 修订中
    Polishing,
}

/// 胶囊的逻辑尺寸（按显示器缩放）
pub(crate) const WIDTH: f32 = 120.0;
pub(crate) const HEIGHT: f32 = 36.0;
/// 两侧圆形按钮的直径、和胶囊的间距
const BUTTON: f32 = 28.0;
const BUTTON_GAP: f32 = 6.0;
const SIDE: f32 = BUTTON + BUTTON_GAP;
/// 窗口的逻辑宽度：胶囊居中，两侧留出按钮的位置（不显示按钮时那里是透明的）
pub(crate) const WINDOW_WIDTH: f32 = WIDTH + 2.0 * SIDE;
/// 按钮淡入淡出的速度（每帧）
const HOVER_PER_FRAME: f32 = 0.2;
/// 距工作区（任务栏上沿 / 程序坞上沿）的距离
pub(crate) const BOTTOM_GAP: f32 = 16.0;
const N_BARS: usize = 12;
pub(crate) const FRAME_MS: u32 = 16;
const FADE_PER_FRAME: f32 = 0.16;

/// 显示期间检查焦点所在屏幕的间隔
pub(crate) const FOLLOW_MS: u128 = 200;

static MODE: Mutex<Mode> = Mutex::new(Mode::Hidden);

/// 录音到达最长时长、自动停止的时刻（没有限制时为 None）
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);
/// 剩余时间不到这么多秒时，在胶囊右侧显示倒计时
const COUNTDOWN_SECS: f32 = 20.0;
/// 最后这么多秒倒计时变红
const URGENT_SECS: u32 = 5;

/// 创建悬浮窗（整个进程调用一次）
pub fn init() {
    crate::platform::overlay::init();
}

/// 切换悬浮条状态（任意线程可调用）
pub fn set(mode: Mode) {
    *MODE.lock().unwrap() = mode;
    crate::platform::overlay::refresh();
}

/// 设置录音自动停止的时刻（开始录音时设置，结束时清除）
pub fn set_deadline(deadline: Option<Instant>) {
    *DEADLINE.lock().unwrap() = deadline;
}

/// 悬浮条换屏时要调用的函数（贴在它上方的小窗用来跟过去，见 translate_card.rs）。
/// 用登记回调而不是直接调用：悬浮条的代码不牵进 WebView 窗口的代码（单元测试程序没有加载它们需要的系统组件）
static ON_SCREEN_CHANGE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

pub fn on_screen_change(f: fn()) {
    let _ = ON_SCREEN_CHANGE.set(f);
}

/// 悬浮条跟随输入焦点换到了另一块屏幕（各平台的悬浮窗线程调用）
pub(crate) fn moved_screen() {
    if let Some(f) = ON_SCREEN_CHANGE.get() {
        f();
    }
}

/// 当前要求的状态
pub(crate) fn current() -> Mode {
    *MODE.lock().unwrap()
}

/// 悬浮条上的部位
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Part {
    Capsule,
    Cancel,
    Confirm,
}

/// 点了悬浮条上的按钮（各平台的悬浮窗线程调用）：和按对应的热键一样处理
pub(crate) fn clicked(part: Part) {
    let ev = match part {
        Part::Cancel => crate::hotkeys::HotkeyEvent::Cancel,
        Part::Confirm => crate::hotkeys::HotkeyEvent::Confirm,
        Part::Capsule => return,
    };
    log::info!("Overlay button clicked: {part:?}");
    crate::hotkeys::send(ev);
}

/// 动画状态：只在悬浮窗的绘制线程里访问
pub(crate) struct Anim {
    pub visible: bool,
    pub alpha: f32,
    /// 正在淡出时保留最后一个可见状态继续绘制
    shown_mode: Mode,
    /// 显示器缩放比例（胶囊按它放大绘制）
    pub scale: f32,
    bars: [f32; N_BARS],
    last_shift: Instant,
    start: Instant,
    /// 录音快到最长时长时剩下的秒数（向上取整），显示在胶囊右侧
    countdown: Option<u32>,
    /// 鼠标停在悬浮条上（两侧显示按钮）；hover_t 是按钮淡入的进度 0..1
    hover: bool,
    hover_t: f32,
    /// 鼠标下的按钮、按下时所在的按钮、上一帧左键是否按着（判断点击）
    hot: Option<Part>,
    pressed: Option<Part>,
    was_down: bool,
}

impl Anim {
    pub fn new() -> Self {
        Self {
            visible: false,
            alpha: 0.0,
            shown_mode: Mode::Hidden,
            scale: 1.0,
            bars: [0.0; N_BARS],
            last_shift: Instant::now(),
            start: Instant::now(),
            countdown: None,
            hover: false,
            hover_t: 0.0,
            hot: None,
            pressed: None,
            was_down: false,
        }
    }

    /// 当前状态有哪些按钮（取消，完成）：录音时都有，识别、润色中只有取消
    fn buttons(&self) -> (bool, bool) {
        match self.shown_mode {
            Mode::Recording | Mode::Revising | Mode::Translating => (true, true),
            Mode::Processing | Mode::Polishing => (true, false),
            Mode::Hidden => (false, false),
        }
    }

    /// 窗口内的逻辑坐标 (x, y) 落在哪个部位。按钮只在展开（鼠标停在悬浮条上）时才算；其余地方鼠标穿透
    pub(crate) fn hit(&self, x: f32, y: f32) -> Option<Part> {
        if !self.visible || current() == Mode::Hidden {
            return None;
        }
        let in_button = |cx: f32| (x - cx).powi(2) + (y - HEIGHT / 2.0).powi(2) <= (BUTTON / 2.0).powi(2);
        let (cancel, confirm) = self.buttons();
        if self.hover && cancel && in_button(BUTTON / 2.0) {
            return Some(Part::Cancel);
        }
        if self.hover && confirm && in_button(WINDOW_WIDTH - BUTTON / 2.0) {
            return Some(Part::Confirm);
        }
        ((SIDE..=SIDE + WIDTH).contains(&x) && (0.0..=HEIGHT).contains(&y)).then_some(Part::Capsule)
    }

    /// 每帧调用：鼠标位置（窗口内的逻辑坐标）和左键是否按着。返回被点击（在同一个按钮上按下再松开）的按钮
    pub(crate) fn pointer(&mut self, x: f32, y: f32, down: bool) -> Option<Part> {
        let part = self.hit(x, y);
        let in_window = (0.0..=WINDOW_WIDTH).contains(&x) && (0.0..=HEIGHT).contains(&y);
        // 移到胶囊上时展开；展开后鼠标在整个窗口（含两侧按钮）内都保持展开
        self.hover = current() != Mode::Hidden && (part == Some(Part::Capsule) || (self.hover && in_window));
        self.hot = part.filter(|p| *p != Part::Capsule);
        let mut clicked = None;
        if down && !self.was_down {
            self.pressed = self.hot;
        } else if !down && self.was_down {
            if self.pressed.is_some() && self.pressed == self.hot {
                clicked = self.pressed;
            }
            self.pressed = None;
        }
        self.was_down = down;
        clicked
    }

    /// 鼠标下是不是按钮（各平台据此显示手形光标等）
    pub(crate) fn over_button(&self) -> bool {
        self.hot.is_some()
    }

    /// 状态改变时调用。返回 true 表示刚从隐藏变为显示：调用方定位窗口、开始逐帧绘制
    pub fn show(&mut self) -> bool {
        let mode = current();
        if mode == Mode::Hidden {
            return false; // 由逐帧动画淡出后隐藏
        }
        if mode != self.shown_mode && self.shown_mode != Mode::Processing {
            self.bars = [0.0; N_BARS];
        }
        self.shown_mode = mode;
        if self.visible {
            return false;
        }
        self.visible = true;
        self.alpha = 0.0;
        self.start = Instant::now();
        true
    }

    /// 每帧调用：淡入淡出、更新波形。返回 false 表示已经完全淡出：调用方隐藏窗口、停止逐帧绘制
    pub fn frame(&mut self) -> bool {
        if current() == Mode::Hidden {
            self.alpha -= FADE_PER_FRAME;
            if self.alpha <= 0.0 {
                self.alpha = 0.0;
                self.visible = false;
                self.shown_mode = Mode::Hidden;
                return false;
            }
        } else {
            self.alpha = (self.alpha + FADE_PER_FRAME).min(1.0);
        }
        let target = if self.hover { 1.0 } else { 0.0 };
        self.hover_t += (target - self.hover_t).clamp(-HOVER_PER_FRAME, HOVER_PER_FRAME);
        self.update_bars();
        true
    }

    /// 录音时：每 60 ms 把当前音量推入波形（从右往左滚动）；转写中：波浪动画；润色中：高光扫过
    fn update_bars(&mut self) {
        self.countdown = match self.shown_mode {
            Mode::Recording | Mode::Translating => DEADLINE.lock().unwrap().and_then(|d| {
                let left = d.saturating_duration_since(Instant::now()).as_secs_f32();
                (left > 0.0 && left <= COUNTDOWN_SECS).then(|| left.ceil() as u32)
            }),
            _ => None,
        };
        match self.shown_mode {
            Mode::Recording | Mode::Revising | Mode::Translating => {
                if self.last_shift.elapsed().as_millis() >= 60 {
                    self.last_shift = Instant::now();
                    self.bars.rotate_left(1);
                    // 语音 RMS 通常在 0.01~0.2，开方压缩动态范围
                    let level = (audio::input_level() * 8.0).sqrt().min(1.0);
                    self.bars[N_BARS - 1] = level;
                }
            }
            Mode::Processing => {
                let t = self.start.elapsed().as_secs_f32();
                for (i, b) in self.bars.iter_mut().enumerate() {
                    let wave = (t * 6.0 - i as f32 * 0.55).sin().max(0.0);
                    *b = 0.15 + 0.45 * wave;
                }
            }
            Mode::Polishing => {
                // 一道高光从左往右扫过
                let t = self.start.elapsed().as_secs_f32();
                let span = N_BARS as f32 + 6.0;
                let pos = (t * 9.0) % span - 3.0;
                for (i, b) in self.bars.iter_mut().enumerate() {
                    let d = i as f32 - pos;
                    *b = 0.22 + 0.55 * (-d * d / 3.0).exp();
                }
            }
            Mode::Hidden => {}
        }
    }

    /// 按 scale 绘制整个窗口：居中的胶囊，展开时加上两侧的按钮（预乘 alpha 的 RGBA，不含整体淡入淡出的 alpha）
    pub fn draw(&self) -> Option<Pixmap> {
        let s = self.scale;
        let capsule = self.draw_capsule()?;
        let (w, h) = ((WINDOW_WIDTH * s).round(), (HEIGHT * s).round());
        let mut pm = Pixmap::new(w as u32, h as u32)?;
        let x = ((w - capsule.width() as f32) / 2.0).round() as i32;
        pm.draw_pixmap(x, 0, capsule.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        if self.hover_t > 0.01 {
            let (cancel, confirm) = self.buttons();
            if cancel {
                self.draw_button(&mut pm, Part::Cancel, BUTTON / 2.0 * s, h / 2.0);
            }
            if confirm {
                self.draw_button(&mut pm, Part::Confirm, w - BUTTON / 2.0 * s, h / 2.0);
            }
        }
        Some(pm)
    }

    /// 圆形按钮：深色底、白色图标（取消是 ×，完成是 ✓）；鼠标移上去变成红色 / 绿色底。
    /// 随 hover_t 淡入，并从胶囊旁边滑出一点
    fn draw_button(&self, pm: &mut Pixmap, part: Part, cx: f32, cy: f32) -> Option<()> {
        let (s, a) = (self.scale, self.hover_t);
        let id = Transform::identity();
        let toward_capsule = if part == Part::Cancel { 1.0 } else { -1.0 };
        let cx = cx + toward_capsule * (1.0 - a) * 8.0 * s;
        let hot = self.hot == Some(part);
        let pressed = hot && self.pressed == Some(part);
        let r = (BUTTON / 2.0 * s - 0.5) * if pressed { 0.92 } else { 1.0 };
        let fill = match (part, hot) {
            (Part::Cancel, true) => [255, 77, 79],
            (Part::Confirm, true) => [34, 197, 94],
            _ => [24, 24, 27],
        };
        let circle = PathBuilder::from_circle(cx, cy, r)?;
        pm.fill_path(&circle, &paint(fill[0], fill[1], fill[2], (240.0 * a) as u8), FillRule::Winding, id, None);
        pm.stroke_path(&circle, &paint(255, 255, 255, (40.0 * a) as u8), &Stroke { width: s.max(1.0), ..Default::default() }, id, None);
        let mut pb = PathBuilder::new();
        if part == Part::Cancel {
            let k = 4.5 * s;
            pb.move_to(cx - k, cy - k);
            pb.line_to(cx + k, cy + k);
            pb.move_to(cx + k, cy - k);
            pb.line_to(cx - k, cy + k);
        } else {
            pb.move_to(cx - 5.5 * s, cy + 0.3 * s);
            pb.line_to(cx - 1.8 * s, cy + 4.0 * s);
            pb.line_to(cx + 5.5 * s, cy - 4.0 * s);
        }
        let icon = pb.finish()?;
        let stroke = Stroke { width: 2.0 * s, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
        pm.stroke_path(&icon, &paint(255, 255, 255, (240.0 * a) as u8), &stroke, id, None);
        Some(())
    }

    /// 按 scale 绘制胶囊本身（README 配图也用它）
    fn draw_capsule(&self) -> Option<Pixmap> {
        let s = self.scale;
        let (w, h) = ((WIDTH * s).round(), (HEIGHT * s).round());
        let mut pm = Pixmap::new(w as u32, h as u32)?;
        let id = Transform::identity();

        let polishing = self.shown_mode == Mode::Polishing;
        let t = self.start.elapsed().as_secs_f32();
        let (dot, bar) = match self.shown_mode {
            Mode::Revising => ([167, 139, 250], [221, 214, 254]),
            Mode::Translating => ([52, 211, 153], [167, 243, 208]),
            Mode::Processing => ([96, 205, 255], [186, 230, 253]),
            Mode::Polishing => ([251, 191, 36], [253, 230, 138]),
            _ => ([255, 77, 79], [255, 255, 255]),
        };

        // 胶囊底 + 细描边（润色中描边换成琥珀色）
        let pill = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, h / 2.0)?;
        pm.fill_path(&pill, &paint(24, 24, 27, 235), FillRule::Winding, id, None);
        let border = if polishing { paint(dot[0], dot[1], dot[2], 120) } else { paint(255, 255, 255, 40) };
        pm.stroke_path(&pill, &border, &tiny_skia::Stroke { width: 1.0 * s.max(1.0), ..Default::default() }, id, None);

        // 左侧状态标记：录音时呼吸闪烁的圆点；转写中常亮圆点；润色中换成星芒 ✦
        let (cx, cy) = (18.0 * s, h / 2.0);
        if polishing {
            let pulse = 0.75 + 0.25 * (t * 5.0).sin();
            let mark = sparkle(cx, cy, 6.5 * s * pulse, 1.6 * s)?;
            pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], 255), FillRule::Winding, id, None);
        } else {
            let pulse = if self.shown_mode == Mode::Processing { 1.0 } else { 0.65 + 0.35 * (t * 4.0).sin().abs() };
            let mark = PathBuilder::from_circle(cx, cy, 4.5 * s)?;
            pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], (255.0 * pulse) as u8), FillRule::Winding, id, None);
        }

        // 快到最长时长：右侧显示剩余秒数，最后几秒变红并闪烁；波形只画放得下的最新几条
        let (left, mut right) = (32.0 * s, w - 16.0 * s);
        if let Some(secs) = self.countdown {
            let digit_h = 14.0 * s;
            let digit_w = 7.5 * s;
            let gap = 3.0 * s;
            let text = secs.to_string();
            let text_w = digit_w * text.len() as f32 + gap * (text.len() - 1) as f32;
            let x0 = w - 15.0 * s - text_w;
            let color = if secs <= URGENT_SECS {
                let blink = 0.6 + 0.4 * (t * 6.0).cos().abs();
                paint(255, 77, 79, (255.0 * blink) as u8)
            } else {
                paint(255, 255, 255, 235)
            };
            for (k, c) in text.chars().enumerate() {
                let x = x0 + (digit_w + gap) * k as f32;
                if let Some(path) = digit(c.to_digit(10).unwrap_or(0), x, (h - digit_h) / 2.0, digit_w, digit_h, 1.9 * s) {
                    pm.fill_path(&path, &color, FillRule::Winding, id, None);
                }
            }
            right = x0 - 7.0 * s;
        }

        // 波形条：中间的条更高，看起来更自然
        let bar_w = 3.0 * s;
        let step = (w - 16.0 * s - left - bar_w) / (N_BARS - 1) as f32;
        // 放得下几条（显示倒计时时变少），取最新的那几条
        let shown = (((right - left - bar_w) / step).floor() as usize + 1).min(N_BARS);
        let (min_h, max_h) = (3.0 * s, h - 14.0 * s);
        for (i, &value) in self.bars[N_BARS - shown..].iter().enumerate() {
            let profile = 0.55 + 0.45 * (std::f32::consts::PI * (i as f32 + 0.5) / shown as f32).sin();
            let bh = min_h + (max_h - min_h) * value * profile;
            let x = left + step * i as f32;
            // 润色中：被高光扫到的条更亮
            let alpha = if polishing { (110.0 + 145.0 * ((value - 0.22) / 0.55).clamp(0.0, 1.0)) as u8 } else { 230 };
            if let Some(path) = rounded_rect(x, (h - bh) / 2.0, bar_w, bh, bar_w / 2.0) {
                pm.fill_path(&path, &paint(bar[0], bar[1], bar[2], alpha), FillRule::Winding, id, None);
            }
        }
        Some(pm)
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = r * 0.5523; // 三次贝塞尔近似四分之一圆
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, a);
    p.anti_alias = true;
    p
}

/// 数码管样式的数字（不用字体：tiny-skia 不画文字）。box 为 (x, y, w, h)，笔画粗 t
fn digit(d: u32, x: f32, y: f32, w: f32, h: f32, t: f32) -> Option<Path> {
    // 七段：a 上横、b 右上竖、c 右下竖、d 下横、e 左下竖、f 左上竖、g 中横
    const SEGMENTS: [&str; 10] = ["abcdef", "bc", "abged", "abgcd", "fgbc", "afgcd", "afgedc", "abc", "abcdefg", "abcdfg"];
    let mut pb = PathBuilder::new();
    let half = h / 2.0;
    let horizontal = |pb: &mut PathBuilder, cy: f32| pb.push_path_rounded(x + t * 0.6, cy - t / 2.0, w - t * 1.2, t);
    let vertical = |pb: &mut PathBuilder, cx: f32, top: f32| pb.push_path_rounded(cx - t / 2.0, top + t * 0.6, t, half - t * 1.2);
    for seg in SEGMENTS.get(d as usize)?.chars() {
        match seg {
            'a' => horizontal(&mut pb, y + t / 2.0),
            'g' => horizontal(&mut pb, y + half),
            'd' => horizontal(&mut pb, y + h - t / 2.0),
            'f' => vertical(&mut pb, x + t / 2.0, y),
            'b' => vertical(&mut pb, x + w - t / 2.0, y),
            'e' => vertical(&mut pb, x + t / 2.0, y + half),
            'c' => vertical(&mut pb, x + w - t / 2.0, y + half),
            _ => {}
        }
    }
    pb.finish()
}

/// 往路径里加一个圆角矩形（数码管的一段）
trait PushRounded {
    fn push_path_rounded(&mut self, x: f32, y: f32, w: f32, h: f32);
}

impl PushRounded for PathBuilder {
    fn push_path_rounded(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if let Some(path) = rounded_rect(x, y, w, h, w.min(h) / 2.0) {
            self.push_path(&path);
        }
    }
}

/// 四角星芒（外半径 r，内半径 inner）
fn sparkle(cx: f32, cy: f32, r: f32, inner: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for k in 0..8 {
        let angle = std::f32::consts::FRAC_PI_4 * k as f32 - std::f32::consts::FRAC_PI_2;
        let radius = if k % 2 == 0 { r } else { inner };
        let (x, y) = (cx + radius * angle.cos(), cy + radius * angle.sin());
        if k == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn anim(mode: Mode, bars: [f32; N_BARS], start: Instant) -> Anim {
        Anim { shown_mode: mode, scale: 2.0, bars, start, visible: true, alpha: 1.0, ..Anim::new() }
    }

    #[test]
    fn buttons_expand_on_hover_and_click() {
        set(Mode::Recording);
        let mut v = anim(Mode::Recording, [0.0; N_BARS], Instant::now());
        let (cy, cancel_x, confirm_x) = (HEIGHT / 2.0, BUTTON / 2.0, WINDOW_WIDTH - BUTTON / 2.0);
        // 没停在悬浮条上时，按钮的位置是穿透的
        assert_eq!(v.hit(confirm_x, cy), None);
        assert_eq!(v.pointer(confirm_x, cy, false), None);
        // 移到胶囊上展开，再移到按钮上
        assert_eq!(v.pointer(WINDOW_WIDTH / 2.0, cy, false), None);
        assert_eq!(v.hit(confirm_x, cy), Some(Part::Confirm));
        assert_eq!(v.pointer(confirm_x, cy, false), None);
        assert!(v.over_button());
        // 按下、松开：点击完成
        assert_eq!(v.pointer(confirm_x, cy, true), None);
        assert_eq!(v.pointer(confirm_x, cy, false), Some(Part::Confirm));
        // 在取消上按下、拖到胶囊上松开：不算点击
        assert_eq!(v.pointer(cancel_x, cy, true), None);
        assert_eq!(v.pointer(WINDOW_WIDTH / 2.0, cy, false), None);
        // 移出窗口收起
        v.pointer(WINDOW_WIDTH + 20.0, cy, false);
        assert_eq!(v.hit(cancel_x, cy), None);
        // 识别中只有取消
        let mut v = anim(Mode::Processing, [0.0; N_BARS], Instant::now());
        v.pointer(WINDOW_WIDTH / 2.0, cy, false);
        assert_eq!(v.hit(confirm_x, cy), None);
        assert_eq!(v.hit(cancel_x, cy), Some(Part::Cancel));
        set(Mode::Hidden);
    }

    /// 手动运行：输出展开了按钮的悬浮条（各状态、鼠标停在完成 / 取消上），看看样子
    /// VT_IMG_DIR=… cargo test --lib export_overlay_buttons -- --ignored
    #[test]
    #[ignore]
    fn export_overlay_buttons() {
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let speech = [0.18, 0.4, 0.72, 0.95, 0.6, 0.38, 0.82, 1.0, 0.58, 0.3, 0.52, 0.22];
        let cy = HEIGHT / 2.0;
        for (name, mode, at) in [
            ("idle", Mode::Recording, None),
            ("hover", Mode::Recording, Some(WINDOW_WIDTH / 2.0)),
            ("confirm", Mode::Recording, Some(WINDOW_WIDTH - BUTTON / 2.0)),
            ("cancel", Mode::Recording, Some(BUTTON / 2.0)),
            ("processing", Mode::Processing, Some(WINDOW_WIDTH / 2.0)),
        ] {
            set(mode);
            let mut v = anim(mode, speech, Instant::now() - Duration::from_millis(400));
            if let Some(x) = at {
                v.pointer(WINDOW_WIDTH / 2.0, cy, false);
                v.pointer(x, cy, false);
                v.hover_t = 1.0;
            }
            let path = dir.join(format!("overlay-{name}.png"));
            v.draw().unwrap().save_png(&path).unwrap();
            println!("wrote {}", path.display());
        }
        set(Mode::Hidden);
    }

    /// README 配图：用与程序相同的绘制代码把各状态的胶囊输出成 PNG（2 倍分辨率），不涉及真实屏幕。
    /// 由 scripts/readme-images.ps1 调用：VT_IMG_DIR=… cargo test --lib export_capsule_images -- --ignored
    #[test]
    #[ignore]
    fn export_capsule_images() {
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into()));
        std::fs::create_dir_all(&dir).unwrap();
        // 一段像说话的波形
        let speech = [0.18, 0.4, 0.72, 0.95, 0.6, 0.38, 0.82, 1.0, 0.58, 0.3, 0.52, 0.22];
        let at = |secs: f32| Instant::now() - Duration::from_secs_f32(secs);
        for (name, mode, t) in [
            ("recording", Mode::Recording, 0.4),
            ("revising", Mode::Revising, 0.4),
            ("translating", Mode::Translating, 0.4),
            ("transcribing", Mode::Processing, 0.35),
            ("polishing", Mode::Polishing, 0.6),
        ] {
            let mut v = anim(mode, speech, at(t));
            v.update_bars();
            if matches!(mode, Mode::Recording | Mode::Revising | Mode::Translating) {
                v.bars = speech; // 录音态的波形来自麦克风，这里用固定样例
            }
            let path = dir.join(format!("capsule-{name}.png"));
            v.draw_capsule().unwrap().save_png(&path).unwrap();
            println!("wrote {}", path.display());
        }
        // 快到最长时长时的倒计时
        for secs in [20, 9, 3] {
            let mut v = anim(Mode::Recording, speech, at(0.4));
            v.countdown = Some(secs);
            let path = dir.join(format!("capsule-countdown-{secs}.png"));
            v.draw_capsule().unwrap().save_png(&path).unwrap();
            println!("wrote {}", path.display());
        }
    }

    /// README 演示动图用：按 30 fps 导出各状态的逐帧动画（scripts/readme-demo.ps1 调用）
    /// 输出 {VT_IMG_DIR}/frames/{recording,revising,translating,transcribing,polishing}-NNNN.png
    #[test]
    #[ignore]
    fn export_capsule_frames() {
        const FPS: f32 = 30.0;
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into())).join("frames");
        std::fs::create_dir_all(&dir).unwrap();
        // 模拟说话的音量：音节起伏 + 句间停顿 + 确定性的抖动
        let level = |t: f32| {
            let syllable = 0.5 + 0.5 * (t * 2.0 * std::f32::consts::PI * 4.2).sin();
            let phrase = ((t * 0.9).sin() * 0.5 + 0.5).powf(0.6);
            let jitter = ((t * 1000.0).sin() * 43758.547).fract().abs();
            (0.15 + 0.85 * syllable * phrase * (0.6 + 0.4 * jitter)).min(1.0)
        };
        for (name, mode, secs) in [
            ("recording", Mode::Recording, 8.0),
            ("revising", Mode::Revising, 6.0),
            ("translating", Mode::Translating, 6.0),
            ("transcribing", Mode::Processing, 2.0),
            ("polishing", Mode::Polishing, 3.0),
        ] {
            let now = Instant::now();
            for i in 0..(secs * FPS) as usize {
                let t = i as f32 / FPS;
                let mut v = anim(mode, [0.0; N_BARS], now - Duration::from_secs_f32(t));
                if matches!(mode, Mode::Recording | Mode::Revising | Mode::Translating) {
                    // 与 update_bars 相同：每 60 ms 推入一格音量，从右往左滚动
                    let shifts = (t * 1000.0 / 60.0) as i32;
                    for (k, b) in v.bars.iter_mut().enumerate() {
                        let n = shifts - (N_BARS - 1 - k) as i32;
                        *b = if n > 0 { level(n as f32 * 0.06) } else { 0.0 };
                    }
                } else {
                    v.update_bars();
                }
                v.draw_capsule().unwrap().save_png(dir.join(format!("{name}-{i:04}.png"))).unwrap();
            }
            println!("wrote {name} ({} frames)", (secs * FPS) as usize);
        }
    }
}
