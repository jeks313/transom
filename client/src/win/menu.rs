//! A proxy's native menu bar, built from its app's `menuBar` (protocol.md §4) by
//! the plan in `menu_plan`. Picking a leaf posts `WM_COMMAND` with its command id;
//! `MenuBar::command` turns that back into the path the host presses.

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateMenu, CreatePopupMenu, DestroyMenu, EnableMenuItem, HMENU,
    MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_CHECKED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED,
};

use crate::menu_plan::{self, Command};
use crate::wire::MenuItem;

/// An `HMENU` bar and the command behind each id (id = index + 1).
pub struct MenuBar {
    pub hmenu: HMENU,
    commands: Vec<Command>,
}

impl MenuBar {
    pub fn build(menus: &[MenuItem]) -> windows::core::Result<MenuBar> {
        unsafe {
            let hmenu = CreateMenu()?;
            let mut commands = Vec::new();
            for (i, menu) in menus.iter().enumerate() {
                let popup = CreatePopupMenu()?;
                fill(popup, &menu.items, &mut vec![i as u32 + 1], &mut commands)?;
                let mut flags = MF_POPUP | MF_STRING;
                if !menu.enabled {
                    flags |= MF_GRAYED;
                }
                append(hmenu, flags, popup.0 as usize, &menu_plan::label(menu))?;
            }
            debug_assert_eq!(commands, menu_plan::commands(menus));
            Ok(MenuBar { hmenu, commands })
        }
    }

    /// The command a `WM_COMMAND` id names.
    pub fn command(&self, id: usize) -> Option<&Command> {
        id.checked_sub(1).and_then(|i| self.commands.get(i))
    }

    /// Take `menus`' enabled and checked states in place, which works while a
    /// menu is open. False when the items themselves differ: rebuild instead.
    pub fn update_states(&mut self, menus: &[MenuItem]) -> bool {
        let next = menu_plan::commands(menus);
        let same = next.len() == self.commands.len()
            && next
                .iter()
                .zip(&self.commands)
                .all(|(a, b)| a.path == b.path && a.title == b.title);
        if !same {
            return false;
        }
        for (i, (new, old)) in next.iter().zip(self.commands.iter_mut()).enumerate() {
            let id = i as u32 + 1;
            unsafe {
                if new.enabled != old.enabled {
                    let state = if new.enabled { MF_ENABLED } else { MF_GRAYED };
                    let _ = EnableMenuItem(self.hmenu, id, MF_BYCOMMAND | state);
                }
                if new.checked != old.checked {
                    let state = if new.checked { MF_CHECKED } else { MF_UNCHECKED };
                    let _ = CheckMenuItem(self.hmenu, id, (MF_BYCOMMAND | state).0);
                }
            }
            *old = new.clone();
        }
        true
    }

    /// Free a bar that is not attached to a window (an attached one goes with
    /// its window).
    pub fn destroy(self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

unsafe fn fill(
    hmenu: HMENU,
    items: &[MenuItem],
    path: &mut Vec<u32>,
    commands: &mut Vec<Command>,
) -> windows::core::Result<()> {
    for (i, item) in menu_plan::visible(items) {
        if item.separator {
            AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null())?;
            continue;
        }
        let mut flags = MF_STRING;
        if !item.enabled {
            flags |= MF_GRAYED;
        }
        path.push(i);
        if item.items.is_empty() {
            if item.checked {
                flags |= MF_CHECKED;
            }
            commands.push(Command {
                path: path.clone(),
                title: item.title.clone(),
                enabled: item.enabled,
                checked: item.checked,
            });
            append(hmenu, flags, commands.len(), &menu_plan::label(item))?;
        } else {
            let popup = CreatePopupMenu()?;
            fill(popup, &item.items, path, commands)?;
            append(hmenu, flags | MF_POPUP, popup.0 as usize, &menu_plan::label(item))?;
        }
        path.pop();
    }
    Ok(())
}

unsafe fn append(
    hmenu: HMENU,
    flags: MENU_ITEM_FLAGS,
    id: usize,
    text: &str,
) -> windows::core::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    AppendMenuW(hmenu, flags, id, PCWSTR(wide.as_ptr()))
}
