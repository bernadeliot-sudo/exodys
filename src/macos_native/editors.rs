use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
};

use zeroize::Zeroizing;

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
        runtime::ProtocolObject, sel,
    },
    objc2_app_kit::{
        NSAccessibility, NSAutoresizingMaskOptions, NSBorderType, NSControl,
        NSControlTextEditingDelegate, NSScrollView, NSSecureTextField, NSTextDelegate, NSTextField,
        NSTextFieldDelegate, NSTextInputClient, NSTextView, NSTextViewDelegate, NSView,
    },
    objc2_foundation::{
        NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    },
};
use serde_json::Value;

const TEXT_FIELD: &str = "exodys/text-field";
const TEXT_AREA: &str = "exodys/text-area";

pub(super) fn register(components: &Components) {
    components.register(
        "exodys/secure-text-field",
        Rc::new(|context| Box::new(SecureTextField::new(context))),
    );
    components.register(
        TEXT_FIELD,
        Rc::new(|context| Box::new(TextField::new(context))),
    );
    components.register(
        TEXT_AREA,
        Rc::new(|context| Box::new(TextArea::new(context))),
    );
}

// AppKit and the Rust UI run on separate threads. Only the input ID travels
// through NSH's JSON event. Unconsumed input is cleared on unmount.
static SECURE_INPUT_ID: AtomicU64 = AtomicU64::new(1);
static SECURE_INPUTS: LazyLock<Mutex<HashMap<u64, Zeroizing<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn take_secure_input(id: u64) -> Option<Zeroizing<String>> {
    SECURE_INPUTS.lock().ok()?.remove(&id)
}

struct SecureTextField {
    field: Retained<NSSecureTextField>,
    target: Retained<SecureTextFieldTarget>,
}

impl SecureTextField {
    fn new(context: ComponentContext) -> Self {
        let mtm = MainThreadMarker::new().expect("native components require the main thread");
        let field = NSSecureTextField::initWithFrame(
            NSSecureTextField::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(240.0, 32.0)),
        );
        field.setAccessibilityLabel(Some(&NSString::from_str("备份口令")));
        field.setPlaceholderString(Some(&NSString::from_str("输入口令后按回车")));
        let target: Retained<SecureTextFieldTarget> = unsafe {
            msg_send![
                super(
                    SecureTextFieldTarget::alloc(mtm).set_ivars(SecureTextFieldState {
                        context,
                        id: SECURE_INPUT_ID.fetch_add(1, Ordering::Relaxed),
                    })
                ),
                init
            ]
        };
        unsafe {
            field.setTarget(Some(&*target));
            field.setAction(Some(sel!(submit:)));
        }
        Self { field, target }
    }
}

fn clear_secure_field(field: &NSSecureTextField) {
    if let Some(editor) = field
        .currentEditor()
        .and_then(|editor| editor.downcast::<NSTextView>().ok())
    {
        editor.setString(&NSString::new());
    }
    field.setStringValue(&NSString::new());
}

impl Drop for SecureTextField {
    fn drop(&mut self) {
        clear_secure_field(&self.field);
        take_secure_input(self.target.ivars().id);
    }
}

impl NativeComponent for SecureTextField {
    fn view(&self) -> Retained<NSView> {
        let field: Retained<NSTextField> = Retained::into_super(self.field.clone());
        let control: Retained<NSControl> = Retained::into_super(field);
        Retained::into_super(control)
    }

    fn set_prop(&mut self, _name: &str, _value: Option<&Value>) {}
}

struct SecureTextFieldState {
    context: ComponentContext,
    id: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SecureTextFieldState]
    #[name = "ExodysSecureTextFieldTarget"]
    struct SecureTextFieldTarget;

    unsafe impl NSObjectProtocol for SecureTextFieldTarget {}

    impl SecureTextFieldTarget {
        #[unsafe(method(submit:))]
        fn submit(&self, sender: Option<&NSSecureTextField>) {
            let Some(sender) = sender else { return };
            let value = Zeroizing::new(sender.stringValue().to_string());
            clear_secure_field(sender);
            let state = self.ivars();
            if let Ok(mut inputs) = SECURE_INPUTS.lock()
                && let std::collections::hash_map::Entry::Vacant(entry) = inputs.entry(state.id)
            {
                entry.insert(value);
                drop(inputs);
                state.context.emit("submit", Value::from(state.id));
            }
        }
    }
);

struct TextField {
    field: Retained<NSTextField>,
    target: Retained<TextFieldTarget>,
    focused: Cell<bool>,
}

impl TextField {
    fn new(context: ComponentContext) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(120.0, 32.0)),
        );
        // Keep the AppKit bezel and background. The outer frame is deliberately
        // not drawn by NSH so the field follows the current macOS appearance.
        field.setBezeled(true);
        field.setDrawsBackground(true);
        field.setEditable(true);
        field.setSelectable(true);
        field.setUsesSingleLineMode(true);
        field.setAccessibilityIdentifier(Some(&NSString::from_str("native-form-text-field")));

        let target: Retained<TextFieldTarget> = unsafe {
            msg_send![
                super(TextFieldTarget::alloc(mtm).set_ivars(TextFieldState {
                    context,
                    max_length: Cell::new(None),
                })),
                init
            ]
        };
        unsafe {
            field.setTarget(Some(&*target));
            field.setAction(Some(sel!(submit:)));
            field.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        }
        Self {
            field,
            target,
            focused: Cell::new(false),
        }
    }
}

impl NativeComponent for TextField {
    fn view(&self) -> Retained<NSView> {
        let control: Retained<NSControl> = Retained::into_super(self.field.clone());
        Retained::into_super(control)
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "value" => {
                let value = value.and_then(Value::as_str).unwrap_or_default();
                if self.field.stringValue().to_string() != value {
                    self.field.setStringValue(&NSString::from_str(value));
                }
            }
            "placeholder" => {
                let placeholder = value.and_then(Value::as_str).unwrap_or_default();
                self.field
                    .setAccessibilityLabel(Some(&NSString::from_str(placeholder)));
                self.field
                    .setPlaceholderString(Some(&NSString::from_str(placeholder)));
            }
            "max_length" => {
                self.target
                    .ivars()
                    .max_length
                    .set(value.and_then(Value::as_u64).map(|n| n as usize));
            }
            "readonly" => {
                self.field
                    .setEditable(!value.and_then(Value::as_bool).unwrap_or(false));
            }
            "auto_focus" => {
                if value.and_then(Value::as_bool).unwrap_or(false)
                    && !self.focused.get()
                    && let Some(window) = self.field.window()
                {
                    self.focused.set(true);
                    let _ = window.makeFirstResponder(Some(&self.field));
                }
            }
            _ => {}
        }
    }
}

struct TextFieldState {
    context: ComponentContext,
    max_length: Cell<Option<usize>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TextFieldState]
    #[name = "ExodysTextFieldTarget"]
    struct TextFieldTarget;

    unsafe impl NSObjectProtocol for TextFieldTarget {}

    impl TextFieldTarget {
        #[unsafe(method(submit:))]
        fn submit(&self, sender: Option<&NSTextField>) {
            let Some(sender) = sender else { return };
            self.ivars()
                .context
                .emit("submit", Value::String(sender.stringValue().to_string()));
        }
    }

    unsafe impl NSControlTextEditingDelegate for TextFieldTarget {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            let Some(object) = notification.object() else { return };
            let Some(field) = object.downcast_ref::<NSTextField>() else { return };
            if field
                .currentEditor()
                .and_then(|editor| editor.downcast::<NSTextView>().ok())
                .is_some_and(|editor| editor.hasMarkedText())
            {
                return;
            }
            let mut value = field.stringValue().to_string();
            if let Some(limit) = self.ivars().max_length.get()
                && value.chars().count() > limit
            {
                value = value.chars().take(limit).collect();
                field.setStringValue(&NSString::from_str(&value));
            }
            self.ivars().context.emit("change", Value::String(value));
        }
    }

    unsafe impl NSTextFieldDelegate for TextFieldTarget {}
);

struct TextArea {
    scroller: Retained<NSScrollView>,
    text: Retained<NSTextView>,
    target: Retained<TextAreaTarget>,
    focused: Cell<bool>,
}

impl TextArea {
    fn new(context: ComponentContext) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let scroller = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(240.0, 100.0)),
        );
        // NSScrollView supplies the system text-editor frame and scroll indicators.
        scroller.setBorderType(NSBorderType::BezelBorder);
        scroller.setDrawsBackground(true);
        scroller.setHasVerticalScroller(true);
        scroller.setHasHorizontalScroller(false);
        scroller.setAutohidesScrollers(true);

        let text = NSTextView::initWithFrame(
            NSTextView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(240.0, 100.0)),
        );
        text.setEditable(true);
        text.setSelectable(true);
        text.setRichText(false);
        text.setImportsGraphics(false);
        // Paste and type configuration syntax verbatim, including JSON quotes and dashes.
        text.setAutomaticQuoteSubstitutionEnabled(false);
        text.setAutomaticDashSubstitutionEnabled(false);
        text.setAutomaticTextReplacementEnabled(false);
        text.setAutomaticSpellingCorrectionEnabled(false);
        text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        text.setDrawsBackground(false);
        text.setHorizontallyResizable(false);
        text.setVerticallyResizable(true);
        text.setMinSize(NSSize::new(0.0, 100.0));
        text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        text.setTextContainerInset(NSSize::new(6.0, 6.0));
        if let Some(container) = unsafe { text.textContainer() } {
            container.setLineFragmentPadding(0.0);
            container.setWidthTracksTextView(true);
        }

        let placeholder = NSTextField::labelWithString(&NSString::new(), mtm);
        placeholder.setSelectable(false);
        placeholder.setFrame(NSRect::new(
            NSPoint::new(7.0, 7.0),
            NSSize::new(500.0, 22.0),
        ));
        placeholder.setHidden(true);
        scroller.setDocumentView(Some(&text));
        text.addSubview(&placeholder);

        let target: Retained<TextAreaTarget> = unsafe {
            msg_send![
                super(TextAreaTarget::alloc(mtm).set_ivars(TextAreaState {
                    context,
                    max_length: Cell::new(None),
                    placeholder,
                })),
                init
            ]
        };
        text.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        Self {
            scroller,
            text,
            target,
            focused: Cell::new(false),
        }
    }
}

impl NativeComponent for TextArea {
    fn view(&self) -> Retained<NSView> {
        Retained::into_super(self.scroller.clone())
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "value" => {
                let value = value.and_then(Value::as_str).unwrap_or_default();
                if self.text.string().to_string() != value {
                    self.text.setString(&NSString::from_str(value));
                }
                self.target.ivars().placeholder.setHidden(!value.is_empty());
            }
            "placeholder" => {
                let placeholder = value.and_then(Value::as_str).unwrap_or_default();
                self.text
                    .setAccessibilityLabel(Some(&NSString::from_str(placeholder)));
                self.target
                    .ivars()
                    .placeholder
                    .setStringValue(&NSString::from_str(placeholder));
                self.target
                    .ivars()
                    .placeholder
                    .setHidden(!self.text.string().to_string().is_empty());
            }
            "max_length" => {
                self.target
                    .ivars()
                    .max_length
                    .set(value.and_then(Value::as_u64).map(|n| n as usize));
            }
            "readonly" => {
                self.text
                    .setEditable(!value.and_then(Value::as_bool).unwrap_or(false));
            }
            "auto_focus" => {
                if value.and_then(Value::as_bool).unwrap_or(false)
                    && !self.focused.get()
                    && let Some(window) = self.text.window()
                {
                    self.focused.set(true);
                    let _ = window.makeFirstResponder(Some(&self.text));
                }
            }
            _ => {}
        }
    }
}

struct TextAreaState {
    context: ComponentContext,
    max_length: Cell<Option<usize>>,
    placeholder: Retained<NSTextField>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TextAreaState]
    #[name = "ExodysTextAreaTarget"]
    struct TextAreaTarget;

    unsafe impl NSObjectProtocol for TextAreaTarget {}

    unsafe impl NSTextDelegate for TextAreaTarget {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, notification: &NSNotification) {
            let Some(object) = notification.object() else {
                return;
            };
            let Some(text) = object.downcast_ref::<NSTextView>() else {
                return;
            };
            if text.hasMarkedText() {
                return;
            }
            let mut value = text.string().to_string();
            if let Some(limit) = self.ivars().max_length.get()
                && value.chars().count() > limit
            {
                value = value.chars().take(limit).collect();
                text.setString(&NSString::from_str(&value));
            }
            self.ivars().placeholder.setHidden(!value.is_empty());
            self.ivars().context.emit("change", Value::String(value));
        }
    }

    unsafe impl NSTextViewDelegate for TextAreaTarget {}
);
