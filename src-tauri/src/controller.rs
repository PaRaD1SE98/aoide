//! 主流程：热键 → 录音 → 转录 → 润色/修订 → 输出

use crate::audio::{Recorder, SAMPLE_RATE};
use crate::sound::{self, Cue};
use crate::config::Config;
use crate::hotkeys::{self, HotkeyEvent};
use crate::transcriber::Transcriber;
use crate::tray::{self, TrayState};
use crate::overlay::{self, Mode};
use crate::{learned_card, notice, result_card, translate_card};
use crate::{input, mute};
use crate::i18n::tr;
use crate::{clipboard, focus, learn, llm, models, screen, trf, vocab};
use crate::focus::Typable;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Default)]
struct RecState {
    recording: bool,
    recorder: Option<Arc<Recorder>>,
    /// 这次录音是语音翻译
    translate: bool,
}

#[derive(Default)]
struct RevState {
    active: bool,
    recorder: Option<Arc<Recorder>>,
    selected: Option<String>,
    /// 开始修订录音时在后台截的屏幕上下文（见 screen.rs）
    screen: Option<screen::Pending>,
    /// 修订跨两个线程（开始 / 停止），任务凭证存在这里交接
    task: Option<Task>,
}

/// 一次任务（录音 → 识别 → 输出）的凭证：存活期间取消键生效；
/// 取消会推进全局纪元，凭证据此判断自己是否已被取消
struct Task {
    ctl: Arc<Controller>,
    epoch: u64,
}

impl Task {
    fn canceled(&self) -> bool {
        self.ctl.epoch.load(Ordering::SeqCst) != self.epoch
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        let left = self.ctl.in_flight.fetch_sub(1, Ordering::SeqCst) - 1;
        hotkeys::set_cancel_armed(left > 0);
    }
}

pub struct Controller {
    app: AppHandle,
    config: RwLock<Config>,
    transcriber: Transcriber,
    rec: Mutex<RecState>,
    rev: Mutex<RevState>,
    /// 每次取消加一
    epoch: AtomicU64,
    /// 进行中的任务数
    in_flight: AtomicUsize,
    /// 连着的几次短语音：(出结果的时间, 识别结果)，最近的在最后（见 earlier_attempts）
    recent_short: Mutex<Vec<(Instant, String)>>,
}

impl Controller {
    pub fn new(app: AppHandle, config: Config) -> Arc<Self> {
        Arc::new(Self {
            app,
            config: RwLock::new(config),
            transcriber: Transcriber::default(),
            rec: Mutex::default(),
            rev: Mutex::default(),
            epoch: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            recent_short: Mutex::default(),
        })
    }

    fn begin_task(self: &Arc<Self>) -> Task {
        // 新的语音输入开始：结束对上一段文字的修改观察
        learn::stop_watching();
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        hotkeys::set_cancel_armed(true);
        Task { ctl: self.clone(), epoch: self.epoch.load(Ordering::SeqCst) }
    }

    /// 取消键：中止录音并丢弃音频；已在识别 / 润色的任务结果作废，不再输出
    fn cancel(self: &Arc<Self>) {
        if self.in_flight.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let mut s = self.rec.lock().unwrap();
            if s.recording {
                if let Some(r) = &s.recorder {
                    r.stop();
                }
                s.recording = false;
            }
        }
        let rev_task = {
            let mut r = self.rev.lock().unwrap();
            if r.active {
                r.active = false;
                if let Some(rec) = r.recorder.take() {
                    rec.stop();
                }
                r.selected = None;
            }
            r.task.take()
        };
        drop(rev_task); // 修订录音中被取消时，停止修订的流程不会再走到，由这里结束任务
        translate_card::close_picker(&self.app);
        mute::end();
        overlay::set(Mode::Hidden);
        self.set_tray(TrayState::Idle);
        sound::play(Cue::Cancel);
        log::info!("Task canceled");
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    /// 在屏幕底部的提示小窗里显示一条消息（见 notice.rs）
    pub fn notify(&self, msg: &str) {
        notice::show(&self.app, msg);
    }

    /// 同 notify，并播放出错的提示音：操作失败、没能完成时用
    pub fn notify_error(&self, msg: &str) {
        sound::play(Cue::Error);
        notice::show(&self.app, msg);
    }

    fn set_tray(&self, state: TrayState) {
        tray::set_state(&self.app, state);
    }

    fn busy(&self) -> bool {
        self.rec.lock().unwrap().recording || self.rev.lock().unwrap().active
    }

    /// 正在录音、识别或润色（自动更新据此推迟安装）
    pub fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0 || self.busy()
    }

    /// 处理结束后收起悬浮条（期间又开始了新的录音则不动）
    fn finish_overlay(&self) {
        if !self.busy() {
            overlay::set(Mode::Hidden);
        }
    }

    /// 图标变绿 2 秒后恢复灰色（期间又开始录音则不动）
    fn flash_done(self: &Arc<Self>) {
        self.set_tray(TrayState::Done);
        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            if !me.busy() {
                me.set_tray(TrayState::Idle);
            }
        });
    }

    /// 安装键盘钩子、分发热键事件、后台预热模型和 UIA
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        hotkeys::install(tx);
        let cfg = self.config();
        hotkeys::update(&cfg.hotkey, &cfg.revise_hotkey, &cfg.revise_stop_hotkey, &cfg.cancel_hotkey, &cfg.translate_hotkey);
        vocab::set_size_limit(u64::from(cfg.vocab_max_kb) * 1024);

        let me = self.clone();
        thread::spawn(move || {
            // 钩子回调必须尽快返回，耗时操作都丢到新线程
            for ev in rx {
                let me2 = me.clone();
                match ev {
                    HotkeyEvent::Tap => {
                        if me.rev.lock().unwrap().active {
                            thread::spawn(move || me2.stop_revision());
                        } else {
                            thread::spawn(move || me2.toggle_recording(false));
                        }
                    }
                    HotkeyEvent::Translate => {
                        // 开始翻译录音；已在录音时同单击，结束录音
                        if !me.rev.lock().unwrap().active {
                            thread::spawn(move || me2.toggle_recording(true));
                        }
                    }
                    HotkeyEvent::Revise => {
                        thread::spawn(move || me2.start_revision());
                    }
                    HotkeyEvent::ReviseStop => {
                        if me.rev.lock().unwrap().active {
                            thread::spawn(move || me2.stop_revision());
                        }
                    }
                    HotkeyEvent::Cancel => {
                        thread::spawn(move || me2.cancel());
                    }
                    HotkeyEvent::Dismiss => {
                        result_card::close(&me.app);
                        learned_card::close(&me.app);
                        if translate_card::is_review_open() {
                            translate_card::close(&me.app);
                        }
                        notice::close(&me.app);
                    }
                }
            }
        });

        self.preload_model();
        thread::spawn(focus::warm_up);
    }

    /// 语音识别能用了：本地模型已下载，或用 API 时已填写 API 地址
    pub fn speech_ready(&self) -> bool {
        let cfg = self.config();
        if cfg.speech_via_api() {
            !cfg.speech_api.trim().is_empty()
        } else {
            models::is_downloaded(&cfg.model_dir(), &cfg.model)
        }
    }

    pub fn preload_model(self: &Arc<Self>) {
        if !self.speech_ready() {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            if let Err(e) = me.transcriber.preload(&me.config()) {
                log::error!("{e}");
                me.notify_error(&e);
            }
        });
    }

    /// 设置页保存后调用：就地生效，无需重启
    pub fn apply_config(self: &Arc<Self>, new: Config) {
        let old = std::mem::replace(&mut *self.config.write().unwrap(), new.clone());
        hotkeys::update(&new.hotkey, &new.revise_hotkey, &new.revise_stop_hotkey, &new.cancel_hotkey, &new.translate_hotkey);
        if old.vocab_max_kb != new.vocab_max_kb {
            vocab::set_size_limit(u64::from(new.vocab_max_kb) * 1024);
            let _ = self.app.emit("vocab-changed", ());
        }
        if (&old.speech_source, &old.model, &old.model_dir, &old.device) != (&new.speech_source, &new.model, &new.model_dir, &new.device) {
            self.transcriber.reset();
            self.preload_model();
        }
        if old.ui_language != new.ui_language {
            crate::i18n::set_language(&new.ui_language);
            tray::refresh_menu(&self.app);
        }
        tray::set_tooltip(&self.app, &new.hotkey);
        log::info!("Config reloaded");
    }

    /// 取出录音；完全没录到（如误触双击）时提示并返回 None
    fn take_audio(&self, rec: Option<&Recorder>, tag: &str) -> Option<Vec<f32>> {
        match rec.and_then(|r| r.audio()) {
            Some(a) => Some(a),
            None => {
                log::info!("{tag}: no audio captured");
                sound::play(Cue::Cancel);
                self.set_tray(TrayState::Idle);
                self.finish_overlay();
                self.notify(tr("没有录到声音", "No audio was recorded", "音声が録音されませんでした"));
                None
            }
        }
    }

    // === 普通录音 / 语音翻译 ===
    /// translate：开始的是语音翻译录音（结束录音时不看这个参数，按开始时的类型处理）
    fn toggle_recording(self: &Arc<Self>, translate: bool) {
        let (rec, task) = {
            let mut s = self.rec.lock().unwrap();
            if s.recording {
                // 先恢复声音再播结束音：macOS 静音的是整个输出设备
                mute::end();
                sound::play(Cue::Stop);
                if let Some(r) = &s.recorder {
                    r.stop();
                }
                s.recording = false;
                if s.translate {
                    translate_card::close_picker(&self.app);
                }
                overlay::set(Mode::Processing);
                return; // 转录由正在等待录音结束的那个线程处理
            }
            s.recording = true;
            s.translate = translate;
            self.set_tray(TrayState::Recording);
            if translate {
                overlay::set(Mode::Translating);
                translate_card::show_picker(&self.app, &self.config().translate_language);
            } else {
                overlay::set(Mode::Recording);
            }
            // 开始音播完才开录：提示音不会被录进去
            sound::play_sync(Cue::Start);
            let r = Recorder::start();
            s.recorder = Some(r.clone());
            (r, self.begin_task())
        };

        // 锁已释放，等待录音结束（第二次按键可以进入上面的分支停止）
        let started = Instant::now();
        let cfg = self.config();
        // 屏幕上下文：趁录音时在后台截取前台窗口（用户正在操作的程序）
        let shot = screen::begin(&cfg);
        if cfg.mute_while_recording {
            mute::begin(); // 在开始提示音之后静音，提示音本身也不受影响
        }
        // 悬浮条在最后 20 秒显示倒计时
        let limit = Duration::from_secs(cfg.max_duration.into());
        overlay::set_deadline(Some(Instant::now() + limit));
        let stopped_by_user = rec.wait(limit);
        overlay::set_deadline(None);
        if !stopped_by_user && !task.canceled() {
            log::info!("Max duration reached");
            rec.stop();
            rec.wait(Duration::from_secs(1));
            let mut s = self.rec.lock().unwrap();
            if s.recorder.as_ref().is_some_and(|r| Arc::ptr_eq(r, &rec)) {
                s.recording = false;
            }
            drop(s);
            mute::end();
            sound::play(Cue::Stop);
            if translate {
                translate_card::close_picker(&self.app);
            }
            overlay::set(Mode::Processing);
        }

        if task.canceled() {
            return;
        }
        let Some(audio) = self.take_audio(Some(&rec), "Recording") else {
            return;
        };
        let seconds = audio.len() as f32 / SAMPLE_RATE as f32;
        let result = self.transcriber.transcribe(&audio, &cfg);
        if task.canceled() {
            return;
        }
        let shot = shot.take();
        match result {
            Ok(text) if text.trim().is_empty() => {
                log::info!("Empty transcription");
                self.set_tray(TrayState::Idle);
            }
            Ok(text) if translate => {
                log::info!("Transcribed (to translate): {text}");
                self.output_translation(text, shot.as_ref(), &cfg, &task);
                self.flash_done();
            }
            Ok(text) => {
                log::info!("Transcribed: {text}");
                let (text, glossary) = self.apply_vocab(text, &cfg);
                let ctx = self.caret_context(&cfg);
                let short = match &ctx {
                    Some(_) => too_short(&text, seconds, cfg.insert_min_seconds, cfg.insert_min_chars),
                    None => too_short(&text, seconds, cfg.polish_min_seconds, cfg.polish_min_chars),
                };
                // 连续短语音：前面连着几次也是短语音时，这次也交给 LLM，并附上之前的识别结果
                let earlier = if short.is_some() { self.earlier_attempts(started, &cfg) } else { Vec::new() };
                let transcript = short.is_some().then(|| text.clone());
                let text = match ctx {
                    Some(ctx) => self.insert_in_context(text, short.as_deref(), &earlier, shot.as_ref(), &ctx, glossary.as_deref(), &cfg),
                    None => self.maybe_polish(text, short.as_deref(), &earlier, shot.as_ref(), glossary.as_deref(), &cfg),
                };
                if task.canceled() {
                    return;
                }
                let pasted = self.output_text(&text);
                self.remember_attempt(transcript, !earlier.is_empty());
                if pasted && cfg.learn_from_edits {
                    learn::watch(self.app.clone(), text, cfg.clone());
                }
                self.flash_done();
            }
            Err(e) => {
                log::error!("Transcribe error: {e}");
                self.notify_error(&trf!("错误：{e}", "Error: {e}", "エラー: {e}"));
                self.set_tray(TrayState::Idle);
            }
        }
        self.finish_overlay();
    }

    // === 语音翻译 ===
    /// 把转写结果翻译成设置的目标语言并输出，再在悬浮条的位置弹出核对小窗（原文可改，改完重新翻译）
    fn output_translation(self: &Arc<Self>, text: String, shot: Option<&screen::Shot>, cfg: &Config, task: &Task) {
        let (text, glossary) = self.apply_vocab(text, cfg);
        let language = cfg.translate_language.trim().to_string();
        let mut view = translate_card::View { language: language.clone(), original: text.clone(), ..Default::default() };
        if cfg.polish_api.trim().is_empty() {
            sound::play(Cue::Error);
            view.error = tr("翻译要用到 LLM：请先在设置的「LLM 润色与修订」里填写 API", "Translation uses the LLM: set up its API under Settings → LLM polish & edit first", "翻訳には LLM を使います。先に設定の「LLM による整形と修正」で API を入力してください").into();
        } else {
            overlay::set(Mode::Polishing);
            match llm::translate_text(&text, &language, shot, glossary.as_deref(), cfg) {
                Ok(t) if !t.trim().is_empty() => {
                    if task.canceled() {
                        return;
                    }
                    log::info!("Translated into {language}: {t}");
                    view.pasted = self.paste_translation(&t);
                    view.translation = t;
                }
                Ok(_) => {
                    sound::play(Cue::Error);
                    view.error = tr("LLM 没有返回译文", "The LLM returned no translation", "LLM から訳文が返されませんでした").into();
                }
                Err(e) => {
                    log::error!("Translate error: {e}");
                    sound::play(Cue::Error);
                    view.error = trf!("翻译失败：{e}", "Translation failed: {e}", "翻訳に失敗しました: {e}");
                }
            }
        }
        if task.canceled() {
            return;
        }
        translate_card::show_review(&self.app, view);
    }

    /// 焦点在输入框时粘贴译文；没有输入框时不粘贴（译文显示在核对小窗里，由用户复制）
    fn paste_translation(&self, text: &str) -> bool {
        thread::sleep(Duration::from_millis(200));
        match focus::can_type_here() {
            Typable::Yes => clipboard::paste_text(text),
            Typable::Unknown => clipboard::paste_text_keep(text),
            Typable::No => false,
        }
    }

    /// 更改翻译的目标语言并存进设置；设置页开着时通知它同步
    pub fn set_translate_language(&self, language: &str) -> Result<(), String> {
        let saved = {
            let mut cfg = self.config.write().unwrap();
            if cfg.translate_language == language {
                return Ok(());
            }
            cfg.translate_language = language.to_string();
            cfg.save()
        };
        saved.map_err(|e| trf!("保存失败：{e}", "Failed to save: {e}", "保存できませんでした: {e}"))?;
        log::info!("Translate language: {language}");
        let _ = self.app.emit("translate-language-changed", language);
        Ok(())
    }

    /// 核对小窗里改了原文（或换了语言）后重新翻译。译文之前已粘贴进输入框时，回到那个输入框把它替换掉；
    /// 替换不了（输入框里的文字变了）时把新译文放进剪贴板，返回 Err 说明原因
    pub fn retranslate(self: &Arc<Self>, original: &str, language: &str) -> Result<String, String> {
        let original = original.trim();
        let language = language.trim();
        if original.is_empty() {
            return Err(tr("原文是空的", "The original text is empty", "原文が空です").into());
        }
        if language.is_empty() {
            return Err(tr("请填写目标语言", "Enter a target language", "翻訳先の言語を入力してください").into());
        }
        self.set_translate_language(language)?;
        let cfg = self.config();
        let glossary = if cfg.vocab_enabled { vocab::glossary(&vocab::relevant(original)) } else { None };
        let new = llm::translate_text(original, language, None, glossary.as_deref(), &cfg)
            .map_err(|e| trf!("翻译失败：{e}", "Translation failed: {e}", "翻訳に失敗しました: {e}"))?;
        if new.trim().is_empty() {
            return Err(tr("LLM 没有返回译文", "The LLM returned no translation", "LLM から訳文が返されませんでした").into());
        }
        log::info!("Retranslated into {language}: {new}");

        let state = self.app.state::<translate_card::TranslateState>();
        let (old, pasted) = {
            let s = state.0.lock().unwrap();
            (s.view.translation.clone(), s.view.pasted)
        };
        let replaced = if pasted && !old.is_empty() {
            translate_card::restore_target(&self.app);
            thread::sleep(Duration::from_millis(250));
            replace_before_caret(&old, &new)
        } else {
            Ok(())
        };
        {
            let mut s = state.0.lock().unwrap();
            s.view.original = original.to_string();
            s.view.language = language.to_string();
            s.view.translation = new.clone();
            s.view.error.clear();
        }
        replaced.map(|_| new)
    }

    /// 词库：高置信度的误识别写法直接替换；其余与识别结果相关的词条整理成给 LLM 的说明
    fn apply_vocab(&self, text: String, cfg: &Config) -> (String, Option<String>) {
        if !cfg.vocab_enabled {
            return (text, None);
        }
        let (text, applied) = vocab::apply_confident(&text);
        for (wrong, right) in &applied {
            log::info!("Vocabulary: {wrong} -> {right}");
        }
        let glossary = vocab::glossary(&vocab::relevant(&text));
        if glossary.is_some() {
            log::info!("Vocabulary: related entries found, passing them to the LLM");
        }
        (text, glossary)
    }

    /// 连续短语音：这次是短语音，上一次也是、且上一次出结果到这次开始录音不超过设定的秒数时，
    /// 返回之前连着的识别结果（最近两次）；否则返回空
    fn earlier_attempts(&self, started: Instant, cfg: &Config) -> Vec<String> {
        let window = Duration::from_secs(cfg.polish_repeat_seconds.into());
        let recent = self.recent_short.lock().unwrap();
        match recent.last() {
            Some((at, _)) if !window.is_zero() && started.saturating_duration_since(*at) <= window => {
                recent[recent.len().saturating_sub(2)..].iter().map(|(_, t)| t.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// 记下这次语音：短语音（short 为识别结果）接在连着的记录后面，没连上时重新开始；不是短语音时清空
    fn remember_attempt(&self, short: Option<String>, chained: bool) {
        let mut recent = self.recent_short.lock().unwrap();
        if !chained {
            recent.clear();
        }
        if let Some(text) = short {
            recent.push((Instant::now(), text));
            let extra = recent.len().saturating_sub(3);
            recent.drain(..extra);
        }
    }

    /// 文字较短、不值得整段润色时，若有相关词条，只按词库修正误识别的词
    fn correct_terms(&self, text: String, glossary: Option<&str>, cfg: &Config) -> String {
        let Some(glossary) = glossary.filter(|_| !cfg.polish_api.trim().is_empty()) else {
            return text;
        };
        overlay::set(Mode::Polishing);
        match llm::correct_terms(&text, glossary, cfg) {
            Ok(t) if !t.is_empty() => {
                if t != text {
                    log::info!("Terms corrected: {t}");
                }
                t
            }
            Ok(_) => text,
            Err(e) => {
                log::warn!("Term correction skipped: {e}");
                text
            }
        }
    }

    /// 开启「按上下文插入」且已配置 LLM 时读取光标前后的文字；光标前后都没有内容时返回 None（按普通润色处理）
    fn caret_context(&self, cfg: &Config) -> Option<focus::CaretContext> {
        if !cfg.insert_smart || cfg.polish_api.trim().is_empty() {
            return None;
        }
        let ctx = focus::caret_context().filter(|c| !c.is_empty())?;
        log::info!(
            "Caret context: {} chars before, {} chars after",
            ctx.before.chars().count(),
            ctx.after.chars().count()
        );
        Some(ctx)
    }

    /// 光标在已有文字中间：让 LLM 根据前后文生成要插入的文字。
    /// 输出仍是在光标处粘贴，前后原文不会被改动；短语音（short 为原因，见 too_short）或调用失败时直接插入转写结果，
    /// 连续短语音（earlier 非空）除外
    #[allow(clippy::too_many_arguments)]
    fn insert_in_context(
        &self,
        text: String,
        short: Option<&str>,
        earlier: &[String],
        shot: Option<&screen::Shot>,
        ctx: &focus::CaretContext,
        glossary: Option<&str>,
        cfg: &Config,
    ) -> String {
        if let Some(why) = short {
            if earlier.is_empty() {
                log::info!("Context insertion skipped ({why})");
                // 不走 LLM 插入（命中词库时仍按词库修正），西文单词之间的空格按前后文补上
                let text = self.correct_terms(text, glossary, cfg);
                return llm::fit_spacing(text.trim(), &ctx.before, &ctx.after);
            }
            log::info!("Repeated short dictation ({why}): inserting with the LLM, {} earlier attempt(s)", earlier.len());
        }
        overlay::set(Mode::Polishing);
        match llm::insert_text(&text, &ctx.before, &ctx.after, earlier, shot, glossary, cfg) {
            Ok(fragment) => {
                log::info!("Insert in context: {fragment}");
                fragment
            }
            Err(e) => {
                log::warn!("Context insertion skipped: {e}");
                text
            }
        }
    }

    /// LLM 润色（如果启用且已配置 API）；短语音（short 为原因，见 too_short）不值得润色，此时只按词库修正
    /// （有相关词条时）。连续短语音（earlier 非空）照样润色，并附上之前几次的识别结果
    fn maybe_polish(
        &self,
        text: String,
        short: Option<&str>,
        earlier: &[String],
        shot: Option<&screen::Shot>,
        glossary: Option<&str>,
        cfg: &Config,
    ) -> String {
        if !cfg.polish || cfg.polish_api.trim().is_empty() {
            return self.correct_terms(text, glossary, cfg);
        }
        if let Some(why) = short {
            if earlier.is_empty() {
                log::info!("Polish skipped ({why})");
                return self.correct_terms(text, glossary, cfg);
            }
            log::info!("Repeated short dictation ({why}): polishing, {} earlier attempt(s)", earlier.len());
        }
        overlay::set(Mode::Polishing);
        match llm::polish_text(&text, earlier, shot, glossary, cfg) {
            Ok(p) if !p.is_empty() && p != text => {
                log::info!("Polished: {p}");
                p
            }
            Ok(_) => text,
            Err(e) => {
                log::warn!("Polish skipped: {e}");
                text
            }
        }
    }

    /// 聚焦在输入框时粘贴输出（粘完恢复原剪贴板），返回 true；无处可粘贴时弹出结果卡片，由用户决定复制。
    /// 判断不了时（macOS 浏览器的网页内容读不到）照样粘贴，文字留在剪贴板里，没粘上的话用户可以自己粘贴
    pub(crate) fn output_text(&self, text: &str) -> bool {
        thread::sleep(Duration::from_millis(200));
        let pasted = match focus::can_type_here() {
            Typable::Yes => clipboard::paste_text(text),
            Typable::Unknown => clipboard::paste_text_keep(text),
            Typable::No => false,
        };
        if pasted {
            return true;
        }
        log::info!("No input focused, showing result card: {text}");
        result_card::show(&self.app, tr("没有检测到输入框，识别结果如下", "No input box detected. Here is the transcript", "入力欄が見つからないため、認識結果をここに表示します"), text);
        false
    }

    // === 修订模式：选中文本 + 口述修改意见 ===
    fn start_revision(self: &Arc<Self>) {
        {
            let mut r = self.rev.lock().unwrap();
            if r.active || self.rec.lock().unwrap().recording {
                return;
            }
            // 先占位：抓取选中文本要几百毫秒，期间的停止键单击应视为「取消修订」而非「开始普通录音」
            r.active = true;
            r.task = Some(self.begin_task());
        }

        let selected = clipboard::get_selected_text();
        {
            let mut r = self.rev.lock().unwrap();
            if !r.active {
                return; // 抓取期间被停止键取消
            }
            if selected.is_empty() {
                r.active = false;
                r.task = None;
            } else {
                sound::play_sync(Cue::ReviseStart);
                r.selected = Some(selected.clone());
                r.recorder = Some(Recorder::start());
                r.screen = Some(screen::begin(&self.config()));
            }
        }
        if selected.is_empty() {
            self.notify_error(tr("请先选中要修改的文本", "Select the text you want to edit first", "修正したいテキストを先に選択してください"));
            return;
        }
        self.set_tray(TrayState::Recording);
        overlay::set(Mode::Revising);
        if self.config().mute_while_recording {
            mute::begin();
        }
        log::info!("Revision recording started: {} chars selected", selected.chars().count());
    }

    /// 停止修订录音 → 转录 → LLM 修改 → 替换选中
    fn stop_revision(self: &Arc<Self>) {
        let (rec, selected, task) = {
            let mut r = self.rev.lock().unwrap();
            if !r.active {
                return;
            }
            r.active = false;
            (r.recorder.take(), r.selected.take().unwrap_or_default(), r.task.take())
        };
        let shot = self.rev.lock().unwrap().screen.take();
        let canceled = || task.as_ref().is_some_and(Task::canceled);
        if let Some(r) = &rec {
            r.stop();
        }
        mute::end();
        sound::play(Cue::Stop);
        overlay::set(Mode::Processing);

        let cfg = self.config();
        let Some(audio) = self.take_audio(rec.as_deref(), "Revision recording") else {
            return;
        };
        let result = (|| -> Result<(), String> {
            let instruction = self.transcriber.transcribe(&audio, &cfg)?;
            if instruction.trim().is_empty() || canceled() {
                return Ok(());
            }
            log::info!("Revision instruction: {instruction}");
            let (instruction, _) = if cfg.vocab_enabled { vocab::apply_confident(&instruction) } else { (instruction, vec![]) };
            let glossary = cfg.vocab_enabled.then(|| vocab::glossary(&vocab::relevant(&format!("{selected}\n{instruction}")))).flatten();
            overlay::set(Mode::Polishing);
            let shot = shot.and_then(screen::Pending::take);
            let revised = llm::revise_text(&selected, &instruction, shot.as_ref(), glossary.as_deref(), &cfg)?;
            if canceled() {
                return Ok(());
            }
            if revised.is_empty() || revised == selected {
                self.notify_error(tr("未检测到修改意见", "No edit instruction detected", "修正の指示が聞き取れませんでした"));
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
            let typable = focus::can_type_here();
            if typable != Typable::No {
                // 粘贴替换选中，之后恢复原剪贴板；判断不了能否输入时不恢复，没替换成功的话用户可以自己粘贴
                let pasted = if typable == Typable::Yes { clipboard::paste_text(&revised) } else { clipboard::paste_text_keep(&revised) };
                log::info!("Revised: {revised}");
                self.notify(tr("已修改", "Text updated", "テキストを修正しました"));
                if pasted && cfg.learn_from_edits {
                    learn::watch(self.app.clone(), revised.clone(), cfg.clone());
                }
            } else {
                // 选区不可编辑（如网页正文），把修改结果放进卡片，由用户决定复制到哪
                log::info!("Revised (result card): {revised}");
                result_card::show(&self.app, tr("选中的内容不可编辑，修改结果如下", "The selection is not editable. Here is the edited text", "選択した内容は編集できないため、修正結果をここに表示します"), &revised);
            }
            Ok(())
        })();
        if canceled() {
            return;
        }
        if let Err(e) = result {
            log::error!("Revision error: {e}");
            self.notify_error(&trf!("修订失败：{e}", "Edit failed: {e}", "修正に失敗しました: {e}"));
        }
        self.finish_overlay();
        self.flash_done();
    }
}

/// 把光标前刚粘贴的 old 换成 new：按 Shift+← 选中 old 再粘贴。
/// 读得到光标前的文字时先核对它确实以 old 结尾（只比较末尾一段、忽略空白），对不上就不动输入框，
/// 把 new 放进剪贴板并返回 Err；读不到（如浏览器网页）时直接替换
fn replace_before_caret(old: &str, new: &str) -> Result<(), String> {
    if let Some(ctx) = focus::caret_context() {
        let squeeze = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<Vec<_>>();
        let before = squeeze(&ctx.before);
        let old_tail = squeeze(old);
        let old_tail = &old_tail[old_tail.len().saturating_sub(200)..];
        if !before.ends_with(old_tail) {
            log::info!("Retranslate: text before the cursor changed, copying instead of replacing");
            clipboard::set_text(new);
            return Err(tr(
                "输入框里的文字已经变了，没有替换；新译文已复制，可以自己粘贴",
                "The text in the input box has changed, so it was not replaced. The new translation is on the clipboard",
                "入力欄のテキストが変わっているため置き換えませんでした。新しい訳文をコピーしたので、貼り付けてください",
            )
            .into());
        }
    }
    // 光标每次移动一个字符；粘贴时的 \r\n 在输入框里也只算一个位置
    input::select_back(old.chars().filter(|&c| c != '\r').count());
    thread::sleep(Duration::from_millis(80));
    clipboard::paste_text(new);
    Ok(())
}

/// 录音太短或文字太少、不值得交给 LLM 整段处理时，返回原因（写日志用）
fn too_short(text: &str, seconds: f32, min_seconds: u32, min_chars: usize) -> Option<String> {
    if seconds < min_seconds as f32 {
        return Some(format!("short recording: {seconds:.1}s < {min_seconds}s"));
    }
    let plain = text.chars().filter(|c| !c.is_whitespace()).count();
    (plain < min_chars).then(|| format!("short text: {plain} < {min_chars} chars"))
}
