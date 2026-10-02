use std::time::Duration;

use tree_sitter::Node;

use super::{
    Extraction, Language, MAX_DOC_CHARS, MAX_FILE_DOC_CHARS, Symbol, clean_signature,
    first_sentence, last_row, parse,
};
use crate::stats::Counters;

pub struct Rust;

impl Language for Rust {
    fn id(&self) -> &'static str {
        "rust@1"
    }

    fn extract_with_budget(
        &self,
        source: &str,
        counters: &Counters,
        budget: Duration,
    ) -> Extraction {
        let mut out = Extraction::empty();
        let Some(tree) = parse(&tree_sitter_rust::LANGUAGE.into(), source, counters, budget) else {
            out.truncated = true;
            return out;
        };
        let root = tree.root_node();
        out.file_doc = file_doc(root, source);
        let mut walker = Walker {
            src: source,
            out: &mut out,
            scope: Vec::new(),
            container: Container::Module,
        };
        walker.items(root);
        out.scrubbed()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Module,
    /// An `impl` or `trait` body: functions here are methods.
    Type,
}

struct Walker<'a> {
    src: &'a str,
    out: &'a mut Extraction,
    scope: Vec<String>,
    container: Container,
}

fn text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

fn field_text<'a>(node: Node<'_>, field: &str, src: &'a str) -> Option<&'a str> {
    node.child_by_field_name(field).map(|n| text(n, src))
}

/// `//!` lines at the very top of the file.
fn file_doc(root: Node<'_>, src: &str) -> String {
    let mut lines = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let t = text(child, src);
        if child.kind() == "line_comment" && t.starts_with("//!") {
            lines.push(t.trim_start_matches("//!").trim_end().to_owned());
        } else if child.kind() == "block_comment" && t.starts_with("/*!") {
            lines.push(
                t.trim_start_matches("/*!")
                    .trim_end_matches("*/")
                    .to_owned(),
            );
        } else {
            break;
        }
    }
    first_sentence(&lines.join("\n"), MAX_FILE_DOC_CHARS)
}

/// The `///` (or `/** */`) comment attached directly above `node`, skipping attributes.
fn leading_doc(node: Node<'_>, src: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current = node;
    while let Some(previous) = current.prev_sibling() {
        let adjacent = current.start_position().row <= last_row(previous) + 1;
        let t = text(previous, src);
        match previous.kind() {
            "attribute_item" if adjacent => {}
            "line_comment" if adjacent && t.starts_with("///") && !t.starts_with("////") => {
                lines.push(t[3..].trim_end().to_owned());
            }
            "block_comment" if adjacent && t.starts_with("/**") && !t.starts_with("/**/") => {
                lines.push(
                    t.trim_start_matches("/**")
                        .trim_end_matches("*/")
                        .lines()
                        .map(|l| l.trim().trim_start_matches('*').trim())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                break;
            }
            _ => break,
        }
        current = previous;
    }
    lines.reverse();
    first_sentence(&lines.join("\n"), MAX_DOC_CHARS)
}

/// The type name an `impl` is for: generics, references and path prefixes removed.
fn type_name(raw: &str) -> String {
    let base = raw.split('<').next().unwrap_or(raw).trim();
    let base = base.trim_start_matches('&').trim();
    let base = base.strip_prefix("mut ").unwrap_or(base);
    let base = base.strip_prefix("dyn ").unwrap_or(base);
    let last = base.rsplit("::").next().unwrap_or(base).trim();
    last.chars().take(40).collect()
}

impl Walker<'_> {
    fn qname(&self, name: &str) -> String {
        if self.scope.is_empty() {
            name.to_owned()
        } else {
            format!("{}::{name}", self.scope.join("::"))
        }
    }

    fn items(&mut self, parent: Node<'_>) {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            if self.out.truncated && self.out.symbols.len() >= super::MAX_SYMBOLS {
                return;
            }
            match child.kind() {
                "function_item" | "function_signature_item" => {
                    let kind = if self.container == Container::Type {
                        "method"
                    } else {
                        "fn"
                    };
                    self.symbol(child, kind, &["body"], None);
                }
                "struct_item" => self.symbol(child, "struct", &["body"], None),
                "enum_item" => self.symbol(child, "enum", &["body"], None),
                "union_item" => self.symbol(child, "union", &["body"], None),
                "type_item" => self.symbol(child, "type", &["type"], None),
                "const_item" => self.symbol(child, "const", &["value"], None),
                "static_item" => self.symbol(child, "static", &["value"], None),
                "macro_definition" => self.macro_definition(child),
                "trait_item" => self.trait_item(child),
                "impl_item" => self.impl_item(child),
                "mod_item" => self.mod_item(child),
                "use_declaration" => {
                    if let Some(argument) = child.child_by_field_name("argument") {
                        let mut specs = Vec::new();
                        flatten_use(argument, "", self.src, &mut specs);
                        for spec in specs {
                            self.out.push_import("use", spec);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn symbol(
        &mut self,
        node: Node<'_>,
        kind: &str,
        end_fields: &[&str],
        via_trait: Option<String>,
    ) {
        let Some(name) = field_text(node, "name", self.src) else {
            return;
        };
        let end = end_fields
            .iter()
            .filter_map(|f| node.child_by_field_name(f))
            .map(|n| n.start_byte())
            .min()
            .unwrap_or_else(|| node.end_byte());
        let signature = clean_signature(&self.src[node.start_byte()..end], false);
        self.out.push_symbol(Symbol {
            kind: kind.to_owned(),
            name: name.to_owned(),
            qname: self.qname(name),
            signature,
            doc: leading_doc(node, self.src),
            line: u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX),
            end_line: u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX),
            via_trait,
        });
    }

    fn macro_definition(&mut self, node: Node<'_>) {
        let Some(name) = field_text(node, "name", self.src) else {
            return;
        };
        self.out.push_symbol(Symbol {
            kind: "macro".to_owned(),
            name: name.to_owned(),
            qname: self.qname(name),
            signature: format!("macro_rules! {name}"),
            doc: leading_doc(node, self.src),
            line: u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX),
            end_line: u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX),
            via_trait: None,
        });
    }

    fn trait_item(&mut self, node: Node<'_>) {
        self.symbol(node, "trait", &["body"], None);
        let Some(name) = field_text(node, "name", self.src) else {
            return;
        };
        self.descend_type(node, name.to_owned(), None);
    }

    fn impl_item(&mut self, node: Node<'_>) {
        let Some(ty) = field_text(node, "type", self.src) else {
            return;
        };
        let via_trait = field_text(node, "trait", self.src).map(type_name);
        self.descend_type(node, type_name(ty), via_trait.as_deref());
    }

    /// Walks an `impl` or `trait` body with the type name as the qualifying scope.
    fn descend_type(&mut self, node: Node<'_>, name: String, via_trait: Option<&str>) {
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let saved = self.container;
        self.container = Container::Type;
        self.scope.push(name);
        match via_trait {
            None => self.items(body),
            Some(via_trait) => self.items_via_trait(body, via_trait),
        }
        self.scope.pop();
        self.container = saved;
    }

    fn items_via_trait(&mut self, body: Node<'_>, via_trait: &str) {
        let before = self.out.symbols.len();
        self.items(body);
        for symbol in &mut self.out.symbols[before..] {
            symbol.via_trait = Some(via_trait.to_owned());
        }
    }

    fn mod_item(&mut self, node: Node<'_>) {
        let Some(name) = field_text(node, "name", self.src) else {
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            // `mod name;` declares a child file rather than defining anything here.
            self.out.push_import("mod", name.to_owned());
            return;
        };
        self.symbol(node, "mod", &["body"], None);
        let saved = self.container;
        self.container = Container::Module;
        self.scope.push(name.to_owned());
        self.items(body);
        self.scope.pop();
        self.container = saved;
    }
}

/// Expands a `use` tree into the full path of every leaf.
fn flatten_use(node: Node<'_>, prefix: &str, src: &str, out: &mut Vec<String>) {
    let join = |tail: &str| {
        if prefix.is_empty() {
            tail.to_owned()
        } else if tail.is_empty() || tail == "self" {
            prefix.to_owned()
        } else {
            format!("{prefix}::{tail}")
        }
    };
    match node.kind() {
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                flatten_use(path, prefix, src, out);
            }
        }
        "use_wildcard" => {
            let inner = node
                .named_child(0)
                .map(|n| text(n, src).to_owned())
                .unwrap_or_default();
            out.push(join(&inner));
        }
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map_or_else(|| prefix.to_owned(), |n| join(text(n, src)));
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(list, &path, src, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                flatten_use(child, prefix, src, out);
            }
        }
        _ => out.push(join(text(node, src))),
    }
}
