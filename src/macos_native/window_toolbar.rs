//! Per-window AppKit chrome. The zero-height anchor follows its actual window,
//! including when an embedded NSH surface is detached or moved.
use std::{cell::RefCell, rc::Rc};

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
        rc::Retained, runtime::ProtocolObject, sel,
    },
    objc2_app_kit::{
        NSImage, NSTitlebarSeparatorStyle, NSToolbar, NSToolbarDelegate, NSToolbarDisplayMode,
        NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItem, NSView, NSWindow,
        NSWindowToolbarStyle,
    },
    objc2_foundation::{NSArray, NSObjectProtocol, NSRect, NSString},
};
use serde::Deserialize;
use serde_json::Value;

pub(super) fn register(components: &Components) {
    components.register(
        "exodys/window-toolbar",
        Rc::new(|context| Box::new(Bridge::new(context))),
    );
}

#[derive(Default, Deserialize)]
struct State {
    connected: bool,
    node: Option<String>,
    address: String,
    busy: bool,
}

impl State {
    fn configure(&self, item: &NSToolbarItem) {
        let (label, symbol, enabled) = match item.itemIdentifier().to_string().as_str() {
            "copy" => ("复制地址", "doc.on.doc", !self.address.is_empty()),
            "probe" => (
                "测试代理",
                "waveform.path.ecg",
                self.connected && !self.busy,
            ),
            "toggle" if self.connected => ("断开", "stop.fill", !self.busy),
            "toggle" => ("连接", "play.fill", self.node.is_some() && !self.busy),
            _ => return,
        };
        let label = NSString::from_str(label);
        item.setLabel(&label);
        let tooltip = if item.itemIdentifier().to_string() == "copy" {
            NSString::from_str(&format!("复制 HTTP / SOCKS5 地址：{}", self.address))
        } else {
            label.clone()
        };
        item.setToolTip(Some(&tooltip));
        if item.itemIdentifier().to_string() == "toggle" {
            item.setTitle(&label);
            item.setBordered(true);
        } else {
            item.setImage(
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    Some(&label),
                )
                .as_deref(),
            );
        }
        item.setEnabled(enabled);
    }
}

fn identifiers(connected: bool) -> Retained<NSArray<NSString>> {
    let mut ids = vec![
        unsafe { NSToolbarFlexibleSpaceItemIdentifier }.retain(),
        NSString::from_str("copy"),
    ];
    if connected {
        ids.push(NSString::from_str("probe"));
    }
    ids.push(NSString::from_str("toggle"));
    NSArray::from_retained_slice(&ids)
}

struct PreviousChrome {
    toolbar: Option<Retained<NSToolbar>>,
    subtitle: Retained<NSString>,
    style: NSWindowToolbarStyle,
    separator: NSTitlebarSeparatorStyle,
}
struct Ivars {
    toolbar: Retained<NSToolbar>,
    previous: RefCell<Option<PreviousChrome>>,
    state: RefCell<State>,
    context: ComponentContext,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    #[name = "ExodysWindowToolbarAnchor"]
    struct Anchor;
    unsafe impl NSObjectProtocol for Anchor {}
    unsafe impl NSToolbarDelegate for Anchor {
        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn defaults(&self, _: &NSToolbar) -> Retained<NSArray<NSString>> { identifiers(self.ivars().state.borrow().connected) }
        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed(&self, _: &NSToolbar) -> Retained<NSArray<NSString>> { identifiers(true) }
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn item(&self, _: &NSToolbar, id: &NSString, _: bool) -> Option<Retained<NSToolbarItem>> {
            if matches!(id.to_string().as_str(), "copy" | "probe" | "toggle") {
                let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), id);
                item.setAutovalidates(false);
                self.ivars().state.borrow().configure(&item);
                unsafe { item.setTarget(Some(self)); item.setAction(Some(sel!(activateItem:))); }
                Some(item)
            } else { None }
        }
    }
    impl Anchor {
        #[unsafe(method(activateItem:))]
        fn activate(&self, item: &NSToolbarItem) {
            if item.isEnabled() { self.ivars().context.emit(&item.itemIdentifier().to_string(), Value::Null); }
        }
        #[unsafe(method(viewWillMoveToWindow:))]
        fn will_move(&self, next: Option<&NSWindow>) {
            self.detach();
            unsafe { let _: () = msg_send![super(self), viewWillMoveToWindow: next]; }
        }
        #[unsafe(method(viewDidMoveToWindow))]
        fn did_move(&self) {
            unsafe { let _: () = msg_send![super(self), viewDidMoveToWindow]; }
            if let Some(window) = self.window() {
                let iv = self.ivars();
                iv.previous.replace(Some(PreviousChrome { toolbar: window.toolbar(), subtitle: window.subtitle(), style: window.toolbarStyle(), separator: window.titlebarSeparatorStyle() }));
                window.setToolbarStyle(NSWindowToolbarStyle::Unified);
                window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
                window.setToolbar(Some(&iv.toolbar));
                self.refresh(&window);
            }
        }
    }
);

impl Anchor {
    fn detach(&self) {
        let iv = self.ivars();
        if let Some(previous) = iv.previous.take()
            && let Some(window) = self.window()
            && window.toolbar().as_deref() == Some(&iv.toolbar)
        {
            window.setToolbar(previous.toolbar.as_deref());
            window.setToolbarStyle(previous.style);
            window.setTitlebarSeparatorStyle(previous.separator);
            window.setSubtitle(&previous.subtitle);
        }
    }
    fn refresh(&self, window: &NSWindow) {
        let iv = self.ivars();
        let state = iv.state.borrow();
        let ids = identifiers(state.connected);
        if iv.toolbar.itemIdentifiers() != ids {
            iv.toolbar.setItemIdentifiers(&ids);
        }
        for item in iv.toolbar.items().iter() {
            state.configure(&item);
        }
        let status = if state.connected {
            "运行中"
        } else {
            "已断开"
        };
        window.setSubtitle(&NSString::from_str(&format!(
            "{status} · {}",
            state.node.as_deref().unwrap_or("未选择节点")
        )));
    }
}

struct Bridge(Retained<Anchor>);
impl Bridge {
    fn new(context: ComponentContext) -> Self {
        let mtm = MainThreadMarker::new().expect("AppKit window toolbar requires main thread");
        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            &NSString::from_str("exodys.connection"),
        );
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        let anchor: Retained<Anchor> = unsafe {
            msg_send![super(Anchor::alloc(mtm).set_ivars(Ivars { toolbar, previous: RefCell::default(), state: RefCell::default(), context })), initWithFrame: NSRect::ZERO]
        };
        anchor
            .ivars()
            .toolbar
            .setDelegate(Some(ProtocolObject::from_ref(&*anchor)));
        Self(anchor)
    }
}
impl NativeComponent for Bridge {
    fn view(&self) -> Retained<NSView> {
        Retained::into_super(self.0.clone())
    }
    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        if name == "state"
            && let Some(state) = value.and_then(|v| State::deserialize(v).ok())
        {
            self.0.ivars().state.replace(state);
            if let Some(window) = self.0.window() {
                self.0.refresh(&window);
            }
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.0.detach();
    }
}
