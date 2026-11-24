#![cfg(target_os = "ios")]

use std::ffi::c_void;
use std::ptr;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::model::{
    AppCardViewModel, Operation, ReadinessRowViewModel, StatusTone, UiState, execute_operation,
};
use crate::objc_ffi::{
    AutoreleasePool, CGRect, FontWeight, Id, NS_LINE_BREAK_BY_TRUNCATING_TAIL,
    NS_LINE_BREAK_BY_WORD_WRAPPING, NS_TEXT_ALIGNMENT_LEFT, NS_TEXT_ALIGNMENT_RIGHT, OwnedId, Sel,
    UI_ACTIVITY_INDICATOR_STYLE_MEDIUM, UI_BUTTON_TYPE_SYSTEM,
    UI_CONTENT_INSET_ADJUSTMENT_ALWAYS, UI_CONTROL_EVENT_TOUCH_UP_INSIDE,
    UI_CONTROL_EVENT_VALUE_CHANGED, UI_CONTROL_STATE_DISABLED, UI_CONTROL_STATE_NORMAL,
    UI_SCROLL_INDICATOR_STYLE_WHITE, UI_STATUS_BAR_STYLE_LIGHT_CONTENT,
    UI_USER_INTERFACE_STYLE_DARK, UIApplicationMain, add_method, alloc_init, alloc_init_frame,
    allocate_class_pair, class, is_main_thread, local_tz_offset_secs, mono_font, msg_bool, msg_id,
    msg_id_i64, msg_insets, msg_rect, msg_size_size, msg_super_void, msg_void, msg_void_bool,
    msg_void_f64, msg_void_i64, msg_void_id, msg_void_id_u64, msg_void_perform_main, msg_void_rect,
    msg_void_size, msg_void_target_action, ns_string, rect, register_class_pair,
    remove_all_subviews, rgb, rgba, sel, size, system_font,
};

struct MainThreadUi {
    controller: Option<OwnedId>,
    scroll_view: Option<OwnedId>,
    content_view: Option<OwnedId>,
    refresh_control: Option<OwnedId>,
    state: UiState,
    busy_operation: Option<Operation>,
    last_layout_width: f64,
    last_safe_lr: (f64, f64),
    needs_rebuild: bool,
}

impl MainThreadUi {
    fn new() -> Self {
        Self {
            controller: None,
            scroll_view: None,
            content_view: None,
            refresh_control: None,
            state: UiState::initial_loading(),
            busy_operation: None,
            last_layout_width: 0.0,
            last_safe_lr: (0.0, 0.0),
            needs_rebuild: true,
        }
    }
}

/// Snapshot of persistent UIKit handles + immutable data cloned out of `APP_UI`
/// BEFORE calling into UIKit, so no Mutex is ever held across a UIKit call
/// (completely preventing re-entrancy deadlocks).
struct LayoutSnapshot {
    controller: Id,
    scroll_view: Id,
    content_view: Id,
    state: UiState,
    busy_operation: Option<Operation>,
}

/// SAFETY: Only ever accessed after asserting `NSThread.isMainThread`.
struct MainThreadCell<T>(Mutex<T>);
unsafe impl<T> Sync for MainThreadCell<T> {}

/// Dedicated storage for `UIApplicationDelegate.window` so `window` / `setWindow:`
/// never contends with `APP_UI`.
static DELEGATE_WINDOW: MainThreadCell<Option<OwnedId>> = MainThreadCell(Mutex::new(None));
static APP_UI: MainThreadCell<Option<MainThreadUi>> = MainThreadCell(Mutex::new(None));
static REBUILDING: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static PENDING_RESULT: Mutex<Option<UiState>> = Mutex::new(None);

fn with_main_ui<R>(f: impl FnOnce(&mut MainThreadUi) -> R) -> R {
    assert!(
        is_main_thread(),
        "UIKit state must only be accessed on the main thread"
    );
    let mut guard = APP_UI.0.lock().unwrap_or_else(|e| e.into_inner());
    let ui = guard.get_or_insert_with(MainThreadUi::new);
    f(ui)
}

// ---------------------------------------------------------------------------
// Rebox dark theme palette
// ---------------------------------------------------------------------------

fn col_bg() -> Id {
    rgb(0.039, 0.047, 0.086)
}
fn col_card() -> Id {
    rgb(0.075, 0.090, 0.153)
}
fn col_diag_card() -> Id {
    rgb(0.067, 0.082, 0.141)
}
fn col_border() -> Id {
    rgb(0.145, 0.173, 0.275)
}
fn col_divider() -> Id {
    rgb(0.125, 0.149, 0.239)
}
fn col_text_primary() -> Id {
    rgb(0.957, 0.965, 0.992)
}
fn col_text_secondary() -> Id {
    rgb(0.620, 0.651, 0.761)
}
fn col_text_muted() -> Id {
    rgb(0.455, 0.494, 0.616)
}
fn col_accent() -> Id {
    rgb(0.565, 0.443, 1.0)
}
fn col_btn_primary() -> Id {
    rgb(0.435, 0.278, 0.961)
}
fn col_btn_secondary() -> Id {
    rgb(0.110, 0.133, 0.224)
}
fn col_btn_secondary_border() -> Id {
    rgb(0.196, 0.231, 0.365)
}

fn tone_fg(tone: StatusTone) -> Id {
    match tone {
        StatusTone::Green => rgb(0.165, 0.816, 0.427),
        StatusTone::Amber => rgb(0.969, 0.659, 0.129),
        StatusTone::Red => rgb(0.953, 0.322, 0.322),
        StatusTone::Neutral => rgb(0.647, 0.690, 0.792),
    }
}

fn tone_pill_bg(tone: StatusTone) -> Id {
    match tone {
        StatusTone::Green => rgba(0.133, 0.773, 0.369, 0.16),
        StatusTone::Amber => rgba(0.961, 0.620, 0.043, 0.16),
        StatusTone::Red => rgba(0.937, 0.267, 0.267, 0.16),
        StatusTone::Neutral => rgba(0.580, 0.639, 0.722, 0.14),
    }
}

fn tone_border(tone: StatusTone) -> Id {
    match tone {
        StatusTone::Green => rgba(0.165, 0.816, 0.427, 0.30),
        StatusTone::Amber => rgba(0.969, 0.659, 0.129, 0.38),
        StatusTone::Red => rgba(0.953, 0.322, 0.322, 0.42),
        StatusTone::Neutral => col_border(),
    }
}

// ---------------------------------------------------------------------------
// UIKit view builders (all alloc/init views wrapped in OwnedId -> auto-released
// after addSubview:, so superview owns the single +1 retain count).
// ---------------------------------------------------------------------------

unsafe fn style_layer(view: Id, radius: f64, border_width: f64, border_color: Option<Id>) {
    unsafe {
        let layer = msg_id(view, c"layer");
        if layer.is_null() {
            return;
        }
        msg_void_f64(layer, c"setCornerRadius:", radius);
        if border_width > 0.0 {
            msg_void_f64(layer, c"setBorderWidth:", border_width);
            if let Some(color) = border_color {
                let cg_color = msg_id(color, c"CGColor");
                msg_void_id(layer, c"setBorderColor:", cg_color);
            }
        }
        msg_void_bool(layer, c"setMasksToBounds:", true);
    }
}

unsafe fn make_view(parent: Id, frame: CGRect, bg: Id) -> Id {
    unsafe {
        let view = alloc_init_frame(c"UIView", frame);
        msg_void_id(view.as_id(), c"setBackgroundColor:", bg);
        msg_void_id(parent, c"addSubview:", view.as_id());
        // Returned raw Id stays alive because `parent` retains it; `OwnedId` drops + releases our +1.
        view.as_id()
    }
}

unsafe fn make_dot(parent: Id, x: f64, y: f64, diameter: f64, color: Id) {
    unsafe {
        let dot = make_view(parent, rect(x, y, diameter, diameter), color);
        style_layer(dot, diameter * 0.5, 0.0, None);
    }
}

unsafe fn make_label_measured(
    parent: Id,
    x: f64,
    y: f64,
    max_width: f64,
    text: &str,
    font: Id,
    color: Id,
    lines: i64,
    alignment: i64,
) -> (Id, f64, f64) {
    unsafe {
        let lbl = alloc_init_frame(c"UILabel", rect(x, y, max_width, 0.0));
        let id = lbl.as_id();
        msg_void_id(id, c"setText:", ns_string(text));
        msg_void_id(id, c"setFont:", font);
        msg_void_id(id, c"setTextColor:", color);
        msg_void_i64(id, c"setNumberOfLines:", lines);
        msg_void_i64(id, c"setTextAlignment:", alignment);
        msg_void_i64(
            id,
            c"setLineBreakMode:",
            if lines == 1 {
                NS_LINE_BREAK_BY_TRUNCATING_TAIL
            } else {
                NS_LINE_BREAK_BY_WORD_WRAPPING
            },
        );
        let fitted = msg_size_size(id, c"sizeThatFits:", size(max_width, 10_000.0));
        let final_w = fitted.width.ceil().min(max_width);
        let final_h = fitted.height.ceil().max(14.0);
        msg_void_rect(id, c"setFrame:", rect(x, y, max_width, final_h));
        msg_void_id(parent, c"addSubview:", id);
        (id, final_w, final_h)
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn make_action_button(
    parent: Id,
    controller: Id,
    frame: CGRect,
    title: &str,
    action: &std::ffi::CStr,
    bg: Id,
    border: Option<Id>,
    text_color: Id,
    font_size: f64,
    enabled: bool,
    alpha: f64,
    show_spinner: bool,
) {
    unsafe {
        // NOTE: `+[UIButton buttonWithType:]` returns an autoreleased (+0) object,
        // NOT an owning (+1) reference — do NOT wrap in `OwnedId::from_owned`.
        let btn = msg_id_i64(class(c"UIButton"), c"buttonWithType:", UI_BUTTON_TYPE_SYSTEM);
        msg_void_rect(btn, c"setFrame:", frame);
        msg_void_id(btn, c"setBackgroundColor:", bg);
        style_layer(
            btn,
            14.0,
            if border.is_some() { 1.0 } else { 0.0 },
            border,
        );
        let title_ns = ns_string(title);
        msg_void_id_u64(btn, c"setTitle:forState:", title_ns, UI_CONTROL_STATE_NORMAL);
        msg_void_id_u64(
            btn,
            c"setTitle:forState:",
            title_ns,
            UI_CONTROL_STATE_DISABLED,
        );
        msg_void_id_u64(
            btn,
            c"setTitleColor:forState:",
            text_color,
            UI_CONTROL_STATE_NORMAL,
        );
        msg_void_id_u64(
            btn,
            c"setTitleColor:forState:",
            text_color,
            UI_CONTROL_STATE_DISABLED,
        );
        let title_lbl = msg_id(btn, c"titleLabel");
        if !title_lbl.is_null() {
            msg_void_id(
                title_lbl,
                c"setFont:",
                system_font(font_size, FontWeight::Semibold),
            );
        }
        msg_void_bool(btn, c"setEnabled:", enabled);
        msg_void_f64(btn, c"setAlpha:", alpha);
        msg_void_target_action(
            btn,
            c"addTarget:action:forControlEvents:",
            controller,
            sel(action),
            UI_CONTROL_EVENT_TOUCH_UP_INSIDE,
        );
        msg_void_id(parent, c"addSubview:", btn);

        if show_spinner {
            let raw = msg_id(class(c"UIActivityIndicatorView"), c"alloc");
            let spin = OwnedId::from_owned(msg_id_i64(
                raw,
                c"initWithActivityIndicatorStyle:",
                UI_ACTIVITY_INDICATOR_STYLE_MEDIUM,
            ));
            let sid = spin.as_id();
            let s_size = 20.0;
            msg_void_rect(
                sid,
                c"setFrame:",
                rect(18.0, (frame.size.height - s_size) * 0.5, s_size, s_size),
            );
            msg_void_id(sid, c"setColor:", col_text_primary());
            msg_void_bool(sid, c"setHidesWhenStopped:", true);
            msg_void_id(btn, c"addSubview:", sid);
            msg_void(sid, c"startAnimating");
        }
    }
}

// ---------------------------------------------------------------------------
// Card rendering
// ---------------------------------------------------------------------------

unsafe fn render_section_header(parent: Id, x: f64, y: f64, width: f64, title: &str) -> f64 {
    unsafe {
        let (_, _, h) = make_label_measured(
            parent,
            x + 4.0,
            y,
            width - 8.0,
            title,
            system_font(12.0, FontWeight::Bold),
            col_text_muted(),
            1,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        y + h + 10.0
    }
}

unsafe fn render_app_card(parent: Id, x: f64, y: f64, width: f64, app: &AppCardViewModel) -> f64 {
    unsafe {
        let pad = 16.0;
        let inner_w = (width - pad * 2.0).max(120.0);
        let card_border = match app.tone {
            StatusTone::Green | StatusTone::Neutral => col_border(),
            StatusTone::Amber | StatusTone::Red => tone_border(app.tone),
        };
        let card = make_view(parent, rect(x, y, width, 100.0), col_card());
        style_layer(card, 16.0, 1.0, Some(card_border));

        let mut cy = pad;

        // Measure status badge pill first so title wraps cleanly without overlapping it.
        let badge_font = system_font(12.0, FontWeight::Semibold);
        let probe = alloc_init_frame(c"UILabel", rect(0.0, 0.0, inner_w * 0.55, 0.0));
        msg_void_id(probe.as_id(), c"setText:", ns_string(&app.badge_text));
        msg_void_id(probe.as_id(), c"setFont:", badge_font);
        let b_fit = msg_size_size(probe.as_id(), c"sizeThatFits:", size(inner_w * 0.55, 40.0));
        drop(probe);

        let dot_d = 7.0;
        let pill_h = (b_fit.height.ceil() + 10.0).max(24.0);
        let pill_w = (10.0 + dot_d + 6.0 + b_fit.width.ceil() + 10.0).min(inner_w * 0.55);
        let pill_x = width - pad - pill_w;

        let pill = make_view(
            card,
            rect(pill_x, cy, pill_w, pill_h),
            tone_pill_bg(app.tone),
        );
        style_layer(pill, pill_h * 0.5, 0.0, None);
        make_dot(pill, 10.0, (pill_h - dot_d) * 0.5, dot_d, tone_fg(app.tone));
        let lbl_x = 10.0 + dot_d + 6.0;
        let lbl_y = ((pill_h - b_fit.height.ceil()) * 0.5).max(0.0);
        make_label_measured(
            pill,
            lbl_x,
            lbl_y,
            (pill_w - lbl_x - 8.0).max(20.0),
            &app.badge_text,
            badge_font,
            tone_fg(app.tone),
            1,
            NS_TEXT_ALIGNMENT_LEFT,
        );

        let name_w = (inner_w - pill_w - 10.0).max(80.0);
        let (_, _, name_h) = make_label_measured(
            card,
            pad,
            cy,
            name_w,
            &app.name,
            system_font(18.0, FontWeight::Bold),
            col_text_primary(),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        cy += name_h.max(pill_h) + 4.0;

        if !app.bundle_id.is_empty() {
            let (_, _, bid_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                &app.bundle_id,
                mono_font(12.0, FontWeight::Regular),
                col_text_secondary(),
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += bid_h + 12.0;
        } else {
            cy += 8.0;
        }

        // Divider
        make_view(card, rect(pad, cy, inner_w, 1.0), col_divider());
        cy += 12.0;

        let rem_color = match app.tone {
            StatusTone::Green | StatusTone::Neutral => col_text_primary(),
            StatusTone::Amber | StatusTone::Red => tone_fg(app.tone),
        };
        let (_, _, rem_h) = make_label_measured(
            card,
            pad,
            cy,
            inner_w,
            &app.remaining_text,
            system_font(19.0, FontWeight::Bold),
            rem_color,
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        cy += rem_h + 4.0;

        let (_, _, exp_h) = make_label_measured(
            card,
            pad,
            cy,
            inner_w,
            &app.expiration_text,
            system_font(13.5, FontWeight::Regular),
            col_text_secondary(),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        cy += exp_h;

        if let Some(detail) = &app.detail_text {
            cy += 8.0;
            let detail_color = match app.tone {
                StatusTone::Red => tone_fg(StatusTone::Red),
                StatusTone::Amber => tone_fg(StatusTone::Amber),
                StatusTone::Green => tone_fg(StatusTone::Green),
                StatusTone::Neutral => col_text_secondary(),
            };
            let (_, _, det_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                detail,
                system_font(13.0, FontWeight::Medium),
                detail_color,
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += det_h;
        }

        let card_h = cy + pad;
        msg_void_rect(card, c"setFrame:", rect(x, y, width, card_h));
        y + card_h + 12.0
    }
}

unsafe fn render_readiness_card(
    parent: Id,
    x: f64,
    y: f64,
    width: f64,
    rows: &[ReadinessRowViewModel],
) -> f64 {
    unsafe {
        let pad_x = 16.0;
        let inner_w = (width - pad_x * 2.0).max(120.0);
        let card = make_view(parent, rect(x, y, width, 100.0), col_card());
        style_layer(card, 16.0, 1.0, Some(col_border()));

        let mut cy = 4.0;
        for (idx, row) in rows.iter().enumerate() {
            let row_pad_y = 12.0;
            let val_font = system_font(14.5, FontWeight::Semibold);
            let title_font = system_font(14.5, FontWeight::Medium);

            let has_dot = row.tone != StatusTone::Neutral;
            let dot_d = 8.0;
            let dot_space = if has_dot { dot_d + 7.0 } else { 0.0 };
            let left_max = inner_w * 0.43;
            let right_max = (inner_w - left_max - 10.0 - dot_space).max(80.0);
            let right_x = width - pad_x - right_max;

            let val_color = match row.tone {
                StatusTone::Neutral => rgb(0.792, 0.824, 0.914),
                _ => tone_fg(row.tone),
            };

            let (_, _, title_h) = make_label_measured(
                card,
                pad_x,
                cy + row_pad_y,
                left_max,
                &row.title,
                title_font,
                col_text_primary(),
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );

            let (_, val_w, val_h) = make_label_measured(
                card,
                right_x,
                cy + row_pad_y,
                right_max,
                &row.value,
                val_font,
                val_color,
                0,
                NS_TEXT_ALIGNMENT_RIGHT,
            );

            if has_dot {
                let dot_x = (width - pad_x - val_w - dot_d - 7.0).max(pad_x + left_max + 4.0);
                let dot_y = cy + row_pad_y + (val_h - dot_d) * 0.5;
                make_dot(card, dot_x, dot_y, dot_d, tone_fg(row.tone));
            }

            cy += title_h.max(val_h) + row_pad_y * 2.0;

            if idx + 1 < rows.len() {
                make_view(card, rect(pad_x, cy, inner_w, 1.0), col_divider());
                cy += 1.0;
            }
        }

        let card_h = cy + 4.0;
        msg_void_rect(card, c"setFrame:", rect(x, y, width, card_h));
        y + card_h + 20.0
    }
}

unsafe fn render_diagnostics_card(parent: Id, x: f64, y: f64, width: f64, state: &UiState) -> f64 {
    unsafe {
        let pad = 16.0;
        let inner_w = (width - pad * 2.0).max(120.0);
        let card = make_view(parent, rect(x, y, width, 100.0), col_diag_card());
        style_layer(card, 16.0, 1.0, Some(tone_border(state.diagnostics_tone)));

        let mut cy = pad;
        let dot_d = 8.0;
        let (_, _, title_h) = make_label_measured(
            card,
            pad + dot_d + 8.0,
            cy,
            inner_w - dot_d - 8.0,
            &state.diagnostics_title,
            system_font(15.0, FontWeight::Semibold),
            col_text_primary(),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        make_dot(
            card,
            pad,
            cy + (title_h - dot_d) * 0.5,
            dot_d,
            tone_fg(state.diagnostics_tone),
        );
        cy += title_h + 10.0;

        let (_, _, body_h) = make_label_measured(
            card,
            pad,
            cy,
            inner_w,
            &state.diagnostics_body,
            system_font(14.0, FontWeight::Regular),
            rgb(0.835, 0.859, 0.933),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        cy += body_h;

        if let Some(hint) = &state.setup_hint {
            cy += 14.0;
            make_view(card, rect(pad, cy, inner_w, 1.0), col_divider());
            cy += 12.0;

            let (_, _, h_title_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                "НАСТРОЙКА",
                system_font(11.5, FontWeight::Bold),
                tone_fg(StatusTone::Amber),
                1,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += h_title_h + 6.0;

            let (_, _, hint_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                hint,
                system_font(13.0, FontWeight::Regular),
                col_text_secondary(),
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += hint_h;
        }

        let card_h = cy + pad;
        msg_void_rect(card, c"setFrame:", rect(x, y, width, card_h));
        y + card_h + 28.0
    }
}

/// Rebuilds the scrollable card hierarchy on the main thread without holding any Mutex
/// during UIKit calls, guarded against re-entrant `viewDidLayoutSubviews` recursion.
fn perform_rebuild() {
    assert!(is_main_thread());
    if REBUILDING.swap(true, Ordering::AcqRel) {
        return;
    }

    struct RebuildGuard;
    impl Drop for RebuildGuard {
        fn drop(&mut self) {
            REBUILDING.store(false, Ordering::Release);
        }
    }
    let _guard = RebuildGuard;

    let snapshot = with_main_ui(|ui| {
        let controller = ui.controller.as_ref()?.as_id();
        let scroll_view = ui.scroll_view.as_ref()?.as_id();
        let content_view = ui.content_view.as_ref()?.as_id();
        Some(LayoutSnapshot {
            controller,
            scroll_view,
            content_view,
            state: ui.state.clone(),
            busy_operation: ui.busy_operation,
        })
    });
    let Some(snap) = snapshot else {
        return;
    };

    unsafe {
        let root_view = msg_id(snap.controller, c"view");
        if root_view.is_null() {
            return;
        }
        let bounds = msg_rect(root_view, c"bounds");
        let w = bounds.size.width;
        if w < 100.0 {
            return;
        }
        let safe = msg_insets(root_view, c"safeAreaInsets");

        msg_void_rect(snap.scroll_view, c"setFrame:", bounds);
        remove_all_subviews(snap.content_view);

        let side_pad = safe.left.max(safe.right).max(18.0);
        let col_w = (w - side_pad * 2.0).clamp(240.0, 520.0);
        let col_x = (w - col_w) * 0.5;

        // `UIScrollViewContentInsetAdjustmentAlways` shifts scroll origin below notch/Dynamic Island.
        let mut y = 16.0;

        // a) Header
        let (_, _, eyebrow_h) = make_label_measured(
            snap.content_view,
            col_x,
            y,
            col_w,
            "REBOX",
            system_font(12.5, FontWeight::Bold),
            col_accent(),
            1,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        y += eyebrow_h + 4.0;

        let (_, _, title_h) = make_label_measured(
            snap.content_view,
            col_x,
            y,
            col_w,
            "Автопродление подписей",
            system_font(27.0, FontWeight::Bold),
            col_text_primary(),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        y += title_h + 8.0;

        let subtitle_text = match snap.busy_operation {
            Some(Operation::Refresh) => "Продлеваем подписи через Apple…",
            Some(Operation::Status) => "Обновляем состояние подписей…",
            None => snap.state.header_subtitle.as_str(),
        };
        let sub_tone = if snap.busy_operation.is_some() {
            StatusTone::Neutral
        } else {
            snap.state.overall_tone
        };
        let dot_d = 8.0;
        let (_, _, sub_h) = make_label_measured(
            snap.content_view,
            col_x + dot_d + 8.0,
            y,
            col_w - dot_d - 8.0,
            subtitle_text,
            system_font(14.0, FontWeight::Medium),
            col_text_secondary(),
            0,
            NS_TEXT_ALIGNMENT_LEFT,
        );
        make_dot(
            snap.content_view,
            col_x,
            y + (sub_h - dot_d) * 0.5,
            dot_d,
            tone_fg(sub_tone),
        );
        y += sub_h + 24.0;

        // b) Tracked apps card(s)
        y = render_section_header(
            snap.content_view,
            col_x,
            y,
            col_w,
            "ОТСЛЕЖИВАЕМЫЕ ПРИЛОЖЕНИЯ",
        );
        if snap.state.apps.is_empty() {
            let pad = 16.0;
            let inner_w = col_w - pad * 2.0;
            let card = make_view(snap.content_view, rect(col_x, y, col_w, 80.0), col_card());
            style_layer(card, 16.0, 1.0, Some(col_border()));
            let mut cy = pad;
            let (_, _, t_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                &snap.state.empty_apps_title,
                system_font(16.0, FontWeight::Semibold),
                col_text_primary(),
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += t_h + 6.0;
            let (_, _, s_h) = make_label_measured(
                card,
                pad,
                cy,
                inner_w,
                &snap.state.empty_apps_subtitle,
                system_font(13.5, FontWeight::Regular),
                col_text_secondary(),
                0,
                NS_TEXT_ALIGNMENT_LEFT,
            );
            cy += s_h + pad;
            msg_void_rect(card, c"setFrame:", rect(col_x, y, col_w, cy));
            y += cy + 20.0;
        } else {
            for app in &snap.state.apps {
                y = render_app_card(snap.content_view, col_x, y, col_w, app);
            }
            y += 8.0;
        }

        // c) System readiness card
        y = render_section_header(snap.content_view, col_x, y, col_w, "ГОТОВНОСТЬ СИСТЕМЫ");
        y = render_readiness_card(snap.content_view, col_x, y, col_w, &snap.state.readiness);

        // d) Action buttons with busy state & spinning UIActivityIndicatorView
        let is_busy = snap.busy_operation.is_some();
        let refresh_busy = snap.busy_operation == Some(Operation::Refresh);
        let status_busy = snap.busy_operation == Some(Operation::Status);

        make_action_button(
            snap.content_view,
            snap.controller,
            rect(col_x, y, col_w, 54.0),
            if refresh_busy {
                "Продлеваем подписи…"
            } else {
                "Продлить сейчас"
            },
            c"refresh:",
            col_btn_primary(),
            None,
            col_text_primary(),
            17.0,
            !is_busy,
            if refresh_busy {
                0.85
            } else if is_busy {
                0.45
            } else {
                1.0
            },
            refresh_busy,
        );
        y += 54.0 + 10.0;

        make_action_button(
            snap.content_view,
            snap.controller,
            rect(col_x, y, col_w, 50.0),
            if status_busy {
                "Обновляем статус…"
            } else {
                "Обновить статус"
            },
            c"check:",
            col_btn_secondary(),
            Some(col_btn_secondary_border()),
            rgb(0.902, 0.922, 0.976),
            16.0,
            !is_busy,
            if status_busy {
                0.85
            } else if is_busy {
                0.45
            } else {
                1.0
            },
            status_busy,
        );
        y += 50.0 + 22.0;

        // e) Diagnostics / Result card
        y = render_section_header(snap.content_view, col_x, y, col_w, "ДИАГНОСТИКА");
        y = render_diagnostics_card(snap.content_view, col_x, y, col_w, &snap.state);

        msg_void_rect(snap.content_view, c"setFrame:", rect(0.0, 0.0, w, y));
        msg_void_size(snap.scroll_view, c"setContentSize:", size(w, y));

        with_main_ui(|ui| {
            ui.last_layout_width = w;
            ui.last_safe_lr = (safe.left, safe.right);
            ui.needs_rebuild = false;
        });
    }
}

// ---------------------------------------------------------------------------
// Async operation execution (background worker -> main-thread UI update)
// ---------------------------------------------------------------------------

fn start_operation(controller: Id, operation: Operation) {
    assert!(is_main_thread());
    if BUSY.swap(true, Ordering::AcqRel) {
        let rc = with_main_ui(|ui| ui.refresh_control.as_ref().map(OwnedId::as_id));
        if let Some(rc_id) = rc {
            unsafe {
                if msg_bool(rc_id, c"isRefreshing") {
                    msg_void(rc_id, c"endRefreshing");
                }
            }
        }
        return;
    }

    with_main_ui(|ui| {
        ui.busy_operation = Some(operation);
        ui.needs_rebuild = true;
    });
    perform_rebuild();

    // Retain `controller` explicitly for transfer to background thread.
    // The thread reconstructs `OwnedId::from_owned` so `release` is always balanced.
    let controller_addr = unsafe { OwnedId::retain(controller) }.into_raw_addr();

    std::thread::spawn(move || {
        let _pool = AutoreleasePool::new();
        let retained_controller = unsafe { OwnedId::from_owned(controller_addr as Id) };

        let new_state = execute_operation(operation, local_tz_offset_secs);
        {
            let mut slot = PENDING_RESULT.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(new_state);
        }

        // Dispatch UI mutation strictly onto the main thread.
        // `performSelectorOnMainThread:` retains the receiver until selector finishes.
        unsafe {
            msg_void_perform_main(
                retained_controller.as_id(),
                sel(c"applyPendingResult"),
                ptr::null_mut(),
                false,
            );
        }
        // `retained_controller` drops here (releasing our thread's +1), then `_pool` drains.
    });
}

// ---------------------------------------------------------------------------
// Objective-C method implementations
// ---------------------------------------------------------------------------

unsafe extern "C" fn delegate_window(_this: Id, _cmd: Sel) -> Id {
    assert!(is_main_thread());
    let guard = DELEGATE_WINDOW.0.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .map(OwnedId::as_id)
        .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn delegate_set_window(_this: Id, _cmd: Sel, window: Id) {
    assert!(is_main_thread());
    let new_owned = if window.is_null() {
        None
    } else {
        Some(unsafe { OwnedId::retain(window) })
    };
    let old = {
        let mut guard = DELEGATE_WINDOW.0.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, new_owned)
    };
    // Drop `old` outside the Mutex in case `release` triggers any UIKit teardown callback.
    drop(old);
}

unsafe extern "C" fn delegate_did_finish_launching(
    _this: Id,
    _cmd: Sel,
    _app: Id,
    _opts: Id,
) -> bool {
    unsafe {
        let screen = msg_id(class(c"UIScreen"), c"mainScreen");
        let bounds = msg_rect(screen, c"bounds");

        let window = alloc_init_frame(c"UIWindow", bounds);
        let window_id = window.as_id();
        msg_void_i64(
            window_id,
            c"setOverrideUserInterfaceStyle:",
            UI_USER_INTERFACE_STYLE_DARK,
        );
        msg_void_id(window_id, c"setBackgroundColor:", col_bg());

        let controller = alloc_init(c"ReboxController");
        let controller_id = controller.as_id();

        {
            let mut guard = DELEGATE_WINDOW.0.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(window);
        }
        with_main_ui(|ui| {
            ui.controller = Some(controller);
        });

        msg_void_id(window_id, c"setRootViewController:", controller_id);
        msg_void(window_id, c"makeKeyAndVisible");

        start_operation(controller_id, Operation::Status);
    }
    true
}

unsafe extern "C" fn controller_preferred_status_bar_style(_this: Id, _cmd: Sel) -> i64 {
    UI_STATUS_BAR_STYLE_LIGHT_CONTENT
}

unsafe extern "C" fn controller_view_did_load(this: Id, _cmd: Sel) {
    unsafe {
        msg_super_void(this, class(c"UIViewController"), c"viewDidLoad");

        msg_void_i64(
            this,
            c"setOverrideUserInterfaceStyle:",
            UI_USER_INTERFACE_STYLE_DARK,
        );

        let root_view = msg_id(this, c"view");
        msg_void_id(root_view, c"setBackgroundColor:", col_bg());

        let bounds = msg_rect(root_view, c"bounds");
        let scroll = alloc_init_frame(c"UIScrollView", bounds);
        let scroll_id = scroll.as_id();
        msg_void_id(scroll_id, c"setBackgroundColor:", col_bg());
        msg_void_bool(scroll_id, c"setAlwaysBounceVertical:", true);
        msg_void_bool(scroll_id, c"setShowsVerticalScrollIndicator:", true);
        msg_void_i64(
            scroll_id,
            c"setIndicatorStyle:",
            UI_SCROLL_INDICATOR_STYLE_WHITE,
        );
        msg_void_i64(
            scroll_id,
            c"setContentInsetAdjustmentBehavior:",
            UI_CONTENT_INSET_ADJUSTMENT_ALWAYS,
        );
        msg_void_id(root_view, c"addSubview:", scroll_id);

        let content = alloc_init_frame(c"UIView", rect(0.0, 0.0, bounds.size.width, 0.0));
        let content_id = content.as_id();
        msg_void_id(content_id, c"setBackgroundColor:", rgba(0.0, 0.0, 0.0, 0.0));
        msg_void_id(scroll_id, c"addSubview:", content_id);

        let refresh_ctl = alloc_init(c"UIRefreshControl");
        let rc_id = refresh_ctl.as_id();
        msg_void_id(rc_id, c"setTintColor:", col_accent());
        msg_void_target_action(
            rc_id,
            c"addTarget:action:forControlEvents:",
            this,
            sel(c"handlePullToRefresh:"),
            UI_CONTROL_EVENT_VALUE_CHANGED,
        );
        msg_void_id(scroll_id, c"setRefreshControl:", rc_id);

        with_main_ui(|ui| {
            ui.scroll_view = Some(scroll);
            ui.content_view = Some(content);
            ui.refresh_control = Some(refresh_ctl);
            ui.needs_rebuild = true;
        });
    }
}

unsafe extern "C" fn controller_view_did_layout_subviews(this: Id, _cmd: Sel) {
    unsafe {
        msg_super_void(this, class(c"UIViewController"), c"viewDidLayoutSubviews");
        if REBUILDING.load(Ordering::Acquire) {
            return;
        }
        let root_view = msg_id(this, c"view");
        if root_view.is_null() {
            return;
        }
        let bounds = msg_rect(root_view, c"bounds");
        let safe = msg_insets(root_view, c"safeAreaInsets");

        let (scroll_id, should_rebuild) = with_main_ui(|ui| {
            let sv = ui.scroll_view.as_ref().map(OwnedId::as_id);
            let width_changed = (bounds.size.width - ui.last_layout_width).abs() > 0.5;
            let insets_changed = (safe.left - ui.last_safe_lr.0).abs() > 0.5
                || (safe.right - ui.last_safe_lr.1).abs() > 0.5;
            (sv, ui.needs_rebuild || width_changed || insets_changed)
        });

        if let Some(sv) = scroll_id {
            msg_void_rect(sv, c"setFrame:", bounds);
        }
        if should_rebuild {
            perform_rebuild();
        }
    }
}

unsafe extern "C" fn controller_safe_area_did_change(this: Id, _cmd: Sel) {
    unsafe {
        msg_super_void(
            this,
            class(c"UIViewController"),
            c"viewSafeAreaInsetsDidChange",
        );
    }
    if REBUILDING.load(Ordering::Acquire) {
        return;
    }
    with_main_ui(|ui| {
        ui.needs_rebuild = true;
    });
    perform_rebuild();
}

unsafe extern "C" fn controller_refresh(this: Id, _cmd: Sel, _sender: Id) {
    start_operation(this, Operation::Refresh);
}

unsafe extern "C" fn controller_check(this: Id, _cmd: Sel, _sender: Id) {
    start_operation(this, Operation::Status);
}

unsafe extern "C" fn controller_pull_to_refresh(this: Id, _cmd: Sel, _sender: Id) {
    start_operation(this, Operation::Status);
}

unsafe extern "C" fn controller_apply_pending_result(_this: Id, _cmd: Sel) {
    let incoming = {
        let mut slot = PENDING_RESULT.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };

    let rc = with_main_ui(|ui| {
        if let Some(new_state) = incoming {
            ui.state = new_state;
        }
        ui.busy_operation = None;
        ui.needs_rebuild = true;
        ui.refresh_control.as_ref().map(OwnedId::as_id)
    });

    BUSY.store(false, Ordering::Release);

    if let Some(rc_id) = rc {
        unsafe {
            if msg_bool(rc_id, c"isRefreshing") {
                msg_void(rc_id, c"endRefreshing");
            }
        }
    }
    perform_rebuild();
}

// ---------------------------------------------------------------------------
// Application entry point
// ---------------------------------------------------------------------------

pub fn run() {
    let _pool = AutoreleasePool::new();
    unsafe {
        let delegate_cls = allocate_class_pair(class(c"UIResponder"), c"ReboxDelegate");
        add_method(
            delegate_cls,
            c"application:didFinishLaunchingWithOptions:",
            delegate_did_finish_launching as *const c_void,
            c"B@:@@",
        );
        add_method(
            delegate_cls,
            c"window",
            delegate_window as *const c_void,
            c"@@:",
        );
        add_method(
            delegate_cls,
            c"setWindow:",
            delegate_set_window as *const c_void,
            c"v@:@",
        );
        register_class_pair(delegate_cls);

        let controller_cls = allocate_class_pair(class(c"UIViewController"), c"ReboxController");
        add_method(
            controller_cls,
            c"preferredStatusBarStyle",
            controller_preferred_status_bar_style as *const c_void,
            c"q@:",
        );
        add_method(
            controller_cls,
            c"viewDidLoad",
            controller_view_did_load as *const c_void,
            c"v@:",
        );
        add_method(
            controller_cls,
            c"viewDidLayoutSubviews",
            controller_view_did_layout_subviews as *const c_void,
            c"v@:",
        );
        add_method(
            controller_cls,
            c"viewSafeAreaInsetsDidChange",
            controller_safe_area_did_change as *const c_void,
            c"v@:",
        );
        add_method(
            controller_cls,
            c"refresh:",
            controller_refresh as *const c_void,
            c"v@:@",
        );
        add_method(
            controller_cls,
            c"check:",
            controller_check as *const c_void,
            c"v@:@",
        );
        add_method(
            controller_cls,
            c"handlePullToRefresh:",
            controller_pull_to_refresh as *const c_void,
            c"v@:@",
        );
        add_method(
            controller_cls,
            c"applyPendingResult",
            controller_apply_pending_result as *const c_void,
            c"v@:",
        );
        register_class_pair(controller_cls);

        let delegate_name = ns_string("ReboxDelegate");
        UIApplicationMain(0, ptr::null(), ptr::null_mut(), delegate_name);
    }
}
