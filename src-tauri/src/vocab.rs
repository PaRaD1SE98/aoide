//! 用户词库：专有名词等的正确写法，以及见过的误识别写法
//!
//! 存在 %APPDATA%\Aoide\vocabulary.json。来源有两种：设置页手动添加；用户手动修改识别结果时自动学习（learn.rs）。
//! 使用方式（controller.rs）：
//! - 高置信度的误识别写法（手动添加的，或同一纠正出现过 2 次以上）直接在本地替换
//! - 其余与识别结果相关的词条（出现了误识别写法，或有读音相近的片段）交给 LLM 判断
//! - whisper 模型额外把词条放进初始提示，让识别本身更偏向这些词（优先最近用到的）
//!
//! 大小有上限（设置页的「词库大小上限」）：平时不删，保存时超出上限才按「最久没用到、用得最少」
//! 删掉自动学到的词条，手动添加的不删。学到、命中替换、交给 LLM 参考，都算用到一次

use crate::phonetic::{self, is_cjk};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Variant {
    /// 误识别成的写法
    pub text: String,
    /// 自动学到的次数
    #[serde(default)]
    pub count: u32,
    /// 用户手动添加
    #[serde(default)]
    pub manual: bool,
}

impl Variant {
    /// 高置信度：手动添加的，或同一纠正出现过 2 次以上
    fn confident(&self) -> bool {
        self.manual || self.count >= 2
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: u64,
    /// 正确写法
    pub term: String,
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// 手动添加（false 为自动学到）
    #[serde(default)]
    pub manual: bool,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// 最后一次用到的时间（Unix 秒）；旧版本存的词条没有，按创建时间算（见 last_used）
    #[serde(default)]
    pub last_used: u64,
    /// 用到的次数
    #[serde(default)]
    pub uses: u32,
}

fn enabled_default() -> bool {
    true
}

impl Entry {
    /// 最后一次用到的时间；没记录过就用创建时间（id 是创建时的毫秒时间戳）
    fn last_used(&self) -> u64 {
        self.last_used.max(self.id / 1000)
    }

    fn touch(&mut self) {
        self.last_used = now_secs();
        self.uses = self.uses.saturating_add(1);
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 词库文件的大小上限（字节）
static LIMIT: AtomicU64 = AtomicU64::new(10 * 1024 * 1024);

/// 设置大小上限；当前已经超出时立即按新上限整理
pub fn set_size_limit(bytes: u64) {
    LIMIT.store(bytes.max(16 * 1024), Ordering::SeqCst);
    with(|entries| {
        if serialize(entries).len() as u64 > LIMIT.load(Ordering::SeqCst) {
            save(entries);
        }
    });
}

/// 词库文件当前的大小（字节）
pub fn size() -> u64 {
    with(|entries| serialize(entries).len() as u64)
}

fn serialize(entries: &[Entry]) -> String {
    serde_json::to_string_pretty(entries).expect("vocabulary is serializable")
}

/// 超出大小上限时，按「最久没用到、用得最少」删掉自动学到的词条，直到回到上限以内；返回删了几条
fn prune(entries: &mut Vec<Entry>) -> usize {
    let limit = LIMIT.load(Ordering::SeqCst) as usize;
    let mut total = serialize(entries).len();
    if total <= limit {
        return 0;
    }
    let mut candidates: Vec<(u64, u32, u64)> =
        entries.iter().filter(|e| !e.manual).map(|e| (e.last_used(), e.uses, e.id)).collect();
    candidates.sort();
    let mut removed = Vec::new();
    for (_, _, id) in candidates {
        if total <= limit {
            break;
        }
        if let Some(e) = entries.iter().find(|e| e.id == id) {
            // 这一条在文件里大约占的字节（缩进后的 JSON 再加分隔符）
            total = total.saturating_sub(serialize(std::slice::from_ref(e)).len());
            removed.push(id);
        }
    }
    entries.retain(|e| !removed.contains(&e.id));
    removed.len()
}

/// 这些词条用到了一次（命中替换、交给 LLM 参考）
fn touch(ids: &[u64]) {
    if ids.is_empty() {
        return;
    }
    with(|entries| {
        for e in entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            e.touch();
        }
        save(entries);
    })
}

/// 一次学习对词库的改动，用于撤销
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    NewEntry { id: u64 },
    NewVariant { id: u64, text: String },
    MoreCount { id: u64, text: String },
}

static STORE: Mutex<Option<Vec<Entry>>> = Mutex::new(None);

fn file() -> PathBuf {
    crate::config::data_dir().join("vocabulary.json")
}

fn with<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    let mut guard = STORE.lock().unwrap();
    let entries = guard.get_or_insert_with(|| {
        std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    });
    f(entries)
}

/// 保存；超出大小上限时先整理（见 prune）
fn save(entries: &mut Vec<Entry>) {
    let removed = prune(entries);
    if removed > 0 {
        log::info!("Vocabulary over its size limit: removed {removed} least recently used learned entries");
    }
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    if let Err(e) = std::fs::write(file(), serialize(entries)) {
        log::error!("Save vocabulary failed: {e}");
    }
}

fn new_id(entries: &[Entry]) -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    now.max(entries.iter().map(|e| e.id + 1).max().unwrap_or(0))
}

pub fn all() -> Vec<Entry> {
    with(|e| e.clone())
}

/// 手动添加：正确写法 + 可选的误识别写法；正确写法已存在时合并
pub fn add_manual(term: &str, variants: &[String]) -> Result<(), String> {
    let term = term.trim();
    if term.is_empty() {
        return Err(crate::i18n::tr("请填写正确写法", "Enter the correct spelling", "正しい表記を入力してください").into());
    }
    with(|entries| {
        let idx = match entries.iter().position(|e| e.term == term) {
            Some(i) => i,
            None => {
                let id = new_id(entries);
                entries.push(Entry { id, term: term.into(), variants: vec![], manual: true, enabled: true, last_used: now_secs(), uses: 0 });
                entries.len() - 1
            }
        };
        let entry = &mut entries[idx];
        entry.manual = true;
        for v in variants.iter().map(|v| v.trim()).filter(|v| !v.is_empty() && *v != term) {
            match entry.variants.iter_mut().find(|x| x.text == v) {
                Some(x) => x.manual = true,
                None => entry.variants.push(Variant { text: v.into(), count: 0, manual: true }),
            }
        }
        save(entries);
        Ok(())
    })
}

pub fn remove(id: u64) {
    with(|entries| {
        entries.retain(|e| e.id != id);
        save(entries);
    })
}

pub fn remove_variant(id: u64, text: &str) {
    with(|entries| {
        if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
            e.variants.retain(|v| v.text != text);
        }
        save(entries);
    })
}

pub fn set_enabled(id: u64, enabled: bool) {
    with(|entries| {
        if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
            e.enabled = enabled;
        }
        save(entries);
    })
}

/// 学到一次纠正「wrong → right」；返回对词库的改动（用于撤销）
pub fn learn(wrong: &str, right: &str) -> Option<Change> {
    let (wrong, right) = (wrong.trim(), right.trim());
    if wrong.is_empty() || right.is_empty() || wrong == right {
        return None;
    }
    with(|entries| {
        let change = match entries.iter_mut().find(|e| e.term == right) {
            Some(e) => {
                e.enabled = true;
                e.touch();
                match e.variants.iter_mut().find(|v| v.text == wrong) {
                    Some(v) => {
                        v.count += 1;
                        Change::MoreCount { id: e.id, text: wrong.into() }
                    }
                    None => {
                        e.variants.push(Variant { text: wrong.into(), count: 1, manual: false });
                        Change::NewVariant { id: e.id, text: wrong.into() }
                    }
                }
            }
            None => {
                let id = new_id(entries);
                entries.push(Entry {
                    id,
                    term: right.into(),
                    variants: vec![Variant { text: wrong.into(), count: 1, manual: false }],
                    manual: false,
                    enabled: true,
                    last_used: now_secs(),
                    uses: 1,
                });
                Change::NewEntry { id }
            }
        };
        save(entries);
        Some(change)
    })
}

/// 撤销学习
pub fn undo(changes: &[Change]) {
    with(|entries| {
        for c in changes.iter().rev() {
            match c {
                Change::NewEntry { id } => entries.retain(|e| e.id != *id),
                Change::NewVariant { id, text } => {
                    if let Some(e) = entries.iter_mut().find(|e| e.id == *id) {
                        e.variants.retain(|v| &v.text != text);
                    }
                }
                Change::MoreCount { id, text } => {
                    if let Some(v) = entries.iter_mut().find(|e| e.id == *id).and_then(|e| e.variants.iter_mut().find(|v| &v.text == text)) {
                        v.count = v.count.saturating_sub(1);
                    }
                }
            }
        }
        save(entries);
    })
}

/// 片段是否足够长、可以不看上下文直接替换：至少 3 个汉字，或至少 3 个字母的整词。
/// 两个字的（如「前段 → 前端」）直接替换会误伤「前段时间」，交给 LLM 结合上下文判断
fn safe_to_replace(s: &str) -> bool {
    let cjk = s.chars().filter(|&c| is_cjk(c)).count();
    let alpha = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
    cjk >= 3 || (cjk == 0 && alpha >= 3)
}

/// 把 text 里的 from 替换为 to；西文按整词匹配（前后不能紧挨字母数字）
fn replace_word(text: &str, from: &str, to: &str) -> String {
    let latin = !from.chars().any(is_cjk);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(from) {
        let before = rest[..pos].chars().last();
        let after = rest[pos + from.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() || is_cjk(c));
        out.push_str(&rest[..pos]);
        if !latin || (boundary(before) && boundary(after)) {
            out.push_str(to);
        } else {
            out.push_str(from);
        }
        rest = &rest[pos + from.len()..];
    }
    out.push_str(rest);
    out
}

/// 高置信度的误识别写法直接替换为正确写法；返回替换后的文字和替换了哪些
pub fn apply_confident(text: &str) -> (String, Vec<(String, String)>) {
    let entries = all();
    let mut out = text.to_string();
    let mut applied = Vec::new();
    let mut used = Vec::new();
    for e in entries.iter().filter(|e| e.enabled) {
        for v in e.variants.iter().filter(|v| v.confident() && safe_to_replace(&v.text)) {
            // 误识别写法是正确写法的一部分时（如「导图」之于「思维导图」）不替换，以免重复
            if e.term.contains(&v.text) || !out.contains(&v.text) {
                continue;
            }
            let replaced = replace_word(&out, &v.text, &e.term);
            if replaced != out {
                applied.push((v.text.clone(), e.term.clone()));
                used.push(e.id);
                out = replaced;
            }
        }
    }
    touch(&used);
    (out, applied)
}

/// 交给 LLM 的相关词条上限
const MAX_RELEVANT: usize = 20;

/// 与这段识别结果相关的词条：出现了误识别写法，或有和正确写法读音相近的片段（正确写法已经出现的不算）
pub fn relevant(text: &str) -> Vec<Entry> {
    let mut scored: Vec<(f32, Entry)> = all()
        .into_iter()
        .filter(|e| e.enabled && !text.contains(&e.term))
        .filter_map(|e| {
            let score = if e.variants.iter().any(|v| text.contains(&v.text)) {
                2.0
            } else {
                phonetic::best_match(text, &e.term)
            };
            (score >= 0.75).then_some((score, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let found: Vec<Entry> = scored.into_iter().take(MAX_RELEVANT).map(|(_, e)| e).collect();
    touch(&found.iter().map(|e| e.id).collect::<Vec<_>>());
    found
}

/// 给 LLM 的词库说明（作为 system 消息，不影响用户自定义的提示词）；没有相关词条时返回 None
pub fn glossary(entries: &[Entry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    // 和提示词一样用英文写
    let mut s = String::from(
        "User vocabulary. Speech recognition often gets these words wrong. If the text contains a word that sounds like one of them and is clearly a misrecognition, write it the way it is spelled here. Leave unrelated words alone.\n",
    );
    for e in entries {
        let seen: Vec<&str> = e.variants.iter().map(|v| v.text.as_str()).collect();
        if seen.is_empty() {
            s.push_str(&format!("- {}\n", e.term));
        } else {
            s.push_str(&format!("- {} (misrecognized before as: {})\n", e.term, seen.join(", ")));
        }
    }
    Some(s)
}

/// whisper 初始提示：启用的词条，最近用到的优先，最多约 200 字符
pub fn prompt_terms() -> String {
    let mut entries: Vec<Entry> = all().into_iter().filter(|e| e.enabled).collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.last_used()));
    let mut out = String::new();
    for e in &entries {
        if out.chars().count() + e.term.chars().count() > 200 {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&e.term.replace('\0', ""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_whole_words_only() {
        assert_eq!(replace_word("用 tori 写的 history", "tori", "Tauri"), "用 Tauri 写的 history");
        assert_eq!(replace_word("思维到图和思维到图", "思维到图", "思维导图"), "思维导图和思维导图");
        assert!(safe_to_replace("奥伊德") && safe_to_replace("tori"));
        assert!(!safe_to_replace("前段") && !safe_to_replace("到") && !safe_to_replace("ab"));
    }

    /// 超出大小上限：删最久没用到的自动学习词条，手动添加的再久没用也留着
    #[test]
    fn prunes_least_recently_used_learned_entries() {
        let entry = |id: u64, manual: bool, last_used: u64| Entry {
            id,
            term: format!("词条{id}"),
            variants: vec![Variant { text: format!("误识别{id}"), count: 1, manual: false }],
            manual,
            enabled: true,
            last_used,
            uses: 1,
        };
        let mut entries = vec![entry(1, false, 300), entry(2, true, 100), entry(3, false, 200), entry(4, false, 400)];
        let total = serialize(&entries).len();
        let one = serialize(&entries[..1]).len();
        // 上限只够少一条
        LIMIT.store((total - one) as u64, Ordering::SeqCst);
        let removed = prune(&mut entries);
        LIMIT.store(1024 * 1024, Ordering::SeqCst);
        assert_eq!(removed, 1);
        let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, [1, 2, 4]);
        // 没超出上限时一条不删
        assert_eq!(prune(&mut entries), 0);
    }
}
