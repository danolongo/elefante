use std::time::Duration;

use tree_sitter::Node;

use super::{
    Extraction, Language, MAX_DOC_CHARS, MAX_FILE_DOC_CHARS, Symbol, clean_signature,
    first_sentence, last_row, parse,
};
use crate::stats::Counters;

/// TypeScript and TSX share a walker and differ only in grammar: identical bytes can parse
/// differently (`<T>x` is an assertion in TS and a JSX element in TSX), so they are separate
/// extractors with separate cache ids.
pub struct TypeScript {
    pub tsx: bool,
}

impl Language for TypeScript {
    fn id(&self) -> &'static str {
        if self.tsx { "tsx@1" } else { "typescript@1" }
    }

    fn extract_with_budget(
        &self,
        source: &str,
        counters: &Counters,
        budget: Duration,
    ) -> Extraction {
        let mut out = Extraction::empty();
        let grammar: tree_sitter::Language = if self.tsx {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        };
        let Some(tree) = parse(&grammar, source, counters, budget) else {
            out.truncated = true;
            return out;
        };
        let root = tree.root_node();
        out.file_doc = file_doc(root, source);
        let mut walker = Walker {
            src: source,
            out: &mut out,
            scope: Vec::new(),
        };
        walker.statements(root);
        out.scrubbed()
    }
}

struct Walker<'a> {
    src: &'a str,
    out: &'a mut Extraction,
    scope: Vec<String>,
}

fn text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

fn jsdoc(raw: &str) -> String {
    let body = raw.trim_start_matches("/**").trim_end_matches("*/");
    let mut lines = Vec::new();
    for line in body.lines() {
        let line = line.trim().trim_start_matches('*').trim();
        if line.starts_with('@') {
            break;
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// A leading comment separated from the code below it by a blank line describes the file.
fn file_doc(root: Node<'_>, src: &str) -> String {
    let Some(first) = root.child(0) else {
        return String::new();
    };
    if first.kind() != "comment" {
        return String::new();
    }
    let detached = root
        .child(1)
        .is_none_or(|next| next.start_position().row > last_row(first) + 1);
    if !detached {
        return String::new();
    }
    let raw = text(first, src);
    let body = if raw.starts_with("/*") {
        jsdoc(raw)
    } else {
        raw.trim_start_matches('/').trim().to_owned()
    };
    first_sentence(&body, MAX_FILE_DOC_CHARS)
}

fn leading_doc(node: Node<'_>, src: &str) -> String {
    let Some(previous) = node.prev_sibling() else {
        return String::new();
    };
    let t = text(previous, src);
    if previous.kind() == "comment"
        && t.starts_with("/**")
        && node.start_position().row <= last_row(previous) + 1
    {
        first_sentence(&jsdoc(t), MAX_DOC_CHARS)
    } else {
        String::new()
    }
}

fn unquote(raw: &str) -> String {
    raw.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_owned()
}

impl Walker<'_> {
    fn qname(&self, name: &str) -> String {
        if self.scope.is_empty() {
            name.to_owned()
        } else {
            format!("{}::{name}", self.scope.join("::"))
        }
    }

    fn statements(&mut self, parent: Node<'_>) {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            self.statement(child, child);
        }
    }

    /// `outer` is the node whose preceding comment documents the item and whose text begins its
    /// signature — the `export` wrapper when there is one, otherwise the declaration itself.
    fn statement(&mut self, node: Node<'_>, outer: Node<'_>) {
        match node.kind() {
            "export_statement" => {
                if let Some(source) = node.child_by_field_name("source") {
                    self.out
                        .push_import("reexport", unquote(text(source, self.src)));
                }
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    self.statement(declaration, node);
                }
            }
            "ambient_declaration" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.statement(child, outer);
                }
            }
            "import_statement" => {
                if let Some(source) = node.child_by_field_name("source") {
                    self.out
                        .push_import("import", unquote(text(source, self.src)));
                }
            }
            "function_declaration" | "generator_function_declaration" => {
                self.symbol(node, outer, "function", &["body"]);
            }
            "class_declaration" | "abstract_class_declaration" => {
                self.symbol(node, outer, "class", &["body"]);
                self.members(
                    node,
                    "class_body",
                    &["method_definition", "abstract_method_signature"],
                );
            }
            "interface_declaration" => {
                self.symbol(node, outer, "interface", &["body"]);
                self.members(node, "interface_body", &["method_signature"]);
            }
            "type_alias_declaration" => self.symbol(node, outer, "type", &["value"]),
            "enum_declaration" => self.symbol(node, outer, "enum", &["body"]),
            "lexical_declaration" | "variable_declaration" => self.variables(node, outer),
            // Overload signatures and `declare function` have no body to end the signature at.
            "function_signature" => self.symbol(node, outer, "function", &[]),
            "internal_module" | "module" => self.namespace(node, outer),
            "expression_statement" => {
                if let Some(inner) = node.named_child(0)
                    && matches!(inner.kind(), "internal_module" | "module")
                {
                    self.namespace(inner, node);
                }
            }
            _ => {}
        }
    }

    fn symbol(&mut self, node: Node<'_>, outer: Node<'_>, kind: &str, end_fields: &[&str]) {
        let Some(name) = node.child_by_field_name("name").map(|n| text(n, self.src)) else {
            return;
        };
        let end = end_fields
            .iter()
            .filter_map(|f| node.child_by_field_name(f))
            .map(|n| n.start_byte())
            .min()
            .unwrap_or_else(|| node.end_byte());
        self.out.push_symbol(Symbol {
            kind: kind.to_owned(),
            name: name.to_owned(),
            qname: self.qname(name),
            signature: clean_signature(&self.src[outer.start_byte()..end], true),
            doc: leading_doc(outer, self.src),
            line: u32::try_from(outer.start_position().row + 1).unwrap_or(u32::MAX),
            end_line: u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX),
            via_trait: None,
        });
    }

    fn members(&mut self, node: Node<'_>, body_kind: &str, member_kinds: &[&str]) {
        let Some(name) = node.child_by_field_name("name").map(|n| text(n, self.src)) else {
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        if body.kind() != body_kind {
            return;
        }
        self.scope.push(name.to_owned());
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            if member_kinds.contains(&member.kind()) {
                self.symbol(member, member, "method", &["body"]);
            }
        }
        self.scope.pop();
    }

    /// Top-level `const f = () => …` is a function; any other initialiser is skipped entirely,
    /// which is also what keeps a literal credential out of the graph.
    fn variables(&mut self, node: Node<'_>, outer: Node<'_>) {
        let mut cursor = node.walk();
        for declarator in node.named_children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let (Some(name), Some(value)) = (
                declarator.child_by_field_name("name"),
                declarator.child_by_field_name("value"),
            ) else {
                continue;
            };
            if name.kind() != "identifier"
                || !matches!(value.kind(), "arrow_function" | "function_expression")
            {
                continue;
            }
            let name = text(name, self.src);
            let end = value
                .child_by_field_name("body")
                .map_or_else(|| value.end_byte(), |b| b.start_byte());
            self.out.push_symbol(Symbol {
                kind: "function".to_owned(),
                name: name.to_owned(),
                qname: self.qname(name),
                signature: clean_signature(&self.src[outer.start_byte()..end], true),
                doc: leading_doc(outer, self.src),
                line: u32::try_from(outer.start_position().row + 1).unwrap_or(u32::MAX),
                end_line: u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX),
                via_trait: None,
            });
        }
    }

    fn namespace(&mut self, node: Node<'_>, outer: Node<'_>) {
        let Some(name) = node.child_by_field_name("name").map(|n| text(n, self.src)) else {
            return;
        };
        self.symbol(node, outer, "namespace", &["body"]);
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        self.scope.push(unquote(name));
        self.statements(body);
        self.scope.pop();
    }
}
