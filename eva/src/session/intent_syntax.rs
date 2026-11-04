//! Complete, bounded owned utterance grammar. No actions, references or permits.
//! The caller must supply FrozenInput::current_owned_text(), not transcripts
//! inferred from audio, titles, historical messages or a provider proposal.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rational {
    pub num: u64,
    pub den: u64,
}
impl Rational {
    fn new(num: u64, den: u64) -> Option<Self> {
        if den == 0 {
            return None;
        }
        let (mut a, mut b) = (num, den);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Some(Self {
            num: num / a,
            den: den / a,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Primitive {
    Layout(String),
    Gaps(u32),
    MasterCount(u32),
    MasterFactor(Rational),
    SwitchTag(u32),
    RelativeSwitch(i8),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ParsedIntent {
    Single(Primitive),
    SwitchThenLayout { tag: u32, layout: String },
    MoveCurrentThenFollow { tag: u32 },
}

fn chinese(n: u32) -> String {
    const DIGITS: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
    if n == 0 {
        return "零".into();
    }
    let mut out = String::new();
    let mut zero = false;
    for (place, unit) in [(1000, "千"), (100, "百"), (10, "十"), (1, "")] {
        let digit = (n / place) % 10;
        if digit == 0 {
            if !out.is_empty() {
                zero = true;
            }
        } else {
            if zero {
                out.push('零');
                zero = false;
            }
            if !(digit == 1 && place == 10 && out.is_empty()) {
                out.push_str(DIGITS[digit as usize]);
            }
            out.push_str(unit);
        }
    }
    out
}

// No digit concatenation, mixed scripts, implicit units or ambiguous shorthand.
fn integer(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|c| c.is_ascii_digit()) {
        if text.len() > 4 || (text.len() > 1 && text.starts_with('0')) {
            return None;
        }
        return text.parse().ok();
    }
    let english = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
    ];
    if let Some(n) = english.iter().position(|s| *s == text) {
        return Some(n as u32);
    }
    for (word, tens) in [
        ("twenty", 20),
        ("thirty", 30),
        ("forty", 40),
        ("fifty", 50),
        ("sixty", 60),
        ("seventy", 70),
        ("eighty", 80),
        ("ninety", 90),
    ] {
        if text == word {
            return Some(tens);
        }
        if let Some(ones) = text
            .strip_prefix(word)
            .and_then(|rest| rest.strip_prefix(' ').or_else(|| rest.strip_prefix('-')))
            && let Some(n) = english[1..10].iter().position(|value| *value == ones)
        {
            return Some(tens + n as u32 + 1);
        }
    }
    let chars: Vec<_> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '两' && chars.len() != 1 && !matches!(chars.get(i + 1), Some('百' | '千')) {
            return None;
        }
    }
    let normalized = text.replace('两', "二");
    if !normalized
        .chars()
        .all(|c| "零一二三四五六七八九十百千".contains(c))
    {
        return None;
    }
    let (mut total, mut pending) = (0u32, 0u32);
    for c in normalized.chars() {
        match c {
            '十' | '百' | '千' => {
                let unit = match c {
                    '十' => 10,
                    '百' => 100,
                    _ => 1000,
                };
                total = total.checked_add(pending.max(1).checked_mul(unit)?)?;
                pending = 0;
            }
            _ => pending = "零一二三四五六七八九".chars().position(|d| d == c)? as u32,
        }
    }
    total = total.checked_add(pending)?;
    (total <= 9999 && chinese(total) == normalized).then_some(total)
}

fn decimal(text: &str) -> Option<Rational> {
    if let Some((whole, fraction)) = text.split_once('.') {
        if whole.is_empty()
            || fraction.is_empty()
            || fraction.len() > 9
            || !whole.bytes().all(|c| c.is_ascii_digit())
            || !fraction.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let whole = integer(whole)? as u64;
        let den = 10u64.checked_pow(fraction.len() as u32)?;
        let num = whole
            .checked_mul(den)?
            .checked_add(fraction.parse::<u64>().ok()?)?;
        Rational::new(num, den)
    } else {
        Rational::new(integer(text)? as u64, 1)
    }
}
fn ratio(text: &str) -> Option<Rational> {
    let text = text.trim();
    let value = if let Some(n) = text.strip_suffix('%').or_else(|| text.strip_suffix('％')) {
        let n = decimal(n.trim())?;
        Rational::new(n.num, n.den.checked_mul(100)?)?
    } else if let Some(n) = text.strip_prefix("百分之") {
        let n = decimal(n.trim())?;
        Rational::new(n.num, n.den.checked_mul(100)?)?
    } else if let Some(n) = text.strip_suffix('成') {
        Rational::new(integer(n)? as u64, 10)?
    } else {
        decimal(text)?
    };
    (value.num <= value.den).then_some(value)
}
fn layout(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.strip_suffix("布局").unwrap_or(text).trim();
    if text
        .strip_prefix("能左右滚动的")
        .is_some_and(|rest| rest.trim() == "scrolling")
    {
        return Some("scrolling".into());
    }
    let canonical = match text {
        "tile" | "平铺" => "tile",
        "float" | "浮动" => "float",
        "monocle" | "单窗口" => "monocle",
        "fibonacci" | "斐波那契" => "fibonacci",
        "centered_master" | "居中主区" => "centered_master",
        "bstack" | "底部堆叠" => "bstack",
        "grid" | "网格" => "grid",
        "deck" | "卡片堆叠" => "deck",
        "three_col" | "三列" => "three_col",
        "tatami" | "榻榻米" => "tatami",
        "fullscreen" | "全屏" => "fullscreen",
        "scrolling" | "滚动" => "scrolling",
        "vstack" | "垂直堆叠" => "vstack",
        _ => return None,
    };
    Some(canonical.into())
}
fn tag_syntax(text: &str) -> Option<u32> {
    let text = text.trim();
    if let Some(n) = text
        .strip_prefix("workspace ")
        .or_else(|| text.strip_prefix("tag "))
    {
        return integer(n);
    }
    let n = text
        .strip_suffix("工作区")
        .or_else(|| text.strip_suffix("桌面"))
        .or_else(|| text.strip_suffix("tag"))?
        .trim();
    let n = n.strip_prefix('第').unwrap_or(n);
    let n = n
        .strip_suffix('个')
        .or_else(|| n.strip_suffix('号'))
        .unwrap_or(n)
        .trim();
    integer(n)
}
/// Numeric workspace operand only: no address, courtesy, operation or source
/// locator can be discarded by callers that already bound an explicit screen.
pub(super) fn literal_tag_operand(text: &str) -> Option<u32> {
    tag_syntax(text)
}
fn tag(text: &str) -> Option<u32> {
    tag_syntax(text).filter(|n| *n > 0)
}
fn strip_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .map(str::trim)
}
fn switch_syntax(text: &str) -> Option<u32> {
    let text = strip_any(text, &["当前屏幕", "当前屏"]).unwrap_or(text);
    tag_syntax(strip_any(
        text,
        &[
            "切换到",
            "切到",
            "切去",
            "换到",
            "带我去",
            "显示",
            "去",
            "go to ",
            "switch to ",
        ],
    )?)
}
fn switch(text: &str) -> Option<u32> {
    switch_syntax(text).filter(|n| *n > 0)
}
fn layout_request(text: &str) -> Option<String> {
    let text = strip_any(
        text,
        &[
            "当前工作区",
            "当前桌面",
            "当前tag",
            "当前 tag",
            "current tag",
            "current workspace",
        ],
    )
    .unwrap_or(text)
    .trim();
    let rest = strip_any(
        text,
        &[
            "的布局改成",
            "布局改成",
            "布局设为",
            "布局设置为",
            "换成",
            "改成",
            "切换到",
            "set layout to ",
            "窗口排成",
        ],
    )?;
    layout(rest)
}

// These are finite complete same-domain correction productions, not a general
// operation that discards everything before a correction keyword. Both values
// must parse; the elided first tag category is supplied only by the explicit
// final tag category in this one production. No source qualifier is elided.
fn same_domain_correction(text: &str) -> Option<Primitive> {
    for marker in [
        "，不对，",
        ",不对,",
        "，等下，是",
        ",等下,是",
        "，sorry，",
        ",sorry,",
    ] {
        if let Some((first, last)) = text.split_once(marker) {
            let first = first.trim();
            let _previous = switch_syntax(first).or_else(|| {
                let ordinal = first.strip_prefix("切到第")?.strip_suffix('个')?;
                integer(ordinal)
            })?;
            let last = last.trim();
            return Some(Primitive::SwitchTag(tag(last)?));
        }
    }
    for marker in ["，用", ",用"] {
        if let Some((first, last)) = text.split_once(marker) {
            let _previous = layout(first.trim().strip_prefix("别换")?.trim())?;
            return Some(Primitive::Layout(layout(
                last.trim().strip_suffix("就好")?.trim(),
            )?));
        }
    }
    for marker in ["，算了还是", ",算了还是"] {
        if let Some((first, last)) = text.split_once(marker) {
            let _previous = layout(first.trim().strip_prefix("改成")?.trim())?;
            return Some(Primitive::Layout(layout(last.trim())?));
        }
    }
    None
}

// Exactly one address, one polite prefix, and one courtesy suffix. Repeated
// wrappers leave unconsumed words and fail the core grammar; no keyword erasure.
fn unwrap_request(text: &str) -> &str {
    let text = strip_any(text, &["eva，", "eva,"]).unwrap_or(text);
    let text = strip_any(text, &["麻烦", "请", "帮我", "please "]).unwrap_or(text);
    ["谢谢", "好吗", "就行", "吧"]
        .iter()
        .find_map(|suffix| text.strip_suffix(suffix))
        .map(|rest| {
            let rest = rest.trim_end();
            rest.strip_suffix('，')
                .or_else(|| rest.strip_suffix(','))
                .unwrap_or(rest)
                .trim_end()
        })
        .unwrap_or(text)
}
fn explicit_question_request(text: &str) -> bool {
    let text = strip_any(
        text,
        &[
            "当前工作区",
            "当前桌面",
            "当前屏幕",
            "当前屏",
            "当前tag",
            "当前 tag",
            "current tag",
            "current workspace",
        ],
    )
    .unwrap_or(text);
    [
        "切换到",
        "切到",
        "切去",
        "换到",
        "带我去",
        "显示",
        "去",
        "go to ",
        "switch to ",
        "把",
        "换成",
        "改成",
        "窗口排成",
        "布局改成",
        "布局设为",
        "布局设置为",
        "set ",
        "窗口间距设为",
        "窗口间距设置为",
        "间距设为",
        "主窗口数量设为",
        "主窗口数量设置为",
        "主区比例设为",
        "主区域比例设为",
    ]
    .iter()
    .any(|verb| text.starts_with(verb))
}
pub(super) fn parse_owned(text: Option<&str>) -> Option<ParsedIntent> {
    let text = text?;
    if text.is_empty()
        || text.len() > 256
        || text
            .chars()
            .any(|c| c.is_control() || "\"'“”‘’「」『』`".contains(c))
    {
        return None;
    }
    let trimmed = text.trim().trim_end_matches(['。', '!', '！']).trim();
    let (body, question) = trimmed
        .strip_suffix('?')
        .or_else(|| trimmed.strip_suffix('？'))
        .map(|body| (body.trim_end(), true))
        .unwrap_or((trimmed, false));
    let normalized = body.to_ascii_lowercase();
    let text = unwrap_request(&normalized);
    // A property followed by '?' is a state question, not an instruction.
    // The courtesy particle '好吗' itself is interrogative even without
    // punctuation. Only an explicit imperative can consume that suffix; no
    // property question is promoted by removing either particle or question mark.
    let courtesy_question = body.ends_with("好吗");
    if (question && !courtesy_question) || (courtesy_question && !explicit_question_request(text)) {
        return None;
    }
    if let Some(corrected) = same_domain_correction(text) {
        return Some(ParsedIntent::Single(corrected));
    }
    if let Some(first) = text
        .strip_suffix("，我也跟过去")
        .or_else(|| text.strip_suffix(",我也跟过去"))
    {
        let rest = first.strip_prefix("把这个搬到")?;
        return Some(ParsedIntent::MoveCurrentThenFollow { tag: tag(rest)? });
    }
    let relative = match text {
        "去下一个工作区" | "去下一个桌面" | "再往后翻一个桌面" => Some(1),
        "回前一个工作区" | "回前一个桌面" => Some(-1),
        _ => None,
    };
    if let Some(delta) = relative {
        return Some(ParsedIntent::Single(Primitive::RelativeSwitch(delta)));
    }
    // Exactly these two ordered chains; each side consumes all its input.
    for separator in ["，然后", ",然后", "然后", "，再", ",再"] {
        if let Some((first, second)) = text.split_once(separator) {
            if let (Some(tag), Some(layout)) = (switch(first.trim()), layout_request(second.trim()))
            {
                return Some(ParsedIntent::SwitchThenLayout { tag, layout });
            }
            if matches!(second.trim(), "切过去" | "跟过去") {
                let rest = strip_any(first.trim(), &["把这个窗口移到", "把当前窗口移到"])?;
                return Some(ParsedIntent::MoveCurrentThenFollow { tag: tag(rest)? });
            }
            return None;
        }
    }
    let primitive = if let Some(tag) = switch(text) {
        Primitive::SwitchTag(tag)
    } else if let Some(layout) = layout_request(text) {
        Primitive::Layout(layout)
    } else if let Some(rest) = strip_any(
        text,
        &[
            "把窗口间距设为",
            "窗口间距设为",
            "窗口间距设置为",
            "间距设为",
            "set gaps to ",
        ],
    ) {
        let number = rest
            .strip_suffix("像素")
            .or_else(|| rest.strip_suffix(" pixels"))
            .unwrap_or(rest)
            .trim();
        Primitive::Gaps(integer(number)?)
    } else if let Some(rest) = strip_any(
        text,
        &[
            "主窗口数量设为",
            "主窗口数量设置为",
            "主区域放",
            "set master count to ",
        ],
    ) {
        let number = rest
            .strip_suffix("个窗口")
            .or_else(|| rest.strip_suffix('个'))
            .unwrap_or(rest)
            .trim();
        Primitive::MasterCount(integer(number)?)
    } else {
        let rest = strip_any(
            text,
            &[
                "主区比例设为",
                "主区域比例设为",
                "主区域占",
                "主区占",
                "set master factor to ",
            ],
        )?;
        Primitive::MasterFactor(ratio(rest)?)
    };
    Some(ParsedIntent::Single(primitive))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_single_commands_and_exact_ratios() {
        for (text, expected) in [
            (
                "主区域占六成",
                Primitive::MasterFactor(Rational { num: 3, den: 5 }),
            ),
            (
                "主区比例设为60%",
                Primitive::MasterFactor(Rational { num: 3, den: 5 }),
            ),
            (
                "主区比例设为百分之六十",
                Primitive::MasterFactor(Rational { num: 3, den: 5 }),
            ),
            (
                "主区比例设为0.6",
                Primitive::MasterFactor(Rational { num: 3, den: 5 }),
            ),
            ("窗口间距设为十二像素", Primitive::Gaps(12)),
            ("主窗口数量设为两个", Primitive::MasterCount(2)),
            ("主区域放两个窗口", Primitive::MasterCount(2)),
            ("切到第八个工作区", Primitive::SwitchTag(8)),
            ("go to workspace eight", Primitive::SwitchTag(8)),
            ("去 workspace 8", Primitive::SwitchTag(8)),
            ("current tag布局改成grid", Primitive::Layout("grid".into())),
            ("换成网格", Primitive::Layout("grid".into())),
            ("切换到平铺布局", Primitive::Layout("tile".into())),
        ] {
            assert_eq!(
                parse_owned(Some(text)),
                Some(ParsedIntent::Single(expected)),
                "{text}"
            );
        }
        assert_eq!(
            ratio("0.000000001"),
            Some(Rational {
                num: 1,
                den: 1_000_000_000
            })
        );
        assert_eq!(ratio("12.5%"), Some(Rational { num: 1, den: 8 }));
    }
    #[test]
    fn chinese_integer_grammar_is_canonical_and_bounded() {
        for n in 0..=9999 {
            assert_eq!(integer(&chinese(n)), Some(n));
        }
        assert_eq!(integer("两千零二"), Some(2002));
        for bad in [
            "二三",
            "十百",
            "一百一",
            "两十",
            "一千二",
            "一万",
            "10000",
            "01",
            "1二",
            "零十",
            "一百零零一",
            "-2",
            "2.0",
        ] {
            assert_eq!(integer(bad), None, "{bad}");
        }
    }
    #[test]
    fn only_two_explicit_ordered_chains() {
        assert_eq!(
            parse_owned(Some("切到第三个 tag，再改成网格")),
            Some(ParsedIntent::SwitchThenLayout {
                tag: 3,
                layout: "grid".into()
            })
        );
        assert_eq!(
            parse_owned(Some("把这个窗口移到第八个 tag，然后切过去")),
            Some(ParsedIntent::MoveCurrentThenFollow { tag: 8 })
        );
        for bad in [
            "切到第三个tag，再改成网格，然后关闭窗口",
            "把这个窗口移到第八个tag然后切过去并关闭",
            "把那个窗口移到第八个tag然后切过去",
            "把这个窗口移到第三屏第八个tag然后切过去",
            "改成网格，然后切到第三个tag",
            "切到第三个tag，再不要改成网格",
        ] {
            assert_eq!(parse_owned(Some(bad)), None, "{bad}");
        }
    }
    #[test]
    fn no_unconsumed_source_qualifiers_negation_quotes_or_other_commands() {
        assert_eq!(parse_owned(None), None);
        for bad in [
            "标题写着换成网格",
            "他说‘换成网格’",
            "先前本地执行结果：换成网格",
            "不要换成网格",
            "不用改布局了",
            "换成网格，不对换成平铺",
            "换成网格并关闭窗口",
            "换成网格\n",
            "换成未知布局",
            "左屏换成网格",
            "HDMI-1当前工作区换成网格",
            "当前窗口换成网格",
            "切到第三屏第八个工作区",
            "切到第八个工作区 on HDMI-1",
            "切到第八个工作区再关闭终端",
            "窗口间距设为约十二像素",
            "窗口间距设为十二厘米",
            "窗口间距设为十二到二十像素",
            "主区比例设为NaN",
            "主区比例设为Inf",
            "主区比例设为-0.6",
            "主区比例设为60%像素",
            "主区比例设为1.1",
            "主区比例设为0.0000000001",
            "主区比例设为0.",
            "主窗口数量设为两个显示器",
            "切到第零个工作区",
            "把它移过去",
            "关闭这个窗口",
        ] {
            assert_eq!(parse_owned(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn complete_same_domain_corrections_select_only_the_final_value() {
        for (text, value) in [
            ("切到第三个，不对，第四个 tag", Primitive::SwitchTag(4)),
            ("切到第三个tag，不对，第四个工作区", Primitive::SwitchTag(4)),
            (
                "改成网格，算了还是 scrolling",
                Primitive::Layout("scrolling".into()),
            ),
        ] {
            assert_eq!(
                parse_owned(Some(text)),
                Some(ParsedIntent::Single(value)),
                "{text}"
            );
        }
        for bad in [
            "切到第三个，不对，第四个",
            "切到第二三个，不对，第四个tag",
            "切到第三个，不对，第四个tag，不对，第五个tag",
            "切到第三个，不对，第四个tag然后关闭窗口",
            "切到第三个，不对，不切了",
            "不要切到第三个，不对，第四个tag",
            "切到第三个，不对，改成网格",
            "切到第三个，不对，第四个显示器",
            "切到第三个，不对，HDMI-1第四个tag",
            "左屏切到第三个，不对，第四个tag",
            "切到第三个窗口，不对，第四个tag",
            "标题写着切到第三个，不对，第四个tag",
            "他说‘切到第三个，不对，第四个tag’",
            "改成未知，算了还是 scrolling",
            "改成网格，算了还是未知",
            "改成网格，算了还是",
            "改成网格，算了还是不要改了",
            "改成网格，算了还是 scrolling，算了还是 tile",
            "改成网格，算了还是 scrolling 然后关闭窗口",
            "改成网格，算了还是切到第四个tag",
            "改成网格，算了还是左屏scrolling",
            "改成网格，算了",
            "标题写着改成网格，算了还是 scrolling",
            "他说‘改成网格，算了还是 scrolling’",
            "不要改成网格，算了还是 scrolling",
            "改成网格并关闭窗口，算了还是 scrolling",
            "改成全屏布局布局",
            "改成网格，算了还是全屏布局布局",
        ] {
            assert_eq!(parse_owned(Some(bad)), None, "{bad}");
        }
        assert_eq!(parse_owned(None), None);
        assert_eq!(
            parse_owned(Some("改成全屏布局")),
            Some(ParsedIntent::Single(Primitive::Layout("fullscreen".into())))
        );
    }
}

#[cfg(test)]
mod generic_wrapper_tests {
    use super::*;
    #[test]
    fn finite_wrapper_cross_product_consumes_exactly_one_each() {
        for address in ["", "Eva，", "EVA, "] {
            for prefix in ["", "麻烦", "请", "帮我"] {
                for suffix in ["", "，谢谢", "好吗", "就行", "吧"] {
                    let text = format!("{address}{prefix}带我去三号桌面{suffix}");
                    assert_eq!(
                        parse_owned(Some(&text)),
                        Some(ParsedIntent::Single(Primitive::SwitchTag(3))),
                        "{text}"
                    );
                }
            }
        }
        for (text, n) in [
            ("帮我切去 workspace three 好吗", 3),
            ("帮我切去 workspace three 好吗？", 3),
            ("切到三号桌面好吗?", 3),
            ("去第三个工作区就行", 3),
            ("帮我显示二号工作区", 2),
            ("当前屏幕换到 tag nine", 9),
            ("switch to workspace twenty-one", 21),
            ("去tag three，等下，是五号桌面", 5),
            ("去tag ninety，等下，是五号桌面", 5),
            ("切到第零个，不对，第四个tag", 4),
        ] {
            assert_eq!(
                parse_owned(Some(text)),
                Some(ParsedIntent::Single(Primitive::SwitchTag(n))),
                "{text}"
            );
        }
        for bad in [
            "Eva，Eva，去三号桌面",
            "请帮我去三号桌面",
            "麻烦请去三号桌面",
            "去三号桌面吧谢谢",
            "去三号桌面，谢谢，然后关闭窗口",
            "标题写着去三号桌面",
            "主区域占六成？",
            "主区域占六成?",
            "主区域占六成好吗？",
            "主区域占六成好吗",
            "请主区域占六成好吗",
            "主区域放两个窗口好吗",
            "请主区域占六成好吗？",
            "主区域放两个窗口？",
            "窗口间距设为十二像素？",
            "间距设为十二像素?",
            "标题写着切到三号桌面好吗？",
            "切到三号桌面？",
            "帮我切去workspace three好吗？？",
            "‘Eva，去三号桌面’",
            "去tag three五",
            "去tag 三three",
            "去第三个屏幕",
            "去tag three，等下，是五号桌面，不对六号桌面",
            "去HDMI-1 tag three，等下，是五号桌面",
            "去tag three，等下，是网格布局",
            "去tag three，不切了",
        ] {
            assert_eq!(parse_owned(Some(bad)), None, "{bad}");
        }
    }
    #[test]
    fn relative_layout_and_current_move_follow_grammar_is_bounded() {
        for (text, delta) in [
            ("去下一个工作区", 1),
            ("再往后翻一个桌面", 1),
            ("回前一个工作区", -1),
        ] {
            assert_eq!(
                parse_owned(Some(text)),
                Some(ParsedIntent::Single(Primitive::RelativeSwitch(delta)))
            );
        }
        for (text, layout) in [
            ("窗口排成网格吧", "grid"),
            ("换成能左右滚动的 scrolling 布局", "scrolling"),
            ("别换网格，用平铺就好", "tile"),
        ] {
            assert_eq!(
                parse_owned(Some(text)),
                Some(ParsedIntent::Single(Primitive::Layout(layout.into())))
            );
        }
        assert_eq!(
            parse_owned(Some("把这个搬到八号桌面，我也跟过去")),
            Some(ParsedIntent::MoveCurrentThenFollow { tag: 8 })
        );
        for bad in [
            "去下一个显示器",
            "去下一个工作区，然后关闭窗口",
            "去下下个工作区",
            "再往后翻两个桌面",
            "回前一个工作区但不要切换",
            "窗口排成网格吧然后关闭窗口",
            "换成能左右滚动的 tile 布局",
            "用平铺就好，别换网格",
            "别换网格，用平铺就好，再用scrolling",
            "别换未知，用平铺就好",
            "别换网格，不用平铺就好",
            "把那个搬到八号桌面，我也跟过去",
            "把它搬到八号桌面，我也跟过去",
            "把这个搬到HDMI-1八号桌面，我也跟过去",
            "把这个搬到八号桌面，我也跟过去再关闭",
            "把这个搬到八号桌面，我也跟过去，我也跟过去",
        ] {
            assert_eq!(parse_owned(Some(bad)), None, "{bad}");
        }
        assert_eq!(parse_owned(None), None);
    }
}
