//! 窗口规则匹配和应用
//!
//! 此模块负责根据窗口的属性（名称、类、实例）匹配和应用配置的规则，
//! 包括标签分配、浮动状态、监视器分配等。

use crate::backend::api::{Backend, WindowType};
use crate::backend::common_define::WindowId;
use crate::config::CONFIG;
use crate::jwm::types::WMRule;
use log::info;

/// 规则匹配和应用工具
pub struct RuleMatcher;

impl RuleMatcher {
    /// 检查规则是否匹配给定的窗口属性
    ///
    /// # 参数
    /// - `rule`: 要匹配的规则
    /// - `name`: 窗口名称
    /// - `class`: 窗口类
    /// - `instance`: 窗口实例
    ///
    /// # 返回
    /// 如果所有非空字段都匹配则返回 true
    ///
    /// # 匹配逻辑
    /// - 空字段视为通配符（匹配任何值）
    /// - 非空字段使用子串匹配（contains）
    /// - 所有非空字段必须同时匹配
    pub fn matches(rule: &WMRule, name: &str, class: &str, instance: &str) -> bool {
        // 如果规则的所有字段都为空，则不匹配任何窗口
        if rule.name.is_empty() && rule.class.is_empty() && rule.instance.is_empty() {
            return false;
        }

        let name_matches = rule.name.is_empty() || name.contains(&rule.name);
        let class_matches = rule.class.is_empty() || class.contains(&rule.class);
        let instance_matches = rule.instance.is_empty() || instance.contains(&rule.instance);

        name_matches && class_matches && instance_matches
    }

    /// 检查窗口是否是弹出式窗口（不应该平铺）
    ///
    /// # 参数
    /// - `backend`: 后端接口
    /// - `win`: 窗口 ID
    ///
    /// # 返回
    /// 如果窗口类型是弹出式则返回 true
    ///
    /// # 弹出式窗口类型
    /// - Dialog, PopupMenu, DropdownMenu
    /// - Tooltip, Notification
    /// - Combo, Dnd, Utility, Splash
    pub fn is_popup_like(backend: &mut dyn Backend, win: WindowId) -> bool {
        let types = backend.property_ops().get_window_types(win);
        for t in types {
            match t {
                WindowType::Dialog
                | WindowType::PopupMenu
                | WindowType::DropdownMenu
                | WindowType::Tooltip
                | WindowType::Notification
                | WindowType::Combo
                | WindowType::Dnd
                | WindowType::Utility
                | WindowType::Splash => {
                    info!("Window {:?} is popup-like type: {:?}", win, t);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// 检查窗口是否应该自动浮动
    ///
    /// # 参数
    /// - `name`, `class`, `instance`: 窗口属性
    ///
    /// # 返回
    /// 如果所有属性都为空（未知窗口）则返回 true
    pub fn should_auto_float(name: &str, class: &str, instance: &str) -> bool {
        name.is_empty() && class.is_empty() && instance.is_empty()
    }

    /// 查找第一个匹配的规则
    ///
    /// # 参数
    /// - `name`, `class`, `instance`: 窗口属性
    ///
    /// # 返回
    /// 第一个匹配的规则，如果没有匹配则返回 None
    pub fn find_matching_rule(name: &str, class: &str, instance: &str) -> Option<WMRule> {
        CONFIG
            .load()
            .get_rules()
            .iter()
            .find(|rule| Self::matches(rule, name, class, instance))
            .cloned()
    }
}

/// 规则应用结果
#[derive(Debug, Clone)]
pub struct RuleApplication {
    /// 是否应用了规则
    pub rule_applied: bool,
    /// 是否设置为浮动
    pub is_floating: bool,
    /// 分配的标签（0 表示未设置）
    pub tags: u32,
    /// 分配的监视器编号（-1 表示未设置）
    pub monitor: i32,
}

impl Default for RuleApplication {
    fn default() -> Self {
        Self {
            rule_applied: false,
            is_floating: false,
            tags: 0,
            monitor: -1,
        }
    }
}

impl RuleApplication {
    /// 从规则创建应用结果
    pub fn from_rule(rule: &WMRule) -> Self {
        Self {
            rule_applied: true,
            is_floating: rule.is_floating,
            tags: rule.tags as u32,
            monitor: rule.monitor,
        }
    }

    /// 创建自动浮动的应用结果（用于未知窗口）
    pub fn auto_float() -> Self {
        Self {
            rule_applied: false,
            is_floating: true,
            tags: 0,
            monitor: -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_rule(name: &str, class: &str, instance: &str) -> WMRule {
        WMRule {
            name: name.to_string(),
            class: class.to_string(),
            instance: instance.to_string(),
            tags: 1,
            is_floating: false,
            monitor: -1,
        }
    }

    #[test]
    fn test_exact_match() {
        let rule = create_test_rule("firefox", "Firefox", "Navigator");
        assert!(RuleMatcher::matches(
            &rule,
            "firefox",
            "Firefox",
            "Navigator"
        ));
    }

    #[test]
    fn test_substring_match() {
        let rule = create_test_rule("fire", "Fire", "Nav");
        assert!(RuleMatcher::matches(
            &rule,
            "firefox",
            "Firefox",
            "Navigator"
        ));
    }

    #[test]
    fn test_wildcard_match() {
        // 空字段作为通配符
        let rule = create_test_rule("", "Firefox", "");
        assert!(RuleMatcher::matches(&rule, "anything", "Firefox", "anything"));

        // 只匹配 class
        assert!(RuleMatcher::matches(
            &rule,
            "window1",
            "Firefox",
            "instance1"
        ));
    }

    #[test]
    fn test_no_match() {
        let rule = create_test_rule("firefox", "Firefox", "Navigator");

        // 名称不匹配
        assert!(!RuleMatcher::matches(
            &rule,
            "chrome",
            "Firefox",
            "Navigator"
        ));

        // 类不匹配
        assert!(!RuleMatcher::matches(
            &rule,
            "firefox",
            "Chrome",
            "Navigator"
        ));

        // 实例不匹配
        assert!(!RuleMatcher::matches(
            &rule,
            "firefox",
            "Firefox",
            "Browser"
        ));
    }

    #[test]
    fn test_empty_rule_no_match() {
        let rule = create_test_rule("", "", "");
        // 完全空的规则不匹配任何窗口
        assert!(!RuleMatcher::matches(&rule, "anything", "anything", "anything"));
    }

    #[test]
    fn test_should_auto_float() {
        // 所有属性为空应该自动浮动
        assert!(RuleMatcher::should_auto_float("", "", ""));

        // 任何非空属性都不应该自动浮动
        assert!(!RuleMatcher::should_auto_float("name", "", ""));
        assert!(!RuleMatcher::should_auto_float("", "class", ""));
        assert!(!RuleMatcher::should_auto_float("", "", "instance"));
    }

    #[test]
    fn test_rule_application_from_rule() {
        let rule = WMRule {
            name: "test".to_string(),
            class: "Test".to_string(),
            instance: "test".to_string(),
            tags: 2,
            is_floating: true,
            monitor: 1,
        };

        let app = RuleApplication::from_rule(&rule);
        assert!(app.rule_applied);
        assert!(app.is_floating);
        assert_eq!(app.tags, 2);
        assert_eq!(app.monitor, 1);
    }

    #[test]
    fn test_rule_application_auto_float() {
        let app = RuleApplication::auto_float();
        assert!(!app.rule_applied);
        assert!(app.is_floating);
        assert_eq!(app.tags, 0);
        assert_eq!(app.monitor, -1);
    }

    #[test]
    fn test_case_sensitive_match() {
        let rule = create_test_rule("Firefox", "Firefox", "Navigator");

        // 大小写敏感
        assert!(RuleMatcher::matches(
            &rule,
            "Firefox",
            "Firefox",
            "Navigator"
        ));
        assert!(!RuleMatcher::matches(
            &rule,
            "firefox",
            "Firefox",
            "Navigator"
        ));
    }

    #[test]
    fn test_partial_match() {
        let rule = create_test_rule("term", "", "");

        // 部分匹配
        assert!(RuleMatcher::matches(&rule, "xterm", "anything", "anything"));
        assert!(RuleMatcher::matches(&rule, "terminal", "anything", "anything"));
        assert!(RuleMatcher::matches(&rule, "terminator", "anything", "anything"));

        // 不匹配
        assert!(!RuleMatcher::matches(&rule, "chrome", "anything", "anything"));
    }
}
