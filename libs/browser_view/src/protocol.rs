//! What a browser view's owner and the browser host say to each other, as
//! JSON envelopes on the hosted-app custom channel (`StudioToApp::Custom`
//! down, `AppToStudio::Custom` up). Every message names its view.

use makepad_platform::makepad_micro_serde::*;

/// The registry id of the browser host app.
pub const HOST_APP: &str = "browser_host";

/// The host's environment variable naming the profile folder a browser
/// host keeps its cookies and storage in.
pub const PROFILE_ENV: &str = "MAKEPAD_CEF_PROFILE_DIR";

/// From the owner to the browser host.
#[derive(Clone, Debug, PartialEq, SerJson, DeJson)]
pub enum ViewOp {
    /// Make the view (a window of the host) and load `url`.
    Open { url: String, dark: bool, background: u32, document_script: Option<String> },
    Navigate { url: String },
    Back,
    Forward,
    /// Move through session history by `delta` entries (zero does nothing).
    History { delta: i32 },
    Reload,
    Stop,
    /// Set the page's local content scale. One restores native view metrics.
    PageZoom { factor: f64 },
    /// Restore the native page metrics and visual viewport scale.
    ResetZoom,
    /// A trackpad pinch centered at this point in the view.
    BeginVisualPinch { x: f64, y: f64 },
    UpdateVisualPinch { scale: f64 },
    EndVisualPinch,
    /// The page takes (or loses) the keyboard.
    Focus { on: bool },
    /// A hidden page stops painting.
    Hidden { on: bool },
    /// The page's color scheme and the ground under a transparent page.
    Appearance { dark: bool, background: u32 },
    /// Evaluate `expression` in the page; the answer comes back as
    /// `ViewEvent::Evaluated` with the same `id`.
    Eval { id: u64, expression: String },
    /// Stop capturing the page's sound (started by an audio ring setup).
    AudioStop,
    /// CEF-level input, exactly as the page gets it (see `cef_input`): for
    /// an owner that maps its own events (a recorder's locked page, scripted
    /// input) instead of handing the view's hits to its window. Positions
    /// are in the page's points.
    MouseMove { x: i32, y: i32, modifiers: u32, leave: bool },
    MouseClick { x: i32, y: i32, modifiers: u32, button: i32, up: bool, clicks: i32 },
    MouseWheel { x: i32, y: i32, modifiers: u32, dx: i32, dy: i32 },
    Key { kind: i32, modifiers: u32, windows_key_code: i32, native_key_code: i32, character: u16, unmodified_character: u16, system: bool },
    /// Text committed as an input method would.
    Text { text: String },
    /// Uncommitted IME preedit text.
    ImeComposition { text: String },
    /// Cancel active IME composition.
    ImeCancelComposition,
    /// A pressed button's gesture ends with no release (nothing clicks).
    CaptureLost,
    /// Close the view and its window.
    Close,
    /// Adopt a host-created popup browser under an embedder-assigned `view_id`.
    AdoptPopup { popup_id: u64, view_id: u64 },
    /// Answer a pending file chooser dialog (`paths == None` cancels).
    FileDialogResult { request_id: u64, paths: Option<Vec<String>> },
    /// Answer a pending download request (`save_path == None` cancels).
    DownloadResult { download_id: u32, save_path: Option<String> },
    /// Answer a pending JavaScript dialog (`accept == false` cancels).
    JsDialogResult { request_id: u64, accept: bool, user_input: String },
    /// Answer a pending HTTP authentication challenge (`username == None` cancels).
    AuthResult { request_id: u64, username: Option<String>, password: String },
    /// Answer a pending site permission prompt.
    PermissionResult { request_id: u64, granted: bool },
}

/// From the browser host to the owner.
#[derive(Clone, Debug, PartialEq, SerJson, DeJson)]
pub enum ViewEvent {
    /// The view's window: its frames arrive as that hosted window's.
    Window { window_id: usize },
    /// What the chrome shows, and the committed main-frame navigation
    /// (`epoch`, what frames are stamped with too).
    State { title: String, url: String, loading: bool, can_go_back: bool, can_go_forward: bool, epoch: u64 },
    /// The page asked for a new window.
    Popup { url: String },
    /// The page opened a live popup browser (`window.opener` preserved).
    PopupWindow { popup_id: u64, url: String, window_id: usize },
    /// A URL intended for the host OS (such as `mailto:` or `tel:`).
    ExternalUrl { url: String },
    /// The page asked to open or save a file (`mode`: 0=open, 1=multi, 2=folder, 3=save).
    FileDialog { request_id: u64, mode: u8, title: String, default_path: String, accept_filters: Vec<String> },
    /// A file download is waiting for a destination path.
    DownloadRequested { download_id: u32, suggested_name: String, url: String },
    /// A file download finished, was canceled, or was interrupted (`state`: 0=complete, 1=canceled, 2=interrupted).
    DownloadUpdated { download_id: u32, file_name: String, full_path: String, state: u8, percent: i32 },
    /// A JavaScript dialog or beforeunload prompt (`kind`: 0=alert, 1=confirm, 2=prompt, 3=beforeunload).
    JsDialog { request_id: u64, kind: u8, origin_url: String, message: String, default_prompt: String },
    /// An HTTP or proxy authentication challenge.
    AuthRequested { request_id: u64, is_proxy: bool, host: String, port: i32, realm: String, scheme: String },
    /// A site permission prompt (camera, microphone, geolocation, etc.).
    PermissionRequested { request_id: u64, origin: String, permissions: u32 },
    /// A line the page wrote to its console.
    Console { level: i32, message: String, source: String, line: i32 },
    /// The answer to `ViewOp::Eval`: the value as JSON, or the exception.
    Evaluated { id: u64, ok: bool, value: String },
    /// Active IME composition caret rectangle in page points.
    ImeCaret { x: i32, y: i32, width: i32, height: i32 },
    /// Remote debugging port exposed by the browser host.
    DevToolsReady { port: u16 },
    DevToolsTarget { target_id: String },
    DevToolsTargetError { message: String },
    /// The page (or the engine) failed; the view shows nothing more.
    Error { message: String },
    /// The view is gone (after `ViewOp::Close`).
    Closed,
}

#[derive(SerJson, DeJson)]
struct OpEnvelope {
    makepad_browser_view: u64,
    op: ViewOp,
}

#[derive(SerJson, DeJson)]
struct EventEnvelope {
    makepad_browser_event: u64,
    event: ViewEvent,
}

pub fn op_to_json(view: u64, op: &ViewOp) -> String {
    OpEnvelope { makepad_browser_view: view, op: op.clone() }.serialize_json()
}

pub fn parse_op(json: &str) -> Option<(u64, ViewOp)> {
    if !json.contains("\"makepad_browser_view\"") {
        return None;
    }
    OpEnvelope::deserialize_json(json).ok().map(|e| (e.makepad_browser_view, e.op))
}

pub fn event_to_json(view: u64, event: &ViewEvent) -> String {
    EventEnvelope { makepad_browser_event: view, event: event.clone() }.serialize_json()
}

pub fn parse_event(json: &str) -> Option<(u64, ViewEvent)> {
    if !json.contains("\"makepad_browser_event\"") {
        return None;
    }
    EventEnvelope::deserialize_json(json).ok().map(|e| (e.makepad_browser_event, e.event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_round_trip() {
        for op in [
            ViewOp::Open { url: "https://makepad.nl/?a=\"b\"".into(), dark: true, background: 0xff102030, document_script: Some("1+1".into()) },
            ViewOp::Back,
            ViewOp::PageZoom { factor: 1.4 },
            ViewOp::ResetZoom,
            ViewOp::BeginVisualPinch { x: 135.5, y: 48.0 },
            ViewOp::UpdateVisualPinch { scale: 1.05 },
            ViewOp::EndVisualPinch,
            ViewOp::Focus { on: true },
            ViewOp::Eval { id: 7, expression: "1 + 1".into() },
            ViewOp::ImeComposition { text: "かな".into() },
            ViewOp::ImeCancelComposition,
            ViewOp::Close,
            ViewOp::AdoptPopup { popup_id: 2, view_id: 9 },
            ViewOp::FileDialogResult { request_id: 4, paths: Some(vec!["/tmp/a.txt".into()]) },
            ViewOp::DownloadResult { download_id: 7, save_path: None },
            ViewOp::JsDialogResult { request_id: 8, accept: true, user_input: "ok".into() },
            ViewOp::AuthResult { request_id: 9, username: Some("alice".into()), password: "pw".into() },
            ViewOp::PermissionResult { request_id: 10, granted: true },
        ] {
            let json = op_to_json(3, &op);
            assert_eq!(parse_op(&json), Some((3, op)));
            assert_eq!(parse_event(&json), None);
        }
    }

    #[test]
    fn events_round_trip() {
        for event in [
            ViewEvent::Window { window_id: 2 },
            ViewEvent::State { title: "t".into(), url: "u".into(), loading: true, can_go_back: false, can_go_forward: true, epoch: 3 },
            ViewEvent::Evaluated { id: 7, ok: true, value: "2".into() },
            ViewEvent::ImeCaret { x: 10, y: 20, width: 8, height: 18 },
            ViewEvent::DevToolsReady { port: 9222 },
            ViewEvent::PopupWindow { popup_id: 2, url: "https://auth.example".into(), window_id: 4 },
            ViewEvent::ExternalUrl { url: "mailto:user@example.com".into() },
            ViewEvent::FileDialog { request_id: 4, mode: 1, title: "Pick".into(), default_path: "".into(), accept_filters: vec![".png".into()] },
            ViewEvent::DownloadRequested { download_id: 7, suggested_name: "a.zip".into(), url: "https://example.com/a.zip".into() },
            ViewEvent::DownloadUpdated { download_id: 7, file_name: "a.zip".into(), full_path: "/tmp/a.zip".into(), state: 0, percent: 42 },
            ViewEvent::JsDialog { request_id: 8, kind: 2, origin_url: "https://example.com".into(), message: "Name?".into(), default_prompt: "a".into() },
            ViewEvent::AuthRequested { request_id: 9, is_proxy: false, host: "example.com".into(), port: 443, realm: "Admin".into(), scheme: "basic".into() },
            ViewEvent::PermissionRequested { request_id: 10, origin: "https://example.com".into(), permissions: 5 },
            ViewEvent::Closed,
        ] {
            let json = event_to_json(5, &event);
            assert_eq!(parse_event(&json), Some((5, event)));
            assert_eq!(parse_op(&json), None);
        }
    }
}
