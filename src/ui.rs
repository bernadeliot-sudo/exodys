#[cfg(target_os = "macos")]
mod backup;
mod group;
mod policies;
mod rule_sets;
mod settings_file;
#[cfg(target_os = "macos")]
mod sync;
#[cfg(target_os = "windows")]
mod windows;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsh_base::toolbar;
use nsh_core::desktop::WindowAttributes;
#[cfg(target_os = "macos")]
use nsh_core::desktop::{TitleBarOptions, TitleBarStyle};
use nsh_core::element::desktop::current_window;
#[cfg(target_os = "macos")]
use nsh_core::element::desktop::title_bar;
use nsh_core::file_dialog::{FileFilter, OpenDialog};
use nsh_core::prelude::*;

use crate::catalog::{Matches, NodeRef, Profile, default_directory};
use crate::engine::address;
use crate::import::{MAX_BYTES, Source};
use crate::session::{Job, Session, Snapshot};

// Flat document canvas; the inset source list supplies the one raised surface.
const CANVAS: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x1e1e20);
const SURFACE: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x2c2c2e);
const INK: AdaptiveColor = AdaptiveColor::rgb(0x1d1d1f, 0xf5f5f7);
const SUBTLE: AdaptiveColor = AdaptiveColor::rgb(0x6e6e73, 0x98989d);
const LINE: AdaptiveColor = AdaptiveColor::rgb(0xd6d6dc, 0x48484a);
const ERROR: AdaptiveColor = AdaptiveColor::rgb(0xff3b30, 0xff6961);

#[cfg(target_os = "macos")]
fn macos_backdrop(material: MacOsBackdropMaterial) -> MacOsBackdrop {
    let mut backdrop = MacOsBackdrop::default();
    backdrop.material = material;
    backdrop.blending = MacOsBackdropBlending::WithinWindow;
    backdrop.state = MacOsBackdropState::FollowsWindowActiveState;
    backdrop
}

#[component]
pub fn App() -> impl IntoElement {
    let mut window = WindowAttributes::default();
    #[cfg(target_os = "macos")]
    {
        let mut title = TitleBarOptions::default();
        title.style = TitleBarStyle::Hidden;
        title.height = Some(px(38.0));
        title.controls_offset = Some(point(px(23.0), px(18.0)));
        window.title_bar = Some(title);
    }
    window.min_size = Some(Some(size(px(740.0), px(560.0))));
    current_window().set(window);
    let ui = Ui::new();
    ui.load();
    cleanup(move || {
        if let Some(session) = ui.session.get_untracked()
            && let Ok(mut session) = session.try_lock()
        {
            session.close_window();
        }
    });
    let scheduled = computed(move || {
        ui.state.with(|state| {
            state.preferences.scheduled_refresh || !state.preferences.rule_set_updates.is_empty()
        })
    });
    effect(move || {
        if scheduled.get() {
            set_interval(Duration::from_secs(60), move || ui.refresh_due());
        }
    });
    #[cfg(target_os = "macos")]
    let syncing = computed(move || ui.state.with(|state| state.sync.unlocked));
    #[cfg(target_os = "macos")]
    effect(move || {
        if syncing.get() {
            set_interval(Duration::from_secs(60), move || ui.sync_due());
        }
    });
    let connected = computed(move || ui.state.with(|state| state.connected));
    effect(move || {
        if connected.get() {
            set_interval(Duration::from_secs(1), move || ui.check_connection());
        }
    });

    view()
        .flex_1()
        .flex_row()
        .bg(CANVAS)
        .text_color(INK)
        .child(sidebar(ui))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .flex_col()
                .child(feedback(ui))
                .child(
                    view()
                        .flex_1()
                        .min_h(px(0.0))
                        .px(px(22.0))
                        .pt(px(14.0))
                        .pb(px(10.0))
                        .child(dynamic(move || match ui.page.get() {
                            Page::Nodes => nodes(ui).into_element(),
                            Page::Import => import_form(ui).into_element(),
                            Page::Rename(id) => rename_form(ui, id).into_element(),
                            Page::RefreshCycle(id) => refresh_cycle_form(ui, id).into_element(),
                            Page::Remove(id) => remove_form(ui, id).into_element(),
                            Page::Settings => settings_form(ui).into_element(),
                            #[cfg(target_os = "macos")]
                            Page::Backup => backup::page(ui).into_element(),
                            #[cfg(target_os = "macos")]
                            Page::Sync => sync::page(ui).into_element(),
                            Page::Routing => routing_form(ui).into_element(),
                            Page::Group => group::page(ui).into_element(),
                            Page::RuleSets => rule_sets::page(ui).into_element(),
                            Page::Policies => policies::page(ui).into_element(),
                        })),
                ),
        )
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Nodes,
    Import,
    Rename(u64),
    RefreshCycle(u64),
    Remove(u64),
    Settings,
    #[cfg(target_os = "macos")]
    Backup,
    #[cfg(target_os = "macos")]
    Sync,
    Routing,
    RuleSets,
    Policies,
    Group,
}

#[derive(Clone, PartialEq)]
struct Notice {
    message: String,
    error: bool,
}

#[derive(Clone, Copy)]
struct Ui {
    owner: Owner,
    session: Signal<Option<Arc<Mutex<Session>>>>,
    state: Signal<Snapshot>,
    focus: Signal<Option<u64>>,
    filter: Signal<String>,
    draft: Signal<String>,
    page: Signal<Page>,
    busy: Signal<Option<&'static str>>,
    notice: Signal<Option<Notice>>,
}

impl Ui {
    fn new() -> Self {
        Self {
            owner: Owner::current(),
            session: signal_with(None, SignalEquals::Never),
            state: signal(Snapshot::default()),
            focus: signal(None),
            filter: signal(String::new()),
            draft: signal(String::new()),
            page: signal(Page::Nodes),
            busy: signal(None),
            notice: signal(None),
        }
    }

    fn load(self) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        self.busy.set(Some("打开配置…"));
        self.notice.set(None);
        spawn(async move {
            let opened = blocking(|| {
                let session = Session::open(default_directory()?)?;
                // Isolated test/development workspaces must not recover a real network configuration.
                let recovery_error = if std::env::var_os("EXODYS_DATA_DIR").is_none() {
                    crate::system_proxy::recover().err()
                } else {
                    None
                };
                Ok::<_, String>((session, recovery_error))
            })
            .await;
            batch(|| {
                match opened {
                    Ok(Ok((session, recovery_error))) => {
                        if let Some(error) = recovery_error {
                            self.error(error);
                        }
                        let state = session.snapshot();
                        self.focus.set(
                            state
                                .preferences
                                .selected
                                .as_ref()
                                .map(|selection| selection.profile),
                        );
                        self.state.set(state);
                        self.session.set(Some(Arc::new(Mutex::new(session))));
                    }
                    Ok(Err(message)) => self.error(message),
                    Err(_) => self.error("无法打开配置。请重试。"),
                }
                self.busy.set(None);
            });
        });
    }

    fn error(self, message: impl Into<String>) {
        self.notice.set(Some(Notice {
            message: message.into(),
            error: true,
        }));
    }

    fn navigate(self, id: Option<u64>) {
        batch(|| {
            self.focus.set(id);
            self.filter.set(String::new());
            self.page.set(Page::Nodes);
        });
    }

    fn form(self, page: Page, value: impl Into<String>) {
        batch(|| {
            self.draft.set(value.into());
            self.notice.set(None);
            self.page.set(page);
        });
    }

    fn work(self, label: &'static str, job: Job, close_form: bool) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        let Some(session) = self.session.get_untracked() else {
            return;
        };
        let sync_tick = matches!(&job, Job::SyncTick);
        batch(|| {
            if !sync_tick {
                self.notice.set(None);
            }
            self.busy.set(Some(label));
        });
        let task = async move {
            let result = blocking(move || {
                let mut session = session
                    .lock()
                    .map_err(|_| "无法完成操作。请重新打开 Exodys。".to_owned())?;
                let outcome = session.run(job);
                Ok::<_, String>((session.snapshot(), outcome))
            })
            .await;
            batch(|| {
                match result {
                    Ok(Ok((state, outcome))) => {
                        let focus = self
                            .focus
                            .get_untracked()
                            .filter(|id| state.catalog.profile(*id).is_some());
                        let fallback = state
                            .preferences
                            .selected
                            .as_ref()
                            .map(|selected| selected.profile);
                        self.state.set(state);
                        match outcome {
                            Ok(outcome) => {
                                if close_form {
                                    self.draft.set(String::new());
                                    self.navigate(outcome.focus.or(focus).or(fallback));
                                } else if focus.is_none() {
                                    self.focus.set(fallback);
                                }
                                if !sync_tick || outcome.message.is_some() {
                                    self.notice.set(outcome.message.map(|message| Notice {
                                        message,
                                        error: false,
                                    }));
                                }
                            }
                            Err(message) => self.error(message),
                        }
                    }
                    Ok(Err(message)) => self.error(message),
                    Err(_) => self.error("后台操作意外结束。请重试。"),
                }
                self.busy.set(None);
            });
        };
        // A form can unmount its own button before the session task finishes.
        self.owner.run(|| spawn(task));
    }

    fn refresh_due(self) {
        if self.busy.get_untracked().is_some()
            || !matches!(self.page.get_untracked(), Page::Nodes | Page::Group)
        {
            return;
        }
        let due = self
            .session
            .get_untracked()
            .is_some_and(|session| session.try_lock().is_ok_and(|s| s.has_due_refresh()));
        if due {
            self.work("检查定时更新…", Job::RefreshDue, false);
        }
    }

    #[cfg(target_os = "macos")]
    fn sync_due(self) {
        if matches!(
            self.page.get_untracked(),
            Page::Nodes | Page::Group | Page::Sync
        ) && self
            .state
            .with_untracked(|state| !state.connected && state.sync.unlocked)
        {
            self.work("检查自动同步…", Job::SyncTick, false);
        }
    }

    fn check_connection(self) {
        if !self.state.with_untracked(|state| state.connected)
            || self.busy.get_untracked().is_some()
        {
            return;
        }
        let Some(session) = self.session.get_untracked() else {
            return;
        };
        // Restoring network preferences may need authorization. Only check child
        // status on the main thread; the session job handles restoration off it.
        let failed = session
            .try_lock()
            .is_ok_and(|mut session| session.connection_issue().is_some());
        if failed {
            self.work("清理已退出的代理…", Job::Disconnect, false);
        }
    }

    fn pick_file(self) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        self.busy.set(Some("选择文件…"));
        let request = pick_paths(
            OpenDialog::files()
                .filter(FileFilter::new(
                    "代理配置",
                    ["yaml", "yml", "json", "txt", "conf"],
                ))
                .button_label("导入"),
        );
        spawn(async move {
            let selection = request.await;
            self.busy.set(None);
            match selection {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.into_iter().next() {
                        self.work("读取配置…", Job::Import(Source::File(path)), true);
                    }
                }
                Ok(None) => {}
                Err(_) => self.error("无法打开文件选择器。你也可以粘贴配置内容。"),
            }
        });
    }
}

fn sidebar(ui: Ui) -> impl IntoElement {
    let profiles = computed(move || ui.state.with(|state| state.catalog.clone()));
    let panel = view()
        .test_id("profile-sidebar")
        .w(px(218.0))
        .m(px(9.0))
        .px(px(10.0))
        .pb(px(10.0))
        .gap(px(8.0))
        .rounded(px(16.0))
        .shadow_sm();
    #[cfg(target_os = "macos")]
    let panel = panel
        .pt(move || px((title_bar().top.0 - 9.0).max(10.0)))
        .macos_backdrop(macos_backdrop(MacOsBackdropMaterial::Sidebar));
    #[cfg(target_os = "windows")]
    let panel = panel.pt(px(10.0)).bg(windows::SIDEBAR);
    panel
        .child(
            view()
                .flex_row()
                .items_center()
                .h(px(34.0))
                .pl(px(6.0))
                .gap(px(6.0))
                .child(
                    text("配置")
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    text(move || profiles.with(|c| c.profiles.len().to_string()))
                        .text_size(px(12.0))
                        .text_color(SUBTLE),
                )
                .child(view().flex_1())
                .child(sidebar_button(ui, "导入配置", "plus", move || {
                    ui.form(Page::Import, "")
                })),
        )
        .child(dynamic(move || {
            if profiles.with(|catalog| catalog.profiles.is_empty()) {
                text("暂无配置")
                    .flex_1()
                    .text_color(SUBTLE)
                    .text_size(px(13.0))
                    .p(px(9.0))
                    .into_element()
            } else {
                #[cfg(target_os = "macos")]
                let list = native("exodys/profile-table")
                    .test_id("profile-table")
                    .flex_1()
                    .min_h(px(0.0))
                    .prop("rows", move || {
                        serde_json::Value::Array(profiles.with(|catalog| {
                            catalog
                                .profiles
                                .iter()
                                .map(|profile| {
                                    serde_json::json!({
                                        "id": profile.id,
                                        "name": profile.name,
                                        "count": profile.nodes.len(),
                                    })
                                })
                                .collect()
                        }))
                    })
                    .prop("selected", move || {
                        ui.focus
                            .get()
                            .map_or(serde_json::Value::Null, serde_json::Value::from)
                    })
                    .prop("disabled", move || {
                        serde_json::Value::Bool(ui.busy.get().is_some())
                    })
                    .on_native_event(move |event: &NativeEvent| {
                        if &*event.name == "select"
                            && let Some(id) = event.data.as_u64()
                        {
                            ui.navigate(Some(id));
                        }
                    })
                    .into_element();
                #[cfg(target_os = "windows")]
                let list = windows::profile_list(ui, profiles).into_element();
                list
            }
        }))
        .child(action(
            "策略组",
            Tone::Standard,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || ui.form(Page::Policies, ""),
        ))
        .child(action(
            "自动选择组",
            Tone::Standard,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || ui.page.set(Page::Group),
        ))
        .child(
            text(move || ui.state.with(connection_status))
                .test_id("connection-status")
                .pl(px(6.0))
                .text_size(px(12.0))
                .truncate(),
        )
        .child(connection(ui))
        .child(
            view()
                .flex_row()
                .items_center()
                .pl(px(6.0))
                .gap(px(6.0))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(3.0))
                        .child(
                            text(move || {
                                if ui.state.with(|s| s.tun_active) {
                                    "连接模式"
                                } else {
                                    "本地代理"
                                }
                            })
                            .text_color(SUBTLE)
                            .text_size(px(11.0)),
                        )
                        .child(
                            text(move || {
                                ui.state.with(|s| {
                                    if s.tun_active {
                                        "TUN".to_owned()
                                    } else {
                                        address(s.preferences.port).to_string()
                                    }
                                })
                            })
                            .text_size(px(13.0))
                            .truncate(),
                        ),
                )
                .child(sidebar_button(
                    ui,
                    "本地端口设置",
                    "gearshape",
                    move || {
                        ui.form(
                            Page::Settings,
                            ui.state
                                .with_untracked(|state| state.preferences.port.to_string()),
                        )
                    },
                )),
        )
}

fn sidebar_button(
    ui: Ui,
    label: &'static str,
    symbol: &'static str,
    press: impl Fn() + 'static,
) -> impl IntoElement {
    icon_action(
        label,
        symbol,
        move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
        press,
    )
}

fn icon_action(
    label: &'static str,
    symbol: &'static str,
    disabled: impl Fn() -> bool + 'static,
    press: impl Fn() + 'static,
) -> impl IntoElement {
    #[cfg(target_os = "macos")]
    let control = native_button(move || label, disabled, Tone::Standard, press)
        .prop("symbol", serde_json::Value::from(symbol))
        .size(px(30.0));
    #[cfg(target_os = "windows")]
    let control = windows::icon_button(label, symbol, disabled, press);
    control
}

fn connection_node(state: &Snapshot) -> Option<&str> {
    if state.preferences.auto_enabled {
        (!state.preferences.auto_members.is_empty()).then_some("自动选择组")
    } else {
        state.node().map(|node| node.name.as_str())
    }
}

fn connection_status(state: &Snapshot) -> String {
    let status = if state.connected {
        "运行中"
    } else {
        "已断开"
    };
    match (state.tun_active, connection_node(state)) {
        (true, Some(node)) => format!("{status} · TUN · {node}"),
        (true, None) => format!("{status} · TUN"),
        (false, node) => format!("{status} · {}", node.unwrap_or("未选择节点")),
    }
}

fn connection(ui: Ui) -> impl IntoElement {
    view()
        .test_id("connection-actions")
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            native_button(
                move || {
                    if ui.state.with(|s| s.connected) {
                        "断开"
                    } else {
                        "连接"
                    }
                },
                move || {
                    ui.busy.get().is_some()
                        || ui
                            .state
                            .with(|s| !s.connected && connection_node(s).is_none())
                },
                Tone::Primary,
                move || {
                    if ui.state.with_untracked(|s| s.connected) {
                        ui.work("断开连接…", Job::Disconnect, false);
                    } else if ui.state.with_untracked(|s| connection_node(s).is_some()) {
                        ui.work("启动本地代理…", Job::Connect, false);
                    }
                },
            )
            .h(px(30.0))
            .flex_1(),
        )
        .child(when(
            move || ui.state.with(|s| s.connected && !s.tun_active),
            move || {
                icon_action(
                    "测试代理",
                    "waveform.path.ecg",
                    move || ui.busy.get().is_some(),
                    move || {
                        if ui.state.with_untracked(|s| s.connected && !s.tun_active) {
                            ui.work(
                                "通过代理发送测试请求…",
                                Job::Probe("https://cp.cloudflare.com/generate_204".into()),
                                false,
                            );
                        }
                    },
                )
            },
        ))
        .child(when(
            move || ui.state.with(|s| !s.tun_active),
            move || {
                icon_action(
                    "复制地址",
                    "doc.on.doc",
                    || false,
                    move || {
                        if !ui.state.with_untracked(|s| s.tun_active) {
                            set_clipboard(
                                ui.state
                                    .with_untracked(|s| address(s.preferences.port).to_string()),
                            );
                            ui.notice.set(Some(Notice {
                                message: "已复制本地代理地址。".into(),
                                error: false,
                            }));
                        }
                    },
                )
            },
        ))
}

#[cfg(target_os = "windows")]
use windows::button as native_button;

#[cfg(target_os = "macos")]
fn native_button<T: Into<String>>(
    label: impl Fn() -> T + 'static,
    disabled: impl Fn() -> bool + 'static,
    tone: Tone,
    execute: impl Fn() + 'static,
) -> NativeView {
    let role = match tone {
        Tone::Primary => "primary",
        Tone::Standard => "standard",
        Tone::Danger => "destructive",
    };
    native("exodys/button")
        .prop("title", move || serde_json::Value::String(label().into()))
        .prop("disabled", move || serde_json::Value::Bool(disabled()))
        .prop("role", move || serde_json::Value::String(role.into()))
        .on_native_event(move |event: &NativeEvent| {
            if &*event.name == "press" {
                execute();
            }
        })
}

fn feedback(ui: Ui) -> impl IntoElement {
    when(
        move || ui.busy.get().is_some() || ui.notice.with(Option::is_some),
        move || {
            view()
                .flex_row()
                .gap(px(8.0))
                .items_center()
                .px(px(12.0))
                .py(px(8.0))
                .bg(SURFACE)
                .border_1()
                .border_color(LINE)
                .rounded(px(8.0))
                .child(
                    text(move || {
                        if let Some(busy) = ui.busy.get() {
                            return busy.to_owned();
                        }
                        ui.notice.with(|notice| {
                            notice
                                .as_ref()
                                .map(|notice| notice.message.clone())
                                .unwrap_or_default()
                        })
                    })
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(move || {
                        if ui.notice.with(|n| n.as_ref().is_some_and(|n| n.error)) {
                            ERROR
                        } else {
                            INK
                        }
                    }),
                )
                .child(when(
                    move || ui.busy.get().is_none(),
                    move || {
                        action(
                            "关闭提示",
                            Tone::Standard,
                            || false,
                            move || ui.notice.set(None),
                        )
                    },
                ))
        },
    )
}

fn nodes(ui: Ui) -> impl IntoElement {
    let current = computed(move || {
        ui.state.with(|state| {
            ui.focus
                .get()
                .and_then(|id| state.catalog.profile(id).cloned())
        })
    });
    view().flex_1().min_h(px(0.0)).child(dynamic(move || {
        if current.with(Option::is_some) {
            node_browser(ui, current).into_element()
        } else {
            empty_library(ui).into_element()
        }
    }))
}

fn empty_library(ui: Ui) -> impl IntoElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .p(px(18.0))
        .child(
            text(move || {
                if ui.session.with(Option::is_none) {
                    "配置库"
                } else {
                    "导入第一个配置"
                }
            })
            .text_size(px(21.0))
            .font_weight(FontWeight::SEMIBOLD),
        )
        .child(when(
            move || ui.session.with(Option::is_some),
            move || {
                view()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        text(
                            "粘贴订阅地址或节点链接，也可以打开 Clash YAML 或 sing-box JSON 文件。",
                        )
                        .text_color(SUBTLE)
                        .text_size(px(13.0)),
                    )
                    .child(action(
                        "导入配置",
                        Tone::Primary,
                        move || ui.busy.get().is_some(),
                        move || ui.form(Page::Import, ""),
                    ))
            },
        ))
        .child(when(
            move || ui.session.with(Option::is_none) && ui.busy.get().is_none(),
            move || action("重新加载", Tone::Standard, || false, move || ui.load()),
        ))
}

fn node_browser(ui: Ui, profile: Memo<Option<Arc<Profile>>>) -> impl IntoElement {
    let matches = computed(move || {
        profile.with(|profile| {
            profile
                .as_ref()
                .map(|profile| ui.filter.with(|query| profile.search(query)))
                .unwrap_or(Matches::Empty)
        })
    });
    let profile_id = computed(move || profile.with(|p| p.as_ref().map(|p| p.id)));
    let active = computed(move || {
        ui.state
            .with(|state| profile_id.get().is_some_and(|id| state.profile_in_use(id)))
    });
    let selected = computed(move || ui.state.with(|state| state.preferences.selected.clone()));
    let selection_disabled = move || {
        ui.busy.get().is_some()
            || ui
                .state
                .with(|s| s.connected && (s.tun_active || s.preferences.auto_enabled))
    };
    view()
        .flex_1()
        .min_h(px(0.0))
        .flex_col()
        .gap(px(11.0))
        .child(
            toolbar()
                .accessibility_label("配置工具栏")
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(4.0))
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    p.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                                })
                            })
                            .text_size(px(21.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate(),
                        )
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    format!(
                                        "{} / {} 个节点",
                                        matches.with(Matches::len),
                                        p.as_ref().map_or(0, |p| p.nodes.len())
                                    )
                                })
                            })
                            .text_size(px(12.0))
                            .text_color(SUBTLE),
                        ),
                )
                .child(when(
                    move || profile.with(|p| p.as_ref().is_some_and(|p| p.subscription.is_some())),
                    move || {
                        view()
                            .flex_row()
                            .gap(px(4.0))
                            .child(icon_action(
                                "刷新订阅",
                                "arrow.clockwise",
                                move || ui.busy.get().is_some() || active.get(),
                                move || {
                                    if let Some(id) = profile_id.get_untracked() {
                                        ui.work("刷新订阅…", Job::Refresh(id), false);
                                    }
                                },
                            ))
                            .child(icon_action(
                                "更新周期",
                                "clock",
                                move || ui.busy.get().is_some(),
                                move || {
                                    if let Some(p) = profile.get_untracked() {
                                        ui.form(
                                            Page::RefreshCycle(p.id),
                                            p.refresh_hours
                                                .unwrap_or(crate::catalog::DEFAULT_REFRESH_HOURS)
                                                .to_string(),
                                        );
                                    }
                                },
                            ))
                    },
                ))
                .child(icon_action(
                    "重命名",
                    "pencil",
                    move || ui.busy.get().is_some(),
                    move || {
                        if let Some(p) = profile.get_untracked() {
                            ui.form(Page::Rename(p.id), p.name.clone());
                        }
                    },
                ))
                .child(icon_action(
                    "删除",
                    "trash",
                    move || ui.busy.get().is_some() || active.get(),
                    move || {
                        if let Some(id) = profile_id.get_untracked() {
                            ui.form(Page::Remove(id), "");
                        }
                    },
                )),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(search_field(ui).flex_1().h(px(32.0)).min_w(px(180.0)))
                .child(group::member_button(ui, profile_id)),
        )
        .child({
            #[cfg(target_os = "macos")]
            let list = native("exodys/node-table")
                .test_id("node-table")
                .flex_1()
                .min_h(px(0.0))
                .prop("rows", move || {
                    serde_json::Value::Array(profile.with(|profile| {
                        matches.with(|matches| {
                            profile
                                .as_ref()
                                .map(|profile| {
                                    (0..matches.len())
                                        .filter_map(|position| {
                                            let index = matches.at(position)?;
                                            let node = profile.nodes.get(index)?;
                                            Some(serde_json::json!({
                                                "id": node.id,
                                                "name": node.name,
                                                "protocol": node.protocol(),
                                                "endpoint": node.endpoint(),
                                            }))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                    }))
                })
                .prop("selected", move || {
                    selected.with(|selection| {
                        selection
                            .as_ref()
                            .filter(|selection| Some(selection.profile) == profile_id.get())
                            .map_or(serde_json::Value::Null, |selection| {
                                serde_json::Value::String(selection.node.clone())
                            })
                    })
                })
                .prop("disabled", move || {
                    serde_json::Value::Bool(selection_disabled())
                })
                .on_native_event(move |event: &NativeEvent| {
                    if &*event.name == "select"
                        && !ui.state.with_untracked(|s| {
                            s.connected && (s.tun_active || s.preferences.auto_enabled)
                        })
                        && let Some(key) = event.data.as_str()
                        && let Some(id) = profile_id.get_untracked()
                    {
                        ui.work(
                            "应用节点选择…",
                            Job::Select(NodeRef {
                                profile: id,
                                node: key.to_owned(),
                            }),
                            false,
                        );
                    }
                });
            #[cfg(target_os = "windows")]
            let list = windows::node_list(ui, profile, matches, selected, selection_disabled);
            list
        })
        .child(when(
            move || matches.with(Matches::is_empty),
            || {
                text("没有匹配节点。试试其他搜索词。")
                    .text_color(SUBTLE)
                    .p(px(20.0))
            },
        ))
}

#[cfg(target_os = "macos")]
fn search_field(ui: Ui) -> NativeView {
    native("exodys/search-field")
        .test_id("node-search")
        .prop("value", move || ui.filter.get())
        .prop("placeholder", || {
            serde_json::Value::String("搜索节点名称、协议或地址".into())
        })
        .on_native_event(move |event: &NativeEvent| {
            if &*event.name == "change"
                && let Some(value) = event.data.as_str()
            {
                ui.filter.set(value.to_owned());
            }
        })
}

#[cfg(target_os = "windows")]
use windows::search_field;

fn import_form(ui: Ui) -> impl IntoElement {
    form_panel("导入配置")
        .child(
            text("粘贴订阅地址、节点链接或配置内容，也可以直接导入文件。请确认订阅来源。")
                .text_color(SUBTLE)
                .text_size(px(13.0)),
        )
        .child(
            editor(
                "exodys/text-area",
                ui.draft,
                "订阅地址或节点内容",
                "import-content",
                || {},
                EditorOptions {
                    auto_focus: true,
                    max_length: Some(MAX_BYTES as u32 + 1),
                    ..EditorOptions::default()
                },
            )
            .h(px(121.0)),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(action(
                    "从文件导入",
                    Tone::Standard,
                    move || ui.busy.get().is_some(),
                    move || ui.pick_file(),
                ))
                .child(view().flex_1())
                .child(cancel(ui))
                .child(action(
                    "导入",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.draft.with(|d| d.trim().is_empty()),
                    move || {
                        ui.work(
                            "导入配置…",
                            Job::Import(Source::Text(ui.draft.get_untracked())),
                            true,
                        );
                    },
                )),
        )
}

fn rename_form(ui: Ui, id: u64) -> impl IntoElement {
    let save = move || ui.work("保存名称…", Job::Rename(id, ui.draft.get_untracked()), true);
    form_panel("重命名配置")
        .child(
            editor(
                "exodys/text-field",
                ui.draft,
                "配置名称",
                "profile-name",
                save,
                EditorOptions {
                    auto_focus: true,
                    max_length: Some(120),
                    ..EditorOptions::default()
                },
            )
            .h(px(28.0)),
        )
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "保存名称",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.draft.with(|d| d.trim().is_empty()),
                    save,
                )),
        )
}

fn refresh_cycle_form(ui: Ui, id: u64) -> impl IntoElement {
    let save = move || match ui.draft.get_untracked().trim().parse::<u16>() {
        Ok(hours) if hours <= crate::catalog::MAX_REFRESH_HOURS => {
            ui.work("保存更新周期…", Job::SetRefreshHours(id, hours), true);
        }
        _ => ui.error("请输入 0 至 720 之间的整数。"),
    };
    form_panel("订阅更新周期")
        .child(
            text("开启定时更新后，按此周期间隔刷新。默认 24 小时，填 0 则只手动刷新这条订阅。")
                .text_color(SUBTLE),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    editor(
                        "exodys/text-field",
                        ui.draft,
                        "24",
                        "refresh-hours",
                        save,
                        EditorOptions {
                            max_length: Some(3),
                            ..EditorOptions::default()
                        },
                    )
                    .h(px(28.0))
                    .w(px(90.0)),
                )
                .child(text("小时")),
        )
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "保存周期",
                    Tone::Primary,
                    move || ui.busy.get().is_some(),
                    save,
                )),
        )
}

fn remove_form(ui: Ui, id: u64) -> impl IntoElement {
    form_panel("删除配置？")
        .child(
            text(move || {
                ui.state.with(|state| {
                    state
                        .catalog
                        .profile(id)
                        .map(|profile| {
                            format!(
                                "将从本机移除「{}」及其 {} 个节点。",
                                profile.name,
                                profile.nodes.len()
                            )
                        })
                        .unwrap_or_default()
                })
            })
            .text_color(SUBTLE),
        )
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "删除配置",
                    Tone::Danger,
                    move || ui.busy.get().is_some(),
                    move || ui.work("删除配置…", Job::Remove(id), true),
                )),
        )
}

fn settings_form(ui: Ui) -> impl IntoElement {
    let save = move || match ui.draft.with_untracked(|draft| draft.trim().parse::<u16>()) {
        Ok(port) if port >= 1024 => ui.work("保存端口…", Job::SetPort(port), true),
        _ => ui.error("请输入 1024 至 65535 之间的端口。"),
    };
    form_panel("本地代理设置")
        .child(
            text("在需要代理的应用中使用 127.0.0.1 和下方端口。HTTP 与 SOCKS5 共用此端口。")
                .text_color(SUBTLE)
                .text_size(px(13.0)),
        )
        .child(
            text("本地端口")
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            editor(
                "exodys/text-field",
                ui.draft,
                "2080",
                "local-port",
                save,
                EditorOptions {
                    max_length: Some(5),
                    readonly: Some(computed(move || ui.state.with(|s| s.connected))),
                    ..EditorOptions::default()
                },
            )
            .h(px(28.0))
            .w(px(130.0)),
        )
        .child(when(
            move || ui.state.with(|s| s.connected),
            || text("断开连接后可修改端口。").text_color(SUBTLE),
        ))
        .child(
            view()
                .flex_row()
                .gap(px(8.0))
                .child(action(
                    "分流规则",
                    Tone::Standard,
                    move || ui.busy.get().is_some(),
                    move || {
                        ui.form(
                            Page::Routing,
                            ui.state
                                .with_untracked(|s| s.preferences.routing_rules.clone()),
                        );
                    },
                ))
                .child(action(
                    "规则集",
                    Tone::Standard,
                    move || ui.busy.get().is_some(),
                    move || {
                        ui.form(Page::RuleSets, "");
                    },
                )),
        )
        .children({
            #[cfg(target_os = "macos")]
            let settings = macos_connection_settings(ui);
            #[cfg(target_os = "windows")]
            let settings = [] as [AnyElement; 0];
            settings
        })
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(native_button(
                    move || {
                        if ui.state.with(|s| s.preferences.scheduled_refresh) {
                            "关闭定时更新"
                        } else {
                            "启用定时更新"
                        }
                    },
                    move || ui.busy.get().is_some(),
                    Tone::Standard,
                    move || {
                        ui.work(
                            "保存更新设置…",
                            Job::SetScheduledRefresh(
                                !ui.state.with_untracked(|s| s.preferences.scheduled_refresh),
                            ),
                            false,
                        )
                    },
                ))
                .child(
                    text("应用打开时按订阅周期更新；跳过当前连接使用的配置。")
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(13.0))
                        .text_color(SUBTLE),
                ),
        )
        .child(settings_file::controls(ui))
        .children({
            #[cfg(target_os = "macos")]
            let settings = [view()
                .flex_row()
                .child(action(
                    "备份与同步",
                    Tone::Standard,
                    move || ui.busy.get().is_some(),
                    move || ui.form(Page::Backup, ""),
                ))
                .into_element()];
            #[cfg(target_os = "windows")]
            let settings = [] as [AnyElement; 0];
            settings
        })
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "保存端口",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.state.with(|s| s.connected),
                    save,
                )),
        )
}

#[cfg(target_os = "macos")]
fn macos_connection_settings(ui: Ui) -> [AnyElement; 2] {
    [
        text("macOS 系统代理只影响遵循系统 HTTP/HTTPS 代理的应用。连接时可能需要授权，断开时恢复原设置。").text_size(px(13.0)).text_color(SUBTLE).into_element(),
        view().flex_row().gap(px(8.0))
            .child(native_button(
                move || if ui.state.with(|s| s.preferences.system_proxy) { "关闭系统代理模式" } else { "启用系统代理模式" },
                move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), Tone::Standard,
                move || ui.work("保存系统代理选项…", Job::SetSystemProxy(!ui.state.with_untracked(|s| s.preferences.system_proxy)), false),
            ))
            .child(action("恢复系统代理", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || ui.work("检查系统代理…", Job::RecoverSystemProxy, false)))
            .child(native_button(
                move || if ui.state.with(|s| s.tun_active) { "断开 TUN" } else { "连接 TUN" },
                move || ui.busy.get().is_some() || ui.state.with(|s| s.connected && !s.tun_active),
                Tone::Standard,
                move || if ui.state.with_untracked(|s| s.tun_active) {
                    ui.work("停止 TUN…", Job::Disconnect, false);
                } else {
                    ui.work("正在连接 TUN…", Job::ConnectTun, false);
                },
            )).into_element(),
    ]
}

fn pick_clash_rules(ui: Ui) {
    if ui.busy.get_untracked().is_some() || ui.state.with_untracked(|s| s.connected) {
        return;
    }
    ui.notice.set(None);
    ui.busy.set(Some("选择规则文件…"));
    let request = pick_paths(
        OpenDialog::files()
            .filter(FileFilter::new("Clash YAML", ["yaml", "yml"]))
            .button_label("载入草稿"),
    );
    spawn(async move {
        match request.await {
            Ok(Some(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    ui.busy.set(Some("检查规则文件…"));
                    match blocking(move || crate::routing::read_clash_rules(&path)).await {
                        Ok(Ok(draft)) => {
                            ui.draft.set(draft);
                            ui.notice.set(Some(Notice {
                                message: "已载入规则草稿。请检查后保存。".into(),
                                error: false,
                            }));
                        }
                        Ok(Err(error)) => ui.error(error),
                        Err(_) => ui.error("读取规则文件时发生错误。"),
                    }
                }
            }
            Ok(None) => (),
            Err(_) => ui.error("无法打开规则文件选择器。"),
        }
        ui.busy.set(None);
    });
}

fn routing_form(ui: Ui) -> impl IntoElement {
    let save = move || {
        ui.work(
            "保存分流规则…",
            Job::SetRouting(ui.draft.get_untracked()),
            true,
        )
    };
    form_panel("分流规则")
        .child(text("从上到下匹配，未命中时按当前连接模式代理。每行一条，例如 DOMAIN-SUFFIX,example.org,DIRECT。").text_size(px(13.0)))
        .child(text("支持 DOMAIN、DOMAIN-SUFFIX、DOMAIN-KEYWORD、IP-CIDR、IP-CIDR6、RULE-SET、GEOIP、GEOSITE 和 MATCH。动作可选 DIRECT、PROXY、REJECT 或 GROUP:组名。").text_size(px(13.0)).text_color(SUBTLE))
        .child(editor("exodys/text-area", ui.draft, "分流规则", "routing-rules", || {}, EditorOptions { readonly: Some(computed(move || ui.busy.get().is_some() || ui.state.with(|s| s.connected))), ..EditorOptions::default() }).h(px(150.0)))
        .child(when(move || ui.state.with(|s| s.connected), || text("断开连接后可修改分流规则。")))
        .child(text("从 Clash YAML 载入会替换草稿；检查并保存后才生效。").text_size(px(13.0)).text_color(SUBTLE))
        .child(view().flex_row().items_center().gap(px(8.0))
            .child(action("从 YAML 载入草稿", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || pick_clash_rules(ui)))
            .child(view().flex_1()).child(cancel(ui)).child(action("保存规则", Tone::Primary,
            move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), save)))
}

fn form_panel(title: &'static str) -> View {
    view()
        .bg(SURFACE)
        .rounded(px(10.0))
        .border_1()
        .border_color(LINE)
        .p(px(17.0))
        .gap(px(12.0))
        .child(
            text(title)
                .text_size(px(20.0))
                .font_weight(FontWeight::SEMIBOLD),
        )
}

fn cancel(ui: Ui) -> impl IntoElement {
    action(
        "取消",
        Tone::Standard,
        move || ui.busy.get().is_some(),
        move || {
            ui.draft.set(String::new());
            ui.page.set(Page::Nodes);
        },
    )
}

#[derive(Default)]
struct EditorOptions {
    auto_focus: bool,
    max_length: Option<u32>,
    readonly: Option<Memo<bool>>,
}

#[cfg(target_os = "windows")]
use windows::editor;

#[cfg(target_os = "macos")]
fn editor(
    component: &'static str,
    value: Signal<String>,
    label: &'static str,
    id: &'static str,
    submit: impl Fn() + 'static,
    options: EditorOptions,
) -> NativeView {
    let mut field = native(component);
    if let Some(limit) = options.max_length {
        field = field.prop("max_length", serde_json::Value::from(limit));
    }
    if let Some(readonly) = options.readonly {
        field = field.prop("readonly", move || serde_json::Value::Bool(readonly.get()));
    }
    if options.auto_focus {
        field = field.prop("auto_focus", serde_json::Value::Bool(true));
    }
    field
        .key(id)
        .test_id(id)
        .prop("value", move || serde_json::Value::String(value.get()))
        .prop("placeholder", serde_json::Value::String(label.into()))
        .on_native_event(move |event: &NativeEvent| {
            if let Some(text) = event.data.as_str() {
                match &*event.name {
                    "change" => value.set(text.to_owned()),
                    "submit" => {
                        value.set(text.to_owned());
                        submit();
                    }
                    _ => {}
                }
            }
        })
}

enum Tone {
    Primary,
    Standard,
    Danger,
}

fn action(
    label: &'static str,
    tone: Tone,
    disabled: impl Fn() -> bool + 'static,
    execute: impl Fn() + 'static,
) -> impl IntoElement {
    native_button(move || label, disabled, tone, execute)
        .min_w(px(64.0))
        .min_h(px(24.0))
}
