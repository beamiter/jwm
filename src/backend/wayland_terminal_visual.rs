//! Pure nested drawing order. Never mutates native stacking or hit testing.
use crate::backend::common_define::WindowId;

/// Top-to-bottom order, matching Smithay element lists. System windows keep
/// their relative order above the visual copy; other applications stay below.
pub(crate) fn drawing_order(
    stack: &[WindowId],
    terminal: Option<WindowId>,
    protected: impl Fn(WindowId) -> bool,
) -> Vec<WindowId> {
    let Some(terminal) = terminal.filter(|win| stack.contains(win)) else {
        return stack.iter().rev().copied().collect();
    };
    let mut system = Vec::new();
    let mut apps = Vec::new();
    for &win in stack.iter().rev() {
        if win == terminal {
            continue;
        }
        if protected(win) {
            system.push(win);
        } else {
            apps.push(win);
        }
    }
    system.push(terminal);
    system.extend(apps);
    system
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn promotion_is_visual_only_and_below_system_windows() {
        let stack: Vec<_> = (1..=5).map(WindowId::from_raw).collect();
        let before = stack.clone();
        let order = drawing_order(&stack, Some(stack[0]), |w| w == stack[1] || w == stack[3]);
        assert_eq!(
            order,
            vec![stack[3], stack[1], stack[0], stack[4], stack[2]]
        );
        assert_eq!(stack, before);
        assert_eq!(
            drawing_order(&stack, None, |_| false),
            stack.iter().rev().copied().collect::<Vec<_>>()
        );
        assert_eq!(
            drawing_order(&stack, Some(WindowId::from_raw(99)), |_| false),
            stack.iter().rev().copied().collect::<Vec<_>>()
        );
    }
}
