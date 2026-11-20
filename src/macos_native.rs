use std::rc::Rc;

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, sel,
    },
    objc2_app_kit::{NSAccessibility, NSButton, NSControl, NSSearchField, NSTextField, NSView},
    objc2_foundation::{NSObject, NSObjectProtocol, NSString},
};
use serde_json::Value;

mod editors;
mod tables;

pub fn register_components(components: &Components) {
    editors::register(components);
    tables::register(components);
    components.register(
        "exodys/search-field",
        Rc::new(|context| Box::new(SearchField::new(context))),
    );
    components.register(
        "exodys/button",
        Rc::new(|context| Box::new(Button::new(context))),
    );
}

struct SearchField {
    field: Retained<NSSearchField>,
    _target: Retained<SearchFieldTarget>,
}

impl SearchField {
    fn new(context: ComponentContext) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let field = NSSearchField::new(mtm);
        field.setSendsSearchStringImmediately(true);
        field.setAccessibilityIdentifier(Some(&NSString::from_str("native-node-search")));
        field.setAccessibilityLabel(Some(&NSString::from_str("搜索节点")));

        let target: Retained<SearchFieldTarget> = unsafe {
            msg_send![
                super(SearchFieldTarget::alloc(mtm).set_ivars(context)),
                init
            ]
        };
        unsafe {
            field.setTarget(Some(&*target));
            field.setAction(Some(sel!(searchChanged:)));
        }
        Self {
            field,
            _target: target,
        }
    }
}

impl NativeComponent for SearchField {
    fn view(&self) -> Retained<NSView> {
        let text_field: Retained<NSTextField> = Retained::into_super(self.field.clone());
        let control: Retained<NSControl> = Retained::into_super(text_field);
        Retained::into_super(control)
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "value" => {
                let value = value.and_then(Value::as_str).unwrap_or_default();
                let value = NSString::from_str(value);
                if self.field.stringValue() != value {
                    self.field.setStringValue(&value);
                }
            }
            "placeholder" => {
                let placeholder = value.and_then(Value::as_str).unwrap_or_default();
                self.field
                    .setPlaceholderString(Some(&NSString::from_str(placeholder)));
            }
            _ => {}
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ComponentContext]
    #[name = "ExodysSearchFieldTarget"]
    struct SearchFieldTarget;

    unsafe impl NSObjectProtocol for SearchFieldTarget {}

    impl SearchFieldTarget {
        #[unsafe(method(searchChanged:))]
        fn search_changed(&self, sender: Option<&NSSearchField>) {
            let Some(sender) = sender else {
                return;
            };
            self.ivars().emit(
                "change",
                Value::String(sender.stringValue().to_string()),
            );
        }
    }
);

struct Button {
    button: Retained<NSButton>,
    target: Retained<ButtonTarget>,
}

impl Button {
    fn new(context: ComponentContext) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let target: Retained<ButtonTarget> =
            unsafe { msg_send![super(ButtonTarget::alloc(mtm).set_ivars(context)), init] };
        // AppKit keeps the target weak; Button owns it for the control's lifetime.
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::new(),
                Some(&target),
                Some(sel!(pressed:)),
                mtm,
            )
        };
        Self { button, target }
    }
}

impl NativeComponent for Button {
    fn view(&self) -> Retained<NSView> {
        let control: Retained<NSControl> = Retained::into_super(self.button.clone());
        Retained::into_super(control)
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "title" => {
                let title = value.and_then(Value::as_str).unwrap_or_default();
                self.button.setTitle(&NSString::from_str(title));
                let size = self.button.intrinsicContentSize();
                self.target
                    .ivars()
                    .set_size(size.width as f32, size.height as f32);
            }
            "disabled" => {
                self.button
                    .setEnabled(!value.and_then(Value::as_bool).unwrap_or(false));
            }
            "role" => {
                let role = value.and_then(Value::as_str);
                self.button
                    .setHasDestructiveAction(role == Some("destructive"));
                let accent = (role == Some("primary"))
                    .then(nsh_platform_macos::objc2_app_kit::NSColor::controlAccentColor);
                self.button.setBezelColor(accent.as_deref());
            }
            _ => {}
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ComponentContext]
    #[name = "ExodysButtonTarget"]
    struct ButtonTarget;

    unsafe impl NSObjectProtocol for ButtonTarget {}

    impl ButtonTarget {
        #[unsafe(method(pressed:))]
        fn pressed(&self, _sender: Option<&NSButton>) {
            self.ivars().emit("press", Value::Null);
        }
    }
);
