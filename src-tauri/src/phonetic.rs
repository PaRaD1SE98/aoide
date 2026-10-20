//! 发音相似度：判断用户的修改是不是在纠正语音识别错误（读音相近才算），以及识别结果里有没有和词条读音相近的片段
//!
//! - 中文转拼音后比较：忽略声调，多音字取最接近的读音；口音常混的音视为相近（平翘舌 z/zh、c/ch、s/sh，
//!   n/l/r，f/h，前后鼻音 an/ang、en/eng、in/ing）
//! - 中英混合（音译，如「奥伊德」→「Aoide」）：把拼音和英文都当作字母串，比较编辑距离

use pinyin::ToPinyinMulti;

pub fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}')
}

/// 一个汉字的全部读音（不带声调）
fn readings(c: char) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = c.to_pinyin_multi().map(|m| m.into_iter().map(|p| p.plain()).collect()).unwrap_or_default();
    v.dedup();
    v
}

/// 把一个拼音音节里口音常混的音归一
fn fuzzy_syllable(s: &str) -> String {
    let mut s = s.to_string();
    for (from, to) in [("zh", "z"), ("ch", "c"), ("sh", "s")] {
        if let Some(rest) = s.strip_prefix(from) {
            s = format!("{to}{rest}");
            break;
        }
    }
    match s.chars().next() {
        Some('l' | 'r') => s.replace_range(0..1, "n"),
        Some('f') => s.replace_range(0..1, "h"),
        _ => {}
    }
    for (from, to) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
        if let Some(head) = s.strip_suffix(from) {
            s = format!("{head}{to}");
            break;
        }
    }
    s
}

/// 两个音节的差异：0 完全相同；0.25 只差口音常混的音；否则按字母编辑距离
fn syllable_cost(a: &str, b: &str) -> f32 {
    if a == b {
        return 0.0;
    }
    let (fa, fb) = (fuzzy_syllable(a), fuzzy_syllable(b));
    if fa == fb {
        0.25
    } else {
        1.0 - strsim::normalized_levenshtein(&fa, &fb) as f32
    }
}

/// 两个汉字的读音差异（多音字取最接近的读音）；没有读音的字符按是否相同计
fn char_cost(a: char, b: char) -> f32 {
    if a == b {
        return 0.0;
    }
    let (ra, rb) = (readings(a), readings(b));
    if ra.is_empty() || rb.is_empty() {
        return 1.0;
    }
    ra.iter().flat_map(|x| rb.iter().map(move |y| syllable_cost(x, y))).fold(1.0, f32::min)
}

/// 字母串表示：汉字取第一个读音的拼音，西文转小写；再统一做模糊化，便于中英之间比较
fn letters(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if is_cjk(c) {
            if let Some(r) = readings(c).first() {
                out.push_str(&fuzzy_syllable(r));
            }
        } else if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    let out = out.replace("ph", "f").replace("zh", "z").replace("ch", "c").replace("sh", "s").replace("ng", "n");
    // 连续重复的字母合并（whisper → wisper、apple → aple）
    let mut chars: Vec<char> = out.chars().collect();
    chars.dedup();
    chars.into_iter().collect()
}

/// 读音相似度 0~1
pub fn similarity(a: &str, b: &str) -> f32 {
    let ca: Vec<char> = a.chars().filter(|c| !c.is_whitespace()).collect();
    let cb: Vec<char> = b.chars().filter(|c| !c.is_whitespace()).collect();
    if ca.is_empty() || cb.is_empty() {
        return 0.0;
    }
    if ca == cb {
        return 1.0;
    }
    if ca.iter().all(|&c| is_cjk(c)) && cb.iter().all(|&c| is_cjk(c)) {
        // 按音节对齐的编辑距离：插入 / 删除一个字代价 1，替换代价为读音差异
        let (n, m) = (ca.len(), cb.len());
        let mut dp = vec![vec![0.0f32; m + 1]; n + 1];
        for (i, row) in dp.iter_mut().enumerate() {
            row[0] = i as f32;
        }
        for j in 0..=m {
            dp[0][j] = j as f32;
        }
        for i in 1..=n {
            for j in 1..=m {
                let sub = dp[i - 1][j - 1] + char_cost(ca[i - 1], cb[j - 1]);
                dp[i][j] = sub.min(dp[i - 1][j] + 1.0).min(dp[i][j - 1] + 1.0);
            }
        }
        return (1.0 - dp[n][m] / n.max(m) as f32).max(0.0);
    }
    let (la, lb) = (letters(a), letters(b));
    if la.is_empty() || lb.is_empty() {
        return 0.0;
    }
    strsim::normalized_levenshtein(&la, &lb) as f32
}

/// 文本里最像 term 的片段的相似度（找识别结果里有没有某个词条的误识别）。
/// 中文词条在连续汉字里滑动比较；西文词条和连续汉字（音译）、相邻的 1~3 个西文单词比较
pub fn best_match(text: &str, term: &str) -> f32 {
    let term_chars: Vec<char> = term.chars().filter(|c| !c.is_whitespace()).collect();
    if term_chars.len() < 2 {
        return 0.0;
    }
    // 把文本切成「连续汉字」和「西文单词」两种片段
    let mut runs: Vec<Vec<char>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if is_cjk(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
        if c.is_ascii_alphanumeric() {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    if !word.is_empty() {
        words.push(word);
    }

    let mut best = 0.0f32;
    let term_cjk = term_chars.iter().all(|&c| is_cjk(c));
    let (min_len, max_len) = if term_cjk {
        (term_chars.len().saturating_sub(1).max(2), term_chars.len() + 1)
    } else {
        // 音译：大约每个汉字对应 1~3 个字母
        (1, letters(term).len().div_ceil(2) + 1)
    };
    for run in &runs {
        for len in min_len..=max_len.min(run.len()) {
            for w in run.windows(len) {
                best = best.max(similarity(&w.iter().collect::<String>(), term));
            }
        }
    }
    if !term_cjk {
        for len in 1..=3.min(words.len()) {
            for w in words.windows(len) {
                best = best.max(similarity(&w.join(" "), term));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_sounds() {
        // 同音字
        assert_eq!(similarity("思维到图", "思维导图"), 1.0);
        assert_eq!(similarity("前段", "前端"), 1.0);
        // 平翘舌、前后鼻音、n/l：口音导致的误识别
        assert!(similarity("四十", "事实") >= 0.75, "{}", similarity("四十", "事实"));
        assert!(similarity("陈述", "成熟") >= 0.75, "{}", similarity("陈述", "成熟"));
        assert!(similarity("牛奶", "流来") >= 0.75, "{}", similarity("牛奶", "流来"));
        // 中英音译 / 英文近音
        assert!(similarity("奥伊德", "Aoide") >= 0.75, "{}", similarity("奥伊德", "Aoide"));
        assert!(similarity("wisper", "Whisper") >= 0.75, "{}", similarity("wisper", "Whisper"));
        assert!(similarity("Tori", "Tauri") >= 0.45, "{}", similarity("Tori", "Tauri"));
    }

    #[test]
    fn different_words() {
        // 用户改了主意，不是识别错误
        assert!(similarity("下午", "晚上") < 0.45, "{}", similarity("下午", "晚上"));
        assert!(similarity("开会", "吃饭") < 0.45, "{}", similarity("开会", "吃饭"));
        assert!(similarity("Monday", "Friday") < 0.75, "{}", similarity("Monday", "Friday"));
        assert!(similarity("苹果", "Banana") < 0.45, "{}", similarity("苹果", "Banana"));
    }

    #[test]
    fn finds_misrecognitions_in_text() {
        assert!(best_match("我用思维到图整理了一下", "思维导图") >= 0.9);
        assert!(best_match("今天试了一下奥伊德的语音输入", "Aoide") >= 0.75);
        assert!(best_match("this app is built with tori and rust", "Tauri") >= 0.6);
        assert!(best_match("今天天气很好", "思维导图") < 0.6);
    }
}
