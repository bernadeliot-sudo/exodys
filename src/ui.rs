use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsh_base::toolbar;
use nsh_core::desktop::WindowAttributes;
use nsh_core::element::desktop::current_window;
use nsh_core::file_dialog::{FileFilter, OpenDialog};
use nsh_core::prelude::*;

use crate::catalog::{Matches, NodeRef, Profile, default_directory};
use crate::engine::address;
use crate::import::{MAX_BYTES, Source};
use crate::session::{Job, Session, Snapshot};

// macOS semantic colors: quiet surfaces, system blue for actions, and a
// source-list selection tint instead of the previous dashboard palette.
const PAPER: AdaptiveColor = AdaptiveColor::rgb(0xf5f5f7, 0x1c1c1e);
const SURFACE: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x2c2c2e);
const INK: AdaptiveColor = AdaptiveColor::rgb(0x1d1d1f, 0xf5f5f7);
const SUBTLE: AdaptiveColor = AdaptiveColor::rgb(0x6e6e73, 0x98989d);
const LINE: AdaptiveColor = AdaptiveColor::rgb(0xd6d6dc, 0x48484a);
const ERROR: AdaptiveColor = AdaptiveColor::rgb(0xff3b30, 0xff6961);
const SUCCESS: AdaptiveColor = AdaptiveColor::rgb(0x34c759, 0x30d158);

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
    window.min_size = Some(Some(size(px(740.0), px(560.0))));
    current_window().set(window);
    let ui = Ui::new();
    ui.load();
    cleanup(move || {
        if let Some(session) = ui.session.get_untracked()
            && let Ok(mut session) = session.try_lock()
        {
            session.disconnect();
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
        .bg(PAPER)
        .macos_backdrop(macos_backdrop(MacOsBackdropMaterial::WindowBackground))
        .text_color(INK)
        .child(sidebar(ui))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .flex_col()
                .bg(PAPER)
                .child(connection(ui))
                .child(feedback(ui))
                .child(
                    view()
                        .flex_1()
                        .min_h(px(0.0))
                        .p(px(22.0))
                        .child(dynamic(move || match ui.page.get() {
                            Page::Nodes => nodes(ui).into_element(),
                            Page::Import => import_form(ui).into_element(),
                            Page::Rename(id) => rename_form(ui, id).into_element(),
                            Page::Remove(id) => remove_form(ui, id).into_element(),
                            Page::Settings => settings_form(ui).into_element(),
                        })),
                ),
        )
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Nodes,
    Import,
    Rename(u64),
    Remove(u64),
    Settings,
}

#[derive(Clone, PartialEq)]
struct Notice {
    message: String,
    error: bool,
}

#[derive(Clone, Copy)]
struct Ui {
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
            let opened = blocking(|| default_directory().and_then(Session::open)).await;
            batch(|| {
                match opened {
                    Ok(Ok(session)) => {
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
        batch(|| {
            self.notice.set(None);
            self.busy.set(Some(label));
        });
        spawn(async move {
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
                                }
                                self.notice.set(outcome.message.map(|message| Notice {
                                    message,
                                    error: false,
                                }));
                            }
                            Err(message) => self.error(message),
                        }
                    }
                    Ok(Err(message)) => self.error(message),
                    Err(_) => self.error("后台操作意外结束。请重试。"),
                }
                self.busy.set(None);
            });
        });
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
        if let Ok(mut session) = session.try_lock()
            && session.check_connection()
        {
            batch(|| {
                self.state.set(session.snapshot());
                self.error("sing-box 已退出。本地代理已断开，请重新连接。");
            });
        };
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
    view()
        .w(px(220.0))
        .flex_col()
        .p(px(12.0))
        .gap(px(10.0))
        .border_r_1()
        .border_color(LINE)
        .macos_backdrop(macos_backdrop(MacOsBackdropMaterial::Sidebar))
        .child(
            view().h(px(34.0)).px(px(8.0)).items_center().child(
                text("Exodys")
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD),
            ),
        )
        .child(action(
            "导入配置",
            Tone::Primary,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || ui.form(Page::Import, ""),
        ))
        .child(
            view()
                .flex_row()
                .justify_between()
                .items_center()
                .h(px(24.0))
                .px(px(8.0))
                .child(
                    text("配置")
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(SUBTLE),
                )
                .child(
                    text(move || profiles.with(|c| c.profiles.len().to_string()))
                        .text_size(px(11.0))
                        .text_color(SUBTLE),
                ),
        )
        .child(dynamic(move || {
            if profiles.with(|catalog| catalog.profiles.is_empty()) {
                text("暂无配置")
                    .text_color(SUBTLE)
                    .text_size(px(13.0))
                    .p(px(9.0))
                    .into_element()
            } else {
                native("exodys/profile-table")
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
                    .into_element()
            }
        }))
        .child(action(
            "本地端口设置",
            Tone::Standard,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || {
                ui.form(
                    Page::Settings,
                    ui.state
                        .with_untracked(|state| state.preferences.port.to_string()),
                );
            },
        ))
}

fn connection(ui: Ui) -> impl IntoElement {
    let connected = computed(move || ui.state.with(|s| s.connected));
    let selected_name = computed(move || ui.state.with(|s| s.node().map(|node| node.name.clone())));
    let socket = computed(move || ui.state.with(|s| address(s.preferences.port).to_string()));
    toolbar()
        .accessibility_label("连接工具栏")
        .h(px(52.0))
        .px(px(16.0))
        .flex_row()
        .items_center()
        .gap(px(9.0))
        .border_b_1()
        .border_color(LINE)
        .child(
            view()
                .size(px(8.0))
                .rounded_full()
                .bg(move || if connected.get() { SUCCESS } else { SUBTLE }),
        )
        .child(
            text(move || {
                if connected.get() {
                    "运行中"
                } else {
                    "已断开"
                }
            })
            .text_size(px(12.0))
            .text_color(SUBTLE),
        )
        .child(
            view().flex_1().min_w(px(0.0)).pl(px(7.0)).child(
                text(move || {
                    selected_name.with(|name| name.as_deref().unwrap_or("未选择节点").to_owned())
                })
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(14.0)),
            ),
        )
        .child(
            text(move || format!("HTTP / SOCKS5  ·  {}", socket.get()))
                .text_size(px(11.0))
                .text_color(SUBTLE)
                .truncate(),
        )
        .child(panel_button("复制地址", move || {
            set_clipboard(socket.get_untracked());
            ui.notice.set(Some(Notice {
                message: "已复制本地代理地址。".into(),
                error: false,
            }));
        }))
        .child(when(
            move || connected.get(),
            move || {
                panel_button("测试代理", move || {
                    ui.work(
                        "通过代理发送测试请求…",
                        Job::Probe("https://cp.cloudflare.com/generate_204".into()),
                        false,
                    );
                })
            },
        ))
        .child(
            native_button(
                move || if connected.get() { "断开" } else { "连接" },
                move || ui.busy.get().is_some() || selected_name.with(Option::is_none),
                Tone::Primary,
                move || {
                    if connected.get_untracked() {
                        ui.work("断开连接…", Job::Disconnect, false);
                    } else {
                        ui.work("启动本地代理…", Job::Connect, false);
                    }
                },
            )
            .test_id("connection-toggle")
            .min_w(px(70.0)),
        )
}

fn panel_button(label: &'static str, execute: impl Fn() + 'static) -> impl IntoElement {
    native_button(move || label, || false, Tone::Standard, execute)
}

fn native_button(
    label: impl Fn() -> &'static str + 'static,
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
        .rounded(px(10.0))
        .border_1()
        .border_color(LINE)
        .bg(SURFACE)
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
        ui.state.with(|state| {
            state.connected
                && state
                    .preferences
                    .selected
                    .as_ref()
                    .is_some_and(|node| Some(node.profile) == profile_id.get())
        })
    });
    let selected = computed(move || ui.state.with(|state| state.preferences.selected.clone()));
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
                        action(
                            "刷新订阅",
                            Tone::Standard,
                            move || ui.busy.get().is_some() || active.get(),
                            move || {
                                if let Some(id) = profile_id.get_untracked() {
                                    ui.work("刷新订阅…", Job::Refresh(id), false);
                                }
                            },
                        )
                    },
                ))
                .child(action(
                    "重命名",
                    Tone::Standard,
                    move || ui.busy.get().is_some(),
                    move || {
                        if let Some(p) = profile.get_untracked() {
                            ui.form(Page::Rename(p.id), p.name.clone());
                        }
                    },
                ))
                .child(action(
                    "删除",
                    Tone::Standard,
                    move || ui.busy.get().is_some() || active.get(),
                    move || {
                        if let Some(id) = profile_id.get_untracked() {
                            ui.form(Page::Remove(id), "");
                        }
                    },
                )),
        )
        .child(
            native("exodys/search-field")
                .test_id("node-search")
                .h(px(32.0))
                .min_w(px(180.0))
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
                }),
        )
        .child(
            native("exodys/node-table")
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
                    serde_json::Value::Bool(ui.busy.get().is_some())
                })
                .on_native_event(move |event: &NativeEvent| {
                    if &*event.name == "select"
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
                }),
        )
        .child(when(
            move || matches.with(Matches::is_empty),
            || {
                text("没有匹配节点。试试其他搜索词。")
                    .text_color(SUBTLE)
                    .p(px(20.0))
            },
        ))
}

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
            )
            .h(px(121.0))
            .prop("auto_focus", serde_json::Value::Bool(true))
            .prop("max_length", serde_json::Value::from(MAX_BYTES + 1)),
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
            )
            .h(px(28.0))
            .prop("auto_focus", serde_json::Value::Bool(true))
            .prop("max_length", serde_json::Value::from(120)),
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
            editor("exodys/text-field", ui.draft, "2080", "local-port", save)
                .h(px(28.0))
                .w(px(130.0))
                .prop("max_length", serde_json::Value::from(5))
                .prop("readonly", move || {
                    serde_json::Value::Bool(ui.state.with(|s| s.connected))
                }),
        )
        .child(when(
            move || ui.state.with(|s| s.connected),
            || text("断开连接后可修改端口。").text_color(SUBTLE),
        ))
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

fn editor(
    component: &'static str,
    value: Signal<String>,
    label: &'static str,
    id: &'static str,
    submit: impl Fn() + 'static,
) -> NativeView {
    native(component)
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
