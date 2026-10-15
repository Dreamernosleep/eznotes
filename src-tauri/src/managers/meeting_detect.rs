//! Automatic meeting detection (Windows).
//!
//! Windows records, per app, when it started and stopped using the
//! microphone (the privacy "consent store" in the registry); an entry whose
//! stop time is 0 is using the mic right now. A meeting app (Zoom, Teams,
//! Webex, Slack, Discord) holding the mic counts as a meeting. A browser
//! holding the mic counts only when one of its windows shows a meeting
//! (Google Meet, Teams or Zoom on the web).
//!
//! When one is found and nothing is recording, a small card asks whether to
//! record it. A meeting recorded from the card stops by itself once the app
//! releases the mic.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Emitter, Manager};

use crate::managers::meeting::{MeetingManager, StopReason};
use crate::settings::{get_settings, write_settings};

const POLL: Duration = Duration::from_secs(2);
/// After the app releases the mic, wait this long (and for quiet) before
/// stopping a meeting it started.
const RELEASE_GRACE: Duration = Duration::from_secs(5);

/// A meeting app and how to recognise it.
pub struct MeetingApp {
    pub name: &'static str,
    /// Lower-case executable file names (matched by name, never full path:
    /// Slack and Discord move to a new folder on every update).
    pub exes: &'static [&'static str],
    /// Packaged (Store) app ids as they appear in the registry.
    pub packaged: &'static [&'static str],
    /// For browsers: a window title must contain one of these.
    pub titles: &'static [&'static str],
}

pub const MEETING_APPS: &[MeetingApp] = &[
    MeetingApp {
        name: "Zoom",
        exes: &["zoom.exe"],
        packaged: &[],
        titles: &[],
    },
    MeetingApp {
        name: "Microsoft Teams",
        exes: &["ms-teams.exe", "teams.exe"],
        packaged: &["MSTeams_8wekyb3d8bbwe"],
        titles: &[],
    },
    MeetingApp {
        name: "Webex",
        exes: &["ciscocollabhost.exe", "webexmta.exe", "webex.exe"],
        packaged: &[],
        titles: &[],
    },
    MeetingApp {
        name: "Slack",
        exes: &["slack.exe"],
        packaged: &[],
        titles: &[],
    },
    MeetingApp {
        name: "Discord",
        exes: &["discord.exe"],
        packaged: &[],
        titles: &[],
    },
    MeetingApp {
        name: "Google Meet",
        exes: &["chrome.exe", "msedge.exe"],
        packaged: &[],
        titles: &["Meet - ", "Google Meet"],
    },
    MeetingApp {
        name: "Microsoft Teams",
        exes: &["chrome.exe", "msedge.exe"],
        packaged: &[],
        titles: &["Microsoft Teams"],
    },
    MeetingApp {
        name: "Zoom",
        exes: &["chrome.exe", "msedge.exe"],
        packaged: &[],
        titles: &["Zoom Meeting", "Zoom Workplace"],
    },
];

/// An app currently using the microphone, by its registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicHolder {
    /// Lower-case exe file name, or the packaged app id.
    pub id: String,
    pub packaged: bool,
}

/// A visible top-level window: its process and title.
#[derive(Debug, Clone)]
pub struct AppWindow {
    pub pid: u32,
    /// Lower-case exe file name of the window's process.
    pub exe: String,
    pub title: String,
}

/// A meeting found on screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DetectedMeeting {
    /// Display name, e.g. "Zoom".
    pub app: String,
    /// What the ignore list stores: the exe or Store id for desktop apps,
    /// the service name for browser meetings (so ignoring Google Meet doesn't
    /// also ignore Teams on the web).
    pub app_id: String,
    /// The registry id of the app holding the mic (exe or Store id), used to
    /// notice when the meeting ends.
    pub holder: String,
    /// Process to capture audio from, when a window was found for it.
    pub pid: Option<u32>,
}

/// The NonPackaged registry key names a desktop app by its full path with
/// `\` replaced by `#`.
fn exe_from_nonpackaged_key(key: &str) -> String {
    key.rsplit('#').next().unwrap_or(key).to_lowercase()
}

/// Find a meeting among the apps using the mic, skipping ignored ones.
pub fn detect(
    holders: &[MicHolder],
    windows: &[AppWindow],
    ignored: &HashSet<String>,
) -> Option<DetectedMeeting> {
    for app in MEETING_APPS {
        let in_browser = !app.titles.is_empty();
        if in_browser && ignored.contains(app.name) {
            continue;
        }
        for holder in holders {
            if !in_browser && ignored.contains(&holder.id) {
                continue;
            }
            let matches = if holder.packaged {
                app.packaged.contains(&holder.id.as_str())
            } else {
                app.exes.contains(&holder.id.as_str())
            };
            if !matches {
                continue;
            }
            let window = windows.iter().find(|w| {
                app.exes.contains(&w.exe.as_str())
                    && (app.titles.is_empty() || app.titles.iter().any(|t| w.title.contains(t)))
            });
            if !app.titles.is_empty() && window.is_none() {
                continue; // a browser using the mic, but not for a meeting
            }
            return Some(DetectedMeeting {
                app: app.name.to_string(),
                app_id: if in_browser {
                    app.name.to_string()
                } else {
                    holder.id.clone()
                },
                holder: holder.id.clone(),
                pid: window.map(|w| w.pid),
            });
        }
    }
    None
}

#[cfg(windows)]
pub fn mic_holders() -> Vec<MicHolder> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const STORE: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone";
    let in_use = |key: &RegKey| {
        key.get_value::<u64, _>("LastUsedTimeStop").ok() == Some(0)
            && key.get_value::<u64, _>("LastUsedTimeStart").unwrap_or(0) > 0
    };

    let mut holders = Vec::new();
    let Ok(store) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(STORE) else {
        return holders;
    };
    for name in store.enum_keys().flatten() {
        if name == "NonPackaged" {
            let Ok(desktop) = store.open_subkey(&name) else {
                continue;
            };
            for app in desktop.enum_keys().flatten() {
                if desktop.open_subkey(&app).is_ok_and(|k| in_use(&k)) {
                    holders.push(MicHolder {
                        id: exe_from_nonpackaged_key(&app),
                        packaged: false,
                    });
                }
            }
        } else if store.open_subkey(&name).is_ok_and(|k| in_use(&k)) {
            holders.push(MicHolder {
                id: name,
                packaged: true,
            });
        }
    }
    holders
}

#[cfg(windows)]
pub fn visible_windows() -> Vec<AppWindow> {
    use windows::core::BOOL;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = &mut *(lparam.0 as *mut Vec<(u32, String)>);
        if IsWindowVisible(hwnd).as_bool() {
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(hwnd, &mut buf);
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                found.push((pid, String::from_utf16_lossy(&buf[..n as usize])));
            }
        }
        true.into()
    }

    let mut found: Vec<(u32, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
    }

    let mut exe_of: HashMap<u32, String> = HashMap::new();
    found
        .into_iter()
        .filter_map(|(pid, title)| {
            let exe = exe_of
                .entry(pid)
                .or_insert_with(|| unsafe {
                    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    else {
                        return String::new();
                    };
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    let ok = QueryFullProcessImageNameW(
                        process,
                        PROCESS_NAME_WIN32,
                        PWSTR(buf.as_mut_ptr()),
                        &mut len,
                    )
                    .is_ok();
                    let _ = CloseHandle(process);
                    if !ok {
                        return String::new();
                    }
                    let path = String::from_utf16_lossy(&buf[..len as usize]);
                    path.rsplit('\\').next().unwrap_or("").to_lowercase()
                })
                .clone();
            (!exe.is_empty()).then_some(AppWindow { pid, exe, title })
        })
        .collect()
}

/// Event telling the UI a meeting was detected (or `null`: the prompt is gone).
pub const PROMPT_EVENT: &str = "meeting-prompt";
const PROMPT_WINDOW: &str = "meeting_prompt";

/// What the detector currently offers or follows.
#[derive(Default)]
struct DetectorState {
    /// The meeting the prompt card is showing.
    prompting: Option<DetectedMeeting>,
    /// App ids the user said "Not now" to, until they release the mic.
    dismissed: HashSet<String>,
    /// App id of a meeting recorded from the prompt, for the auto-stop.
    following: Option<String>,
    released_at: Option<Instant>,
}

pub struct MeetingDetector {
    app: AppHandle,
    state: Mutex<DetectorState>,
}

impl MeetingDetector {
    pub fn new(app: &AppHandle) -> Self {
        Self {
            app: app.clone(),
            state: Mutex::new(DetectorState::default()),
        }
    }

    /// Poll in the background for the life of the app.
    #[cfg(windows)]
    pub fn spawn(self: &Arc<Self>) {
        let detector = self.clone();
        thread::spawn(move || loop {
            thread::sleep(POLL);
            if get_settings(&detector.app).meeting_auto_detect {
                detector.tick(&mic_holders(), &visible_windows());
            } else {
                detector.hide_prompt();
            }
        });
    }

    fn tick(&self, holders: &[MicHolder], windows: &[AppWindow]) {
        let Some(meetings) = self.app.try_state::<Arc<MeetingManager>>() else {
            return;
        };
        let ignored: HashSet<String> = get_settings(&self.app)
            .meeting_ignored_apps
            .into_iter()
            .collect();
        let detected = detect(holders, windows, &ignored);
        let holding: HashSet<&str> = holders.iter().map(|h| h.id.as_str()).collect();

        let mut state = self.state.lock().unwrap();
        state.dismissed.retain(|id| holding.contains(id.as_str()));

        if meetings.is_recording() {
            state.prompting = None;
            let Some(app_id) = state.following.clone() else {
                drop(state);
                self.hide_prompt();
                return;
            };
            if holding.contains(app_id.as_str()) {
                state.released_at = None;
            } else {
                let since = *state.released_at.get_or_insert_with(Instant::now);
                let quiet = meetings
                    .silence()
                    .is_none_or(|silence| silence >= RELEASE_GRACE);
                if since.elapsed() >= RELEASE_GRACE && quiet {
                    info!("{app_id} released the microphone; stopping the meeting");
                    state.following = None;
                    state.released_at = None;
                    drop(state);
                    if let Err(e) = meetings.stop(StopReason::AppClosed) {
                        error!("Automatic meeting stop failed: {e}");
                    }
                    return;
                }
            }
            drop(state);
            self.hide_prompt();
            return;
        }
        state.following = None;

        let offer = detected.filter(|d| !state.dismissed.contains(&d.holder));
        if offer == state.prompting {
            return;
        }
        state.prompting = offer.clone();
        drop(state);
        match offer {
            Some(meeting) => self.show_prompt(&meeting),
            None => self.hide_prompt(),
        }
    }

    fn show_prompt(&self, meeting: &DetectedMeeting) {
        debug!("Meeting detected: {} ({})", meeting.app, meeting.app_id);
        crate::meeting_prompt::show(&self.app, PROMPT_WINDOW);
        let _ = self.app.emit_to(PROMPT_WINDOW, PROMPT_EVENT, Some(meeting));
    }

    fn hide_prompt(&self) {
        if let Some(window) = self.app.get_webview_window(PROMPT_WINDOW) {
            let _ = window.hide();
        }
    }

    /// The meeting currently offered, for a prompt window that just opened.
    pub fn current(&self) -> Option<DetectedMeeting> {
        self.state.lock().unwrap().prompting.clone()
    }

    /// "Record": start recording the offered meeting, capturing only its app's
    /// audio when its process is known.
    pub fn accept(&self) -> anyhow::Result<i64> {
        let meeting = self
            .state
            .lock()
            .unwrap()
            .prompting
            .take()
            .ok_or_else(|| anyhow::anyhow!("No meeting is being offered"))?;
        self.hide_prompt();
        let manager = self.app.state::<Arc<MeetingManager>>();
        let id = manager.start_for_app(Some(meeting.app.clone()), meeting.pid)?;
        self.state.lock().unwrap().following = Some(meeting.holder);
        Ok(id)
    }

    /// "Not now": hide until the app stops using the mic.
    pub fn dismiss(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(meeting) = state.prompting.take() {
            state.dismissed.insert(meeting.holder);
        }
        drop(state);
        self.hide_prompt();
    }

    /// "Never for this app": add it to the ignore list.
    pub fn ignore_app(&self) {
        let meeting = self.state.lock().unwrap().prompting.take();
        if let Some(meeting) = meeting {
            let mut settings = get_settings(&self.app);
            if !settings.meeting_ignored_apps.contains(&meeting.app_id) {
                settings.meeting_ignored_apps.push(meeting.app_id);
                write_settings(&self.app, settings);
            }
        }
        self.hide_prompt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(id: &str) -> MicHolder {
        MicHolder {
            id: id.to_string(),
            packaged: false,
        }
    }

    fn window(pid: u32, exe: &str, title: &str) -> AppWindow {
        AppWindow {
            pid,
            exe: exe.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn registry_key_to_exe_name() {
        assert_eq!(
            exe_from_nonpackaged_key("C:#Users#paulp#AppData#Local#slack#app-4.52.178#slack.exe"),
            "slack.exe"
        );
        assert_eq!(
            exe_from_nonpackaged_key("C:#Users#paulp#AppData#Roaming#Zoom#bin#Zoom.exe"),
            "zoom.exe"
        );
    }

    #[test]
    fn desktop_meeting_app_counts_on_its_own() {
        let found = detect(
            &[holder("zoom.exe")],
            &[window(42, "zoom.exe", "Zoom Meeting")],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(found.app, "Zoom");
        assert_eq!(found.pid, Some(42));
    }

    #[test]
    fn browser_needs_a_meeting_window() {
        let none = HashSet::new();
        assert_eq!(
            detect(
                &[holder("chrome.exe")],
                &[window(7, "chrome.exe", "YouTube - Google Chrome")],
                &none
            ),
            None
        );
        let found = detect(
            &[holder("chrome.exe")],
            &[
                window(7, "chrome.exe", "YouTube - Google Chrome"),
                window(7, "chrome.exe", "Meet - abc-defg-hij - Google Chrome"),
            ],
            &none,
        )
        .unwrap();
        assert_eq!(found.app, "Google Meet");
    }

    #[test]
    fn other_mic_users_and_ignored_apps_are_skipped() {
        let none = HashSet::new();
        assert_eq!(detect(&[holder("wispr flow.exe")], &[], &none), None);
        assert_eq!(detect(&[holder("handy.exe")], &[], &none), None);
        let ignored: HashSet<String> = ["slack.exe".to_string()].into();
        assert_eq!(detect(&[holder("slack.exe")], &[], &ignored), None);
    }

    #[test]
    fn ignoring_a_browser_meeting_service_keeps_the_others() {
        let windows = [
            window(7, "chrome.exe", "Meet - abc-defg-hij"),
            window(7, "chrome.exe", "Chat | Microsoft Teams"),
        ];
        let ignored: HashSet<String> = ["Google Meet".to_string()].into();
        let found = detect(&[holder("chrome.exe")], &windows, &ignored).unwrap();
        assert_eq!(found.app, "Microsoft Teams");
        assert_eq!(found.app_id, "Microsoft Teams");
        assert_eq!(found.holder, "chrome.exe");
    }

    #[test]
    fn packaged_teams_is_found_by_its_store_id() {
        let teams = MicHolder {
            id: "MSTeams_8wekyb3d8bbwe".to_string(),
            packaged: true,
        };
        let found = detect(&[teams], &[], &HashSet::new()).unwrap();
        assert_eq!(found.app, "Microsoft Teams");
        assert_eq!(found.pid, None);
    }
}
