use std::{cell::RefCell, rc::Rc};

use lsp_types::{Range, Uri};
use serde::Serialize;

use crate::{
    constants::{OYarn, SymType},
    core::{
        file_mgr::FileMgr,
        symbols::{
            module_symbol::ModuleSymbol,
            package_symbol::PackageSymbol,
            symbol::Symbol,
        },
    },
    threads::SessionInfo,
};

#[derive(Debug, Serialize, Clone)]
pub struct OverrideMarkerTarget {
    pub uri: Uri,
    pub range: Range,
}

#[derive(Debug, Serialize, Clone)]
pub struct OverrideMarker {
    pub line: u32,
    pub kind: String,
    pub targets: Vec<OverrideMarkerTarget>,
}

#[derive(Debug, Serialize, Clone)]
pub struct PublishOverrideMarkersParams {
    pub uri: Uri,
    pub markers: Vec<OverrideMarker>,
}

/// Compute override markers for all Odoo model classes in a file and send them to the client.
/// Called after file validation completes for in-workspace Python files.
pub fn compute_and_publish_override_markers(session: &mut SessionInfo, file_sym: &Rc<RefCell<Symbol>>) {
    if !file_sym.borrow().in_workspace() {
        return;
    }

    let file_path = match file_sym.borrow().paths().first().cloned() {
        Some(p) => p,
        None => return,
    };

    let uri = FileMgr::pathname2uri(&file_path);
    let mut markers: Vec<OverrideMarker> = Vec::new();

    let class_syms = get_classes_in_file_symbol(file_sym);

    for class_sym in &class_syms {
        let model_name = {
            let class = class_sym.borrow();
            if class.typ() != SymType::CLASS {
                continue;
            }
            match class.as_class_sym()._model.as_ref() {
                Some(m) if !m.name.is_empty() => m.name.clone(),
                _ => continue,
            }
        };

        let model = match session.sync_odoo.models.get(&model_name).cloned() {
            Some(m) => m,
            None => continue,
        };

        // get_symbols(session, None) returns all symbols regardless of module
        let all_classes = model.borrow().get_symbols(session, None);
        let other_classes: Vec<Rc<RefCell<Symbol>>> = all_classes
            .into_iter()
            .filter(|s| !Rc::ptr_eq(s, class_sym))
            .collect();

        let current_module = class_sym.borrow().find_module();

        let func_entries: Vec<(OYarn, Rc<RefCell<Symbol>>)> = {
            let class = class_sym.borrow();
            let class_data = class.as_class_sym();
            let mut entries = Vec::new();
            for (name, sections) in class_data.symbols.iter() {
                for syms in sections.values() {
                    for sym in syms {
                        if sym.borrow().typ() == SymType::FUNCTION {
                            entries.push((name.clone(), sym.clone()));
                            break;
                        }
                    }
                }
            }
            entries
        };

        for (func_name, func_sym) in &func_entries {
            let mut overrides_targets: Vec<OverrideMarkerTarget> = Vec::new();
            let mut overridden_by_targets: Vec<OverrideMarkerTarget> = Vec::new();

            for other_class in &other_classes {
                let other_module = other_class.borrow().find_module();

                let kind = match (&current_module, &other_module) {
                    (Some(cur), Some(other)) => {
                        let other_dir = other.borrow().as_module_package().dir_name.clone();
                        let cur_dir = cur.borrow().as_module_package().dir_name.clone();
                        if ModuleSymbol::is_in_deps(session, cur, &other_dir) {
                            "overrides"
                        } else if ModuleSymbol::is_in_deps(session, other, &cur_dir) {
                            "overriddenBy"
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };

                let other_class_ref = other_class.borrow();
                let other_class_data = other_class_ref.as_class_sym();

                if let Some(other_sections) = other_class_data.symbols.get(func_name) {
                    'outer: for other_syms in other_sections.values() {
                        for other_sym in other_syms {
                            let (is_func, range, file_weak) = {
                                let borrowed = other_sym.borrow();
                                (
                                    borrowed.typ() == SymType::FUNCTION,
                                    borrowed.range().clone(),
                                    borrowed.get_file(),
                                )
                            };
                            if is_func {
                                if let Some(file_weak) = file_weak {
                                    if let Some(other_file_sym) = file_weak.upgrade() {
                                        if let Some(other_path) = other_file_sym.borrow().paths().first().cloned() {
                                            let fi_opt = session.sync_odoo.get_file_mgr().borrow().get_file_info(&other_path);
                                            if let Some(fi) = fi_opt {
                                                let lsp_range = fi.borrow().text_range_to_range(&range, session.sync_odoo.encoding);
                                                let target = OverrideMarkerTarget {
                                                    uri: FileMgr::pathname2uri(&other_path),
                                                    range: lsp_range,
                                                };
                                                if kind == "overrides" {
                                                    overrides_targets.push(target);
                                                } else {
                                                    overridden_by_targets.push(target);
                                                }
                                            }
                                        }
                                    }
                                }
                                break 'outer;
                            }
                        }
                    }
                }
            }

            let func_range = func_sym.borrow().range().clone();
            let fi_opt = session.sync_odoo.get_file_mgr().borrow().get_file_info(&file_path);
            if let Some(fi) = fi_opt {
                let lsp_range = fi.borrow().text_range_to_range(&func_range, session.sync_odoo.encoding);
                let line = lsp_range.start.line;
                if !overrides_targets.is_empty() {
                    markers.push(OverrideMarker { line, kind: "overrides".to_string(), targets: overrides_targets });
                }
                if !overridden_by_targets.is_empty() {
                    markers.push(OverrideMarker { line, kind: "overriddenBy".to_string(), targets: overridden_by_targets });
                }
            }
        }
    }

    session.send_notification("$Odoo/overrideMarkers", PublishOverrideMarkersParams { uri, markers });
}

fn get_classes_in_file_symbol(file_sym: &Rc<RefCell<Symbol>>) -> Vec<Rc<RefCell<Symbol>>> {
    let borrowed = file_sym.borrow();
    let symbols_map = match &*borrowed {
        Symbol::File(fs) => &fs.symbols,
        Symbol::Package(ps) => match ps {
            PackageSymbol::Module(m) => &m.symbols,
            PackageSymbol::PythonPackage(p) => &p.symbols,
        },
        _ => return vec![],
    };

    symbols_map
        .values()
        .flat_map(|sections| sections.values())
        .flatten()
        .filter(|s| s.borrow().typ() == SymType::CLASS)
        .cloned()
        .collect()
}
