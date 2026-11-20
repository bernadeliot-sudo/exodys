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
    use native::objc2::{MainThreadMarker, MainThreadOnly, Message, rc::Retained, sel};
    use native::objc2_app_kit::{
        NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType,
        NSBitmapImageFileType, NSBitmapImageRepPropertyKey, NSButton, NSSearchField, NSTableView,
        NSTextField, NSTextInputClient, NSTextView, NSToolbarItem, NSView, NSWindow,
        NSWindowStyleMask,
    };
    use native::objc2_foundation::{
        NSDate, NSDictionary, NSIndexSet, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSString,
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
        let expected_selection = {
            let mut setup = Session::open(directory.path().into()).unwrap();
            let mut nodes = String::new();
            for index in 1..=128 {
                use std::fmt::Write;
                writeln!(nodes, "socks5://localhost:9#Osaka%20relay%20{index:03}").unwrap();
            }
            nodes.push_str("socks5://127.0.0.1:9#Tokyo%20relay\n");
            let id = setup
                .run(Job::Import(Source::Text(nodes)))
                .unwrap()
                .focus
                .unwrap();
            setup.run(Job::Rename(id, "演示配置".into())).unwrap();
            setup.run(Job::SetPort(port)).unwrap();
            setup
                .run(Job::Import(Source::Text(
                    "socks5://localhost:9#Second".into(),
                )))
                .unwrap();
            let snapshot = setup.snapshot();
            let tokyo = snapshot
                .catalog
                .profile(id)
                .unwrap()
                .nodes
                .iter()
                .find(|n| n.name == "Tokyo relay")
                .unwrap();
            exodys::catalog::NodeRef {
                profile: id,
                node: tokyo.id.clone(),
            }
        };

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
        let profiles = table(&root, "native-profile-table", 2);
        table(&root, "native-node-table", 129);
        println!(
            "Mounted native tables with 129 nodes in {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        select(&profiles, 1);
        table(&root, "native-node-table", 1);
        select(&profiles, 0);
        table(&root, "native-node-table", 129);
        press(&root, "重命名");
        let rename = until("focused name field", || {
            let host = by_id(&root, "profile-name")?;
            let field = find(&host, &|v| v.downcast_ref::<NSTextField>().is_some())?
                .downcast::<NSTextField>()
                .ok()?;
            field.currentEditor()?.downcast::<NSTextView>().ok()
        });
        unsafe {
            rename.insertText_replacementRange(
                &NSString::from_str(&"名".repeat(121)),
                NSRange::new(0, rename.string().length()),
            );
        }
        until("120-character profile name limit", || {
            (rename.string().length() == 120).then_some(())
        });
        unsafe {
            rename.insertText_replacementRange(
                &NSString::from_str("本机演示"),
                NSRange::new(0, rename.string().length()),
            );
            NSTextInputClient::doCommandBySelector(&*rename, sel!(insertNewline:));
        }
        table(&root, "native-node-table", 129);
        let saved: exodys::catalog::Catalog =
            serde_json::from_slice(&std::fs::read(directory.path().join("profiles.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved.profile(expected_selection.profile).unwrap().name,
            "本机演示"
        );
        press(&root, "本地端口设置");
        press(&root, "启用系统代理模式");
        press(&root, "关闭系统代理模式");
        button(&root, "启用系统代理模式"); // Leave the real network untouched before connecting.
        button(&root, "导出设置");
        button(&root, "恢复设置");
        press(&root, "分流规则");
        button(&root, "保存规则");
        let rules = until("native routing editor", || {
            find(&root, &|v| {
                !v.isHiddenOrHasHiddenAncestor()
                    && v.downcast_ref::<NSTextView>()
                        .is_some_and(|text| text.isEditable() && !text.isFieldEditor())
            })?
            .downcast::<NSTextView>()
            .ok()
        });
        assert!(window.makeFirstResponder(Some(&rules)));
        unsafe {
            rules.insertText_replacementRange(
                &NSString::from_str("DOMAIN,fixture.test,DIRECT"),
                NSRange::new(0, 0),
            );
        }
        press(&root, "保存规则");
        table(&root, "native-node-table", 129);
        let data = std::fs::read(directory.path().join("preferences.json")).unwrap();
        let prefs: exodys::catalog::Preferences = serde_json::from_slice(&data).unwrap();
        assert_eq!(prefs.routing_rules, "DOMAIN,fixture.test,DIRECT");
        press(&root, "导入配置");
        let editor = until("focused scrolling multiline editor", || {
            let host = by_id(&root, "import-content")?;
            let editor = find(&host, &|v| v.downcast_ref::<NSTextView>().is_some())?
                .downcast::<NSTextView>()
                .ok()?;
            (editor.enclosingScrollView().is_some()
                && window
                    .firstResponder()?
                    .downcast_ref::<NSTextView>()
                    .is_some())
            .then_some(editor)
        });
        unsafe {
            editor.insertText_replacementRange(&NSString::from_str("one\ntwo"), NSRange::new(0, 0))
        };
        assert_eq!(editor.string().to_string(), "one\ntwo");
        button(&root, "导入"); // enabled by the native change event
        press(&root, "取消");
        let search = until("native search field", || by_id(&root, "node-search"));

        let search = until("AppKit search field", || {
            find(&search, &|view| {
                view.downcast_ref::<NSSearchField>().is_some()
            })?
            .downcast::<NSSearchField>()
            .ok()
        });
        assert!(window.makeFirstResponder(Some(&search)));
        let editor = until("native field editor", || {
            search.currentEditor()?.downcast::<NSTextView>().ok()
        });
        let query = NSString::from_str("Tokyo");
        // NSTextInputClient accepts NSString and a valid replacement span in this empty field.
        unsafe { editor.insertText_replacementRange(&query, NSRange::new(0, 0)) };
        let nodes = table(&root, "native-node-table", 1);
        select(&nodes, 0);
        until("filtered selection persisted by node ID", || {
            let data = std::fs::read(directory.path().join("preferences.json")).ok()?;
            let preferences: exodys::catalog::Preferences = serde_json::from_slice(&data).ok()?;
            (preferences.selected.as_ref() == Some(&expected_selection)).then_some(())
        });
        unsafe {
            editor.insertText_replacementRange(&NSString::new(), NSRange::new(0, query.length()))
        };
        let unfiltered = table(&root, "native-node-table", 129);
        assert_eq!(
            unfiltered.selectedRow(),
            128,
            "selection follows Tokyo after clearing search"
        );
        assert!(window.makeFirstResponder(None));

        assert!(
            !window
                .toolbar()
                .unwrap()
                .items()
                .iter()
                .any(|item| item.itemIdentifier().to_string() == "probe")
        );
        assert!(window.subtitle().to_string().contains("Tokyo relay"));
        let copy = toolbar_item(&window, "copy", "复制地址");
        assert!(
            copy.toolTip()
                .unwrap()
                .to_string()
                .contains(&address(port).to_string())
        );
        activate(&app, &toolbar_item(&window, "toggle", "连接"));
        until("sing-box SOCKS listener", || listening(port).then_some(()));
        toolbar_item(&window, "toggle", "断开");
        toolbar_item(&window, "probe", "测试代理");
        assert!(window.subtitle().to_string().contains("运行中"));
        press(&root, "本地端口设置");
        until("read-only connected port", || {
            let host = by_id(&root, "local-port")?;
            let field = find(&host, &|v| v.downcast_ref::<NSTextField>().is_some())?
                .downcast::<NSTextField>()
                .ok()?;
            (!field.isEditable() && field.stringValue().to_string() == port.to_string())
                .then_some(())
        });
        press(&root, "取消");
        table(&root, "native-node-table", 129);
        if let Some(path) = screenshots.as_deref() {
            // Window and content are retained while caching their common frame view.
            let frame = unsafe { root.superview() }.unwrap();
            screenshot(&frame, path);
        }
        activate(&app, &toolbar_item(&window, "toggle", "断开"));
        until("listener closed", || (!listening(port)).then_some(()));
        activate(&app, &toolbar_item(&window, "toggle", "连接"));
        until("listener restarted", || listening(port).then_some(()));
        toolbar_item(&window, "toggle", "断开");
        surface.stop();
        until("window toolbar detached on stop", || {
            window.toolbar().is_none().then_some(())
        });
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

    fn table(root: &NSView, id: &str, rows: isize) -> Retained<NSTableView> {
        until(&format!("{id} with {rows} rows"), || {
            let table = by_id(root, id)?.downcast::<NSTableView>().ok()?;
            (table.numberOfRows() == rows && table.isEnabled()).then_some(table)
        })
    }

    fn select(table: &NSTableView, index: usize) {
        table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(index), false);
    }

    fn button(root: &NSView, title: &str) -> Retained<NSButton> {
        until(&format!("enabled button: {title}"), || {
            find(root, &|v| {
                !v.isHiddenOrHasHiddenAncestor()
                    && v.downcast_ref::<NSButton>().is_some_and(|b| {
                        b.isEnabled()
                            && (b.title().to_string() == title
                                || b.accessibilityLabel()
                                    .is_some_and(|label| label.to_string() == title))
                    })
            })?
            .downcast()
            .ok()
        })
    }

    fn toolbar_item(window: &NSWindow, id: &str, label: &str) -> Retained<NSToolbarItem> {
        until(&format!("window toolbar item: {label}"), || {
            let toolbar = window.toolbar()?;
            assert_eq!(toolbar.identifier().to_string(), "exodys.connection");
            toolbar.visibleItems()?.iter().find(|item| {
                item.itemIdentifier().to_string() == id
                    && item.label().to_string() == label
                    && item.isEnabled()
            })
        })
    }

    fn activate(app: &NSApplication, item: &NSToolbarItem) {
        let target = item.target().unwrap();
        assert!(unsafe {
            app.sendAction_to_from(item.action().unwrap(), Some(&target), Some(item))
        });
    }

    fn listening(port: u16) -> bool {
        TcpStream::connect_timeout(&address(port), Duration::from_millis(100)).is_ok()
    }

    fn press(root: &NSView, title: &str) {
        let button = button(root, title);
        unsafe { button.performClick(None) };
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
