//! Drives the real AppKit view without requesting system-wide input or capture access.

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use exodys::{
        engine::address,
        import::Source,
        session::{Job, Session},
    };
    use native::objc2::{MainThreadMarker, MainThreadOnly, Message, msg_send, rc::Retained};
    use native::objc2_app_kit::{
        NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType,
        NSBitmapImageFileType, NSBitmapImageRepPropertyKey, NSTextField, NSTextInputClient,
        NSTextView, NSView, NSWindow, NSWindowStyleMask,
    };
    use native::objc2_foundation::{
        NSDate, NSDictionary, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSString,
    };
    use nsh_platform_macos as native;

    pub fn run() {
        let screenshots = std::env::args_os().nth(1).map(std::path::PathBuf::from);
        let directory = tempfile::tempdir().unwrap();
        // This is a standalone test process. Set its workspace before AppKit or NSH starts threads.
        unsafe { std::env::set_var("EXODYS_DATA_DIR", directory.path()) };
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        {
            let mut setup = Session::open(directory.path().into()).unwrap();
            let mut nodes = String::from("socks5://127.0.0.1:9#Tokyo%20relay\n");
            for index in 1..=128 {
                use std::fmt::Write;
                writeln!(nodes, "socks5://localhost:9#Osaka%20relay%20{index:03}").unwrap();
            }
            let id = setup
                .run(Job::Import(Source::Text(nodes)))
                .unwrap()
                .focus
                .unwrap();
            setup.run(Job::Rename(id, "演示配置".into())).unwrap();
            setup.run(Job::SetPort(port)).unwrap();
        }

        let mtm = MainThreadMarker::new().expect("AppKit runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();
        let frame = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(960.0, 700.0));
        // The test owns this window on AppKit's main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Retained by this scope through close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Exodys"));
        let options = native::AppOptions {
            size: (960, 700),
            ..Default::default()
        };
        exodys::register_native_components(&options.components);
        let surface = native::AppView::new(mtm, options);
        let root = surface.view();
        window.setContentView(Some(&root));
        let start = Instant::now();
        surface.load(native::App::Rust(native::Root::of::<exodys::App>()));
        window.makeKeyAndOrderFront(None);
        app.activate();
        let search = until("native search field", || by_id(&root, "node-search"));
        println!(
            "AppKit mount plus loading 129 nodes: {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );

        let search = until("editable AppKit text field", || {
            find(&search, &|view| {
                view.downcast_ref::<NSTextField>().is_some()
            })?
            .downcast::<NSTextField>()
            .ok()
        });
        assert!(window.makeFirstResponder(Some(&search)));
        let editor = until("native field editor", || {
            search.currentEditor()?.downcast::<NSTextView>().ok()
        });
        let query = NSString::from_str("Tokyo");
        // NSTextInputClient accepts NSString and a valid replacement span in this empty field.
        unsafe { editor.insertText_replacementRange(&query, NSRange::new(0, 0)) };
        until("one matching native row", || {
            (row_count(&root) == 1).then_some(())
        });
        assert!(window.makeFirstResponder(None));

        let button = until("native connect button", || {
            by_id(&root, "connection-toggle")
        });
        press(&button);
        until("sing-box SOCKS listener", || listening(port).then_some(()));
        // The button changes as the engine finishes starting; wait for the control to re-enable.
        until("connected native button", || {
            button.isAccessibilityEnabled().then_some(())
        });
        if let Some(path) = screenshots.as_deref() {
            screenshot(&root, path);
        }
        press(&button);
        until("listener closed", || (!listening(port)).then_some(()));
        until("disconnected native button", || {
            button.isAccessibilityEnabled().then_some(())
        });
        press(&button);
        until("listener restarted", || listening(port).then_some(()));
        until("reconnected native button", || {
            button.isAccessibilityEnabled().then_some(())
        });
        surface.stop();
        window.close();
        until("listener closed after quit", || {
            (!listening(port)).then_some(())
        });
        println!("Native search, connect, disconnect and teardown passed.");
    }

    fn until<T>(label: &str, mut condition: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(value) = condition() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
        }
    }

    fn find(view: &NSView, predicate: &dyn Fn(&NSView) -> bool) -> Option<Retained<NSView>> {
        if predicate(view) {
            return Some(view.retain());
        }
        view.subviews()
            .iter()
            .find_map(|subview| find(&subview, predicate))
    }

    fn by_id(root: &NSView, id: &str) -> Option<Retained<NSView>> {
        find(root, &|view| {
            !view.isHiddenOrHasHiddenAncestor()
                && view
                    .accessibilityIdentifier()
                    .is_some_and(|value| value.to_string() == id)
        })
    }

    fn row_count(view: &NSView) -> usize {
        if view.isHiddenOrHasHiddenAncestor() {
            return 0;
        }
        usize::from(
            view.accessibilityIdentifier()
                .is_some_and(|value| value.to_string() == "node-row"),
        ) + view
            .subviews()
            .iter()
            .map(|view| row_count(&view))
            .sum::<usize>()
    }

    fn listening(port: u16) -> bool {
        TcpStream::connect_timeout(&address(port), Duration::from_millis(100)).is_ok()
    }

    fn press(button: &NSView) {
        // NSH's pressable NSViews implement the AppKit accessibility action.
        let pressed: bool = unsafe { msg_send![button, accessibilityPerformPress] };
        assert!(pressed, "AppKit did not activate the button");
    }

    fn screenshot(view: &NSView, path: &Path) {
        let bounds = view.bounds();
        let bitmap = view.bitmapImageRepForCachingDisplayInRect(bounds).unwrap();
        view.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
        let properties =
            NSDictionary::<NSBitmapImageRepPropertyKey, native::objc2::runtime::AnyObject>::new();
        // Empty output settings are valid for PNG.
        let bytes = unsafe {
            bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
        }
        .unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes.to_vec()).unwrap();
    }
}
