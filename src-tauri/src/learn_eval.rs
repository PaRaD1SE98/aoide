//! 词库学习的评测集：一组「识别结果 → 用户改后」的例子和应该学到的词条，用真实的 LLM API 跑一遍，
//! 统计三种学习方式各自的正确率。改了学习流程（learn.rs）或相关提示词（llm.rs）之后都要跑：
//!
//!   cargo test --lib learn_eval -- --ignored --nocapture
//!
//! 用本机配置里的 LLM API。可选环境变量：EVAL_MODES=mechanical,hybrid,llm（默认全部）、
//! EVAL_RUNS=次数（默认 1，LLM 的回答有随机性，多跑几次看稳定性）、EVAL_ONLY=例子名的一部分。
//! 用户的 app.log 里出现新的误学（Learn: 开头的行）时，把它加进来

use crate::config::Config;
use crate::learn::{decide, Mode};
use crate::llm;

/// 一个例子：名字、识别结果、用户改后的版本、应该学到的词条。
/// 每个词条列出几种都算对的写法（比如「静止曲线 → 净值曲线」和「静止 → 净值」）；空表示什么都不该学
struct Case {
    name: &'static str,
    old: &'static str,
    new: &'static str,
    expect: &'static [&'static [(&'static str, &'static str)]],
}

const fn case(name: &'static str, old: &'static str, new: &'static str, expect: &'static [&'static [(&'static str, &'static str)]]) -> Case {
    Case { name, old, new, expect }
}

const NONE: &[&[(&str, &str)]] = &[];

const CASES: &[Case] = &[
    // —— 识别错误：应该学到 ——
    case("思维导图", "我用思维到图整理了一下需求", "我用思维导图整理了一下需求", &[&[("思维到图", "思维导图")]]),
    case("Tauri", "这个应用是用 tori 写的", "这个应用是用 Tauri 写的", &[&[("tori", "Tauri")]]),
    case("Aoide", "今天试了一下奥伊德的语音输入", "今天试了一下Aoide的语音输入", &[&[("奥伊德", "Aoide")]]),
    case("净值曲线（带「在」）", "我们来看一下在静止曲线上的表现", "我们来看一下在净值曲线上的表现", &[&[("静止曲线", "净值曲线"), ("静止", "净值")]]),
    case("净值图（带「作为」）", "把静值图作为主要的展示", "把净值图作为主要的展示", &[&[("静值", "净值"), ("静值图", "净值图")]]),
    case("所有格", "the Transancellous office said no", "the Chancellor's office said no", &[&[("Transancellous", "Chancellor")]]),
    case("休市", "下周一港股修饰，不用盯盘", "下周一港股休市，不用盯盘", &[&[("修饰", "休市"), ("港股修饰", "港股休市")]]),
    case("涨停不卖", "这只股票涨停补卖，等明天再看", "这只股票涨停不卖，等明天再看", &[&[("补卖", "不卖"), ("涨停补卖", "涨停不卖")]]),
    case("LLM", "这个功能交给 LN 来处理", "这个功能交给 LLM 来处理", &[&[("LN", "LLM")]]),
    case("UMass", "她去年从 Umax 毕业", "她去年从 UMass 毕业", &[&[("Umax", "UMass")]]),
    case("前端", "前段页面的样式还要再调一下", "前端页面的样式还要再调一下", &[&[("前段", "前端"), ("前段页面", "前端页面")]]),
    case("GitHub", "I pushed the code to get hub last night", "I pushed the code to GitHub last night", &[&[("get hub", "GitHub")]]),
    case(
        "一次改两处",
        "我们用科特林写安卓，数据库用波斯格瑞斯",
        "我们用Kotlin写安卓，数据库用PostgreSQL",
        &[&[("科特林", "Kotlin")], &[("波斯格瑞斯", "PostgreSQL")]],
    ),
    case("VS Code", "我平时在 VS 扣的里写代码", "我平时在 VS Code 里写代码", &[&[("VS 扣的", "VS Code"), ("扣的", "Code")]]),
    case("Kubernetes", "we deployed it on cuber netties yesterday", "we deployed it on Kubernetes yesterday", &[&[("cuber netties", "Kubernetes")]]),
    case("人名", "请把报告发给李思思，她明天要用", "请把报告发给李丝丝，她明天要用", &[&[("李思思", "李丝丝"), ("思思", "丝丝")]]),
    case(
        "长段落里的一处",
        "今天的会议主要讨论了三个问题。第一是下个季度的预算，第二是新员工的培训安排，第三是客户反馈的那个登录问题。关于登录问题，我们决定先用奥斯认证来替换现在的方案，下周给出初步结果。",
        "今天的会议主要讨论了三个问题。第一是下个季度的预算，第二是新员工的培训安排，第三是客户反馈的那个登录问题。关于登录问题，我们决定先用OAuth认证来替换现在的方案，下周给出初步结果。",
        &[&[("奥斯", "OAuth"), ("奥斯认证", "OAuth认证")]],
    ),
    case("识别错误 + 改了内容", "我用思维到图整理了下午的会议", "我用思维导图整理了晚上的会议", &[&[("思维到图", "思维导图")]]),
    case("Claude", "让克劳德帮我检查一下代码", "让Claude帮我检查一下代码", &[&[("克劳德", "Claude")]]),
    case("pandas", "用胖达斯读一下这个 CSV 文件", "用pandas读一下这个 CSV 文件", &[&[("胖达斯", "pandas")]]),
    case("权限", "我没有这个文件的全线", "我没有这个文件的权限", &[&[("全线", "权限")]]),
    case("部署", "代码写完了，今晚不叔到服务器上", "代码写完了，今晚部署到服务器上", &[&[("不叔", "部署")]]),
    // —— 不是识别错误：什么都不该学 ——
    case("改了时间", "我们明天下午去公司开会", "我们明天晚上去公司开会", NONE),
    case("换了说法", "下周的会议改到周三", "下周的会面改到周三", NONE),
    case("整句重写", "明天下午三点开会", "改到后天上午吧", NONE),
    case("发送后的占位文字", "既然你都发现有错了，那你就把它修了吧", "Queue another message", NONE),
    case("发送后的占位文字（短）", "帮我提交并且推送", "Queue another message", NONE),
    case("发送后的中文占位文字", "好的，我晚点再看", "输入消息", NONE),
    case("纯增加", "明天开会", "明天下午开会", NONE),
    case("只改标点", "明天开会。", "明天开会！", NONE),
    case("只改大小写", "hello world", "Hello world", NONE),
    case("数字写法", "下午三点开会", "下午3点开会", NONE),
    case("同义改写", "我觉得这个方案挺好的", "我认为这个方案很好", NONE),
    case("删语气词", "嗯，那个，我们明天开会吧", "我们明天开会吧", NONE),
    case("换词", "我们需要尽快处理这个问题", "我们需要马上处理这个问题", NONE),
    case("英文改写", "Can you send me the file today", "Could you send me the file today", NONE),
    case("英文换词", "The results look good to me", "The results look great to me", NONE),
    case("翻译成英文", "这个问题我们下周再讨论", "Let's discuss this next week", NONE),
    case("调语序", "我把文件发给了他", "我把文件发给他了", NONE),
    case("句末追加", "今天先到这里", "今天先到这里，明天继续", NONE),
    case("加客气话", "你帮我看一下这个", "麻烦你帮我看一下这个", NONE),
    case("改楼层", "会议在三楼会议室", "会议在五楼会议室", NONE),
    case("换了人", "把这个任务交给小王", "把这个任务交给小李", NONE),
    case("删重复", "这个功能非常非常重要", "这个功能非常重要", NONE),
    case("改地点", "我明天去北京出差", "我明天去上海出差", NONE),
    // —— 留出集：提示词里没有出现过的说法，用来看是不是只记住了提示词里的例子 ——
    case("留出·Redis", "我们用瑞迪斯做缓存", "我们用Redis做缓存", &[&[("瑞迪斯", "Redis")]]),
    case("留出·JSON", "这个接口返回的杰森格式不对", "这个接口返回的JSON格式不对", &[&[("杰森", "JSON")]]),
    case("留出·滞后", "这个项目的进度有点之后，得加人", "这个项目的进度有点滞后，得加人", &[&[("之后", "滞后")]]),
    case("留出·邮箱", "请把合同发到我的邮相", "请把合同发到我的邮箱", &[&[("邮相", "邮箱")]]),
    case("留出·预案", "上线前记得做好回滚预按", "上线前记得做好回滚预案", &[&[("预按", "预案"), ("回滚预按", "回滚预案")]]),
    case("留出·sync", "let's schedule a sink tomorrow morning", "let's schedule a sync tomorrow morning", &[&[("sink", "sync")]]),
    case("留出·auth", "the bug is in the off module", "the bug is in the auth module", &[&[("off", "auth")]]),
    case("留出·同音姓", "明天和王建国开会", "明天和汪建国开会", &[&[("王建国", "汪建国"), ("王", "汪")]]),
    case("留出·Python", "用拍森写个脚本把数据导出来", "用Python写个脚本把数据导出来", &[&[("拍森", "Python")]]),
    case(
        "留出·长段落两处",
        "上周我们完成了用户登录模块的开发，这周主要做支付功能，支付会接入微信和知付宝。另外数据库从买SQL迁移到了PostgreSQL。",
        "上周我们完成了用户登录模块的开发，这周主要做支付功能，支付会接入微信和支付宝。另外数据库从MySQL迁移到了PostgreSQL。",
        &[&[("知付宝", "支付宝")], &[("买SQL", "MySQL")]],
    ),
    case("留出·改日期", "这个需求我们下个月再做", "这个需求我们下周再做", NONE),
    case("留出·改颜色", "我比较喜欢用红色", "我比较喜欢用蓝色", NONE),
    case("留出·改价格", "价格大概在五百左右", "价格大概在八百左右", NONE),
    case("留出·换说法", "我们先开个短会", "我们先开个碰头会", NONE),
    case("留出·改程度", "这个bug很严重", "这个bug比较严重", NONE),
    case("留出·英文换说法", "Please review the PR when you have time", "Please review the PR when you get a chance", NONE),
    case("留出·英文换词", "I think we should ship it", "I believe we should ship it", NONE),
    case("留出·换人", "这份报告发给张经理", "这份报告发给李总", NONE),
    case("留出·改日子", "我明天请假", "我后天请假", NONE),
    case("留出·近义词", "开会的时候记得带电脑", "开会的时候记得带笔记本", NONE),
    case("留出·换动词", "用户反馈说页面加载很慢", "用户反馈说页面打开很慢", NONE),
    case("留出·日文占位文字", "了解です、あとで確認します", "メッセージを入力", NONE),
    case("留出·中间插词", "我们需要讨论方案", "我们需要详细讨论方案", NONE),
];

/// 一个例子在一种方式下的结果
struct Outcome {
    learned: Vec<(String, String)>,
    /// 没学到的词条（取第一种写法显示）
    missed: Vec<(String, String)>,
    /// 不该学却学了的
    extra: Vec<(String, String)>,
}

fn judge(case: &Case, learned: Vec<(String, String)>) -> Outcome {
    let hits = |forms: &[(&str, &str)], (w, r): &(String, String)| forms.iter().any(|(fw, fr)| fw == w && fr == r);
    let missed = case
        .expect
        .iter()
        .filter(|forms| !learned.iter().any(|l| hits(forms, l)))
        .map(|forms| (forms[0].0.to_string(), forms[0].1.to_string()))
        .collect();
    let extra = learned.iter().filter(|l| !case.expect.iter().any(|forms| hits(forms, l))).cloned().collect();
    Outcome { learned, missed, extra }
}

fn run(case: &Case, mode: Mode, cfg: &Config) -> Outcome {
    let learned = decide(case.old, case.new, mode, |candidates| llm::review_corrections(case.old, case.new, candidates, cfg));
    judge(case, learned.into_iter().map(|c| (c.wrong, c.right)).collect())
}

#[test]
#[ignore]
fn learn_eval() {
    let cfg = Config::load();
    assert!(!cfg.polish_api.trim().is_empty(), "configure an LLM API first");
    let modes: Vec<Mode> = std::env::var("EVAL_MODES")
        .unwrap_or_else(|_| "mechanical,hybrid,llm".into())
        .split(',')
        .map(|m| match m.trim() {
            "mechanical" => Mode::Mechanical,
            "hybrid" => Mode::Hybrid,
            "llm" => Mode::Llm,
            other => panic!("unknown mode {other}"),
        })
        .collect();
    let runs: usize = std::env::var("EVAL_RUNS").ok().and_then(|n| n.parse().ok()).unwrap_or(1);
    let only = std::env::var("EVAL_ONLY").unwrap_or_default();
    let cases: Vec<&Case> = CASES.iter().filter(|c| c.name.contains(only.as_str())).collect();
    println!("model {} — {} cases × {runs} run(s)", cfg.polish_model, cases.len());

    let mut summary = Vec::new();
    for &mode in &modes {
        // 4 个一组并发请求
        let jobs: Vec<(usize, &Case)> = (0..runs).flat_map(|_| cases.iter().copied().enumerate()).collect();
        let mut results: Vec<(usize, Outcome)> = Vec::new();
        for chunk in jobs.chunks(4) {
            let done: Vec<(usize, Outcome)> = std::thread::scope(|s| {
                let cfg = &cfg;
                let handles: Vec<_> = chunk.iter().map(|&(i, c)| s.spawn(move || (i, run(c, mode, cfg)))).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            results.extend(done);
        }
        println!("\n=== {mode:?} ===");
        let (mut passed, mut missed, mut extra, mut expected) = (0, 0, 0, 0);
        for (i, o) in &results {
            let c = cases[*i];
            expected += c.expect.len();
            missed += o.missed.len();
            extra += o.extra.len();
            if o.missed.is_empty() && o.extra.is_empty() {
                passed += 1;
            } else {
                println!("FAIL {}: learned {:?}; missed {:?}; wrong {:?}", c.name, o.learned, o.missed, o.extra);
            }
        }
        let total = results.len();
        let line = format!(
            "{mode:?}: {passed}/{total} cases right ({:.0}%), found {}/{expected} corrections, {extra} wrong entries",
            100.0 * passed as f32 / total as f32,
            expected - missed
        );
        println!("{line}");
        summary.push((mode, passed as f32 / total as f32, extra, line));
    }
    println!("\n=== summary ===");
    for (_, _, _, line) in &summary {
        println!("{line}");
    }
    // 交给 LLM 的两种方式：至少 90% 的例子完全对，学错的词条不超过例子数的 5%
    for (mode, rate, extra, line) in summary {
        if mode != Mode::Mechanical {
            assert!(rate >= 0.9 && extra as f32 <= 0.05 * (cases.len() * runs) as f32, "{line}");
        }
    }
}
