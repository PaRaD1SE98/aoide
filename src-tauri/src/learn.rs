//! 从用户的手动修改中学习：识别结果粘贴进输入框后，观察这段文字被改成了什么，
//! 比对出「误识别 → 正确写法」存入词库（vocab.rs），并弹出可撤销的提示（learned_card.rs）
//!
//! 流程：
//! 1. 粘贴后记下输入框全文，用插入位置前后的原文作为锚点，之后每 0.5 秒、以及每次按键后，重新定位这段文字。
//!    按键后的读取让「改完马上按回车发送」也能读到清空前的最后一版
//! 2. 改动停下 2.5 秒、焦点离开输入框、输入框被清空（聊天发送）、开始下一次语音输入时，比对一次
//! 3. 比对按字（中文）/ 按词（西文）进行，只看「替换」；中文的改动扩展到完整的词（jieba 分词），
//!    改动太大（整句重写）、纯增删、只改标点大小写、含数字的都不算
//! 4. 读音相似度 ≥ 0.75 直接入库；0.45 ~ 0.75 配置了 LLM 时交给 LLM 判断；更低的视为用户改了主意

use crate::config::Config;
use crate::focus::TextWatch;
use crate::phonetic::{self, is_cjk};
use crate::{learned_card, llm, vocab};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// 读音相似度达到这个值直接认定为识别错误
const ACCEPT: f32 = 0.75;
/// 低于这个值认定为用户改了主意；中间的交给 LLM 判断
const REJECT: f32 = 0.45;

const POLL: Duration = Duration::from_millis(500);
/// 检查有没有按键的间隔
const TICK: Duration = Duration::from_millis(25);
/// 按键后等多久再读，让目标程序先处理完这个键
const KEY_DELAY: Duration = Duration::from_millis(50);
/// 按键触发的读取，两次之间至少隔多久（连续打字时不至于读得太频繁）
const MIN_GAP: Duration = Duration::from_millis(100);
/// 改动停下多久算「改完了」
const SETTLE: Duration = Duration::from_millis(2500);
/// 焦点离开 / 文字找不到持续多久后结束观察
const GONE: Duration = Duration::from_millis(1200);
/// 最长观察时间
const MAX_WATCH: Duration = Duration::from_secs(180);

/// 每开始一次新的观察（或新的语音输入）加一，旧的观察据此结束
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 开始新的语音输入时调用：结束正在进行的观察（结束前会把已有的修改比对一次）
pub fn stop_watching() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    pub wrong: String,
    pub right: String,
    pub score: f32,
}

/// 粘贴完成后调用：在后台观察焦点输入框里这段文字的修改
pub fn watch(app: AppHandle, inserted: String, cfg: Config) {
    let inserted = inserted.trim().replace("\r\n", "\n");
    if inserted.chars().count() < 2 {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    thread::spawn(move || {
        // 等目标程序处理完粘贴
        thread::sleep(Duration::from_millis(400));
        let Some(w) = TextWatch::focused() else {
            log::info!("Learn: focused control is not readable, not watching");
            return;
        };
        let timing = Timing { poll: POLL, tick: TICK, key_delay: KEY_DELAY, min_gap: MIN_GAP, settle: SETTLE, gone: GONE, max: MAX_WATCH };
        observe(&w, &inserted, &GENERATION, generation, timing, |old, new| process(&app, old, new, &cfg));
    });
}

/// 被观察的输入框：实际是 focus::TextWatch，测试里用按时间变化的脚本模拟
trait Source {
    fn text(&self) -> Option<String>;
    fn still_focused(&self) -> bool;
    /// 用户按下的键的计数，变了就说明按过键
    fn keys(&self) -> u64;
}

impl Source for TextWatch {
    fn text(&self) -> Option<String> {
        TextWatch::text(self)
    }
    fn still_focused(&self) -> bool {
        TextWatch::still_focused(self)
    }
    fn keys(&self) -> u64 {
        crate::hotkeys::key_presses()
    }
}

#[derive(Clone, Copy)]
struct Timing {
    poll: Duration,
    tick: Duration,
    key_delay: Duration,
    min_gap: Duration,
    settle: Duration,
    gone: Duration,
    max: Duration,
}

/// 观察循环：每当一版修改稳定下来、或观察结束时，调用 on_edit(上次比对过的版本, 现在的版本)。
/// 计数器 counter 变得不等于 generation 时（开始了新的语音输入）结束观察
fn observe(src: &impl Source, inserted: &str, counter: &AtomicU64, generation: u64, timing: Timing, mut on_edit: impl FnMut(&str, &str)) {
    let Some(start) = src.text() else { return };
    let Some(pos) = start.rfind(inserted) else {
        log::info!("Learn: inserted text not found in the control, not watching");
        return;
    };
    let prefix = start[..pos].to_string();
    let suffix = start[pos + inserted.len()..].to_string();
    let started = Instant::now();
    let mut processed = inserted.to_string(); // 已比对过的版本
    let mut current = inserted.to_string(); // 最近一次定位到的版本
    let mut changed_at = Instant::now();
    let mut gone_since: Option<Instant> = None;
    let mut last_read = Instant::now();
    let mut seen_keys = src.keys();
    // 按过键、还没读的话，最后一次按键的时间
    let mut key_at: Option<Instant> = None;

    loop {
        thread::sleep(timing.tick);
        if counter.load(Ordering::SeqCst) != generation || started.elapsed() > timing.max {
            break;
        }
        let keys = src.keys();
        if keys != seen_keys {
            seen_keys = keys;
            key_at = Some(Instant::now());
        }
        // 读取只更新「最近一次看到的版本」；比对仍要等改动停下、焦点离开、文字消失等（见下面）
        let key_due = key_at.is_some_and(|t| t.elapsed() >= timing.key_delay) && last_read.elapsed() >= timing.min_gap;
        if key_due || last_read.elapsed() >= timing.poll {
            key_at = None;
            last_read = Instant::now();
            let located = src.text().and_then(|s| region(&s, &prefix, &suffix).map(str::to_string));
            match located.filter(|r| !r.trim().is_empty()) {
                Some(r) => {
                    if r != current {
                        current = r;
                        changed_at = Instant::now();
                    }
                    gone_since = if src.still_focused() { None } else { gone_since.or(Some(Instant::now())) };
                }
                // 找不到这段文字（比如消息已发送、输入框被清空）：保留最后一次看到的版本
                None => gone_since = gone_since.or(Some(Instant::now())),
            }
        }
        if gone_since.is_some_and(|t| t.elapsed() >= timing.gone) {
            break;
        }
        if current != processed && changed_at.elapsed() >= timing.settle {
            on_edit(&processed, &current);
            processed = current.clone();
        }
    }
    if current != processed {
        on_edit(&processed, &current);
    }
}

/// 在全文里定位插入的那段文字：优先用完整的前后文，前后文被改动过时退而用紧挨着的 20 个字符
fn region<'a>(s: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() + suffix.len() && s.starts_with(prefix) && s.ends_with(suffix) {
        return Some(&s[prefix.len()..s.len() - suffix.len()]);
    }
    let tail: String = {
        let chars: Vec<char> = prefix.chars().collect();
        chars[chars.len().saturating_sub(20)..].iter().collect()
    };
    let head: String = suffix.chars().take(20).collect();
    let a = if tail.is_empty() { 0 } else { s.find(&tail)? + tail.len() };
    let b = if head.is_empty() { s.len() } else { a + s[a..].find(&head)? };
    Some(&s[a..b])
}

/// 比对一次修改，把认定为识别错误的纠正存入词库并弹出提示
fn process(app: &AppHandle, old: &str, new: &str, cfg: &Config) {
    let llm_ready = !cfg.polish_api.trim().is_empty();
    let mut learned = Vec::new();
    for c in extract_corrections(old, new) {
        let accept = if c.score >= ACCEPT {
            true
        } else if c.score >= REJECT && llm_ready {
            match llm::judge_correction(&c.wrong, &c.right, old, new, cfg) {
                Ok(yes) => yes,
                Err(e) => {
                    log::warn!("Learn: LLM judgement failed: {e}");
                    false
                }
            }
        } else {
            false
        };
        log::info!("Learn: {:?} -> {:?} (score {:.2}) accepted={accept}", c.wrong, c.right, c.score);
        if accept {
            if let Some(change) = vocab::learn(&c.wrong, &c.right) {
                learned.push((c, change));
            }
        }
    }
    if !learned.is_empty() {
        let _ = app.emit("vocab-changed", ());
        learned_card::show(app, learned);
    }
}

/// 分词单位：一个汉字、一个西文单词（字母数字连写）、一段空白，或一个其他字符；返回字符下标范围
fn tokens(chars: &[char]) -> Vec<(usize, usize)> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '\'' || c == '-';
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut j = i + 1;
        if word(c) {
            while j < chars.len() && word(chars[j]) {
                j += 1;
            }
        } else if c.is_whitespace() {
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
        }
        out.push((i, j));
        i = j;
    }
    out
}

fn jieba() -> &'static jieba_rs::Jieba {
    static J: OnceLock<jieba_rs::Jieba> = OnceLock::new();
    J.get_or_init(jieba_rs::Jieba::new)
}

/// 去掉两端的空白和标点
fn trim_marks(s: &str) -> String {
    s.trim_matches(|c: char| !(c.is_alphanumeric() || is_cjk(c))).to_string()
}

/// 比对识别结果和用户改后的版本，找出可能是「纠正识别错误」的替换
pub fn extract_corrections(old: &str, new: &str) -> Vec<Correction> {
    let (oc, nc): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let (ot, nt) = (tokens(&oc), tokens(&nc));
    let text = |chars: &[char], (a, b): (usize, usize)| chars[a..b].iter().collect::<String>();
    let os: Vec<String> = ot.iter().map(|&r| text(&oc, r)).collect();
    let ns: Vec<String> = nt.iter().map(|&r| text(&nc, r)).collect();
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &os, &ns);

    // 改动的比例太大：用户重写了这段话，不是在纠正识别错误
    let (mut changed_old, mut changed_new) = (0, 0);
    for op in &ops {
        let (tag, o, n) = op.as_tag_tuple();
        if tag != similar::DiffTag::Equal {
            changed_old += o.len();
            changed_new += n.len();
        }
    }
    if changed_old.max(changed_new) * 2 > os.len().max(ns.len()).max(1) {
        return Vec::new();
    }

    // 相邻的非相等操作合并成一处修改
    let mut hunks: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = Vec::new();
    let mut open = false;
    for op in &ops {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            open = false;
        } else if open {
            let last = hunks.last_mut().unwrap();
            last.0.end = o.end;
            last.1.end = n.end;
        } else {
            hunks.push((o, n));
            open = true;
        }
    }

    let mut out = Vec::new();
    for (o, n) in hunks {
        if o.is_empty() || n.is_empty() {
            continue; // 纯增删
        }
        let (mut oa, mut ob) = (ot[o.start].0, ot[o.end - 1].1);
        let (mut na, mut nb) = (nt[n.start].0, nt[n.end - 1].1);
        // 读音相似度只比较实际改动的部分；下面扩展出来的上下文两边相同，算进去会抬高分数
        let core_wrong: String = oc[oa..ob].iter().collect();
        let core_right: String = nc[na..nb].iter().collect();
        // 中文的改动扩展到完整的词：只改了「到→导」时，要学的是「思维到图 → 思维导图」。
        // 分词可能把专有名词切碎（思维 / 导图），不足 4 个字时再并入相邻的词（先左后右）
        if nc[na..nb].iter().any(|&c| is_cjk(c)) {
            let new_text: String = nc.iter().collect();
            let words = jieba().cut(&new_text, true);
            let hit: Vec<usize> = (0..words.len()).filter(|&i| words[i].start < nb && words[i].end > na).collect();
            let (mut first, mut last) = (hit.first().copied().unwrap_or(0), hit.last().copied().unwrap_or(0));
            let cjk_word = |i: usize| words[i].word.chars().all(is_cjk);
            while !hit.is_empty() && words[last].end - words[first].start < 4 {
                if first > 0 && cjk_word(first - 1) {
                    first -= 1;
                } else if last + 1 < words.len() && cjk_word(last + 1) {
                    last += 1;
                } else {
                    break;
                }
            }
            let (ea, eb) = if hit.is_empty() { (na, nb) } else { (words[first].start, words[last].end) };
            let (left, right) = (na - ea, eb - nb);
            // 扩展出来的部分在改前改后必须一致（来自未改动的上下文），且词不能太长
            if left <= oa && ob + right <= oc.len() && eb - ea <= 10
                && oc[oa - left..oa] == nc[ea..na] && oc[ob..ob + right] == nc[nb..eb]
            {
                (oa, ob, na, nb) = (oa - left, ob + right, ea, eb);
            }
        }
        let wrong = trim_marks(&oc[oa..ob].iter().collect::<String>());
        let right = trim_marks(&nc[na..nb].iter().collect::<String>());
        if wrong.is_empty() || right.is_empty() || wrong == right {
            continue;
        }
        // 只改大小写、只改数字写法（三 → 3）都不算识别错误
        if wrong.to_lowercase() == right.to_lowercase() || wrong.chars().chain(right.chars()).any(|c| c.is_ascii_digit()) {
            continue;
        }
        // 词条长度：2~12 个汉字，或 2 个字母以上、不超过 4 个单词
        let cjk = right.chars().filter(|&c| is_cjk(c)).count();
        let letters = right.chars().filter(|c| c.is_ascii_alphabetic()).count();
        let words = right.split_whitespace().count();
        if cjk > 12 || words > 4 || (cjk < 2 && letters < 2) {
            continue;
        }
        let score = phonetic::similarity(&trim_marks(&core_wrong), &trim_marks(&core_right));
        out.push(Correction { wrong, right, score });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(old: &str, new: &str) -> Vec<(String, String)> {
        extract_corrections(old, new).into_iter().map(|c| (c.wrong, c.right)).collect()
    }

    #[test]
    fn extracts_corrected_words() {
        // 只改了一个字，扩展成完整的词
        assert_eq!(pairs("我用思维到图整理了一下需求", "我用思维导图整理了一下需求"), [("思维到图".into(), "思维导图".into())]);
        // 中文音译改成英文专有名词
        assert_eq!(pairs("今天试了一下奥伊德的语音输入", "今天试了一下Aoide的语音输入"), [("奥伊德".into(), "Aoide".into())]);
        // 英文单词
        assert_eq!(pairs("this app is built with tori and rust", "this app is built with Tauri and rust"), [("tori".into(), "Tauri".into())]);
        let c = extract_corrections("我用思维到图整理了一下需求", "我用思维导图整理了一下需求");
        assert!(c[0].score >= ACCEPT);
    }

    #[test]
    fn ignores_non_corrections() {
        // 整句重写
        assert!(pairs("明天下午三点开会", "改到后天上午吧").is_empty());
        // 纯增删、只改标点、只改大小写、数字写法
        assert!(pairs("明天开会", "明天下午开会").is_empty());
        assert!(pairs("明天开会。", "明天开会！").is_empty());
        assert!(pairs("hello world", "Hello world").is_empty());
        assert!(pairs("下午三点开会", "下午3点开会").is_empty());
    }

    #[test]
    fn scores_changed_minds_low() {
        let c = extract_corrections("我们明天下午去公司开会讨论", "我们明天晚上去公司开会讨论");
        assert_eq!(c.len(), 1);
        assert!(c[0].score < REJECT, "{c:?}");
    }

    /// 按时间变化的输入框：script 为 (从开始算起的毫秒数, 文字, 焦点是否还在)，取最近一个已到时间的状态；
    /// keys 为各次按键的毫秒数
    struct Scripted {
        start: Instant,
        script: Vec<(u64, Option<&'static str>, bool)>,
        keys: Vec<u64>,
    }

    impl Scripted {
        fn now(&self) -> (Option<&'static str>, bool) {
            let t = self.start.elapsed().as_millis() as u64;
            let (_, text, focused) = self.script.iter().rev().find(|(at, ..)| *at <= t).unwrap();
            (*text, *focused)
        }
    }

    impl Source for Scripted {
        fn text(&self) -> Option<String> {
            self.now().0.map(str::to_string)
        }
        fn still_focused(&self) -> bool {
            self.now().1
        }
        fn keys(&self) -> u64 {
            let t = self.start.elapsed().as_millis() as u64;
            self.keys.iter().filter(|&&at| at <= t).count() as u64
        }
    }

    const FAST: Timing = Timing {
        poll: Duration::from_millis(10),
        tick: Duration::from_millis(5),
        key_delay: Duration::from_millis(10),
        min_gap: Duration::from_millis(10),
        settle: Duration::from_millis(120),
        gone: Duration::from_millis(60),
        max: Duration::from_secs(3),
    };

    fn run(script: Vec<(u64, Option<&'static str>, bool)>, inserted: &str, counter: &AtomicU64) -> Vec<(String, String)> {
        run_with(script, vec![], inserted, counter, FAST)
    }

    fn run_with(
        script: Vec<(u64, Option<&'static str>, bool)>,
        keys: Vec<u64>,
        inserted: &str,
        counter: &AtomicU64,
        timing: Timing,
    ) -> Vec<(String, String)> {
        let src = Scripted { start: Instant::now(), script, keys };
        let mut edits = Vec::new();
        observe(&src, inserted, counter, 0, timing, |a, b| edits.push((a.to_string(), b.to_string())));
        edits
    }

    #[test]
    fn observes_edit_then_focus_leaves() {
        let edits = run(
            vec![
                (0, Some("前文。我用思维到图整理需求"), true),
                (40, Some("前文。我用思维导图整理需求"), true),
                (400, Some("前文。我用思维导图整理需求"), false),
            ],
            "我用思维到图整理需求",
            &AtomicU64::new(0),
        );
        assert_eq!(edits, [("我用思维到图整理需求".into(), "我用思维导图整理需求".into())]);
    }

    #[test]
    fn keeps_last_edit_when_message_is_sent() {
        // 改完马上发送（输入框清空），还没等到「停下 2.5 秒」：用清空前最后看到的版本
        let edits = run(
            vec![(0, Some("思维到图很好用"), true), (30, Some("思维导图很好用"), true), (60, Some(""), true)],
            "思维到图很好用",
            &AtomicU64::new(0),
        );
        assert_eq!(edits, [("思维到图很好用".into(), "思维导图很好用".into())]);
    }

    #[test]
    fn reads_right_after_keys_between_polls() {
        // 改完不到一个定时读取的间隔就按回车发送：靠按键后的那次读取拿到清空前的版本
        let slow = Timing { poll: Duration::from_secs(10), ..FAST };
        let script = || vec![(0, Some("思维到图很好用"), true), (30, Some("思维导图很好用"), true), (80, Some(""), true)];
        let edits = run_with(script(), vec![30, 80], "思维到图很好用", &AtomicU64::new(0), slow);
        assert_eq!(edits, [("思维到图很好用".into(), "思维导图很好用".into())]);
        // 没有按键时，只靠定时读取就看不到这次修改（说明上面确实是按键触发的读取）
        let counter = AtomicU64::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                counter.fetch_add(1, Ordering::SeqCst);
            });
            assert!(run_with(script(), vec![], "思维到图很好用", &counter, slow).is_empty());
        });
    }

    #[test]
    fn compares_each_settled_version_once() {
        // 两次分开的修改：各比对一次，第二次以第一次改完的版本为基准
        let edits = run(
            vec![
                (0, Some("用奥伊德和tori写的"), true),
                (30, Some("用Aoide和tori写的"), true),
                (300, Some("用Aoide和Tauri写的"), true),
                (600, Some("用Aoide和Tauri写的"), false),
            ],
            "用奥伊德和tori写的",
            &AtomicU64::new(0),
        );
        assert_eq!(
            edits,
            [
                ("用奥伊德和tori写的".into(), "用Aoide和tori写的".into()),
                ("用Aoide和tori写的".into(), "用Aoide和Tauri写的".into()),
            ]
        );
    }

    #[test]
    fn stops_when_next_dictation_starts() {
        let counter = AtomicU64::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(80));
                counter.fetch_add(1, Ordering::SeqCst); // 开始了新的语音输入
            });
            let edits = run(vec![(0, Some("思维到图"), true), (30, Some("思维导图"), true)], "思维到图", &counter);
            assert_eq!(edits, [("思维到图".into(), "思维导图".into())]);
        });
    }

    #[test]
    fn locates_inserted_text() {
        assert_eq!(region("前文。思维导图很好用。后文", "前文。", "。后文"), Some("思维导图很好用"));
        // 前文被改动过：用紧挨着的 20 个字符定位
        assert_eq!(region("改过的前文。思维导图很好用。后文", "前文。", "。后文"), Some("思维导图很好用"));
        // 插入在末尾，后面又接着打了字
        assert_eq!(region("你好思维导图，再见", "你好", ""), Some("思维导图，再见"));
    }
}
