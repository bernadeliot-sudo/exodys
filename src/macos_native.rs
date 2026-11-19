use std::rc::Rc;

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, sel,
    },
    objc2_app_kit::{NSAccessibility, NSControl, NSSearchField, NSTextField, NSView},
    objc2_foundation::{NSObject, NSObjectProtocol, NSString},
};
use serde_json::Value;

pub fn register_components(components: &Components) {
    components.register(
        "exodys/search-field",
        Rc::new(|context| Box::new(SearchField::new(context))),
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
                self.field.setStringValue(&NSString::from_str(value));
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
