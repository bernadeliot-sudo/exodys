use std::{cell::RefCell, rc::Rc};

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
        runtime::ProtocolObject,
    },
    objc2_app_kit::{
        NSAccessibility, NSAutoresizingMaskOptions, NSControlTextEditingDelegate, NSScrollView,
        NSTableCellView, NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDelegate,
        NSTableViewStyle, NSTextField, NSUserInterfaceItemIdentification, NSView,
    },
    objc2_foundation::{
        NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    },
};
use serde_json::Value;

const PROFILE_TABLE: &str = "exodys/profile-table";
const NODE_TABLE: &str = "exodys/node-table";

pub(super) fn register(components: &Components) {
    components.register(
        PROFILE_TABLE,
        Rc::new(|context| Box::new(Table::new(context, TableKind::Profiles))),
    );
    components.register(
        NODE_TABLE,
        Rc::new(|context| Box::new(Table::new(context, TableKind::Nodes))),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableKind {
    Profiles,
    Nodes,
}

#[derive(Clone, Debug)]
struct ProfileRow {
    id: u64,
    name: String,
    count: usize,
}

#[derive(Clone, Debug)]
struct NodeRow {
    id: String,
    name: String,
    protocol: String,
    endpoint: String,
}

#[derive(Default)]
struct TableData {
    profiles: Vec<ProfileRow>,
    nodes: Vec<NodeRow>,
    selected: Option<String>,
    suppress_selection_event: std::cell::Cell<bool>,
}

struct TargetState {
    context: ComponentContext,
    kind: TableKind,
    data: Rc<RefCell<TableData>>,
    table: RefCell<Option<Retained<NSTableView>>>,
}

struct Table {
    scroll: Retained<NSScrollView>,
    table: Retained<NSTableView>,
    _target: Retained<TableTarget>,
    kind: TableKind,
    data: Rc<RefCell<TableData>>,
}

impl Table {
    fn new(context: ComponentContext, kind: TableKind) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let table = NSTableView::new(mtm);
        table.setAllowsMultipleSelection(false);
        table.setAllowsEmptySelection(true);
        table.setStyle(match kind {
            TableKind::Profiles => NSTableViewStyle::Plain,
            TableKind::Nodes => NSTableViewStyle::Inset,
        });
        table.setRowHeight(match kind {
            TableKind::Profiles => 30.0,
            TableKind::Nodes => 29.0,
        });
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setAccessibilityIdentifier(Some(&NSString::from_str(match kind {
            TableKind::Profiles => "native-profile-table",
            TableKind::Nodes => "native-node-table",
        })));
        table.setAccessibilityLabel(Some(&NSString::from_str(match kind {
            TableKind::Profiles => "配置列表",
            TableKind::Nodes => "节点列表",
        })));

        match kind {
            TableKind::Profiles => {
                table.addTableColumn(&column(mtm, "name", "配置", 126.0));
                table.addTableColumn(&column(mtm, "count", "节点", 62.0));
                table.setHeaderView(None);
                table.setBackgroundColor(&nsh_platform_macos::objc2_app_kit::NSColor::clearColor());
            }
            TableKind::Nodes => {
                table.addTableColumn(&column(mtm, "name", "节点", 260.0));
                table.addTableColumn(&column(mtm, "protocol", "协议", 100.0));
                table.addTableColumn(&column(mtm, "endpoint", "地址", 220.0));
            }
        }

        let data = Rc::new(RefCell::new(TableData::default()));
        let target: Retained<TableTarget> = unsafe {
            msg_send![
                super(TableTarget::alloc(mtm).set_ivars(TargetState {
                    context,
                    kind,
                    data: data.clone(),
                    table: RefCell::new(None),
                })),
                init
            ]
        };
        unsafe {
            let data_source: &ProtocolObject<dyn NSTableViewDataSource> =
                ProtocolObject::from_ref(&*target);
            let delegate: &ProtocolObject<dyn NSTableViewDelegate> =
                ProtocolObject::from_ref(&*target);
            table.setDataSource(Some(data_source));
            table.setDelegate(Some(delegate));
        }
        *target.ivars().table.borrow_mut() = Some(table.clone());

        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&table));

        Self {
            scroll,
            table,
            _target: target,
            kind,
            data,
        }
    }

    fn set_rows(&mut self, value: Option<&Value>) {
        let rows = value.and_then(Value::as_array);
        let mut data = self.data.borrow_mut();
        match self.kind {
            TableKind::Profiles => {
                data.profiles = rows
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .filter_map(|row| {
                        Some(ProfileRow {
                            id: row.get("id")?.as_u64()?,
                            name: row.get("name")?.as_str()?.to_owned(),
                            count: row.get("count")?.as_u64()? as usize,
                        })
                    })
                    .collect();
            }
            TableKind::Nodes => {
                data.nodes = rows
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .filter_map(|row| {
                        Some(NodeRow {
                            id: row.get("id")?.as_str()?.to_owned(),
                            name: row.get("name")?.as_str()?.to_owned(),
                            protocol: row.get("protocol")?.as_str()?.to_owned(),
                            endpoint: row.get("endpoint")?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
            }
        }
        data.suppress_selection_event.set(true);
        drop(data);
        self.table.reloadData();
        self.sync_selection();
    }

    fn set_selected(&mut self, value: Option<&Value>) {
        let mut data = self.data.borrow_mut();
        data.selected = match self.kind {
            TableKind::Profiles => value.and_then(Value::as_u64).map(|id| id.to_string()),
            TableKind::Nodes => value.and_then(Value::as_str).map(str::to_owned),
        };
        drop(data);
        self.sync_selection();
    }

    fn sync_selection(&self) {
        let selected = self.data.borrow().selected.clone();
        let row = selected.and_then(|selected| match self.kind {
            TableKind::Profiles => self
                .data
                .borrow()
                .profiles
                .iter()
                .position(|profile| profile.id.to_string() == selected),
            TableKind::Nodes => self
                .data
                .borrow()
                .nodes
                .iter()
                .position(|node| node.id == selected),
        });
        self.data.borrow().suppress_selection_event.set(true);
        if let Some(row) = row {
            let indexes = NSIndexSet::indexSetWithIndex(row);
            self.table
                .selectRowIndexes_byExtendingSelection(&indexes, false);
        } else if self.table.selectedRow() >= 0 {
            self.table.deselectRow(self.table.selectedRow());
        }
        self.data.borrow().suppress_selection_event.set(false);
    }
}

impl NativeComponent for Table {
    fn view(&self) -> Retained<NSView> {
        Retained::into_super(self.scroll.clone())
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "rows" => self.set_rows(value),
            "selected" => self.set_selected(value),
            "disabled" => {
                self.table
                    .setEnabled(!value.and_then(Value::as_bool).unwrap_or(false));
                self.sync_selection();
            }
            _ => {}
        }
    }
}

fn column(
    mtm: MainThreadMarker,
    identifier: &str,
    title: &str,
    width: f64,
) -> Retained<NSTableColumn> {
    let identifier = NSString::from_str(identifier);
    let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &identifier);
    column.setTitle(&NSString::from_str(title));
    column.setWidth(width);
    column
}

impl TargetState {
    fn row_count(&self) -> usize {
        let data = self.data.borrow();
        match self.kind {
            TableKind::Profiles => data.profiles.len(),
            TableKind::Nodes => data.nodes.len(),
        }
    }

    fn value(&self, column: Option<&NSTableColumn>, row: NSInteger) -> Option<String> {
        let row = usize::try_from(row).ok()?;
        let identifier = column?.identifier().to_string();
        let data = self.data.borrow();
        match self.kind {
            TableKind::Profiles => {
                let profile = data.profiles.get(row)?;
                match identifier.as_str() {
                    "name" => Some(profile.name.clone()),
                    "count" => Some(format!("{} 个", profile.count)),
                    _ => None,
                }
            }
            TableKind::Nodes => {
                let node = data.nodes.get(row)?;
                match identifier.as_str() {
                    "name" => Some(node.name.clone()),
                    "protocol" => Some(node.protocol.clone()),
                    "endpoint" => Some(node.endpoint.clone()),
                    _ => None,
                }
            }
        }
    }

    fn cell(
        &self,
        table: &NSTableView,
        column: &NSTableColumn,
        row: NSInteger,
    ) -> Option<Retained<NSView>> {
        let value = self.value(Some(column), row)?;
        let id = column.identifier();
        let cell = unsafe { table.makeViewWithIdentifier_owner(&id, None) }
            .and_then(|v| v.downcast::<NSTableCellView>().ok())
            .unwrap_or_else(|| {
                let frame = NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(column.width(), table.rowHeight()),
                );
                let cell =
                    NSTableCellView::initWithFrame(NSTableCellView::alloc(table.mtm()), frame);
                cell.setIdentifier(Some(&id));
                let label = NSTextField::labelWithString(&NSString::new(), table.mtm());
                let height = label.intrinsicContentSize().height;
                label.setFrame(NSRect::new(
                    NSPoint::new(2.0, (frame.size.height - height) * 0.5),
                    NSSize::new(frame.size.width - 4.0, height),
                ));
                label.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable
                        | NSAutoresizingMaskOptions::ViewMinYMargin
                        | NSAutoresizingMaskOptions::ViewMaxYMargin,
                );
                cell.addSubview(&label);
                unsafe { cell.setTextField(Some(&label)) };
                cell
            });
        unsafe { cell.textField() }?.setStringValue(&NSString::from_str(&value));
        Some(Retained::into_super(cell))
    }

    fn select(&self, row: NSInteger) {
        if self.data.borrow().suppress_selection_event.get() {
            return;
        }
        let Some(row) = usize::try_from(row).ok() else {
            return;
        };
        let data = self.data.borrow();
        match self.kind {
            TableKind::Profiles => {
                let Some(profile) = data.profiles.get(row) else {
                    return;
                };
                if data.selected.as_deref() == Some(&profile.id.to_string()) {
                    return;
                }
                self.context.emit("select", Value::from(profile.id));
            }
            TableKind::Nodes => {
                let Some(node) = data.nodes.get(row) else {
                    return;
                };
                if data.selected.as_deref() == Some(node.id.as_str()) {
                    return;
                }
                self.context.emit("select", Value::String(node.id.clone()));
            }
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetState]
    #[name = "ExodysTableTarget"]
    struct TableTarget;

    unsafe impl NSObjectProtocol for TableTarget {}
    unsafe impl NSControlTextEditingDelegate for TableTarget {}
    unsafe impl NSTableViewDataSource for TableTarget {}
    unsafe impl NSTableViewDelegate for TableTarget {}

    impl TableTarget {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().row_count() as NSInteger
        }

        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn cell_view(&self, table: &NSTableView, column: Option<&NSTableColumn>, row: NSInteger) -> Option<Retained<NSView>> {
            column.and_then(|column| self.ivars().cell(table, column, row))
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _notification: &NSNotification) {
            let Some(table) = self.ivars().table.borrow().as_ref().cloned() else {
                return;
            };
            self.ivars().select(table.selectedRow());
        }
    }
);
