use std::{cell::RefCell, collections::{HashMap, HashSet}, ops::Range, rc::Rc};

use roxmltree::Node;

use crate::{constants::OYarn, core::{evaluation::ContextValue, odoo::SyncOdoo, symbols::{module_symbol::ModuleSymbol, symbol::Symbol}, xml_data::OdooData}, threads::SessionInfo, Sy, S};

pub enum XmlAstResult {
    SYMBOL(Rc<RefCell<Symbol>>),
    #[allow(non_camel_case_types)]
    XML_DATA(Rc<RefCell<Symbol>>, Range<usize>), //xml file symbol and range of the xml data
}

impl XmlAstResult {
    pub fn as_symbol(&self) -> Rc<RefCell<Symbol>> {
        match self {
            XmlAstResult::SYMBOL(sym) => sym.clone(),
            XmlAstResult::XML_DATA(_, _) =>panic!("Xml Data is not a symbol"),
        }
    }

    pub fn as_xml_data(&self) -> (Rc<RefCell<Symbol>>, Range<usize>) {
        match self {
            XmlAstResult::SYMBOL(_) => panic!("Symbol is not an XML Data"),
            XmlAstResult::XML_DATA(sym, range) => (sym.clone(), range.clone()),
        }
    }
}

pub struct XmlAstUtils {}

impl XmlAstUtils {

    pub fn get_symbols(session: &mut SessionInfo, file_symbol: &Rc<RefCell<Symbol>>, root: roxmltree::Node, offset: usize, on_dep_only: bool) -> (Vec<XmlAstResult>, Option<Range<usize>>) {
        let mut results = (vec![], None);
        let from_module = file_symbol.borrow().find_module();
        let mut context_xml = HashMap::new();
        for node in root.children() {
            XmlAstUtils::visit_node(session, &node, offset, from_module.clone(), &mut context_xml, &mut results, on_dep_only);
        }
        results
    }

    fn visit_node(session: &mut SessionInfo<'_>, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        if node.is_element() {
            match node.tag_name().name()  {
                "record" => {
                    XmlAstUtils::visit_record(session, &node, offset, from_module.clone(), ctxt, results, on_dep_only);
                }
                "field" => {
                    XmlAstUtils::visit_field(session, &node, offset, from_module.clone(), ctxt, results, on_dep_only);
                },
                "menuitem" => {
                    XmlAstUtils::visit_menu_item(session, &node, offset, from_module.clone(), ctxt, results, on_dep_only);
                },
                "template" => {
                    XmlAstUtils::visit_template(session, &node, offset, from_module.clone(), ctxt, results, on_dep_only);
                }
                _ => {
                    for child in node.children() {
                        XmlAstUtils::visit_node(session, &child, offset, from_module.clone(), ctxt, results, on_dep_only);
                    }
                }
            }
        } else if node.is_text() {
            XmlAstUtils::visit_text(session, &node, offset, from_module, ctxt, results, on_dep_only);
        }
    }

    fn visit_record(session: &mut SessionInfo<'_>, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        for attr in node.attributes() {
            if attr.name() == "model" {
                let model_name = attr.value().to_string();
                ctxt.insert(S!("record_model"), ContextValue::STRING(model_name.clone()));
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    if let Some(model) = session.sync_odoo.models.get(&Sy!(model_name)).cloned() {
                        let from_module = match on_dep_only {
                            true => from_module.clone(),
                            false => None,
                        };
                        results.0.extend(model.borrow().all_symbols(session, from_module, false).iter().filter(|s| s.1.is_none()).map(|s| XmlAstResult::SYMBOL(s.0.clone())));
                        results.1 = Some(attr.range_value());
                    }
                }
            } else if attr.name() == "id" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            }
        }
        // For ir.ui.view records, pre-scan children to find the view's target model
        // so that Ctrl+click on fields inside the arch navigates to the correct model's field
        if node.attribute("model") == Some("ir.ui.view") {
            let mut found = false;
            for child in node.children().filter(|n| n.is_element() && n.tag_name().name() == "field") {
                if child.attribute("name") == Some("model") {
                    if let Some(text) = child.children().find(|n| n.is_text()).and_then(|n| n.text()) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            ctxt.insert(S!("view_arch_model"), ContextValue::STRING(trimmed.to_string()));
                            found = true;
                        }
                    }
                }
            }
            // Inheriting views often omit `model` — follow inherit_id to the base view
            if !found {
                if let Some(inherit_ref) = node.children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "field")
                    .find(|n| n.attribute("name") == Some("inherit_id"))
                    .and_then(|n| n.attribute("ref"))
                {
                    if let Some(file_symbol) = from_module.as_ref() {
                        if let Some(model_name) = XmlAstUtils::resolve_inherited_view_model(session, file_symbol, inherit_ref) {
                            ctxt.insert(S!("view_arch_model"), ContextValue::STRING(model_name));
                        }
                    }
                }
            }
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module.clone(), ctxt, results, on_dep_only);
        }
        ctxt.remove(&S!("record_model"));
        ctxt.remove(&S!("view_arch_model"));
    }

    fn visit_field(session: &mut SessionInfo<'_>, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        let mut is_arch_field = false;
        for attr in node.attributes() {
            if attr.name() == "name" {
                let field_name_val = attr.value().to_string();
                // If this is the arch field of an ir.ui.view, swap record_model to the view's target model
                if field_name_val == "arch" {
                    if let Some(view_model) = ctxt.get(&S!("view_arch_model")).cloned() {
                        ctxt.insert(S!("record_model"), view_model);
                        is_arch_field = true;
                    }
                }
                ctxt.insert(S!("field_name"), ContextValue::STRING(field_name_val.clone()));
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    let model_name = ctxt.get(&S!("record_model")).cloned().unwrap_or(ContextValue::STRING(S!(""))).as_string();
                    if model_name.is_empty() {
                        continue;
                    }
                    if let Some(model) = session.sync_odoo.models.get(&Sy!(model_name)).cloned() {
                        let from_module = match on_dep_only {
                            true => from_module.clone(),
                            false => None,
                        };
                        for symbol in model.borrow().all_symbols(session, from_module, true) {
                            if symbol.1.is_none() {
                                let content = symbol.0.borrow().get_content_symbol(attr.value(), u32::MAX);
                                for symbol in content.symbols.iter() {
                                    results.0.push(XmlAstResult::SYMBOL(symbol.clone()));
                                }
                            }
                        }
                        results.1 = Some(attr.range_value());
                    }
                }
            } else if attr.name() == "ref" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            }
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module.clone(), ctxt, results, on_dep_only);
        }
        if is_arch_field {
            // Restore record_model to ir.ui.view after leaving arch
            ctxt.insert(S!("record_model"), ContextValue::STRING(S!("ir.ui.view")));
        }
        ctxt.remove(&S!("field_name"));
    }

    fn visit_text(session: &mut SessionInfo, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        if node.range().start <= offset && node.range().end >= offset {
            let model = ctxt.get(&S!("record_model")).cloned().unwrap_or(ContextValue::STRING(S!(""))).as_string();
            let field = ctxt.get(&S!("field_name")).cloned().unwrap_or(ContextValue::STRING(S!(""))).as_string();
            if model.is_empty() || field.is_empty() {
                return;
            }
            if field == "model" || field == "res_model" { //do not check model, let's assume it will contains a model name
                XmlAstUtils::add_model_result(session, node, from_module, results, on_dep_only);
            }
        }
    }

    fn visit_menu_item(session: &mut SessionInfo<'_>, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        for attr in node.attributes() {
            if attr.name() == "action" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            } else if attr.name() == "groups" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            }
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module.clone(), ctxt, results, on_dep_only);
        }
    }

    fn visit_template(session: &mut SessionInfo<'_>, node: &Node, offset: usize, from_module: Option<Rc<RefCell<Symbol>>>, ctxt: &mut HashMap<String, ContextValue>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        for attr in node.attributes() {
            if attr.name() == "inherit_id" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            } else if attr.name() == "groups" {
                if attr.range_value().start <= offset && attr.range_value().end >= offset {
                    XmlAstUtils::add_xml_id_result(session, attr.value(), &from_module.as_ref().unwrap(), attr.range_value(), results, on_dep_only);
                    results.1 = Some(attr.range_value());
                }
            }
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module.clone(), ctxt, results, on_dep_only);
        }
    }

    fn add_model_result(session: &mut SessionInfo, node: &Node, from_module: Option<Rc<RefCell<Symbol>>>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        if let Some(model) = session.sync_odoo.models.get(node.text().unwrap()).cloned() {
            let from_module = match on_dep_only {
                true => from_module.clone(),
                false => None,
            };
            results.0.extend(model.borrow().all_symbols(session, from_module, false).iter().filter(|s| s.1.is_none()).map(|s| XmlAstResult::SYMBOL(s.0.clone())));
            results.1 = Some(node.range());
        }
    }

    /// Walk the inherit_id chain to find the implicit model of an inheriting view.
    /// Returns the model name (e.g. "res.partner") or None if it cannot be resolved.
    fn resolve_inherited_view_model(session: &mut SessionInfo, from_file: &Rc<RefCell<Symbol>>, inherit_ref: &str) -> Option<String> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut current = inherit_ref.to_string();
        loop {
            if !visited.insert(current.clone()) {
                return None;
            }
            let xml_ids = SyncOdoo::get_xml_ids(session, from_file, &current, &(0..0), &mut vec![]);
            let parent_record = xml_ids.into_iter().find_map(|d| match d {
                OdooData::RECORD(r) => Some(r),
                _ => None,
            })?;
            if let Some(model) = parent_record.fields.iter()
                .find(|f| f.name.as_str() == "model")
                .and_then(|f| f.text.as_ref())
                .map(|t| t.trim().to_string()) {
                return Some(model);
            }
            let next = parent_record.fields.iter()
                .find(|f| f.name.as_str() == "inherit_id")
                .and_then(|f| f.ref_key.as_ref())
                .map(|(v, _)| v.clone())?;
            current = next;
        }
    }

    fn add_xml_id_result(session: &mut SessionInfo, xml_id: &str, file_symbol: &Rc<RefCell<Symbol>>, range: Range<usize>, results: &mut (Vec<XmlAstResult>, Option<Range<usize>>), on_dep_only: bool) {
        let mut xml_ids = SyncOdoo::get_xml_ids(session, file_symbol, xml_id, &range, &mut vec![]);
        if on_dep_only {
            xml_ids = xml_ids.into_iter().filter(|x| 
                {
                    let file = x.get_file_symbol();
                    if let Some(file) = file {
                        if let Some(file) = file.upgrade() {
                            let module = file.borrow().find_module();
                            if let Some(module) = module {
                                return ModuleSymbol::is_in_deps(session, &file_symbol.borrow().find_module().unwrap(), module.borrow().name());
                            }
                        }
                    }
                        return false;
                }
            ).collect::<Vec<_>>();
        }
        for xml_data in xml_ids.iter() {
            match xml_data {
                OdooData::RECORD(r) => {
                    results.0.push(XmlAstResult::XML_DATA(r.file_symbol.upgrade().unwrap(), r.range.clone()));
                },
                _ => {}
            }
        }
    }

}