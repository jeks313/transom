//! How an app's menu bar (protocol.md §4 `menuBar`) becomes a Windows menu bar:
//! which items show, their text, and the command each leaf becomes. Portable and
//! unit tested; `win::menu` turns the plan into an `HMENU`.
//!
//! A leaf's path is its indices among its menus' children, as the host read them;
//! the bar's first menu is index 1, because the host leaves out the Apple menu
//! (index 0). Paths survive hidden items, so a pick names the item the host saw.

use crate::wire::MenuItem;

/// One command: what a menu pick sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub path: Vec<u32>,
    pub title: String,
    pub enabled: bool,
    pub checked: bool,
}

/// The items of one menu that show, with their indices. A Mac menu lists an
/// item's Option-key alternate right after it under the same title ("Close
/// Window" then "Close Window" with ⌥); Windows has no alternates, so only the
/// first shows. Separators at either end or next to another are dropped too.
pub fn visible(items: &[MenuItem]) -> Vec<(u32, &MenuItem)> {
    let mut out: Vec<(u32, &MenuItem)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let prev = out.last().map(|(_, p)| *p);
        if item.separator {
            if prev.map(|p| !p.separator).unwrap_or(false) {
                out.push((i as u32, item));
            }
            continue;
        }
        if prev.map(|p| !p.separator && p.title == item.title).unwrap_or(false) {
            continue;
        }
        out.push((i as u32, item));
    }
    while out.last().map(|(_, p)| p.separator).unwrap_or(false) {
        out.pop();
    }
    out
}

/// The text Windows shows: `&` doubled (it would mark a mnemonic), and the
/// shortcut after a tab so it lines up on the right.
pub fn label(item: &MenuItem) -> String {
    let title = item.title.replace('&', "&&");
    if item.shortcut.is_empty() {
        title
    } else {
        format!("{title}\t{}", item.shortcut)
    }
}

/// Every leaf that shows, depth first, in the order `win::menu` gives them
/// command ids (id = index + 1).
pub fn commands(menus: &[MenuItem]) -> Vec<Command> {
    let mut out = Vec::new();
    for (i, menu) in menus.iter().enumerate() {
        collect(&menu.items, &mut vec![i as u32 + 1], &mut out);
    }
    out
}

fn collect(items: &[MenuItem], path: &mut Vec<u32>, out: &mut Vec<Command>) {
    for (i, item) in visible(items) {
        if item.separator {
            continue;
        }
        path.push(i);
        if item.items.is_empty() {
            out.push(Command {
                path: path.clone(),
                title: item.title.clone(),
                enabled: item.enabled,
                checked: item.checked,
            });
        } else {
            collect(&item.items, path, out);
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> MenuItem {
        MenuItem {
            title: title.into(),
            enabled: true,
            checked: false,
            shortcut: String::new(),
            separator: false,
            items: Vec::new(),
        }
    }

    fn sep() -> MenuItem {
        MenuItem {
            separator: true,
            enabled: false,
            ..item("")
        }
    }

    #[test]
    fn alternates_and_stray_separators_are_hidden_but_indices_kept() {
        let items = vec![
            sep(),
            item("Close Window"),
            item("Close Window"),
            sep(),
            sep(),
            item("Print"),
            sep(),
        ];
        let shown: Vec<(u32, &str)> = visible(&items)
            .into_iter()
            .map(|(i, m)| (i, m.title.as_str()))
            .collect();
        assert_eq!(shown, vec![(1, "Close Window"), (3, ""), (5, "Print")]);
    }

    #[test]
    fn label_escapes_ampersands_and_tabs_the_shortcut() {
        let mut m = item("Find & Replace");
        assert_eq!(label(&m), "Find && Replace");
        m.shortcut = "Ctrl+F".into();
        assert_eq!(label(&m), "Find && Replace\tCtrl+F");
    }

    #[test]
    fn command_paths_count_the_apple_menu_and_hidden_items() {
        let mut edit = item("Edit");
        let mut find = item("Find");
        find.items = vec![item("Find…"), item("Find Next")];
        edit.items = vec![item("Undo"), sep(), find];
        let mut file = item("File");
        file.items = vec![item("New"), item("New")];
        let paths: Vec<Vec<u32>> = commands(&[file, edit]).into_iter().map(|c| c.path).collect();
        assert_eq!(
            paths,
            vec![vec![1, 0], vec![2, 0], vec![2, 2, 0], vec![2, 2, 1]]
        );
    }
}
