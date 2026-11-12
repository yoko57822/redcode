use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Action, App, AppContext as _, Entity, ParentElement as _, SharedString, Styled as _, Window,
};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{WindowExt as _, h_flex};

const MAX_RESULTS: usize = 200;

pub struct PickerItem {
    pub label: SharedString,
    /// Its key binding, as it applies where focus was, is shown as the row's hint.
    pub action: Option<Box<dyn Action>>,
}

struct PickerState {
    labels: Vec<SharedString>,
    hints: Vec<Option<Kbd>>,
    /// Indices into `items`, in display order.
    filtered: Vec<usize>,
    command: Entity<CommandState>,
}

/// Indices of the labels matching `query` as a case-insensitive subsequence, best first:
/// contiguous substring matches, then earlier first match, then shorter labels.
pub fn fuzzy_filter(query: &str, labels: &[SharedString]) -> Vec<usize> {
    if query.is_empty() {
        return (0..labels.len().min(MAX_RESULTS)).collect();
    }
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let needle: String = query.iter().collect();
    let mut matches: Vec<(u8, usize, usize, usize)> = Vec::new();
    for (ix, label) in labels.iter().enumerate() {
        let lower = label.to_lowercase();
        let mut wanted = query.iter().peekable();
        let mut first = None;
        for (pos, c) in lower.chars().enumerate() {
            if wanted.peek() == Some(&&c) {
                first.get_or_insert(pos);
                wanted.next();
            }
        }
        if wanted.peek().is_some() {
            continue;
        }
        let (rank, first) = match lower.find(&needle) {
            Some(byte) => (0, lower[..byte].chars().count()),
            None => (1, first.unwrap_or(0)),
        };
        matches.push((rank, first, lower.chars().count(), ix));
    }
    matches.sort();
    matches.truncate(MAX_RESULTS);
    matches.into_iter().map(|(.., ix)| ix).collect()
}

/// Opens a filterable list dialog. `on_pick` receives the index into `items` after the dialog
/// closed and focus returned to where it was.
pub fn open_picker(
    placeholder: &'static str,
    items: Vec<PickerItem>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let prev = window.focused(cx);
    let command = cx.new(|cx| CommandState::new(window, cx));
    let labels: Vec<SharedString> = items.iter().map(|item| item.label.clone()).collect();
    // Resolved against the previously focused element: that is where a picked action goes, and
    // the dialog's own key context would hide context-bound bindings.
    let hints = items
        .iter()
        .map(|item| {
            let action = item.action.as_ref()?;
            Kbd::binding_for_action_in(action.as_ref(), prev.as_ref()?, window)
        })
        .collect();
    let filtered = fuzzy_filter("", &labels);
    let state = cx.new(|_| PickerState {
        labels,
        hints,
        filtered,
        command,
    });
    let on_pick = Rc::new(on_pick);
    let focus_on_mount = Cell::new(true);

    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_on_mount.replace(false) {
            let command = state.read(cx).command.clone();
            command.update(cx, |command, cx| command.focus(window, cx));
        }
        let state = state.clone();
        let prev = prev.clone();
        let on_pick = on_pick.clone();
        dialog
            .close_button(false)
            .p_0()
            .content(move |content, _window, cx| {
                let picker = state.read(cx);
                let mut command = Command::new(&picker.command)
                    .filterable(false)
                    .bordered(false)
                    .placeholder(placeholder)
                    .on_query({
                        let state = state.clone();
                        move |query, window, cx| {
                            state.update(cx, |picker, _| {
                                picker.filtered = fuzzy_filter(query, &picker.labels);
                            });
                            window.refresh();
                        }
                    })
                    .on_confirm({
                        let state = state.clone();
                        let prev = prev.clone();
                        let on_pick = on_pick.clone();
                        move |index, window, cx| {
                            let Some(&item_ix) = state.read(cx).filtered.get(index.row) else {
                                return;
                            };
                            window.close_dialog(cx);
                            if let Some(prev) = &prev {
                                prev.focus(window, cx);
                            }
                            let on_pick = on_pick.clone();
                            window.defer(cx, move |window, cx| on_pick(item_ix, window, cx));
                        }
                    });
                for &ix in &picker.filtered {
                    let label = picker.labels[ix].clone();
                    let mut item = CommandItem::new().label(label.clone());
                    if let Some(hint) = picker.hints[ix].clone() {
                        item = item.child(move |_, _| {
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap_2()
                                .child(label.clone())
                                .child(hint.clone())
                        });
                    }
                    command = command.item(item);
                }
                content.child(command)
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[&'static str]) -> Vec<SharedString> {
        items.iter().map(|item| SharedString::from(*item)).collect()
    }

    #[test]
    fn empty_query_keeps_original_order() {
        let labels = labels(&["b", "a", "c"]);
        assert_eq!(fuzzy_filter("", &labels), vec![0, 1, 2]);
    }

    #[test]
    fn substring_matches_rank_before_subsequence_matches() {
        // "nt" is only a subsequence of the first two labels and a substring of "Print".
        let labels = labels(&["Terminal: New Terminal", "View: Next Tab", "Print"]);
        // Among subsequence matches the earlier first match wins: "termiNal" before "view: Next".
        assert_eq!(fuzzy_filter("nt", &labels), vec![2, 0, 1]);
    }

    #[test]
    fn earlier_and_shorter_matches_win_within_a_rank() {
        let labels = labels(&["View: Split Down", "Split Right", "Terminal: Split Right"]);
        assert_eq!(fuzzy_filter("split", &labels), vec![1, 0, 2]);
    }

    #[test]
    fn non_matching_labels_are_excluded() {
        let labels = labels(&["alpha", "beta"]);
        assert_eq!(fuzzy_filter("xz", &labels), Vec::<usize>::new());
        assert_eq!(fuzzy_filter("BT", &labels), vec![1]);
    }

    #[test]
    fn results_are_capped() {
        let many: Vec<SharedString> = (0..500).map(|i| format!("item {i}").into()).collect();
        assert_eq!(fuzzy_filter("", &many).len(), MAX_RESULTS);
        assert_eq!(fuzzy_filter("item", &many).len(), MAX_RESULTS);
    }
}
