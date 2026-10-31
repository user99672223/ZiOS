//! iOS Window implementation using UIWindow and UIViewController.
//!
//! iOS windows are fundamentally different from desktop windows:
//! - Always fullscreen (or split-screen on iPad)
//! - No title bar or window chrome
//! - Touch-based input
//! - Safe area insets for notch/home indicator
//!
//! The window is backed by a UIWindow containing a UIViewController
//! whose view hosts a CAMetalLayer.

use super::events::*;
use super::text_input::TextInputView;
use super::{CallbackSlot, IosDisplay, platform::IosPlatformState};
use crate::keys::{KeyInput, KeyRepeat, key_input};
use gpui::{
    AnyWindowHandle, BackGestureEvent, BackGesturePhase, Bounds, Capslock, Corners, DevicePixels,
    DispatchEventResult, DisplayId, Edges, GpuSpecs, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, ModifiersChangedEvent, Pixels, PlatformAtlas,
    PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, Scene, Size, TextInputConfiguration, TextInputStateChange,
    TouchEvent, TouchId, TouchPhase, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowInsets, WindowParams, WindowVisibility, px, size,
};
use gpui_apple::metal_renderer::{Context as MetalContext, MetalRenderer};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
    sel,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSNotification, NSNotificationCenter, NSObjectProtocol, NSSet, NSValue,
};
use objc2_quartz_core::CAMetalLayer;
use objc2_ui_kit::{
    NSValueUIGeometryExtensions, UIKeyboardFrameEndUserInfoKey,
    UIKeyboardWillChangeFrameNotification, UIKeyboardWillHideNotification, UIStatusBarStyle,
};
use objc2_ui_kit::{
    UICornerConfiguration, UICornerRadius, UIEvent, UIGestureRecognizer,
    UIGestureRecognizerDelegate, UIPanGestureRecognizer, UIRectCorner, UIRectEdge, UIScreen,
    UIPress, UIPressesEvent, UIScreenEdgePanGestureRecognizer, UIScrollTypeMask, UITextRange,
    UITouch, UITraitCollection,
    UITraitEnvironment, UIUserInterfaceLayoutDirection, UIUserInterfaceStyle, UIView,
    UIViewAutoresizing, UIViewController, UIWindow, UIWindowScene,
};
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, UiKitDisplayHandle, UiKitWindowHandle};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::NonNull,
    rc::{Rc, Weak},
    sync::Arc,
};

const KEYBOARD_DISMISS_DISTANCE: Pixels = px(24.);

#[derive(Clone, Copy)]
struct KeyboardDismissTouch {
    id: TouchId,
    start_position: Point<Pixels>,
}

/// Global storage for the current status bar style.
/// 0 = default (dark content), 1 = light content.
/// Accessed from the main thread only.
static STATUS_BAR_STYLE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[derive(Default)]
pub(super) struct WindowReference(pub(super) RefCell<Weak<IosWindowState>>);

impl WindowReference {
    pub(super) fn with_window<R>(&self, callback: impl FnOnce(&IosWindowState) -> R) -> Option<R> {
        let window = self.0.borrow().upgrade()?;
        // Keep callback storage and native objects alive even if the callback
        // synchronously closes the platform window.
        Some(callback(&window))
    }
}

define_class!(
    #[unsafe(super = UIViewController)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIViewController"]
    #[ivars = WindowReference]
    struct ViewController;

    impl ViewController {
        #[unsafe(method(preferredStatusBarStyle))]
        fn preferred_status_bar_style(&self) -> UIStatusBarStyle {
            let style = STATUS_BAR_STYLE.load(std::sync::atomic::Ordering::Relaxed);
            if style == 1 {
                UIStatusBarStyle::LightContent
            } else {
                UIStatusBarStyle::DarkContent
            }
        }

        #[unsafe(method(viewDidLayoutSubviews))]
        fn view_did_layout_subviews(&self) {
            unsafe {
                let _: () = msg_send![super(self), viewDidLayoutSubviews];
            }
            self.ivars().with_window(IosWindowState::handle_layout_change);
        }
    }
);

impl ViewController {
    fn new(main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(WindowReference::default());
        unsafe { msg_send![super(this), init] }
    }
}

/// Set the iOS status bar content style (light or dark text/icons).
///
/// This updates the stored style and asks the root view controller
/// to re-query `preferredStatusBarStyle`.
pub fn set_status_bar_style(style: crate::StatusBarContentStyle) {
    use crate::StatusBarContentStyle;

    let value = match style {
        StatusBarContentStyle::Light => 1,
        StatusBarContentStyle::Dark => 0,
    };
    STATUS_BAR_STYLE.store(value, std::sync::atomic::Ordering::Relaxed);

    // This public free function predates explicit platform ownership.
    // Keep its single-application behavior without making runtime events global.
    if let Some(platform) = super::application::current_platform() {
        platform.application.with_windows(|window| {
            window.view_controller.setNeedsStatusBarAppearanceUpdate();
        });
    }
}

define_class!(
    #[unsafe(super = UIView)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIMetalView"]
    #[ivars = WindowReference]
    struct MetalView;

    unsafe impl NSObjectProtocol for MetalView {}

    impl MetalView {
        #[unsafe(method(layerClass))]
        fn layer_class() -> &'static AnyClass {
            CAMetalLayer::class()
        }

        // This callback also covers iOS 15/16, before UIKit's trait-registration API.
        #[unsafe(method(traitCollectionDidChange:))]
        fn trait_collection_did_change(&self, previous: Option<&UITraitCollection>) {
            unsafe {
                let _: () = msg_send![super(self), traitCollectionDidChange: previous];
                if self
                    .traitCollection()
                    .hasDifferentColorAppearanceComparedToTraitCollection(previous)
                {
                    self.ivars()
                        .with_window(IosWindowState::notify_appearance_changed);
                }
            }
        }

        #[unsafe(method(touchesBegan:withEvent:))]
        fn touches_began(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesMoved:withEvent:))]
        fn touches_moved(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesEnded:withEvent:))]
        fn touches_ended(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        #[unsafe(method(touchesCancelled:withEvent:))]
        fn touches_cancelled(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event);
        }

        // Hardware-keyboard presses arrive at the first responder. The Metal view
        // holds that role whenever the hidden text input view does not.
        #[unsafe(method(canBecomeFirstResponder))]
        fn can_become_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(pressesBegan:withEvent:))]
        fn presses_began(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            let forwarded = self.ivars().with_window(|window| {
                window.handle_presses(presses, PressPhase::Began, false)
            });
            let forwarded = forwarded.unwrap_or_else(|| presses.retain());
            if !forwarded.is_empty() {
                unsafe {
                    let _: () = msg_send![super(self), pressesBegan: &*forwarded, withEvent: event];
                }
            }
        }

        #[unsafe(method(pressesEnded:withEvent:))]
        fn presses_ended(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            let forwarded = self.ivars().with_window(|window| {
                window.handle_presses(presses, PressPhase::Ended, false)
            });
            let forwarded = forwarded.unwrap_or_else(|| presses.retain());
            if !forwarded.is_empty() {
                unsafe {
                    let _: () = msg_send![super(self), pressesEnded: &*forwarded, withEvent: event];
                }
            }
        }

        #[unsafe(method(pressesCancelled:withEvent:))]
        fn presses_cancelled(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            let forwarded = self.ivars().with_window(|window| {
                window.handle_presses(presses, PressPhase::Ended, false)
            });
            let forwarded = forwarded.unwrap_or_else(|| presses.retain());
            if !forwarded.is_empty() {
                unsafe {
                    let _: () = msg_send![super(self), pressesCancelled: &*forwarded, withEvent: event];
                }
            }
        }

        #[unsafe(method(handleScroll:))]
        fn handle_scroll(&self, recognizer: &UIPanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_scroll(recognizer));
        }

        #[unsafe(method(handleBackGesture:))]
        fn handle_back_gesture(&self, recognizer: &UIScreenEdgePanGestureRecognizer) {
            self.ivars()
                .with_window(|window| window.handle_back_gesture(recognizer));
        }
    }

    unsafe impl UIGestureRecognizerDelegate for MetalView {
        // Only the back-gesture recognizer uses this view as its delegate.
        #[unsafe(method(gestureRecognizerShouldBegin:))]
        fn gesture_recognizer_should_begin(&self, recognizer: &UIGestureRecognizer) -> bool {
            self.ivars()
                .with_window(|window| window.begin_back_gesture(recognizer))
                .unwrap_or(false)
        }
    }
);

impl MetalView {
    fn new(frame: CGRect, main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(WindowReference::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn handle_touches(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
        for touch in touches {
            self.ivars()
                .with_window(|window| window.handle_touch(&touch, event));
        }
    }
}

pub(crate) struct IosWindow {
    state: Rc<IosWindowState>,
}

impl std::ops::Deref for IosWindow {
    type Target = IosWindowState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[allow(clippy::type_complexity)]
pub(crate) struct IosWindowState {
    platform: Weak<IosPlatformState>,
    keyboard_observers:
        RefCell<Vec<Retained<objc2::runtime::ProtocolObject<dyn NSObjectProtocol>>>>,
    /// The UIWindow object
    window: Retained<UIWindow>,
    /// The UIViewController
    view_controller: Retained<ViewController>,
    /// The Metal-backed UIView
    view: Retained<MetalView>,
    /// The hidden text input view for keyboard input
    text_input_view: Retained<TextInputView>,
    /// Current bounds in pixels
    bounds: Cell<Bounds<Pixels>>,
    /// Scale factor
    scale_factor: Cell<f32>,
    /// Input handler for text input
    input_handler: CallbackSlot<PlatformInputHandler>,
    request_frame_callback: CallbackSlot<Box<dyn FnMut(RequestFrameOptions)>>,
    force_next_frame: Cell<bool>,
    /// Callback for input events
    input_callback: CallbackSlot<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    /// Callback for active status changes
    active_status_callback: CallbackSlot<Box<dyn FnMut(bool)>>,
    visibility: Cell<WindowVisibility>,
    visibility_callback: CallbackSlot<Box<dyn FnMut(WindowVisibility)>>,
    /// Callback for hover status changes (not really applicable on iOS)
    hover_status_callback: CallbackSlot<Box<dyn FnMut(bool)>>,
    /// Callback for resize events
    resize_callback: CallbackSlot<Box<dyn FnMut(Size<Pixels>, f32)>>,
    display_id: Cell<DisplayId>,
    display_changed_callback: CallbackSlot<Box<dyn FnMut()>>,
    /// Callback for move events (not applicable on iOS)
    moved_callback: CallbackSlot<Box<dyn FnMut()>>,
    /// Callback for should close
    should_close_callback: CallbackSlot<Box<dyn FnMut() -> bool>>,
    /// Callback for hit test
    hit_test_callback: CallbackSlot<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    /// Callback for close
    close_callback: CallbackSlot<Box<dyn FnOnce()>>,
    /// Callback for appearance changes
    appearance_changed_callback: CallbackSlot<Box<dyn FnMut()>>,
    insets_changed_callback: CallbackSlot<Box<dyn FnMut(WindowInsets)>>,
    keyboard_dismiss_touch: Cell<Option<KeyboardDismissTouch>>,
    /// Where the back gesture an element claimed started, while it runs.
    back_gesture_start: Cell<Option<Point<Pixels>>>,
    keyboard_height: Cell<f32>,
    /// Current mouse position (from touch)
    mouse_position: Cell<Point<Pixels>>,
    /// Current modifiers
    modifiers: Cell<Modifiers>,
    capslock: Cell<Capslock>,
    /// The hardware key GPUI is repeating, if any.
    key_repeat: RefCell<Option<KeyRepeat>>,
    /// HID usages of presses whose `pressesBegan` was forwarded to UIKit; only
    /// those get their end forwarded too.
    forwarded_presses: RefCell<Vec<isize>>,
    /// Pointer drawn over the editor while a remote mouse (lan-mouse) drives it.
    pointer: RefCell<Option<PointerOverlay>>,
    /// Points per GPUI logical pixel (1 on the phone).
    ui_scale_cell: Cell<f32>,
    renderer: Mutex<MetalRenderer>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PressPhase {
    Began,
    Ended,
}

impl IosWindow {
    #[allow(deprecated)] // Window construction can precede scene connection.
    pub fn new(
        _handle: AnyWindowHandle,
        _params: WindowParams,
        platform: &Rc<IosPlatformState>,
    ) -> anyhow::Result<Self> {
        // Create the window on the main screen
        let screen = IosDisplay::main();
        let screen_bounds = screen.bounds();
        let scale_factor = screen.scale();

        let main_thread = MainThreadMarker::new().expect("UIKit requires the main thread");
        // Create UIWindow
        let window_scene = platform.application.window_scene();
        let window_scene = window_scene.as_deref();
        let (window, screen_bounds_cg, scale) = if let Some(scene) = window_scene {
            let screen = scene.screen();
            let bounds = screen.bounds();
            let window = UIWindow::initWithWindowScene(UIWindow::alloc(main_thread), scene);
            window.setFrame(bounds);
            (window, bounds, screen.scale())
        } else {
            let screen = UIScreen::mainScreen(main_thread);
            let bounds = screen.bounds();
            let window = UIWindow::initWithFrame(UIWindow::alloc(main_thread), bounds);
            (window, bounds, screen.scale())
        };

        let metal_frame = CGRect::new(CGPoint::ZERO, screen_bounds_cg.size);
        let view = MetalView::new(metal_frame, main_thread);

        let layer = view.layer();
        layer.setContentsScale(scale);

        // Auto-resize the Metal view when the parent view changes size
        // (e.g. rotation). UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight
        view.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
        );

        // Enable user interaction on the Metal view for touch handling
        view.setUserInteractionEnabled(true);
        view.setMultipleTouchEnabled(true);

        // UIKit has no public API for the display's corner radius, but since
        // iOS 26 a view filling the window with a container-concentric corner
        // configuration resolves its radii to the display's. This only sets the
        // layer's corner radius; without `masksToBounds` the Metal drawable is
        // not clipped.
        if reports_corner_radii(&view) {
            view.setCornerConfiguration(&UICornerConfiguration::configurationWithRadius(
                &UICornerRadius::containerConcentricRadius(),
            ));
        }

        // Trackpad and mouse-wheel scrolling reaches UIKit apps only through a
        // pan recognizer that accepts scroll types. With no allowed touch types
        // it never claims finger touches, so those keep reaching `touchesBegan`
        // and GPUI's own recognizer.
        let scroll_target: &AnyObject = &view;
        // SAFETY: `MetalView` implements `handleScroll:` and owns the recognizer.
        let scroll_recognizer = unsafe {
            UIPanGestureRecognizer::initWithTarget_action(
                UIPanGestureRecognizer::alloc(main_thread),
                Some(scroll_target),
                Some(sel!(handleScroll:)),
            )
        };
        scroll_recognizer.setAllowedScrollTypesMask(UIScrollTypeMask::All);
        scroll_recognizer.setAllowedTouchTypes(&NSArray::new());
        scroll_recognizer.setCancelsTouchesInView(false);
        view.addGestureRecognizer(&scroll_recognizer);

        // The leading-edge swipe is iOS's back gesture. The delegate lets it
        // begin only when an element claims it; it then cancels the touches it
        // took over, so GPUI's own recognizer abandons any pan it had started.
        let back_target: &AnyObject = &view;
        // SAFETY: `MetalView` implements `handleBackGesture:` and owns the recognizer.
        let back_recognizer = unsafe {
            UIScreenEdgePanGestureRecognizer::initWithTarget_action(
                UIScreenEdgePanGestureRecognizer::alloc(main_thread),
                Some(back_target),
                Some(sel!(handleBackGesture:)),
            )
        };
        back_recognizer.setEdges(
            if view.effectiveUserInterfaceLayoutDirection()
                == UIUserInterfaceLayoutDirection::RightToLeft
            {
                UIRectEdge::Right
            } else {
                UIRectEdge::Left
            },
        );
        back_recognizer.setDelegate(Some(ProtocolObject::from_ref(&*view)));
        view.addGestureRecognizer(&back_recognizer);

        let view_controller = ViewController::new(main_thread);
        view_controller.setView(Some(&view));
        window.setRootViewController(Some(&view_controller));

        // Make the window visible
        window.makeKeyAndVisible();

        // Create a hidden text input view for keyboard handling.
        // Uses our custom GPUITextInputView which implements UIKeyInput
        // so iOS actually routes keyboard text to us.
        let text_input_frame = CGRect::new(CGPoint::ZERO, CGSize::new(1.0, 1.0));
        let text_input_view = TextInputView::new(text_input_frame, main_thread);
        text_input_view.setAlpha(0.01);
        text_input_view.setUserInteractionEnabled(true);
        view.addSubview(&text_input_view);

        let pixel_w = (screen_bounds_cg.size.width * scale) as i32;
        let pixel_h = (screen_bounds_cg.size.height * scale) as i32;
        let mut renderer = MetalRenderer::from_layer(
            MetalContext::default(),
            layer
                .downcast_ref::<CAMetalLayer>()
                .ok_or_else(|| anyhow::anyhow!("UIKit view must have a CAMetalLayer"))?,
            false,
        );
        renderer.update_drawable_size(size(DevicePixels(pixel_w), DevicePixels(pixel_h)));

        let state = IosWindowState {
            platform: Rc::downgrade(platform),
            keyboard_observers: RefCell::default(),
            display_id: Cell::new(IosDisplay::from_screen(window.screen()).id()),
            display_changed_callback: CallbackSlot::default(),
            window,
            view_controller,
            view,
            text_input_view,
            bounds: Cell::new(screen_bounds),
            scale_factor: Cell::new(scale_factor),
            input_handler: CallbackSlot::default(),
            request_frame_callback: CallbackSlot::default(),
            force_next_frame: Cell::new(true),
            input_callback: CallbackSlot::default(),
            active_status_callback: CallbackSlot::default(),
            visibility: Cell::new(WindowVisibility::Visible),
            visibility_callback: CallbackSlot::default(),
            hover_status_callback: CallbackSlot::default(),
            resize_callback: CallbackSlot::default(),
            moved_callback: CallbackSlot::default(),
            should_close_callback: CallbackSlot::default(),
            hit_test_callback: CallbackSlot::default(),
            close_callback: CallbackSlot::default(),
            appearance_changed_callback: CallbackSlot::default(),
            insets_changed_callback: CallbackSlot::default(),
            keyboard_dismiss_touch: Cell::new(None),
            back_gesture_start: Cell::new(None),
            keyboard_height: Cell::new(0.),
            mouse_position: Cell::new(Point::default()),
            modifiers: Cell::new(Modifiers::default()),
            capslock: Cell::new(Capslock::default()),
            key_repeat: RefCell::new(None),
            forwarded_presses: RefCell::new(Vec::new()),
            pointer: RefCell::new(None),
            ui_scale_cell: Cell::new(1.0),
            renderer: Mutex::new(renderer),
        };

        Ok(Self {
            state: Rc::new(state),
        })
    }

    pub(crate) fn register(&self) {
        if let Some(platform) = self.platform.upgrade() {
            platform.application.register_window(&self.state);
        }

        let window = Rc::downgrade(&self.state);
        *self.view_controller.ivars().0.borrow_mut() = window.clone();
        *self.view.ivars().0.borrow_mut() = window.clone();
        self.text_input_view.set_window(window);

        self.state.register_keyboard_observers();
        self.state.claim_key_focus();
    }
}

impl IosWindowState {
    pub(super) fn attach_to_scene(&self, scene: &UIWindowScene) {
        self.window.setWindowScene(Some(scene));
        // A window keeps its frame when it changes scenes; fill the new screen.
        self.window.setFrame(scene.screen().bounds());
        self.window.makeKeyAndVisible();
        self.handle_layout_change();
    }

    pub(super) fn is_on_scene(&self, scene: &UIWindowScene) -> bool {
        self.window
            .windowScene()
            .is_some_and(|current| std::ptr::eq::<UIWindowScene>(&*current, scene))
    }

    /// The UI scale for the screen this window is on. The phone always uses 1;
    /// on a monitor it is the user's choice, or chosen from the resolution.
    fn ui_scale(&self) -> f32 {
        let screen = self.window.screen();
        let platform = self.platform.upgrade();
        let phone_screen = platform
            .as_ref()
            .and_then(|platform| platform.application.window_scene())
            .map(|scene| scene.screen());
        if phone_screen.is_none_or(|phone| std::ptr::eq::<UIScreen>(&*screen, &*phone)) {
            return 1.0;
        }
        let configured = platform.and_then(|platform| platform.application.external_ui_scale.get());
        configured.unwrap_or_else(|| {
            // A 4K monitor reported at 1x would make everything tiny.
            let native_height = screen.nativeBounds().size.height;
            let scale = screen.scale();
            if native_height / scale >= 2000.0 { 2.0 } else { 1.0 }
        })
    }

    fn register_keyboard_observers(self: &Rc<Self>) {
        unsafe {
            let notification_center = NSNotificationCenter::defaultCenter();

            let window = Rc::downgrade(self);
            let frame_change_block =
                block2::RcBlock::new(move |notification: NonNull<NSNotification>| {
                    let Some(user_info) = notification.as_ref().userInfo() else {
                        return;
                    };
                    let Some(frame_value) = user_info.objectForKey(UIKeyboardFrameEndUserInfoKey)
                    else {
                        return;
                    };
                    let Some(frame_value) = frame_value.downcast_ref::<NSValue>() else {
                        return;
                    };
                    let frame = frame_value.CGRectValue();
                    if let Some(window) = window.upgrade() {
                        window.set_keyboard_height(frame.size.height as f32);
                    }
                });

            let window = Rc::downgrade(self);
            let hide_block = block2::RcBlock::new(move |_notification: NonNull<NSNotification>| {
                if let Some(window) = window.upgrade() {
                    window.set_keyboard_height(0.);
                }
            });

            let frame_observer = notification_center.addObserverForName_object_queue_usingBlock(
                Some(UIKeyboardWillChangeFrameNotification),
                None,
                None,
                &frame_change_block,
            );
            let hide_observer = notification_center.addObserverForName_object_queue_usingBlock(
                Some(UIKeyboardWillHideNotification),
                None,
                None,
                &hide_block,
            );
            self.keyboard_observers
                .borrow_mut()
                .extend([frame_observer, hide_observer]);
        }
    }

    /// Delivers a UIKit touch through GPUI's platform-neutral touch API.
    pub fn handle_touch(&self, touch: &UITouch, _event: Option<&UIEvent>) {
        // GPUI timestamps delivery itself. Replaying UIKit's historical coalesced
        // samples here would make them appear simultaneous and distort fling velocity.
        let id = touch_id(touch);
        self.handle_touch_sample(touch, id);
    }

    fn handle_touch_sample(&self, touch: &UITouch, id: TouchId) {
        let position = touch_location_in_view(touch, &self.view) / self.ui_scale_cell.get();
        self.mouse_position.set(position);

        let event = TouchEvent {
            id,
            phase: touch_phase(touch),
            position,
            predicted_position: None,
            force: touch_force(touch),
        };
        self.handle_keyboard_dismiss_touch(&event);
        self.input_callback
            .with(|callback| callback(PlatformInput::Touch(event)));
    }

    /// Delivers one step of a trackpad or mouse-wheel scroll gesture.
    fn handle_scroll(&self, recognizer: &UIPanGestureRecognizer) {
        let Some(mut event) = scroll_event(recognizer, &self.view) else {
            return;
        };
        event.position = event.position / self.ui_scale_cell.get();
        self.mouse_position.set(event.position);
        self.modifiers.set(event.modifiers);
        self.input_callback
            .with(|callback| callback(PlatformInput::Scroll(event)));
    }

    /// Offers a starting back gesture to GPUI, returning whether an element
    /// claimed it.
    fn begin_back_gesture(&self, recognizer: &UIGestureRecognizer) -> bool {
        let location = recognizer.locationInView(Some(&self.view));
        let start_position = Point::new(px(location.x as f32), px(location.y as f32));
        let claimed = self
            .input_callback
            .with(|callback| {
                callback(PlatformInput::BackGesture(BackGestureEvent {
                    phase: BackGesturePhase::Started,
                    start_position,
                    progress: 0.,
                }))
            })
            .is_some_and(|result| result.default_prevented);
        self.back_gesture_start
            .set(claimed.then_some(start_position));
        claimed
    }

    /// Delivers a step of a claimed back gesture.
    fn handle_back_gesture(&self, recognizer: &UIScreenEdgePanGestureRecognizer) {
        let Some(start_position) = self.back_gesture_start.get() else {
            return;
        };
        let Some(event) = back_event(recognizer, &self.view, start_position) else {
            return;
        };
        if matches!(
            event.phase,
            BackGesturePhase::Committed | BackGesturePhase::Cancelled
        ) {
            self.back_gesture_start.set(None);
        }
        self.input_callback
            .with(|callback| callback(PlatformInput::BackGesture(event)));
    }

    fn handle_keyboard_dismiss_touch(&self, event: &TouchEvent) {
        if self.keyboard_height.get() <= 0. {
            self.keyboard_dismiss_touch.set(None);
            return;
        }

        match event.phase {
            TouchPhase::Started if self.keyboard_dismiss_touch.get().is_none() => {
                self.keyboard_dismiss_touch.set(Some(KeyboardDismissTouch {
                    id: event.id,
                    start_position: event.position,
                }));
            }
            TouchPhase::Moved => {
                let Some(touch) = self
                    .keyboard_dismiss_touch
                    .get()
                    .filter(|touch| touch.id == event.id)
                else {
                    return;
                };
                let delta = event.position - touch.start_position;
                if delta.y >= KEYBOARD_DISMISS_DISTANCE && delta.y.abs() > delta.x.abs() * 1.25 {
                    self.keyboard_dismiss_touch.set(None);
                    self.hide_keyboard();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if self
                    .keyboard_dismiss_touch
                    .get()
                    .is_some_and(|touch| touch.id == event.id)
                {
                    self.keyboard_dismiss_touch.set(None);
                }
            }
            TouchPhase::Started => {}
        }
    }

    pub(super) fn request_frame(&self) {
        self.claim_key_focus();
        self.repeat_held_key();
        self.hide_idle_pointer();
        self.request_frame_callback.with(|callback| {
            let force_render = self.force_next_frame.replace(false);
            callback(RequestFrameOptions {
                force_render,
                ..Default::default()
            });
        });
    }

    /// Keeps a first responder for hardware keys: the text input view while it
    /// edits, otherwise the Metal view. Nothing is claimed while UIKit presents
    /// its own controller (Safari, document picker), which needs the keyboard.
    fn claim_key_focus(&self) {
        if !self.window.isKeyWindow()
            || self.view_controller.presentedViewController().is_some()
            || self.text_input_view.isFirstResponder()
            || self.view.isFirstResponder()
        {
            return;
        }
        self.view.becomeFirstResponder();
    }

    /// Handles hardware-key presses from the first responder and returns the
    /// presses UIKit should still process (pass to `super`).
    ///
    /// GPUI sees every key first, as on macOS. A key GPUI does not handle that
    /// produces text goes to UIKit's text system when the text input view is the
    /// responder (so IME and dead keys work), and is inserted directly otherwise.
    pub(super) fn handle_presses(
        &self,
        presses: &NSSet<UIPress>,
        phase: PressPhase,
        from_text_input: bool,
    ) -> Retained<NSSet<UIPress>> {
        let main_thread = MainThreadMarker::from(&*self.view);
        let mut forwarded = Vec::new();
        for press in presses {
            let Some(key) = press.key(main_thread) else {
                // Not a keyboard press (for example, a remote or game controller).
                forwarded.push(press);
                continue;
            };
            let raw = raw_key(&key);
            let forward = match phase {
                PressPhase::Began => {
                    let forward = self.key_began(&raw, from_text_input);
                    if forward {
                        self.forwarded_presses.borrow_mut().push(raw.hid_usage);
                    }
                    forward
                }
                PressPhase::Ended => {
                    self.key_ended(&raw);
                    let mut forwarded_presses = self.forwarded_presses.borrow_mut();
                    let position = forwarded_presses.iter().position(|usage| *usage == raw.hid_usage);
                    position.map(|index| forwarded_presses.swap_remove(index)).is_some()
                }
            };
            if forward {
                forwarded.push(press);
            }
        }
        NSSet::from_retained_slice(&forwarded)
    }

    fn key_began(&self, raw: &crate::keys::RawKey, from_text_input: bool) -> bool {
        self.set_modifiers(raw.modifiers, Capslock { on: raw.capslock });
        let keystroke = match key_input(raw) {
            KeyInput::Modifiers(..) => return true,
            KeyInput::Key(keystroke) => keystroke,
        };
        // While the IME composes, keys belong to it.
        if from_text_input && self.text_input_has_marked_text() {
            return true;
        }
        self.key_repeat.borrow_mut().take();

        let handled = self.dispatch_key_down(&keystroke, false);
        let forward = !handled && from_text_input && keystroke.key_char.is_some();
        if !handled && !forward {
            self.insert_key_char(&keystroke);
        }
        // UIKit repeats what reaches its text system; GPUI repeats the rest.
        if !forward && KeyRepeat::repeats(&keystroke) {
            *self.key_repeat.borrow_mut() =
                Some(KeyRepeat::new(keystroke, std::time::Instant::now()));
        }
        forward
    }

    fn key_ended(&self, raw: &crate::keys::RawKey) {
        let keystroke = match key_input(raw) {
            KeyInput::Modifiers(modifiers, capslock) => {
                // Modifier flags on a key-up still include the released key on
                // some keyboards; the next event corrects them.
                self.set_modifiers(modifiers, capslock);
                return;
            }
            KeyInput::Key(keystroke) => keystroke,
        };
        let mut key_repeat = self.key_repeat.borrow_mut();
        if key_repeat
            .as_ref()
            .is_some_and(|repeat| repeat.keystroke().key == keystroke.key)
        {
            key_repeat.take();
        }
        drop(key_repeat);
        self.input_callback
            .with(|callback| callback(PlatformInput::KeyUp(KeyUpEvent { keystroke })));
    }

    fn set_modifiers(&self, modifiers: Modifiers, capslock: Capslock) {
        if self.modifiers.get() == modifiers && self.capslock.get() == capslock {
            return;
        }
        self.modifiers.set(modifiers);
        self.capslock.set(capslock);
        self.input_callback.with(|callback| {
            callback(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }))
        });
    }

    fn dispatch_key_down(&self, keystroke: &Keystroke, is_held: bool) -> bool {
        self.input_callback
            .with(|callback| {
                callback(PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held,
                    prefer_character_input: false,
                }))
            })
            .is_some_and(|result| !result.propagate)
    }

    fn insert_key_char(&self, keystroke: &Keystroke) {
        if let Some(key_char) = &keystroke.key_char {
            self.input_handler
                .with(|handler| handler.replace_text_in_range(None, key_char));
        }
    }

    fn text_input_has_marked_text(&self) -> bool {
        let marked: Option<Retained<UITextRange>> =
            unsafe { msg_send![&*self.text_input_view, markedTextRange] };
        marked.is_some()
    }

    fn repeat_held_key(&self) {
        let due = self.key_repeat.borrow_mut().as_mut().and_then(|repeat| {
            repeat
                .poll(std::time::Instant::now())
                .then(|| repeat.keystroke().clone())
        });
        if let Some(keystroke) = due {
            if !self.dispatch_key_down(&keystroke, true) {
                self.insert_key_char(&keystroke);
            }
        }
    }

    /// Delivers input from a source other than UIKit (the lan-mouse receiver)
    /// through the same path as native input: keys that GPUI does not handle
    /// insert text and repeat while held, and mouse input moves the drawn pointer.
    pub(crate) fn inject_input(&self, input: PlatformInput) {
        match input {
            PlatformInput::KeyDown(event) => {
                self.key_repeat.borrow_mut().take();
                let handled = self.dispatch_key_down(&event.keystroke, event.is_held);
                if !handled {
                    self.insert_key_char(&event.keystroke);
                }
                if KeyRepeat::repeats(&event.keystroke) {
                    *self.key_repeat.borrow_mut() =
                        Some(KeyRepeat::new(event.keystroke, std::time::Instant::now()));
                }
            }
            PlatformInput::KeyUp(event) => {
                let mut key_repeat = self.key_repeat.borrow_mut();
                if key_repeat
                    .as_ref()
                    .is_some_and(|repeat| repeat.keystroke().key == event.keystroke.key)
                {
                    key_repeat.take();
                }
                drop(key_repeat);
                self.input_callback
                    .with(|callback| callback(PlatformInput::KeyUp(event)));
            }
            PlatformInput::ModifiersChanged(event) => {
                self.set_modifiers(event.modifiers, event.capslock);
            }
            input => {
                let position = match &input {
                    PlatformInput::MouseMove(event) => Some(event.position),
                    PlatformInput::MouseDown(event) => Some(event.position),
                    PlatformInput::MouseUp(event) => Some(event.position),
                    PlatformInput::Scroll(event) => Some(event.position),
                    _ => None,
                };
                if let Some(position) = position {
                    self.mouse_position.set(position);
                    self.show_pointer(position);
                }
                self.input_callback.with(|callback| callback(input));
            }
        }
    }

    pub(crate) fn set_pointer_style(&self, style: gpui::CursorStyle) {
        if let Some(pointer) = self.pointer.borrow_mut().as_mut() {
            pointer.set_style(style);
        }
    }

    fn show_pointer(&self, position: Point<Pixels>) {
        let mut pointer = self.pointer.borrow_mut();
        let pointer = pointer.get_or_insert_with(|| PointerOverlay::new(&self.view));
        pointer.move_to(position * self.ui_scale_cell.get());
    }

    fn hide_idle_pointer(&self) {
        if let Some(pointer) = self.pointer.borrow_mut().as_mut() {
            pointer.hide_if_idle();
        }
    }

    /// Query the safe area insets from the UIView.
    ///
    /// Returns `(top, bottom, left, right)` in logical points.
    /// These represent the areas occupied by system UI (status bar,
    /// home indicator, camera notch) that content should avoid.
    fn safe_area_insets(&self) -> (f32, f32, f32, f32) {
        let insets = self.view.safeAreaInsets();
        (
            insets.top as f32,
            insets.bottom as f32,
            insets.left as f32,
            insets.right as f32,
        )
    }

    fn current_insets(&self) -> WindowInsets {
        let (top, bottom, left, right) = self.safe_area_insets();
        WindowInsets {
            safe_area: Edges {
                top: px(top),
                right: px(right),
                bottom: px(bottom),
                left: px(left),
            },
            ime: Edges {
                bottom: px(self.keyboard_height.get()),
                ..Default::default()
            },
            corner_radii: self.display_corner_radii(),
        }
    }

    fn display_corner_radii(&self) -> Corners<Pixels> {
        if !reports_corner_radii(&self.view) {
            return Corners::default();
        }
        let radius = |corner| px(self.view.effectiveRadiusForCorner(corner) as f32);
        Corners {
            top_left: radius(UIRectCorner::TopLeft),
            top_right: radius(UIRectCorner::TopRight),
            bottom_right: radius(UIRectCorner::BottomRight),
            bottom_left: radius(UIRectCorner::BottomLeft),
        }
    }

    fn notify_insets_changed(&self) {
        self.insets_changed_callback
            .with(|callback| callback(self.current_insets()));
    }

    fn notify_appearance_changed(&self) {
        self.appearance_changed_callback.with(|callback| callback());
    }

    fn set_keyboard_height(&self, height: f32) {
        let height = height.max(0.);
        if (self.keyboard_height.get() - height).abs() <= 0.5 {
            return;
        }
        self.keyboard_height.set(height);
        if height <= 0. {
            self.keyboard_dismiss_touch.set(None);
        }
        self.notify_insets_changed();
    }

    pub fn show_keyboard(&self) {
        self.text_input_view.set_keyboard_visible(true);
    }

    pub fn hide_keyboard(&self) {
        self.text_input_view.set_keyboard_visible(false);
    }

    pub(super) fn with_input_handler<R>(
        &self,
        callback: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        self.input_handler.with(callback)
    }

    /// Notify the window when its UIKit scene becomes active or inactive.
    pub fn notify_active_status_change(&self, is_active: bool) {
        log::info!("GPUI iOS: Window active status changed to: {}", is_active);
        if !is_active {
            // Key-up events are not delivered while inactive.
            self.key_repeat.borrow_mut().take();
            self.forwarded_presses.borrow_mut().clear();
        }

        self.active_status_callback
            .with(|callback| callback(is_active));
    }

    pub(crate) fn notify_visibility_change(&self, visibility: WindowVisibility) {
        if self.visibility.replace(visibility) != visibility {
            self.visibility_callback
                .with(|callback| callback(visibility));
        }
    }

    pub fn handle_layout_change(&self) {
        let view_bounds = self.view.bounds();
        let display = IosDisplay::from_screen(self.window.screen());
        if self.display_id.replace(display.id()) != display.id() {
            self.display_changed_callback.with(|callback| callback());
        }
        let scale = display.scale() as f64;

        // UI scale enlarges GPUI's logical pixels; the drawable stays native.
        let ui_scale = self.ui_scale();
        self.ui_scale_cell.set(ui_scale);
        let width = view_bounds.size.width as f32 / ui_scale;
        let height = view_bounds.size.height as f32 / ui_scale;
        let new_scale = scale as f32 * ui_scale;

        let old_bounds = self.bounds.get();
        let old_scale = self.scale_factor.get();

        let new_size = size(px(width), px(height));
        self.notify_insets_changed();

        if old_bounds.size == new_size && (old_scale - new_scale).abs() < 0.01 {
            return;
        }

        log::info!(
            "GPUI iOS: Layout changed — {:?} @{:.1}x → {:?} @{:.1}x",
            old_bounds.size,
            old_scale,
            new_size,
            new_scale,
        );

        // Update stored bounds (in logical pixels, matching GPUI convention).
        let new_bounds = Bounds {
            origin: Default::default(),
            size: new_size,
        };
        self.bounds.set(new_bounds);
        self.scale_factor.set(new_scale);
        // Update the Metal layer's contentsScale so the drawable has the
        // correct pixel dimensions.
        self.view.layer().setContentsScale(scale);

        let pixel_width = (width * new_scale) as i32;
        let pixel_height = (height * new_scale) as i32;
        self.renderer
            .lock()
            .update_drawable_size(size(DevicePixels(pixel_width), DevicePixels(pixel_height)));

        self.resize_callback
            .with(|callback| callback(new_size, new_scale));
    }
}

/// Whether UIKit can resolve corner configurations, which arrived in iOS 26.
fn reports_corner_radii(view: &UIView) -> bool {
    view.respondsToSelector(sel!(effectiveRadiusForCorner:))
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        if let Some(platform) = self.platform.upgrade() {
            platform.application.unregister_window(&self.state);
        }
        let observers = std::mem::take(&mut *self.keyboard_observers.borrow_mut());
        for observer in observers {
            unsafe { NSNotificationCenter::defaultCenter().removeObserver((&*observer).as_ref()) };
        }

        *self.view_controller.ivars().0.borrow_mut() = Weak::new();
        *self.view.ivars().0.borrow_mut() = Weak::new();
        self.text_input_view.set_window(Weak::new());
        self.text_input_view.removeFromSuperview();
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let view = NonNull::new(Retained::as_ptr(&self.view).cast_mut().cast::<c_void>())
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(view);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let handle = UiKitDisplayHandle::new();
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows cannot be resized programmatically
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        match unsafe { self.view.traitCollection().userInterfaceStyle() } {
            UIUserInterfaceStyle::Dark => WindowAppearance::Dark,
            _ => WindowAppearance::Light,
        }
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::from_screen(self.window.screen())))
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.display_changed_callback.set(callback);
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    fn capslock(&self) -> Capslock {
        self.capslock.get()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.text_input_view
            .set_input_field(input_handler.focus_id());
        self.input_handler.set(input_handler);
        self.text_input_view.refresh_keyboard();
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        let handler = self.input_handler.take();
        self.text_input_view.refresh_keyboard();
        handler
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        self.window.makeKeyAndVisible();
    }

    fn is_active(&self) -> bool {
        self.window.isKeyWindow()
    }

    fn is_hovered(&self) -> bool {
        // Hover isn't really applicable on iOS
        false
    }

    fn set_title(&mut self, _title: &str) {
        // iOS apps don't have window titles
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        // Could adjust view background color
    }

    fn minimize(&self) {
        // iOS apps cannot be minimized
    }

    fn zoom(&self) {
        // iOS apps cannot be zoomed
    }

    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.request_frame_callback.set(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.input_callback.set(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.active_status_callback.set(callback);
    }

    fn visibility(&self) -> WindowVisibility {
        self.visibility.get()
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.visibility_callback.set(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.hover_status_callback.set(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.resize_callback.set(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.moved_callback.set(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.should_close_callback.set(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.hit_test_callback.set(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.close_callback.set(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.appearance_changed_callback.set(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.renderer.lock().draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.lock().sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // iOS handles IME positioning automatically
    }

    fn insets(&self) -> WindowInsets {
        self.current_insets()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.insets_changed_callback.set(callback);
    }

    fn show_soft_keyboard(&self) {
        self.show_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        self.hide_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        self.text_input_view.state_changed(change);
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        self.text_input_view.set_configuration(&configuration);
    }
}

#[cfg(test)]
mod tests {
    use super::CallbackSlot;

    #[test]
    fn callback_is_unavailable_during_dispatch_and_restored_afterward() {
        let slot = CallbackSlot::default();
        slot.set(1);
        assert_eq!(
            slot.with(|value| {
                *value += 1;
                assert!(
                    slot.with(|_| panic!("reentered the same handler"))
                        .is_none()
                );
                *value
            }),
            Some(2)
        );
        assert_eq!(slot.with(|value| *value), Some(2));
    }

    #[test]
    fn callback_replacement_during_dispatch_is_preserved() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| {
            slot.set(2);
            slot.with(|value| *value += 1);
        });
        assert_eq!(slot.take(), Some(3));
    }

    #[test]
    fn withdrawing_an_in_flight_handler_does_not_restore_it() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| assert!(slot.take().is_none()));
        assert!(
            slot.with(|_| panic!("restored a withdrawn handler"))
                .is_none()
        );
    }

    #[test]
    fn removing_a_replacement_does_not_resurrect_the_previous_handler() {
        let slot = CallbackSlot::default();
        slot.set(1);
        slot.with(|_| {
            slot.set(2);
            assert_eq!(slot.take(), Some(2));
        });
        assert!(slot.take().is_none());
    }
}

/// A pointer image over the Metal view. Moving a UIKit view is a Core Animation
/// update that needs no GPUI frame, which keeps pointer latency independent of
/// rendering.
struct PointerOverlay {
    image_view: Retained<objc2_ui_kit::UIImageView>,
    style: gpui::CursorStyle,
    last_moved: std::time::Instant,
    visible: bool,
}

impl PointerOverlay {
    const SIZE: f64 = 22.0;
    const HIDE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

    fn new(parent: &UIView) -> Self {
        let main_thread = MainThreadMarker::from(parent);
        let image_view = objc2_ui_kit::UIImageView::initWithImage(
            objc2_ui_kit::UIImageView::alloc(main_thread),
            Self::image(gpui::CursorStyle::Arrow).as_deref(),
        );
        image_view.setFrame(CGRect::new(CGPoint::ZERO, CGSize::new(Self::SIZE, Self::SIZE)));
        image_view.setUserInteractionEnabled(false);
        parent.addSubview(&image_view);
        Self {
            image_view,
            style: gpui::CursorStyle::Arrow,
            last_moved: std::time::Instant::now(),
            visible: true,
        }
    }

    /// SF Symbols matching GPUI's cursor shapes.
    fn image(style: gpui::CursorStyle) -> Option<Retained<objc2_ui_kit::UIImage>> {
        use gpui::CursorStyle::*;
        let name = match style {
            IBeam | IBeamCursorForVerticalLayout => "character.cursor.ibeam",
            PointingHand => "hand.point.up.left.fill",
            ResizeLeft | ResizeRight | ResizeLeftRight | ResizeColumn => "arrow.left.and.right",
            ResizeUp | ResizeDown | ResizeUpDown | ResizeRow => "arrow.up.and.down",
            OperationNotAllowed => "nosign",
            ClosedHand | OpenHand => "hand.raised.fill",
            Crosshair => "plus",
            _ => "cursorarrow",
        };
        objc2_ui_kit::UIImage::systemImageNamed(&objc2_foundation::NSString::from_str(name))
    }

    fn set_style(&mut self, style: gpui::CursorStyle) {
        if self.style != style {
            self.style = style;
            self.image_view.setImage(Self::image(style).as_deref());
        }
    }

    fn move_to(&mut self, position: Point<Pixels>) {
        // The arrow's hot spot is its tip; other shapes are centered.
        let offset = if matches!(self.style, gpui::CursorStyle::Arrow) {
            0.0
        } else {
            Self::SIZE / 2.0
        };
        self.image_view.setFrame(CGRect::new(
            CGPoint::new(
                f64::from(f32::from(position.x)) - offset,
                f64::from(f32::from(position.y)) - offset,
            ),
            CGSize::new(Self::SIZE, Self::SIZE),
        ));
        if !self.visible {
            self.image_view.setHidden(false);
            self.visible = true;
        }
        self.last_moved = std::time::Instant::now();
    }

    fn hide_if_idle(&mut self) {
        if self.visible && self.last_moved.elapsed() >= Self::HIDE_AFTER {
            self.image_view.setHidden(true);
            self.visible = false;
        }
    }
}
