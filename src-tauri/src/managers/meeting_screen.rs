//! Reading the meeting app's window through Windows UI Automation (the
//! accessibility tree screen readers use), for the participants' names.

#[cfg(windows)]
mod imp {
    use anyhow::Result;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    /// Visible top-level windows of process `pid`.
    fn windows_of(pid: u32) -> Vec<HWND> {
        unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let (pid, found) = &mut *(lparam.0 as *mut (u32, Vec<HWND>));
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut owner));
            if owner == *pid && IsWindowVisible(hwnd).as_bool() {
                found.push(hwnd);
            }
            true.into()
        }
        let mut state = (pid, Vec::new());
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut state as *mut _ as isize));
        }
        state.1
    }

    /// `(control type id, name)` of every named element in the app's windows.
    pub fn element_names(pid: u32) -> Result<Vec<(i32, String)>> {
        unsafe {
            // Harmless if this thread already joined the MTA.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let all = automation.CreateTrueCondition()?;
            let mut out = Vec::new();
            for hwnd in windows_of(pid) {
                let Ok(root) = automation.ElementFromHandle(hwnd) else {
                    continue;
                };
                let Ok(found) = root.FindAll(TreeScope_Descendants, &all) else {
                    continue;
                };
                for i in 0..found.Length().unwrap_or(0) {
                    let Ok(element) = found.GetElement(i) else {
                        continue;
                    };
                    let name = element
                        .CurrentName()
                        .map(|b| b.to_string())
                        .unwrap_or_default();
                    if !name.trim().is_empty() {
                        let kind = element.CurrentControlType().map(|c| c.0).unwrap_or(0);
                        out.push((kind, name));
                    }
                }
            }
            Ok(out)
        }
    }
}

#[cfg(windows)]
pub use imp::element_names;

/// Participants named in the app's window right now.
#[cfg(windows)]
pub fn participant_names(pid: u32) -> Vec<String> {
    let mut names: Vec<String> = element_names(pid)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, label)| crate::managers::meeting_names::participant_from_label(&label))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(all(test, windows))]
mod tests {
    /// Manual check against a live call: `EZNOTES_MEETING_PID=<pid> cargo
    /// test participants_from_live_meeting -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn participants_from_live_meeting() {
        let pid: u32 = std::env::var("EZNOTES_MEETING_PID")
            .expect("set EZNOTES_MEETING_PID")
            .parse()
            .unwrap();
        // The first read wakes up the browser's accessibility tree.
        let _ = super::participant_names(pid);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let names = super::participant_names(pid);
        println!("participants: {names:?}");
        assert!(!names.is_empty());
    }
}
