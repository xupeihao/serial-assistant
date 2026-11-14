use crate::window::scale;
use std::cell::RefCell;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub type Position = (HWND, i32, i32, i32, i32);

#[derive(Default)]
pub struct WindowLayout {
    previous: RefCell<Vec<Position>>,
}

impl WindowLayout {
    pub fn apply(&self, parent: HWND, dpi: u32, positions: impl IntoIterator<Item = Position>) {
        let mut previous = self.previous.borrow_mut();
        let mut changed = Vec::new();
        let mut uncovered = Vec::new();
        let s = |n| scale(n, dpi);
        for (index, (hwnd, x, y, width, height)) in positions.into_iter().enumerate() {
            let next = (hwnd, s(x), s(y), s(x + width) - s(x), s(y + height) - s(y));
            if let Some(current) = previous.get_mut(index) {
                if *current == next {
                    continue;
                }
                let (_, left, top, _, _) = *current;
                let mut bounds = RECT::default();
                // A combo box's requested height includes its closed dropdown list.
                if unsafe { GetWindowRect(hwnd, &mut bounds) } != 0 {
                    uncovered.push(RECT {
                        left,
                        top,
                        right: left + bounds.right - bounds.left,
                        bottom: top + bounds.bottom - bounds.top,
                    });
                }
                *current = next;
            } else {
                previous.push(next);
            }
            changed.push(next);
        }
        if changed.is_empty() {
            return;
        }
        unsafe {
            // Batch the moves before queuing any painting of the exposed background.
            let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_DEFERERASE;
            let mut batch = BeginDeferWindowPos(changed.len() as i32);
            for &(hwnd, x, y, width, height) in &changed {
                if batch.is_null() {
                    break;
                }
                batch = DeferWindowPos(batch, hwnd, null_mut(), x, y, width, height, flags);
            }
            if batch.is_null() || EndDeferWindowPos(batch) == 0 {
                for (hwnd, x, y, width, height) in changed {
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        x,
                        y,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            for rect in uncovered {
                // WS_CLIPCHILDREN limits this to the background exposed by the layout.
                InvalidateRect(parent, &rect, 0);
            }
        }
    }
}
