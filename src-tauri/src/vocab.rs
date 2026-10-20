//! 用户词库：专有名词等的正确写法，以及见过的误识别写法
//!
//! 存在 %APPDATA%\Aoide\vocabulary.json。来源有两种：设置页手动添加；用户手动修改识别结果时自动学习（learn.rs）。
//! 使用方式（controller.rs）：
//! - 高置信度的误识别写法（手动添加的，或同一纠正出现过 2 次以上）直接在本地替换
//! - 其余与识别结果相关的词条（出现了误识别写法，或有读音相近的片段）交给 LLM 判断
//! - whisper 模型额外把词条放进初始提示，让识别本身更偏向这些词

use crate::phonetic::{self, is_cjk};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
}

fn enabled_default() -> bool {
    true
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

fn save(entries: &[Entry]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    if let Err(e) = std::fs::write(file(), serde_json::to_string_pretty(entries).expect("vocabulary is serializable")) {
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
        return Err(crate::i18n::tr("请填写正确写法", "Enter the correct spelling").into());
    }
    with(|entries| {
        let idx = match entries.iter().position(|e| e.term == term) {
            Some(i) => i,
            None => {
                let id = new_id(entries);
                entries.push(Entry { id, term: term.into(), variants: vec![], manual: true, enabled: true });
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
    for e in entries.iter().filter(|e| e.enabled) {
        for v in e.variants.iter().filter(|v| v.confident() && safe_to_replace(&v.text)) {
            // 误识别写法是正确写法的一部分时（如「导图」之于「思维导图」）不替换，以免重复
            if e.term.contains(&v.text) || !out.contains(&v.text) {
                continue;
            }
            let replaced = replace_word(&out, &v.text, &e.term);
            if replaced != out {
                applied.push((v.text.clone(), e.term.clone()));
                out = replaced;
            }
        }
    }
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
    scored.into_iter().take(MAX_RELEVANT).map(|(_, e)| e).collect()
}

/// 给 LLM 的词库说明（作为 system 消息，不影响用户自定义的提示词）；没有相关词条时返回 None
pub fn glossary(entries: &[Entry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let en = crate::i18n::is_en();
    let mut s = String::from(if en {
        "User vocabulary. Speech recognition often gets these words wrong. If the text contains a word that sounds like one of them and is clearly a misrecognition, write it the way it is spelled here. Leave unrelated words alone.\n"
    } else {
        "用户词库：语音识别经常把下面这些词识别错。如果文字里出现读音相近、明显是误识别的词，请改成这里的正确写法；不相关的词不要改动。\n"
    });
    for e in entries {
        let seen: Vec<&str> = e.variants.iter().map(|v| v.text.as_str()).collect();
        if seen.is_empty() {
            s.push_str(&format!("- {}\n", e.term));
        } else if en {
            s.push_str(&format!("- {} (misrecognized before as: {})\n", e.term, seen.join(", ")));
        } else {
            s.push_str(&format!("- {}（曾被误识别为：{}）\n", e.term, seen.join("、")));
        }
    }
    Some(s)
}

/// whisper 初始提示：启用的词条，最多约 200 字符
pub fn prompt_terms() -> String {
    let mut out = String::new();
    for e in all().iter().filter(|e| e.enabled) {
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
}
