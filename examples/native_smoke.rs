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
        NSTextField, NSTextInputClient, NSTextView, NSView, NSWindow, NSWindowButton,
        NSWindowStyleMask, NSWindowTitleVisibility,
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
        let frame = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(740.0, 560.0));
        // The test owns this window on AppKit's main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Retained by this scope through close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Exodys"));
        let options = native::AppOptions {
            size: (740, 560),
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
        let nodes = table(&root, "native-node-table", 129);
        assert_integrated_chrome(&window, &root);
        assert_eq!(window.title().to_string(), "Exodys");
        let connection_actions = by_id(&root, "connection-actions").unwrap();
        button(&connection_actions, "连接");
        until("initial connection status", || {
            (connection_status(&root)?.starts_with("已断开")).then_some(())
        });
        assert_list_row(&profiles, &["演示配置", "129 个节点"]);
        assert_list_row(&nodes, &["Osaka relay 001", "localhost:9", "SOCKS5"]);
        let narrow_width = nodes.rectOfColumn(0).size.width;
        assert!(narrow_width > 300.0);
        window.setContentSize(NSSize::new(960.0, 700.0));
        until("node list fits wider window", || {
            let width = nodes.rectOfColumn(0).size.width;
            (width >= narrow_width + 210.0).then_some(())
        });
        assert_list_row(&nodes, &["Osaka relay 001", "localhost:9", "SOCKS5"]);
        window.setContentSize(NSSize::new(740.0, 560.0));
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

        assert!(find_button(&connection_actions, "测试代理").is_none());
        until("manual selection in sidebar", || {
            (connection_status(&root)? == "已断开 · Tokyo relay").then_some(())
        });
        assert_eq!(
            button(&connection_actions, "复制地址")
                .toolTip()
                .unwrap()
                .to_string(),
            "复制地址"
        );
        press(&connection_actions, "连接");
        until("sing-box SOCKS listener", || listening(port).then_some(()));
        button(&connection_actions, "断开");
        button(&connection_actions, "测试代理");
        until("connected status in sidebar", || {
            (connection_status(&root)? == "运行中 · Tokyo relay").then_some(())
        });
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
        press(&connection_actions, "断开");
        until("listener closed", || (!listening(port)).then_some(()));
        until("disconnected status in sidebar", || {
            (connection_status(&root)? == "已断开 · Tokyo relay").then_some(())
        });
        press(&connection_actions, "连接");
        until("listener restarted", || listening(port).then_some(()));
        button(&connection_actions, "断开");
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

    fn connection_status(root: &NSView) -> Option<String> {
        let view = by_id(root, "connection-status")?;
        let value = view.accessibilityValue()?.downcast::<NSString>().ok()?;
        Some(value.to_string())
    }

    fn table(root: &NSView, id: &str, rows: isize) -> Retained<NSTableView> {
        until(&format!("{id} with {rows} rows"), || {
            let table = by_id(root, id)?.downcast::<NSTableView>().ok()?;
            (table.numberOfRows() == rows && table.isEnabled()).then_some(table)
        })
    }

    fn assert_list_row(table: &NSTableView, expected: &[&str]) {
        assert_eq!(table.numberOfColumns(), 1);
        assert!(table.headerView().is_none());
        let cell = until("two-line native list cell", || {
            table.viewAtColumn_row_makeIfNecessary(0, 0, false)
        });
        let labels: Vec<_> = cell
            .subviews()
            .iter()
            .filter_map(|view| view.downcast::<NSTextField>().ok())
            .collect();
        let values: Vec<_> = labels
            .iter()
            .map(|label| label.stringValue().to_string())
            .collect();
        assert_eq!(values, expected);
        let frame = cell.bounds();
        assert!(labels[0].frame().origin.y > labels[1].frame().origin.y);
        for label in &labels {
            let frame_of_label = label.frame();
            assert!(frame_of_label.origin.x >= 0.0 && frame_of_label.size.width > 0.0);
            assert!(frame_of_label.origin.x + frame_of_label.size.width <= frame.size.width + 1.0);
        }
        if let Some(accessory) = labels.get(2) {
            let title = labels[0].frame();
            assert!(accessory.frame().origin.x > title.origin.x + title.size.width);
        }
    }

    fn select(table: &NSTableView, index: usize) {
        table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(index), false);
    }

    fn button(root: &NSView, title: &str) -> Retained<NSButton> {
        until(&format!("enabled button: {title}"), || {
            find_button(root, title)
        })
    }

    fn find_button(root: &NSView, title: &str) -> Option<Retained<NSButton>> {
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
    }

    fn assert_integrated_chrome(window: &NSWindow, root: &NSView) {
        assert!(window.toolbar().is_none());
        assert_eq!(window.titleVisibility(), NSWindowTitleVisibility::Hidden);
        assert!(window.titlebarAppearsTransparent());
        assert!(
            window
                .styleMask()
                .contains(NSWindowStyleMask::FullSizeContentView)
        );
        let sidebar = by_id(root, "profile-sidebar").unwrap();
        let header = button(&sidebar, "导入配置");
        let controls = [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ]
        .map(|kind| window.standardWindowButton(kind).unwrap());
        until("window buttons within the sidebar above its header", || {
            let sidebar = sidebar.convertRect_toView(sidebar.bounds(), None);
            let header = header.convertRect_toView(header.bounds(), None);
            controls
                .iter()
                .all(|control| {
                    let frame = control.convertRect_toView(control.bounds(), None);
                    !control.isHiddenOrHasHiddenAncestor()
                        && frame.origin.x >= sidebar.origin.x
                        && frame.origin.x + frame.size.width
                            <= sidebar.origin.x + sidebar.size.width
                        && frame.origin.y + frame.size.height
                            <= sidebar.origin.y + sidebar.size.height
                        && frame.origin.y >= header.origin.y + header.size.height
                })
                .then_some(())
        });
        let frame = unsafe { root.superview() }.unwrap();
        for control in controls.iter().chain([&button(root, "重命名")]) {
            let rect = control.convertRect_toView(control.bounds(), None);
            let center = NSPoint::new(
                rect.origin.x + rect.size.width / 2.0,
                rect.origin.y + rect.size.height / 2.0,
            );
            let hit = frame.hitTest(center).unwrap();
            assert!(hit.downcast_ref::<NSButton>() == Some(&**control));
        }
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
