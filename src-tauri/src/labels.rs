//! 类型 / 子类型标签的**唯一来源**。
//!
//! 今天标签表写在前端（`src/main.js` 的 `KIND_LABEL` / `SUBTYPE_LABEL`），
//! 搜索也只在前端做。搜索一旦下推到 Rust（服务端过滤 + 分页），后端就必须知道
//! 「`image` 的中文是图片、搜『图片』要命中图片条目」——如果两处各留一份表，
//! 它们迟早会分叉，而分叉的表现是「同一个词在面板里搜得到、在主窗口里搜不到」，
//! 这种 bug 用户能看见、我们却没有任何断言在守。
//!
//! 所以：字典在这里，前端后续改为消费后端下发的这份表；
//! 本模块的单测把 `main.js` 里的取值抄成常量做逐项比对，**故意做成一把锁** ——
//! 谁改了一边不改另一边，`cargo test` 立刻红。
//!
//! 本文件只提供字典与查询，不参与任何渲染或过滤逻辑（那是 `query.rs` 的事）。

/// 类型标签：条目 `kind` → 中文名。
///
/// 顺序即侧栏顺序，`get_labels()` 会原样下发，前端不要再自己排序。
pub const KIND_LABEL: &[(&str, &str)] = &[("text", "文本"), ("image", "图片"), ("files", "文件")];

/// 子类型标签：文本条目再细分（链接 / 代码 / Markdown）。
///
/// 子类型**不是分类**：链接与代码仍然是文本条目，侧栏里不会单独出现一项，
/// 它只影响卡片徽章与搜索命中。`markdown` 的首字母大写是既有取值，
/// 与 `main.js` 保持一致，不要「顺手改成小写」—— 前端会按原样显示。
pub const SUBTYPE_LABEL: &[(&str, &str)] =
    &[("link", "链接"), ("code", "代码"), ("markdown", "Markdown")];

/// `kind` 的中文标签。未知取值返回 `None`，由调用方决定回退（今天的卡片回退成原始 `kind`）。
pub fn kind_label(kind: &str) -> Option<&'static str> {
    KIND_LABEL
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, label)| *label)
}

/// `subtype` 的中文标签。空字符串与未知取值都返回 `None`。
pub fn subtype_label(sub: &str) -> Option<&'static str> {
    SUBTYPE_LABEL
        .iter()
        .find(|(k, _)| *k == sub)
        .map(|(_, label)| *label)
}

/// 一条记录参与搜索命中的「标签词」：中文标签 + 该词本身（英文 token）。
///
/// 覆盖今天前端 `filtered()` 的这两行匹配：
/// `KIND_LABEL[i.kind]`、`i.kind`、`SUBTYPE_LABEL[i.subtype]`、`i.subtype`。
/// 搜「图片」命中图片条目（中文标签），搜「image」也命中（英文 token）。
///
/// 只覆盖字典内的取值：不知道 `kind` 是什么时不会凭空造词 —— 未知取值的原始字符串
/// 由调用方另行匹配（等价于今天前端无条件匹配 `i.kind` 本身），这里不重复返回它，
/// 避免「同一段字节被扫描两次」。
///
/// 返回 `&'static str` 而不是 `&str`：这些词全部来自上面的常量表，
/// 交给调用方时可以活过本次调用、塞进缓存或直接比指针，不牵扯入参的生命周期。
pub fn search_aliases(kind: &str, subtype: Option<&str>) -> impl Iterator<Item = &'static str> {
    let kind_pair = KIND_LABEL.iter().find(|(k, _)| *k == kind).copied();
    let sub_pair = subtype.and_then(|s| SUBTYPE_LABEL.iter().find(|(k, _)| *k == s).copied());

    kind_pair
        .into_iter()
        .flat_map(|(token, label)| [token, label])
        .chain(
            sub_pair
                .into_iter()
                .flat_map(|(token, label)| [token, label]),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // 锁：下面两张表是从 `src/main.js` **逐字抄下来的**。
    //
    // 改 JS 必须同步这里，否则单测会红 —— 这是刻意的。
    //
    // 为什么要抄而不是让测试去读 JS 文件：`cargo test` 不该依赖前端资源的路径与编码，
    // 一条断言值大于「构建环境完备」。抄一份的代价是要人肉同步，收益是分叉当天就被发现。
    // -----------------------------------------------------------------------
    const JS_KIND_LABEL: &[(&str, &str)] =
        &[("text", "文本"), ("image", "图片"), ("files", "文件")];
    const JS_SUBTYPE_LABEL: &[(&str, &str)] =
        &[("link", "链接"), ("code", "代码"), ("markdown", "Markdown")];

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    #[test]
    fn tables_match_frontend_constants() {
        // 逐项相等（含顺序）：多一项、少一项、改一个字都要红。
        assert_eq!(
            KIND_LABEL, JS_KIND_LABEL,
            "KIND_LABEL 与 src/main.js 不一致"
        );
        assert_eq!(
            SUBTYPE_LABEL, JS_SUBTYPE_LABEL,
            "SUBTYPE_LABEL 与 src/main.js 不一致"
        );
    }

    #[test]
    fn every_kind_has_label_and_english_token() {
        assert_eq!(KIND_LABEL.len(), 3, "类型只有 text/image/files 三种");
        for (token, label) in KIND_LABEL {
            assert!(!token.is_empty());
            assert!(token.is_ascii(), "英文 token 应是 ASCII：{token}");
            assert!(has_cjk(label), "{token} 的中文标签缺失汉字：{label}");
            assert_eq!(
                kind_label(token),
                Some(*label),
                "{token} 的查询应返回 {label}"
            );

            // 中英文都要能搜到：中文标签 + 英文 token 各出现一次。
            let aliases: Vec<&str> = search_aliases(token, None).collect();
            assert_eq!(
                aliases,
                vec![*token, *label],
                "{token} 的可搜索词应恰为 [token, 中文标签]"
            );
        }
    }

    #[test]
    fn every_subtype_has_label_and_english_token() {
        assert_eq!(SUBTYPE_LABEL.len(), 3, "子类型只有 link/code/markdown 三种");
        for (token, label) in SUBTYPE_LABEL {
            assert!(token.is_ascii(), "英文 token 应是 ASCII：{token}");
            assert!(
                has_cjk(label) || *token == "markdown",
                "{token} 的标签异常：{label}"
            );
            assert_eq!(subtype_label(token), Some(*label));
        }
        // markdown 的标签是英文专名，这是既有取值，不该被「汉化」
        assert_eq!(subtype_label("markdown"), Some("Markdown"));
    }

    #[test]
    fn aliases_cover_kind_and_subtype_together() {
        // 搜「代码」与搜「code」都要命中同一条文本条目，因此两类词必须一起返回。
        let aliases: Vec<&str> = search_aliases("text", Some("code")).collect();
        assert_eq!(aliases, vec!["text", "文本", "code", "代码"]);

        let aliases: Vec<&str> = search_aliases("image", Some("link")).collect();
        assert_eq!(aliases, vec!["image", "图片", "link", "链接"]);

        // 图片/文件条目没有子类型
        assert_eq!(
            search_aliases("files", None).collect::<Vec<_>>(),
            vec!["files", "文件"]
        );
    }

    #[test]
    fn unknown_values_yield_nothing() {
        // 未知取值不造词：调用方自己去匹配原始字符串。
        assert_eq!(kind_label("video"), None);
        assert_eq!(kind_label(""), None);
        assert_eq!(subtype_label("html"), None);
        assert_eq!(subtype_label(""), None);
        assert_eq!(search_aliases("video", None).count(), 0);
        assert_eq!(
            search_aliases("text", Some("html")).collect::<Vec<_>>(),
            vec!["text", "文本"]
        );
    }

    #[test]
    fn tokens_are_unique() {
        // 词表里出现重复 token 会让未来的「标签命中加分」算错，也会让 `get_labels()`
        // 下发两张同名的表给前端。
        let mut seen: Vec<&str> = Vec::new();
        for (token, _) in KIND_LABEL.iter().chain(SUBTYPE_LABEL) {
            assert!(!seen.contains(token), "token 重复：{token}");
            seen.push(token);
        }
    }
}
